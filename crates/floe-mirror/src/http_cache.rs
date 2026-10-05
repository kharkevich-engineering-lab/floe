//! Forge-neutral conditional-GET cache (§B.6): `url → {etag, next, body}`, kept
//! in the bucket at `mirror/<kind>/http-cache.json`. A **cache**: losing it costs
//! rate limit and nothing else (principle I), so it is overwritten without CAS
//! (`PutMode::Overwrite`) at the end of a pass and a body that does not decode is
//! treated as empty.

use std::collections::{BTreeMap, BTreeSet};

use floe_store::{ObjectStore, ObjectStoreExt, PutMode};
use serde::{Deserialize, Serialize};

/// One cached response: its validator, its `Link rel="next"` (a 304 does not
/// promise to repeat headers), and the projected body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub etag: String,
    pub next: Option<String>,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct HttpCache {
    entries: BTreeMap<String, CacheEntry>,
    touched: BTreeSet<String>,
    dirty: bool,
}

impl HttpCache {
    /// `mirror/<kind>/http-cache.json`.
    pub fn key(kind: &str) -> String {
        format!("mirror/{kind}/http-cache.json")
    }

    pub fn get(&mut self, url: &str) -> Option<&CacheEntry> {
        self.touched.insert(url.to_string());
        self.entries.get(url)
    }

    pub fn put(&mut self, url: &str, entry: CacheEntry) {
        self.touched.insert(url.to_string());
        if self.entries.get(url) != Some(&entry) {
            self.entries.insert(url.to_string(), entry);
            self.dirty = true;
        }
    }

    pub fn remove(&mut self, url: &str) {
        if self.entries.remove(url).is_some() {
            self.dirty = true;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop every entry this pass did not ask for (after a complete discovery:
    /// listings that are no longer configured, lookups of resolved ids).
    pub fn retain_touched(&mut self) {
        let before = self.entries.len();
        let touched = &self.touched;
        self.entries.retain(|k, _| touched.contains(k));
        self.dirty |= self.entries.len() != before;
    }

    /// Load from the bucket; any failure is an empty cache (warmth only).
    pub async fn load(store: &dyn ObjectStore, kind: &str) -> HttpCache {
        let key = Self::key(kind);
        let entries = match store.get_bytes(&key).await {
            Ok(Some((_, bytes))) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(key, error = %e, "http cache does not decode; starting empty");
                BTreeMap::new()
            }),
            Ok(None) => BTreeMap::new(),
            Err(e) => {
                tracing::warn!(key, error = %e, "http cache unreadable; starting empty");
                BTreeMap::new()
            }
        };
        HttpCache {
            entries,
            touched: BTreeSet::new(),
            dirty: false,
        }
    }

    /// Write back when changed (`PutMode::Overwrite`; best effort).
    pub async fn save(&mut self, store: &dyn ObjectStore, kind: &str) {
        if !self.dirty {
            return;
        }
        let key = Self::key(kind);
        match serde_json::to_vec(&self.entries) {
            Ok(body) => match store.put_bytes(&key, body, PutMode::Overwrite).await {
                Ok(_) => self.dirty = false,
                Err(e) => tracing::warn!(key, error = %e, "http cache not saved"),
            },
            Err(e) => tracing::warn!(key, error = %e, "http cache not encoded"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floe_store::memory::MemoryStore;

    #[tokio::test]
    async fn round_trips_and_prunes_untouched() {
        let store = MemoryStore::shared();
        let mut c = HttpCache::load(store.as_ref(), "github").await;
        assert!(c.is_empty());
        let e = |etag: &str| CacheEntry {
            etag: etag.into(),
            next: None,
            body: serde_json::json!([1]),
        };
        c.put("https://a/1", e("x"));
        c.put("https://a/2", e("y"));
        c.save(store.as_ref(), "github").await;
        let mut c = HttpCache::load(store.as_ref(), "github").await;
        assert_eq!(c.len(), 2);
        assert_eq!(c.get("https://a/1").map(|e| e.etag.as_str()), Some("x"));
        c.retain_touched();
        c.save(store.as_ref(), "github").await;
        let c = HttpCache::load(store.as_ref(), "github").await;
        assert_eq!(c.len(), 1);
        // Garbage in the bucket is an empty cache, not an error.
        store
            .put_bytes(&HttpCache::key("github"), b"{".to_vec(), PutMode::Overwrite)
            .await
            .unwrap();
        assert!(HttpCache::load(store.as_ref(), "github").await.is_empty());
    }
}
