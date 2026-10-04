//! `floe config check|dump` — validate or print the effective configuration.

use std::sync::Arc;

use anyhow::Result;

use crate::ConfigAction;
use floe_config::Config;

pub async fn run(action: ConfigAction, cfg: &Arc<Config>) -> Result<()> {
    match action {
        ConfigAction::Check { env_files, strict } => {
            let mut cfg: Config = (**cfg).clone();
            let mut vars: Vec<(String, String)> = Vec::new();
            for f in &env_files {
                let text = std::fs::read_to_string(f)
                    .map_err(|e| anyhow::anyhow!("reading {}: {e}", f.display()))?;
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        vars.push((k.trim().to_string(), v.trim().trim_matches('"').to_string()));
                    }
                }
            }
            let ignored = cfg.apply_env_report(vars.into_iter())?;
            cfg.validate()?;
            mirror_and_catalog(&cfg)?;
            for (k, why) in &ignored {
                eprintln!("ignored {k}: {why}");
            }
            if ignored.is_empty() {
                println!("config OK");
            } else {
                println!(
                    "config OK ({} override(s) ignored — unknown in this build)",
                    ignored.len()
                );
                if strict {
                    std::process::exit(3);
                }
            }
            Ok(())
        }
        ConfigAction::Dump => {
            let toml = toml::to_string_pretty(&**cfg)
                .map_err(|e| anyhow::anyhow!("serializing config: {e}"))?;
            println!("{toml}");
            Ok(())
        }
    }
}

/// `[github_mirror]` and `[catalog]` as they take effect, and whether the env
/// vars they name are set (never their values). A catalog this binary cannot
/// write is an error here, as it is at startup.
fn mirror_and_catalog(cfg: &Config) -> Result<()> {
    let gm = &cfg.github_mirror;
    if gm.enabled {
        print_section("github_mirror", gm)?;
        println!("# {}", env_line(&gm.token_env));
    }
    let cat = &cfg.catalog;
    if cat.enabled {
        if !cfg!(feature = "catalog") {
            anyhow::bail!(
                "catalog.enabled = true, but this binary was built without the catalog feature \
                 (cargo build --release -p floe-cli --features catalog)"
            );
        }
        print_section("catalog", cat)?;
        let vars = [
            cat.token_env.as_deref(),
            cat.credential_env.as_deref(),
            Some(cat.s3_access_key_env.as_str()),
            Some(cat.s3_secret_key_env.as_str()),
        ];
        for var in vars.into_iter().flatten() {
            println!("# {}", env_line(var));
        }
    }
    Ok(())
}

fn print_section<T: serde::Serialize>(name: &str, section: &T) -> Result<()> {
    let body = toml::to_string_pretty(section)
        .map_err(|e| anyhow::anyhow!("serializing [{name}]: {e}"))?;
    println!("[{name}]\n{body}");
    Ok(())
}

fn env_line(var: &str) -> String {
    let set = std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
    format!("{var}: {}", if set { "set" } else { "UNSET" })
}
