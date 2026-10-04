fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/floe/v1/wal.proto");
    let mut cfg = prost_build::Config::new();
    cfg.bytes(["."]);
    cfg.compile_protos(&["proto/floe/v1/wal.proto"], &["proto"])?;
    Ok(())
}
