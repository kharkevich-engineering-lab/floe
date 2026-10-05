//! The REST payloads the mirror reads, only the fields it uses. Cached bodies
//! are these projections (re-serialized), so the cache never holds a field the
//! mirror does not need.

use serde::{Deserialize, Serialize};

use crate::source::{RemoteRepo, SourceError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // GitHub's flags, as GitHub sends them
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub owner: Owner,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub pushed_at: Option<String>,
    /// KiB.
    #[serde(default)]
    pub size: u64,
}

impl From<Repo> for RemoteRepo {
    fn from(r: Repo) -> Self {
        RemoteRepo {
            id: r.id.to_string(),
            owner: r.owner.login,
            name: r.name,
            private: r.private,
            archived: r.archived,
            fork: r.fork,
            disabled: r.disabled,
            default_branch: r.default_branch,
            pushed_at: r.pushed_at,
            size_kb: r.size,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub login: String,
}

/// Decode `v` as `T` and re-encode it: the projection that is cached.
pub fn project<T: Serialize + serde::de::DeserializeOwned>(
    v: serde_json::Value,
) -> Result<serde_json::Value, SourceError> {
    let t: T = serde_json::from_value(v).map_err(|e| SourceError::Decode(e.to_string()))?;
    serde_json::to_value(t).map_err(|e| SourceError::Decode(e.to_string()))
}

pub fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, SourceError> {
    serde_json::from_value(v).map_err(|e| SourceError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_only_used_fields() {
        let raw = serde_json::json!([{
            "id": 7, "name": "w", "owner": {"login": "Acme", "id": 1, "type": "Organization"},
            "private": true, "archived": false, "fork": false, "default_branch": "main",
            "pushed_at": "2026-10-04T11:58:01Z", "size": 5120, "full_name": "Acme/w",
            "permissions": {"admin": true}
        }]);
        let p = project::<Vec<Repo>>(raw).unwrap();
        let text = p.to_string();
        assert!(!text.contains("permissions"), "{text}");
        let repos: Vec<Repo> = decode(p).unwrap();
        let r: RemoteRepo = repos.into_iter().next().unwrap().into();
        assert_eq!(r.id, "7");
        assert_eq!(r.full_name(), "Acme/w");
        assert_eq!(r.size_kb, 5120);
    }
}
