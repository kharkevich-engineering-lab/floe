//! Iceberg schemas, partition specs and sort orders of the four tables
//! (`docs/design/github-mirror.md` §C.3), and rows → Arrow `RecordBatch`.
//!
//! Field ids are fixed here and append-only: schema evolution adds fields,
//! never reuses an id. Each table is one column list ([`columns`]) and one
//! value list per row ([`row_values`]) in the same order; the tests build a
//! batch for every table, so the two cannot drift silently.

use std::sync::Arc;

use ::iceberg::spec::{
    NestedField, NullOrder, PrimitiveType, Schema, SortDirection, SortField, SortOrder, Transform,
    Type, UnboundPartitionSpec,
};
use ::iceberg::{Error, ErrorKind, Result};
use arrow_array::builder::{
    BooleanBuilder, Int32Builder, Int64Builder, StringBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};

use crate::rows::{Row, Table, Timestamp};

/// The column types the tables use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Str,
    Long,
    Int,
    Bool,
    /// `timestamptz` (µs, UTC).
    Ts,
}

#[derive(Debug, Clone, Copy)]
struct Col {
    id: i32,
    name: &'static str,
    ty: Ty,
    required: bool,
}

const fn req(id: i32, name: &'static str, ty: Ty) -> Col {
    Col {
        id,
        name,
        ty,
        required: true,
    }
}

const fn opt(id: i32, name: &'static str, ty: Ty) -> Col {
    Col {
        id,
        name,
        ty,
        required: false,
    }
}

const REF_EVENTS: &[Col] = &[
    req(1, "repo", Ty::Str),
    req(2, "seq", Ty::Long),
    req(3, "ref_name", Ty::Str),
    req(4, "action", Ty::Str),
    req(5, "ref_type", Ty::Str),
    req(6, "old_oid", Ty::Str),
    req(7, "new_oid", Ty::Str),
    req(8, "principal", Ty::Str),
    opt(9, "request_id", Ty::Str),
    req(10, "entry_kind", Ty::Str),
    opt(11, "writer", Ty::Str),
    req(12, "committed_at", Ty::Ts),
    req(13, "ingested_at", Ty::Ts),
    req(14, "is_archive", Ty::Bool),
    opt(15, "upstream", Ty::Str),
];

const FORCE_PUSH_LOG: &[Col] = &[
    req(1, "repo", Ty::Str),
    req(2, "seq", Ty::Long),
    req(3, "ref_name", Ty::Str),
    req(4, "kind", Ty::Str),
    req(5, "old_oid", Ty::Str),
    opt(6, "new_oid", Ty::Str),
    req(7, "archive_ref", Ty::Str),
    opt(8, "upstream", Ty::Str),
    req(9, "committed_at", Ty::Ts),
    req(10, "ingested_at", Ty::Ts),
];

const SYNC_RUNS: &[Col] = &[
    req(1, "run_id", Ty::Str),
    req(2, "kind", Ty::Str),
    opt(3, "source", Ty::Str),
    opt(4, "repo", Ty::Str),
    req(5, "instance", Ty::Str),
    req(6, "started_at", Ty::Ts),
    req(7, "finished_at", Ty::Ts),
    req(8, "outcome", Ty::Str),
    opt(9, "refs_published", Ty::Int),
    opt(10, "refs_archived", Ty::Int),
    opt(11, "bytes_fetched", Ty::Long),
    opt(12, "seq", Ty::Long),
    opt(13, "detail", Ty::Str),
    opt(14, "api_requests", Ty::Int),
    opt(15, "api_not_modified", Ty::Int),
    opt(16, "rate_remaining", Ty::Int),
];

const REPO_INVENTORY: &[Col] = &[
    req(1, "observed_at", Ty::Ts),
    req(2, "change", Ty::Str),
    req(3, "floe_repo", Ty::Str),
    req(4, "source", Ty::Str),
    opt(5, "source_id", Ty::Str),
    opt(6, "full_name", Ty::Str),
    opt(7, "status", Ty::Str),
    opt(8, "private", Ty::Bool),
    opt(9, "archived", Ty::Bool),
    opt(10, "fork", Ty::Bool),
    opt(11, "default_branch", Ty::Str),
    opt(12, "upstream_url", Ty::Str),
    opt(13, "pushed_at", Ty::Ts),
    opt(14, "size_kb", Ty::Long),
    opt(15, "head_seq", Ty::Long),
    opt(16, "snapshot_id", Ty::Str),
];

fn columns(table: Table) -> &'static [Col] {
    match table {
        Table::RefEvents => REF_EVENTS,
        Table::ForcePushLog => FORCE_PUSH_LOG,
        Table::SyncRuns => SYNC_RUNS,
        Table::RepoInventory => REPO_INVENTORY,
    }
}

/// The dedup key, declared as identifier fields (documentation; appends do
/// not enforce it): `(repo, seq, ref_name)`.
fn identifier_ids(table: Table) -> &'static [i32] {
    match table {
        Table::RefEvents | Table::ForcePushLog => &[1, 2, 3],
        Table::SyncRuns | Table::RepoInventory => &[],
    }
}

/// The table's Iceberg schema (schema id 0).
pub fn schema(table: Table) -> Result<Schema> {
    let fields = columns(table).iter().map(|c| {
        let ty = Type::Primitive(match c.ty {
            Ty::Str => PrimitiveType::String,
            Ty::Long => PrimitiveType::Long,
            Ty::Int => PrimitiveType::Int,
            Ty::Bool => PrimitiveType::Boolean,
            Ty::Ts => PrimitiveType::Timestamptz,
        });
        Arc::new(if c.required {
            NestedField::required(c.id, c.name, ty)
        } else {
            NestedField::optional(c.id, c.name, ty)
        })
    });
    Schema::builder()
        .with_schema_id(0)
        .with_fields(fields)
        .with_identifier_field_ids(identifier_ids(table).iter().copied())
        .build()
}

/// `day(committed_at)`, `month(committed_at)`, `day(started_at)`, `day(observed_at)`.
pub fn partition_spec(table: Table) -> Result<UnboundPartitionSpec> {
    let (source_id, name, transform) = match table {
        Table::RefEvents => (12, "committed_at_day", Transform::Day),
        Table::ForcePushLog => (9, "committed_at_month", Transform::Month),
        Table::SyncRuns => (6, "started_at_day", Transform::Day),
        Table::RepoInventory => (1, "observed_at_day", Transform::Day),
    };
    Ok(UnboundPartitionSpec::builder()
        .add_partition_field(source_id, name, transform)?
        .build())
}

/// `ref_events` is sorted by `repo, seq`; the others are unsorted.
pub fn sort_order(table: Table) -> Result<SortOrder> {
    let ids: &[i32] = match table {
        Table::RefEvents => &[1, 2],
        _ => &[],
    };
    if ids.is_empty() {
        return Ok(SortOrder::unsorted_order());
    }
    let mut builder = SortOrder::builder();
    builder.with_order_id(1);
    for id in ids {
        builder.with_sort_field(
            SortField::builder()
                .source_id(*id)
                .transform(Transform::Identity)
                .direction(SortDirection::Ascending)
                .null_order(NullOrder::First)
                .build(),
        );
    }
    builder.build_unbound()
}

/// Our columns must be what the catalog holds (names, types, requiredness by
/// id). A catalog that reassigned ids, or a table someone altered, fails
/// loudly at connect instead of writing misplaced columns.
pub fn check_compatible(table: Table, actual: &Schema) -> Result<()> {
    let expected = schema(table)?;
    for field in expected.as_struct().fields() {
        let found = actual.field_by_id(field.id);
        let same = found.is_some_and(|f| {
            f.name == field.name && f.field_type == field.field_type && f.required == field.required
        });
        if !same {
            return Err(Error::new(
                ErrorKind::DataInvalid,
                format!(
                    "{table}: field {} ({}) differs in the catalog; floe expects the schema of docs/design/github-mirror.md §C.3",
                    field.id, field.name
                ),
            ));
        }
    }
    Ok(())
}

/// One cell, in column order.
enum Value<'a> {
    Str(Option<&'a str>),
    Long(Option<i64>),
    Int(Option<i32>),
    Bool(Option<bool>),
    Ts(Option<Timestamp>),
}

fn long(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn int(v: Option<u32>) -> Option<i32> {
    v.map(|v| i32::try_from(v).unwrap_or(i32::MAX))
}

/// The cells of `row`, in [`columns`] order. `ingested_at` is the commit time.
fn row_values(row: &Row, ingested_at: Timestamp) -> Vec<Value<'_>> {
    use Value::{Bool, Int, Long, Str, Ts};
    match row {
        Row::RefEvent(r) => vec![
            Str(Some(&r.repo)),
            Long(Some(long(r.seq))),
            Str(Some(&r.ref_name)),
            Str(Some(&r.action)),
            Str(Some(&r.ref_type)),
            Str(Some(&r.old_oid)),
            Str(Some(&r.new_oid)),
            Str(Some(&r.principal)),
            Str(r.request_id.as_deref()),
            Str(Some(&r.entry_kind)),
            Str(r.writer.as_deref()),
            Ts(Some(r.committed_at)),
            Ts(Some(ingested_at)),
            Bool(Some(r.is_archive)),
            Str(r.upstream.as_deref()),
        ],
        Row::ForcePush(r) => vec![
            Str(Some(&r.repo)),
            Long(Some(long(r.seq))),
            Str(Some(&r.ref_name)),
            Str(Some(&r.kind)),
            Str(Some(&r.old_oid)),
            Str(r.new_oid.as_deref()),
            Str(Some(&r.archive_ref)),
            Str(r.upstream.as_deref()),
            Ts(Some(r.committed_at)),
            Ts(Some(ingested_at)),
        ],
        Row::SyncRun(r) => vec![
            Str(Some(&r.run_id)),
            Str(Some(&r.kind)),
            Str(r.source.as_deref()),
            Str(r.repo.as_deref()),
            Str(Some(&r.instance)),
            Ts(Some(r.started_at)),
            Ts(Some(r.finished_at)),
            Str(Some(&r.outcome)),
            Int(int(r.refs_published)),
            Int(int(r.refs_archived)),
            Long(r.bytes_fetched.map(long)),
            Long(r.seq.map(long)),
            Str(r.detail.as_deref()),
            Int(int(r.api_requests)),
            Int(int(r.api_not_modified)),
            Int(int(r.rate_remaining)),
        ],
        Row::Inventory(r) => vec![
            Ts(Some(r.observed_at)),
            Str(Some(&r.change)),
            Str(Some(&r.floe_repo)),
            Str(Some(&r.source)),
            Str(r.source_id.as_deref()),
            Str(r.full_name.as_deref()),
            Str(r.status.as_deref()),
            Bool(r.private),
            Bool(r.archived),
            Bool(r.fork),
            Str(r.default_branch.as_deref()),
            Str(r.upstream_url.as_deref()),
            Ts(r.pushed_at),
            Long(r.size_kb.map(long)),
            Long(r.head_seq.map(long)),
            Str(r.snapshot_id.as_deref()),
        ],
    }
}

enum Builder {
    Str(StringBuilder),
    Long(Int64Builder),
    Int(Int32Builder),
    Bool(BooleanBuilder),
    Ts(TimestampMicrosecondBuilder),
}

impl Builder {
    fn new(ty: Ty, capacity: usize) -> Builder {
        match ty {
            Ty::Str => Builder::Str(StringBuilder::with_capacity(capacity, capacity * 32)),
            Ty::Long => Builder::Long(Int64Builder::with_capacity(capacity)),
            Ty::Int => Builder::Int(Int32Builder::with_capacity(capacity)),
            Ty::Bool => Builder::Bool(BooleanBuilder::with_capacity(capacity)),
            Ty::Ts => Builder::Ts(TimestampMicrosecondBuilder::with_capacity(capacity)),
        }
    }

    fn push(&mut self, v: Value<'_>) -> bool {
        match (self, v) {
            (Builder::Str(b), Value::Str(v)) => b.append_option(v),
            (Builder::Long(b), Value::Long(v)) => b.append_option(v),
            (Builder::Int(b), Value::Int(v)) => b.append_option(v),
            (Builder::Bool(b), Value::Bool(v)) => b.append_option(v),
            (Builder::Ts(b), Value::Ts(v)) => b.append_option(v.map(|t| t.timestamp_micros())),
            _ => return false,
        }
        true
    }

    fn finish(self) -> ArrayRef {
        match self {
            Builder::Str(mut b) => Arc::new(b.finish()),
            Builder::Long(mut b) => Arc::new(b.finish()),
            Builder::Int(mut b) => Arc::new(b.finish()),
            Builder::Bool(mut b) => Arc::new(b.finish()),
            Builder::Ts(mut b) => {
                Arc::new(b.finish().with_timezone(::iceberg::arrow::UTC_TIME_ZONE))
            }
        }
    }
}

/// `rows` (all of `table`) as one batch of `schema` (the table's current
/// schema, already [`check_compatible`]), stamping `ingested_at`.
pub fn record_batch(
    table: Table,
    schema: &Schema,
    rows: &[Row],
    ingested_at: Timestamp,
) -> Result<RecordBatch> {
    let cols = columns(table);
    let mut builders: Vec<Builder> = cols
        .iter()
        .map(|c| Builder::new(c.ty, rows.len()))
        .collect();
    for row in rows {
        if row.table() != table {
            return Err(Error::new(
                ErrorKind::DataInvalid,
                format!("a {} row in a {table} batch", row.table()),
            ));
        }
        let values = row_values(row, ingested_at);
        if values.len() != builders.len() {
            return Err(Error::new(
                ErrorKind::Unexpected,
                format!(
                    "{table}: {} values for {} columns",
                    values.len(),
                    builders.len()
                ),
            ));
        }
        for (b, v) in builders.iter_mut().zip(values) {
            if !b.push(v) {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    format!("{table}: a value does not match its column type"),
                ));
            }
        }
    }
    // Project our columns onto the catalog's schema by field id.
    let arrow = ::iceberg::arrow::schema_to_arrow_schema(schema)?;
    let mut by_id: Vec<(i32, ArrayRef)> = cols
        .iter()
        .zip(builders)
        .map(|(c, b)| (c.id, b.finish()))
        .collect();
    let mut arrays = Vec::with_capacity(arrow.fields().len());
    for field in schema.as_struct().fields() {
        let pos = by_id.iter().position(|(id, _)| *id == field.id);
        match pos {
            Some(pos) => arrays.push(by_id.swap_remove(pos).1),
            None if !field.required => {
                arrays.push(arrow_array::new_null_array(
                    &::iceberg::arrow::type_to_arrow_type(&field.field_type)?,
                    rows.len(),
                ));
            }
            None => {
                return Err(Error::new(
                    ErrorKind::DataInvalid,
                    format!(
                        "{table}: required field {} ({}) is not floe's",
                        field.id, field.name
                    ),
                ));
            }
        }
    }
    RecordBatch::try_new(Arc::new(arrow), arrays).map_err(|e| {
        Error::new(
            ErrorKind::Unexpected,
            format!("{table}: building the record batch"),
        )
        .with_source(e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::{ForcePushRow, InventoryRecord, RefEventRow, SyncRun};
    use chrono::DateTime;

    fn at() -> Timestamp {
        DateTime::from_timestamp(1_791_072_000, 123_456_000).unwrap()
    }

    fn sample(table: Table) -> Row {
        match table {
            Table::RefEvents => Row::RefEvent(RefEventRow {
                repo: "o/r".into(),
                seq: 7,
                ref_name: "refs/heads/main".into(),
                action: "update".into(),
                ref_type: "branch".into(),
                old_oid: "1".repeat(40),
                new_oid: "2".repeat(40),
                principal: "upstream".into(),
                request_id: None,
                entry_kind: "push".into(),
                writer: Some("h".into()),
                committed_at: at(),
                is_archive: false,
                upstream: Some("https://github.com/o/r.git".into()),
            }),
            Table::ForcePushLog => Row::ForcePush(ForcePushRow {
                repo: "o/r".into(),
                seq: 7,
                ref_name: "refs/heads/main".into(),
                kind: "delete".into(),
                old_oid: "1".repeat(40),
                new_oid: None,
                archive_ref: "refs/archive/1/refs/heads/main".into(),
                upstream: None,
                committed_at: at(),
            }),
            Table::SyncRuns => Row::SyncRun(SyncRun {
                refs_published: Some(3),
                bytes_fetched: Some(u64::MAX),
                ..SyncRun::new("follow", at())
            }),
            Table::RepoInventory => Row::Inventory(InventoryRecord {
                private: Some(true),
                pushed_at: Some(at()),
                size_kb: Some(12),
                ..InventoryRecord::new(at(), "snapshot", "o/r", "own")
            }),
        }
    }

    /// The field ids are the contract (append-only): pin them.
    #[test]
    fn field_ids_are_stable() {
        let pinned = |t: Table| -> Vec<(i32, String, bool)> {
            schema(t)
                .unwrap()
                .as_struct()
                .fields()
                .iter()
                .map(|f| (f.id, f.name.clone(), f.required))
                .collect()
        };
        let names = |t: Table| {
            pinned(t)
                .into_iter()
                .map(|(_, n, _)| n)
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(
            names(Table::RefEvents),
            "repo,seq,ref_name,action,ref_type,old_oid,new_oid,principal,request_id,entry_kind,writer,committed_at,ingested_at,is_archive,upstream"
        );
        assert_eq!(
            names(Table::ForcePushLog),
            "repo,seq,ref_name,kind,old_oid,new_oid,archive_ref,upstream,committed_at,ingested_at"
        );
        assert_eq!(
            names(Table::SyncRuns),
            "run_id,kind,source,repo,instance,started_at,finished_at,outcome,refs_published,refs_archived,bytes_fetched,seq,detail,api_requests,api_not_modified,rate_remaining"
        );
        assert_eq!(
            names(Table::RepoInventory),
            "observed_at,change,floe_repo,source,source_id,full_name,status,private,archived,fork,default_branch,upstream_url,pushed_at,size_kb,head_seq,snapshot_id"
        );
        // Types too (s/l/i/b/t = string/long/int/boolean/timestamptz, `!` =
        // required), read back from the Iceberg schema, not from `columns`.
        let types = |t: Table| {
            schema(t)
                .unwrap()
                .as_struct()
                .fields()
                .iter()
                .map(|f| {
                    let ty = match *f.field_type {
                        Type::Primitive(PrimitiveType::String) => "s",
                        Type::Primitive(PrimitiveType::Long) => "l",
                        Type::Primitive(PrimitiveType::Int) => "i",
                        Type::Primitive(PrimitiveType::Boolean) => "b",
                        Type::Primitive(PrimitiveType::Timestamptz) => "t",
                        _ => "?",
                    };
                    format!("{ty}{}", if f.required { "!" } else { "" })
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(
            types(Table::RefEvents),
            "s!,l!,s!,s!,s!,s!,s!,s!,s,s!,s,t!,t!,b!,s"
        );
        assert_eq!(types(Table::ForcePushLog), "s!,l!,s!,s!,s!,s,s!,s,t!,t!");
        assert_eq!(
            types(Table::SyncRuns),
            "s!,s!,s,s,s!,t!,t!,s!,i,i,l,l,s,i,i,i"
        );
        assert_eq!(
            types(Table::RepoInventory),
            "t!,s!,s!,s!,s,s,s,b,b,b,s,s,t,l,l,s"
        );
        for t in Table::ALL {
            // Ids are 1..=n in declaration order.
            for (i, (id, _, _)) in pinned(t).into_iter().enumerate() {
                assert_eq!(usize::try_from(id).unwrap(), i + 1, "{t}");
            }
        }
        let mut ids: Vec<i32> = schema(Table::RefEvents)
            .unwrap()
            .identifier_field_ids()
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn arrow_and_iceberg_schemas_round_trip() {
        for t in Table::ALL {
            let s = schema(t).unwrap();
            let arrow = ::iceberg::arrow::schema_to_arrow_schema(&s).unwrap();
            let back = ::iceberg::arrow::arrow_schema_to_schema(&arrow).unwrap();
            assert_eq!(back.as_struct(), s.as_struct(), "{t}");
            check_compatible(t, &back).unwrap();
        }
    }

    #[test]
    fn partition_specs_and_sort_orders_bind() {
        for t in Table::ALL {
            let s = Arc::new(schema(t).unwrap());
            let spec = partition_spec(t).unwrap();
            let bound = spec.bind(s.clone()).unwrap();
            assert_eq!(bound.fields().len(), 1, "{t}");
            for f in &sort_order(t).unwrap().fields {
                assert!(s.field_by_id(f.source_id).is_some(), "{t}");
            }
        }
        assert_eq!(sort_order(Table::RefEvents).unwrap().fields.len(), 2);
        assert!(sort_order(Table::SyncRuns).unwrap().is_unsorted());
    }

    #[test]
    fn every_table_builds_a_batch() {
        for t in Table::ALL {
            let s = schema(t).unwrap();
            let batch = record_batch(t, &s, &[sample(t), sample(t)], at()).unwrap();
            assert_eq!(batch.num_rows(), 2, "{t}");
            assert_eq!(batch.num_columns(), s.as_struct().fields().len(), "{t}");
        }
        let s = schema(Table::SyncRuns).unwrap();
        assert!(record_batch(Table::SyncRuns, &s, &[sample(Table::RefEvents)], at()).is_err());
    }

    #[test]
    fn a_drifted_catalog_schema_is_refused() {
        let altered = Schema::builder()
            .with_fields([
                Arc::new(NestedField::required(
                    1,
                    "repo",
                    Type::Primitive(PrimitiveType::String),
                )),
                Arc::new(NestedField::required(
                    2,
                    "seq",
                    Type::Primitive(PrimitiveType::Int),
                )),
            ])
            .build()
            .unwrap();
        let err = check_compatible(Table::RefEvents, &altered).unwrap_err();
        assert!(err.to_string().contains("seq"), "{err}");
    }
}
