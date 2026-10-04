//! Rows of the four audit tables (`docs/design/github-mirror.md` §C.3). Plain
//! structs whose field names are the column names: they are the contract with
//! follow (§A), the mirror (§B) and the server's catalog tail. `ingested_at`
//! is not a field anywhere — the writer stamps it at commit time.

use std::collections::HashMap;
use std::hash::BuildHasher;

use chrono::{DateTime, Utc};
use floe_proto::v1::{EntryKind, LogEntry};

/// A UTC timestamp (`timestamptz`, µs in Iceberg).
pub type Timestamp = DateTime<Utc>;

/// The four tables. `name()` is the Iceberg table name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    RefEvents,
    ForcePushLog,
    SyncRuns,
    RepoInventory,
}

impl Table {
    pub const ALL: [Table; 4] = [
        Table::RefEvents,
        Table::ForcePushLog,
        Table::SyncRuns,
        Table::RepoInventory,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Table::RefEvents => "ref_events",
            Table::ForcePushLog => "force_push_log",
            Table::SyncRuns => "sync_runs",
            Table::RepoInventory => "repo_inventory",
        }
    }

    /// Dense index for per-table arrays.
    pub(crate) fn index(self) -> usize {
        match self {
            Table::RefEvents => 0,
            Table::ForcePushLog => 1,
            Table::SyncRuns => 2,
            Table::RepoInventory => 3,
        }
    }
}

impl std::fmt::Display for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `ref_events`: one ref transition committed to a repository's WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEventRow {
    /// `owner/name`.
    pub repo: String,
    pub seq: u64,
    pub ref_name: String,
    /// `create` / `update` / `delete` (docs/EVENTS.md).
    pub action: String,
    /// `branch` / `tag` / `""`.
    pub ref_type: String,
    /// Full zero OID on create.
    pub old_oid: String,
    /// Full zero OID on delete.
    pub new_oid: String,
    /// `meta.principal` (`upstream` for follow).
    pub principal: String,
    pub request_id: Option<String>,
    /// `push` / `ref_update`.
    pub entry_kind: String,
    pub writer: Option<String>,
    pub committed_at: Timestamp,
    /// `ref_name` starts with `refs/archive/`.
    pub is_archive: bool,
    /// `meta.upstream` when the principal is `upstream`.
    pub upstream: Option<String>,
}

/// One ref transition as the events bridge classifies it
/// (`events::refs_from_entries`, the single implementation of the zero-OID,
/// HEAD-skip and classify rules). The catalog does not re-derive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTransition {
    pub action: String,
    pub ref_type: String,
    pub ref_name: String,
    pub old_oid: String,
    pub new_oid: String,
}

impl RefEventRow {
    /// The row for `t`, taking provenance from the entry it came from (same
    /// seq). `None` for entry kinds that never move refs.
    pub fn from_entry(repo: &str, entry: &LogEntry, t: RefTransition) -> Option<RefEventRow> {
        let entry_kind = entry_kind_name(entry)?;
        let principal = entry.meta.get("principal").cloned().unwrap_or_default();
        let upstream = if principal == "upstream" {
            non_empty(entry.meta.get("upstream"))
        } else {
            None
        };
        Some(RefEventRow {
            repo: repo.to_string(),
            seq: entry.seq,
            is_archive: is_archive_ref(&t.ref_name),
            ref_name: t.ref_name,
            action: t.action,
            ref_type: t.ref_type,
            old_oid: t.old_oid,
            new_oid: t.new_oid,
            principal,
            request_id: non_empty(entry.meta.get("request_id")),
            entry_kind: entry_kind.to_string(),
            writer: Some(entry.writer.clone()).filter(|w| !w.is_empty()),
            committed_at: committed_at(entry),
            upstream,
        })
    }
}

/// Every durable row one log entry yields: a `ref_events` row per transition
/// (as `events::refs_from_entries` classified them for this entry) and a
/// `force_push_log` row per archive line.
pub fn rows_for_entry(
    repo: &str,
    entry: &LogEntry,
    transitions: impl IntoIterator<Item = RefTransition>,
) -> Vec<Row> {
    let mut out: Vec<Row> = transitions
        .into_iter()
        .filter_map(|t| RefEventRow::from_entry(repo, entry, t))
        .map(Row::RefEvent)
        .collect();
    out.extend(
        ForcePushRow::from_entry(repo, entry)
            .into_iter()
            .map(Row::ForcePush),
    );
    out
}

/// `push` / `ref_update`; `None` for COMPACT, CHECKPOINT, SETTINGS (no ref moves).
pub fn entry_kind_name(entry: &LogEntry) -> Option<&'static str> {
    match EntryKind::try_from(entry.kind) {
        Ok(EntryKind::Push) => Some("push"),
        Ok(EntryKind::RefUpdate) => Some("ref_update"),
        _ => None,
    }
}

/// `LogEntry.created_at` as UTC; the epoch when absent or out of range.
pub fn committed_at(entry: &LogEntry) -> Timestamp {
    entry
        .created_at
        .as_ref()
        .and_then(|t| {
            let nanos = u32::try_from(t.nanos).ok()?;
            DateTime::from_timestamp(t.seconds, nanos)
        })
        .unwrap_or_default()
}

/// Archive refs (§A.3) are reserved: follow writes them, nothing moves them.
pub fn is_archive_ref(name: &str) -> bool {
    name.starts_with(ARCHIVE_PREFIX)
}

const ARCHIVE_PREFIX: &str = "refs/archive/";

fn non_empty(v: Option<&String>) -> Option<String> {
    v.filter(|s| !s.is_empty()).cloned()
}

/// `force_push_log`: one rewrite or delete follow archived (§A.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcePushRow {
    pub repo: String,
    pub seq: u64,
    /// The original ref.
    pub ref_name: String,
    /// `rewrite` / `delete`.
    pub kind: String,
    /// The archived tip.
    pub old_oid: String,
    /// `None` on delete.
    pub new_oid: Option<String>,
    /// `refs/archive/<ts>/<ref>`.
    pub archive_ref: String,
    pub upstream: Option<String>,
    pub committed_at: Timestamp,
}

impl ForcePushRow {
    /// Every archive line of `entry` (empty when it archived nothing).
    pub fn from_entry(repo: &str, entry: &LogEntry) -> Vec<ForcePushRow> {
        if entry_kind_name(entry).is_none() {
            return Vec::new();
        }
        let upstream = non_empty(entry.meta.get("upstream"));
        let at = committed_at(entry);
        parse_follow_archived(&entry.meta)
            .into_iter()
            .map(|line| ForcePushRow {
                repo: repo.to_string(),
                seq: entry.seq,
                kind: if line.new_oid.is_some() {
                    "rewrite"
                } else {
                    "delete"
                }
                .to_string(),
                ref_name: line.original_ref,
                old_oid: line.old_oid,
                new_oid: line.new_oid,
                archive_ref: line.archive_ref,
                upstream: upstream.clone(),
                committed_at: at,
            })
            .collect()
    }
}

/// The log entry meta key follow writes when a round archived something (§A.5).
pub const FOLLOW_ARCHIVED_META: &str = "follow.archived";

/// One line of `meta["follow.archived"]`:
/// `<archive_ref> <original_ref> <old_oid> <new_oid|->`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedLine {
    pub archive_ref: String,
    pub original_ref: String,
    pub old_oid: String,
    /// `None` when upstream deleted the ref (`-`).
    pub new_oid: Option<String>,
}

/// Parse `meta["follow.archived"]` (§A.5). Pure and total: a malformed line is
/// skipped, logged and counted (`floe_catalog_malformed_archive_lines_total`),
/// never a panic — an odd entry must not wedge the catalog's cursor.
pub fn parse_follow_archived<S: BuildHasher>(
    meta: &HashMap<String, String, S>,
) -> Vec<ArchivedLine> {
    let Some(text) = meta.get(FOLLOW_ARCHIVED_META) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut malformed = 0u64;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(parsed) = parse_archived_line(line) {
            out.push(parsed);
        } else {
            malformed += 1;
            tracing::warn!(line, "catalog: malformed follow.archived line skipped");
        }
    }
    if malformed > 0 {
        metrics::counter!("floe_catalog_malformed_archive_lines_total").increment(malformed);
    }
    out
}

fn parse_archived_line(line: &str) -> Option<ArchivedLine> {
    let mut fields = line.split_ascii_whitespace();
    let (archive_ref, original_ref, old_oid, new_oid) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    if fields.next().is_some() || !is_oid(old_oid) {
        return None;
    }
    // `refs/archive/<unix-ts>/<original-ref>`, and the two must agree.
    let (ts, rest) = archive_ref.strip_prefix(ARCHIVE_PREFIX)?.split_once('/')?;
    if ts.is_empty() || !ts.bytes().all(|b| b.is_ascii_digit()) || rest != original_ref {
        return None;
    }
    let new_oid = match new_oid {
        "-" => None,
        oid if is_oid(oid) => Some(oid.to_string()),
        _ => return None,
    };
    Some(ArchivedLine {
        archive_ref: archive_ref.to_string(),
        original_ref: original_ref.to_string(),
        old_oid: old_oid.to_string(),
        new_oid,
    })
}

/// A full hex object id (SHA-1 or SHA-256).
fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `sync_runs`: one follow round that did work, one follow failure, or one
/// mirror pass. Lossy telemetry (§C.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRun {
    /// uuid v4.
    pub run_id: String,
    /// `follow` / `discovery`.
    pub kind: String,
    /// `upstream.source` prefix (`github`); `None` for hand-configured follow.
    pub source: Option<String>,
    /// `None` for `discovery`.
    pub repo: Option<String>,
    /// `floe_store::coord::instance_id()`.
    pub instance: String,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    /// follow: `in-sync`/`published`/`archived`/`refused`/`failed`;
    /// discovery: `ok`/`incomplete`/`failed`.
    pub outcome: String,
    pub refs_published: Option<u32>,
    pub refs_archived: Option<u32>,
    /// Pack bytes ingested.
    pub bytes_fetched: Option<u64>,
    /// Published seq.
    pub seq: Option<u64>,
    pub detail: Option<String>,
    pub api_requests: Option<u32>,
    pub api_not_modified: Option<u32>,
    pub rate_remaining: Option<u32>,
}

impl SyncRun {
    /// A run of `kind` started at `started_at` on this instance, with a fresh
    /// `run_id`; set the rest with struct update syntax.
    pub fn new(kind: &str, started_at: Timestamp) -> SyncRun {
        SyncRun {
            run_id: uuid::Uuid::new_v4().to_string(),
            kind: kind.to_string(),
            source: None,
            repo: None,
            instance: floe_store::coord::instance_id().to_string(),
            started_at,
            finished_at: started_at,
            outcome: String::new(),
            refs_published: None,
            refs_archived: None,
            bytes_fetched: None,
            seq: None,
            detail: None,
            api_requests: None,
            api_not_modified: None,
            rate_remaining: None,
        }
    }
}

/// `repo_inventory`: a change of one repository's record, or one row of the
/// daily snapshot (§C.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryRecord {
    pub observed_at: Timestamp,
    /// `snapshot`/`created`/`updated`/`renamed`/`archived`/`excluded`/`gone`/`detached`/`conflict`.
    pub change: String,
    pub floe_repo: String,
    /// `github` / `own`.
    pub source: String,
    pub source_id: Option<String>,
    pub full_name: Option<String>,
    /// Mirror status; `own` for own repositories.
    pub status: Option<String>,
    pub private: Option<bool>,
    pub archived: Option<bool>,
    pub fork: Option<bool>,
    pub default_branch: Option<String>,
    pub upstream_url: Option<String>,
    pub pushed_at: Option<Timestamp>,
    pub size_kb: Option<u64>,
    /// Snapshot rows only (the manifest's head).
    pub head_seq: Option<u64>,
    /// Snapshot rows only; the completed one is in `catalog/inventory.json`.
    pub snapshot_id: Option<String>,
}

impl InventoryRecord {
    /// A `change` record of `floe_repo` from `source`; set the rest with
    /// struct update syntax.
    pub fn new(
        observed_at: Timestamp,
        change: &str,
        floe_repo: &str,
        source: &str,
    ) -> InventoryRecord {
        InventoryRecord {
            observed_at,
            change: change.to_string(),
            floe_repo: floe_repo.to_string(),
            source: source.to_string(),
            source_id: None,
            full_name: None,
            status: None,
            private: None,
            archived: None,
            fork: None,
            default_branch: None,
            upstream_url: None,
            pushed_at: None,
            size_kb: None,
            head_seq: None,
            snapshot_id: None,
        }
    }
}

/// A row of any table: the buffer's unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    RefEvent(RefEventRow),
    ForcePush(ForcePushRow),
    SyncRun(SyncRun),
    Inventory(InventoryRecord),
}

impl Row {
    pub fn table(&self) -> Table {
        match self {
            Row::RefEvent(_) => Table::RefEvents,
            Row::ForcePush(_) => Table::ForcePushLog,
            Row::SyncRun(_) => Table::SyncRuns,
            Row::Inventory(_) => Table::RepoInventory,
        }
    }
}

impl From<RefEventRow> for Row {
    fn from(r: RefEventRow) -> Row {
        Row::RefEvent(r)
    }
}
impl From<ForcePushRow> for Row {
    fn from(r: ForcePushRow) -> Row {
        Row::ForcePush(r)
    }
}
impl From<SyncRun> for Row {
    fn from(r: SyncRun) -> Row {
        Row::SyncRun(r)
    }
}
impl From<InventoryRecord> for Row {
    fn from(r: InventoryRecord) -> Row {
        Row::Inventory(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floe_proto::v1::LogEntry;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    fn meta(text: &str) -> HashMap<String, String> {
        HashMap::from([(FOLLOW_ARCHIVED_META.to_string(), text.to_string())])
    }

    #[test]
    fn parse_follow_archived_reads_rewrites_and_deletes() {
        let text = format!(
            "refs/archive/1791072000/refs/heads/main refs/heads/main {A} {B}\n\
             refs/archive/1791072000/refs/heads/feat/old-api refs/heads/feat/old-api {B} -\n"
        );
        let lines = parse_follow_archived(&meta(&text));
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].archive_ref,
            "refs/archive/1791072000/refs/heads/main"
        );
        assert_eq!(lines[0].original_ref, "refs/heads/main");
        assert_eq!(lines[0].old_oid, A);
        assert_eq!(lines[0].new_oid.as_deref(), Some(B));
        assert_eq!(lines[1].original_ref, "refs/heads/feat/old-api");
        assert_eq!(lines[1].new_oid, None);
    }

    #[test]
    fn parse_follow_archived_skips_malformed_lines() {
        let text = format!(
            "garbage\n\
             refs/archive/1/refs/heads/a refs/heads/a {A}\n\
             refs/archive/1/refs/heads/a refs/heads/a {A} {B} extra\n\
             refs/archive/x1/refs/heads/a refs/heads/a {A} {B}\n\
             refs/archive/1/refs/heads/a refs/heads/b {A} {B}\n\
             refs/heads/a refs/heads/a {A} {B}\n\
             refs/archive/1/refs/heads/a refs/heads/a nothex {B}\n\
             refs/archive/1/refs/heads/a refs/heads/a {A} zz\n\
             \n\
             refs/archive/2/refs/tags/v1 refs/tags/v1 {A} {B}\n"
        );
        let lines = parse_follow_archived(&meta(&text));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].original_ref, "refs/tags/v1");
        assert!(parse_follow_archived(&HashMap::new()).is_empty());
        assert!(parse_follow_archived(&meta("")).is_empty());
    }

    fn entry(kind: EntryKind, meta: &[(&str, &str)]) -> LogEntry {
        LogEntry {
            seq: 7,
            kind: kind as i32,
            created_at: Some(floe_proto::prost_types::Timestamp {
                seconds: 1_791_072_000,
                nanos: 5_000,
            }),
            writer: "host-a".into(),
            meta: meta
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..LogEntry::default()
        }
    }

    #[test]
    fn ref_event_row_takes_provenance_from_the_entry() {
        let e = entry(
            EntryKind::Push,
            &[
                ("principal", "upstream"),
                ("upstream", "https://github.com/o/r.git"),
                ("request_id", "rq"),
            ],
        );
        let t = RefTransition {
            action: "create".into(),
            ref_type: String::new(),
            ref_name: "refs/archive/1791072000/refs/heads/main".into(),
            old_oid: "0".repeat(40),
            new_oid: A.into(),
        };
        let row = RefEventRow::from_entry("o/r", &e, t).unwrap();
        assert_eq!(row.seq, 7);
        assert!(row.is_archive);
        assert_eq!(row.entry_kind, "push");
        assert_eq!(row.principal, "upstream");
        assert_eq!(row.upstream.as_deref(), Some("https://github.com/o/r.git"));
        assert_eq!(row.request_id.as_deref(), Some("rq"));
        assert_eq!(row.writer.as_deref(), Some("host-a"));
        assert_eq!(row.committed_at.timestamp(), 1_791_072_000);
        assert_eq!(row.committed_at.timestamp_subsec_micros(), 5);

        // A human push: no upstream column even if the meta key leaked in.
        let human = entry(
            EntryKind::RefUpdate,
            &[("principal", "alice"), ("upstream", "x")],
        );
        let t = RefTransition {
            action: "update".into(),
            ref_type: "branch".into(),
            ref_name: "refs/heads/main".into(),
            old_oid: A.into(),
            new_oid: B.into(),
        };
        let row = RefEventRow::from_entry("o/r", &human, t.clone()).unwrap();
        assert_eq!(row.entry_kind, "ref_update");
        assert!(!row.is_archive);
        assert_eq!(row.upstream, None);
        assert_eq!(row.request_id, None);

        assert!(RefEventRow::from_entry("o/r", &entry(EntryKind::Compact, &[]), t).is_none());
    }

    #[test]
    fn force_push_rows_come_from_the_archive_meta() {
        let text = format!(
            "refs/archive/9/refs/heads/main refs/heads/main {A} {B}\nrefs/archive/9/refs/tags/t refs/tags/t {B} -"
        );
        let e = entry(
            EntryKind::Push,
            &[
                ("principal", "upstream"),
                ("upstream", "u"),
                (FOLLOW_ARCHIVED_META, &text),
            ],
        );
        let rows = ForcePushRow::from_entry("o/r", &e);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "rewrite");
        assert_eq!(rows[0].ref_name, "refs/heads/main");
        assert_eq!(rows[0].new_oid.as_deref(), Some(B));
        assert_eq!(rows[1].kind, "delete");
        assert_eq!(rows[1].archive_ref, "refs/archive/9/refs/tags/t");
        assert_eq!(rows[1].upstream.as_deref(), Some("u"));
        assert!(ForcePushRow::from_entry("o/r", &entry(EntryKind::Push, &[])).is_empty());

        // One entry: the archive create, the rewrite, and the log line.
        let t = |name: &str, old: &str, new: &str, action: &str| RefTransition {
            action: action.into(),
            ref_type: String::new(),
            ref_name: name.into(),
            old_oid: old.into(),
            new_oid: new.into(),
        };
        let zero = "0".repeat(40);
        let rows = rows_for_entry(
            "o/r",
            &e,
            [
                t("refs/archive/9/refs/heads/main", &zero, A, "create"),
                t("refs/heads/main", A, B, "update"),
            ],
        );
        let tables: Vec<Table> = rows.iter().map(Row::table).collect();
        assert_eq!(
            tables,
            [
                Table::RefEvents,
                Table::RefEvents,
                Table::ForcePushLog,
                Table::ForcePushLog
            ]
        );
        assert!(matches!(&rows[0], Row::RefEvent(r) if r.is_archive));
    }

    #[test]
    fn rows_know_their_table() {
        let now = Utc::now();
        let run = SyncRun::new("follow", now);
        assert_eq!(run.run_id.len(), 36);
        assert_eq!(Row::from(run).table(), Table::SyncRuns);
        let inv = InventoryRecord::new(now, "snapshot", "o/r", "own");
        assert_eq!(Row::from(inv).table(), Table::RepoInventory);
        for (i, t) in Table::ALL.iter().enumerate() {
            assert_eq!(t.index(), i);
        }
    }
}
