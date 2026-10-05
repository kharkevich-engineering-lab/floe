//! The forge side: what a source of repositories must answer (§B.3). GitHub
//! implements it ([`crate::github`]); GitLab/Gitea implement the same trait later.
//! Nothing here, nor in the planner, applier or state, names a forge: everything
//! per forge is keyed by [`Source::kind`].

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::http_cache::HttpCache;

/// A forge we can discover repositories on.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    /// `github` | `gitlab` | `gitea`: the prefix of `upstream.source`
    /// (`github:<id>`), the state/cache/lease key segment and the metric label.
    fn kind(&self) -> &'static str;
    /// Every repository the selection names, as visible to our credential.
    /// `complete = false` when any listing failed or was cut short (rate limit):
    /// the planner then never infers "gone" from absence.
    async fn discover(
        &self,
        sel: &Selection,
        cache: &mut HttpCache,
    ) -> Result<Discovery, SourceError>;
    /// One repository by its stable id (rename detection, gone/forbidden checks).
    async fn lookup(&self, id: &str, cache: &mut HttpCache) -> Result<Lookup, SourceError>;
    /// The git URL follow fetches (`upstream.git`).
    fn git_url(&self, r: &RemoteRepo) -> String;
    /// The LFS endpoint (`upstream.lfs`).
    fn lfs_url(&self, r: &RemoteRepo) -> Option<String>;
}

/// One repository as the forge reports it (only what the mirror uses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // independent forge flags, not a state machine
pub struct RemoteRepo {
    /// Stable across renames and transfers (GitHub's numeric id, as a string).
    pub id: String,
    /// As the forge spells it.
    pub owner: String,
    pub name: String,
    pub private: bool,
    pub archived: bool,
    pub fork: bool,
    pub disabled: bool,
    /// `None` for an empty repository.
    pub default_branch: Option<String>,
    /// RFC 3339; the change signal (a push moves it).
    pub pushed_at: Option<String>,
    pub size_kb: u64,
}

impl RemoteRepo {
    /// `owner/name` as the forge spells it.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// What to discover. Interpreted per forge: `orgs` are GitLab groups or Gitea
/// organisations; a forge without stars ignores `starred`. The union, not a
/// GitHub schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub users: Vec<String>,
    pub orgs: Vec<String>,
    pub starred: Vec<String>,
    /// Explicit `owner/name`.
    pub repos: Vec<String>,
}

/// The result of one discovery.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// Deduplicated by id.
    pub repos: Vec<RemoteRepo>,
    pub complete: bool,
    pub stats: ApiStats,
    /// The credential's own login, when the forge reported it.
    pub login: Option<String>,
}

/// One repository looked up by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found(RemoteRepo),
    /// 404: deleted, or private and no longer visible (indistinguishable, R12).
    Gone,
    /// 401/403 that is not a rate limit: access revoked.
    Forbidden,
}

/// API accounting for one pass (metrics, `sync_runs`, the loop's back-off).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiStats {
    pub requests: u32,
    pub not_modified: u32,
    pub rate_remaining: Option<u32>,
    /// Unix seconds.
    pub rate_reset: Option<i64>,
    /// Set when the pass stopped at a rate limit: the loop sleeps until then.
    pub rate_limited_until: Option<SystemTime>,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The credential's env var is unset or empty.
    #[error("no token: environment variable {0} is unset or empty")]
    NoToken(String),
    /// 401: the token is invalid or expired.
    #[error("unauthorized: the token was rejected")]
    Unauthorized,
    #[error("rate limited until {until:?}")]
    RateLimited { until: SystemTime },
    #[error("http: {0}")]
    Http(String),
    #[error("decode: {0}")]
    Decode(String),
}
