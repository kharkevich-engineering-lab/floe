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

    /// Every env var name the document makes this host read, by path (D61):
    /// `{ env = … }` secrets and the catalog's `*_env` keys.
    pub fn env_refs(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Secret::Env(n) = &self.github_mirror.token {
            out.push(("github_mirror.token", n.clone()));
        }
        if let Some(Secret::Env(n)) = &self.events.webhook_secret {
            out.push(("events.webhook_secret", n.clone()));
        }
        let c = &self.catalog;
        for (path, v) in [("catalog.token_env", &c.token_env), ("catalog.credential_env", &c.credential_env)] {
            if let Some(n) = v.as_deref().filter(|n| !n.trim().is_empty()) {
                out.push((path, n.to_string()));
            }
        }
        // Unset/empty names are not references (only `auth = "sigv4"` needs
        // them, and `CatalogConfig::validate` says so).
        for (path, v) in [
            ("catalog.s3_access_key_env", &c.s3_access_key_env),
            ("catalog.s3_secret_key_env", &c.s3_secret_key_env),
        ] {
            if !v.trim().is_empty() {
                out.push((path, v.clone()));
            }
        }
        out
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
        // D61: env names are host facts. Checked here, so publish (400) and
        // every apply (a document written before the rule) both refuse it.
        for (path, name) in rt.env_refs() {
            anyhow::ensure!(
                self.env_ref_allowed(path, &name),
                "{path}: env var {name:?} is not allowed by this host's config_store.allowed_env (D61); use a FLOE_SECRET_* name, or list it there"
            );
        }
        let mut cfg = self.clone();
        cfg.github_mirror.clone_from(&rt.github_mirror);
        cfg.catalog.clone_from(&rt.catalog);
        cfg.events.clone_from(&rt.events);
        // Resolved live through the alias (registered by whoever applies the
        // document); unset resolves to "no token", as an unset variable did.
        cfg.github_mirror.token_env = GITHUB_MIRROR_TOKEN_ALIAS.to_string();
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
    fn env_references_follow_the_host_allowlist() {
        let host = Config::default();
        let mut rt = RuntimeConfig::default();
        host.with_runtime(&rt).unwrap();
        for bad in ["FLOE_CONFIG_KEY", "FLOE__SERVER__AUTH__SESSION_SECRET", "AWS_SECRET_ACCESS_KEY", "HOME", "FLOE_SECRETX"] {
            rt.github_mirror.token = Secret::Env(bad.into());
            let err = host.with_runtime(&rt).unwrap_err().to_string();
            assert!(err.starts_with("github_mirror.token:"), "{bad}: {err}");
        }
        rt.github_mirror.token = Secret::Env("FLOE_SECRET_GH".into());
        host.with_runtime(&rt).unwrap();
        // Bearer-style catalog keys follow the same list; AWS names only as signing keys.
        rt.catalog.token_env = Some("AWS_SECRET_ACCESS_KEY".into());
        assert!(host.with_runtime(&rt).unwrap_err().to_string().starts_with("catalog.token_env:"));
        rt.catalog.token_env = None;
        rt.catalog.s3_secret_key_env = "FLOE_CONFIG_KEY".into();
        assert!(host.with_runtime(&rt).is_err());
        rt.catalog.s3_secret_key_env = "AWS_SECRET_ACCESS_KEY".into();
        host.with_runtime(&rt).unwrap();
        // Empty names are no references: fine unless the catalog signs (validate's rule).
        rt.catalog.s3_access_key_env = String::new();
        rt.catalog.s3_secret_key_env = "  ".into();
        assert!(rt.env_refs().iter().all(|(p, _)| !p.starts_with("catalog.s3_")));
        host.with_runtime(&rt).unwrap();
        rt.catalog.s3_access_key_env = "AWS_ACCESS_KEY_ID".into();
        rt.catalog.s3_secret_key_env = "AWS_SECRET_ACCESS_KEY".into();
        // Only an exact host entry opens a host-only name.
        let mut open = Config::default();
        open.config_store.allowed_env = vec!["AWS_*".into(), "MY_TOKEN".into(), "FLOE_SECRET_*".into()];
        rt.events.webhook_secret = Some(Secret::Env("AWS_SESSION_TOKEN".into()));
        assert!(open.with_runtime(&rt).is_err(), "a glob never reaches AWS_*");
        rt.events.webhook_secret = Some(Secret::Env("MY_TOKEN".into()));
        open.with_runtime(&rt).unwrap();
    }

    #[test]
    fn service_urls_are_https_or_loopback() {
        for ok in ["https://api.github.com", "https://ghe.example.com/api/v3", "http://127.0.0.1:8080", "http://localhost"] {
            crate::check_service_url("k", ok).unwrap();
        }
        for bad in [
            "http://169.254.169.254/latest",
            "http://localhost.evil.example",
            "https://x@evil.example",
            "https://api.github.com#frag",
            "https://api.github.com?x=1",
            "https://api.github.com/",
            "ftp://x",
            "https://",
        ] {
            assert!(crate::check_service_url("k", bad).is_err(), "{bad}");
        }
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
