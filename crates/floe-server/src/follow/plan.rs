//! What a follow round publishes (D48), as pure functions: the diff between the
//! WAL's matching refs and upstream's, and the one `RefTransaction` that applies it
//! — with the old tip of every rewritten or deleted ref kept under
//! `refs/archive/<unix-ts>/<original-ref>` in the same transaction. Ancestry is
//! the op's (async, on the serving copy after ingest); it is fed back in as a
//! [`Kind`] per change.

use std::collections::HashMap;

use floe_config::OnRewrite;
use floe_proto::v1::RefUpdate;

/// Where archived tips live. Reserved: follow never follows it (`refpattern`).
pub(crate) const ARCHIVE_PREFIX: &str = "refs/archive/";

pub(crate) struct Observed<'a> {
    /// WAL refs matching the patterns.
    pub have: &'a HashMap<String, String>,
    /// Upstream refs matching the patterns (after the fetch's `--prune`).
    pub tips: &'a HashMap<String, String>,
    /// Upstream advertised at least one ref of any name (`Probe::advertised_any`).
    pub advertised_any: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    Create {
        name: String,
        new: String,
    },
    Update {
        name: String,
        old: String,
        new: String,
    },
    Delete {
        name: String,
        old: String,
    },
}

impl Change {
    pub(crate) fn name(&self) -> &str {
        match self {
            Change::Create { name, .. }
            | Change::Update { name, .. }
            | Change::Delete { name, .. } => name,
        }
    }

    /// The kind when it does not take an ancestry check: a create loses nothing, a
    /// delete always does, and a tag retarget is a force-push (POLICY.md). `None` =
    /// a branch-like update: `is_ancestor(old, new)` decides.
    pub(crate) fn fixed_kind(&self) -> Option<Kind> {
        match self {
            Change::Create { .. } => Some(Kind::FastForward),
            Change::Delete { .. } => Some(Kind::Rewrite),
            Change::Update { name, .. } if name.starts_with("refs/tags/") => Some(Kind::Rewrite),
            Change::Update { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    FastForward,
    Rewrite,
}

/// The diff, sorted by ref name; never a no-op. When upstream advertised **no refs
/// at all** it yields no `Delete` (the empty-advertisement guard: an outage or a
/// wiped repository, not a mass delete) and says so in the notice. The guard is
/// about the whole advertisement: an exact ref, or the last tag, deleted upstream
/// is an ordinary deletion.
pub(crate) fn diff(o: &Observed<'_>) -> (Vec<Change>, Option<String>) {
    let mut out = Vec::new();
    for (name, new) in o.tips {
        match o.have.get(name) {
            None => out.push(Change::Create {
                name: name.clone(),
                new: new.clone(),
            }),
            Some(old) if old != new => out.push(Change::Update {
                name: name.clone(),
                old: old.clone(),
                new: new.clone(),
            }),
            Some(_) => {}
        }
    }
    let gone: Vec<(&String, &String)> = o
        .have
        .iter()
        .filter(|(n, _)| !o.tips.contains_key(*n))
        .collect();
    let mut notice = None;
    if o.advertised_any {
        out.extend(gone.into_iter().map(|(n, old)| Change::Delete {
            name: n.clone(),
            old: old.clone(),
        }));
    } else if !gone.is_empty() {
        notice = Some(format!(
            "upstream advertised no refs at all; {} followed ref(s) left as is",
            gone.len()
        ));
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    (out, notice)
}

/// HEAD as `[upstream] head` wants it, decided against the post-plan ref set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadWant {
    /// `upstream.head` (`refs/heads/main`).
    pub target: String,
    /// The WAL's HEAD target now.
    pub current: String,
    /// Whether `target` exists in the WAL now (before the plan).
    pub exists_now: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// The one `RefTransaction`: archive creates + applies (+ HEAD), sorted.
    pub updates: Vec<RefUpdate>,
    pub archived: Vec<Archived>,
    /// Human lines: what policy `refuse` left as is.
    pub refused: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Archived {
    pub archive_ref: String,
    pub original: String,
    pub old: String,
    /// `None` = deleted upstream.
    pub new: Option<String>,
}

impl Plan {
    /// `meta["follow.archived"]`: `<archive_ref> <original_ref> <old_oid> <new_oid|->`,
    /// one line per archive, sorted. `None` when nothing was archived.
    pub(crate) fn archived_meta(&self) -> Option<String> {
        if self.archived.is_empty() {
            return None;
        }
        let mut lines: Vec<String> = self
            .archived
            .iter()
            .map(|a| {
                format!(
                    "{} {} {} {}",
                    a.archive_ref,
                    a.original,
                    a.old,
                    a.new.as_deref().unwrap_or("-")
                )
            })
            .collect();
        lines.sort();
        Some(lines.join("\n"))
    }

    /// Whether the plan moves HEAD.
    pub(crate) fn moves_head(&self) -> bool {
        self.updates
            .iter()
            .any(|u| !u.new_symbolic_target.is_empty())
    }
}

/// `refs/archive/<ts>/<original>`.
pub(crate) fn archive_ref(ts: u64, original: &str) -> String {
    format!("{ARCHIVE_PREFIX}{ts}/{original}")
}

/// How far ahead of `now` an existing archive's `<unix-ts>` may be and still count
/// (clock skew between maintainers). Anything later was not written by follow —
/// `refs/archive/` is an ordinary namespace a pusher can write to — and must not
/// pin the timestamp (a forged `refs/archive/18446744073709551615/x` would
/// otherwise make every later archive name collide, rejecting each round forever).
const ARCHIVE_TS_SKEW: u64 = 24 * 60 * 60;

/// The op's archive timestamp: `max(now, newest plausible archive ts + 1)`, then
/// past any `<unix-ts>` already used in the snapshot, so it increases per repository
/// and no name the op picks exists in the snapshot it planned from. `names` = every
/// ref name in that snapshot.
pub(crate) fn archive_ts<'a>(now: u64, names: impl Iterator<Item = &'a str>) -> u64 {
    let used: std::collections::HashSet<u64> = names
        .filter_map(|n| n.strip_prefix(ARCHIVE_PREFIX))
        .filter_map(|rest| rest.split('/').next())
        .filter_map(|ts| ts.parse::<u64>().ok())
        .collect();
    let newest = used
        .iter()
        .copied()
        .filter(|n| *n <= now.saturating_add(ARCHIVE_TS_SKEW))
        .max();
    let mut ts = newest.map_or(now, |n| now.max(n.saturating_add(1)));
    // Bounded by the snapshot's size: each step skips one used value.
    while used.contains(&ts) {
        match ts.checked_add(1) {
            Some(next) => ts = next,
            None => break,
        }
    }
    ts
}

/// The transaction for `changes` (each with its kind) under `policy`, archive
/// timestamp `ts`, and the HEAD wish. `Archive`: a fast-forward is one update; a
/// rewrite is `create refs/archive/<ts>/<ref> at old` + the update; a delete is the
/// archive create + the delete. `Refuse`: rewrites and deletes become `refused`
/// lines and are left out.
pub(crate) fn build(
    changes: Vec<(Change, Kind)>,
    policy: OnRewrite,
    ts: u64,
    head: Option<&HeadWant>,
) -> Plan {
    let mut plan = Plan::default();
    // After the plan: refs the transaction creates/updates (true) or deletes (false).
    let mut after: HashMap<String, bool> = HashMap::new();
    let upd = |name: &str, old: &str, new: &str| RefUpdate {
        name: name.to_string(),
        old_oid: old.to_string(),
        new_oid: new.to_string(),
        ..Default::default()
    };
    for (change, kind) in changes {
        let kind = change.fixed_kind().unwrap_or(kind);
        match (change, kind, policy) {
            (Change::Create { name, new }, _, _) => {
                plan.updates.push(upd(&name, "", &new));
                after.insert(name, true);
            }
            (Change::Update { name, old, new }, Kind::FastForward, _) => {
                plan.updates.push(upd(&name, &old, &new));
                after.insert(name, true);
            }
            (Change::Update { name, old, new }, Kind::Rewrite, OnRewrite::Archive) => {
                let archive = archive_ref(ts, &name);
                plan.updates.push(upd(&archive, "", &old));
                plan.updates.push(upd(&name, &old, &new));
                plan.archived.push(Archived {
                    archive_ref: archive,
                    original: name.clone(),
                    old,
                    new: Some(new),
                });
                after.insert(name, true);
            }
            (Change::Update { name, old, new }, Kind::Rewrite, OnRewrite::Refuse) => {
                plan.refused.push(format!(
                    "{name}: upstream rewound {}→{} (not a fast-forward)",
                    short(&old),
                    short(&new)
                ));
            }
            (Change::Delete { name, old }, _, OnRewrite::Archive) => {
                let archive = archive_ref(ts, &name);
                plan.updates.push(upd(&archive, "", &old));
                plan.updates.push(upd(&name, &old, ""));
                plan.archived.push(Archived {
                    archive_ref: archive,
                    original: name.clone(),
                    old,
                    new: None,
                });
                after.insert(name, false);
            }
            (Change::Delete { name, .. }, _, OnRewrite::Refuse) => {
                plan.refused
                    .push(format!("{name}: deleted upstream; left as is"));
            }
        }
    }
    if let Some(h) = head
        && h.target != h.current
        && after.get(&h.target).copied().unwrap_or(h.exists_now)
    {
        plan.updates.push(RefUpdate {
            name: "HEAD".into(),
            new_symbolic_target: h.target.clone(),
            ..Default::default()
        });
    }
    plan.updates.sort_by(|a, b| a.name.cmp(&b.name));
    plan
}

pub(crate) fn short(oid: &str) -> &str {
    if oid.is_empty() {
        "(none)"
    } else {
        oid.get(..oid.len().min(12)).unwrap_or(oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    }

    fn names(p: &Plan) -> Vec<(&str, &str, &str)> {
        p.updates
            .iter()
            .map(|u| (u.name.as_str(), u.old_oid.as_str(), u.new_oid.as_str()))
            .collect()
    }

    #[test]
    fn diff_creates_updates_deletes_sorted_and_never_noops() {
        let have = map(&[
            ("refs/heads/a", "1"),
            ("refs/heads/b", "2"),
            ("refs/heads/c", "3"),
        ]);
        let tips = map(&[
            ("refs/heads/a", "1"),
            ("refs/heads/b", "9"),
            ("refs/heads/d", "4"),
        ]);
        let (changes, notice) = diff(&Observed {
            have: &have,
            tips: &tips,
            advertised_any: true,
        });
        assert_eq!(notice, None);
        assert_eq!(
            changes,
            vec![
                Change::Update {
                    name: "refs/heads/b".into(),
                    old: "2".into(),
                    new: "9".into()
                },
                Change::Delete {
                    name: "refs/heads/c".into(),
                    old: "3".into()
                },
                Change::Create {
                    name: "refs/heads/d".into(),
                    new: "4".into()
                },
            ]
        );
    }

    #[test]
    fn empty_advertisement_never_deletes() {
        let have = map(&[("refs/heads/main", "1"), ("refs/tags/v1", "2")]);
        let tips = HashMap::new();
        let (changes, notice) = diff(&Observed {
            have: &have,
            tips: &tips,
            advertised_any: false,
        });
        assert!(changes.is_empty());
        assert!(notice.is_some_and(|n| n.contains("2 followed ref(s)")));
    }

    #[test]
    fn single_exact_ref_deleted_upstream_is_a_delete() {
        // `follow = ["refs/heads/main"]`, main deleted, upstream still advertises others.
        let have = map(&[("refs/heads/main", "1")]);
        let (changes, notice) = diff(&Observed {
            have: &have,
            tips: &HashMap::new(),
            advertised_any: true,
        });
        assert_eq!(notice, None);
        assert_eq!(
            changes,
            vec![Change::Delete {
                name: "refs/heads/main".into(),
                old: "1".into()
            }]
        );
    }

    #[test]
    fn last_tag_deleted_is_a_delete() {
        let have = map(&[("refs/tags/v1", "7")]);
        let (changes, _) = diff(&Observed {
            have: &have,
            tips: &HashMap::new(),
            advertised_any: true,
        });
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].fixed_kind(), Some(Kind::Rewrite));
    }

    #[test]
    fn build_archive_pairs_rewrites_and_deletes() {
        let changes = vec![
            (
                Change::Create {
                    name: "refs/heads/new".into(),
                    new: "n".into(),
                },
                Kind::FastForward,
            ),
            (
                Change::Update {
                    name: "refs/heads/ff".into(),
                    old: "a".into(),
                    new: "b".into(),
                },
                Kind::FastForward,
            ),
            (
                Change::Update {
                    name: "refs/heads/main".into(),
                    old: "c".into(),
                    new: "d".into(),
                },
                Kind::Rewrite,
            ),
            (
                Change::Delete {
                    name: "refs/heads/feat/old".into(),
                    old: "e".into(),
                },
                Kind::Rewrite,
            ),
        ];
        let p = build(changes, OnRewrite::Archive, 100, None);
        assert!(p.refused.is_empty());
        assert_eq!(
            names(&p),
            vec![
                ("refs/archive/100/refs/heads/feat/old", "", "e"),
                ("refs/archive/100/refs/heads/main", "", "c"),
                ("refs/heads/feat/old", "e", ""),
                ("refs/heads/ff", "a", "b"),
                ("refs/heads/main", "c", "d"),
                ("refs/heads/new", "", "n"),
            ]
        );
        assert_eq!(
            p.archived_meta().as_deref(),
            Some(
                "refs/archive/100/refs/heads/feat/old refs/heads/feat/old e -\n\
                 refs/archive/100/refs/heads/main refs/heads/main c d"
            )
        );
    }

    #[test]
    fn build_refuse_keeps_d33_behaviour() {
        let changes = vec![
            (
                Change::Update {
                    name: "refs/heads/main".into(),
                    old: "c".into(),
                    new: "d".into(),
                },
                Kind::Rewrite,
            ),
            (
                Change::Delete {
                    name: "refs/heads/x".into(),
                    old: "e".into(),
                },
                Kind::Rewrite,
            ),
            (
                Change::Update {
                    name: "refs/heads/ff".into(),
                    old: "a".into(),
                    new: "b".into(),
                },
                Kind::FastForward,
            ),
        ];
        let p = build(changes, OnRewrite::Refuse, 100, None);
        assert_eq!(names(&p), vec![("refs/heads/ff", "a", "b")]);
        assert!(p.archived.is_empty() && p.archived_meta().is_none());
        assert_eq!(p.refused.len(), 2);
        assert!(
            p.refused[0].contains("not a fast-forward"),
            "{:?}",
            p.refused
        );
        assert!(p.refused[1].contains("deleted upstream; left as is"));
    }

    #[test]
    fn tag_move_is_a_rewrite_even_when_called_a_fast_forward() {
        let p = build(
            vec![(
                Change::Update {
                    name: "refs/tags/v1".into(),
                    old: "a".into(),
                    new: "b".into(),
                },
                Kind::FastForward,
            )],
            OnRewrite::Archive,
            5,
            None,
        );
        assert_eq!(p.archived.len(), 1);
        assert_eq!(p.archived[0].archive_ref, "refs/archive/5/refs/tags/v1");
    }

    #[test]
    fn archive_ts_is_above_the_newest_existing_archive() {
        let refs = [
            "refs/heads/main",
            "refs/archive/1700/refs/heads/main",
            "refs/archive/2000/refs/tags/v1",
            "refs/archive/junk/refs/heads/x",
        ];
        assert_eq!(archive_ts(1000, refs.iter().copied()), 2001);
        assert_eq!(archive_ts(3000, refs.iter().copied()), 3000);
        assert_eq!(archive_ts(42, std::iter::empty()), 42);
    }

    #[test]
    fn archive_ts_ignores_forged_future_archives_and_never_reuses_a_name() {
        // A pushed `refs/archive/<u64::MAX>/...` must not pin ts (every later name would collide).
        let refs = [
            "refs/archive/18446744073709551615/x",
            "refs/archive/1700/refs/heads/main",
        ];
        assert_eq!(archive_ts(1000, refs.iter().copied()), 1701);
        // A value just past the skew window is skipped over, never reused.
        let now = 1_000_000;
        let edge = format!("refs/archive/{}/x", now + ARCHIVE_TS_SKEW);
        let past = format!("refs/archive/{}/x", now + ARCHIVE_TS_SKEW + 1);
        let refs = [edge.as_str(), past.as_str()];
        assert_eq!(
            archive_ts(now, refs.iter().copied()),
            now + ARCHIVE_TS_SKEW + 2
        );
        // `now` itself already used: the next free second.
        let taken = format!("refs/archive/{now}/x");
        assert_eq!(archive_ts(now, std::iter::once(taken.as_str())), now + 1);
    }

    #[test]
    fn head_moves_only_when_its_target_exists_after_the_plan() {
        let want = |exists_now| HeadWant {
            target: "refs/heads/main".into(),
            current: "refs/heads/master".into(),
            exists_now,
        };
        // Exists already: moves.
        assert!(build(vec![], OnRewrite::Archive, 1, Some(&want(true))).moves_head());
        // Missing and not created: no move.
        assert!(!build(vec![], OnRewrite::Archive, 1, Some(&want(false))).moves_head());
        // Created by this round: moves.
        let create = vec![(
            Change::Create {
                name: "refs/heads/main".into(),
                new: "1".into(),
            },
            Kind::FastForward,
        )];
        let p = build(create, OnRewrite::Archive, 1, Some(&want(false)));
        assert!(p.moves_head());
        assert_eq!(p.updates[0].name, "HEAD");
        // Deleted by this round: no move.
        let delete = vec![(
            Change::Delete {
                name: "refs/heads/main".into(),
                old: "1".into(),
            },
            Kind::Rewrite,
        )];
        assert!(!build(delete, OnRewrite::Archive, 1, Some(&want(true))).moves_head());
        // Already there: no move.
        let same = HeadWant {
            current: "refs/heads/main".into(),
            ..want(true)
        };
        assert!(!build(vec![], OnRewrite::Archive, 1, Some(&same)).moves_head());
    }
}
