//! `floe` — the full CLI (serve | compact | bundle | repo | wal | synth | import | mirror | config).
fn main() -> anyhow::Result<()> {
    floe_cli::main()
}
