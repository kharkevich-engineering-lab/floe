//! `floe config check|dump` — validate or print the bootstrap configuration;
//! `floe config show|set|validate|history|rollback|import` — the versioned
//! runtime config document in the config store (D60, `docs/design/admin-ui.md` §9).

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::ConfigAction;
use floe_config::{Config, RuntimeConfig};
use floe_server::config_store::{ConfigStore, PublishError, PublishRequest, redact};

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
            floe_server::check_build(&cfg)?;
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
            let mut doc: toml::Table = toml::Table::try_from(&**cfg)
                .map_err(|e| anyhow::anyhow!("serializing config: {e}"))?;
            for section in floe_config::runtime::RUNTIME_SECTIONS {
                doc.remove(*section);
            }
            let toml = toml::to_string_pretty(&doc)
                .map_err(|e| anyhow::anyhow!("serializing config: {e}"))?;
            println!("{toml}");
            println!("# runtime sections live in the config store: `floe config show`");
            Ok(())
        }
        ConfigAction::Show { revision, json } => {
            let cs = open(cfg).await?;
            let record = match revision {
                Some(n) => Some(cs.revision(n).await.map_err(publish_err)?),
                None => cs.current().await?.map(|(_, r)| r),
            };
            let Some(record) = record else {
                println!(
                    "# no config document yet (revision 0): the built-in runtime defaults apply"
                );
                print_document(&RuntimeConfig::default().to_json()?, json)?;
                return Ok(());
            };
            println!(
                "# revision {} by {} at {}{}{}",
                record.revision,
                record.author,
                record.updated_at.to_rfc3339(),
                if record.message.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", record.message)
                },
                record
                    .rolled_back_from
                    .map(|n| format!(" (rollback to {n})"))
                    .unwrap_or_default()
            );
            print_document(&redact(&record.document), json)
        }
        ConfigAction::Set {
            file,
            message,
            base,
        } => {
            let cs = open(cfg).await?;
            let document = read_document(&file)?;
            let published = cs
                .publish(
                    &PublishRequest {
                        document: &document,
                        author: &author(),
                        message: &message,
                        base_revision: base,
                        rolled_back_from: None,
                    },
                    cfg,
                )
                .await
                .map_err(publish_err)?;
            print_published(&published);
            Ok(())
        }
        ConfigAction::Validate { file } => {
            let cs = open(cfg).await?;
            let document = read_document(&file)?;
            let current = cs.current().await?;
            match cs.prepare(&document, current.as_ref().map(|(_, r)| r), cfg) {
                Ok(p) => {
                    println!("config document OK ({} change(s))", p.diff.len());
                    for d in &p.diff {
                        println!("  {:?} {}", d.op, d.path);
                    }
                    if !p.restart_required.is_empty() {
                        println!("restart required for: {}", p.restart_required.join(", "));
                    }
                    Ok(())
                }
                Err(errors) => {
                    for e in &errors {
                        eprintln!(
                            "{}{}",
                            e.path
                                .as_deref()
                                .map(|p| format!("{p}: "))
                                .unwrap_or_default(),
                            e.message
                        );
                    }
                    anyhow::bail!("invalid config document ({} error(s))", errors.len())
                }
            }
        }
        ConfigAction::History { n } => {
            let cs = open(cfg).await?;
            let entries = cs.history(None, n).await?;
            if entries.is_empty() {
                println!("(no revisions yet)");
            }
            for e in entries {
                println!(
                    "{:>5}  {}  {:<20}  {}{}  [{} change(s)]",
                    e.revision,
                    e.updated_at.to_rfc3339(),
                    e.author,
                    e.message,
                    e.rolled_back_from
                        .map(|n| format!(" (rollback to {n})"))
                        .unwrap_or_default(),
                    e.diff.len()
                );
            }
            Ok(())
        }
        ConfigAction::Rollback { revision, message } => {
            let cs = open(cfg).await?;
            let published = cs
                .rollback(revision, &author(), &message, None, cfg)
                .await
                .map_err(publish_err)?;
            print_published(&published);
            Ok(())
        }
        ConfigAction::Import { file, message } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let document = import_document(&text)?;
            let cs = open(cfg).await?;
            let message = if message.is_empty() {
                format!("import from {}", file.display())
            } else {
                message
            };
            let published = cs
                .publish(
                    &PublishRequest {
                        document: &document,
                        author: &author(),
                        message: &message,
                        base_revision: None,
                        rolled_back_from: None,
                    },
                    cfg,
                )
                .await
                .map_err(publish_err)?;
            print_published(&published);
            println!(
                "now delete [github_mirror], [catalog] and [events] from {} (the file refuses them)",
                file.display()
            );
            Ok(())
        }
    }
}

async fn open(cfg: &Arc<Config>) -> Result<ConfigStore> {
    let main = Box::pin(floe_store::open_store(cfg)).await?;
    Box::pin(ConfigStore::open(cfg, &main)).await
}

fn author() -> String {
    format!(
        "cli:{}",
        std::env::var("USER").unwrap_or_else(|_| "unknown".into())
    )
}

fn publish_err(e: PublishError) -> anyhow::Error {
    match e {
        PublishError::Invalid(errors) => {
            for e in &errors {
                eprintln!(
                    "{}{}",
                    e.path
                        .as_deref()
                        .map(|p| format!("{p}: "))
                        .unwrap_or_default(),
                    e.message
                );
            }
            anyhow::anyhow!(
                "invalid config document ({} error(s)); nothing was published",
                errors.len()
            )
        }
        other => anyhow::anyhow!("{other}"),
    }
}

fn print_published(p: &floe_server::config_store::Published) {
    println!("config published: revision {}", p.record.revision);
    for d in &p.record.diff {
        println!("  {:?} {}", d.op, d.path);
    }
    if !p.restart_required.is_empty() {
        println!("restart required for: {}", p.restart_required.join(", "));
    }
}

/// A whole document from a file (or `-`): JSON when it starts with `{`, else TOML.
fn read_document(file: &std::path::Path) -> Result<serde_json::Value> {
    let text = if file.as_os_str() == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        s
    } else {
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?
    };
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(&text).context("parsing the JSON document");
    }
    RuntimeConfig::from_toml(&text)?.to_json()
}

fn print_document(doc: &serde_json::Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(doc)?);
    } else {
        let rt = RuntimeConfig::from_json(doc)?;
        println!("{}", toml::to_string_pretty(&rt).context("encoding TOML")?);
    }
    Ok(())
}

/// `floe config import`: the runtime sections of a pre-D60 `floe.toml`, with
/// `github_mirror.token_env = "X"` → `token = { env = "X" }` and a literal
/// `events.webhook_secret` → a value to seal.
pub fn import_document(text: &str) -> Result<serde_json::Value> {
    let table: toml::Table = text.parse().context("parsing the TOML file")?;
    let mut doc = toml::Table::new();
    for section in floe_config::runtime::RUNTIME_SECTIONS {
        if let Some(v) = table.get(*section) {
            doc.insert((*section).to_string(), v.clone());
        }
    }
    anyhow::ensure!(
        !doc.is_empty(),
        "no [github_mirror], [catalog] or [events] section to import"
    );
    if let Some(toml::Value::Table(gm)) = doc.get_mut("github_mirror")
        && let Some(toml::Value::String(var)) = gm.remove("token_env")
    {
        let mut t = toml::Table::new();
        t.insert("env".into(), toml::Value::String(var));
        gm.insert("token".into(), toml::Value::Table(t));
    }
    if let Some(toml::Value::Table(ev)) = doc.get_mut("events")
        && let Some(toml::Value::String(secret)) = ev.remove("webhook_secret")
        && !secret.is_empty()
    {
        let mut t = toml::Table::new();
        t.insert("value".into(), toml::Value::String(secret));
        ev.insert("webhook_secret".into(), toml::Value::Table(t));
    }
    let rt: RuntimeConfig = doc.try_into().context("the imported sections")?;
    rt.to_json()
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
        if cat.auth == floe_config::CatalogAuth::Sigv4 {
            // D43/D63: neither key set = the AWS SDK credential chain signs.
            println!(
                "# sigv4: service {}, region {}",
                cat.sigv4_service,
                cat.sigv4_region()
            );
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

    /// `import` maps a pre-D60 file's runtime sections onto the document shape.
    #[test]
    fn import_maps_old_sections() {
        let doc = import_document(
            r#"
[store]
bucket = "b"
[github_mirror]
enabled = true
token_env = "GH_PAT"
orgs = ["acme"]
private_visible_to_all_readers = true
[events]
webhook_url = "https://h.example/x"
webhook_secret = "s3cret"
"#,
        )
        .unwrap();
        assert_eq!(
            doc["github_mirror"]["token"],
            serde_json::json!({"env": "GH_PAT"})
        );
        assert_eq!(doc["github_mirror"]["orgs"][0], "acme");
        assert_eq!(
            doc["events"]["webhook_secret"],
            serde_json::json!({"value": "s3cret"})
        );
        assert!(doc.get("store").is_none());
        assert!(import_document("[store]\nbucket = \"b\"\n").is_err());
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
