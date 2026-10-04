//! D60 — the runtime half of the configuration (`docs/design/admin-ui.md` §1):
//! the sections that live in the versioned config document in the bucket, not
//! in `floe.toml`. The file refuses them ([`Config::parse`]) and `FLOE__`
//! overrides of them are ignored; the document cannot carry a bootstrap key
//! (`deny_unknown_fields`). One home per key.
//!
//! [`Config`] keeps fields of these types as the **effective** values
//! ([`Config::with_runtime`]), so every consumer reads them as before.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::secret::{GITHUB_MIRROR_TOKEN_ALIAS, Secret};
use crate::{CatalogConfig, Config, EventsConfig, GithubMirrorConfig};

/// The top-level sections of the config document.
pub const RUNTIME_SECTIONS: &[&str] = &["github_mirror", "catalog", "events"];

/// Dotted paths of the document's secret fields (D61).
pub const SECRET_PATHS: &[&str] = &["github_mirror.token", "events.webhook_secret"];

/// Paths (a section or one key) whose change needs a process restart: the
/// events bridge and the catalog writer are built once per process, and follow
/// scopes the mirror token to `git_url` from the registry's config.
pub const RESTART_ONLY: &[&str] = &["catalog", "events", "github_mirror.git_url"];

/// The config document (`config/current.json` → `document`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RuntimeConfig {
    /// D49: the GitHub mirror.
    pub github_mirror: GithubMirrorConfig,
    /// D50: Iceberg audit tables.
    pub catalog: CatalogConfig,
    /// D32: the events bridge's webhook.
    pub events: EventsConfig,
}

impl RuntimeConfig {
    /// The document's own checks (fleet-wide: no host-role rule). Secrets must
    /// be in a resolvable form; the server checks they open before this.
    pub fn validate(&self) -> Result<()> {
        if self.github_mirror.enabled {
            self.github_mirror.check()?;
        }
        self.catalog.validate()?;
        self.events.check()?;
        Ok(())
    }

    /// Parse a document from JSON (the store's and the API's form).
    pub fn from_json(v: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(v.clone()).context("config document")
    }

    /// Parse a document from TOML (`floe config set file.toml`).
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("config document (TOML)")
    }

    /// The document as JSON.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).context("encoding the config document")
    }

    /// The secret at a [`SECRET_PATHS`] path.
    pub fn secret(&self, path: &str) -> Option<&Secret> {
        match path {
            "github_mirror.token" => Some(&self.github_mirror.token),
            "events.webhook_secret" => self.events.webhook_secret.as_ref(),
            _ => None,
        }
    }

    /// Mutable access to the secret at a [`SECRET_PATHS`] path (`None` when
    /// the field is optional and unset).
    pub fn secret_mut(&mut self, path: &str) -> Option<&mut Secret> {
        match path {
            "github_mirror.token" => Some(&mut self.github_mirror.token),
            "events.webhook_secret" => self.events.webhook_secret.as_mut(),
            _ => None,
        }
    }

    /// The runtime sections as a bootstrap `Config` carries them (the old
    /// file's sections, for `floe config import`).
    pub fn from_config(cfg: &Config) -> Self {
        RuntimeConfig {
            github_mirror: cfg.github_mirror.clone(),
            catalog: cfg.catalog.clone(),
            events: cfg.events.clone(),
        }
    }
}

impl Config {
    /// D60: the effective config = this bootstrap with the runtime sections
    /// taken from `rt` (whose secrets must already be opened: `Env`/`Value`),
    /// validated. The mirror's token is read through
    /// [`GITHUB_MIRROR_TOKEN_ALIAS`]; whoever applies the document registers
    /// that alias (`floe_config::secret::set_alias`).
    pub fn with_runtime(&self, rt: &RuntimeConfig) -> Result<Config> {
        let mut cfg = self.clone();
        cfg.github_mirror = rt.github_mirror.clone();
        cfg.catalog = rt.catalog.clone();
        cfg.events = rt.events.clone();
        cfg.github_mirror.token_env = GITHUB_MIRROR_TOKEN_ALIAS.to_string();
        // The alias is always registered on an instance that applied a
        // document; unset resolves to "no token", as an unset variable did.
        cfg.github_mirror.use_token = true;
        cfg.validate()
            .context("the effective config (bootstrap ⊕ config document)")?;
        Ok(cfg)
    }
}

/// Flatten a JSON value into `(dotted path, leaf)` pairs; arrays are leaves.
pub fn flatten(v: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, serde_json::Value)>) {
        match v.as_object() {
            Some(map) if !is_secret_object(v) => {
                for (k, child) in map {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&p, child, out);
                }
            }
            _ => out.push((prefix.to_string(), v.clone())),
        }
    }
    let mut out = Vec::new();
    walk("", v, &mut out);
    out
}

/// A one-key object whose key is a [`Secret`] variant: a leaf, not a table.
fn is_secret_object(v: &serde_json::Value) -> bool {
    v.as_object().is_some_and(|m| {
        m.len() == 1
            && m.keys()
                .all(|k| matches!(k.as_str(), "env" | "sealed" | "value" | "redacted"))
    })
}

/// Restart-only paths ([`RESTART_ONLY`]) whose value differs between the
/// document a process started with and `now`.
pub fn restart_required(started: &RuntimeConfig, now: &RuntimeConfig) -> Vec<String> {
    let (Ok(a), Ok(b)) = (serde_json::to_value(started), serde_json::to_value(now)) else {
        return Vec::new();
    };
    let a = flatten(&a);
    let b: std::collections::BTreeMap<String, serde_json::Value> = flatten(&b).into_iter().collect();
    let mut out: Vec<String> = a
        .into_iter()
        .filter(|(p, v)| {
            RESTART_ONLY
                .iter()
                .any(|r| p == r || p.starts_with(&format!("{r}.")))
                && b.get(p) != Some(v)
        })
        .map(|(p, _)| p)
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_round_trips_and_refuses_bootstrap_keys() {
        let rt = RuntimeConfig::default();
        let v = rt.to_json().unwrap();
        assert_eq!(v["github_mirror"]["token"]["env"], "FLOE_GITHUB_TOKEN");
        assert!(v["github_mirror"].get("token_env").is_none(), "token_env is derived, never stored");
        let back = RuntimeConfig::from_json(&v).unwrap();
        assert_eq!(back.to_json().unwrap(), v);
        let err = RuntimeConfig::from_json(&serde_json::json!({"server": {"listen": "0.0.0.0:1"}})).unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
        let t = RuntimeConfig::from_toml("[events]\nwebhook_url = \"https://h.example/x\"\nwebhook_secret = { env = \"S\" }\n").unwrap();
        assert_eq!(t.events.webhook_secret, Some(Secret::Env("S".into())));
        t.validate().unwrap();
    }

    #[test]
    fn validate_is_fleet_wide() {
        let mut rt = RuntimeConfig::default();
        rt.github_mirror.enabled = true;
        rt.github_mirror.users = vec!["@me".into()];
        rt.github_mirror.private_visible_to_all_readers = true;
        rt.validate().unwrap();
        // A serve-only host applies it too: the mirror just does not run there.
        let mut host = Config::default();
        host.server.roles = vec![crate::Role::Serve];
        let eff = host.with_runtime(&rt).unwrap();
        assert_eq!(eff.github_mirror.token_env, GITHUB_MIRROR_TOKEN_ALIAS);
        rt.github_mirror.include = vec!["nope".into()];
        assert!(rt.validate().is_err());
        let mut rt = RuntimeConfig::default();
        rt.events.webhook_url = Some("ftp://x".into());
        assert!(rt.validate().unwrap_err().to_string().contains("webhook_url"));
    }

    #[test]
    fn restart_required_names_restart_only_paths() {
        let a = RuntimeConfig::default();
        let mut b = a.clone();
        b.github_mirror.interval = std::time::Duration::from_secs(1);
        assert!(restart_required(&a, &b).is_empty(), "the mirror is live");
        b.github_mirror.git_url = "https://ghe.example".into();
        b.catalog.namespace = "other".into();
        b.events.webhook_secret = Some(Secret::Env("X".into()));
        let r = restart_required(&a, &b);
        assert!(r.contains(&"github_mirror.git_url".to_string()), "{r:?}");
        assert!(r.contains(&"catalog.namespace".to_string()), "{r:?}");
        assert!(r.contains(&"events.webhook_secret".to_string()), "{r:?}");
    }

    #[test]
    fn flatten_keeps_secrets_as_leaves() {
        let v = serde_json::json!({"a": {"b": 1, "t": {"env": "X"}, "l": [1, 2]}});
        let f = flatten(&v);
        assert!(f.contains(&("a.t".to_string(), serde_json::json!({"env": "X"}))));
        assert!(f.contains(&("a.l".to_string(), serde_json::json!([1, 2]))));
    }
}
