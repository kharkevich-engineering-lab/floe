//! Cloudflare as the DNS-01 provider: API v4, a token scoped to `Zone:DNS:Edit` (plus
//! `Zone:Read` when the zone is looked up rather than configured).
//!
//! * zone: `server.tls.acme.cloudflare.zone_id`, else `GET /zones?name=<candidate>` walking
//!   from the record's parent towards the apex (cached for the process);
//! * create: `POST /zones/{zone}/dns_records` `{type: TXT, name, content, ttl: 60}`;
//! * delete: `DELETE /zones/{zone}/dns_records/{id}` (404 = already gone).
//!
//! The token is sent only as `Authorization: Bearer` and never appears in an error or a log.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::dns::{DnsProvider, TxtRecord};

pub struct Cloudflare {
    http: reqwest::Client,
    api: String,
    token: String,
    zone_id: Option<String>,
    /// record parent name → zone id.
    zones: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for Cloudflare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cloudflare")
            .field("api", &self.api)
            .field("zone_id", &self.zone_id)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiMessage>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct ApiMessage {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct Zone {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Record {
    id: String,
}

impl Cloudflare {
    /// From `server.tls.acme.cloudflare`; the token is read from the env var it names.
    pub fn from_config(cfg: &floe_config::CloudflareConfig, http: reqwest::Client) -> Result<Self> {
        let var = &cfg.api_token_env;
        let token = std::env::var(var)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .with_context(|| {
                format!("{var} is not set: server.tls.acme.cloudflare.api_token_env names the env var with a Cloudflare API token scoped to Zone:DNS:Edit")
            })?;
        Ok(Self::new(&cfg.api_url, token.trim(), cfg.zone_id.clone(), http))
    }

    pub fn new(api: &str, token: &str, zone_id: Option<String>, http: reqwest::Client) -> Self {
        Cloudflare {
            http,
            api: api.trim_end_matches('/').to_string(),
            token: token.to_string(),
            zone_id,
            zones: Mutex::new(HashMap::new()),
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        what: &str,
        req: reqwest::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, Option<T>)> {
        let rsp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Cloudflare {what}: {}", e.without_url()))?;
        let status = rsp.status();
        let body = rsp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("Cloudflare {what}: {}", e.without_url()))?;
        let env: Envelope<T> = serde_json::from_slice(&body).map_err(|_| {
            anyhow::anyhow!("Cloudflare {what}: HTTP {status}, not an API v4 envelope")
        })?;
        if !env.success {
            let msgs = env
                .errors
                .iter()
                .map(|m| format!("{} {}", m.code, m.message))
                .collect::<Vec<_>>()
                .join("; ");
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok((status, None));
            }
            anyhow::bail!("Cloudflare {what}: HTTP {status}: {msgs}");
        }
        Ok((status, env.result))
    }

    /// The zone holding `fqdn`.
    pub async fn zone_for(&self, fqdn: &str) -> Result<String> {
        if let Some(z) = &self.zone_id {
            return Ok(z.clone());
        }
        let parent = fqdn.strip_prefix("_acme-challenge.").unwrap_or(fqdn).to_string();
        if let Some(z) = self.zones.lock().ok().and_then(|m| m.get(&parent).cloned()) {
            return Ok(z);
        }
        let labels: Vec<&str> = parent.split('.').collect();
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels.get(i..).unwrap_or_default().join(".");
            let req = self
                .http
                .get(format!("{}/zones", self.api))
                .query(&[("name", candidate.as_str()), ("status", "active")]);
            let (_, zones) = self.call::<Vec<Zone>>("zone lookup", req).await?;
            if let Some(z) = zones
                .unwrap_or_default()
                .into_iter()
                .find(|z| z.name.eq_ignore_ascii_case(&candidate))
            {
                if let Ok(mut m) = self.zones.lock() {
                    m.insert(parent.clone(), z.id.clone());
                }
                tracing::debug!(zone = %z.name, "Cloudflare zone for the challenge");
                return Ok(z.id);
            }
        }
        anyhow::bail!(
            "no active Cloudflare zone holds {parent} (does the token have Zone:Read? or set server.tls.acme.cloudflare.zone_id)"
        )
    }
}

#[async_trait]
impl DnsProvider for Cloudflare {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    async fn create_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord> {
        let zone = self.zone_for(fqdn).await?;
        let req = self
            .http
            .post(format!("{}/zones/{zone}/dns_records", self.api))
            .json(&serde_json::json!({
                "type": "TXT",
                "name": fqdn,
                "content": value,
                "ttl": 60,
                "comment": "floe ACME DNS-01 challenge (deleted after validation)",
            }));
        let (_, rec) = self.call::<Record>("TXT create", req).await?;
        let rec = rec.context("Cloudflare TXT create: no record in the answer")?;
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: rec.id,
            zone,
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> Result<()> {
        let req = self.http.delete(format!(
            "{}/zones/{}/dns_records/{}",
            self.api, record.zone, record.id
        ));
        self.call::<serde_json::Value>("TXT delete", req).await?;
        Ok(())
    }
}
