//! The catalog's WAL tail (`docs/design/github-mirror.md` §C.4/§C.6, D50): the
//! `ref_events`/`force_push_log` rows, read from the WAL through the catalog's
//! own cursor (`repos/<o>/<r>/catalog/cursor.json`), and the daily
//! `repo_inventory` snapshot. Runs on the events host when `catalog.enabled`.
//!
//! Its own loop, never a target inside `Bridge::catch_up`: a bucket
//! notification only `try_send`s the repository here ([`CatalogTail::wake`],
//! dropped when the queue is full; the sweep is the backstop), so the webhook's
//! notify answer, its catch-up and its sweep never wait for an Iceberg commit,
//! and a catalog outage is catalog lag, not a 503 storm. While the writer is
//! down a catch-up returns before any bucket request.
//!
//! The rows come from the same `events::refs_from_entries` the webhook uses
//! (one implementation of the zero-OID, HEAD-skip and classify rules), plus
//! the entry's provenance (`floe_catalog::rows_for_entry`). At-least-once:
//! consumers dedup on `(repo, seq, ref_name)`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use floe_catalog::cursor::{self, CatchUp, ColdStart, Epoch, SourceError, TailSource};
use floe_catalog::{CatalogWriter, InventoryRecord, RefTransition, Row, Timestamp};
use floe_git::RepoId;
use floe_store::coord::{self, LeaseGuard};
use floe_wal::{Registry, RepoHandle};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, mpsc};

use crate::events;

/// Catch-ups at once, from the sweep and from wake-ups (each one may wait up
/// to `flush_interval` for its group commit while holding a slot). A slot is
/// taken only under the repository's serial lock, so catch-ups queued behind
/// one busy repository never hold slots other repositories need.
const CONCURRENCY: usize = 8;
/// Pending wake-ups (one per repository: wakes of a repository already
/// pending are merged); beyond it a wake is dropped (the sweep covers it).
const WAKE_QUEUE: usize = 1024;
/// Bucket-root lease of the daily inventory snapshot (one host fleet-wide).
const INVENTORY_LEASE: &str = "leases/catalog-inventory.pb";
/// Bucket-root schedule of the snapshot: restarts and lease handovers neither
/// skip nor repeat a day.
pub const INVENTORY_KEY: &str = "catalog/inventory.json";
const INVENTORY_EVERY: Duration = Duration::from_hours(24);
/// How often a host checks whether the snapshot is due.
const INVENTORY_CHECK: Duration = Duration::from_hours(1);
/// First check after startup (lets the writer connect first).
const INVENTORY_FIRST_CHECK: Duration = Duration::from_mins(2);
const INVENTORY_LEASE_TTL: Duration = Duration::from_mins(5);

/// `catalog/inventory.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventorySchedule {
    pub last_snapshot: Option<DateTime<Utc>>,
    /// The completed snapshot's id; rows of an interrupted attempt carry another.
    pub snapshot_id: Option<String>,
}

pub struct CatalogTail {
    registry: Arc<Registry>,
    writer: Arc<CatalogWriter>,
    /// `catalog.backfill`.
    backfill: bool,
    /// Catch-ups serialize per repository (as the bridge's do).
    serial: dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Repositories woken whose catch-up has not yet taken its serial lock.
    pending: dashmap::DashSet<String>,
    slots: Arc<Semaphore>,
    wake_tx: mpsc::Sender<RepoId>,
    wake_rx: parking_lot::Mutex<Option<mpsc::Receiver<RepoId>>>,
    epoch: tokio::sync::OnceCell<Epoch>,
}

impl CatalogTail {
    pub fn new(registry: Arc<Registry>, writer: Arc<CatalogWriter>, backfill: bool) -> Arc<Self> {
        let (wake_tx, wake_rx) = mpsc::channel(WAKE_QUEUE);
        Arc::new(CatalogTail {
            registry,
            writer,
            backfill,
            serial: dashmap::DashMap::new(),
            pending: dashmap::DashSet::new(),
            slots: Arc::new(Semaphore::new(CONCURRENCY)),
            wake_tx,
            wake_rx: parking_lot::Mutex::new(Some(wake_rx)),
            epoch: tokio::sync::OnceCell::new(),
        })
    }

    /// `id` committed something (a bucket notification): catch it up soon.
    /// Never awaits; merged with a wake of `id` that has not started yet (that
    /// catch-up reads the head when it starts); dropped and counted when the
    /// queue is full.
    pub fn wake(&self, id: &RepoId) {
        let key = id.to_string();
        if !self.pending.insert(key.clone()) {
            return;
        }
        if self.wake_tx.try_send(id.clone()).is_err() {
            self.pending.remove(&key);
            metrics::counter!("floe_catalog_wake_dropped_total").increment(1);
        }
    }

    /// Start the wake consumer, the sweep (`events.sweep_interval`, 0 = off)
    /// and the inventory snapshot check. Call once; a second call starts only
    /// another sweep.
    pub fn spawn(self: &Arc<Self>, sweep_every: Duration) {
        let rx = self.wake_rx.lock().take();
        if let Some(mut rx) = rx {
            let tail = self.clone();
            tokio::spawn(async move {
                // At most two tasks per repository (one running, one pending):
                // the slot is taken inside `catch_up`, under the serial lock.
                while let Some(id) = rx.recv().await {
                    let t = tail.clone();
                    tokio::spawn(async move {
                        t.catch_up_logged(&id, "wake").await;
                    });
                }
            });
        }
        if !sweep_every.is_zero() {
            let tail = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(sweep_every).await;
                    if floe_wal::tasks::draining() {
                        return;
                    }
                    tail.sweep().await;
                }
            });
        }
        let tail = self.clone();
        tokio::spawn(async move {
            let mut wait = INVENTORY_FIRST_CHECK;
            loop {
                tokio::time::sleep(wait).await;
                wait = INVENTORY_CHECK;
                if floe_wal::tasks::draining() {
                    return;
                }
                match tail.snapshot_if_due().await {
                    Ok(Some(n)) => {
                        tracing::info!(rows = n, "catalog: inventory snapshot committed");
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        error = format!("{e:#}"),
                        "catalog: inventory snapshot failed; retried within the hour"
                    ),
                }
            }
        });
    }

    /// Every repository, [`CONCURRENCY`] at a time. Nothing at all while the
    /// writer is down (not even the listing).
    pub async fn sweep(&self) {
        if !self.writer.is_up() {
            return;
        }
        let repos = match self.registry.list().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "catalog tail: sweep list failed");
                return;
            }
        };
        let mut passes = futures::stream::iter(repos.into_iter().map(|id| async move {
            self.catch_up_logged(&id, "sweep").await;
        }))
        .buffer_unordered(CONCURRENCY);
        while passes.next().await.is_some() {}
        self.serial.retain(|_, lock| Arc::strong_count(lock) > 1);
    }

    async fn catch_up_logged(&self, id: &RepoId, why: &'static str) {
        match self.catch_up(id).await {
            Ok(c) if c.rows > 0 => {
                tracing::debug!(repo = %id, from = c.from_seq, head = c.head_seq, rows = c.rows, why, "catalog tail: delivered");
            }
            Ok(_) => {}
            Err(e) => {
                // A repository deleted since the notification: nothing to do.
                if matches!(
                    e.downcast_ref::<floe_wal::WalError>(),
                    Some(floe_wal::WalError::NotFound)
                ) {
                    return;
                }
                tracing::debug!(repo = %id, error = format!("{e:#}"), why, "catalog tail: catch-up did not advance");
            }
        }
    }

    /// Deliver `(cursor, head]` of `id` to the writer (`floe_catalog::cursor`):
    /// the repository's serial lock, then a [`CONCURRENCY`] slot.
    pub async fn catch_up(&self, id: &RepoId) -> anyhow::Result<CatchUp> {
        let key = id.to_string();
        let serial = self
            .serial
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _serial = serial.lock().await;
        // From here on a new wake queues another catch-up (this one may have
        // read the head before that commit).
        self.pending.remove(&key);
        // Cost nothing on the bucket while the catalog is down.
        if !self.writer.is_up() {
            return Err(floe_catalog::CatalogError::Unavailable.into());
        }
        let _slot = self.slots.acquire().await?;
        let epoch = self.epoch().await?;
        let handle = self.registry.open(id).await?;
        let source = HandleSource {
            id,
            handle: &handle,
        };
        let start = ColdStart {
            backfill: self.backfill,
            epoch,
        };
        Ok(cursor::catch_up(handle.store(), &source, self.writer.as_ref(), &start).await?)
    }

    /// `catalog/epoch.json`, read (or created) once per process.
    async fn epoch(&self) -> anyhow::Result<Epoch> {
        let root = self.registry.store().clone();
        let epoch = self
            .epoch
            .get_or_try_init(|| async move { cursor::load_epoch(root.as_ref(), Utc::now()).await })
            .await?;
        Ok(*epoch)
    }

    /// The daily snapshot (§C.6), when due and when this host gets
    /// [`INVENTORY_LEASE`]. `Some(rows)` when one completed.
    pub async fn snapshot_if_due(&self) -> anyhow::Result<Option<usize>> {
        if !self.writer.is_up() {
            return Ok(None);
        }
        let root = self.registry.store().clone();
        let now = Utc::now();
        if !snapshot_due(&schedule(&root).await?, now) {
            return Ok(None);
        }
        let Some(guard) = coord::try_acquire(
            root.clone(),
            INVENTORY_LEASE,
            coord::instance_id(),
            "catalog-inventory",
            INVENTORY_LEASE_TTL,
        )
        .await?
        else {
            return Ok(None);
        };
        let lost = guard.released_flag();
        let guard = Arc::new(tokio::sync::Mutex::new(guard));
        let hb = LeaseGuard::spawn_heartbeat(
            guard.clone(),
            INVENTORY_LEASE_TTL / 3,
            INVENTORY_LEASE_TTL,
        );
        // Re-read under the lease: another host may have just finished it.
        let result = match schedule(&root).await {
            Ok(s) if snapshot_due(&s, now) => self.snapshot(now, &lost).await.map(Some),
            Ok(_) => Ok(None),
            Err(e) => Err(e),
        };
        hb.abort();
        let _ = hb.await;
        if let Ok(g) = Arc::try_unwrap(guard)
            && let Err(e) = g.into_inner().release().await
        {
            tracing::debug!(error = %e, "catalog inventory lease release failed; it expires");
        }
        result
    }

    /// One snapshot: a row per repository (`source = 'own'` without a mirror
    /// entry), appended durably in chunks; the schedule advances only after
    /// the last chunk committed, so an interrupted snapshot is retried whole.
    async fn snapshot(&self, now: Timestamp, lost: &AtomicBool) -> anyhow::Result<usize> {
        let root = self.registry.store().clone();
        let snapshot_id = uuid::Uuid::new_v4().to_string();
        // A missing state is already the default; an error is a real failure,
        // and a snapshot without it would label every mirror `own` for a day.
        let mirror = floe_mirror::state::load(root.as_ref(), "github").await?;
        let by_floe: HashMap<&str, (&String, &floe_mirror::RepoEntry)> = mirror
            .repos
            .iter()
            .filter_map(|(sid, e)| e.floe.as_deref().map(|f| (f, (sid, e))))
            .collect();
        let mut rows = Vec::new();
        for id in self.registry.list().await? {
            let name = id.to_string();
            let head_seq = match self.registry.open(&id).await {
                Ok(h) => head_seq(&h).await.ok(),
                Err(e) => {
                    tracing::debug!(repo = %id, error = %e, "catalog inventory: repository not openable");
                    None
                }
            };
            let rec = inventory_row(
                now,
                &name,
                by_floe.get(name.as_str()).copied(),
                head_seq,
                &snapshot_id,
            );
            rows.push(Row::Inventory(rec));
        }
        let n = rows.len();
        let max = self.writer.max_append_rows().max(1);
        let mut rows = rows.into_iter();
        loop {
            let chunk: Vec<Row> = rows.by_ref().take(max).collect();
            if chunk.is_empty() {
                break;
            }
            if lost.load(Ordering::SeqCst) {
                anyhow::bail!("inventory lease lost mid-snapshot");
            }
            self.writer.append_durable(chunk).await?;
        }
        // The last append may have outlived the lease: the next holder's
        // schedule wins.
        if lost.load(Ordering::SeqCst) {
            anyhow::bail!("inventory lease lost before the schedule was written");
        }
        let done = InventorySchedule {
            last_snapshot: Some(now),
            snapshot_id: Some(snapshot_id),
        };
        coord::cas_update_json::<InventorySchedule, _>(root.as_ref(), INVENTORY_KEY, 5, |cur| {
            Ok(advance_schedule(cur, &done))
        })
        .await?;
        Ok(n)
    }
}

async fn schedule(root: &floe_store::DynStore) -> anyhow::Result<InventorySchedule> {
    Ok(
        coord::get_json::<InventorySchedule>(root.as_ref(), INVENTORY_KEY)
            .await?
            .map(|(_, s)| s)
            .unwrap_or_default(),
    )
}

/// The schedule CAS: `done`, unless the stored snapshot is newer (never move
/// the schedule backwards).
fn advance_schedule(
    cur: Option<&InventorySchedule>,
    done: &InventorySchedule,
) -> Option<InventorySchedule> {
    match cur {
        Some(c) if c.last_snapshot > done.last_snapshot => None,
        _ => Some(done.clone()),
    }
}

fn snapshot_due(s: &InventorySchedule, now: Timestamp) -> bool {
    s.last_snapshot.is_none_or(|t| {
        now.signed_duration_since(t)
            .to_std()
            .is_ok_and(|d| d >= INVENTORY_EVERY)
    })
}

/// One snapshot row: the mirror's facts when `mirror` has an entry for the
/// repository, else an own repository.
fn inventory_row(
    now: Timestamp,
    name: &str,
    mirror: Option<(&String, &floe_mirror::RepoEntry)>,
    head_seq: Option<u64>,
    snapshot_id: &str,
) -> InventoryRecord {
    let base = |source: &str| InventoryRecord {
        head_seq,
        snapshot_id: Some(snapshot_id.to_string()),
        ..InventoryRecord::new(now, "snapshot", name, source)
    };
    match mirror {
        None => InventoryRecord {
            status: Some("own".into()),
            ..base("own")
        },
        Some((sid, e)) => InventoryRecord {
            source_id: Some(sid.clone()),
            full_name: Some(e.full_name.clone()),
            status: Some(e.status.as_str().to_string()),
            private: Some(e.private),
            archived: Some(e.archived),
            fork: Some(e.fork),
            default_branch: e.default_branch.clone(),
            pushed_at: e
                .pushed_at
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc)),
            size_kb: Some(e.size_kb),
            ..base("github")
        },
    }
}

async fn head_seq(handle: &RepoHandle) -> Result<u64, floe_wal::WalError> {
    let guard = handle.sync_refs_only().await?;
    let head = handle.manifest().head_seq;
    drop(guard);
    Ok(head)
}

/// One repository's WAL for `floe_catalog::cursor::catch_up`.
struct HandleSource<'a> {
    id: &'a RepoId,
    handle: &'a RepoHandle,
}

#[async_trait::async_trait]
impl TailSource for HandleSource<'_> {
    async fn head_seq(&self) -> Result<u64, SourceError> {
        Ok(head_seq(self.handle).await?)
    }

    async fn retained_log_start(&self) -> Result<u64, SourceError> {
        Ok(self.handle.retained_log_start().await?)
    }

    async fn committed_at(&self, seq: u64) -> Result<Option<Timestamp>, SourceError> {
        let entries = self.handle.read_log_retained(seq, Some(seq)).await?;
        Ok(entries
            .iter()
            .find(|e| e.seq == seq)
            .map(floe_catalog::rows::committed_at))
    }

    async fn rows(&self, from: u64, to: u64) -> Result<Vec<Row>, SourceError> {
        let entries = self.handle.read_log_retained(from + 1, Some(to)).await?;
        Ok(rows_from_entries(self.id, &entries))
    }
}

/// The durable rows of `entries`: the webhook's ref events of each entry,
/// with that entry's provenance, plus its archive lines.
pub(crate) fn rows_from_entries(id: &RepoId, entries: &[floe_proto::v1::LogEntry]) -> Vec<Row> {
    let repo = id.to_string();
    let mut out = Vec::new();
    let mut events = Vec::new();
    for entry in entries {
        events.clear();
        events::refs_from_entries(id, std::slice::from_ref(entry), &mut events);
        let transitions = events.drain(..).map(|e| RefTransition {
            action: e.action.as_str().to_string(),
            ref_type: e.ref_type.to_string(),
            ref_name: e.ref_name,
            old_oid: e.old,
            new_oid: e.new,
        });
        out.extend(floe_catalog::rows_for_entry(&repo, entry, transitions));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use floe_proto::v1::{EntryKind, LogEntry, RefTransaction, RefUpdate};

    fn update(name: &str, old: &str, new: &str) -> RefUpdate {
        RefUpdate {
            name: name.into(),
            old_oid: old.into(),
            new_oid: new.into(),
            ..RefUpdate::default()
        }
    }

    /// The rows equal the webhook's events, one per transition, with the
    /// entry's provenance; an archive line becomes a `force_push_log` row.
    #[test]
    fn rows_follow_the_webhook_events_and_archive_meta() {
        let id = RepoId::new("gh-acme", "widgets").unwrap();
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let archive = "refs/archive/1700000000/refs/heads/main".to_string();
        let mut entry = LogEntry {
            seq: 7,
            kind: EntryKind::Push as i32,
            txn: Some(RefTransaction {
                updates: vec![
                    update("refs/heads/main", &a, &b),
                    update(&archive, "", &a),
                    RefUpdate {
                        name: "HEAD".into(),
                        new_symbolic_target: "refs/heads/main".into(),
                        ..RefUpdate::default()
                    },
                ],
                push_options: Vec::new(),
                atomic: true,
            }),
            ..LogEntry::default()
        };
        entry.meta.insert("principal".into(), "upstream".into());
        entry.meta.insert(
            floe_catalog::FOLLOW_ARCHIVED_META.into(),
            format!("{archive} refs/heads/main {a} {b}"),
        );
        let rows = rows_from_entries(&id, std::slice::from_ref(&entry));
        let mut webhook = Vec::new();
        events::refs_from_entries(&id, std::slice::from_ref(&entry), &mut webhook);
        let ref_rows: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                Row::RefEvent(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(ref_rows.len(), webhook.len(), "{rows:?}");
        for (row, ev) in ref_rows.iter().zip(&webhook) {
            assert_eq!(row.ref_name, ev.ref_name);
            assert_eq!(row.action, ev.action.as_str());
            assert_eq!(
                (row.old_oid.as_str(), row.new_oid.as_str()),
                (ev.old.as_str(), ev.new.as_str())
            );
            assert_eq!(row.seq, 7);
            assert_eq!(row.principal, "upstream");
        }
        assert!(
            ref_rows
                .iter()
                .any(|r| r.is_archive && r.ref_name == archive)
        );
        let forced: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r, Row::ForcePush(_)))
            .collect();
        assert_eq!(forced.len(), 1, "{rows:?}");
    }

    #[test]
    fn snapshot_is_due_daily() {
        let now = Utc::now();
        assert!(snapshot_due(&InventorySchedule::default(), now));
        let recent = InventorySchedule {
            last_snapshot: Some(now - chrono::Duration::hours(3)),
            snapshot_id: None,
        };
        assert!(!snapshot_due(&recent, now));
        let old = InventorySchedule {
            last_snapshot: Some(now - chrono::Duration::hours(25)),
            snapshot_id: None,
        };
        assert!(snapshot_due(&old, now));
        // A clock behind the stored time is not due.
        let future = InventorySchedule {
            last_snapshot: Some(now + chrono::Duration::hours(1)),
            snapshot_id: None,
        };
        assert!(!snapshot_due(&future, now));
    }

    /// A host whose snapshot finished late never moves the schedule back
    /// over a newer one.
    #[test]
    fn schedule_never_moves_backwards() {
        let now = Utc::now();
        let done = InventorySchedule {
            last_snapshot: Some(now),
            snapshot_id: Some("mine".into()),
        };
        assert_eq!(advance_schedule(None, &done), Some(done.clone()));
        let older = InventorySchedule {
            last_snapshot: Some(now - chrono::Duration::hours(25)),
            snapshot_id: Some("old".into()),
        };
        assert_eq!(advance_schedule(Some(&older), &done), Some(done.clone()));
        let newer = InventorySchedule {
            last_snapshot: Some(now + chrono::Duration::minutes(5)),
            snapshot_id: Some("theirs".into()),
        };
        assert_eq!(advance_schedule(Some(&newer), &done), None);
    }

    #[test]
    fn inventory_rows_mark_own_and_mirrored() {
        let now = Utc::now();
        let own = inventory_row(now, "acme/app", None, Some(3), "s1");
        assert_eq!(own.source, "own");
        assert_eq!(own.status.as_deref(), Some("own"));
        assert_eq!(own.head_seq, Some(3));
        assert_eq!(own.snapshot_id.as_deref(), Some("s1"));
        assert_eq!(own.change, "snapshot");
        let entry: floe_mirror::RepoEntry = serde_json::from_value(serde_json::json!({
            "full_name": "Acme/Widgets",
            "floe": "gh-acme/widgets",
            "status": "active",
            "private": true,
            "archived": false,
            "fork": false,
            "default_branch": "main",
            "pushed_at": "2026-10-01T00:00:00Z",
            "size_kb": 42,
            "settings_revision": 1
        }))
        .unwrap();
        let sid = "123".to_string();
        let m = inventory_row(now, "gh-acme/widgets", Some((&sid, &entry)), None, "s1");
        assert_eq!(m.source, "github");
        assert_eq!(m.source_id.as_deref(), Some("123"));
        assert_eq!(m.full_name.as_deref(), Some("Acme/Widgets"));
        assert_eq!(m.status.as_deref(), Some("active"));
        assert_eq!(m.private, Some(true));
        assert!(m.pushed_at.is_some());
    }
}
