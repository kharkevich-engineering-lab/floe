//! [`GithubSource`]: the GitHub (and GitHub Enterprise Server) implementation
//! of [`Source`] over the REST API (§B.11). The token is read from the env var
//! `github_mirror.token` (through `token_env`, D61) at every pass (rotation needs no restart) and only
//! ever travels in the `Authorization` header.

pub mod http;
pub mod model;

use std::collections::BTreeMap;

use floe_config::GithubMirrorConfig;

use crate::http_cache::HttpCache;
use crate::source::{ApiStats, Discovery, Lookup, RemoteRepo, Selection, Source, SourceError};
use http::{Fetched, GithubHttp};
use model::{Repo, User};

pub struct GithubSource {
    api: String,
    git: String,
    token_env: String,
    http: GithubHttp,
    /// Tests only: a token that is not in the environment.
    #[cfg(test)]
    fixed_token: Option<String>,
}

impl GithubSource {
    pub fn new(cfg: &GithubMirrorConfig) -> Result<Self, SourceError> {
        Ok(GithubSource {
            api: cfg.api_url.trim_end_matches('/').to_string(),
            git: cfg.git_url.trim_end_matches('/').to_string(),
            token_env: cfg.token_env.clone(),
            http: GithubHttp::new(cfg.min_rate_remaining)?,
            #[cfg(test)]
            fixed_token: None,
        })
    }

    fn token(&self) -> Result<String, SourceError> {
        #[cfg(test)]
        if let Some(t) = &self.fixed_token {
            return Ok(t.clone());
        }
        // D61: the config store's alias first (a token entered in the GUI), then the env.
        match floe_config::secret::env_var(&self.token_env) {
            Some(t) => Ok(t.trim().to_string()),
            None => Err(SourceError::NoToken(self.token_env.clone())),
        }
    }

    /// Whether `url` is under `api_url` (the only place the token is sent).
    fn on_api(&self, url: &str) -> bool {
        url.strip_prefix(self.api.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// The listing URLs a selection names (first pages).
    fn listings(&self, sel: &Selection, login: Option<&str>) -> Vec<String> {
        let api = &self.api;
        let is_me = |u: &str| u == "@me" || login.is_some_and(|l| l.eq_ignore_ascii_case(u));
        let mut urls = Vec::new();
        for u in &sel.users {
            if is_me(u) {
                urls.push(format!(
                    "{api}/user/repos?affiliation=owner&visibility=all&per_page=100&sort=full_name"
                ));
            } else {
                urls.push(format!(
                    "{api}/users/{u}/repos?type=owner&per_page=100&sort=full_name"
                ));
            }
        }
        for o in &sel.orgs {
            urls.push(format!(
                "{api}/orgs/{o}/repos?type=all&per_page=100&sort=full_name"
            ));
        }
        for u in &sel.starred {
            if is_me(u) {
                urls.push(format!("{api}/user/starred?per_page=100"));
            } else {
                urls.push(format!("{api}/users/{u}/starred?per_page=100"));
            }
        }
        urls.sort();
        urls.dedup();
        urls
    }
}

/// How one listing ended.
enum Listing {
    Whole,
    /// Cut short or failed; the discovery is incomplete but goes on.
    Partial,
    /// Rate limited: stop the whole discovery (incomplete).
    Stop,
}

#[async_trait::async_trait]
impl Source for GithubSource {
    fn kind(&self) -> &'static str {
        "github"
    }

    async fn discover(
        &self,
        sel: &Selection,
        cache: &mut HttpCache,
    ) -> Result<Discovery, SourceError> {
        let token = self.token()?;
        let mut stats = ApiStats::default();
        let mut complete = true;
        let mut stopped = false;
        let mut repos: BTreeMap<String, RemoteRepo> = BTreeMap::new();

        // The token's identity: `@me`, and a sanity check (401 fails the pass).
        let login = match self
            .http
            .get(
                &token,
                &format!("{}/user", self.api),
                cache,
                &mut stats,
                model::project::<User>,
            )
            .await
        {
            Ok(Fetched::Ok { body, .. }) => Some(model::decode::<User>(body)?.login),
            Ok(_) => None,
            Err(SourceError::RateLimited { .. }) => {
                return Ok(Discovery {
                    repos: Vec::new(),
                    complete: false,
                    stats,
                    login: None,
                });
            }
            Err(e) => return Err(e),
        };

        'listings: for first in self.listings(sel, login.as_deref()) {
            let mut url = Some(first);
            while let Some(u) = url.take() {
                let outcome = match self
                    .http
                    .get(&token, &u, cache, &mut stats, model::project::<Vec<Repo>>)
                    .await
                {
                    Ok(Fetched::Ok { body, next }) => {
                        for r in model::decode::<Vec<Repo>>(body)? {
                            let r = RemoteRepo::from(r);
                            repos.insert(r.id.clone(), r);
                        }
                        match next {
                            // The token only ever goes to `api_url` (a `Link`
                            // or a stale cached `next` may name another host).
                            Some(n) if !self.on_api(&n) => {
                                tracing::warn!(url = %u, next = %n, "github listing's next page is not under api_url; listing cut short");
                                Listing::Partial
                            }
                            n => {
                                url = n;
                                Listing::Whole
                            }
                        }
                    }
                    Ok(Fetched::NotFound) => {
                        tracing::warn!(url = %u, "github listing not found (no such user or organisation)");
                        Listing::Whole
                    }
                    Ok(Fetched::Forbidden(msg)) => {
                        tracing::warn!(url = %u, message = %msg, "github listing forbidden");
                        Listing::Partial
                    }
                    Err(SourceError::RateLimited { .. }) => Listing::Stop,
                    Err(e @ (SourceError::Unauthorized | SourceError::NoToken(_))) => {
                        return Err(e);
                    }
                    Err(e) => {
                        tracing::warn!(url = %u, error = %e, "github listing failed");
                        Listing::Partial
                    }
                };
                match outcome {
                    Listing::Whole => {}
                    Listing::Partial => {
                        complete = false;
                        break;
                    }
                    Listing::Stop => {
                        complete = false;
                        stopped = true;
                        break 'listings;
                    }
                }
            }
        }

        if !stopped {
            for full in &sel.repos {
                let url = format!("{}/repos/{full}", self.api);
                match self
                    .http
                    .get(&token, &url, cache, &mut stats, model::project::<Repo>)
                    .await
                {
                    Ok(Fetched::Ok { body, .. }) => {
                        let r = RemoteRepo::from(model::decode::<Repo>(body)?);
                        repos.insert(r.id.clone(), r);
                    }
                    // Absent: a lookup by id decides what happened to it.
                    Ok(Fetched::NotFound | Fetched::Forbidden(_)) => {}
                    Err(SourceError::RateLimited { .. }) => {
                        complete = false;
                        break;
                    }
                    Err(e @ (SourceError::Unauthorized | SourceError::NoToken(_))) => {
                        return Err(e);
                    }
                    Err(e) => {
                        tracing::warn!(repo = %full, error = %e, "github repository fetch failed");
                        complete = false;
                    }
                }
            }
        }

        Ok(Discovery {
            repos: repos.into_values().collect(),
            complete,
            stats,
            login,
        })
    }

    async fn lookup(&self, id: &str, cache: &mut HttpCache) -> Result<Lookup, SourceError> {
        let token = self.token()?;
        let mut stats = ApiStats::default();
        // By id: follows renames and transfers.
        let url = format!("{}/repositories/{id}", self.api);
        match self
            .http
            .get(&token, &url, cache, &mut stats, model::project::<Repo>)
            .await?
        {
            Fetched::Ok { body, .. } => Ok(Lookup::Found(model::decode::<Repo>(body)?.into())),
            Fetched::NotFound => Ok(Lookup::Gone),
            Fetched::Forbidden(_) => Ok(Lookup::Forbidden),
        }
    }

    fn git_url(&self, r: &RemoteRepo) -> String {
        format!("{}/{}/{}.git", self.git, r.owner, r.name)
    }

    fn lfs_url(&self, r: &RemoteRepo) -> Option<String> {
        Some(format!("{}/info/lfs", self.git_url(r)))
    }
}

#[cfg(test)]
mod tests;
