//! `floe-server --config floe.toml` — the standalone server (D39): exactly `floe serve`,
//! under the name a single-binary deployment expects.
fn main() -> anyhow::Result<()> {
    floe_cli::main_server()
}
