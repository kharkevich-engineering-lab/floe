//! The catalog writer's group-commit buffer (`docs/design/github-mirror.md`
//! §C.5): one per process, one queue per table, one flusher task that owns
//! every conversation with the catalog.
//!
//! Two ways in, and neither can stall a caller for long:
//! * [`CatalogWriter::append_durable`] (the catalog tail and the inventory
//!   snapshot only): enqueue and wait for the commit that holds the rows. It
//!   fails at once with `Unavailable` while there is no live catalog, with
//!   `Backpressure` beyond `max_buffer_rows`, and with `Timeout` when the
//!   commit is not confirmed within `flush_interval + commit_timeout` (the age
//!   wait, then the commit). On any error the caller leaves its cursor where it
//!   was, so the backlog stays in the WAL and not in memory. One append carries
//!   at most [`CatalogWriter::max_append_rows`] rows; the tail splits.
//! * `record_*` ([`Recorder`], follow and the mirror): a non-blocking push
//!   that is dropped and counted when the catalog is down or the buffer full.
//!
//! The flusher commits a table once it holds `flush_rows` rows or its oldest
//! row is `flush_interval` old. A failed commit fails its waiters, drops its
//! rows (durable ones are re-read from a cursor) and takes the writer down
//! until a reconnect succeeds, so an outage costs lag, never memory. Startup
//! never waits for the catalog: connecting happens here, with backoff, and an
//! attempt that gets no answer within `commit_timeout` counts as a failure.
//! Shutdown interrupts a connect in progress.
//!
//! The [`Committer`] is the seam: the Iceberg implementation is the real one
//! (`iceberg.rs`, feature `iceberg`), tests use a fake.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

use crate::Recorder;
use crate::rows::{InventoryRecord, Row, SyncRun, Table, Timestamp};

/// Reconnect backoff bounds (§C.5: cap 5 min).
const CONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const CONNECT_BACKOFF_MAX: Duration = Duration::from_mins(5);

/// Why a durable append did not commit. The rows are not in the catalog (or
/// their fate is unknown, `Timeout`): the caller keeps its cursor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog unavailable")]
    Unavailable,
    #[error("catalog buffer full ({buffered} rows buffered, max_buffer_rows {limit})")]
    Backpressure { buffered: usize, limit: usize },
    #[error("catalog commit not confirmed within {0:?}")]
    Timeout(Duration),
    #[error("catalog commit to {table} failed: {message}")]
    Commit { table: Table, message: String },
    #[error("catalog writer closed")]
    Closed,
}

/// The error a [`Committer`] reports; only its text travels further.
pub type CommitError = Box<dyn std::error::Error + Send + Sync>;

/// Where rows go. Called only by the flusher, one call at a time.
#[async_trait::async_trait]
pub trait Committer: Send + Sync + 'static {
    /// Reach the catalog and make sure the namespace and tables exist (or
    /// fail if they do not and creating them is off). Retried with backoff.
    async fn connect(&self) -> Result<(), CommitError>;
    /// Append `rows` (all of `table`) in one commit, stamping `ingested_at`.
    async fn commit(
        &self,
        table: Table,
        rows: &[Row],
        ingested_at: Timestamp,
    ) -> Result<(), CommitError>;
}

/// The flush and bounding knobs (`[catalog]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushPolicy {
    pub flush_interval: Duration,
    pub flush_rows: usize,
    pub max_buffer_rows: usize,
    pub commit_timeout: Duration,
}

impl FlushPolicy {
    pub fn from_config(cfg: &floe_config::CatalogConfig) -> FlushPolicy {
        FlushPolicy {
            flush_interval: cfg.flush_interval,
            flush_rows: cfg.flush_rows.max(1),
            max_buffer_rows: cfg.max_buffer_rows.max(1),
            commit_timeout: cfg.commit_timeout,
        }
    }
}

type Waiter = oneshot::Sender<Result<(), CatalogError>>;

#[derive(Default)]
struct Pending {
    rows: Vec<Row>,
    /// When the oldest buffered row arrived (`None` while empty).
    since: Option<Instant>,
    waiters: Vec<Waiter>,
}

#[derive(Default)]
struct State {
    tables: [Pending; 4],
    /// Rows taken by a commit that has not finished: still in memory, still
    /// counted against `max_buffer_rows`.
    in_flight: usize,
    closed: bool,
}

impl State {
    fn buffered(&self) -> usize {
        self.tables.iter().map(|p| p.rows.len()).sum::<usize>() + self.in_flight
    }
}

struct Shared {
    committer: Arc<dyn Committer>,
    policy: FlushPolicy,
    state: Mutex<State>,
    up: AtomicBool,
    wake: Notify,
}

/// The process's catalog writer. Cheap to share (`Arc`); dropping the last
/// handle does not stop the flusher, [`CatalogWriter::shutdown`] does.
pub struct CatalogWriter {
    shared: Arc<Shared>,
    flusher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CatalogWriter {
    /// Spawn the flusher (it connects in the background) and return at once.
    pub fn start(committer: Arc<dyn Committer>, policy: FlushPolicy) -> Arc<CatalogWriter> {
        let shared = Arc::new(Shared {
            committer,
            policy,
            state: Mutex::new(State::default()),
            up: AtomicBool::new(false),
            wake: Notify::new(),
        });
        metrics::gauge!("floe_catalog_up").set(0.0);
        let flusher = tokio::spawn(shared.clone().run());
        Arc::new(CatalogWriter {
            shared,
            flusher: Mutex::new(Some(flusher)),
        })
    }

    /// A live catalog connection exists (`floe_catalog_up`).
    pub fn is_up(&self) -> bool {
        self.shared.up.load(Ordering::Acquire)
    }

    /// The most rows one [`CatalogWriter::append_durable`] should carry: one
    /// flush's worth, which also always fits an empty buffer.
    pub fn max_append_rows(&self) -> usize {
        let p = &self.shared.policy;
        p.flush_rows.min(p.max_buffer_rows)
    }

    /// Enqueue `rows` and wait for the commit(s) that hold them (one per table
    /// touched). See the module docs for the fail-fast cases.
    pub async fn append_durable(&self, rows: Vec<Row>) -> Result<(), CatalogError> {
        if rows.is_empty() {
            return Ok(());
        }
        let waiters = self.shared.enqueue(rows, true)?;
        // Rows under `flush_rows` wait up to `flush_interval` before their
        // commit starts; `commit_timeout` bounds the commit itself. Bounding
        // both with `commit_timeout` alone would time out every small append
        // whenever `flush_interval >= commit_timeout`, while its rows still
        // commit later: a duplicate on every retry and a cursor that never
        // moves.
        let p = self.shared.policy;
        let timeout = p.flush_interval.saturating_add(p.commit_timeout);
        let all = async {
            for rx in waiters {
                rx.await.map_err(|_| CatalogError::Closed)??;
            }
            Ok(())
        };
        tokio::time::timeout(timeout, all)
            .await
            .map_err(|_| CatalogError::Timeout(timeout))?
    }

    /// Final flush, then stop the flusher. Bound it with the drain timeout:
    /// losing it costs telemetry only (durable rows come back from a cursor).
    pub async fn shutdown(&self) {
        self.shared.state.lock().closed = true;
        self.shared.wake.notify_one();
        let handle = self.flusher.lock().take();
        if let Some(handle) = handle
            && let Err(e) = handle.await
        {
            tracing::warn!(error = %e, "catalog: flusher task failed");
        }
    }

    fn record(&self, row: Row) {
        // Lossy by contract: the error is the drop, already counted.
        let _ = self.shared.enqueue(vec![row], false);
    }
}

impl Recorder for CatalogWriter {
    fn record_sync_run(&self, run: SyncRun) {
        self.record(Row::SyncRun(run));
    }
    fn record_inventory(&self, rec: InventoryRecord) {
        self.record(Row::Inventory(rec));
    }
}

impl Shared {
    /// Buffer `rows`, all or nothing. Durable rows get a waiter per table.
    fn enqueue(
        &self,
        rows: Vec<Row>,
        durable: bool,
    ) -> Result<Vec<oneshot::Receiver<Result<(), CatalogError>>>, CatalogError> {
        let mut st = self.state.lock();
        let refused = if st.closed {
            Some(CatalogError::Closed)
        } else if !self.up.load(Ordering::Acquire) {
            Some(CatalogError::Unavailable)
        } else {
            let buffered = st.buffered();
            let limit = self.policy.max_buffer_rows;
            (buffered.saturating_add(rows.len()) > limit)
                .then_some(CatalogError::Backpressure { buffered, limit })
        };
        if let Some(e) = refused {
            drop(st);
            if !durable {
                count_dropped(&rows);
            }
            return Err(e);
        }
        let now = Instant::now();
        let mut wake = false;
        let mut touched = [false; 4];
        for row in rows {
            let table = row.table();
            metrics::counter!("floe_catalog_rows_total", "table" => table.name()).increment(1);
            let Some(p) = st.tables.get_mut(table.index()) else {
                continue;
            };
            // A table that was empty has no deadline yet: the flusher must
            // learn about it to arm the age flush.
            if p.since.is_none() {
                p.since = Some(now);
                wake = true;
            }
            p.rows.push(row);
            if let Some(t) = touched.get_mut(table.index()) {
                *t = true;
            }
        }
        let mut receivers = Vec::new();
        for table in Table::ALL {
            if !touched.get(table.index()).copied().unwrap_or(false) {
                continue;
            }
            let Some(p) = st.tables.get_mut(table.index()) else {
                continue;
            };
            if durable {
                let (tx, rx) = oneshot::channel();
                p.waiters.push(tx);
                receivers.push(rx);
            }
            wake |= p.rows.len() >= self.policy.flush_rows;
            gauge_buffer(table, p.rows.len());
        }
        drop(st);
        if wake {
            self.wake.notify_one();
        }
        Ok(receivers)
    }

    async fn run(self: Arc<Shared>) {
        let mut backoff = CONNECT_BACKOFF_MIN;
        loop {
            let closed = self.state.lock().closed;
            if !self.up.load(Ordering::Acquire) {
                if closed {
                    self.fail_all(&CatalogError::Closed);
                    return;
                }
                let Some(result) = self.connect_or_close().await else {
                    self.fail_all(&CatalogError::Closed);
                    return;
                };
                match result {
                    Ok(()) => {
                        tracing::info!("catalog: connected");
                        self.up.store(true, Ordering::Release);
                        metrics::gauge!("floe_catalog_up").set(1.0);
                        backoff = CONNECT_BACKOFF_MIN;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, retry_in = ?backoff, "catalog: connect failed");
                        tokio::select! {
                            () = tokio::time::sleep(backoff) => {}
                            () = self.wake.notified() => {}
                        }
                        backoff = (backoff * 2).min(CONNECT_BACKOFF_MAX);
                    }
                }
                continue;
            }
            self.flush_due(closed).await;
            if closed {
                // A commit that failed during the final flush took the writer
                // down; whatever it left behind is failed here.
                self.fail_all(&CatalogError::Closed);
                return;
            }
            if !self.up.load(Ordering::Acquire) {
                continue; // a commit failed: reconnect before waiting
            }
            let next = self.next_deadline();
            tokio::select! {
                () = self.wake.notified() => {}
                () = sleep_until(next) => {}
            }
        }
    }

    /// One connect attempt, bounded by `commit_timeout` (the REST client has
    /// no request timeout: a catalog that never answers would otherwise keep
    /// the writer down for good). `None` when shutdown came first.
    async fn connect_or_close(&self) -> Option<Result<(), String>> {
        let bound = self.policy.commit_timeout;
        let connect = tokio::time::timeout(bound, self.committer.connect());
        tokio::pin!(connect);
        loop {
            tokio::select! {
                r = &mut connect => {
                    return Some(match r {
                        Ok(r) => r.map_err(|e| e.to_string()),
                        Err(_) => Err(format!("no answer within {bound:?}")),
                    });
                }
                // Appends fail fast while down, so only shutdown wakes us.
                () = self.wake.notified() => {
                    if self.state.lock().closed {
                        return None;
                    }
                }
            }
        }
    }

    /// The oldest-row deadline over all non-empty tables.
    fn next_deadline(&self) -> Option<Instant> {
        let st = self.state.lock();
        st.tables
            .iter()
            .filter_map(|p| p.since)
            .min()
            .map(|since| since + self.policy.flush_interval)
    }

    /// Commit every table that is due (all non-empty ones when `force`).
    async fn flush_due(&self, force: bool) {
        for table in Table::ALL {
            if !self.up.load(Ordering::Acquire) {
                return;
            }
            let Some((rows, waiters)) = self.take_if_due(table, force) else {
                continue;
            };
            self.commit(table, rows, waiters).await;
        }
    }

    fn take_if_due(&self, table: Table, force: bool) -> Option<(Vec<Row>, Vec<Waiter>)> {
        let mut st = self.state.lock();
        let p = st.tables.get_mut(table.index())?;
        let since = p.since?;
        let due = force
            || p.rows.len() >= self.policy.flush_rows
            || since.elapsed() >= self.policy.flush_interval;
        if !due {
            return None;
        }
        p.since = None;
        let rows = std::mem::take(&mut p.rows);
        let waiters = std::mem::take(&mut p.waiters);
        st.in_flight += rows.len();
        gauge_buffer(table, 0);
        Some((rows, waiters))
    }

    async fn commit(&self, table: Table, rows: Vec<Row>, waiters: Vec<Waiter>) {
        let started = std::time::Instant::now();
        // The waiters give up after `flush_interval + commit_timeout`; this
        // bound (one `commit_timeout` later) only keeps a catalog that never
        // answers from wedging the flusher, and lets the waiters' `Timeout`
        // fire first.
        let p = self.policy;
        let bound = p
            .flush_interval
            .saturating_add(p.commit_timeout.saturating_mul(2));
        let result = match tokio::time::timeout(
            bound,
            self.committer.commit(table, &rows, chrono::Utc::now()),
        )
        .await
        {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err(format!("no answer within {bound:?}")),
        };
        metrics::histogram!("floe_catalog_commit_seconds", "table" => table.name())
            .record(started.elapsed().as_secs_f64());
        self.state.lock().in_flight -= rows.len();
        match result {
            Ok(()) => {
                metrics::counter!("floe_catalog_commits_total", "table" => table.name(), "outcome" => "ok")
                    .increment(1);
                tracing::debug!(%table, rows = rows.len(), "catalog: committed");
                for w in waiters {
                    let _ = w.send(Ok(()));
                }
            }
            Err(message) => {
                metrics::counter!("floe_catalog_commits_total", "table" => table.name(), "outcome" => "error")
                    .increment(1);
                tracing::warn!(%table, rows = rows.len(), error = %message,
                    "catalog: commit failed; reconnecting (durable rows stay behind their cursors)");
                count_dropped(&rows);
                let e = CatalogError::Commit { table, message };
                for w in waiters {
                    let _ = w.send(Err(e.clone()));
                }
                // Down until a reconnect: appends fail fast instead of piling up.
                self.up.store(false, Ordering::Release);
                metrics::gauge!("floe_catalog_up").set(0.0);
                self.fail_all(&CatalogError::Unavailable);
            }
        }
    }

    /// Drop everything buffered and fail its waiters with `e`.
    fn fail_all(&self, e: &CatalogError) {
        let mut st = self.state.lock();
        for table in Table::ALL {
            let Some(p) = st.tables.get_mut(table.index()) else {
                continue;
            };
            let rows = std::mem::take(&mut p.rows);
            let waiters = std::mem::take(&mut p.waiters);
            p.since = None;
            count_dropped(&rows);
            gauge_buffer(table, 0);
            for w in waiters {
                let _ = w.send(Err(e.clone()));
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn count_dropped(rows: &[Row]) {
    for row in rows {
        metrics::counter!("floe_catalog_dropped_total", "table" => row.table().name()).increment(1);
    }
}

#[allow(clippy::cast_precision_loss)] // a row count, far below 2^52
fn gauge_buffer(table: Table, rows: usize) {
    metrics::gauge!("floe_catalog_buffer_rows", "table" => table.name()).set(rows as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{InventoryRecord, SyncRun};
    use std::sync::atomic::AtomicUsize;

    /// Records commits; can refuse to connect, fail commits, or hang.
    #[derive(Default)]
    struct Fake {
        commits: Mutex<Vec<(Table, Vec<Row>)>>,
        connects: AtomicUsize,
        refuse_connect: AtomicBool,
        hang_connect: AtomicBool,
        fail_commits: AtomicBool,
        hang: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Committer for Fake {
        async fn connect(&self) -> Result<(), CommitError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            if self.hang_connect.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.refuse_connect.load(Ordering::SeqCst) {
                return Err("catalog down".into());
            }
            Ok(())
        }
        async fn commit(
            &self,
            table: Table,
            rows: &[Row],
            _: Timestamp,
        ) -> Result<(), CommitError> {
            if self.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.fail_commits.load(Ordering::SeqCst) {
                return Err("409 conflict, retries exhausted".into());
            }
            self.commits.lock().push((table, rows.to_vec()));
            Ok(())
        }
    }

    impl Fake {
        fn tables(&self) -> Vec<(Table, usize)> {
            self.commits
                .lock()
                .iter()
                .map(|(t, r)| (*t, r.len()))
                .collect()
        }
    }

    fn policy(flush_rows: usize, max: usize) -> FlushPolicy {
        FlushPolicy {
            flush_interval: Duration::from_secs(30),
            flush_rows,
            max_buffer_rows: max,
            commit_timeout: Duration::from_mins(1),
        }
    }

    fn inv(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| {
                Row::Inventory(InventoryRecord::new(
                    chrono::Utc::now(),
                    "snapshot",
                    &format!("o/r{i}"),
                    "own",
                ))
            })
            .collect()
    }

    fn run() -> SyncRun {
        SyncRun::new("follow", chrono::Utc::now())
    }

    async fn started(fake: &Arc<Fake>, p: FlushPolicy) -> Arc<CatalogWriter> {
        let w = CatalogWriter::start(fake.clone(), p);
        wait_for(|| w.is_up()).await;
        w
    }

    async fn wait_for(mut f: impl FnMut() -> bool) {
        for _ in 0..10_000 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("condition never held");
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_by_rows_and_completes_the_waiter() {
        let fake = Arc::new(Fake::default());
        let w = started(&fake, policy(3, 100)).await;
        w.append_durable(inv(3)).await.unwrap();
        assert_eq!(fake.tables(), vec![(Table::RepoInventory, 3)]);
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_by_age() {
        let fake = Arc::new(Fake::default());
        let w = started(&fake, policy(100, 1000)).await;
        w.record_sync_run(run());
        tokio::time::sleep(Duration::from_secs(29)).await;
        assert!(fake.tables().is_empty(), "flushed before flush_interval");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(fake.tables(), vec![(Table::SyncRuns, 1)]);
        // A durable append under the row threshold also commits by age.
        let t0 = Instant::now();
        w.append_durable(inv(1)).await.unwrap();
        assert!(t0.elapsed() >= Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn a_durable_append_waits_for_every_table_it_touched() {
        let fake = Arc::new(Fake::default());
        let w = started(&fake, policy(2, 100)).await;
        let mut rows = inv(2);
        rows.push(Row::SyncRun(run()));
        w.append_durable(rows).await.unwrap();
        let mut tables = fake.tables();
        tables.sort_by_key(|(t, _)| t.index());
        assert_eq!(
            tables,
            vec![(Table::SyncRuns, 1), (Table::RepoInventory, 2)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backpressure_fails_durable_appends_and_drops_telemetry() {
        let fake = Arc::new(Fake::default());
        let w = started(&fake, policy(10, 4)).await;
        let w2 = w.clone();
        let first = tokio::spawn(async move { w2.append_durable(inv(3)).await });
        wait_for(|| w.shared.state.lock().buffered() == 3).await;
        let err = w.append_durable(inv(2)).await.unwrap_err();
        assert_eq!(
            err,
            CatalogError::Backpressure {
                buffered: 3,
                limit: 4
            }
        );
        w.record_sync_run(run()); // fits: 4
        w.record_sync_run(run()); // dropped
        assert_eq!(w.shared.state.lock().buffered(), 4);
        first.await.unwrap().unwrap(); // by age
        wait_for(|| fake.tables().iter().map(|(_, n)| n).sum::<usize>() == 4).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_commit_that_never_answers_times_out() {
        let fake = Arc::new(Fake::default());
        let mut p = policy(1, 100);
        p.commit_timeout = Duration::from_secs(5);
        let w = started(&fake, p).await;
        fake.hang.store(true, Ordering::SeqCst);
        let err = w.append_durable(inv(1)).await.unwrap_err();
        // flush_interval (30 s) + commit_timeout.
        assert_eq!(err, CatalogError::Timeout(Duration::from_secs(35)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_small_append_commits_when_flush_interval_exceeds_commit_timeout() {
        let fake = Arc::new(Fake::default());
        let mut p = policy(100, 1000);
        p.flush_interval = Duration::from_mins(2);
        let w = started(&fake, p).await;
        // Under flush_rows: it waits the full 2 min age, then commits.
        let t0 = Instant::now();
        w.append_durable(inv(1)).await.unwrap();
        assert!(t0.elapsed() >= Duration::from_mins(2));
        assert_eq!(fake.tables(), vec![(Table::RepoInventory, 1)]);
    }

    #[tokio::test(start_paused = true)]
    async fn max_append_rows_fits_an_empty_buffer() {
        let fake = Arc::new(Fake::default());
        assert_eq!(
            CatalogWriter::start(fake.clone(), policy(10, 100)).max_append_rows(),
            10
        );
        assert_eq!(
            CatalogWriter::start(fake, policy(10, 4)).max_append_rows(),
            4
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_connect_that_never_answers_is_retried() {
        let fake = Arc::new(Fake::default());
        fake.hang_connect.store(true, Ordering::SeqCst);
        let w = CatalogWriter::start(fake.clone(), policy(1, 100));
        // commit_timeout (60 s), then 1 s of backoff: a second attempt.
        tokio::time::sleep(Duration::from_secs(62)).await;
        assert!(fake.connects.load(Ordering::SeqCst) >= 2);
        assert!(!w.is_up());
        fake.hang_connect.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(70)).await;
        assert!(w.is_up());
        w.append_durable(inv(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_a_hanging_connect() {
        let fake = Arc::new(Fake::default());
        fake.hang_connect.store(true, Ordering::SeqCst);
        let w = CatalogWriter::start(fake.clone(), policy(1, 100));
        wait_for(|| fake.connects.load(Ordering::SeqCst) == 1).await;
        let t0 = Instant::now();
        w.shutdown().await;
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_commit_fails_waiters_keeps_rows_out_and_reconnects() {
        let fake = Arc::new(Fake::default());
        let w = started(&fake, policy(1, 100)).await;
        fake.fail_commits.store(true, Ordering::SeqCst);
        fake.refuse_connect.store(true, Ordering::SeqCst);
        let err = w.append_durable(inv(1)).await.unwrap_err();
        assert!(
            matches!(
                err,
                CatalogError::Commit {
                    table: Table::RepoInventory,
                    ..
                }
            ),
            "{err}"
        );
        // Down: fail fast, nothing buffered, telemetry dropped.
        assert!(!w.is_up());
        assert_eq!(
            w.append_durable(inv(1)).await.unwrap_err(),
            CatalogError::Unavailable
        );
        w.record_inventory(InventoryRecord::new(
            chrono::Utc::now(),
            "created",
            "o/x",
            "github",
        ));
        assert_eq!(w.shared.state.lock().buffered(), 0);
        // The catalog comes back: the failed rows are not replayed.
        fake.fail_commits.store(false, Ordering::SeqCst);
        fake.refuse_connect.store(false, Ordering::SeqCst);
        wait_for(|| w.is_up()).await;
        let mut rows = inv(1);
        if let Some(Row::Inventory(r)) = rows.first_mut() {
            r.floe_repo = "o/after".into();
        }
        w.append_durable(rows).await.unwrap();
        let commits = fake.commits.lock().clone();
        assert_eq!(commits.len(), 1);
        assert!(matches!(&commits[0].1[0], Row::Inventory(r) if r.floe_repo == "o/after"));
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_until_connected_and_connect_backs_off() {
        let fake = Arc::new(Fake::default());
        fake.refuse_connect.store(true, Ordering::SeqCst);
        let w = CatalogWriter::start(fake.clone(), policy(1, 100));
        assert_eq!(
            w.append_durable(inv(1)).await.unwrap_err(),
            CatalogError::Unavailable
        );
        w.record_sync_run(run());
        assert_eq!(w.shared.state.lock().buffered(), 0);
        // 1 + 2 + 4 + 8 s of backoff: about 4 attempts in 10 s, not hundreds.
        tokio::time::sleep(Duration::from_secs(10)).await;
        let attempts = fake.connects.load(Ordering::SeqCst);
        assert!((3..=5).contains(&attempts), "{attempts} connect attempts");
        fake.refuse_connect.store(false, Ordering::SeqCst);
        wait_for(|| w.is_up()).await;
        w.append_durable(inv(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_flushes_what_is_buffered() {
        let fake = Arc::new(Fake::default());
        let w = started(&fake, policy(100, 1000)).await;
        w.record_sync_run(run());
        w.record_inventory(InventoryRecord::new(
            chrono::Utc::now(),
            "created",
            "o/r",
            "github",
        ));
        w.shutdown().await;
        let mut tables = fake.tables();
        tables.sort_by_key(|(t, _)| t.index());
        assert_eq!(
            tables,
            vec![(Table::SyncRuns, 1), (Table::RepoInventory, 1)]
        );
        assert_eq!(
            w.append_durable(inv(1)).await.unwrap_err(),
            CatalogError::Closed
        );
    }
}
