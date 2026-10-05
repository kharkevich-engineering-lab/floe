//! DNS-01 plumbing: the provider trait (Cloudflare today; Route 53 and RFC 2136 slot in
//! behind it), and the propagation check that asks nameservers for the challenge record
//! before the CA is told to look.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hickory_resolver::Resolver;
use hickory_resolver::config::{NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::RData;

/// A TXT record a provider created, with whatever it needs to delete exactly that record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecord {
    /// `_acme-challenge.<domain>` without a trailing dot.
    pub fqdn: String,
    pub value: String,
    /// Provider record id.
    pub id: String,
    /// Provider zone id.
    pub zone: String,
}

/// Where `_acme-challenge` TXT records are written. One implementation per DNS host; an
/// implementation never logs its credential.
#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Short name for logs and status (`cloudflare`).
    fn name(&self) -> &'static str;
    /// Create TXT `fqdn` = `value` (adding to, never replacing, other values at that name:
    /// a wildcard and its base share `_acme-challenge.<base>`).
    async fn create_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord>;
    /// Delete exactly `record`. Already gone is success.
    async fn delete_txt(&self, record: &TxtRecord) -> Result<()>;
}

/// The DNS-01 record name for an identifier: `*.example.com` and `example.com` both validate
/// at `_acme-challenge.example.com` (RFC 8555 §8.4).
pub fn challenge_name(identifier: &str) -> String {
    format!(
        "_acme-challenge.{}",
        identifier.strip_prefix("*.").unwrap_or(identifier)
    )
}

/// Waits until every nameserver answers the expected TXT values.
#[derive(Debug, Clone)]
pub struct Propagation {
    /// Explicit resolvers (`server.tls.acme.resolvers`); empty = authoritative nameservers.
    pub resolvers: Vec<SocketAddr>,
    pub timeout: Duration,
    /// Pause between rounds.
    pub interval: Duration,
}

impl Propagation {
    pub fn new(resolvers: &[String], timeout: Duration) -> Result<Self> {
        Ok(Propagation {
            resolvers: resolvers
                .iter()
                .map(|r| r.parse().with_context(|| format!("resolver {r:?}")))
                .collect::<Result<_>>()?,
            timeout,
            interval: Duration::from_secs(5),
        })
    }

    /// The nameservers to ask about `fqdn`: the configured resolvers, else the authoritative
    /// nameservers of the closest enclosing zone (NS lookup walking up the labels through the
    /// system resolver), else Cloudflare's and Google's public resolvers.
    pub async fn nameservers(&self, fqdn: &str) -> Vec<SocketAddr> {
        if !self.resolvers.is_empty() {
            return self.resolvers.clone();
        }
        match authoritative(fqdn).await {
            Ok(v) if !v.is_empty() => v,
            Ok(_) | Err(_) => {
                tracing::warn!(%fqdn, "could not find the zone's authoritative nameservers; checking propagation through public resolvers");
                vec![
                    SocketAddr::from(([1, 1, 1, 1], 53)),
                    SocketAddr::from(([8, 8, 8, 8], 53)),
                ]
            }
        }
    }

    /// Poll until each of `nameservers` returns every value in `want` for TXT `fqdn`.
    pub async fn wait(
        &self,
        fqdn: &str,
        want: &[String],
        nameservers: &[SocketAddr],
        mut on_round: impl FnMut(usize, usize),
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let resolvers: Vec<(SocketAddr, Resolver<TokioRuntimeProvider>)> = nameservers
            .iter()
            .map(|ns| Ok((*ns, direct_resolver(*ns)?)))
            .collect::<Result<_>>()?;
        loop {
            let mut ok = 0usize;
            for (_, r) in &resolvers {
                r.clear_cache();
                let have = txt_values(r, fqdn).await;
                if want.iter().all(|w| have.iter().any(|h| h == w)) {
                    ok += 1;
                }
            }
            on_round(ok, resolvers.len());
            if ok == resolvers.len() {
                return Ok(());
            }
            if tokio::time::Instant::now() + self.interval > deadline {
                anyhow::bail!(
                    "TXT {fqdn} not visible on {}/{} nameservers ({}) after {:?}",
                    resolvers.len() - ok,
                    resolvers.len(),
                    nameservers
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    self.timeout
                );
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

fn direct_resolver(ns: SocketAddr) -> Result<Resolver<TokioRuntimeProvider>> {
    let mut cfg = NameServerConfig::udp_and_tcp(ns.ip());
    for c in &mut cfg.connections {
        c.port = ns.port();
    }
    let mut b = Resolver::builder_with_config(
        ResolverConfig::from_name_servers(vec![cfg]),
        TokioRuntimeProvider::default(),
    );
    let o = b.options_mut();
    o.timeout = Duration::from_secs(3);
    o.attempts = 1;
    b.build().context("building DNS resolver")
}

async fn txt_values(r: &Resolver<TokioRuntimeProvider>, fqdn: &str) -> Vec<String> {
    match r.txt_lookup(format!("{fqdn}.")).await {
        Ok(l) => l
            .answers()
            .iter()
            .filter_map(|rec| match &rec.data {
                RData::TXT(t) => Some(
                    t.txt_data
                        .iter()
                        .map(|c| String::from_utf8_lossy(c).into_owned())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The authoritative nameservers (ip:53) of the zone enclosing `fqdn`.
async fn authoritative(fqdn: &str) -> Result<Vec<SocketAddr>> {
    let system = Resolver::builder_tokio()
        .context("system resolver")?
        .build()
        .context("system resolver")?;
    let labels: Vec<&str> = fqdn.trim_end_matches('.').split('.').collect();
    for i in 0..labels.len().saturating_sub(1) {
        let zone = labels.get(i..).unwrap_or_default().join(".");
        let Ok(ns) = system.ns_lookup(format!("{zone}.")).await else {
            continue;
        };
        let names: Vec<String> = ns
            .answers()
            .iter()
            .filter_map(|rec| match &rec.data {
                RData::NS(n) => Some(n.0.to_string()),
                _ => None,
            })
            .collect();
        if names.is_empty() {
            continue;
        }
        let mut out = Vec::new();
        for n in names {
            if let Ok(ips) = system.lookup_ip(n.as_str()).await {
                out.extend(ips.iter().map(|ip| SocketAddr::new(ip, 53)));
            }
        }
        tracing::debug!(%zone, nameservers = ?out, "authoritative nameservers for the challenge");
        return Ok(out);
    }
    anyhow::bail!("no NS records above {fqdn}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_and_base_share_the_challenge_name() {
        assert_eq!(
            challenge_name("*.git.example.com"),
            "_acme-challenge.git.example.com"
        );
        assert_eq!(
            challenge_name("git.example.com"),
            "_acme-challenge.git.example.com"
        );
    }

    #[test]
    fn explicit_resolvers_parse() {
        let p = Propagation::new(&["127.0.0.1:8053".into()], Duration::from_secs(10)).unwrap();
        assert_eq!(p.resolvers, vec![SocketAddr::from(([127, 0, 0, 1], 8053))]);
        assert!(Propagation::new(&["nope".into()], Duration::from_secs(10)).is_err());
    }
}
