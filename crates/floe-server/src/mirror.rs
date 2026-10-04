//! The server side of the GitHub mirror (D49, `docs/design/github-mirror.md`
//! §B.8/§D.2): the placement-gated follow nudge and the loop's spawn. The only
//! coupling between the server and `floe-mirror`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use floe_git::RepoId;
use tokio::sync::Semaphore;

use crate::AppState;

/// Nudged follows running at once on this host: a pass can nudge every new
/// repository (initial clones) and every pushed one, and git serving must not
/// compete with all of them (the follow loop itself is sequential).
const NUDGE_CONCURRENCY: usize = 2;
/// A nudged follow holds its slot at most this long (a stuck fetch must not
/// block the others; the follow loop's backstop still covers its repository).
const NUDGE_HOLD_MAX: Duration = Duration::from_secs(3600);

/// Run a `follow` op now for a repository **this host maintains** (D28/D30);
/// elsewhere a no-op, and the maintaining host's follow loop picks the change
/// up within `upstream.follow_interval`. A running `follow` task is joined (the
/// task lock), so a nudge never runs a second fetch against the scratch. At
/// most [`NUDGE_CONCURRENCY`] nudged follows run at once; the rest queue.
pub fn nudge(state: Arc<AppState>) -> floe_mirror::Nudge {
    let slots = Arc::new(Semaphore::new(NUDGE_CONCURRENCY));
    Box::new(move |id: &RepoId| {
        if !state.cfg.placement.maintains(id.owner(), id.name()) {
            return;
        }
        let st = state.clone();
        let id = id.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let Ok(_slot) = slots.acquire_owned().await else {
                return;
            };
            if floe_wal::tasks::draining() {
                return;
            }
            match crate::ops::start(st, id.clone(), "follow", HashMap::new()).await {
                Ok(task) => {
                    if !task.wait_done(NUDGE_HOLD_MAX).await {
                        tracing::warn!(repo = %id, "nudged follow still running; releasing its slot");
                    }
                }
                Err(crate::ops::StartError::AlreadyRunning(_)) => {}
                Err(crate::ops::StartError::UnknownOp) => {
                    tracing::debug!(repo = %id, "follow nudge: repository not openable here");
                }
            }
        });
    })
}

/// The mirror loop for a maintainer with `github_mirror.enabled`: its own loop,
/// never a unit of the priority loop (discovery must not wait behind a base
/// rebuild). Returns when draining.
pub async fn run_loop(state: Arc<AppState>) {
    let source = match floe_mirror::github::GithubSource::new(&state.cfg.github_mirror) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!(error = %e, "github mirror disabled: client setup failed");
            return;
        }
    };
    let target = floe_mirror::WalTarget::new(
        state.registry.clone(),
        state.store.clone(),
        nudge(state.clone()),
    );
    let mirror = Arc::new(floe_mirror::Mirror {
        cfg: state.cfg.clone(),
        source,
        target: Arc::new(target),
        store: state.store.clone(),
    });
    // Catalog telemetry (`sync_runs`/`repo_inventory`, §C.6): lossy, after the pass.
    let recorder = state.recorder.clone();
    floe_mirror::run_loop(
        mirror,
        Box::new(move |r: &floe_mirror::PassReport| record_pass(recorder.as_ref(), r)),
    )
    .await;
}

/// One `sync_runs` row (`kind = discovery`) per finished pass and one
/// `repo_inventory` change row per repository the pass created or whose
/// status changed. A failed pass is a metric and a log line only.
fn record_pass(recorder: &dyn floe_catalog::Recorder, r: &floe_mirror::PassReport) {
    let now = chrono::Utc::now();
    let finished = r.finished_at.unwrap_or(now);
    recorder.record_sync_run(floe_catalog::SyncRun {
        source: Some("github".into()),
        finished_at: finished,
        outcome: r.outcome.to_string(),
        detail: Some(r.summary()),
        api_requests: Some(r.api.requests),
        api_not_modified: Some(r.api.not_modified),
        rate_remaining: r.api.rate_remaining,
        ..floe_catalog::SyncRun::new("discovery", r.started_at.unwrap_or(now))
    });
    for (repo, change) in &r.changes {
        recorder.record_inventory(floe_catalog::InventoryRecord::new(
            finished, change, repo, "github",
        ));
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Captured {
        runs: Mutex<Vec<floe_catalog::SyncRun>>,
        inventory: Mutex<Vec<floe_catalog::InventoryRecord>>,
    }

    impl floe_catalog::Recorder for Captured {
        fn record_sync_run(&self, run: floe_catalog::SyncRun) {
            self.runs.lock().push(run);
        }
        fn record_inventory(&self, rec: floe_catalog::InventoryRecord) {
            self.inventory.lock().push(rec);
        }
    }

    /// A pass becomes one `discovery` run and one inventory row per change.
    #[test]
    fn pass_report_feeds_the_catalog() {
        let rec = Captured::default();
        let report = floe_mirror::PassReport {
            started_at: Some(chrono::Utc::now()),
            finished_at: Some(chrono::Utc::now()),
            outcome: "ok",
            complete: true,
            changes: vec![
                ("gh-acme/widgets".into(), "created".into()),
                ("gh-acme/gadgets".into(), "gone".into()),
            ],
            api: floe_mirror::ApiStats {
                requests: 4,
                not_modified: 3,
                rate_remaining: Some(4990),
                ..floe_mirror::ApiStats::default()
            },
            ..floe_mirror::PassReport::default()
        };
        super::record_pass(&rec, &report);
        let runs = rec.runs.lock();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!((run.kind.as_str(), run.outcome.as_str()), ("discovery", "ok"));
        assert_eq!(run.source.as_deref(), Some("github"));
        assert_eq!((run.api_requests, run.api_not_modified, run.rate_remaining), (Some(4), Some(3), Some(4990)));
        assert!(run.repo.is_none());
        let inv = rec.inventory.lock();
        let changes: Vec<_> = inv.iter().map(|r| (r.floe_repo.as_str(), r.change.as_str(), r.source.as_str())).collect();
        assert_eq!(changes, [("gh-acme/widgets", "created", "github"), ("gh-acme/gadgets", "gone", "github")]);
    }

    /// §B.7.2: the mirror's read-only policy parses and refuses every push.
    #[test]
    fn read_only_policy_refuses_pushes() {
        use floe_proto::v1::{RefTransaction, RefUpdate};
        let policy =
            crate::policy::parse_document(floe_mirror::target::READ_ONLY_POLICY.as_bytes()).unwrap();
        let oid = "1".repeat(40);
        let txn = RefTransaction {
            updates: vec![
                RefUpdate {
                    name: "refs/heads/main".into(),
                    old_oid: String::new(),
                    new_oid: oid.clone(),
                    ..RefUpdate::default()
                },
                RefUpdate {
                    name: "refs/tags/v1".into(),
                    old_oid: oid,
                    new_oid: String::new(),
                    ..RefUpdate::default()
                },
            ],
            push_options: Vec::new(),
            atomic: false,
        };
        let eval = crate::policy::evaluate(&policy, "alice", &txn, |_| false);
        assert!(eval.per_ref.iter().all(|(_, r)| r.as_ref().is_err_and(|e| e.contains("github-mirror-read-only"))), "{:?}", eval.per_ref);
        assert!(!eval.any_allowed());
    }
}
