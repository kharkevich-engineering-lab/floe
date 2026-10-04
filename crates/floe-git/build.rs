//! Bake the release version into the git `agent=` capability. The release workflow
//! passes the version semantic-release is about to tag as `FLOE_VERSION` (same logic as
//! floe-server's build.rs, which feeds `floe --version`); any other build reports the
//! crate version.

fn main() {
    println!("cargo:rerun-if-env-changed=FLOE_VERSION");
    let version = std::env::var("FLOE_VERSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=FLOE_VERSION={version}");
}
