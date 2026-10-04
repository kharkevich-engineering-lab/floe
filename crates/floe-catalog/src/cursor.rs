//! The catalog's WAL cursor (`docs/design/github-mirror.md` §C.4, D32/D46/D47):
//! one repository's catch-up from `repos/<o>/<r>/catalog/cursor.json` to its
//! head, written so the server's catalog tail is a loop around [`catch_up`].
//!
//! Same shape as the events bridge: read the cursor (the last seq already
//! delivered), turn the entries `(cursor, head]` into rows, wait for the
//! catalog to commit them, CAS the cursor to head. A failure anywhere leaves
//! the cursor, so the next sweep retries the same range: at-least-once, never
//! a gap, consumers dedup on `(repo, seq, ref_name)`. The cursor is this
//! target's own (the D46 rule) and the bridge's `Cursor` JSON.
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
    async fn append_durable(&self, rows: Vec<Row>) -> Result<(), CatalogError>;
}

#[async_trait::async_trait]
impl DurableSink for CatalogWriter {
    fn is_up(&self) -> bool {
        CatalogWriter::is_up(self)
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

/// Deliver `(cursor, head]` to `sink`, then advance the cursor. `store` is
/// the repository's (prefixed) store.
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
    let (from, version) = loop {
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
    if head <= from {
        return Ok(report);
    }
    let rows = source.rows(from, head).await.map_err(source_err)?;
    report.rows = rows.len();
    sink.append_durable(rows).await?;
    match store
        .put_bytes(CURSOR_KEY, cursor_body(head)?, PutMode::Update(version))
        .await
    {
        Ok(_) => {}
        // Another tail advanced it: our rows were duplicates (dedup key).
        Err(StoreError::PreconditionFailed { .. }) => {
            tracing::warn!(from, head, "catalog tail: cursor CAS lost (two tails?)");
        }
        Err(e) => return Err(e.into()),
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
            Ok((from + 1..=to).map(row).collect())
        }
    }

    #[derive(Default)]
    struct Sink {
        down: AtomicBool,
        fail: AtomicBool,
        got: Mutex<Vec<Row>>,
    }

    #[async_trait::async_trait]
    impl DurableSink for Sink {
        fn is_up(&self) -> bool {
            !self.down.load(Ordering::SeqCst)
        }
        async fn append_durable(&self, rows: Vec<Row>) -> Result<(), CatalogError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(CatalogError::Unavailable);
            }
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
        let wal = Wal {
            head: 3,
            retained: 0,
            base: 1000,
        };
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
        let wal = Wal { head: 5, ..wal };
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
        let wal = Wal {
            head: 7,
            retained: 2,
            base: 0,
        };
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
        let wal = Wal {
            head: 4,
            retained: 0,
            base: 1000,
        };
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
        let wal = Wal {
            head: 4,
            retained: 0,
            base: 1000,
        };
        let err = catch_up(&store, &wal, &sink, &start(false, 100))
            .await
            .unwrap_err();
        assert!(matches!(err, TailError::Catalog(CatalogError::Unavailable)));
        assert_eq!(cursor(&store).await, None);
    }
}
