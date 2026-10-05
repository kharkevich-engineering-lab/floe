//! The reconciler's durable memory (§B.6): `mirror/<kind>/state.json`, CAS'd by
//! version. Only the lease holder writes it, so the CAS is a safety net against a
//! lost lease, not a contention point: a write whose stored `generation` is not
//! the one this pass read aborts the pass.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use floe_store::ObjectStore;
use floe_store::coord::{CoordError, cas_update_json, get_json};
use serde::{Deserialize, Serialize};

use crate::source::RemoteRepo;

pub const STATE_VERSION: u32 = 1;

/// `mirror/<kind>/state.json`.
pub fn key(kind: &str) -> String {
    format!("mirror/{kind}/state.json")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MirrorState {
    pub version: u32,
    /// Bumped by every write; the CAS guard.
    pub generation: u64,
    pub updated_at: Option<DateTime<Utc>>,
    /// Instance id of the last writer.
    pub holder: String,
    pub token_login: Option<String>,
    pub last_pass: Option<LastPass>,
    /// By forge id.
    pub repos: BTreeMap<String, RepoEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastPass {
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub complete: bool,
    pub created: u32,
    pub updated: u32,
    pub errors: u32,
}

/// No status is terminal (§B.9 lists the way back out of each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Active,
    /// Selected but above `max_repo_size`: never auto-created (handoff recipe).
    TooLarge,
    /// No longer selected; frozen (`follow = []`).
    Excluded,
    /// 404 for `gone_after`; frozen.
    Gone,
    /// 401/403 for `gone_after`; frozen.
    Forbidden,
    /// Every candidate name is taken by a repository that is not ours.
    Conflict,
    /// A human edited `[upstream]`: never touched again until re-adopted.
    Detached,
    /// The last action failed; retried every pass.
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::TooLarge => "too-large",
            Status::Excluded => "excluded",
            Status::Gone => "gone",
            Status::Forbidden => "forbidden",
            Status::Conflict => "conflict",
            Status::Detached => "detached",
            Status::Error => "error",
        }
    }

    /// Frozen statuses publish `follow = []` (data kept, follow stopped).
    pub fn frozen(self) -> bool {
        matches!(self, Status::Excluded | Status::Gone | Status::Forbidden)
    }

    pub const ALL: [Status; 8] = [
        Status::Active,
        Status::TooLarge,
        Status::Excluded,
        Status::Gone,
        Status::Forbidden,
        Status::Conflict,
        Status::Detached,
        Status::Error,
    ];
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts the forge reports per repository"
)]
pub struct RepoEntry {
    /// `owner/name` as the forge spells it now (renames update it).
    pub full_name: String,
    /// The floe repository (`acme/widgets`), fixed at creation. `None` until created.
    pub floe: Option<String>,
    /// The floe repository this mirror is about to create, saved in the state
    /// before the manifest CAS-create: an empty, never-written repository of
    /// exactly this name is then the mirror's own (a crash between the create and
    /// the first publish) and is adopted. Nothing else that exists without the
    /// mirror's marker ever is (§B.7.1). Cleared once claimed or in conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claiming: Option<String>,
    pub status: Status,
    pub private: bool,
    pub archived: bool,
    pub fork: bool,
    pub default_branch: Option<String>,
    pub pushed_at: Option<String>,
    pub size_kb: u64,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    /// First complete discovery that lacked the id; cleared when seen selected again.
    pub missing_since: Option<DateTime<Utc>>,
    pub last_lookup: Option<DateTime<Utc>>,
    /// sha256 of the canonical `[upstream]` table the mirror last published.
    pub settings_sha: Option<String>,
    pub settings_revision: u64,
    /// The read-only `policy.json` is known to be in place (set only after a
    /// successful write; until then every pass plans it again when `read_only`).
    #[serde(default)]
    pub policy: bool,
    pub last_error: Option<String>,
}

impl RepoEntry {
    /// The forge facts a [`RemoteRepo`] carries, for an entry the forge did not
    /// report this pass (renders its frozen table).
    pub fn to_remote(&self, id: &str) -> RemoteRepo {
        let (owner, name) = self
            .full_name
            .split_once('/')
            .unwrap_or((self.full_name.as_str(), ""));
        RemoteRepo {
            id: id.to_string(),
            owner: owner.to_string(),
            name: name.to_string(),
            private: self.private,
            archived: self.archived,
            fork: self.fork,
            disabled: false,
            default_branch: self.default_branch.clone(),
            pushed_at: self.pushed_at.clone(),
            size_kb: self.size_kb,
        }
    }

    /// Record what the forge reports now (does not touch status or floe facts).
    pub fn observe(&mut self, r: &RemoteRepo, now: DateTime<Utc>) {
        self.full_name = r.full_name();
        self.private = r.private;
        self.archived = r.archived;
        self.fork = r.fork;
        self.default_branch.clone_from(&r.default_branch);
        self.pushed_at.clone_from(&r.pushed_at);
        self.size_kb = r.size_kb;
        self.first_seen.get_or_insert(now);
        self.last_seen = Some(now);
    }
}

/// Read the state (absent = empty, generation 0).
pub async fn load(store: &dyn ObjectStore, kind: &str) -> Result<MirrorState, CoordError> {
    Ok(get_json::<MirrorState>(store, &key(kind))
        .await?
        .map_or_else(
            || MirrorState {
                version: STATE_VERSION,
                ..MirrorState::default()
            },
            |(_, s)| s,
        ))
}

/// Write `state` if the stored generation is still `state.generation` (or the
/// object is absent and it is 0); bumps the generation on success. Anything else
/// is [`CoordError::Aborted`]: another writer holds the state now.
pub async fn save(
    store: &dyn ObjectStore,
    kind: &str,
    state: &mut MirrorState,
    holder: &str,
) -> Result<(), CoordError> {
    let expected = state.generation;
    let mut next = state.clone();
    next.version = STATE_VERSION;
    next.generation = expected + 1;
    next.updated_at = Some(Utc::now());
    next.holder = holder.to_string();
    let written = cas_update_json::<MirrorState, _>(store, &key(kind), 3, |cur| {
        let stored = cur.map_or(0, |c| c.generation);
        if stored == expected {
            Ok(Some(next.clone()))
        } else {
            Err(CoordError::Aborted)
        }
    })
    .await?;
    if let Some((_, s)) = written {
        *state = s;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use floe_store::memory::MemoryStore;

    #[tokio::test]
    async fn save_is_guarded_by_generation() {
        let store = MemoryStore::shared();
        let mut a = load(store.as_ref(), "github").await.unwrap();
        assert_eq!(a.generation, 0);
        a.repos.insert("1".into(), RepoEntry::default());
        save(store.as_ref(), "github", &mut a, "h1").await.unwrap();
        assert_eq!(a.generation, 1);
        let mut b = load(store.as_ref(), "github").await.unwrap();
        assert_eq!(b, a);
        save(store.as_ref(), "github", &mut b, "h2").await.unwrap();
        // `a` is now stale.
        assert!(matches!(
            save(store.as_ref(), "github", &mut a, "h1").await,
            Err(CoordError::Aborted)
        ));
        let json = serde_json::to_string(&Status::TooLarge).unwrap();
        assert_eq!(json, "\"too-large\"");
    }
}
