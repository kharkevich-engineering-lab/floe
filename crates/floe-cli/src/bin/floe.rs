//! `floe` — the full CLI (serve | compact | bundle | repo | wal | synth | import | mirror | github | config).
fn main() -> anyhow::Result<()> {
    floe_cli::main()
}
