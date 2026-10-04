//! Upstream follow — ingress through the WAL. Refs of a repository matching the
//! `[upstream] follow` patterns (D48: git refspec globs, `^` negatives) follow the
//! same refs on an upstream git host (`[upstream] git`, D24 settings or host
//! config), performed by the host that maintains the repository (D28: its writer):
//!
//! every `maintenance.follow_interval` the loop syncs refs only and **probes**
//! upstream (one `ls-remote`); when a matching ref differs, it runs the `follow` op
//! as a task (the same `(repo, "follow")` task lock a manual op uses, so one fetch
//! touches the scratch at a time): Serve-level sync, fetch the delta into a scratch
//! repository over the serving copy's objects (`floe_git::follow`), stream the pack
//! through `ingest_pack`, classify each change (fast-forward or rewrite, on the
//! serving copy after ingest), connectivity, one `publish_push` — the same PUSH
//! entry receive-pack publishes, `principal = upstream`. With `on_rewrite =
//! "archive"` (default) a rewritten or deleted ref's old tip is kept under
//! `refs/archive/<unix-ts>/<ref>` **in that same entry** (`plan.rs`); nothing is
//! lost. `"refuse"` is D33: rewinds and deletions are left as is and reported
//! `refused` every round — without a task once the loop knows the op would refuse
//! (a deletion, a tag move, a rewind already refused at the same oids). Policy is not evaluated (follow is configuration, not a principal).
//!
//! Its own loop, not a unit of the priority loop (`maintain.rs`): ingress must not
//! wait behind a 30-minute base rebuild, and as the top unit it would starve the
//! derived work of a busy repository. No task, and no Serve-level sync, for a round
//! that found nothing.

mod plan;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use floe_config::OnRewrite;
use floe_config::refpattern::RefPatterns;
use floe_git::{IngestOptions, RepoId};
use floe_proto::v1::RefTransaction;
use tracing::{Instrument, debug, info, warn};

use crate::AppState;
use plan::{Change, HeadWant, Kind, Observed, short};

/// Run forever: a round every `maintenance.follow_interval` (0 = never).
pub async fn run_loop(state: Arc<AppState>) {
    let interval = state.cfg.maintenance.follow_interval;
    if interval.is_zero() {
        return;
    }
    info!(interval = ?interval, "upstream follow loop started");
    loop {
        tokio::time::sleep(interval).await;
        if floe_wal::tasks::draining() {
            info!("upstream follow loop: draining, no new round");
            return;
        }
        match run_pass(&state).await {
            Ok(r) if r.behind > 0 || r.failed > 0 => info!(
                repos = r.repos,
                behind = r.behind,
                published = r.published,
                failed = r.failed,
                "upstream follow round"
            ),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "upstream follow round failed"),
        }
    }
}

/// What the last round did for a repository on this instance (the Settings tab
/// shows it next to the configuration; per instance, like tasks — the maintaining
/// host answers the repository's routes).
#[derive(Debug, Clone, serde::Serialize)]
pub struct FollowStatus {
    /// RFC 3339.
    pub at: String,
    /// `in-sync` | `published` | `archived` (published, with at least one archive) |
    /// `refused` | `failed`
    pub outcome: &'static str,
    /// Human line: what moved / why it did not.
    pub detail: String,
    /// `ref → oid` upstream has (matching refs, as probed this round).
    pub upstream: HashMap<String, String>,
    /// `ref → oid` the WAL had before the round (matching refs).
    pub ours: HashMap<String, String>,
    /// Human lines: `<original> → <archive ref>` kept by this round (D48).
    pub archived: Vec<String>,
    /// When the round ended (`upstream.follow_interval`); not serialized.
    #[serde(skip)]
    pub finished: Instant,
    /// Policy `refuse`: rewinds already refused at these exact old/new oids (the
    /// loop does not start the op again for them); not serialized.
    #[serde(skip)]
    refused_rewinds: Vec<Change>,
}

/// Per-repo last-round status on this instance.
#[derive(Default)]
pub struct FollowStatuses(parking_lot::Mutex<HashMap<String, FollowStatus>>);

/// One round's outcome for [`FollowStatuses::set`].
struct Round {
    outcome: &'static str,
    detail: String,
    upstream: HashMap<String, String>,
    ours: HashMap<String, String>,
    archived: Vec<String>,
    /// Policy `refuse`: rewinds refused at these exact old/new oids.
    refused_rewinds: Vec<Change>,
}

impl Round {
    fn failed(detail: String) -> Self {
        Round {
            outcome: "failed",
            detail,
            upstream: HashMap::new(),
            ours: HashMap::new(),
            archived: Vec::new(),
            refused_rewinds: Vec::new(),
        }
    }
}

impl FollowStatuses {
    pub fn get(&self, repo: &str) -> Option<FollowStatus> {
        self.0.lock().get(repo).cloned()
    }
    fn finished(&self, repo: &str) -> Option<Instant> {
        self.0.lock().get(repo).map(|s| s.finished)
    }
    /// The last round's outcome and detail.
    fn last(&self, repo: &str) -> Option<(&'static str, String)> {
        self.0
            .lock()
            .get(repo)
            .map(|s| (s.outcome, s.detail.clone()))
    }
    fn refused_rewinds(&self, repo: &str) -> Vec<Change> {
        self.0
            .lock()
            .get(repo)
            .map(|s| s.refused_rewinds.clone())
            .unwrap_or_default()
    }
    fn set(&self, repo: &str, r: Round) {
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        self.0.lock().insert(
            repo.to_string(),
            FollowStatus {
                at,
                outcome: r.outcome,
                detail: r.detail,
                upstream: r.upstream,
                ours: r.ours,
                archived: r.archived,
                finished: Instant::now(),
                refused_rewinds: r.refused_rewinds,
            },
        );
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct FollowReport {
    /// Assigned repositories with `upstream.follow` set.
    pub repos: usize,
    /// Of those, how many had a matching ref that differs from upstream's.
    pub behind: usize,
    /// `follow` ops that published.
    pub published: usize,
    pub failed: usize,
}

/// One round: for every assigned repository that follows an upstream, sync refs
/// and probe upstream (no task, no Serve-level sync, no pack prefetch); when a
/// matching ref differs (or HEAD wants a move), run the `follow` op on it. With
/// `on_rewrite = "refuse"`, changes the op would refuse anyway (a deletion, a tag
/// move, a rewind it already refused at the same old/new) are reported `refused`
/// here, without a task. One repository's failure never ends the pass.
#[allow(clippy::too_many_lines)] // one linear round per repository, kept in order
pub async fn run_pass(state: &Arc<AppState>) -> anyhow::Result<FollowReport> {
    let mut report = FollowReport::default();
    for id in state.registry.list().await? {
        if !state.cfg.placement.maintains(id.owner(), id.name()) {
            continue;
        }
        if floe_wal::tasks::draining() {
            break;
        }
        let handle = match state.registry.open(&id).await {
            Ok(h) => h,
            Err(e) => {
                report.failed += 1;
                warn!(repo = %id, error = %e, "follow: opening the repository failed; next repository");
                continue;
            }
        };
        // Refs only, and no background pack prefetch: an in-sync round must not
        // re-materialize the packs of a repository the LRU evicted. The manifest
        // carries the settings (D24).
        if let Err(e) = handle.sync_refs_only().await.map(drop) {
            report.failed += 1;
            warn!(repo = %id, error = %e, "follow: syncing refs failed; next repository");
            continue;
        }
        let cfg = handle.effective_config();
        let Some(upstream) = cfg.upstream.git.clone() else {
            continue;
        };
        if cfg.upstream.follow.is_empty() {
            continue;
        }
        report.repos += 1;
        let patterns = match RefPatterns::parse(&cfg.upstream.follow) {
            Ok(p) => p,
            Err(e) => {
                // Settings are validated at publish; only a host config can get here.
                warn!(repo = %id, error = %e, "follow: invalid upstream.follow; skipping");
                continue;
            }
        };
        let repo = id.to_string();
        if let Some(min) = cfg.upstream.follow_interval
            && state
                .follow
                .finished(&repo)
                .is_some_and(|t| t.elapsed() < min)
        {
            continue;
        }
        let t0 = Instant::now();
        let started = chrono::Utc::now();
        let probed = async {
            let snapshot = handle.local().refs()?;
            let have = matching(&snapshot, &patterns);
            let token = token_for(state, &cfg, &upstream).await?;
            let probe = floe_git::follow::probe(&upstream, token.as_deref(), &patterns).await?;
            anyhow::Ok((head_want(&cfg, &snapshot), have, probe))
        }
        .await;
        let (want, have, probe) = match probed {
            Ok(p) => p,
            Err(e) => {
                report.failed += 1;
                metrics::counter!("floe_follow_rounds_total", "repo" => repo.clone(), "outcome" => "fetch-failed").increment(1);
                warn!(repo = %id, %upstream, error = format!("{e:#}"), elapsed_ms = elapsed_ms(t0), "follow: probing upstream failed");
                let round = Round::failed(format!("probe of {upstream} failed: {e:#}"));
                record_run(state, &cfg, &repo, started, &round, RunStats::default());
                state.follow.set(&repo, round);
                continue;
            }
        };
        let (mut changes, notice) = plan::diff(&Observed {
            have: &have,
            tips: &probe.tips,
            advertised_any: probe.advertised_any,
        });
        // `refuse`: what the op would refuse without looking (deletions, tag moves)
        // or already refused at these exact old/new oids is reported here; only the
        // rest is worth a task and a Serve-level sync.
        let mut refused: Vec<String> = notice.into_iter().collect();
        let mut rewinds: Vec<Change> = Vec::new();
        if cfg.upstream.on_rewrite == OnRewrite::Refuse {
            let last = state.follow.refused_rewinds(&repo);
            let (skip, rest): (Vec<Change>, Vec<Change>) = changes
                .into_iter()
                .partition(|c| c.fixed_kind() == Some(Kind::Rewrite) || last.contains(c));
            changes = rest;
            rewinds = skip.iter().filter(|c| last.contains(c)).cloned().collect();
            let skip = skip.into_iter().map(|c| (c, Kind::Rewrite)).collect();
            refused.extend(plan::build(skip, OnRewrite::Refuse, 0, None).refused);
        }
        let head_moves = want
            .as_ref()
            .is_some_and(|h| h.target != h.current && h.exists_now);
        if changes.is_empty() && !head_moves {
            debug!(repo = %id, %upstream, elapsed_ms = elapsed_ms(t0), "follow: in sync");
            let (outcome, detail) = if refused.is_empty() {
                (
                    "in-sync",
                    format!(
                        "{} up to date with {upstream}",
                        cfg.upstream.follow.join(", ")
                    ),
                )
            } else {
                ("refused", refused.join("; "))
            };
            let round = Round {
                outcome,
                detail,
                upstream: probe.tips,
                ours: have,
                archived: Vec::new(),
                refused_rewinds: rewinds,
            };
            record_run(state, &cfg, &repo, started, &round, RunStats::default());
            state.follow.set(&repo, round);
            continue;
        }
        report.behind += 1;
        if !handle.packs_fit() {
            report.failed += 1;
            warn!(repo = %id, "follow: the whole object set must be local on this host (negotiation + thin-pack bases); skipping");
            let round = Round {
                upstream: probe.tips,
                ours: have,
                ..Round::failed(
                    "behind upstream, but this repository's object set does not fit this host's cache (follow needs it local); skipped".into(),
                )
            };
            record_run(state, &cfg, &repo, started, &round, RunStats::default());
            state.follow.set(&repo, round);
            continue;
        }
        let mut stats = RunStats::default();
        let round = match run_op(state, &id, HashMap::new()).await {
            Ok(v) => {
                let n = v
                    .get("published")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                if n > 0 {
                    report.published += 1;
                }
                let seq = v.get("seq").and_then(serde_json::Value::as_u64);
                stats = RunStats {
                    published: Some(n),
                    seq,
                };
                let strings = |key: &str| -> Vec<String> {
                    v.get(key)
                        .and_then(|r| r.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                extend_unique(&mut refused, strings("refused"));
                let archived = strings("archived");
                let mut detail = match seq {
                    Some(seq) => format!("{n} ref(s) published at seq {seq}"),
                    None => "nothing published".to_string(),
                };
                if !archived.is_empty() {
                    let _ = write!(detail, "; archived: {}", archived.join("; "));
                }
                if !refused.is_empty() {
                    let _ = write!(detail, "; refused: {}", refused.join("; "));
                }
                let outcome = match (n > 0, archived.is_empty(), refused.is_empty()) {
                    // The op's own probe found nothing to do (upstream moved back
                    // between the loop's probe and the op's).
                    (false, _, true) => {
                        detail = format!(
                            "{} up to date with {upstream}",
                            cfg.upstream.follow.join(", ")
                        );
                        "in-sync"
                    }
                    (false, _, false) => "refused",
                    (true, true, _) => "published",
                    (true, false, _) => "archived",
                };
                Round {
                    outcome,
                    detail,
                    upstream: probe.tips,
                    ours: have,
                    archived,
                    refused_rewinds: rewinds,
                }
            }
            Err(why) => {
                report.failed += 1;
                // A plan that `refuse` left empty is a policy outcome; anything else
                // (ancestry, fetch, unpack, connectivity, publish) is a failure.
                if let Some(rest) = why.strip_prefix(NOTHING_PUBLISHABLE) {
                    // Every change the op saw was refused: remember the rewinds so
                    // the next rounds do not repeat the task while they stand.
                    rewinds.extend(
                        changes
                            .into_iter()
                            .filter(|c| matches!(c, Change::Update { .. })),
                    );
                    extend_unique(
                        &mut refused,
                        rest.trim_start_matches([' ', '—'])
                            .split("; ")
                            .map(String::from),
                    );
                    Round {
                        outcome: "refused",
                        detail: refused.join("; "),
                        upstream: probe.tips,
                        ours: have,
                        archived: Vec::new(),
                        refused_rewinds: rewinds,
                    }
                } else {
                    Round {
                        upstream: probe.tips,
                        ours: have,
                        ..Round::failed(why)
                    }
                }
            }
        };
        record_run(state, &cfg, &repo, started, &round, stats);
        state.follow.set(&repo, round);
    }
    Ok(report)
}

/// What a round's op reported, for [`record_run`].
#[derive(Debug, Default, Clone, Copy)]
struct RunStats {
    published: Option<u64>,
    seq: Option<u64>,
}

/// Catalog telemetry for a round that did work, was refused or failed
/// (`sync_runs`, D50): lossy, after the write finished, never awaited — like a
/// metric. Call before `state.follow.set` (it compares with the last round).
fn record_run(
    state: &AppState,
    cfg: &floe_config::Config,
    repo: &str,
    started: chrono::DateTime<chrono::Utc>,
    round: &Round,
    stats: RunStats,
) {
    let last = state.follow.last(repo);
    if !worth_recording(last.as_ref().map(|(o, d)| (*o, d.as_str())), round) {
        return;
    }
    state.recorder.record_sync_run(floe_catalog::SyncRun {
        source: source_kind(cfg.upstream.source.as_deref()),
        repo: Some(repo.to_string()),
        finished_at: chrono::Utc::now(),
        outcome: round.outcome.to_string(),
        refs_published: stats.published.and_then(|n| u32::try_from(n).ok()),
        refs_archived: u32::try_from(round.archived.len()).ok(),
        seq: stats.seq,
        detail: Some(round.detail.clone()),
        ..floe_catalog::SyncRun::new("follow", started)
    });
}

/// Whether a round is a `sync_runs` row, given the repository's last round on
/// this instance: never an in-sync one, and a refused or failed one only when
/// it differs from the last (a standing refusal or an unreachable upstream
/// would otherwise add one identical row per repository per tick).
fn worth_recording(last: Option<(&str, &str)>, round: &Round) -> bool {
    match round.outcome {
        "in-sync" => false,
        "refused" | "failed" => last != Some((round.outcome, round.detail.as_str())),
        _ => true,
    }
}

/// `upstream.source` (`github:<id>`) → the run's `source` (`github`).
fn source_kind(source: Option<&str>) -> Option<String> {
    source.map(|s| s.split_once(':').map_or(s, |(kind, _)| kind).to_string())
}

/// The op's error when policy `refuse` left nothing to publish (the loop tells it
/// apart from a failure by this prefix).
const NOTHING_PUBLISHABLE: &str = "follow: nothing publishable";

/// Start the `follow` op as a task and wait for it: its result value, or why it failed.
async fn run_op(
    state: &Arc<AppState>,
    id: &RepoId,
    params: HashMap<String, String>,
) -> Result<serde_json::Value, String> {
    let task = match crate::ops::start(state.clone(), id.clone(), "follow", params).await {
        Ok(t) | Err(crate::ops::StartError::AlreadyRunning(t)) => t,
        Err(crate::ops::StartError::UnknownOp) => return Err("follow: unknown op".into()),
    };
    if !task.wait_done(std::time::Duration::from_secs(3600)).await {
        warn!(repo = %id, "follow: op still running after 1h; moving on");
        return Err("follow: op still running after 1h".into());
    }
    match task.outcome() {
        Some(Ok(o)) => Ok(o.value.unwrap_or(serde_json::Value::Null)),
        Some(Err((_, why))) => Err(why),
        None => Err("follow: op finished without an outcome".into()),
    }
}

/// The `follow` op (`ops.rs`; the loop starts it when the probe saw a difference,
/// a human from the UI/CLI): Serve-level sync, fetch, diff, ingest the pack like a
/// push, classify, plan (archive or refuse rewrites), connectivity, one publish.
#[allow(clippy::too_many_lines)] // one linear round, kept in order
pub(crate) async fn op(
    state: &Arc<AppState>,
    handle: &floe_wal::RepoHandle,
    id: &RepoId,
    _params: &HashMap<String, String>,
    log: crate::ops::Log<'_>,
) -> Result<(String, serde_json::Value), String> {
    let cfg = handle.effective_config();
    let upstream = cfg
        .upstream
        .git
        .clone()
        .ok_or("follow: no upstream.git for this repository")?;
    if cfg.upstream.follow.is_empty() {
        return Err("follow: upstream.follow is empty for this repository".into());
    }
    let patterns = RefPatterns::parse(&cfg.upstream.follow).map_err(|e| format!("follow: {e}"))?;
    let t0 = Instant::now();
    let guard = handle.sync().await.map_err(|e| format!("sync: {e}"))?;
    let local = handle.local().clone();
    let snapshot = local.refs().map_err(|e| format!("refs: {e}"))?;
    let have = matching(&snapshot, &patterns);
    let token = token_for(state, &cfg, &upstream)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let probe = floe_git::follow::probe(&upstream, token.as_deref(), &patterns)
        .await
        .map_err(|e| format!("probe of upstream: {e}"))?;
    log(format!(
        "fetching {} from {upstream}",
        cfg.upstream.follow.join(", ")
    ));
    let delta = floe_git::follow::fetch_refs(
        &upstream,
        token.as_deref(),
        &local.path().join("objects"),
        &have,
        &patterns,
        &probe,
        &scratch_dir(state, id),
    )
    .await
    .map_err(|e| format!("fetch from upstream: {e}"))?;

    let (changes, notice) = plan::diff(&Observed {
        have: &have,
        tips: &delta.tips,
        advertised_any: probe.advertised_any,
    });
    if let Some(n) = &notice {
        log(n.clone());
    }
    let want = head_want(&cfg, &snapshot);
    let head_moves = want
        .as_ref()
        .is_some_and(|h| h.target != h.current && h.exists_now);
    if changes.is_empty() && !head_moves {
        delta.discard_pack().await;
        return Ok((
            "in sync with upstream".into(),
            serde_json::json!({"published": 0, "refused": notice.into_iter().collect::<Vec<_>>()}),
        ));
    }

    // Objects: the fetched pack goes through the same ingest as a push (the scratch
    // completed it from our own objects, so it is not thin).
    let ingested = match &delta.pack {
        Some(p) => {
            let bytes = tokio::fs::metadata(p).await.map_or(0, |m| m.len());
            log(format!("ingesting {bytes} bytes of objects from upstream"));
            let file = tokio::fs::File::open(p)
                .await
                .map_err(|e| format!("opening the fetched pack: {e}"))?;
            local
                .ingest_pack(
                    file,
                    IngestOptions {
                        fsck: cfg.wal.fsck_objects,
                        max_bytes: None,
                        thin: false,
                    },
                )
                .await
                .map_err(|e| format!("unpack failed: {e}"))?
        }
        None => None, // a ref moved to objects we already hold (e.g. a rewind)
    };
    // Classify after ingest: before it the new commits are not in the serving copy
    // and every fast-forward would read as a rewrite. "Cannot tell" fails the round.
    let mut classified: Vec<(Change, Kind)> = Vec::with_capacity(changes.len());
    for c in changes {
        let kind = match (c.fixed_kind(), &c) {
            (Some(k), _) => k,
            (None, Change::Update { name, old, new }) => match local.is_ancestor(old, new).await {
                Ok(true) => Kind::FastForward,
                Ok(false) => Kind::Rewrite,
                Err(e) => {
                    delta.discard_pack().await;
                    return Err(format!(
                        "follow: {name}: cannot tell whether {}→{} is a fast-forward: {e}",
                        short(old),
                        short(new)
                    ));
                }
            },
            (None, _) => Kind::Rewrite,
        };
        classified.push((c, kind));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let ts = plan::archive_ts(now, snapshot.refs.iter().map(|r| r.name.as_str()));
    let mut planned = plan::build(classified, cfg.upstream.on_rewrite, ts, want.as_ref());
    if let Some(n) = notice {
        planned.refused.push(n);
    }
    if planned.moves_head()
        && let Some(h) = &want
    {
        log(format!("HEAD: {} → {}", h.current, h.target));
    }
    if cfg.wal.check_connectivity {
        // Archive refs point at objects already held; deletes and HEAD have no object.
        let tips: Vec<gix_hash::ObjectId> = planned
            .updates
            .iter()
            .filter(|u| !u.name.starts_with(plan::ARCHIVE_PREFIX))
            .filter_map(|u| gix_hash::ObjectId::from_hex(u.new_oid.as_bytes()).ok())
            .collect();
        local
            .check_connectivity_async(&tips, true)
            .instrument(tracing::info_span!(
                "follow.connectivity",
                tips = tips.len()
            ))
            .await
            .map_err(|e| format!("connectivity: {e}"))?;
    }
    drop(guard);
    let mut refused = std::mem::take(&mut planned.refused);
    if planned.updates.is_empty() {
        delta.discard_pack().await;
        metrics::counter!("floe_follow_rounds_total", "repo" => id.to_string(), "outcome" => "refused").increment(1);
        return Err(format!("{NOTHING_PUBLISHABLE} — {}", refused.join("; ")));
    }
    let archived_meta = planned.archived_meta();
    let mut txn = RefTransaction {
        updates: std::mem::take(&mut planned.updates),
        ..Default::default()
    };
    local.fill_peeled(&mut txn);
    let mut meta = HashMap::from([
        ("principal".to_string(), "upstream".to_string()),
        ("upstream".to_string(), upstream.clone()),
        ("agent".to_string(), "floe follow".to_string()),
    ]);
    if let Some(a) = archived_meta {
        meta.insert("follow.archived".to_string(), a);
    }
    let res = handle
        .publish_push(ingested, txn, meta)
        .await
        .map_err(|e| format!("publish: {e}"))?;
    delta.discard_pack().await;
    // The round is one transaction (D48): all of it or nothing.
    let rejected: Vec<String> = res
        .per_ref
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name}: {e}")))
        .collect();
    if !rejected.is_empty() {
        refused.extend(rejected);
        metrics::counter!("floe_follow_rounds_total", "repo" => id.to_string(), "outcome" => "refused").increment(1);
        let summary = format!(
            "round rejected (a ref moved under it; the next round re-plans): {}",
            refused.join("; ")
        );
        log(summary.clone());
        return Ok((
            summary,
            serde_json::json!({"published": 0, "refused": refused}),
        ));
    }
    let published = res
        .per_ref
        .iter()
        .filter(|(name, _)| !name.starts_with(plan::ARCHIVE_PREFIX))
        .count() as u64;
    let archived: Vec<String> = planned
        .archived
        .iter()
        .map(|a| format!("{} → {}", a.original, a.archive_ref))
        .collect();
    for a in &planned.archived {
        let kind = if a.new.is_some() { "rewrite" } else { "delete" };
        log(format!(
            "{}: {} upstream; old tip {} kept as {}",
            a.original,
            if a.new.is_some() {
                "rewritten"
            } else {
                "deleted"
            },
            short(&a.old),
            a.archive_ref
        ));
        metrics::counter!("floe_follow_archived_total", "repo" => id.to_string(), "kind" => kind)
            .increment(1);
    }
    let outcome = if archived.is_empty() {
        "published"
    } else {
        "archived"
    };
    metrics::counter!("floe_follow_rounds_total", "repo" => id.to_string(), "outcome" => outcome)
        .increment(1);
    metrics::counter!("floe_follow_refs_total", "repo" => id.to_string()).increment(published);
    info!(repo = %id, seq = res.seq, refs = published, archived = archived.len(), refused = refused.len(), %upstream, source = cfg.upstream.source.as_deref().unwrap_or(""), elapsed_ms = elapsed_ms(t0), "follow published");
    log(format!(
        "{published} ref update(s) published at seq {}",
        res.seq
    ));
    let mut summary = format!(
        "{published} ref(s) from upstream published at seq {} in {:.1}s",
        res.seq,
        t0.elapsed().as_secs_f64(),
    );
    if !archived.is_empty() {
        let _ = write!(summary, "; archived: {}", archived.join("; "));
    }
    if !refused.is_empty() {
        let _ = write!(summary, "; refused: {}", refused.join("; "));
    }
    Ok((
        summary,
        serde_json::json!({"published": published, "seq": res.seq, "refused": refused, "archived": archived}),
    ))
}

/// Append the lines of `more` not already in `into` (the loop and the op build
/// the same `refused` lines for the same change).
fn extend_unique(into: &mut Vec<String>, more: impl IntoIterator<Item = String>) {
    for l in more {
        if !l.is_empty() && !into.contains(&l) {
            into.push(l);
        }
    }
}

/// The WAL's refs matching `patterns` (from the synced local copy's snapshot).
fn matching(
    snapshot: &floe_git::RefSnapshotData,
    patterns: &RefPatterns,
) -> HashMap<String, String> {
    snapshot
        .refs
        .iter()
        .filter(|r| patterns.matches(&r.name))
        .map(|r| (r.name.clone(), r.oid.clone()))
        .collect()
}

/// `[upstream] head` against the snapshot, when set.
fn head_want(cfg: &floe_config::Config, snapshot: &floe_git::RefSnapshotData) -> Option<HeadWant> {
    let target = cfg.upstream.head.clone()?;
    Some(HeadWant {
        exists_now: snapshot.refs.iter().any(|r| r.name == target),
        current: snapshot.head_target.clone(),
        target,
    })
}

async fn token_for(
    state: &AppState,
    cfg: &floe_config::Config,
    upstream: &str,
) -> anyhow::Result<Option<String>> {
    match cfg.upstream_token_env(upstream) {
        Some(name) => Ok(Some(
            state
                .lfs_upstream
                .secret(name)
                .await
                .map_err(|e| anyhow::anyhow!("upstream token: {e}"))?,
        )),
        None => Ok(None),
    }
}

/// `cache.dir/follow/<owner>/<name>.git` — the persistent scratch over the serving copy.
fn scratch_dir(state: &AppState, id: &RepoId) -> PathBuf {
    state
        .cfg
        .cache
        .dir
        .join("follow")
        .join(id.owner())
        .join(format!("{}.git", id.name()))
}

fn elapsed_ms(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(outcome: &'static str, detail: &str) -> Round {
        Round {
            outcome,
            ..Round::failed(detail.into())
        }
    }

    /// In-sync rounds are never rows; work always is; a refusal or failure is
    /// one row until it changes, not one per tick.
    #[test]
    fn rounds_worth_a_sync_run() {
        assert!(!worth_recording(None, &round("in-sync", "up to date")));
        assert!(worth_recording(None, &round("published", "1 ref(s)")));
        assert!(worth_recording(
            Some(("published", "1 ref(s)")),
            &round("published", "1 ref(s)")
        ));
        let refused = round("refused", "refs/tags/v1: tag moved");
        assert!(worth_recording(None, &refused));
        assert!(worth_recording(Some(("in-sync", "x")), &refused));
        assert!(!worth_recording(
            Some(("refused", "refs/tags/v1: tag moved")),
            &refused
        ));
        assert!(worth_recording(
            Some(("refused", "refs/heads/gone: deleted upstream")),
            &refused
        ));
        let failed = round("failed", "probe failed: timeout");
        assert!(worth_recording(
            Some(("refused", "probe failed: timeout")),
            &failed
        ));
        assert!(!worth_recording(
            Some(("failed", "probe failed: timeout")),
            &failed
        ));
    }

    #[test]
    fn run_source_is_the_kind() {
        assert_eq!(source_kind(Some("github:1")).as_deref(), Some("github"));
        assert_eq!(source_kind(Some("github")).as_deref(), Some("github"));
        assert_eq!(source_kind(None), None);
    }
}
