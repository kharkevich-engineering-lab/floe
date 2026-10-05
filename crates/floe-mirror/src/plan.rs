//! The planner (§B.8 step 5), pure: (state, discovery, lookups) → (new state,
//! steps per repository). It decides what the mirror wants; [`crate::apply`] checks
//! the floe side (ownership, human edits) while doing it.
//!
//! The planner never touches a `detached` entry's settings, never infers "gone"
//! from an incomplete discovery, and never deletes anything.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use floe_config::GithubMirrorConfig;

use crate::select::{Verdict, is_explicit, select};
use crate::settings;
use crate::source::{Discovery, Lookup, RemoteRepo, Source};
use crate::state::{MirrorState, RepoEntry, Status};

/// Lookups per pass (§B.6): 2,000 removed stars are all looked up within 40 passes.
pub const MAX_LOOKUPS: usize = 50;

/// One step for one repository. A step may end its repository's sequence
/// (a conflict, a too-large repository nobody prepared, a failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Map the name and claim it: an empty repository named for us or one
    /// carrying our marker is adopted; otherwise create it when `allow_create`
    /// (a too-large repository is only adopted, never created).
    Create { allow_create: bool },
    /// The read-only `policy.json` (when `read_only`).
    PutPolicy,
    /// Publish the rendered `[upstream]` table (after the human-edit check).
    Publish { reason: String },
    /// Ask follow to run now.
    Nudge,
}

impl Step {
    pub fn name(&self) -> &'static str {
        match self {
            Step::Create { .. } => "create",
            Step::PutPolicy => "policy",
            Step::Publish { .. } => "publish",
            Step::Nudge => "nudge",
        }
    }
}

/// The steps for one repository (by forge id), in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub id: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The state with this pass's bookkeeping (observations, statuses,
    /// `missing_since`, `last_lookup`) applied; the applier records outcomes on it.
    pub state: MirrorState,
    pub actions: Vec<Action>,
    /// `(id, change)` for inventory telemetry: `renamed`, `archived`, `excluded`,
    /// `gone`, `forbidden`, `resumed`, `too-large`, `updated`. `created` and
    /// `conflict` are outcomes, added by the applier.
    pub changes: Vec<(String, String)>,
}

pub struct PlanInput<'a> {
    pub cfg: &'a GithubMirrorConfig,
    pub source: &'a dyn Source,
    pub now: DateTime<Utc>,
    pub discovery: &'a Discovery,
    /// Results of this pass's lookups (ids from [`lookup_candidates`]).
    pub lookups: &'a BTreeMap<String, Lookup>,
}

/// Ids in the state that discovery did not report, in lookup order: never
/// looked up first, then oldest `last_lookup`, then oldest `missing_since`. Only
/// ids already missing, or about to be (a complete discovery lacks them).
/// Detached entries are never looked up (the mirror leaves them alone);
/// excluded ones come last (frozen already, a lookup only relabels them).
pub fn lookup_candidates(
    state: &MirrorState,
    discovery: &Discovery,
    now: DateTime<Utc>,
) -> Vec<String> {
    let seen: BTreeSet<&str> = discovery.repos.iter().map(|r| r.id.as_str()).collect();
    let mut c: Vec<(&String, &RepoEntry)> = state
        .repos
        .iter()
        .filter(|(id, e)| {
            !seen.contains(id.as_str())
                && e.status != Status::Detached
                && (e.missing_since.is_some() || discovery.complete)
        })
        .collect();
    c.sort_by_key(|(id, e)| {
        (
            e.status == Status::Excluded,
            e.last_lookup,
            e.missing_since.unwrap_or(now),
            (*id).clone(),
        )
    });
    c.into_iter()
        .take(MAX_LOOKUPS)
        .map(|(id, _)| id.clone())
        .collect()
}

pub fn plan(state: &MirrorState, input: &PlanInput<'_>) -> Plan {
    let mut p = Planner {
        input,
        state: state.clone(),
        actions: Vec::new(),
        changes: Vec::new(),
        creations: 0,
    };
    if let Some(login) = &input.discovery.login {
        p.state.token_login = Some(login.clone());
    }
    let mut seen = BTreeSet::new();
    for r in &input.discovery.repos {
        seen.insert(r.id.clone());
        p.seen(r, true);
    }
    let ids: Vec<String> = state.repos.keys().cloned().collect();
    for id in ids {
        if seen.contains(&id) {
            continue;
        }
        p.absent(&id);
    }
    Plan {
        state: p.state,
        actions: p.actions,
        changes: p.changes,
    }
}

struct Planner<'a, 'b> {
    input: &'a PlanInput<'b>,
    state: MirrorState,
    actions: Vec<Action>,
    changes: Vec<(String, String)>,
    creations: usize,
}

impl Planner<'_, '_> {
    fn cfg(&self) -> &GithubMirrorConfig {
        self.input.cfg
    }

    fn push(&mut self, id: &str, steps: Vec<Step>) {
        if !steps.is_empty() {
            self.actions.push(Action {
                id: id.to_string(),
                steps,
            });
        }
    }

    fn change(&mut self, id: &str, what: &str) {
        self.changes.push((id.to_string(), what.to_string()));
    }

    /// The forge reported `r`: by discovery (`listed`: one of the configured
    /// listings returned it), or by a Found lookup for an id a complete
    /// discovery lacked (`listed = false`: no listing has it any more, so it is
    /// out of the selection unless explicit in `repos` by its current or
    /// recorded name, which is the rename path; §B.6, §B.9).
    fn seen(&mut self, r: &RemoteRepo, listed: bool) {
        let now = self.input.now;
        let old = self.state.repos.get(&r.id).cloned();
        let explicit = is_explicit(self.cfg(), &r.full_name())
            || old
                .as_ref()
                .is_some_and(|e| is_explicit(self.cfg(), &e.full_name));
        // §B.9: `skip_archived` only stops a mirror from *starting*. A repository
        // already mirrored that is archived at the source keeps following (at
        // 24h), so its selection is judged as if it were not archived.
        let verdict = if !listed && !explicit {
            Verdict::Out
        } else if r.archived && old.as_ref().is_some_and(|e| e.floe.is_some()) {
            let live = RemoteRepo {
                archived: false,
                ..r.clone()
            };
            select(self.cfg(), &live, explicit)
        } else {
            select(self.cfg(), r, explicit)
        };
        let Some(old) = old else {
            self.new_repo(r, verdict);
            return;
        };
        let mut e = old.clone();
        e.observe(r, now);
        // Found but not selected stays missing (§B.6: cleared when seen selected).
        if verdict != Verdict::Out {
            e.missing_since = None;
        }
        if e.status == Status::Detached {
            self.state.repos.insert(r.id.clone(), e);
            return;
        }
        if verdict == Verdict::Out {
            if e.status != Status::Excluded {
                self.change(&r.id, "excluded");
            }
            e.status = Status::Excluded;
            let steps = self.publish_if_changed(&r.id, &e, "frozen: no longer selected");
            self.state.repos.insert(r.id.clone(), e);
            self.push(&r.id, steps);
            return;
        }
        if e.floe.is_none() {
            // Never created (too large, a conflict, an error before the create,
            // or excluded before it was ever mirrored): as a new repository.
            self.state.repos.insert(r.id.clone(), e);
            self.create(r, verdict, false);
            return;
        }
        let resumed = old.status.frozen();
        e.status = Status::Active;
        let reason = if resumed {
            "resumed".to_string()
        } else if old.full_name != e.full_name {
            tracing::info!(id = %r.id, from = %old.full_name, to = %e.full_name, floe = ?e.floe, "renamed at the source; floe name unchanged");
            format!("renamed from {}", old.full_name)
        } else if old.default_branch != e.default_branch {
            "default branch".to_string()
        } else if old.archived != e.archived {
            if e.archived { "archived" } else { "unarchived" }.to_string()
        } else {
            "settings".to_string()
        };
        let mut steps = Vec::new();
        // Convergent: a policy write that failed (or never ran: a crash after
        // the create, `read_only` turned on later) is retried every pass.
        if self.cfg().read_only && !e.policy {
            steps.push(Step::PutPolicy);
        }
        steps.extend(self.publish_if_changed(&r.id, &e, &reason));
        if resumed {
            self.change(&r.id, "resumed");
        } else if old.full_name != e.full_name {
            self.change(&r.id, "renamed");
        } else if old.archived != e.archived {
            self.change(&r.id, if e.archived { "archived" } else { "updated" });
        } else if !steps.is_empty() || old.private != e.private {
            self.change(&r.id, "updated");
        }
        // Never published yet (the first publish failed): the creation's nudge
        // is still owed.
        if resumed || old.pushed_at != e.pushed_at || old.settings_sha.is_none() {
            steps.push(Step::Nudge);
        }
        self.state.repos.insert(r.id.clone(), e);
        self.push(&r.id, steps);
    }

    fn new_repo(&mut self, r: &RemoteRepo, verdict: Verdict) {
        if verdict == Verdict::Out {
            return;
        }
        let mut e = RepoEntry::default();
        e.observe(r, self.input.now);
        self.state.repos.insert(r.id.clone(), e);
        self.create(r, verdict, true);
    }

    /// Plan the creation (or the too-large handoff check) of an entry without a
    /// floe repository. A new entry beyond `max_new_per_pass` is not recorded
    /// at all: the next discovery finds it again. A `conflict` retry costs an
    /// `exists` check per candidate name, not a creation, so it is not counted
    /// (stuck conflicts must not starve new repositories).
    fn create(&mut self, r: &RemoteRepo, verdict: Verdict, is_new: bool) {
        let read_only = self.cfg().read_only;
        let conflict_retry = !is_new
            && self
                .state
                .repos
                .get(&r.id)
                .is_some_and(|e| e.status == Status::Conflict);
        let mut steps = Vec::new();
        let status = if verdict == Verdict::TooLarge {
            if self
                .state
                .repos
                .get(&r.id)
                .is_none_or(|e| e.status != Status::TooLarge)
            {
                tracing::info!(id = %r.id, repo = %r.full_name(), size_kb = r.size_kb, "too large to mirror automatically: `floe import` it on an SSD host into its mapped name, then set [upstream] source = \"{}\"", settings::marker(self.input.source.kind(), &r.id));
                self.change(&r.id, "too-large");
            }
            steps.push(Step::Create {
                allow_create: false,
            });
            Status::TooLarge
        } else {
            if !conflict_retry {
                if self.creations >= self.cfg().max_new_per_pass {
                    if is_new {
                        self.state.repos.remove(&r.id);
                    }
                    return;
                }
                self.creations += 1;
            }
            steps.push(Step::Create { allow_create: true });
            // A conflict stays one until the claim succeeds (the applier
            // reports `conflict` only on the transition).
            if conflict_retry {
                Status::Conflict
            } else {
                Status::Active
            }
        };
        if read_only {
            steps.push(Step::PutPolicy);
        }
        steps.push(Step::Publish {
            reason: "created".into(),
        });
        steps.push(Step::Nudge);
        if let Some(e) = self.state.repos.get_mut(&r.id) {
            e.status = status;
        }
        self.push(&r.id, steps);
    }

    /// The forge did not report `id` this pass.
    fn absent(&mut self, id: &str) {
        let now = self.input.now;
        let complete = self.input.discovery.complete;
        let looked_up = self.input.lookups.get(id).cloned();
        let Some(mut e) = self.state.repos.get(id).cloned() else {
            return;
        };
        if complete && e.missing_since.is_none() {
            e.missing_since = Some(now);
        }
        if looked_up.is_some() {
            e.last_lookup = Some(now);
        }
        self.state.repos.insert(id.to_string(), e.clone());
        let Some(found) = looked_up else {
            return;
        };
        let (status, change) = match found {
            Lookup::Found(r) => {
                // Absent from a complete discovery yet Found: unstarred, removed
                // from `repos`, its owner dropped from `users`/`orgs`, or
                // transferred to an owner no listing covers. Not selected (frozen
                // as `excluded`) unless explicit in `repos` (a rename). An
                // incomplete discovery proves nothing: decided on the next
                // complete one.
                if complete
                    || is_explicit(self.cfg(), &r.full_name())
                    || is_explicit(self.cfg(), &e.full_name)
                {
                    self.seen(&r, false);
                }
                return;
            }
            Lookup::Gone => (Status::Gone, "gone"),
            Lookup::Forbidden => (Status::Forbidden, "forbidden"),
        };
        if e.status == Status::Detached || e.status == status {
            return;
        }
        let gone_after = chrono::Duration::from_std(self.cfg().gone_after)
            .unwrap_or_else(|_| chrono::Duration::days(1));
        let Some(since) = e.missing_since else {
            return;
        };
        if now.signed_duration_since(since) < gone_after {
            return;
        }
        e.status = status;
        self.change(id, change);
        let steps = self.publish_if_changed(id, &e, &format!("frozen: {change} at the source"));
        self.state.repos.insert(id.to_string(), e);
        self.push(id, steps);
    }

    /// A `Publish` step when the entry has a floe repository and its rendered
    /// table differs from the one last published.
    fn publish_if_changed(&self, id: &str, e: &RepoEntry, reason: &str) -> Vec<Step> {
        if e.floe.is_none() {
            return Vec::new();
        }
        let table = settings::render(self.input.source, self.cfg(), &e.to_remote(id), e.status);
        if e.settings_sha.as_deref() == Some(settings::hash(&table).as_str()) {
            return Vec::new();
        }
        vec![Step::Publish {
            reason: reason.to_string(),
        }]
    }
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)] // c(fg), r(epo), s(tate), p(lan), d(iscovery), t(ime)
mod tests {
    use super::*;
    use crate::fake::{FakeSource, remote};

    fn cfg() -> GithubMirrorConfig {
        GithubMirrorConfig {
            users: vec!["@me".into()],
            ..GithubMirrorConfig::default()
        }
    }

    fn t(h: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000 + h * 3600, 0).unwrap_or_default()
    }

    fn disc(repos: Vec<RemoteRepo>, complete: bool) -> Discovery {
        Discovery {
            repos,
            complete,
            ..Discovery::default()
        }
    }

    fn run(
        state: &MirrorState,
        cfg: &GithubMirrorConfig,
        d: &Discovery,
        lookups: &BTreeMap<String, Lookup>,
        now: DateTime<Utc>,
    ) -> Plan {
        let src = FakeSource::new("https://github.com");
        plan(
            state,
            &PlanInput {
                cfg,
                source: &src,
                now,
                discovery: d,
                lookups,
            },
        )
    }

    /// An entry as the applier leaves it after a successful creation.
    fn mirrored(state: &mut MirrorState, cfg: &GithubMirrorConfig, r: &RemoteRepo) {
        let src = FakeSource::new("https://github.com");
        let mut e = RepoEntry::default();
        e.observe(r, t(0));
        e.floe = Some(format!(
            "{}/{}",
            r.owner.to_lowercase(),
            r.name.to_lowercase()
        ));
        e.settings_sha = Some(settings::hash(&settings::render(
            &src,
            cfg,
            r,
            Status::Active,
        )));
        e.settings_revision = 1;
        e.policy = true;
        state.repos.insert(r.id.clone(), e);
    }

    fn steps(p: &Plan, id: &str) -> Vec<&'static str> {
        p.actions
            .iter()
            .filter(|a| a.id == id)
            .flat_map(|a| a.steps.iter().map(Step::name))
            .collect()
    }

    #[test]
    fn new_repository_is_created_then_idempotent() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        let p = run(
            &MirrorState::default(),
            &c,
            &disc(vec![r.clone()], true),
            &BTreeMap::new(),
            t(0),
        );
        assert_eq!(steps(&p, "1"), ["create", "policy", "publish", "nudge"]);
        let mut s = MirrorState::default();
        mirrored(&mut s, &c, &r);
        let p = run(&s, &c, &disc(vec![r], true), &BTreeMap::new(), t(1));
        assert!(p.actions.is_empty(), "{:?}", p.actions);
    }

    #[test]
    fn push_nudges_rename_and_default_branch_publish() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        let mut s = MirrorState::default();
        mirrored(&mut s, &c, &r);
        let mut pushed = r.clone();
        pushed.pushed_at = Some("2026-10-04T12:00:00Z".into());
        let p = run(&s, &c, &disc(vec![pushed], true), &BTreeMap::new(), t(1));
        assert_eq!(steps(&p, "1"), ["nudge"]);

        let mut renamed = r.clone();
        renamed.name = "Gadgets".into();
        let p = run(&s, &c, &disc(vec![renamed], true), &BTreeMap::new(), t(1));
        assert_eq!(steps(&p, "1"), ["publish"]);
        let e = p.state.repos.get("1").unwrap();
        assert_eq!(e.full_name, "Acme/Gadgets");
        assert_eq!(e.floe.as_deref(), Some("acme/widgets"));

        let mut branch = r.clone();
        branch.default_branch = Some("trunk".into());
        let p = run(&s, &c, &disc(vec![branch], true), &BTreeMap::new(), t(1));
        assert!(matches!(
            p.actions.first().and_then(|a| a.steps.first()),
            Some(Step::Publish { reason }) if reason == "default branch"
        ));

        let mut archived = r.clone();
        archived.archived = true;
        let p = run(&s, &c, &disc(vec![archived], true), &BTreeMap::new(), t(1));
        assert_eq!(
            steps(&p, "1"),
            ["publish"],
            "archived keeps following at 24h"
        );
        assert_eq!(p.state.repos.get("1").unwrap().status, Status::Active);

        let mut private = r;
        private.private = true;
        let p = run(&s, &c, &disc(vec![private], true), &BTreeMap::new(), t(1));
        assert!(p.actions.is_empty(), "visibility is state only");
        assert!(p.state.repos.get("1").unwrap().private);
    }

    #[test]
    fn incomplete_discovery_never_marks_missing_or_gone() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        let mut s = MirrorState::default();
        mirrored(&mut s, &c, &r);
        let d = disc(vec![], false);
        assert!(lookup_candidates(&s, &d, t(30)).is_empty());
        let lookups = BTreeMap::from([("1".to_string(), Lookup::Gone)]);
        let p = run(&s, &c, &d, &lookups, t(30));
        let e = p.state.repos.get("1").unwrap();
        assert!(e.missing_since.is_none());
        assert_eq!(e.status, Status::Active);
        assert!(p.actions.is_empty());
    }

    #[test]
    fn gone_only_after_gone_after_then_frozen_and_resumed() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        let mut s = MirrorState::default();
        mirrored(&mut s, &c, &r);
        let d = disc(vec![], true);
        let gone = BTreeMap::from([("1".to_string(), Lookup::Gone)]);
        let p = run(&s, &c, &d, &gone, t(1));
        let e = p.state.repos.get("1").unwrap();
        assert_eq!(e.missing_since, Some(t(1)));
        assert_eq!(e.last_lookup, Some(t(1)));
        assert_eq!(e.status, Status::Active);
        assert!(p.actions.is_empty());
        let p = run(&p.state, &c, &d, &gone, t(30));
        let e = p.state.repos.get("1").unwrap();
        assert_eq!(e.status, Status::Gone);
        assert_eq!(steps(&p, "1"), ["publish"]);
        assert_eq!(e.missing_since, Some(t(1)));

        // Forbidden likewise.
        let forbidden = BTreeMap::from([("1".to_string(), Lookup::Forbidden)]);
        let mut s2 = s.clone();
        if let Some(e) = s2.repos.get_mut("1") {
            e.missing_since = Some(t(0));
        }
        let p2 = run(&s2, &c, &d, &forbidden, t(30));
        assert_eq!(p2.state.repos.get("1").unwrap().status, Status::Forbidden);

        // Restored: seen again ⇒ active, publish the follow back, nudge.
        let mut frozen = p.state.clone();
        if let Some(e) = frozen.repos.get_mut("1") {
            let src = FakeSource::new("https://github.com");
            e.settings_sha = Some(settings::hash(&settings::render(
                &src,
                &c,
                &r,
                Status::Gone,
            )));
        }
        let p = run(&frozen, &c, &disc(vec![r], true), &BTreeMap::new(), t(31));
        let e = p.state.repos.get("1").unwrap();
        assert_eq!(e.status, Status::Active);
        assert!(e.missing_since.is_none());
        assert_eq!(steps(&p, "1"), ["publish", "nudge"]);
    }

    #[test]
    fn no_longer_selected_is_excluded_and_frozen() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        let mut s = MirrorState::default();
        mirrored(&mut s, &c, &r);
        let mut excl = c.clone();
        excl.exclude = vec!["acme/*".into()];
        let p = run(
            &s,
            &excl,
            &disc(vec![r.clone()], true),
            &BTreeMap::new(),
            t(1),
        );
        assert_eq!(p.state.repos.get("1").unwrap().status, Status::Excluded);
        assert_eq!(steps(&p, "1"), ["publish"]);

        // Transfer out of the selection: absent, lookup Found under a new name.
        let mut narrow = c.clone();
        narrow.include = vec!["acme/*".into()];
        let mut moved = r.clone();
        moved.owner = "Other".into();
        let lookups = BTreeMap::from([("1".to_string(), Lookup::Found(moved.clone()))]);
        let p = run(&s, &narrow, &disc(vec![], true), &lookups, t(1));
        let e = p.state.repos.get("1").unwrap();
        assert_eq!(e.status, Status::Excluded);
        assert_eq!(e.full_name, "Other/Widgets");
        // ... unless it is explicit by its old name: then it is a rename.
        let mut explicit = narrow.clone();
        explicit.repos = vec!["Acme/Widgets".into()];
        let p = run(&s, &explicit, &disc(vec![], true), &lookups, t(1));
        assert_eq!(p.state.repos.get("1").unwrap().status, Status::Active);
        assert_eq!(steps(&p, "1"), ["publish"]);
    }

    /// Found by lookup but in no listing any more: not selected, frozen once,
    /// and stays missing (no flip-flop, so lookups wind down to the excluded
    /// tail). Each way out of the selection: unstarred, dropped from `repos`,
    /// owner dropped from `users`.
    #[test]
    fn found_but_no_longer_listed_is_excluded_and_frozen() {
        let r = remote("1", "Acme", "Widgets");
        let starred = GithubMirrorConfig {
            starred: vec!["@me".into()],
            ..GithubMirrorConfig::default()
        };
        let listed = GithubMirrorConfig {
            repos: vec!["Acme/Widgets".into()],
            ..GithubMirrorConfig::default()
        };
        let user = GithubMirrorConfig {
            users: vec!["acme".into()],
            ..GithubMirrorConfig::default()
        };
        let unstarred = starred.clone();
        let mut dropped = listed.clone();
        dropped.repos = vec!["Acme/Other".into()];
        let mut no_user = user.clone();
        no_user.users = vec!["someone-else".into()];
        for (why, before, after) in [
            ("unstarred", &starred, &unstarred),
            ("removed from repos", &listed, &dropped),
            ("owner removed from users", &user, &no_user),
        ] {
            let mut s = MirrorState::default();
            mirrored(&mut s, before, &r);
            let lookups = BTreeMap::from([("1".to_string(), Lookup::Found(r.clone()))]);
            // Discovery under the new selection no longer returns it.
            let p = run(&s, after, &disc(vec![], true), &lookups, t(1));
            let e = p.state.repos.get("1").unwrap();
            assert_eq!(e.status, Status::Excluded, "{why}");
            assert!(e.missing_since.is_some(), "{why}: stays missing");
            assert_eq!(steps(&p, "1"), ["publish"], "{why}: frozen (follow = [])");
            assert_eq!(
                p.changes,
                [("1".to_string(), "excluded".to_string())],
                "{why}"
            );
            // The next pass: still excluded, nothing to publish, no new change.
            let mut s2 = p.state.clone();
            if let Some(e) = s2.repos.get_mut("1") {
                e.settings_sha = Some(settings::hash(&settings::render(
                    &FakeSource::new("https://github.com"),
                    after,
                    &e.to_remote("1"),
                    Status::Excluded,
                )));
            }
            let p = run(&s2, after, &disc(vec![], true), &lookups, t(2));
            assert_eq!(
                p.state.repos.get("1").unwrap().status,
                Status::Excluded,
                "{why}"
            );
            assert!(
                p.actions.is_empty() && p.changes.is_empty(),
                "{why}: {:?}",
                p.actions
            );
        }
        // An incomplete discovery proves nothing: the entry is left as it is.
        let mut s = MirrorState::default();
        mirrored(&mut s, &starred, &r);
        let lookups = BTreeMap::from([("1".to_string(), Lookup::Found(r.clone()))]);
        let p = run(&s, &unstarred, &disc(vec![], false), &lookups, t(1));
        assert_eq!(p.state.repos.get("1").unwrap().status, Status::Active);
        assert!(p.actions.is_empty(), "{:?}", p.actions);
    }

    #[test]
    fn detached_is_never_published() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        let mut s = MirrorState::default();
        mirrored(&mut s, &c, &r);
        if let Some(e) = s.repos.get_mut("1") {
            e.status = Status::Detached;
        }
        let mut renamed = r;
        renamed.name = "New".into();
        let p = run(&s, &c, &disc(vec![renamed], true), &BTreeMap::new(), t(1));
        assert!(p.actions.is_empty());
        assert_eq!(p.state.repos.get("1").unwrap().full_name, "Acme/New");
        assert!(lookup_candidates(&p.state, &disc(vec![], true), t(2)).is_empty());
    }

    #[test]
    fn too_large_is_only_adopted_and_max_new_bounds_creations() {
        let mut c = cfg();
        c.max_new_per_pass = 2;
        let mut big = remote("9", "Acme", "Big");
        big.size_kb = 10 * 1024 * 1024;
        let repos = vec![
            remote("1", "a", "x"),
            remote("2", "a", "y"),
            remote("3", "a", "z"),
            big,
        ];
        let p = run(
            &MirrorState::default(),
            &c,
            &disc(repos, true),
            &BTreeMap::new(),
            t(0),
        );
        let creates = p
            .actions
            .iter()
            .filter(|a| a.steps.first() == Some(&Step::Create { allow_create: true }))
            .count();
        assert_eq!(creates, 2);
        assert!(
            !p.state.repos.contains_key("3"),
            "beyond the bound: not recorded"
        );
        assert_eq!(p.state.repos.get("9").unwrap().status, Status::TooLarge);
        assert_eq!(
            p.actions
                .iter()
                .find(|a| a.id == "9")
                .and_then(|a| a.steps.first()),
            Some(&Step::Create {
                allow_create: false
            })
        );
    }

    #[test]
    fn a_missing_policy_and_first_publish_are_retried() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        // The create landed; the policy write (and so the publish) failed.
        let mut s = MirrorState::default();
        let mut e = RepoEntry::default();
        e.observe(&r, t(0));
        e.floe = Some("acme/widgets".into());
        e.status = Status::Error;
        s.repos.insert("1".into(), e);
        let p = run(&s, &c, &disc(vec![r.clone()], true), &BTreeMap::new(), t(1));
        assert_eq!(steps(&p, "1"), ["policy", "publish", "nudge"]);
        assert_eq!(p.state.repos.get("1").unwrap().status, Status::Active);
        // `read_only` off: no policy, the owed publish and nudge only.
        let mut rw = c.clone();
        rw.read_only = false;
        let p = run(&s, &rw, &disc(vec![r], true), &BTreeMap::new(), t(1));
        assert_eq!(steps(&p, "1"), ["publish", "nudge"]);
    }

    #[test]
    fn conflicts_do_not_use_the_creation_budget() {
        let mut c = cfg();
        c.max_new_per_pass = 2;
        let mut s = MirrorState::default();
        let mut repos = Vec::new();
        for id in ["1", "2", "3"] {
            let r = remote(id, "a", &format!("r{id}"));
            let mut e = RepoEntry::default();
            e.observe(&r, t(0));
            e.status = Status::Conflict;
            s.repos.insert(id.into(), e);
            repos.push(r);
        }
        repos.push(remote("9", "a", "new"));
        let p = run(&s, &c, &disc(repos, true), &BTreeMap::new(), t(1));
        assert_eq!(steps(&p, "9").first(), Some(&"create"), "{:?}", p.actions);
        assert_eq!(steps(&p, "1").first(), Some(&"create"), "still retried");
        assert_eq!(p.state.repos.get("1").unwrap().status, Status::Conflict);
        assert!(
            p.changes.iter().all(|(_, what)| what != "created"),
            "{:?}",
            p.changes
        );
    }

    #[test]
    fn found_but_unselected_stays_missing() {
        let c = cfg();
        let r = remote("1", "Acme", "Widgets");
        let mut s = MirrorState::default();
        mirrored(&mut s, &c, &r);
        if let Some(e) = s.repos.get_mut("1") {
            e.missing_since = Some(t(0));
        }
        let mut excl = c.clone();
        excl.exclude = vec!["acme/*".into()];
        let lookups = BTreeMap::from([("1".to_string(), Lookup::Found(r))]);
        let p = run(&s, &excl, &disc(vec![], true), &lookups, t(1));
        let e = p.state.repos.get("1").unwrap();
        assert_eq!(e.status, Status::Excluded);
        assert_eq!(e.missing_since, Some(t(0)));
    }

    #[test]
    fn lookup_order_is_oldest_last_lookup_first() {
        let mut s = MirrorState::default();
        for (id, last) in [("a", Some(t(5))), ("b", None), ("c", Some(t(1)))] {
            s.repos.insert(
                id.into(),
                RepoEntry {
                    missing_since: Some(t(0)),
                    last_lookup: last,
                    ..RepoEntry::default()
                },
            );
        }
        s.repos.insert("d".into(), RepoEntry::default());
        assert_eq!(
            lookup_candidates(&s, &disc(vec![], false), t(9)),
            ["b", "c", "a"],
            "incomplete: only ids already missing"
        );
        assert_eq!(
            lookup_candidates(&s, &disc(vec![], true), t(9)),
            ["b", "d", "c", "a"]
        );
    }
}
