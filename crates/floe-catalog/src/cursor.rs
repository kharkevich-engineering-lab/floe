//! The catalog's WAL cursor (`docs/design/github-mirror.md` §C.4, D32/D46/D47):
//! one repository's catch-up from `repos/<o>/<r>/catalog/cursor.json` to its
//! head, written so the server's catalog tail is a loop around [`catch_up`].
//!
//! Same shape as the events bridge: read the cursor (the last seq already
//! delivered), turn the entries `(cursor, head]` into rows, wait for the
//! catalog to commit them, CAS the cursor forward. A failure anywhere leaves
//! the cursor at the last fully committed window, so the next sweep retries
//! from there: at-least-once, never a gap, consumers dedup on
//! `(repo, seq, ref_name)`. The cursor is this target's own (the D46 rule) and
//! the bridge's `Cursor` JSON.
//!
//! The range is delivered in windows of [`WINDOW_ENTRIES`] entries, each in
//! appends of at most [`DurableSink::max_append_rows`] rows, and the cursor is
//! advanced (CAS) after every window: a backlog larger than the writer's buffer
//! (a backfill, a mirror's initial import with many tags) still drains, and
//! only one window's rows are in memory at a time.
//!
//! What the catalog does not own stays behind [`TailSource`]: the WAL handle
//! and `events::refs_from_entries` live in floe-server, which implements it.
//! Callers serialize catch-ups per repository (as the bridge does).

use chrono::{DateTime, Utc};
use floe_store::{ObjectStore, ObjectStoreExt, PutMode, StoreError};
use serde::{Deserialize, Serialize};

use crate::buffer::{CatalogError, CatalogWriter};
use crate::rows::{Row, Timestamp};

/// Repo-relative key of the catalog's cursor. Unrelated to
/// `floe_proto::keys::CATALOG` (`meta/repos.pb`).
pub const CURSOR_KEY: &str = "catalog/cursor.json";
/// Bucket-root key recording when the catalog was first enabled.
pub const EPOCH_KEY: &str = "catalog/epoch.json";
/// Entries read (and rows held in memory) per cursor advance.
pub const WINDOW_ENTRIES: u64 = 256;

/// The bridge's cursor JSON: the last seq already delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub published_seq: u64,
    pub updated_at: String,
}

/// `catalog/epoch.json`: written once (CAS create); every host reads the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Epoch {
    pub enabled_at: DateTime<Utc>,
}

/// Why a catch-up did not advance the cursor.
#[derive(Debug, thiserror::Error)]
pub enum TailError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("WAL: {0}")]
    Source(String),
    #[error("{key}: {source}")]
    Json {
        key: &'static str,
        source: serde_json::Error,
    },
}

/// The error a [`TailSource`] reports; only its text travels further.
pub type SourceError = Box<dyn std::error::Error + Send + Sync>;

/// One repository's WAL as the tail needs it (floe-server: a `RepoHandle`).
#[async_trait::async_trait]
pub trait TailSource: Send + Sync {
    /// The head seq after a refs-level sync.
    async fn head_seq(&self) -> Result<u64, SourceError>;
    /// `RepoHandle::retained_log_start` (D47): the seq before the oldest
    /// retained entry.
    async fn retained_log_start(&self) -> Result<u64, SourceError>;
    /// `created_at` of entry `seq`; `None` when it is not retained.
    async fn committed_at(&self, seq: u64) -> Result<Option<Timestamp>, SourceError>;
    /// The rows of the entries `(from, to]` (see `rows::rows_for_entry`).
    async fn rows(&self, from: u64, to: u64) -> Result<Vec<Row>, SourceError>;
}

/// Where durable rows go: the [`CatalogWriter`], or a fake in tests.
#[async_trait::async_trait]
pub trait DurableSink: Send + Sync {
    fn is_up(&self) -> bool;
    /// The most rows one `append_durable` may carry: more is never accepted
    /// (`Backpressure` even on an empty buffer), so the tail splits.
    fn max_append_rows(&self) -> usize;
    async fn append_durable(&self, rows: Vec<Row>) -> Result<(), CatalogError>;
}

#[async_trait::async_trait]
impl DurableSink for CatalogWriter {
    fn is_up(&self) -> bool {
        CatalogWriter::is_up(self)
    }
    fn max_append_rows(&self) -> usize {
        CatalogWriter::max_append_rows(self)
    }
    async fn append_durable(&self, rows: Vec<Row>) -> Result<(), CatalogError> {
        CatalogWriter::append_durable(self, rows).await
    }
}

/// How a repository without a cursor starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColdStart {
    /// `catalog.backfill`.
    pub backfill: bool,
    pub epoch: Epoch,
}

/// What one [`catch_up`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchUp {
    /// Cursor before (the last seq already delivered).
    pub from_seq: u64,
    pub head_seq: u64,
    pub rows: usize,
}

/// Read `catalog/epoch.json`, creating it with `now` when absent. Whoever
/// wins the create, every host then reads the same value.
pub async fn load_epoch(root: &dyn ObjectStore, now: Timestamp) -> Result<Epoch, TailError> {
    loop {
        if let Some((_, bytes)) = root.get_bytes(EPOCH_KEY).await? {
            return serde_json::from_slice(&bytes).map_err(|source| TailError::Json {
                key: EPOCH_KEY,
                source,
            });
        }
        let epoch = Epoch { enabled_at: now };
        let body = serde_json::to_vec(&epoch).map_err(|source| TailError::Json {
            key: EPOCH_KEY,
            source,
        })?;
        match root.put_bytes(EPOCH_KEY, body, PutMode::Create).await {
            Ok(_) => return Ok(epoch),
            Err(StoreError::PreconditionFailed { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The first cursor of a repository (§C.4): its retained log start when
/// backfilling or when its oldest retained entry is no older than the epoch
/// (it was created after the catalog was enabled, so its initial import
/// belongs in `ref_events`); else its current head.
pub fn cold_start_seq(
    start: &ColdStart,
    retained_start: u64,
    oldest_committed_at: Option<Timestamp>,
    head_seq: u64,
) -> u64 {
    if start.backfill {
        return retained_start;
    }
    match oldest_committed_at {
        Some(at) if at < start.epoch.enabled_at => head_seq,
        // Newer than the epoch, or nothing retained to deliver.
        _ => retained_start,
    }
}

/// Deliver `(cursor, head]` to `sink`, advancing the cursor window by window.
/// `store` is the repository's (prefixed) store.
pub async fn catch_up(
    store: &dyn ObjectStore,
    source: &dyn TailSource,
    sink: &dyn DurableSink,
    start: &ColdStart,
) -> Result<CatchUp, TailError> {
    // While the catalog is down nothing here can succeed: cost nothing.
    if !sink.is_up() {
        return Err(CatalogError::Unavailable.into());
    }
    let head = source.head_seq().await.map_err(source_err)?;
    // Persist the starting boundary before any delivery, as the bridge does:
    // a retry after a checkpoint must not start at a newer retained start.
    let (from, mut version) = loop {
        if let Some((meta, bytes)) = store.get_bytes(CURSOR_KEY).await? {
            let c: Cursor = serde_json::from_slice(&bytes).map_err(|source| TailError::Json {
                key: CURSOR_KEY,
                source,
            })?;
            break (c.published_seq, meta.version);
        }
        let retained = source.retained_log_start().await.map_err(source_err)?;
        let oldest = if start.backfill || retained >= head {
            None
        } else {
            source
                .committed_at(retained + 1)
                .await
                .map_err(source_err)?
        };
        let first = cold_start_seq(start, retained, oldest, head);
        match store
            .put_bytes(CURSOR_KEY, cursor_body(first)?, PutMode::Create)
            .await
        {
            Ok(meta) => break (first, meta.version),
            Err(StoreError::PreconditionFailed { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    };
    let mut report = CatchUp {
        from_seq: from,
        head_seq: head,
        rows: 0,
    };
    let max_rows = sink.max_append_rows().max(1);
    let mut at = from;
    while at < head {
        let to = head.min(at.saturating_add(WINDOW_ENTRIES));
        let rows = source.rows(at, to).await.map_err(source_err)?;
        let n = rows.len();
        // One entry may yield more rows than one append may carry: split.
        let mut rows = rows.into_iter();
        loop {
            let chunk: Vec<Row> = rows.by_ref().take(max_rows).collect();
            if chunk.is_empty() {
                break;
            }
            sink.append_durable(chunk).await?;
        }
        report.rows += n;
        match store
            .put_bytes(CURSOR_KEY, cursor_body(to)?, PutMode::Update(version))
            .await
        {
            Ok(meta) => version = meta.version,
            // Another tail advanced it: our rows were duplicates (dedup key),
            // and the rest of the range is that tail's.
            Err(StoreError::PreconditionFailed { .. }) => {
                tracing::warn!(from = at, to, "catalog tail: cursor CAS lost (two tails?)");
                return Ok(report);
            }
            Err(e) => return Err(e.into()),
        }
        at = to;
    }
    Ok(report)
}

fn cursor_body(seq: u64) -> Result<Vec<u8>, TailError> {
    serde_json::to_vec(&Cursor {
        published_seq: seq,
        updated_at: Utc::now().to_rfc3339(),
    })
    .map_err(|source| TailError::Json {
        key: CURSOR_KEY,
        source,
    })
}

#[allow(clippy::needless_pass_by_value)] // used as `map_err(source_err)`
fn source_err(e: SourceError) -> TailError {
    TailError::Source(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{InventoryRecord, Row};
    use floe_store::memory::MemoryStore;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A WAL whose entries `retained+1..=head` were committed at `at(seq)`.
    struct Wal {
        head: u64,
        retained: u64,
        /// Seconds since the epoch of entry `seq` = base + seq.
        base: i64,
        /// Rows each entry yields.
        per_entry: usize,
        fail_rows: AtomicBool,
        /// The `(from, to]` ranges `rows` was asked for.
        reads: Mutex<Vec<(u64, u64)>>,
    }

    impl Wal {
        fn new(head: u64, retained: u64, base: i64) -> Wal {
            Wal {
                head,
                retained,
                base,
                per_entry: 1,
                fail_rows: AtomicBool::new(false),
                reads: Mutex::new(Vec::new()),
            }
        }
    }

    fn ts(secs: i64) -> Timestamp {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn row(seq: u64) -> Row {
        Row::Inventory(InventoryRecord::new(
            ts(0),
            "snapshot",
            &format!("o/r{seq}"),
            "own",
        ))
    }

    #[async_trait::async_trait]
    impl TailSource for Wal {
        async fn head_seq(&self) -> Result<u64, SourceError> {
            Ok(self.head)
        }
        async fn retained_log_start(&self) -> Result<u64, SourceError> {
            Ok(self.retained)
        }
        async fn committed_at(&self, seq: u64) -> Result<Option<Timestamp>, SourceError> {
            Ok((seq > self.retained && seq <= self.head)
                .then(|| ts(self.base + i64::try_from(seq).unwrap())))
        }
        async fn rows(&self, from: u64, to: u64) -> Result<Vec<Row>, SourceError> {
            if self.fail_rows.load(Ordering::SeqCst) {
                return Err("log GET failed".into());
            }
            self.reads.lock().push((from, to));
            let per = self.per_entry;
            Ok((from + 1..=to)
                .flat_map(|seq| std::iter::repeat_with(move || row(seq)).take(per))
                .collect())
        }
    }

    #[derive(Default)]
    struct Sink {
        down: AtomicBool,
        fail: AtomicBool,
        /// `max_append_rows` (0: unbounded); larger appends are refused, as
        /// the writer refuses them.
        max: usize,
        /// Appends that succeed before every later one fails (0: no limit).
        fail_after: usize,
        appends: Mutex<Vec<usize>>,
        got: Mutex<Vec<Row>>,
    }

    #[async_trait::async_trait]
    impl DurableSink for Sink {
        fn is_up(&self) -> bool {
            !self.down.load(Ordering::SeqCst)
        }
        fn max_append_rows(&self) -> usize {
            if self.max == 0 { usize::MAX } else { self.max }
        }
        async fn append_durable(&self, rows: Vec<Row>) -> Result<(), CatalogError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(CatalogError::Unavailable);
            }
            if rows.len() > self.max_append_rows() {
                return Err(CatalogError::Backpressure {
                    buffered: 0,
                    limit: self.max,
                });
            }
            let mut appends = self.appends.lock();
            if self.fail_after > 0 && appends.len() >= self.fail_after {
                return Err(CatalogError::Unavailable);
            }
            appends.push(rows.len());
            self.got.lock().extend(rows);
            Ok(())
        }
    }

    async fn cursor(store: &MemoryStore) -> Option<u64> {
        let (_, bytes) = store.get_bytes(CURSOR_KEY).await.unwrap()?;
        Some(
            serde_json::from_slice::<Cursor>(&bytes)
                .unwrap()
                .published_seq,
        )
    }

    fn start(backfill: bool, enabled_at: i64) -> ColdStart {
        ColdStart {
            backfill,
            epoch: Epoch {
                enabled_at: ts(enabled_at),
            },
        }
    }

    #[test]
    fn cold_start_rules() {
        let at = |s| Some(ts(s));
        // Backfill: the retained start, whatever the dates.
        assert_eq!(cold_start_seq(&start(true, 100), 3, at(50), 9), 3);
        // A repository older than the catalog: its head.
        assert_eq!(cold_start_seq(&start(false, 100), 3, at(50), 9), 9);
        // Created at or after the epoch: its whole retained history.
        assert_eq!(cold_start_seq(&start(false, 100), 0, at(100), 9), 0);
        assert_eq!(cold_start_seq(&start(false, 100), 0, at(150), 9), 0);
        // Nothing retained: nothing to deliver either way.
        assert_eq!(cold_start_seq(&start(false, 100), 4, None, 4), 4);
    }

    #[tokio::test]
    async fn epoch_is_created_once_and_then_read() {
        let root = MemoryStore::new();
        let first = load_epoch(&root, ts(100)).await.unwrap();
        let again = load_epoch(&root, ts(999)).await.unwrap();
        assert_eq!(first.enabled_at, ts(100));
        assert_eq!(again, first);
    }

    #[tokio::test]
    async fn catches_up_and_advances_the_cursor() {
        let store = MemoryStore::new();
        let sink = Sink::default();
        // Created after the epoch: the cold cursor is the retained start.
        let wal = Wal::new(3, 0, 1000);
        let c = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap();
        assert_eq!(
            c,
            CatchUp {
                from_seq: 0,
                head_seq: 3,
                rows: 3
            }
        );
        assert_eq!(cursor(&store).await, Some(3));
        // Up to date: nothing appended, cursor unchanged.
        let c = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap();
        assert_eq!(c.rows, 0);
        let wal = Wal::new(5, wal.retained, wal.base);
        let c = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap();
        assert_eq!((c.from_seq, c.rows), (3, 2));
        assert_eq!(sink.got.lock().len(), 5);
        assert_eq!(cursor(&store).await, Some(5));
    }

    #[tokio::test]
    async fn without_backfill_an_old_repository_starts_at_head() {
        let store = MemoryStore::new();
        let sink = Sink::default();
        let wal = Wal::new(7, 2, 0);
        let c = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap();
        assert_eq!(
            c,
            CatchUp {
                from_seq: 7,
                head_seq: 7,
                rows: 0
            }
        );
        assert_eq!(cursor(&store).await, Some(7));
        // With backfill, a fresh repository gets its retained history.
        let store = MemoryStore::new();
        let c = catch_up(&store, &wal, &sink, &start(true, 100))
            .await
            .unwrap();
        assert_eq!((c.from_seq, c.rows), (2, 5));
    }

    #[tokio::test]
    async fn a_failing_catalog_leaves_the_cursor() {
        let store = MemoryStore::new();
        let sink = Sink::default();
        let wal = Wal::new(4, 0, 1000);
        sink.fail.store(true, Ordering::SeqCst);
        let err = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap_err();
        assert!(
            matches!(err, TailError::Catalog(CatalogError::Unavailable)),
            "{err}"
        );
        // The boundary was persisted before delivery; the range is retried.
        assert_eq!(cursor(&store).await, Some(0));
        sink.fail.store(false, Ordering::SeqCst);
        let c = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap();
        assert_eq!((c.from_seq, c.rows), (0, 4));
        assert_eq!(cursor(&store).await, Some(4));
    }

    #[tokio::test]
    async fn a_down_catalog_costs_no_store_reads() {
        let store = MemoryStore::new();
        let sink = Sink::default();
        sink.down.store(true, Ordering::SeqCst);
        let wal = Wal::new(4, 0, 1000);
        let err = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap_err();
        assert!(matches!(err, TailError::Catalog(CatalogError::Unavailable)));
        assert_eq!(cursor(&store).await, None);
    }

    #[tokio::test]
    async fn a_backlog_larger_than_one_append_drains_in_windows() {
        let store = MemoryStore::new();
        let sink = Sink {
            max: 100,
            ..Sink::default()
        };
        // 1000 entries of 3 rows: 3000 rows, 30x what one append may carry.
        let mut wal = Wal::new(1000, 0, 1000);
        wal.per_entry = 3;
        let c = catch_up(&store, &wal, &sink, &start(true, 100))
            .await
            .unwrap();
        assert_eq!((c.from_seq, c.head_seq, c.rows), (0, 1000, 3000));
        assert_eq!(cursor(&store).await, Some(1000));
        assert_eq!(sink.got.lock().len(), 3000);
        assert!(sink.appends.lock().iter().all(|&n| n <= 100));
        let reads = wal.reads.lock().clone();
        assert!(
            reads.iter().all(|(f, t)| t - f <= WINDOW_ENTRIES),
            "{reads:?}"
        );
        assert_eq!(reads.first(), Some(&(0, WINDOW_ENTRIES)));
        assert_eq!(reads.last().map(|r| r.1), Some(1000));
    }

    #[tokio::test]
    async fn one_entry_with_more_rows_than_an_append_is_split() {
        let store = MemoryStore::new();
        let sink = Sink {
            max: 4,
            ..Sink::default()
        };
        // A mirror's initial import: one entry, many tags.
        let mut wal = Wal::new(1, 0, 1000);
        wal.per_entry = 10;
        let c = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap();
        assert_eq!(c.rows, 10);
        assert_eq!(*sink.appends.lock(), vec![4, 4, 2]);
        assert_eq!(cursor(&store).await, Some(1));
    }

    #[tokio::test]
    async fn a_failure_keeps_the_windows_already_committed() {
        let store = MemoryStore::new();
        // One append per window (256 rows); the third fails.
        let sink = Sink {
            fail_after: 2,
            ..Sink::default()
        };
        let wal = Wal::new(600, 0, 1000);
        let err = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap_err();
        assert!(matches!(err, TailError::Catalog(CatalogError::Unavailable)));
        assert_eq!(cursor(&store).await, Some(2 * WINDOW_ENTRIES));
    }

    #[tokio::test]
    async fn a_failing_source_leaves_the_cursor() {
        let store = MemoryStore::new();
        let sink = Sink::default();
        let wal = Wal::new(4, 0, 1000);
        wal.fail_rows.store(true, Ordering::SeqCst);
        let err = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap_err();
        assert!(
            matches!(err, TailError::Source(ref m) if m == "log GET failed"),
            "{err}"
        );
        assert_eq!(cursor(&store).await, Some(0));
        assert!(sink.got.lock().is_empty());
    }

    /// A WAL whose `rows` lets another tail advance the cursor first.
    struct Racing<'a> {
        wal: Wal,
        store: &'a MemoryStore,
        other: u64,
    }

    #[async_trait::async_trait]
    impl TailSource for Racing<'_> {
        async fn head_seq(&self) -> Result<u64, SourceError> {
            self.wal.head_seq().await
        }
        async fn retained_log_start(&self) -> Result<u64, SourceError> {
            self.wal.retained_log_start().await
        }
        async fn committed_at(&self, seq: u64) -> Result<Option<Timestamp>, SourceError> {
            self.wal.committed_at(seq).await
        }
        async fn rows(&self, from: u64, to: u64) -> Result<Vec<Row>, SourceError> {
            self.store
                .put_bytes(CURSOR_KEY, cursor_body(self.other)?, PutMode::Overwrite)
                .await?;
            self.wal.rows(from, to).await
        }
    }

    #[tokio::test]
    async fn a_lost_cursor_cas_keeps_the_other_tails_cursor() {
        let store = MemoryStore::new();
        let sink = Sink::default();
        let racing = Racing {
            wal: Wal::new(5, 0, 1000),
            store: &store,
            other: 4,
        };
        let c = catch_up(&store, &racing, &sink, &start(false, 100))
            .await
            .unwrap();
        // Our rows were delivered (duplicates, dedup key); the cursor is theirs.
        assert_eq!(c.rows, 5);
        assert_eq!(cursor(&store).await, Some(4));
    }
}
