//! D60 — the versioned runtime config store (`docs/design/admin-ui.md` §2).
//!
//! Objects under `<config_store.prefix>` in `[config_store] bucket` (default:
//! `config/` in the main bucket):
//!
//! * `current.json` — the **commit point**, CAS'd: `{revision, updated_at,
//!   author, message, rolled_back_from?, document, diff}`; the document's
//!   secrets are sealed (D61).
//! * `history/<revision:020>.json` — `Create`, immutable: every committed
//!   revision; with the whole record (`records` mode) or, when the bucket keeps
//!   object versions, metadata + the object version of `current.json` that
//!   revision wrote (`versions` mode).
//! * `instances/<instance>.json` — `Overwrite` heartbeats ([`live`]).
//!
//! Publishing validates with the same code as startup and fails closed: an
//! invalid document is a list of [`FieldError`]s and nothing is written.

pub mod live;
pub mod schema;
pub mod seal;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use floe_config::runtime::{RESTART_ONLY, RUNTIME_SECTIONS, SECRET_PATHS, flatten};
use floe_config::{Config, HistoryMode, RuntimeConfig, Secret};
use floe_store::{DynStore, ObjectStoreExt, PutMode, Version};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use seal::SealKey;

/// The commit point.
pub const CURRENT: &str = "current.json";
/// Heartbeats of the instances that follow this store.
pub const INSTANCES: &str = "instances/";
/// Largest document accepted (a few KiB in practice).
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Most history entries one call returns.
pub const MAX_HISTORY_PAGE: usize = 50;
/// How long a lost publish race is retried when the caller named no base.
const PUBLISH_ATTEMPTS: u32 = 5;
/// Bound on the startup question "does this bucket keep object versions?".
const VERSIONING_PROBE: Duration = Duration::from_secs(3);
/// Heartbeats older than this are deleted by the lister.
const INSTANCE_EXPIRY: chrono::TimeDelta = chrono::TimeDelta::days(1);

/// `history/<revision:020>.json`.
pub fn history_key(revision: u64) -> String {
    format!("history/{revision:020}.json")
}

/// One committed revision (`current.json`, and a history record's body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub revision: u64,
    pub updated_at: DateTime<Utc>,
    pub author: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back_from: Option<u64>,
    /// The document as stored: secrets sealed or env references.
    pub document: Value,
    /// What changed against the previous revision (secrets masked).
    #[serde(default)]
    pub diff: Vec<DiffEntry>,
}

/// `history/<revision>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub revision: u64,
    pub updated_at: DateTime<Utc>,
    pub author: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back_from: Option<u64>,
    #[serde(default)]
    pub diff: Vec<DiffEntry>,
    /// `records` mode: the stored document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<Value>,
    /// `versions` mode: the version of `current.json` this revision wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_version: Option<String>,
}

impl HistoryEntry {
    fn of(r: &Record, document: Option<Value>, object_version: Option<String>) -> Self {
        HistoryEntry {
            revision: r.revision,
            updated_at: r.updated_at,
            author: r.author.clone(),
            message: r.message.clone(),
            rolled_back_from: r.rolled_back_from,
            diff: r.diff.clone(),
            document,
            object_version,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Added,
    Removed,
    Changed,
}

/// One changed leaf (dotted path; arrays and secrets are leaves).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffEntry {
    pub path: String,
    pub op: DiffOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<Value>,
}

/// A validation error, with the document path it names when it names one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub message: String,
}

impl FieldError {
    fn at(path: &str, message: impl Into<String>) -> Self {
        FieldError {
            path: Some(path.to_string()),
            message: message.into(),
        }
    }

    /// From a validation error: the path is the first `section.key` token of
    /// the innermost cause (`github_mirror.include entry …` → `github_mirror.include`).
    pub fn from_error(e: &anyhow::Error) -> Self {
        FieldError {
            path: error_path(&e.root_cause().to_string()),
            message: format!("{e:#}"),
        }
    }
}

fn error_path(message: &str) -> Option<String> {
    let token: String = message
        .split_whitespace()
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '.')
        .collect();
    let token = token.trim_end_matches('.');
    let (section, rest) = token.split_once('.')?;
    (RUNTIME_SECTIONS.contains(&section) && !rest.is_empty()).then(|| token.to_string())
}

/// Why a publish (or a read of a revision) did not happen.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("invalid config document ({} error(s))", .0.len())]
    Invalid(Vec<FieldError>),
    #[error("the config changed: current revision is {current}")]
    Conflict { current: u64 },
    #[error("no revision {0}")]
    NotFound(u64),
    #[error("revision {0} is no longer kept by the bucket")]
    Gone(u64),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// A document checked and resolved for writing.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// What `current.json` will carry (secrets sealed).
    pub stored: Value,
    /// Secrets opened: what an instance applies.
    pub opened: RuntimeConfig,
    /// Against the current revision, secrets masked.
    pub diff: Vec<DiffEntry>,
    /// Restart-only paths the change touches.
    pub restart_required: Vec<String>,
}

/// A publish request.
#[derive(Debug, Clone)]
pub struct PublishRequest<'a> {
    pub document: &'a Value,
    pub author: &'a str,
    pub message: &'a str,
    /// The revision the editor started from; another one is a 409.
    pub base_revision: Option<u64>,
    pub rolled_back_from: Option<u64>,
}

/// What a publish wrote.
#[derive(Debug, Clone)]
pub struct Published {
    pub record: Record,
    pub restart_required: Vec<String>,
}

/// `current.json` against a known version.
#[derive(Debug)]
pub enum Changed {
    Unchanged,
    Missing,
    Changed(Version, Box<Record>),
}

/// One instance's heartbeat (`instances/<id>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub instance: String,
    pub version: String,
    pub roles: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub seen_at: DateTime<Utc>,
    pub applied_revision: u64,
    #[serde(default)]
    pub restart_required: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_error: Option<String>,
}

/// The config store of this process.
pub struct ConfigStore {
    store: DynStore,
    location: String,
    history: HistoryMode,
    key_env: String,
    key: Option<Arc<SealKey>>,
}

impl std::fmt::Debug for ConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigStore")
            .field("location", &self.location)
            .field("history", &self.history)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl ConfigStore {
    /// Open the store `[config_store]` names: a dedicated bucket (same backend
    /// and credentials as `[store]`) or `main`, under the prefix. The sealing
    /// key is read once from `config_store.key_env`.
    pub async fn open(cfg: &Config, main: &DynStore) -> Result<ConfigStore> {
        let cs = &cfg.config_store;
        let prefix = normalize_prefix(&cs.prefix);
        let (base, location) = match cs.bucket.as_deref().filter(|b| !b.trim().is_empty()) {
            Some(bucket) => {
                let mut c = cfg.clone();
                c.store.bucket = bucket.to_string();
                c.store.prefix = String::new();
                let store = floe_store::open_store(&c)
                    .await
                    .with_context(|| format!("opening the config bucket {bucket}"))?;
                (store, format!("{bucket}/{prefix}"))
            }
            None => (
                main.clone(),
                format!("{}/{}{prefix}", cfg.store.bucket, cfg.store_prefix()),
            ),
        };
        let store: DynStore = if prefix.is_empty() {
            base
        } else {
            Arc::new(floe_store::Prefixed::new(base, prefix))
        };
        let history = resolve_history(cs.history, &store).await?;
        let key = SealKey::from_env(&cs.key_env)?.map(Arc::new);
        Ok(ConfigStore {
            store,
            location,
            history,
            key_env: cs.key_env.clone(),
            key,
        })
    }

    /// A store over `store` as is (tests, embedding).
    pub fn new(store: DynStore, history: HistoryMode, key: Option<SealKey>) -> ConfigStore {
        ConfigStore {
            store,
            location: "memory".into(),
            history: match history {
                HistoryMode::Auto => HistoryMode::Records,
                m => m,
            },
            key_env: "FLOE_CONFIG_KEY".into(),
            key: key.map(Arc::new),
        }
    }

    /// `bucket/prefix` for humans.
    pub fn location(&self) -> &str {
        &self.location
    }

    /// `records` or `versions` (never `auto` once open).
    pub fn history_mode(&self) -> HistoryMode {
        self.history
    }

    /// Whether this process can seal and open secrets.
    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    /// The env var the sealing key is read from.
    pub fn key_env(&self) -> &str {
        &self.key_env
    }

    /// The raw store (heartbeats, tests).
    pub fn store(&self) -> &DynStore {
        &self.store
    }

    /// `current.json` with its version; `None` before the first publish.
    pub async fn current(&self) -> Result<Option<(Version, Record)>> {
        match self.store.get_bytes(CURRENT).await? {
            None => Ok(None),
            Some((meta, body)) => Ok(Some((
                meta.version,
                serde_json::from_slice(&body).context("decoding config current.json")?,
            ))),
        }
    }

    /// The conditional GET every instance revalidates with.
    pub async fn current_if_changed(&self, known: Option<&Version>) -> Result<Changed> {
        let Some(known) = known else {
            return Ok(match self.current().await? {
                None => Changed::Missing,
                Some((v, r)) => Changed::Changed(v, Box::new(r)),
            });
        };
        match self.store.get_if_changed(CURRENT, known).await {
            Ok(None) => Ok(Changed::Unchanged),
            Ok(Some((meta, body))) => Ok(Changed::Changed(
                meta.version,
                Box::new(serde_json::from_slice(&body).context("decoding config current.json")?),
            )),
            Err(e) if e.is_not_found() => Ok(Changed::Missing),
            Err(e) => Err(e.into()),
        }
    }

    fn open_sealed(&self, sealed: &str, path: &str) -> Result<String, String> {
        match &self.key {
            None => Err(format!(
                "{path} is sealed, but {} is not set on this instance",
                self.key_env
            )),
            Some(k) => k.open(sealed, path).map_err(|e| format!("{e:#}")),
        }
    }

    /// Open every sealed secret of a stored document (what an instance applies).
    pub fn open_document(&self, stored: &Value) -> Result<RuntimeConfig> {
        let mut doc = RuntimeConfig::from_json(stored)?;
        for path in SECRET_PATHS {
            if let Some(secret) = doc.secret_mut(path) {
                match secret.clone() {
                    Secret::Sealed(s) => {
                        let v = self.open_sealed(&s, path).map_err(anyhow::Error::msg)?;
                        *secret = Secret::Value(v);
                    }
                    Secret::Redacted(_) => {
                        anyhow::bail!("{path}: a stored document cannot carry a redaction marker")
                    }
                    Secret::Env(_) | Secret::Value(_) => {}
                }
            }
        }
        Ok(doc)
    }

    /// Check and resolve a submitted document against the current record and
    /// this instance's bootstrap config: parse, seal `value`s, keep
    /// `redacted` ones, open `sealed` ones, validate (fail closed).
    pub fn prepare(
        &self,
        submitted: &Value,
        current: Option<&Record>,
        bootstrap: &Config,
    ) -> Result<Prepared, Vec<FieldError>> {
        let size = serde_json::to_vec(submitted).map_or(0, |v| v.len());
        if size > MAX_DOCUMENT_BYTES {
            return Err(vec![FieldError {
                path: None,
                message: format!("the document is {size} bytes; the limit is {MAX_DOCUMENT_BYTES}"),
            }]);
        }
        let mut doc = RuntimeConfig::from_json(submitted).map_err(|e| vec![FieldError::from_error(&e)])?;
        let current_doc = current.and_then(|r| RuntimeConfig::from_json(&r.document).ok());
        let mut opened = doc.clone();
        let mut errors = Vec::new();
        for path in SECRET_PATHS {
            let Some(secret) = doc.secret_mut(path) else {
                continue;
            };
            let resolved = match secret.clone() {
                Secret::Env(name) => {
                    if name.trim().is_empty() {
                        Err("an env reference needs a variable name".to_string())
                    } else {
                        Ok((Secret::Env(name.clone()), Secret::Env(name)))
                    }
                }
                Secret::Value(v) => match &self.key {
                    None => Err(format!(
                        "set {} on the instances to store secrets in the config store, or use an env reference ({{\"env\": \"NAME\"}})",
                        self.key_env
                    )),
                    Some(k) => k
                        .seal(&v, path)
                        .map(|s| (Secret::Sealed(s), Secret::Value(v)))
                        .map_err(|e| format!("{e:#}")),
                },
                Secret::Redacted(_) => match current_doc.as_ref().and_then(|d| d.secret(path)) {
                    Some(Secret::Sealed(s)) => self
                        .open_sealed(s, path)
                        .map(|v| (Secret::Sealed(s.clone()), Secret::Value(v))),
                    Some(other) => Ok((other.clone(), other.clone())),
                    None => Err("nothing is stored to keep; enter a value".to_string()),
                },
                Secret::Sealed(s) => self
                    .open_sealed(&s, path)
                    .map(|v| (Secret::Sealed(s), Secret::Value(v))),
            };
            match resolved {
                Ok((stored, open)) => {
                    *secret = stored;
                    if let Some(o) = opened.secret_mut(path) {
                        *o = open;
                    }
                }
                Err(message) => errors.push(FieldError::at(path, message)),
            }
        }
        if errors.is_empty() {
            if let Err(e) = opened.validate() {
                errors.push(FieldError::from_error(&e));
            } else if let Err(e) = bootstrap.with_runtime(&opened) {
                errors.push(FieldError::from_error(&e));
            } else if opened.catalog.enabled && !cfg!(feature = "catalog") {
                errors.push(FieldError::at(
                    "catalog.enabled",
                    "this binary was built without the catalog feature (cargo build --release -p floe-cli --features catalog)",
                ));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let stored = doc.to_json().map_err(|e| vec![FieldError::from_error(&e)])?;
        let diff = diff(current.map(|r| &r.document), &stored);
        let restart_required = restart_paths(&diff);
        Ok(Prepared {
            stored,
            opened,
            diff,
            restart_required,
        })
    }

    /// Publish a document as the next revision (§2.2).
    pub async fn publish(
        &self,
        req: &PublishRequest<'_>,
        bootstrap: &Config,
    ) -> Result<Published, PublishError> {
        for _ in 0..PUBLISH_ATTEMPTS {
            let current = self.current().await?;
            let current_revision = current.as_ref().map_or(0, |(_, r)| r.revision);
            if let Some(base) = req.base_revision
                && base != current_revision
            {
                return Err(PublishError::Conflict {
                    current: current_revision,
                });
            }
            if let Some((v, r)) = &current {
                self.ensure_history(r, v).await;
            }
            let prepared = self
                .prepare(req.document, current.as_ref().map(|(_, r)| r), bootstrap)
                .map_err(PublishError::Invalid)?;
            let record = Record {
                revision: current_revision + 1,
                updated_at: Utc::now(),
                author: req.author.to_string(),
                message: req.message.trim().to_string(),
                rolled_back_from: req.rolled_back_from,
                document: prepared.stored,
                diff: prepared.diff,
            };
            let body = serde_json::to_vec_pretty(&record).context("encoding the config record")?;
            let mode = match &current {
                None => PutMode::Create,
                Some((v, _)) => PutMode::Update(v.clone()),
            };
            match self.store.put_bytes(CURRENT, body, mode).await {
                Ok(meta) => {
                    self.ensure_history(&record, &meta.version).await;
                    tracing::info!(
                        revision = record.revision,
                        author = %record.author,
                        message = %record.message,
                        changes = record.diff.len(),
                        rolled_back_from = ?record.rolled_back_from,
                        "config published"
                    );
                    return Ok(Published {
                        record,
                        restart_required: prepared.restart_required,
                    });
                }
                Err(e) if e.is_precondition_failed() => {
                    if req.base_revision.is_some() {
                        let now = self.current().await?.map_or(0, |(_, r)| r.revision);
                        return Err(PublishError::Conflict { current: now });
                    }
                }
                Err(e) => return Err(PublishError::Store(e.into())),
            }
        }
        Err(PublishError::Store(anyhow::anyhow!(
            "the config changed under every one of {PUBLISH_ATTEMPTS} attempts"
        )))
    }

    /// Make sure `history/<record.revision>` exists (best effort; the next
    /// publish heals a miss).
    async fn ensure_history(&self, record: &Record, version: &Version) {
        let entry = match self.history {
            HistoryMode::Versions => HistoryEntry::of(record, None, Some(version.to_string())),
            HistoryMode::Records | HistoryMode::Auto => {
                HistoryEntry::of(record, Some(record.document.clone()), None)
            }
        };
        let Ok(body) = serde_json::to_vec_pretty(&entry) else {
            return;
        };
        match self
            .store
            .put_bytes(&history_key(record.revision), body, PutMode::Create)
            .await
        {
            Ok(_) => {}
            Err(e) if e.is_precondition_failed() => {}
            Err(e) => {
                tracing::warn!(revision = record.revision, error = %e, "config history write failed; the next publish retries it");
            }
        }
    }

    /// Newest first, revisions `< before` (or from the current one), at most
    /// `n` ([`MAX_HISTORY_PAGE`]). Documents are left out.
    pub async fn history(&self, before: Option<u64>, n: usize) -> Result<Vec<HistoryEntry>> {
        let Some((_, current)) = self.current().await? else {
            return Ok(Vec::new());
        };
        let top = before.map_or(current.revision, |b| {
            b.saturating_sub(1).min(current.revision)
        });
        let revisions: Vec<u64> = (1..=top).rev().take(n.clamp(1, MAX_HISTORY_PAGE)).collect();
        let reads = revisions
            .iter()
            .map(|rev| self.history_entry(*rev, &current));
        let mut out = Vec::new();
        for got in futures::future::join_all(reads).await {
            if let Some(mut e) = got? {
                e.document = None;
                e.object_version = None;
                out.push(e);
            }
        }
        Ok(out)
    }

    async fn history_entry(&self, rev: u64, current: &Record) -> Result<Option<HistoryEntry>> {
        if rev == current.revision {
            return Ok(Some(HistoryEntry::of(current, None, None)));
        }
        match self.store.get_bytes(&history_key(rev)).await? {
            None => Ok(None),
            Some((_, body)) => serde_json::from_slice::<HistoryEntry>(&body)
                .map(Some)
                .with_context(|| format!("decoding {}", history_key(rev))),
        }
    }

    /// One revision's record (its stored document; redact before showing).
    pub async fn revision(&self, n: u64) -> Result<Record, PublishError> {
        let Some((_, current)) = self.current().await? else {
            return Err(PublishError::NotFound(n));
        };
        if n == current.revision {
            return Ok(current);
        }
        if n == 0 || n > current.revision {
            return Err(PublishError::NotFound(n));
        }
        let Some((_, body)) = self
            .store
            .get_bytes(&history_key(n))
            .await
            .map_err(anyhow::Error::from)?
        else {
            return Err(PublishError::Gone(n));
        };
        let entry: HistoryEntry =
            serde_json::from_slice(&body).with_context(|| format!("decoding {}", history_key(n)))?;
        if let Some(document) = entry.document.clone() {
            return Ok(Record {
                revision: entry.revision,
                updated_at: entry.updated_at,
                author: entry.author,
                message: entry.message,
                rolled_back_from: entry.rolled_back_from,
                document,
                diff: entry.diff,
            });
        }
        let Some(ov) = entry.object_version else {
            return Err(PublishError::Gone(n));
        };
        match self.store.get_version(CURRENT, &Version::new(ov)).await {
            Ok(Some(body)) => {
                let r: Record = serde_json::from_slice(&body).context("decoding a config object version")?;
                Ok(r)
            }
            Ok(None) => Err(PublishError::Gone(n)),
            Err(e) => Err(PublishError::Store(e.into())),
        }
    }

    /// Publish revision `n`'s document as a new revision (§2.4).
    pub async fn rollback(
        &self,
        n: u64,
        author: &str,
        message: &str,
        base_revision: Option<u64>,
        bootstrap: &Config,
    ) -> Result<Published, PublishError> {
        let old = self.revision(n).await?;
        let msg = if message.trim().is_empty() {
            format!("roll back to revision {n}")
        } else {
            message.to_string()
        };
        self.publish(
            &PublishRequest {
                document: &old.document,
                author,
                message: &msg,
                base_revision,
                rolled_back_from: Some(n),
            },
            bootstrap,
        )
        .await
    }

    /// Write this instance's heartbeat (`Overwrite`: a heartbeat, principle II).
    pub async fn heartbeat(&self, status: &InstanceStatus) -> Result<()> {
        let body = serde_json::to_vec_pretty(status)?;
        self.store
            .put_bytes(&instance_key(&status.instance), body, PutMode::Overwrite)
            .await?;
        Ok(())
    }

    /// Every instance heartbeat (a LIST: admin pages only, never a hot path);
    /// entries older than a day are deleted.
    pub async fn instances(&self) -> Result<Vec<InstanceStatus>> {
        let mut keys = Vec::new();
        let mut listing = self.store.list(INSTANCES, None);
        while let Some(meta) = listing.next().await {
            keys.push(meta?.key);
            if keys.len() >= 500 {
                break;
            }
        }
        let reads = keys.iter().map(|k| async move {
            let got = self.store.get_bytes(k).await.ok().flatten();
            (k.clone(), got.and_then(|(_, b)| serde_json::from_slice::<InstanceStatus>(&b).ok()))
        });
        let now = Utc::now();
        let mut out = Vec::new();
        for (key, status) in futures::future::join_all(reads).await {
            match status {
                Some(s) if now.signed_duration_since(s.seen_at) <= INSTANCE_EXPIRY => out.push(s),
                _ => {
                    if let Err(e) = self.store.delete(&key, None).await {
                        tracing::debug!(key, error = %e, "expired instance heartbeat not deleted");
                    }
                }
            }
        }
        out.sort_by(|a, b| b.seen_at.cmp(&a.seen_at));
        Ok(out)
    }
}

fn normalize_prefix(p: &str) -> String {
    let p = p.trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

/// `instances/<id>.json`, the id made key-safe.
fn instance_key(instance: &str) -> String {
    let safe: String = instance
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    format!("{INSTANCES}{safe}.json")
}

async fn resolve_history(mode: HistoryMode, store: &DynStore) -> Result<HistoryMode> {
    let probe = || async {
        match tokio::time::timeout(VERSIONING_PROBE, store.object_versioning()).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow::Error::from(e)),
            Err(_) => Err(anyhow::anyhow!("the store did not answer within {VERSIONING_PROBE:?}")),
        }
    };
    match mode {
        HistoryMode::Records => Ok(HistoryMode::Records),
        HistoryMode::Versions => {
            let on = probe().await.context("config_store.history = \"versions\"")?;
            anyhow::ensure!(
                on,
                "config_store.history = \"versions\", but the config bucket keeps no object versions (enable bucket versioning, or use \"auto\"/\"records\")"
            );
            Ok(HistoryMode::Versions)
        }
        HistoryMode::Auto => match probe().await {
            Ok(true) => Ok(HistoryMode::Versions),
            Ok(false) => Ok(HistoryMode::Records),
            Err(e) => {
                tracing::warn!(error = %e, "config store: cannot tell whether the bucket keeps object versions; history is written as records");
                Ok(HistoryMode::Records)
            }
        },
    }
}

/// The document a reader may see: every sealed secret becomes `{"redacted": true}`.
pub fn redact(stored: &Value) -> Value {
    let mut out = stored.clone();
    for path in SECRET_PATHS {
        let mut cur = &mut out;
        let mut ok = true;
        for part in path.split('.') {
            match cur.get_mut(part) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && (cur.get("sealed").is_some() || cur.get("value").is_some()) {
            *cur = serde_json::json!({"redacted": true});
        }
    }
    out
}

/// A secret leaf as a diff may show it: env references as they are, anything
/// else as `"(secret)"`.
fn mask(path: &str, v: &Value) -> Value {
    if SECRET_PATHS.contains(&path) && v.get("env").is_none() && !v.is_null() {
        Value::String("(secret)".into())
    } else {
        v.clone()
    }
}

/// Changed leaves between two stored documents (secrets masked; a re-entered
/// secret shows as changed, a kept one does not).
pub fn diff(before: Option<&Value>, after: &Value) -> Vec<DiffEntry> {
    let empty = Value::Object(serde_json::Map::new());
    let a: BTreeMap<String, Value> = flatten(before.unwrap_or(&empty)).into_iter().collect();
    let b: BTreeMap<String, Value> = flatten(after).into_iter().collect();
    let mut out = Vec::new();
    for (path, new) in &b {
        match a.get(path) {
            None => out.push(DiffEntry {
                path: path.clone(),
                op: DiffOp::Added,
                old: None,
                new: Some(mask(path, new)),
            }),
            Some(old) if old != new => out.push(DiffEntry {
                path: path.clone(),
                op: DiffOp::Changed,
                old: Some(mask(path, old)),
                new: Some(mask(path, new)),
            }),
            Some(_) => {}
        }
    }
    for (path, old) in &a {
        if !b.contains_key(path) {
            out.push(DiffEntry {
                path: path.clone(),
                op: DiffOp::Removed,
                old: Some(mask(path, old)),
                new: None,
            });
        }
    }
    out
}

/// Restart-only paths a diff touches.
pub fn restart_paths(diff: &[DiffEntry]) -> Vec<String> {
    let mut out: Vec<String> = diff
        .iter()
        .filter(|d| {
            RESTART_ONLY
                .iter()
                .any(|r| d.path == *r || d.path.starts_with(&format!("{r}.")))
        })
        .map(|d| d.path.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests;
