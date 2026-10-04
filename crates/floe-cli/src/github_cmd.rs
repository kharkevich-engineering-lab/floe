//! `floe github sync|status` — the GitHub mirror from the command line (D49,
//! `docs/design/github-mirror.md` §B.12). `sync` takes the same bucket lease as
//! a server's mirror loop, so the two never reconcile together. Follow is not
//! run here (no nudge): the maintaining host picks new repositories up within
//! one `upstream.follow_interval`. Passes write the same catalog telemetry as
//! the server's loop (`sync_runs`/`repo_inventory`, D50) when `[catalog]` is
//! enabled; `--dry-run` writes none.

use std::sync::Arc;

use anyhow::Result;

use floe_config::Config;
use floe_mirror::{Mirror, PassOptions, PassReport, Status};
use floe_store::open_store;
use floe_wal::Registry;

use crate::GithubAction;
use crate::cli::println_kv;

pub async fn run(action: GithubAction, cfg: &Arc<Config>) -> Result<()> {
    let store = open_store(cfg).await?;
    match action {
        GithubAction::Status { json } => status(&store, json).await,
        GithubAction::Sync { once, dry_run } => {
            let gm = &cfg.github_mirror;
            // `validate` checks the section only where the mirror is enabled
            // (a maintain host); a bad prefix or glob must not run a pass here.
            gm.check(cfg)?;
            // The token check comes before the lease.
            if std::env::var(&gm.token_env).map_or(true, |t| t.trim().is_empty()) {
                eprintln!(
                    "floe: the GitHub token variable {} is unset or empty",
                    gm.token_env
                );
                std::process::exit(2);
            }
            std::fs::create_dir_all(&cfg.cache.dir).ok();
            let registry = Registry::new(store.clone(), cfg.clone());
            let source = Arc::new(floe_mirror::github::GithubSource::new(gm)?);
            let target =
                floe_mirror::WalTarget::new(registry, store.clone(), Box::new(|_: &floe_git::RepoId| {}));
            let mirror = Arc::new(Mirror {
                cfg: cfg.clone(),
                source,
                target: Arc::new(target),
                store: store.clone(),
            });
            if dry_run {
                let report =
                    floe_mirror::reconcile_once(&mirror, PassOptions { dry_run: true }, None)
                        .await?;
                print_report(&report);
                return Ok(());
            }
            let telemetry = Arc::new(floe_server::mirror::PassTelemetry::start(&cfg.catalog)?);
            if once {
                match floe_mirror::run_once_leased(&mirror).await? {
                    Some(report) => {
                        print_report(&report);
                        telemetry.record(&report);
                        telemetry.flush(cfg.server.drain_timeout).await;
                    }
                    None => {
                        let held = floe_mirror::lease_holder(&store, "github").await;
                        match held {
                            Some((holder, until)) => eprintln!(
                                "floe: the mirror lease is held by {holder} until {}",
                                humantime::format_rfc3339_seconds(until)
                            ),
                            None => eprintln!("floe: the mirror lease is held elsewhere"),
                        }
                        std::process::exit(3);
                    }
                }
                return Ok(());
            }
            let hook = telemetry.clone();
            floe_mirror::run_loop(
                mirror,
                Box::new(move |r: &PassReport| {
                    print_report(r);
                    hook.record(r);
                }),
            )
            .await;
            telemetry.flush(cfg.server.drain_timeout).await;
            Ok(())
        }
    }
}

fn print_report(r: &PassReport) {
    for line in &r.plan {
        println!("{line}");
    }
    println_kv("pass", r.summary());
    if let Some(e) = &r.error {
        println_kv("error", e);
    }
    if let Some(until) = r.api.rate_limited_until {
        println_kv("rate_limited_until", humantime::format_rfc3339_seconds(until));
    }
    if let Some(rem) = r.api.rate_remaining {
        println_kv("rate_remaining", rem);
    }
}

async fn status(store: &floe_store::DynStore, json: bool) -> Result<()> {
    let st = floe_mirror::state::load(store.as_ref(), "github").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(());
    }
    for s in Status::ALL {
        let n = st.repos.values().filter(|e| e.status == s).count();
        if n > 0 {
            println_kv(s.as_str(), n);
        }
    }
    if let Some(p) = &st.last_pass {
        println_kv(
            "last_pass",
            format!(
                "{} complete={} created={} updated={} errors={}",
                p.finished_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                p.complete,
                p.created,
                p.updated,
                p.errors
            ),
        );
    }
    if let Some(l) = &st.token_login {
        println_kv("token_login", l);
    }
    for (id, e) in &st.repos {
        if e.status != Status::Active {
            println!(
                "{:<10} {:<40} {} {}",
                e.status.as_str(),
                e.full_name,
                e.floe.as_deref().unwrap_or("-"),
                e.last_error.as_deref().map_or_else(|| format!("(id {id})"), str::to_string)
            );
        }
    }
    Ok(())
}
