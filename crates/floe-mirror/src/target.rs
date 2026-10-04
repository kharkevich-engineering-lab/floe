//! The floe side of the mirror (§B.7), a trait so the planner and applier test
//! without a server. [`WalTarget`] is the real one, over a [`Registry`]: the same
//! manifest CAS create, SETTINGS entry and `policy.json` every other writer uses.
//! No new commit point.

use std::sync::Arc;

use floe_git::RepoId;
use floe_store::{DynStore, ObjectStoreExt, PutMode, StoreError};
use floe_wal::{Registry, WalError};

use crate::settings;

/// What the mirror needs to know about an existing floe repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingRepo {
    /// The settings document (empty when none).
    pub settings_toml: String,
    /// 0 when none.
    pub settings_revision: u64,
    pub settings_author: String,
    /// `upstream.source`.
    pub source: Option<String>,
    /// 0 = never written (no settings, no refs).
    pub head_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOutcome {
    Created,
    AlreadyExists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Published(u64),
    /// The settings revision moved since `exists` read it.
    Conflict,
}

#[async_trait::async_trait]
pub trait Target: Send + Sync {
    async fn exists(&self, id: &RepoId) -> anyhow::Result<Option<ExistingRepo>>;
    async fn create(&self, id: &RepoId) -> anyhow::Result<CreateOutcome>;
    /// Replace only `[upstream]` in the settings document, as a CAS on
    /// `expected_revision` (author [`settings::AUTHOR`]).
    async fn publish_upstream(
        &self,
        id: &RepoId,
        upstream: &toml::Table,
        expected_revision: u64,
        reason: &str,
    ) -> anyhow::Result<PublishOutcome>;
    /// Write `policy.json` unless one exists (a human's file is never overwritten).
    async fn put_policy_if_absent(&self, id: &RepoId, policy_json: &[u8]) -> anyhow::Result<()>;
    /// Ask the maintaining host to follow this repository now. Best effort; a
    /// no-op where unsupported (the CLI).
    fn nudge_follow(&self, id: &RepoId);
}

/// A nudge: the server's placement-gated `ops::start(.., "follow", ..)`.
pub type Nudge = Box<dyn Fn(&RepoId) + Send + Sync>;

/// [`Target`] over the WAL registry (server and CLI).
pub struct WalTarget {
    registry: Arc<Registry>,
    store: DynStore,
    nudge: Nudge,
}

impl WalTarget {
    pub fn new(registry: Arc<Registry>, store: DynStore, nudge: Nudge) -> Self {
        WalTarget {
            registry,
            store,
            nudge,
        }
    }
}

#[async_trait::async_trait]
impl Target for WalTarget {
    async fn exists(&self, id: &RepoId) -> anyhow::Result<Option<ExistingRepo>> {
        let handle = match self.registry.open(id).await {
            Ok(h) => h,
            Err(WalError::NotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Settings ride on the manifest: refs level, no pack I/O, no prefetch.
        drop(handle.sync_refs_only().await?);
        let manifest = handle.manifest();
        let s = handle.settings().unwrap_or_default();
        Ok(Some(ExistingRepo {
            source: settings::source_of(&s.toml),
            settings_toml: s.toml,
            settings_revision: s.revision,
            settings_author: s.author,
            head_seq: manifest.head_seq,
        }))
    }

    async fn create(&self, id: &RepoId) -> anyhow::Result<CreateOutcome> {
        // GitHub is SHA-1; `git.object_format` is ignored on purpose.
        match self.registry.create(id, floe_git::ObjectFormat::Sha1).await {
            Ok(_) => Ok(CreateOutcome::Created),
            Err(WalError::AlreadyExists) => Ok(CreateOutcome::AlreadyExists),
            Err(e) => Err(e.into()),
        }
    }

    async fn publish_upstream(
        &self,
        id: &RepoId,
        upstream: &toml::Table,
        expected_revision: u64,
        reason: &str,
    ) -> anyhow::Result<PublishOutcome> {
        let handle = self.registry.open(id).await?;
        drop(handle.sync_refs_only().await?);
        let current = handle.settings().unwrap_or_default();
        let text = settings::merge(&current.toml, upstream)?;
        let message = format!("floe github mirror: {reason}");
        match handle
            .publish_settings_if(&text, settings::AUTHOR, &message, expected_revision)
            .await
        {
            Ok(rev) => Ok(PublishOutcome::Published(rev)),
            Err(WalError::SettingsConflict { .. }) => Ok(PublishOutcome::Conflict),
            Err(e) => Err(e.into()),
        }
    }

    async fn put_policy_if_absent(&self, id: &RepoId, policy_json: &[u8]) -> anyhow::Result<()> {
        let key = floe_proto::keys::policy_key(id.owner(), id.name());
        if self.store.get_bytes(&key).await?.is_some() {
            return Ok(());
        }
        match self
            .store
            .put_bytes(&key, policy_json.to_vec(), PutMode::Create)
            .await
        {
            Ok(_) | Err(StoreError::PreconditionFailed { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn nudge_follow(&self, id: &RepoId) {
        (self.nudge)(id);
    }
}

/// §B.7.2: deny every ref change; follow bypasses policy (D33), so only follow
/// moves refs. A push would otherwise be undone (and archived) by the next round.
pub const READ_ONLY_POLICY: &str = r#"{
  "version": 1,
  "rules": [
    {
      "name": "github-mirror-read-only",
      "_comment": "managed by floe github mirror; follow bypasses policy (D33)",
      "match": { "refs": ["refs/**"] },
      "effect": { "protect": { "restricts": ["create", "update", "delete", "force-push"] } }
    }
  ]
}
"#;
