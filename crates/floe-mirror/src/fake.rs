//! A scripted [`Source`] for tests (this crate's, and others' with the
//! `testing` feature): set what the next discovery and lookups answer.

use std::collections::BTreeMap;

use parking_lot::Mutex;

use crate::http_cache::HttpCache;
use crate::source::{Discovery, Lookup, RemoteRepo, Selection, Source, SourceError};

pub struct FakeSource {
    git: String,
    discovery: Mutex<Result<Discovery, String>>,
    lookups: Mutex<BTreeMap<String, Lookup>>,
    /// Ids looked up, in order.
    pub looked_up: Mutex<Vec<String>>,
}

impl FakeSource {
    /// `git` is the base of clone URLs (`https://github.com`, or a test floe).
    pub fn new(git: &str) -> Self {
        FakeSource {
            git: git.trim_end_matches('/').to_string(),
            discovery: Mutex::new(Ok(Discovery {
                complete: true,
                ..Discovery::default()
            })),
            lookups: Mutex::new(BTreeMap::new()),
            looked_up: Mutex::new(Vec::new()),
        }
    }

    /// What every following discovery returns.
    pub fn set_repos(&self, repos: Vec<RemoteRepo>, complete: bool) {
        *self.discovery.lock() = Ok(Discovery {
            repos,
            complete,
            login: Some("me".into()),
            ..Discovery::default()
        });
    }

    /// Make every following discovery fail as unauthorized.
    pub fn fail(&self, why: &str) {
        *self.discovery.lock() = Err(why.to_string());
    }

    pub fn set_lookup(&self, id: &str, l: Lookup) {
        self.lookups.lock().insert(id.to_string(), l);
    }
}

#[async_trait::async_trait]
impl Source for FakeSource {
    fn kind(&self) -> &'static str {
        "github"
    }

    async fn discover(
        &self,
        _sel: &Selection,
        _cache: &mut HttpCache,
    ) -> Result<Discovery, SourceError> {
        self.discovery
            .lock()
            .clone()
            .map_err(|_| SourceError::Unauthorized)
    }

    async fn lookup(&self, id: &str, _cache: &mut HttpCache) -> Result<Lookup, SourceError> {
        self.looked_up.lock().push(id.to_string());
        Ok(self.lookups.lock().get(id).cloned().unwrap_or(Lookup::Gone))
    }

    fn git_url(&self, r: &RemoteRepo) -> String {
        format!("{}/{}/{}.git", self.git, r.owner, r.name)
    }

    fn lfs_url(&self, r: &RemoteRepo) -> Option<String> {
        Some(format!("{}/info/lfs", self.git_url(r)))
    }
}

/// A plain public repository with a `main` branch.
pub fn remote(id: &str, owner: &str, name: &str) -> RemoteRepo {
    RemoteRepo {
        id: id.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
        private: false,
        archived: false,
        fork: false,
        disabled: false,
        default_branch: Some("main".into()),
        pushed_at: Some("2026-10-01T00:00:00Z".into()),
        size_kb: 1,
    }
}
