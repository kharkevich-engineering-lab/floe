//! Against a live Iceberg REST catalog (`just test-catalog`: `RustFS` S3 Tables
//! with `SigV4`; `just test-catalog-fixture`: the `iceberg-rest` fixture; both
//! from `podman compose --profile catalog up`).
//! Ignored by default and compiled only with `--features iceberg`:
//!
//! ```text
//! FLOE_TEST_CATALOG_URI=http://localhost:9000/iceberg FLOE_TEST_CATALOG_WAREHOUSE=floe-catalog \
//! FLOE_TEST_CATALOG_AUTH=sigv4 FLOE_TEST_CATALOG_S3_ENDPOINT=http://localhost:9000 \
//! AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… \
//!   cargo test -p floe-catalog --features iceberg --test live_catalog -- --ignored
//! ```
//!
//! `FLOE_TEST_CATALOG_AUTH` is `sigv4` (default), `none`, or `bearer` (with
//! `FLOE_TEST_CATALOG_TOKEN`); `FLOE_TEST_CATALOG_SIGV4_SERVICE` defaults to `s3`.
//!
//! Every run uses a fresh namespace, so runs never see each other's rows.
#![cfg(feature = "iceberg")]
// Helpers outside #[test] fns fail the test the same way (clippy.toml only
// exempts the test functions themselves).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use floe_catalog::iceberg::IcebergCommitter;
use floe_catalog::{
    CatalogAuth, CatalogConfig, CatalogWriter, Committer, FlushPolicy, ForcePushRow,
    InventoryRecord, Recorder, RefEventRow, Row, SyncRun, Table,
};
use futures::TryStreamExt;

fn config() -> CatalogConfig {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    CatalogConfig {
        enabled: true,
        uri: Some(env("FLOE_TEST_CATALOG_URI").expect("FLOE_TEST_CATALOG_URI")),
        warehouse: Some(
            env("FLOE_TEST_CATALOG_WAREHOUSE").unwrap_or_else(|| "floe-catalog".into()),
        ),
        namespace: format!("floe_test_{}", uuid::Uuid::new_v4().simple()),
        s3_endpoint: env("FLOE_TEST_CATALOG_S3_ENDPOINT"),
        auth: match env("FLOE_TEST_CATALOG_AUTH").as_deref() {
            None | Some("sigv4") => CatalogAuth::Sigv4,
            Some("none") => CatalogAuth::None,
            Some("bearer") => CatalogAuth::Bearer,
            Some(other) => panic!("FLOE_TEST_CATALOG_AUTH={other}: sigv4, none or bearer"),
        },
        sigv4_service: env("FLOE_TEST_CATALOG_SIGV4_SERVICE").unwrap_or_else(|| "s3".into()),
        token_env: env("FLOE_TEST_CATALOG_TOKEN").map(|_| "FLOE_TEST_CATALOG_TOKEN".into()),
        ..CatalogConfig::default()
    }
}

fn policy() -> FlushPolicy {
    FlushPolicy {
        flush_interval: Duration::from_millis(200),
        flush_rows: 1000,
        max_buffer_rows: 10_000,
        commit_timeout: Duration::from_mins(1),
    }
}

async fn count(committer: &IcebergCommitter, table: Table) -> usize {
    let t = committer.load_table(table).await.unwrap();
    let batches: Vec<_> = t
        .scan()
        .select_all()
        .build()
        .unwrap()
        .to_arrow()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    batches.iter().map(arrow_array::RecordBatch::num_rows).sum()
}

fn ref_event(seq: u64) -> Row {
    Row::RefEvent(RefEventRow {
        repo: "o/r".into(),
        seq,
        ref_name: "refs/heads/main".into(),
        action: "update".into(),
        ref_type: "branch".into(),
        old_oid: "1".repeat(40),
        new_oid: "2".repeat(40),
        principal: "upstream".into(),
        request_id: None,
        entry_kind: "push".into(),
        writer: None,
        committed_at: Utc::now(),
        is_archive: false,
        upstream: Some("https://github.com/o/r.git".into()),
    })
}

#[tokio::test]
#[ignore = "needs a live Iceberg REST catalog (FLOE_TEST_CATALOG_URI)"]
async fn appends_and_reads_back_all_four_tables() {
    let committer = Arc::new(IcebergCommitter::new(&config()));
    let writer = CatalogWriter::start(committer.clone(), policy());
    for _ in 0..600 {
        if writer.is_up() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(writer.is_up(), "never connected");

    let force = Row::ForcePush(ForcePushRow {
        repo: "o/r".into(),
        seq: 2,
        ref_name: "refs/heads/main".into(),
        kind: "rewrite".into(),
        old_oid: "1".repeat(40),
        new_oid: Some("2".repeat(40)),
        archive_ref: "refs/archive/1791072000/refs/heads/main".into(),
        upstream: None,
        committed_at: Utc::now(),
    });
    writer
        .append_durable(vec![ref_event(1), ref_event(2), force])
        .await
        .unwrap();
    writer.record_sync_run(SyncRun {
        outcome: "published".into(),
        repo: Some("o/r".into()),
        ..SyncRun::new("follow", Utc::now())
    });
    writer.record_inventory(InventoryRecord::new(
        Utc::now(),
        "created",
        "o/r",
        "github",
    ));
    writer.shutdown().await;

    assert_eq!(count(&committer, Table::RefEvents).await, 2);
    assert_eq!(count(&committer, Table::ForcePushLog).await, 1);
    assert_eq!(count(&committer, Table::SyncRuns).await, 1);
    assert_eq!(count(&committer, Table::RepoInventory).await, 1);
}

#[tokio::test]
#[ignore = "needs a live Iceberg REST catalog (FLOE_TEST_CATALOG_URI)"]
async fn concurrent_writers_retry_conflicts() {
    let cfg = config();
    let a = Arc::new(IcebergCommitter::new(&cfg));
    let b = Arc::new(IcebergCommitter::new(&cfg));
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    let rows_a: Vec<Row> = (0..10).map(ref_event).collect();
    let rows_b: Vec<Row> = (10..25).map(ref_event).collect();
    let (ra, rb) = tokio::join!(
        a.commit(Table::RefEvents, &rows_a, Utc::now()),
        b.commit(Table::RefEvents, &rows_b, Utc::now()),
    );
    ra.unwrap();
    rb.unwrap();
    assert_eq!(count(&a, Table::RefEvents).await, 25);
}
