//! The server side of the GitHub mirror (D49, `docs/design/github-mirror.md`
//! §B.8/§D.2): the placement-gated follow nudge and the loop's spawn. The only
//! coupling between the server and `floe-mirror`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// `mirror/github/sync-request.json` (bucket root, `Overwrite`): the admin
/// GUI's "sync now" (D62). Every mirror supervisor polls its version.
pub const SYNC_REQUEST_KEY: &str = "mirror/github/sync-request.json";

/// Ask the fleet's mirror loop to run a pass now: it restarts at its next pass
/// boundary and runs one within its first tick.
pub async fn request_sync(store: &floe_store::DynStore, by: &str) -> anyhow::Result<()> {
    use floe_store::ObjectStoreExt;
    let body = serde_json::to_vec(&serde_json::json!({
        "requested_at": chrono::Utc::now().to_rfc3339(),
        "by": by,
    }))?;
    store
        .put_bytes(SYNC_REQUEST_KEY, body, floe_store::PutMode::Overwrite)
        .await?;
    Ok(())
}

/// The mirror supervisor on a `maintain` host (D49, D60): runs the mirror
/// loop while the live config enables it, and restarts it at a pass boundary
/// when the `github_mirror` section changes or a "sync now" arrives. Its own
/// loop, never a unit of the priority loop. Returns when draining.
pub async fn run_loop(state: Arc<AppState>) {
    let mut rx = state.config.subscribe();
    loop {
        if floe_wal::tasks::draining() {
            return;
        }
        let applied = rx.borrow_and_update().clone();
        let cfg = applied.cfg;
        if !cfg.github_mirror.enabled {
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return; },
                () = until_draining() => return,
            }
            continue;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = tokio::spawn(watch_for_restart(
            state.clone(),
            rx.clone(),
            section_of(&cfg),
            stop.clone(),
        ));
        run_mirror(&state, cfg, &stop).await;
        // The loop returns on its own only after its single pass when
        // `interval = 0` ("startup only"): stay idle until the config changes
        // or a "sync now" arrives, never restart it straight away.
        idle_until_restart(&stop).await;
        watcher.abort();
    }
}

/// Wait until `stop` is set (the watcher saw a config change or a sync
/// request) or draining began.
async fn idle_until_restart(stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) && !floe_wal::tasks::draining() {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The `github_mirror` section as JSON (what a change is compared on).
fn section_of(cfg: &floe_config::Config) -> serde_json::Value {
    serde_json::to_value(&cfg.github_mirror).unwrap_or_default()
}

async fn until_draining() {
    while !floe_wal::tasks::draining() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Set `stop` when the live `github_mirror` section differs from `section`, or
/// when `sync-request.json` changes (polled every `config_store.ttl`, ≥ 5 s).
async fn watch_for_restart(
    state: Arc<AppState>,
    mut rx: tokio::sync::watch::Receiver<crate::config_store::live::Applied>,
    section: serde_json::Value,
    stop: Arc<AtomicBool>,
) {
    use floe_store::ObjectStoreExt;
    let every = state.cfg.config_store.ttl.max(Duration::from_secs(5));
    let known = state
        .store
        .head(SYNC_REQUEST_KEY)
        .await
        .ok()
        .flatten()
        .map(|m| m.version);
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let now = section_of(&rx.borrow_and_update().cfg);
                if now != section {
                    tracing::info!("github mirror: config changed; restarting the loop at the next pass boundary");
                    stop.store(true, Ordering::SeqCst);
                    return;
                }
            }
            () = tokio::time::sleep(every) => {
                let got = match &known {
                    Some(v) => state.store.get_if_changed(SYNC_REQUEST_KEY, v).await.map(|g| g.map(|(m, _)| m.version)),
                    None => state.store.head(SYNC_REQUEST_KEY).await.map(|m| m.map(|m| m.version)),
                };
                if let Ok(Some(_)) = got {
                    tracing::info!("github mirror: sync requested; running a pass now");
                    stop.store(true, Ordering::SeqCst);
                    return;
                }
            }
        }
    }
}

/// One run of the mirror loop with `cfg`, until it stops (drain or `stop`).
async fn run_mirror(state: &Arc<AppState>, cfg: Arc<floe_config::Config>, stop: &AtomicBool) {
    let source = match floe_mirror::github::GithubSource::new(&cfg.github_mirror) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!(error = %e, "github mirror not started: client setup failed");
            // Wait for a config change (or drain) instead of spinning.
            while !stop.load(Ordering::SeqCst) && !floe_wal::tasks::draining() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            return;
        }
    };
    let target = floe_mirror::WalTarget::new(
        state.registry.clone(),
        state.store.clone(),
        nudge(state.clone()),
    );
    let mirror = Arc::new(floe_mirror::Mirror {
        cfg,
        source,
        target: Arc::new(target),
        store: state.store.clone(),
    });
    // Catalog telemetry (`sync_runs`/`repo_inventory`, §C.6): lossy, after the pass.
    let recorder = state.recorder.clone();
    floe_mirror::run_loop_until(
        mirror,
        Box::new(move |r: &floe_mirror::PassReport| record_pass(recorder.as_ref(), r)),
        stop,
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

    /// `interval = 0` = one pass, then idle: the supervisor must not restart
    /// the loop until the watcher sets `stop` (a config change, "sync now").
    #[tokio::test]
    async fn interval_zero_idles_until_a_restart_is_asked_for() {
        let stop = std::sync::Arc::new(super::AtomicBool::new(false));
        let idle = tokio::spawn({
            let stop = stop.clone();
            async move { super::idle_until_restart(&stop).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(!idle.is_finished(), "returned without a restart request");
        stop.store(true, super::Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(5), idle)
            .await
            .expect("returns once asked")
            .unwrap();
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
                ("acme/widgets".into(), "created".into()),
                ("acme/gadgets".into(), "gone".into()),
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
        assert_eq!(changes, [("acme/widgets", "created", "github"), ("acme/gadgets", "gone", "github")]);
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
