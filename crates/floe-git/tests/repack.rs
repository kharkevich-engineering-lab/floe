// Integration tests fail by panicking; clippy.toml's allow-*-in-tests only reaches #[test] fns,
// not the helpers around them, so the panic-path lints are lifted for the whole test crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "test code: a panic is how a test fails"
)]
mod common;

use floe_git::{
    IngestOptions, LocalRepo, ObjectFormat, RepackMode, RepackOptions, RepoId, gix_hash,
};
use floe_proto::v1::{RefTransaction, RefUpdate};

/// A full repack is published as a COMPACT superseding every pack it read, so it must keep
/// what no ref reaches: here a side commit whose branch is gone (a force-push or a ref
/// delete), and which a ref published during a base rebuild could point at again.
#[tokio::test]
async fn a_full_repack_keeps_objects_no_ref_reaches() {
    let source = common::SourceRepo::new();
    let main = source.commit_file("a.txt", "a\n", "main");
    source.branch("side");
    let side = source.commit_file("b.txt", "b\n", "side");

    let root = tempfile::tempdir().unwrap();
    let repo = LocalRepo::init(
        root.path(),
        &RepoId::new("test", "keep").unwrap(),
        ObjectFormat::Sha1,
    )
    .unwrap();
    for pack in [
        source.pack(&[&main], &[], false),
        source.pack(&[&side], &[&main], false),
    ] {
        repo.ingest_pack(
            common::cursor(pack),
            IngestOptions {
                fsck: false,
                max_bytes: None,
                thin: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
    }
    // Only main is a ref: `side` is unreachable.
    repo.apply_ref_txn(
        &RefTransaction {
            updates: vec![RefUpdate {
                name: "refs/heads/main".into(),
                old_oid: String::new(),
                new_oid: main.clone(),
                new_symbolic_target: String::new(),
                new_peeled: String::new(),
            }],
            push_options: vec![],
            atomic: true,
        },
        false,
    )
    .unwrap();
    assert_eq!(repo.packs().unwrap().len(), 2);

    let result = repo
        .repack(RepackOptions {
            mode: RepackMode::Full,
            write_bitmap: true,
            write_midx: false,
            keep: vec![],
        })
        .await
        .unwrap();
    assert_eq!(result.new_packs.len(), 1);
    assert_eq!(result.removed.len(), 2);
    assert_eq!(repo.packs().unwrap().len(), 1);
    let side_oid = gix_hash::ObjectId::from_hex(side.as_bytes()).unwrap();
    assert!(
        repo.has_object(&side_oid),
        "the full repack dropped an object of a pack it supersedes"
    );
    assert!(result.new_packs[0].has_bitmap);
}
