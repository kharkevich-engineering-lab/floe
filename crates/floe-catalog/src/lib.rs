//! floe-catalog: Iceberg audit tables for floe (`docs/design/github-mirror.md`
//! §C, D50) — `ref_events`, `force_push_log`, `sync_runs`, `repo_inventory`.
//!
//! The tables are **derived** copies, never a source of truth and never git
//! objects: `ref_events`/`force_push_log` are re-read from the WAL through a
//! durable cursor ([`cursor`]), `sync_runs`/`repo_inventory` changes are lossy
//! telemetry ([`Recorder`]) bounded by a durable daily inventory snapshot.
//! A catalog outage only adds catalog lag; git, sync, follow and the mirror
//! never await the catalog.
//!
//! The core (rows, [`Recorder`], the group-commit [`buffer`], the [`cursor`])
//! always compiles and pulls no arrow/parquet. The Iceberg REST writer is
//! behind the `iceberg` feature (`floe-server/catalog` ← `floe-cli/catalog`).

pub mod buffer;
pub mod cursor;
pub mod rows;

#[cfg(feature = "iceberg")]
pub mod iceberg;
#[cfg(feature = "iceberg")]
pub mod schema;
#[cfg(feature = "iceberg")]
pub mod sigv4;

pub use buffer::{CatalogError, CatalogWriter, CommitError, Committer, FlushPolicy};
pub use floe_config::{CatalogAuth, CatalogConfig};
pub use rows::{
    ArchivedLine, FOLLOW_ARCHIVED_META, ForcePushRow, InventoryRecord, RefEventRow, RefTransition,
    Row, SyncRun, Table, Timestamp, parse_follow_archived, rows_for_entry,
};

/// The telemetry seam (§C.6): follow and the mirror report through it after a
/// write finished. Both calls are non-blocking and lossy (dropped and counted
/// when the catalog is down or its buffer is full), like a metric.
pub trait Recorder: Send + Sync {
    fn record_sync_run(&self, run: SyncRun);
    fn record_inventory(&self, rec: InventoryRecord);
}

/// The recorder when the catalog is off or not compiled in.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRecorder;

impl Recorder for NoopRecorder {
    fn record_sync_run(&self, _run: SyncRun) {}
    fn record_inventory(&self, _rec: InventoryRecord) {}
}
