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
            let ignored = cfg.apply_env_report(vars.clone().into_iter())?;
            cfg.validate()?;
            mirror_and_catalog(&cfg, &vars)?;
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
/// vars they name are set (never their values) in `file_vars` (the
/// `--env-file`s) or this process's environment. A catalog this binary cannot
/// write is an error here, as it is at startup.
fn mirror_and_catalog(cfg: &Config, file_vars: &[(String, String)]) -> Result<()> {
    let gm = &cfg.github_mirror;
    if gm.enabled {
        print_section("github_mirror", gm)?;
        println!("# {}", env_line(&gm.token_env, file_vars));
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
            println!("# {}", env_line(var, file_vars));
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

fn env_line(var: &str, file_vars: &[(String, String)]) -> String {
    let in_files = file_vars
        .iter()
        .rev()
        .find(|(k, _)| k == var)
        .map(|(_, v)| !v.trim().is_empty());
    let set = in_files.unwrap_or_else(|| std::env::var(var).is_ok_and(|v| !v.trim().is_empty()));
    format!("{var}: {}", if set { "set" } else { "UNSET" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An env file's value counts (the last one wins, as in `apply_env`); a
    /// variable in neither the files nor the process is UNSET.
    #[test]
    fn env_line_reads_env_files() {
        let vars = vec![
            ("FLOE_TEST_CFG_TOKEN_A".to_string(), "ghp_x".to_string()),
            ("FLOE_TEST_CFG_TOKEN_B".to_string(), "ghp_y".to_string()),
            ("FLOE_TEST_CFG_TOKEN_B".to_string(), " ".to_string()),
        ];
        assert_eq!(
            env_line("FLOE_TEST_CFG_TOKEN_A", &vars),
            "FLOE_TEST_CFG_TOKEN_A: set"
        );
        assert_eq!(
            env_line("FLOE_TEST_CFG_TOKEN_B", &vars),
            "FLOE_TEST_CFG_TOKEN_B: UNSET"
        );
        assert_eq!(
            env_line("FLOE_TEST_CFG_TOKEN_C", &vars),
            "FLOE_TEST_CFG_TOKEN_C: UNSET"
        );
    }

    /// Without the feature, an enabled catalog is an error, as at startup.
    #[cfg(not(feature = "catalog"))]
    #[test]
    fn featureless_build_rejects_an_enabled_catalog() {
        let mut cfg = Config::default();
        cfg.catalog.enabled = true;
        let err = mirror_and_catalog(&cfg, &[]).unwrap_err();
        assert!(err.to_string().contains("catalog feature"), "{err}");
    }
}
