//! D61 — secrets in the config document (`docs/design/admin-ui.md` §5).
//!
//! A secret field is an env reference, a sealed value (AES-256-GCM under
//! `FLOE_CONFIG_KEY`, sealed and opened by `floe_server::config_store`), a
//! plain value (input only, or in memory once opened) or a redaction marker
//! (what reads return; "keep the stored value" on write).
//!
//! Consumers that historically read a token from an env var by name
//! (follow, LFS read-through, the GitHub source) resolve names through
//! [`env_var`]: an **alias** registered here when a config document is
//! applied wins over the process environment, so a token entered in the GUI
//! reaches them without a restart. The alias table is an in-memory cache of
//! the bucket and the environment (principle I), rebuilt on every apply.

use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

/// The alias every effective config uses as `github_mirror.token_env` once a
/// config document is applied.
pub const GITHUB_MIRROR_TOKEN_ALIAS: &str = "FLOE_CONFIG_GITHUB_MIRROR_TOKEN";

/// One secret field. Serialized externally tagged: `{"env": "NAME"}`,
/// `{"sealed": "v1.…"}`, `{"value": "…"}`, `{"redacted": true}` (TOML:
/// `token = { env = "NAME" }`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Secret {
    /// Read from this environment variable on every instance; nothing in the bucket.
    Env(String),
    /// `v1.<kid>.<base64>` — AES-256-GCM ciphertext (`floe_server::config_store::seal`).
    Sealed(String),
    /// Plain text: input only on the wire; in memory after the sealed form was opened.
    Value(String),
    /// "A sealed value is stored here" on read; "keep it" on write.
    Redacted(bool),
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Secret::Env(n) => write!(f, "Secret::Env({n:?})"),
            Secret::Sealed(_) => f.write_str("Secret::Sealed(..)"),
            Secret::Value(_) => f.write_str("Secret::Value(***)"),
            Secret::Redacted(_) => f.write_str("Secret::Redacted"),
        }
    }
}

impl Secret {
    /// The plain value, when this process can know it: an env reference
    /// (through [`env_var`]) or an opened value. Empty counts as unset.
    pub fn reveal(&self) -> Option<String> {
        match self {
            Secret::Env(name) => env_var(name),
            Secret::Value(v) => Some(v.clone()).filter(|v| !v.trim().is_empty()),
            Secret::Sealed(_) | Secret::Redacted(_) => None,
        }
    }

    /// Whether this is an input-only or in-memory form that must never be written as is.
    pub fn is_plain(&self) -> bool {
        matches!(self, Secret::Value(_))
    }
}

fn aliases() -> &'static RwLock<HashMap<String, Secret>> {
    static ALIASES: OnceLock<RwLock<HashMap<String, Secret>>> = OnceLock::new();
    ALIASES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register (or with `None` remove) the secret an alias name resolves to.
/// Only `Env` and `Value` resolve; anything else makes the alias unset.
pub fn set_alias(name: &str, secret: Option<Secret>) {
    let mut table = aliases().write().unwrap_or_else(PoisonError::into_inner);
    match secret {
        Some(s) => {
            table.insert(name.to_string(), s);
        }
        None => {
            table.remove(name);
        }
    }
}

/// A token by env var name: a registered alias first (one hop: an alias that
/// is an env reference reads that variable), else the process environment.
/// Empty or whitespace-only values are unset.
pub fn env_var(name: &str) -> Option<String> {
    let alias = aliases()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(name)
        .cloned();
    let value = match alias {
        Some(Secret::Env(other)) => std::env::var(other).ok(),
        Some(Secret::Value(v)) => Some(v),
        Some(Secret::Sealed(_) | Secret::Redacted(_)) => None,
        None => std::env::var(name).ok(),
    };
    value.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Holder {
        token: Secret,
    }

    #[test]
    fn serde_shapes() {
        let s: Secret = serde_json::from_str(r#"{"env":"X"}"#).unwrap();
        assert_eq!(s, Secret::Env("X".into()));
        let s: Secret = serde_json::from_str(r#"{"redacted":true}"#).unwrap();
        assert_eq!(s, Secret::Redacted(true));
        assert_eq!(
            serde_json::to_string(&Secret::Sealed("v1.a.b".into())).unwrap(),
            r#"{"sealed":"v1.a.b"}"#
        );
        let t: Holder = toml::from_str("token = { value = \"v\" }").unwrap();
        assert_eq!(t.token, Secret::Value("v".into()));
        assert!(serde_json::from_str::<Secret>(r#"{"bogus":"x"}"#).is_err());
        assert_eq!(format!("{:?}", Secret::Value("hunter2".into())), "Secret::Value(***)");
    }

    #[test]
    fn aliases_resolve_before_the_environment() {
        let name = "FLOE_TEST_SECRET_ALIAS_ONLY";
        assert_eq!(env_var(name), None);
        set_alias(name, Some(Secret::Value("v1".into())));
        assert_eq!(env_var(name).as_deref(), Some("v1"));
        set_alias(name, Some(Secret::Value("  ".into())));
        assert_eq!(env_var(name), None);
        set_alias(name, Some(Secret::Env("FLOE_TEST_SECRET_ALIAS_UNSET_TARGET".into())));
        assert_eq!(env_var(name), None);
        set_alias(name, None);
        assert_eq!(env_var(name), None);
        assert_eq!(Secret::Value("x".into()).reveal().as_deref(), Some("x"));
        assert_eq!(Secret::Sealed("x".into()).reveal(), None);
    }
}
