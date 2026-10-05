fn main() -> Result<(), Box<dyn std::error::Error>> {
    const PROTOS: [&str; 2] = ["proto/floe/v1/wal.proto", "proto/floe/v1/codeintel.proto"];
    for p in PROTOS {
        println!("cargo:rerun-if-changed={p}");
    }
    let mut cfg = prost_build::Config::new();
    cfg.bytes(["."]);
    // Maps in code-intel objects encode in key order: a shard's META is part of a
    // content-addressed artifact, so the same inputs must give the same bytes.
    cfg.btree_map([
        ".floe.v1.IndexHead.refs",
        ".floe.v1.DirHead.generations",
        ".floe.v1.ShardMeta.extractors",
    ]);
    cfg.compile_protos(&PROTOS, &["proto"])?;
    Ok(())
}
