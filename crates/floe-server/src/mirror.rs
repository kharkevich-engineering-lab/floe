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
const NUDGE_HOLD_MAX: Duration = Duration::from_hours(1);

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

/// Catalog telemetry for mirror passes run outside `floe serve` (`floe github
/// sync`): the writer the same `[catalog]` section starts in a server (or
/// [`floe_catalog::NoopRecorder`] when it is off), fed the same rows as
/// [`run_loop`]. Lossy like every `sync_runs`/`repo_inventory` row: call
/// [`PassTelemetry::flush`] before the process exits.
pub struct PassTelemetry {
    recorder: Arc<dyn floe_catalog::Recorder>,
    writer: Option<Arc<floe_catalog::CatalogWriter>>,
}

impl PassTelemetry {
    /// Start the writer when compiled in and `catalog.enabled`. Fails closed
    /// like `floe serve` when `catalog.enabled` but the binary lacks the feature.
    pub fn start(cfg: &floe_config::CatalogConfig) -> anyhow::Result<Self> {
        let writer = crate::catalog_writer(cfg)?;
        let recorder: Arc<dyn floe_catalog::Recorder> = match &writer {
            Some(w) => w.clone() as Arc<dyn floe_catalog::Recorder>,
            None => Arc::new(floe_catalog::NoopRecorder),
        };
        Ok(Self { recorder, writer })
    }

    /// The rows of one finished pass (see [`record_pass`]).
    pub fn record(&self, r: &floe_mirror::PassReport) {
        record_pass(self.recorder.as_ref(), r);
    }

    /// Final flush, bounded: losing it costs telemetry only.
    pub async fn flush(&self, bound: Duration) {
        if let Some(w) = &self.writer
            && tokio::time::timeout(bound, w.shutdown()).await.is_err()
        {
            tracing::warn!(?bound, "catalog: final flush did not finish; buffered telemetry dropped");
        }
    }
}

/// One `sync_runs` row (`kind = discovery`) per finished pass, a failed one
/// included (`outcome = failed`, the error as detail), and one
/// `repo_inventory` change row per repository the pass created or whose
/// status changed.
fn record_pass(recorder: &dyn floe_catalog::Recorder, r: &floe_mirror::PassReport) {
    let now = chrono::Utc::now();
    let finished = r.finished_at.unwrap_or(now);
    recorder.record_sync_run(floe_catalog::SyncRun {
        source: Some("github".into()),
        finished_at: finished,
        outcome: r.outcome.to_string(),
        detail: Some(r.error.clone().unwrap_or_else(|| r.summary())),
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

    /// A failed pass (GitHub down, a bad token) is a `failed` run carrying
    /// the error, with no inventory rows.
    #[test]
    fn failed_pass_is_a_failed_run() {
        let rec = Captured::default();
        let report = floe_mirror::PassReport {
            started_at: Some(chrono::Utc::now()),
            finished_at: Some(chrono::Utc::now()),
            outcome: "failed",
            error: Some("github: 401 Bad credentials".into()),
            ..floe_mirror::PassReport::default()
        };
        super::record_pass(&rec, &report);
        let runs = rec.runs.lock();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, "failed");
        assert_eq!(runs[0].detail.as_deref(), Some("github: 401 Bad credentials"));
        assert!(rec.inventory.lock().is_empty());
    }

    /// `floe github sync`'s telemetry: nothing to write (and nothing to
    /// flush) while `[catalog]` is off; `enabled` in a featureless binary fails
    /// closed exactly like `floe serve`.
    #[tokio::test]
    async fn cli_pass_telemetry_follows_the_catalog_section() {
        let off = super::PassTelemetry::start(&floe_config::CatalogConfig::default()).unwrap();
        assert!(off.writer.is_none());
        off.record(&floe_mirror::PassReport::default());
        off.flush(std::time::Duration::from_millis(10)).await;
        if cfg!(not(feature = "catalog")) {
            let on = floe_config::CatalogConfig {
                enabled: true,
                ..floe_config::CatalogConfig::default()
            };
            let err = super::PassTelemetry::start(&on).err().expect("featureless build must refuse");
            assert!(err.to_string().contains("without the catalog feature"), "{err:#}");
        }
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
