//! `[codeintel]`, `[codeintel.embed]`, `[codeintel.catalog]`, `[mcp]` and `[access]`
//! (`docs/design/code-intelligence.md` §11, D52–D58).
//!
//! `[codeintel]` and `[mcp]` are runtime sections (D60): they live in the versioned config
//! document (`RuntimeConfig`), never in `floe.toml`; the MCP HMAC key is the bootstrap
//! `server.auth.mcp_handle_secret`. `[access]` is a host default in the file, overridable per
//! repository (D24) like `[bundles]`.
//!
//! The sections are always parsed, whatever the binary was built with, so a config never
//! depends on cargo features. What the binary can *do* is checked separately by
//! [`Config::validate_build`] against the [`BuildFeatures`] the server reports: a key that needs
//! a feature this build lacks is a fatal error naming the build flag.

use std::time::Duration;

use anyhow::Result;
use bytesize::ByteSize;
use serde::{Deserialize, Serialize};

use crate::{AuthMode, Config};

/// `[codeintel]`: the host switch and the indexer/serving knobs. Six keys may also be set per
/// repository ([`CODEINTEL_REPO_KEYS`], D24 extension); everything else is host-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a config section: each bool is an independent documented key"
)]
pub struct CodeIntelConfig {
    /// This host does code intelligence (indexes what it maintains, serves what it serves).
    /// Needs a binary built with `--features codeintel`.
    pub enabled: bool,
    /// Whether a repository is indexed when its settings do not say. Per repository, the
    /// settings key `[codeintel] enabled` overrides this (see [`Config::codeintel_repo_enabled`]).
    pub default_enabled: bool,
    /// Ref patterns indexed per repository (tips only). `refs/archive/*` and `refs/follow/*`
    /// are never indexed.
    pub refs: Vec<String>,
    /// At most this many tracked refs per repository.
    pub max_refs: usize,
    /// Embed this repository's chunks (needs `[codeintel.embed] provider` other than `none`).
    pub semantic: bool,
    /// Allow a remote (`http`) embedder for this repository's chunks (§6.5 privacy gate).
    pub embed_remote: bool,
    /// Files larger than this are not indexed (grep/read only through git).
    pub max_file_bytes: ByteSize,
    /// Globs (whole repository path) never indexed, on top of `.gitattributes` linguist rules.
    pub exclude: Vec<String>,
    /// Monorepo base shards are cut into path-range parts of at most this much content.
    pub part_max_bytes: ByteSize,
    /// A delta larger than this many files forces a new base.
    pub delta_max_files: u32,
    /// A delta larger than this fraction of the base forces a new base.
    pub delta_max_ratio: f64,
    /// Local shard cache budget, counted inside `cache.max_bytes`.
    pub cache_bytes: ByteSize,
    /// Cold artifacts larger than this answer `warming` and hydrate in the background.
    pub sync_fetch_max_bytes: ByteSize,
    /// Query pool threads; 0 = half the CPUs, at least 2.
    pub query_threads: usize,
    /// A cached `head.pb` older than this is revalidated (conditional GET) before use.
    #[serde(with = "humantime_serde")]
    pub head_ttl: Duration,
    /// GC keeps retired generations (and their artifacts) this long. Must be at least
    /// `mcp.snapshot_ttl + 1h` so no handle outlives what it names.
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
    /// Retired generations reachable by `rev = <full sha>` (`head.pb.recent`).
    pub recent_max: u32,
    /// Backoff cap of the catalog phase while the catalog is down.
    #[serde(with = "humantime_serde")]
    pub catalog_retry_max: Duration,
    /// A queued reindex never admitted by a maintainer pass is failed after this.
    #[serde(with = "humantime_serde")]
    pub reindex_queue_timeout: Duration,
    /// `true` = facts and embeddings go to the Iceberg `code.*` tables. `false` = standalone:
    /// shards from git only, embeddings not durable.
    pub require_catalog: bool,
    /// Index mirrors of private upstreams before per-repo `[access]` rules exist for them (D54).
    pub index_private_mirrors: bool,
    /// Memory cap of the maintainer's known-blobs set before it is loaded per bucket.
    pub known_blobs_max_bytes: ByteSize,
    /// Repo patterns whose artifacts are fetched at startup.
    pub prewarm: Vec<String>,
    /// The ad-hoc overlay for an unindexed commit covers at most this many changed files.
    pub adhoc_max_files: u32,
    pub embed: EmbedConfig,
    pub catalog: CodeIntelCatalogConfig,
}

impl Default for CodeIntelConfig {
    fn default() -> Self {
        CodeIntelConfig {
            enabled: false,
            default_enabled: true,
            refs: vec!["HEAD".into()],
            max_refs: 8,
            semantic: true,
            embed_remote: false,
            max_file_bytes: ByteSize::mib(1),
            exclude: [
                "**/vendor/**",
                "**/node_modules/**",
                "**/*.min.js",
                "**/*_pb2.py",
                "**/*.pb.go",
            ]
            .map(String::from)
            .to_vec(),
            part_max_bytes: ByteSize::mib(512),
            delta_max_files: 5000,
            delta_max_ratio: 0.10,
            cache_bytes: ByteSize::gib(4),
            sync_fetch_max_bytes: ByteSize::mib(64),
            query_threads: 0,
            head_ttl: Duration::from_secs(1),
            retention: Duration::from_hours(7 * 24),
            recent_max: 64,
            catalog_retry_max: Duration::from_mins(5),
            reindex_queue_timeout: Duration::from_hours(1),
            require_catalog: true,
            index_private_mirrors: false,
            known_blobs_max_bytes: ByteSize::mib(256),
            prewarm: Vec::new(),
            adhoc_max_files: 200,
            embed: EmbedConfig::default(),
            catalog: CodeIntelCatalogConfig::default(),
        }
    }
}

/// D24 extension: the `[codeintel]` keys a repository's settings may set. Per repository,
/// `enabled` is the repository's own switch (stored as the effective `default_enabled`);
/// the host's `codeintel.enabled` is never overridden by a repository.
pub const CODEINTEL_REPO_KEYS: &[&str] = &[
    "enabled",
    "refs",
    "semantic",
    "embed_remote",
    "max_file_bytes",
    "exclude",
];

/// Embedding provider (`[codeintel.embed] provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EmbedProvider {
    /// No embeddings: `semantic_search` says so; `search` runs lexical and symbol channels.
    #[default]
    None,
    /// In-process model (`--features embed-local`), weights pinned in the bucket.
    Local,
    /// An `OpenAI`-compatible `/v1/embeddings` endpoint (`--features embed-http`).
    Http,
}

/// `[codeintel.embed]` (§6.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbedConfig {
    pub provider: EmbedProvider,
    pub model: String,
    /// `codeintel/models/<sha256>/` holding the pinned weights (required for `local`).
    pub model_object: String,
    /// `OpenAI`-compatible endpoint (required for `http`).
    pub http_url: String,
    pub http_model: String,
    /// Name of the env var holding the API key (D43 style), never the key itself.
    pub api_key_env: String,
    pub batch: u32,
    pub max_rps: u32,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        EmbedConfig {
            provider: EmbedProvider::None,
            model: "jina-v2-base-code".into(),
            model_object: String::new(),
            http_url: String::new(),
            http_model: String::new(),
            api_key_env: String::new(),
            batch: 32,
            max_rps: 20,
        }
    }
}

/// `[codeintel.catalog]`: where the `code.*` tables live. The connection (uri, `SigV4`
/// credentials) is shared with the Iceberg catalog of the github-mirror design.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CodeIntelCatalogConfig {
    pub table_bucket: String,
    pub namespace: String,
    #[serde(with = "humantime_serde")]
    pub flush_interval: Duration,
    pub flush_rows: u64,
}

impl Default for CodeIntelCatalogConfig {
    fn default() -> Self {
        CodeIntelCatalogConfig {
            table_bucket: "floe-code".into(),
            namespace: "code".into(),
            flush_interval: Duration::from_secs(30),
            flush_rows: 50_000,
        }
    }
}

/// `mcp.query_log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QueryLog {
    Off,
    #[default]
    Sampled,
    All,
}

/// `[mcp]` (§7, §11, D55, D57). The endpoints are fixed: `/api/v1/mcp` and `/{owner}/{repo}/mcp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpConfig {
    /// Serve the MCP endpoints. Needs `--features mcp` and `codeintel.enabled`.
    pub enabled: bool,
    /// Browser origins allowed to call the endpoints. Empty = `[server.public_url]`.
    pub allowed_origins: Vec<String>,
    /// Accepted `Host` values. Empty = the host of `server.public_url`.
    pub allowed_hosts: Vec<String>,
    /// Accepted `aud` of `IdP` access tokens. Empty = `["<public_url>/api/v1/mcp"]`.
    pub audiences: Vec<String>,
    pub scopes: Vec<String>,
    /// Lifetime of a snapshot handle.
    #[serde(with = "humantime_serde")]
    pub snapshot_ttl: Duration,
    /// `pin` refuses a retired generation with less lifetime left than this.
    #[serde(with = "humantime_serde")]
    pub min_handle_ttl: Duration,
    pub semantic_deadline_ms: u32,
    pub max_concurrent_per_principal: u32,
    pub query_log: QueryLog,
    pub query_log_text: bool,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
            audiences: Vec::new(),
            scopes: vec!["floe.code.read".into(), "floe.code.admin".into()],
            snapshot_ttl: Duration::from_hours(24),
            min_handle_ttl: Duration::from_mins(10),
            semantic_deadline_ms: 40,
            max_concurrent_per_principal: 8,
            query_log: QueryLog::Sampled,
            query_log_text: false,
        }
    }
}

/// `[access]` (D54): who may read a repository. Host default, overridable per repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AccessConfig {
    /// Entries: `authenticated`, `public`, `group:<name>`, `domain:<domain>`,
    /// `email:<address>`, or a principal name.
    pub read: Vec<String>,
}

/// The `[access] read` rule that reproduces today's behaviour (every authenticated principal).
pub const ACCESS_READ_DEFAULT: &str = "authenticated";

impl Default for AccessConfig {
    fn default() -> Self {
        AccessConfig {
            read: vec![ACCESS_READ_DEFAULT.into()],
        }
    }
}

/// What this binary was built with. The server reports it (`cfg!(feature = …)`); the config
/// crate stays feature-free so every binary parses every key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per cargo feature, mirrored as-is"
)]
pub struct BuildFeatures {
    pub codeintel: bool,
    pub mcp: bool,
    pub embed_local: bool,
    pub embed_http: bool,
}

/// The minimum length of an HMAC key (`server.auth.mcp_handle_secret`, `session_secret`).
const MIN_SECRET_BYTES: usize = 32;

fn is_env_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// One `[access] read` entry, checked for syntax.
fn validate_access_entry(e: &str) -> Result<()> {
    anyhow::ensure!(
        !e.is_empty() && e.trim() == e && !e.chars().any(char::is_whitespace),
        "access.read entries must be non-empty and contain no whitespace (got {e:?})"
    );
    if let Some((kind, value)) = e.split_once(':') {
        anyhow::ensure!(
            matches!(kind, "group" | "domain" | "email") && !value.is_empty(),
            "access.read entry {e:?}: the prefixed forms are group:<name>, domain:<domain> and email:<address>"
        );
        if kind == "email" {
            anyhow::ensure!(
                value.contains('@'),
                "access.read entry {e:?} is not an email address"
            );
        }
    }
    Ok(())
}

/// The rules of the `[codeintel]` and `[mcp]` sections on their own (§11): what the config
/// document must satisfy on every host (`RuntimeConfig::validate`). Host-coupled rules
/// (auth, listener, cache budget, the catalog) are `Config::validate_codeintel`'s.
pub(crate) fn check_document(ci: &CodeIntelConfig, mcp: &McpConfig) -> Result<()> {
    // §3.4: a live record must outlive every handle that can name it by ≥ 1 h.
    let min_retention = mcp.snapshot_ttl.saturating_add(Duration::from_hours(1));
    anyhow::ensure!(
        ci.retention >= min_retention,
        "codeintel.retention ({}) must be at least mcp.snapshot_ttl + 1h ({}): GC would delete generations a snapshot handle can still name",
        humantime::format_duration(ci.retention),
        humantime::format_duration(min_retention)
    );
    anyhow::ensure!(ci.max_refs >= 1, "codeintel.max_refs must be at least 1");
    for r in &ci.refs {
        anyhow::ensure!(
            r == "HEAD" || (r.starts_with("refs/") && !r.ends_with('/')),
            "codeintel.refs entries are HEAD or ref patterns under refs/ (got {r:?})"
        );
        anyhow::ensure!(
            !r.starts_with("refs/archive/") && !r.starts_with("refs/follow/"),
            "codeintel.refs: {r:?} is never indexed (refs/archive/* and refs/follow/* are excluded)"
        );
    }
    for g in ci.exclude.iter().chain(&ci.prewarm) {
        anyhow::ensure!(
            !g.trim().is_empty(),
            "codeintel.exclude / codeintel.prewarm entries must not be empty"
        );
    }
    anyhow::ensure!(
        ci.max_file_bytes.as_u64() > 0 && ci.max_file_bytes <= ci.part_max_bytes,
        "codeintel.max_file_bytes must be > 0 and <= codeintel.part_max_bytes"
    );
    anyhow::ensure!(
        ci.delta_max_ratio > 0.0 && ci.delta_max_ratio <= 1.0,
        "codeintel.delta_max_ratio must be in (0, 1] (got {})",
        ci.delta_max_ratio
    );
    anyhow::ensure!(
        ci.delta_max_files >= 1 && ci.recent_max >= 1 && ci.adhoc_max_files >= 1,
        "codeintel.delta_max_files, recent_max and adhoc_max_files must be at least 1"
    );
    anyhow::ensure!(
        ci.catalog.flush_rows > 0 && !ci.catalog.flush_interval.is_zero(),
        "codeintel.catalog.flush_rows and flush_interval must be > 0"
    );
    anyhow::ensure!(
        !ci.catalog.table_bucket.is_empty() && !ci.catalog.namespace.is_empty(),
        "codeintel.catalog.table_bucket and namespace must not be empty"
    );
    if ci.enabled {
        anyhow::ensure!(
            !ci.refs.is_empty(),
            "codeintel.refs must list at least one ref pattern when codeintel.enabled"
        );
    }

    let e = &ci.embed;
    anyhow::ensure!(
        e.batch >= 1 && e.max_rps >= 1,
        "codeintel.embed.batch and max_rps must be at least 1"
    );
    if !e.api_key_env.is_empty() {
        anyhow::ensure!(
            is_env_var_name(&e.api_key_env),
            "codeintel.embed.api_key_env names an environment variable (A-Z, 0-9, _), never the key itself"
        );
    }
    match e.provider {
        EmbedProvider::None => {}
        EmbedProvider::Local => anyhow::ensure!(
            !e.model_object.is_empty(),
            "codeintel.embed.provider = \"local\" needs codeintel.embed.model_object (the pinned weights in the bucket; nothing is downloaded at runtime)"
        ),
        EmbedProvider::Http => {
            anyhow::ensure!(
                e.http_url.starts_with("https://") || e.http_url.starts_with("http://"),
                "codeintel.embed.provider = \"http\" needs codeintel.embed.http_url (an http(s) URL)"
            );
            anyhow::ensure!(
                !e.http_model.is_empty(),
                "codeintel.embed.provider = \"http\" needs codeintel.embed.http_model"
            );
            if !ci.embed_remote {
                tracing::warn!(
                    "codeintel.embed.provider = \"http\": only repositories whose settings set [codeintel] embed_remote = true are sent to it (§6.5)"
                );
            }
        }
    }
    if ci.enabled && e.provider != EmbedProvider::None && !ci.require_catalog {
        tracing::warn!(
            "codeintel.require_catalog = false: embeddings live only in vector artifacts; a lost artifact means re-embedding"
        );
    }

    anyhow::ensure!(
        !mcp.snapshot_ttl.is_zero() && mcp.min_handle_ttl < mcp.snapshot_ttl,
        "mcp.snapshot_ttl must be > 0 and longer than mcp.min_handle_ttl"
    );
    anyhow::ensure!(
        mcp.semantic_deadline_ms >= 1 && mcp.max_concurrent_per_principal >= 1,
        "mcp.semantic_deadline_ms and max_concurrent_per_principal must be at least 1"
    );
    for o in &mcp.allowed_origins {
        anyhow::ensure!(
            (o.starts_with("https://") || o.starts_with("http://"))
                && !o.trim_end_matches('/').contains("/*")
                && !o.ends_with('/'),
            "mcp.allowed_origins entries are origins with a scheme and no path (got {o:?})"
        );
    }
    for (key, list) in [
        ("mcp.allowed_hosts", &mcp.allowed_hosts),
        ("mcp.audiences", &mcp.audiences),
        ("mcp.scopes", &mcp.scopes),
    ] {
        anyhow::ensure!(
            list.iter().all(|v| !v.trim().is_empty()),
            "{key} entries must not be empty"
        );
    }
    if mcp.enabled {
        anyhow::ensure!(
            ci.enabled,
            "mcp.enabled serves code intelligence: it needs codeintel.enabled"
        );
        anyhow::ensure!(
            mcp.scopes.iter().any(|s| s == "floe.code.read"),
            "mcp.scopes must include floe.code.read"
        );
    }
    Ok(())
}

impl Config {
    /// Whether code intelligence covers this (effective, per-repository) configuration:
    /// the host switch and the repository's own switch (`[codeintel] enabled` in its
    /// settings, defaulting to `codeintel.default_enabled`).
    pub fn codeintel_repo_enabled(&self) -> bool {
        self.codeintel.enabled && self.codeintel.default_enabled
    }

    /// The fail-closed rules of `[codeintel]`, `[mcp]` and `[access]` (§11) that involve this
    /// host's bootstrap (auth, listener, cache, the catalog): run on the effective config. The
    /// sections' own rules are [`check_document`]; what the binary can run is
    /// [`Config::validate_build`].
    pub(crate) fn validate_codeintel(&self) -> Result<()> {
        let ci = &self.codeintel;
        let mcp = &self.mcp;
        check_document(ci, mcp)?;
        if ci.enabled {
            if !self.cache_is_disk() {
                anyhow::ensure!(
                    ci.cache_bytes <= self.cache.max_bytes,
                    "codeintel.cache_bytes ({}) is counted inside cache.max_bytes ({}) and cannot exceed it",
                    ci.cache_bytes,
                    self.cache.max_bytes
                );
            }
            anyhow::ensure!(
                !ci.require_catalog || self.catalog.enabled,
                "codeintel.require_catalog = true needs the Iceberg catalog (catalog.enabled, D50); set codeintel.require_catalog = false for the standalone shape (shards from git only)"
            );
        }
        let auth = &self.server.auth;
        let handle_secret = auth.mcp_handle_secret.as_deref().filter(|s| !s.is_empty());
        if let Some(s) = handle_secret {
            anyhow::ensure!(
                s.len() >= MIN_SECRET_BYTES,
                "server.auth.mcp_handle_secret must be at least {MIN_SECRET_BYTES} bytes when set"
            );
        }
        if mcp.enabled {
            anyhow::ensure!(
                auth.mode != AuthMode::None || self.server.listen.ip().is_loopback(),
                "mcp.enabled with server.auth.mode = \"none\" is loopback-only (listen is {}): every agent would read every repository",
                self.server.listen
            );
            let session = auth.session_secret.as_deref().filter(|s| !s.is_empty());
            anyhow::ensure!(
                handle_secret.is_some() || session.is_some_and(|s| s.len() >= MIN_SECRET_BYTES),
                "mcp.enabled needs an HMAC key for snapshot handles and cursors: set server.auth.mcp_handle_secret (>= {MIN_SECRET_BYTES} bytes, the same on every host; FLOE__SERVER__AUTH__MCP_HANDLE_SECRET) or server.auth.session_secret"
            );
            if matches!(auth.mode, AuthMode::Oidc | AuthMode::Token) {
                anyhow::ensure!(
                    !mcp.allowed_origins.is_empty() || self.server.public_url.is_some(),
                    "mcp.allowed_origins is empty and there is no server.public_url to default it to: in {:?} mode the browser-origin allowlist must not be empty",
                    auth.mode
                );
            }
        }

        // [access] (D54)
        anyhow::ensure!(
            !self.access.read.is_empty(),
            "access.read must not be empty (the default is [\"{ACCESS_READ_DEFAULT}\"])"
        );
        for entry in &self.access.read {
            validate_access_entry(entry)?;
        }
        anyhow::ensure!(
            self.access.read.iter().all(|e| e == ACCESS_READ_DEFAULT),
            "access.read = {:?}: per-repository read rules are not enforced by this build yet (policy::authorize_read, D54, milestone M5); only the default [\"{ACCESS_READ_DEFAULT}\"] is accepted, so no rule is silently ignored",
            self.access.read
        );
        Ok(())
    }

    /// What the binary can run: every key that needs a cargo feature this build lacks is a
    /// fatal error naming the build flag (the rule `[catalog]` follows).
    pub fn validate_build(&self, built: BuildFeatures) -> Result<()> {
        if self.codeintel.enabled && !built.codeintel {
            anyhow::bail!(
                "codeintel.enabled = true, but this binary was built without the codeintel feature (cargo build --release -p floe-cli --features codeintel)"
            );
        }
        if self.mcp.enabled && !built.mcp {
            anyhow::bail!(
                "mcp.enabled = true, but this binary was built without the mcp feature (cargo build --release -p floe-cli --features mcp)"
            );
        }
        match self.codeintel.embed.provider {
            EmbedProvider::Local if !built.embed_local => anyhow::bail!(
                "codeintel.embed.provider = \"local\", but this binary was built without the embed-local feature (cargo build --release -p floe-cli --features embed-local)"
            ),
            EmbedProvider::Http if !built.embed_http => anyhow::bail!(
                "codeintel.embed.provider = \"http\", but this binary was built without the embed-http feature (cargo build --release -p floe-cli --features embed-http)"
            ),
            EmbedProvider::None | EmbedProvider::Local | EmbedProvider::Http => {}
        }
        Ok(())
    }
}

/// D24: shape a serialized effective config's `[codeintel]` section the way a repository sees
/// it — only [`CODEINTEL_REPO_KEYS`], with the repository switch (`default_enabled` in the
/// effective config) shown as `enabled`.
pub(crate) fn repo_codeintel_view(section: &mut toml::Table) {
    if let Some(v) = section.remove("default_enabled") {
        section.insert("enabled".into(), v);
    } else {
        section.remove("enabled");
    }
    section.retain(|k, _| CODEINTEL_REPO_KEYS.contains(&k));
}

/// D24: check a repository's `[codeintel]` settings section and move its `enabled` to the
/// effective `default_enabled` (the host switch is never set per repository).
pub(crate) fn repo_codeintel_overrides(section: &mut toml::Table) -> Result<()> {
    for k in section.keys() {
        anyhow::ensure!(
            CODEINTEL_REPO_KEYS.contains(&k.as_str()),
            "settings: codeintel.{k} is host-only (a repository may set: {})",
            CODEINTEL_REPO_KEYS.join(", ")
        );
    }
    if let Some(v) = section.remove("enabled") {
        section.insert("default_enabled".into(), v);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    fn err(c: &Config) -> String {
        format!("{:#}", c.validate().unwrap_err())
    }

    /// The smallest config that turns code intelligence and MCP on and validates.
    const ON: &str = r#"
[codeintel]
enabled = true
require_catalog = false
[mcp]
enabled = true
[server.auth]
mcp_handle_secret = "0123456789abcdef0123456789abcdef"
"#;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn defaults_are_off_and_validate() {
        let c = Config::default();
        c.validate().unwrap();
        c.validate_build(BuildFeatures::default()).unwrap();
        assert!(!c.codeintel.enabled && !c.mcp.enabled);
        assert!(!c.codeintel_repo_enabled());
        assert_eq!(c.codeintel.refs, vec!["HEAD"]);
        assert_eq!(c.codeintel.max_file_bytes, ByteSize::mib(1));
        assert_eq!(c.codeintel.retention, Duration::from_hours(7 * 24));
        assert_eq!(c.codeintel.embed.provider, EmbedProvider::None);
        assert_eq!(c.codeintel.catalog.namespace, "code");
        assert_eq!(c.mcp.snapshot_ttl, Duration::from_hours(24));
        assert_eq!(c.mcp.query_log, QueryLog::Sampled);
        assert_eq!(c.access.read, vec![ACCESS_READ_DEFAULT]);
        cfg(ON).validate().unwrap();
        assert!(cfg(ON).codeintel_repo_enabled());
    }

    #[test]
    fn every_section_parses_from_toml() {
        let c = cfg(r#"
[codeintel]
enabled = true
default_enabled = false
refs = ["HEAD", "refs/heads/release/*"]
max_refs = 4
semantic = false
embed_remote = true
max_file_bytes = "2MiB"
exclude = ["third_party/**"]
part_max_bytes = "256MiB"
delta_max_files = 100
delta_max_ratio = 0.2
cache_bytes = "1GiB"
sync_fetch_max_bytes = "32MiB"
query_threads = 4
head_ttl = "2s"
retention = "3d"
recent_max = 16
catalog_retry_max = "1m"
reindex_queue_timeout = "30m"
require_catalog = false
index_private_mirrors = false
known_blobs_max_bytes = "64MiB"
prewarm = ["acme/*"]
adhoc_max_files = 50
[codeintel.embed]
provider = "http"
model = "m"
http_url = "http://127.0.0.1:11434/v1/embeddings"
http_model = "nomic"
api_key_env = "EMBED_KEY"
batch = 8
max_rps = 5
[codeintel.catalog]
table_bucket = "t"
namespace = "n"
flush_interval = "10s"
flush_rows = 10
[mcp]
enabled = true
allowed_origins = ["https://floe.example.com"]
allowed_hosts = ["floe.example.com"]
audiences = ["https://floe.example.com/api/v1/mcp"]
scopes = ["floe.code.read"]
snapshot_ttl = "12h"
min_handle_ttl = "5m"
semantic_deadline_ms = 30
max_concurrent_per_principal = 2
query_log = "off"
query_log_text = true
[access]
read = ["authenticated"]
[server.auth]
mcp_handle_secret = "0123456789abcdef0123456789abcdef"
"#);
        c.validate().unwrap();
        assert_eq!(c.codeintel.max_file_bytes, ByteSize::mib(2));
        assert_eq!(c.codeintel.retention, Duration::from_hours(72));
        assert_eq!(c.codeintel.embed.provider, EmbedProvider::Http);
        assert_eq!(c.codeintel.catalog.flush_rows, 10);
        assert_eq!(c.mcp.query_log, QueryLog::Off);
        assert!(!c.codeintel_repo_enabled(), "default_enabled = false");
        assert!(toml::from_str::<Config>("[codeintel]\nbogus = 1\n").is_err());
        assert!(toml::from_str::<Config>("[mcp]\nbogus = 1\n").is_err());
        assert!(toml::from_str::<Config>("[access]\nwrite = []\n").is_err());
        assert!(toml::from_str::<Config>("[codeintel.embed]\nprovider = \"gpu\"\n").is_err());
    }

    #[test]
    fn retention_must_cover_the_snapshot_ttl_plus_an_hour() {
        let mut c = Config::default();
        c.codeintel.retention = Duration::from_hours(24);
        assert!(err(&c).contains("mcp.snapshot_ttl + 1h"), "{}", err(&c));
        c.codeintel.retention = Duration::from_hours(25);
        c.validate().unwrap();
        c.mcp.snapshot_ttl = Duration::from_hours(48);
        assert!(err(&c).contains("codeintel.retention"), "{}", err(&c));
    }

    #[test]
    fn mcp_needs_an_hmac_key_of_at_least_32_bytes() {
        let mut c = cfg(ON);
        c.server.auth.mcp_handle_secret = None;
        assert!(err(&c).contains("mcp_handle_secret"), "{}", err(&c));
        // Empty is "unset" (derive from session_secret), not a key.
        c.server.auth.mcp_handle_secret = Some(String::new());
        assert!(err(&c).contains("mcp_handle_secret"), "{}", err(&c));
        c.server.auth.mcp_handle_secret = Some("short".into());
        assert!(err(&c).contains("at least 32 bytes"), "{}", err(&c));
        // A short handle secret is refused even with MCP off: it would be used later.
        c.mcp.enabled = false;
        assert!(err(&c).contains("at least 32 bytes"), "{}", err(&c));
        c.mcp.enabled = true;
        c.server.auth.mcp_handle_secret = None;
        c.server.auth.session_secret = Some(SECRET.into());
        c.validate().unwrap();
        c.server.auth.mcp_handle_secret = Some(SECRET.into());
        c.server.auth.session_secret = None;
        c.validate().unwrap();
    }

    #[test]
    fn mcp_needs_codeintel_and_the_read_scope() {
        let mut c = cfg(ON);
        c.codeintel.enabled = false;
        assert!(err(&c).contains("needs codeintel.enabled"), "{}", err(&c));
        let mut c = cfg(ON);
        c.mcp.scopes = vec!["floe.code.admin".into()];
        assert!(err(&c).contains("floe.code.read"), "{}", err(&c));
    }

    #[test]
    fn mcp_without_auth_is_loopback_only() {
        // auth.mode = none may bind publicly only with the dev facade on; MCP still refuses.
        let mut c = cfg(ON);
        c.github.enabled = true;
        c.server.listen = "0.0.0.0:8080".parse().unwrap();
        assert!(err(&c).contains("loopback-only"), "{}", err(&c));
        c.mcp.enabled = false;
        c.validate().unwrap();
    }

    #[test]
    fn mcp_origin_allowlist_is_never_empty_with_real_auth() {
        let mut c = cfg(ON);
        c.server.auth.mode = AuthMode::Token;
        c.server.auth.tokens = vec![crate::StaticToken {
            principal: "robot".into(),
            token: "t".into(),
            token_env: None,
            write: false,
            admin: false,
        }];
        assert!(err(&c).contains("mcp.allowed_origins"), "{}", err(&c));
        c.server.public_url = Some("https://floe.example.com".into());
        c.validate().unwrap();
        c.server.public_url = None;
        c.mcp.allowed_origins = vec!["https://agents.example.com".into()];
        c.validate().unwrap();
        c.mcp.allowed_origins = vec!["agents.example.com".into()];
        assert!(err(&c).contains("scheme"), "{}", err(&c));
    }

    #[test]
    fn codeintel_needs_the_catalog_or_standalone() {
        let mut c = cfg(ON);
        c.codeintel.require_catalog = true;
        assert!(err(&c).contains("catalog.enabled"), "{}", err(&c));
        c.catalog.enabled = true;
        c.catalog.uri = Some("http://127.0.0.1:9000/iceberg".into());
        c.catalog.warehouse = Some("floe-catalog".into());
        c.validate().unwrap();
        c.catalog.enabled = false;
        // Off, the default require_catalog = true is fine.
        c.codeintel.enabled = false;
        c.mcp.enabled = false;
        c.validate().unwrap();
    }

    #[test]
    fn codeintel_shape_rules() {
        let mut c = cfg(ON);
        c.codeintel.refs = vec!["refs/archive/x".into()];
        assert!(err(&c).contains("never indexed"), "{}", err(&c));
        c.codeintel.refs = vec!["main".into()];
        assert!(err(&c).contains("HEAD or ref patterns"), "{}", err(&c));
        c.codeintel.refs = Vec::new();
        assert!(err(&c).contains("at least one ref"), "{}", err(&c));
        let mut c = cfg(ON);
        c.codeintel.cache_bytes = ByteSize::gib(64);
        assert!(err(&c).contains("cache.max_bytes"), "{}", err(&c));
        let mut c = cfg(ON);
        c.codeintel.delta_max_ratio = 0.0;
        assert!(err(&c).contains("delta_max_ratio"), "{}", err(&c));
        let mut c = cfg(ON);
        c.codeintel.max_file_bytes = ByteSize::gib(1);
        assert!(err(&c).contains("max_file_bytes"), "{}", err(&c));
    }

    #[test]
    fn embed_provider_rules() {
        let mut c = Config::default();
        c.codeintel.embed.provider = EmbedProvider::Local;
        assert!(err(&c).contains("model_object"), "{}", err(&c));
        c.codeintel.embed.model_object = "codeintel/models/abc/".into();
        c.validate().unwrap();
        c.codeintel.embed.provider = EmbedProvider::Http;
        assert!(err(&c).contains("http_url"), "{}", err(&c));
        c.codeintel.embed.http_url = "https://embed.example.com/v1/embeddings".into();
        assert!(err(&c).contains("http_model"), "{}", err(&c));
        c.codeintel.embed.http_model = "m".into();
        c.validate().unwrap();
        c.codeintel.embed.api_key_env = "sk-live-123".into();
        assert!(err(&c).contains("api_key_env"), "{}", err(&c));
        c.codeintel.embed.api_key_env = "OPENAI_API_KEY".into();
        c.validate().unwrap();
    }

    #[test]
    fn build_features_gate_every_feature_key_and_name_the_flag() {
        let all = BuildFeatures {
            codeintel: true,
            mcp: true,
            embed_local: true,
            embed_http: true,
        };
        let c = cfg(ON);
        c.validate_build(all).unwrap();
        let e = c
            .validate_build(BuildFeatures {
                codeintel: false,
                ..all
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("--features codeintel"), "{e}");
        let e = c
            .validate_build(BuildFeatures { mcp: false, ..all })
            .unwrap_err()
            .to_string();
        assert!(e.contains("--features mcp"), "{e}");
        let mut c = Config::default();
        c.codeintel.embed.provider = EmbedProvider::Local;
        let e = c
            .validate_build(BuildFeatures {
                embed_local: false,
                ..all
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("--features embed-local"), "{e}");
        c.codeintel.embed.provider = EmbedProvider::Http;
        let e = c
            .validate_build(BuildFeatures {
                embed_http: false,
                ..all
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("--features embed-http"), "{e}");
        // A default config runs on a default build.
        Config::default()
            .validate_build(BuildFeatures::default())
            .unwrap();
    }

    #[test]
    fn access_read_is_checked_and_only_the_default_is_accepted_until_enforced() {
        let mut c = Config::default();
        for bad in ["", "email:", "email:nobody", "team:x", "a b"] {
            c.access.read = vec![bad.into()];
            assert!(
                !err(&c).contains("not enforced"),
                "{bad:?} must fail its syntax check first: {}",
                err(&c)
            );
        }
        c.access.read = Vec::new();
        assert!(err(&c).contains("must not be empty"), "{}", err(&c));
        for good in [
            "public",
            "group:eng",
            "domain:example.com",
            "email:a@example.com",
            "robot",
        ] {
            c.access.read = vec![good.into()];
            assert!(err(&c).contains("not enforced"), "{good}: {}", err(&c));
        }
        c.access.read = vec![ACCESS_READ_DEFAULT.into()];
        c.validate().unwrap();
    }

    #[test]
    fn repository_settings_set_the_repo_switch_and_nothing_host_only() {
        let host = cfg(ON);
        let repo = host
            .with_settings(
                "[codeintel]\nenabled = false\nrefs = [\"HEAD\", \"refs/heads/release/*\"]\n",
            )
            .unwrap();
        assert!(
            repo.codeintel.enabled,
            "the host switch is never set per repo"
        );
        assert!(!repo.codeintel_repo_enabled());
        assert_eq!(repo.codeintel.refs.len(), 2);
        // A repo on a host whose default is off can opt in.
        let mut host_off = cfg(ON);
        host_off.codeintel.default_enabled = false;
        assert!(!host_off.codeintel_repo_enabled());
        let repo = host_off
            .with_settings("[codeintel]\nenabled = true\n")
            .unwrap();
        assert!(repo.codeintel_repo_enabled());
        for host_only in [
            "[codeintel]\npart_max_bytes = \"1GiB\"\n",
            "[codeintel]\nrequire_catalog = true\n",
            "[codeintel.embed]\nprovider = \"http\"\n",
            "[mcp]\nenabled = false\n",
        ] {
            assert!(host.with_settings(host_only).is_err(), "{host_only}");
        }
        let e = format!(
            "{:#}",
            host.with_settings("[access]\nread = [\"public\"]\n")
                .unwrap_err()
        );
        assert!(e.contains("not enforced"), "{e}");
        host.with_settings("[access]\nread = [\"authenticated\"]\n")
            .unwrap();

        // The settings view shows the repo keys only, with the switch as `enabled`.
        let shown: toml::Table = repo.public_settings_toml().unwrap().parse().unwrap();
        let ci = shown["codeintel"].as_table().unwrap();
        assert_eq!(ci["enabled"].as_bool(), Some(true));
        assert!(!ci.contains_key("default_enabled") && !ci.contains_key("part_max_bytes"));
        assert!(!ci.contains_key("embed") && !ci.contains_key("catalog"));
        assert!(shown.contains_key("access") && !shown.contains_key("mcp"));
    }

    /// `floe.example.toml` documents every key with its default (AGENTS §5): `[access]` in the
    /// file, and the runtime `[codeintel]`/`[mcp]` sections as the commented document form
    /// `floe config set` takes, both equal to the built-in defaults.
    #[test]
    fn example_toml_documents_the_defaults() {
        let text = include_str!("../../../floe.example.toml");
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.access.read, Config::default().access.read);
        // The commented document block: from `# [codeintel]` to the end, uncommented.
        let start = text.find("\n# [codeintel]").unwrap() + 1;
        let doc = text
            .get(start..)
            .unwrap()
            .lines()
            .map(|l| {
                l.strip_prefix("# ")
                    .or_else(|| l.strip_prefix('#'))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rt = crate::RuntimeConfig::from_toml(&doc).unwrap();
        let d = crate::RuntimeConfig::default();
        assert_eq!(
            toml::to_string(&rt.codeintel).unwrap(),
            toml::to_string(&d.codeintel).unwrap()
        );
        assert_eq!(
            toml::to_string(&rt.mcp).unwrap(),
            toml::to_string(&d.mcp).unwrap()
        );
    }

    /// D60: the sections are runtime — refused in the file, carried by the document, and
    /// validated there on their own.
    #[test]
    fn codeintel_and_mcp_are_runtime_sections() {
        for section in ["codeintel", "mcp"] {
            let e = Config::parse(&format!("[{section}]\nenabled = false\n")).unwrap_err();
            assert!(format!("{e:#}").contains("runtime configuration"), "{e:#}");
        }
        let rt = crate::RuntimeConfig::from_toml(
            "[codeintel]\nenabled = true\nrequire_catalog = false\n[mcp]\nenabled = true\n",
        )
        .unwrap();
        rt.validate().unwrap();
        // The host supplies the HMAC key; without one the effective config is refused.
        let e = Config::default().with_runtime(&rt).unwrap_err();
        assert!(format!("{e:#}").contains("mcp_handle_secret"), "{e:#}");
        let host = cfg(ON);
        let eff = host.with_runtime(&rt).unwrap();
        assert!(eff.codeintel.enabled && eff.mcp.enabled);
        let bad = crate::RuntimeConfig::from_toml("[mcp]\nenabled = true\n").unwrap();
        let e = bad.validate().unwrap_err();
        assert!(
            format!("{e:#}").contains("needs codeintel.enabled"),
            "{e:#}"
        );
        assert!(crate::RuntimeConfig::from_toml("[mcp]\nhandle_secret = \"x\"\n").is_err());
    }
}
