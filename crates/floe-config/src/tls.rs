//! `[server.tls]` — TLS terminated by floe itself (D39, D59).
//!
//! Three modes, nothing else:
//! * `off` — plain HTTP/1.1 + h2c: local development, or behind an edge that terminates TLS.
//! * `files` — a certificate chain and key the operator provides (`cert`, `key`).
//! * `acme` — Let's Encrypt (or any RFC 8555 directory) through the **DNS-01** challenge only,
//!   Cloudflare as the DNS provider; state (account key, chain, encrypted private key) lives in
//!   the bucket under `tls/` (principle I).
//!
//! TLS is bootstrap configuration (file + `FLOE__` env), never a bucket-stored document: the
//! listener needs it before the store is reachable.

use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Let's Encrypt production (the default directory).
pub const LETSENCRYPT_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt staging (`directory = "staging"`).
pub const LETSENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TlsConfig {
    pub mode: TlsMode,
    /// `files` mode: PEM certificate chain (leaf first).
    pub cert: Option<PathBuf>,
    /// `files` mode: PKCS#8 / PKCS#1 / SEC1 private key PEM.
    pub key: Option<PathBuf>,
    /// `acme` mode settings.
    pub acme: AcmeConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Plain HTTP/1.1 + h2c (local development, or behind an edge that terminates TLS).
    #[default]
    Off,
    /// `cert` + `key` from disk; reloaded when either file changes or on SIGHUP.
    Files,
    /// ACME (RFC 8555) through DNS-01; the certificate lives in the bucket.
    Acme,
}

impl Default for TlsConfig {
    fn default() -> Self {
        TlsConfig {
            mode: TlsMode::Off,
            cert: None,
            key: None,
            acme: AcmeConfig::default(),
        }
    }
}

/// The only challenge floe answers. HTTP-01 and TLS-ALPN-01 would need every instance behind
/// the name to answer for the token; DNS-01 needs one writer and covers wildcards.
pub const DNS01: &str = "dns-01";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AcmeConfig {
    /// Names on the certificate (SANs). `*.example.com` wildcards are allowed (DNS-01).
    pub domains: Vec<String>,
    /// ACME account contact (`mailto:`), required by Let's Encrypt for expiry notices.
    pub email: String,
    /// Directory URL; `"staging"` = Let's Encrypt staging. Default: Let's Encrypt production.
    pub directory: String,
    /// Renew when the certificate expires within this window.
    #[serde(with = "humantime_serde")]
    pub renew_before: Duration,
    /// Env var holding the 32-byte key (base64 or hex) that seals private keys in the bucket.
    pub storage_key_env: String,
    /// Must be `"dns-01"`; anything else is refused.
    pub challenge: String,
    /// DNS provider for the challenge record. Only `"cloudflare"` today.
    pub dns_provider: DnsProvider,
    /// How long to wait for the `_acme-challenge` TXT record to be visible before giving up.
    #[serde(with = "humantime_serde")]
    pub propagation_timeout: Duration,
    /// `ip:port` resolvers the propagation check asks. Empty = the zone's authoritative
    /// nameservers (found through the system resolver).
    pub resolvers: Vec<String>,
    /// How often every instance revalidates the certificate object in the bucket
    /// (a conditional GET) and the lease holder checks whether renewal is due.
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    pub cloudflare: CloudflareConfig,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        AcmeConfig {
            domains: vec![],
            email: String::new(),
            directory: LETSENCRYPT_PRODUCTION.to_string(),
            renew_before: Duration::from_hours(720),
            storage_key_env: "FLOE_TLS_STORAGE_KEY".to_string(),
            challenge: DNS01.to_string(),
            dns_provider: DnsProvider::Cloudflare,
            propagation_timeout: Duration::from_mins(5),
            resolvers: vec![],
            poll_interval: Duration::from_mins(10),
            cloudflare: CloudflareConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DnsProvider {
    #[default]
    Cloudflare,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CloudflareConfig {
    /// Env var holding a Cloudflare API token scoped to `Zone:DNS:Edit`.
    pub api_token_env: String,
    /// The zone; empty = looked up from the domain (needs `Zone:Read` on the token too).
    pub zone_id: Option<String>,
    /// API base (tests point it at a mock).
    pub api_url: String,
}

impl Default for CloudflareConfig {
    fn default() -> Self {
        CloudflareConfig {
            api_token_env: "CLOUDFLARE_API_TOKEN".to_string(),
            zone_id: None,
            api_url: "https://api.cloudflare.com/client/v4".to_string(),
        }
    }
}

impl AcmeConfig {
    /// The directory URL with the `staging` shorthand expanded.
    pub fn directory_url(&self) -> &str {
        match self.directory.as_str() {
            "staging" => LETSENCRYPT_STAGING,
            "" | "production" => LETSENCRYPT_PRODUCTION,
            d => d,
        }
    }
}

/// Whether `name` is a DNS name an ACME CA would put on a certificate: lower-case LDH labels,
/// at least two of them, an optional single leading `*.` wildcard label, no IP literals.
pub fn valid_cert_name(name: &str) -> bool {
    let base = name.strip_prefix("*.").unwrap_or(name);
    if base.len() > 253 || base.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    let labels: Vec<&str> = base.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The host of an `http(s)://host[:port][/path]` URL, without brackets; `None` if malformed
/// (userinfo is refused: `http://127.0.0.1@evil.example` must not pass as loopback).
fn url_host(rest: &str) -> Option<&str> {
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        return (after.is_empty() || after.strip_prefix(':').is_some_and(|p| p.parse::<u16>().is_ok()))
            .then_some(host);
    }
    let (host, port) = authority.split_once(':').map_or((authority, None), |(h, p)| (h, Some(p)));
    if port.is_some_and(|p| p.parse::<u16>().is_err()) || host.is_empty() {
        return None;
    }
    Some(host)
}

/// Where the Cloudflare token may be sent: any `https://` host, or plain `http://` to exactly
/// `127.0.0.1`, `::1` or `localhost` (a local mock). Never a host that merely starts with one.
pub fn api_url_allowed(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return url_host(rest).is_some();
    }
    url.strip_prefix("http://")
        .and_then(url_host)
        .is_some_and(|h| matches!(h, "127.0.0.1" | "::1" | "localhost"))
}

impl TlsConfig {
    /// Fail-closed validation of `[server.tls]` (part of `Config::validate`).
    pub fn validate(&self) -> Result<()> {
        let acme_set = self.acme != AcmeConfig::default();
        match self.mode {
            TlsMode::Off => {
                anyhow::ensure!(
                    self.cert.is_none() && self.key.is_none() && !acme_set,
                    "server.tls: mode = \"off\" takes no cert/key/acme settings (set mode = \"files\" or \"acme\")"
                );
            }
            TlsMode::Files => {
                anyhow::ensure!(
                    self.cert.is_some() && self.key.is_some(),
                    "server.tls.cert and server.tls.key must both be set in files mode"
                );
                anyhow::ensure!(
                    !acme_set,
                    "server.tls.acme is only read in acme mode (mode = \"files\")"
                );
            }
            TlsMode::Acme => {
                anyhow::ensure!(
                    self.cert.is_none() && self.key.is_none(),
                    "server.tls.cert/key are only read in files mode; acme mode keeps the certificate in the bucket"
                );
                self.acme.validate()?;
            }
        }
        Ok(())
    }
}

impl AcmeConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.challenge == DNS01,
            "server.tls.acme.challenge = {:?}: only \"dns-01\" is supported (HTTP-01/TLS-ALPN-01 would need every instance behind the name to answer the token)",
            self.challenge
        );
        anyhow::ensure!(
            !self.domains.is_empty(),
            "server.tls.acme.domains must list at least one name in acme mode"
        );
        let mut seen = std::collections::HashSet::new();
        for d in &self.domains {
            anyhow::ensure!(
                valid_cert_name(d),
                "server.tls.acme.domains: {d:?} is not a lower-case DNS name (a single leading `*.` is allowed; no IP addresses)"
            );
            anyhow::ensure!(seen.insert(d.as_str()), "server.tls.acme.domains lists {d:?} twice");
        }
        let email = self.email.trim();
        anyhow::ensure!(
            email.split_once('@').is_some_and(|(u, h)| !u.is_empty() && h.contains('.'))
                && !email.contains(char::is_whitespace),
            "server.tls.acme.email must be a contact address in acme mode (got {:?})",
            self.email
        );
        let dir = self.directory_url();
        anyhow::ensure!(
            dir.starts_with("https://"),
            "server.tls.acme.directory must be an https:// ACME directory URL or \"staging\" (got {:?})",
            self.directory
        );
        anyhow::ensure!(
            !self.storage_key_env.trim().is_empty(),
            "server.tls.acme.storage_key_env must name the env var with the key that seals private keys in the bucket"
        );
        let day = Duration::from_hours(24);
        anyhow::ensure!(
            self.renew_before >= day && self.renew_before <= 60 * day,
            "server.tls.acme.renew_before must be between 1 day and 60 days (got {:?})",
            self.renew_before
        );
        anyhow::ensure!(
            self.propagation_timeout >= Duration::from_secs(10),
            "server.tls.acme.propagation_timeout must be at least 10s"
        );
        anyhow::ensure!(
            self.poll_interval >= Duration::from_secs(1),
            "server.tls.acme.poll_interval must be at least 1s"
        );
        for r in &self.resolvers {
            anyhow::ensure!(
                r.parse::<std::net::SocketAddr>().is_ok(),
                "server.tls.acme.resolvers: {r:?} is not an ip:port"
            );
        }
        match self.dns_provider {
            DnsProvider::Cloudflare => {
                let cf = &self.cloudflare;
                anyhow::ensure!(
                    !cf.api_token_env.trim().is_empty(),
                    "server.tls.acme.cloudflare.api_token_env must name the env var with the Cloudflare API token (Zone:DNS:Edit)"
                );
                anyhow::ensure!(
                    api_url_allowed(&cf.api_url),
                    "server.tls.acme.cloudflare.api_url must be https:// (plain http only for 127.0.0.1, ::1 or localhost; got {:?})",
                    cf.api_url
                );
                if let Some(z) = &cf.zone_id {
                    anyhow::ensure!(
                        !z.is_empty() && z.bytes().all(|b| b.is_ascii_alphanumeric()),
                        "server.tls.acme.cloudflare.zone_id must be a Cloudflare zone id"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acme() -> TlsConfig {
        TlsConfig {
            mode: TlsMode::Acme,
            acme: AcmeConfig {
                domains: vec!["git.example.com".into(), "*.git.example.com".into()],
                email: "ops@example.com".into(),
                ..AcmeConfig::default()
            },
            ..TlsConfig::default()
        }
    }

    #[test]
    fn off_and_files_validate_and_reject_foreign_keys() {
        TlsConfig::default().validate().unwrap();
        let mut t = TlsConfig {
            cert: Some("/c.pem".into()),
            ..TlsConfig::default()
        };
        assert!(t.validate().is_err(), "off with a cert");
        t.mode = TlsMode::Files;
        assert!(t.validate().unwrap_err().to_string().contains("both"));
        t.key = Some("/k.pem".into());
        t.validate().unwrap();
        t.acme.email = "ops@example.com".into();
        assert!(t.validate().unwrap_err().to_string().contains("acme mode"));
        let mut off = TlsConfig::default();
        off.acme.domains = vec!["a.example.com".into()];
        assert!(off.validate().is_err(), "acme keys under mode = off");
    }

    #[test]
    fn acme_requires_domains_email_token_env_and_dns01() {
        acme().validate().unwrap();
        let mut t = acme();
        t.acme.domains.clear();
        assert!(t.validate().unwrap_err().to_string().contains("domains"));
        let mut t = acme();
        t.acme.email = String::new();
        assert!(t.validate().unwrap_err().to_string().contains("email"));
        let mut t = acme();
        t.acme.cloudflare.api_token_env = " ".into();
        assert!(t.validate().unwrap_err().to_string().contains("api_token_env"));
        let mut t = acme();
        t.acme.storage_key_env = String::new();
        assert!(t.validate().unwrap_err().to_string().contains("storage_key_env"));
        for c in ["http-01", "tls-alpn-01", "DNS-01", ""] {
            let mut t = acme();
            t.acme.challenge = c.into();
            assert!(
                t.validate().unwrap_err().to_string().contains("dns-01"),
                "{c} refused"
            );
        }
        let mut t = acme();
        t.cert = Some("/c.pem".into());
        assert!(t.validate().is_err(), "acme with a files cert");
    }

    #[test]
    fn acme_names_directory_and_windows_are_checked() {
        for bad in [
            "Git.example.com",
            "localhost",
            "127.0.0.1",
            "*.*.example.com",
            "a..example.com",
            "-a.example.com",
            "git.example.com.",
            "foo_bar.example.com",
        ] {
            let mut t = acme();
            t.acme.domains = vec![bad.into()];
            assert!(t.validate().is_err(), "{bad} accepted");
        }
        let mut t = acme();
        t.acme.domains = vec!["a.example.com".into(), "a.example.com".into()];
        assert!(t.validate().unwrap_err().to_string().contains("twice"));

        let mut t = acme();
        t.acme.directory = "staging".into();
        t.validate().unwrap();
        assert_eq!(t.acme.directory_url(), LETSENCRYPT_STAGING);
        assert_eq!(AcmeConfig::default().directory_url(), LETSENCRYPT_PRODUCTION);
        t.acme.directory = "http://acme.example.com/dir".into();
        assert!(t.validate().unwrap_err().to_string().contains("https://"));

        let mut t = acme();
        t.acme.renew_before = Duration::from_hours(1);
        assert!(t.validate().is_err());
        t.acme.renew_before = Duration::from_hours(1464);
        assert!(t.validate().is_err());

        let mut t = acme();
        t.acme.resolvers = vec!["1.1.1.1".into()];
        assert!(t.validate().unwrap_err().to_string().contains("ip:port"));
        t.acme.resolvers = vec!["1.1.1.1:53".into(), "[2606:4700:4700::1111]:53".into()];
        t.validate().unwrap();

        let mut t = acme();
        t.acme.cloudflare.zone_id = Some("abc/def".into());
        assert!(t.validate().is_err());
    }

    #[test]
    fn cloudflare_api_url_allows_plain_http_only_to_loopback() {
        for ok in [
            "https://api.cloudflare.com/client/v4",
            "https://api.cloudflare.com:443/client/v4",
            "http://127.0.0.1:8080/client/v4",
            "http://127.0.0.1/client/v4",
            "http://[::1]:9000/v4",
            "http://localhost:3000",
        ] {
            assert!(api_url_allowed(ok), "{ok}");
        }
        for bad in [
            "http://127.0.0.1.evil.com/client/v4",
            "http://127.0.0.1.evil.com:80",
            "http://127.0.0.1@evil.com/v4",
            "http://localhost.evil.com",
            "http://[::1].evil.com",
            "http://api.cloudflare.com/client/v4",
            "http://127.0.0.1:notaport/",
            "ftp://127.0.0.1/",
            "https://",
            "https://user@api.cloudflare.com",
        ] {
            assert!(!api_url_allowed(bad), "{bad}");
        }
        let mut t = acme();
        t.acme.cloudflare.api_url = "http://127.0.0.1.evil.com/client/v4".into();
        assert!(t.validate().unwrap_err().to_string().contains("api_url"));
    }

    #[test]
    fn parses_from_toml() {
        let t: TlsConfig = toml::from_str(
            r#"
mode = "acme"
[acme]
domains = ["git.example.com"]
email = "ops@example.com"
directory = "staging"
renew_before = "20d"
[acme.cloudflare]
zone_id = "0123abcd"
"#,
        )
        .unwrap();
        t.validate().unwrap();
        assert_eq!(t.acme.renew_before, Duration::from_hours(480));
        assert_eq!(t.acme.cloudflare.api_token_env, "CLOUDFLARE_API_TOKEN");
        assert!(
            toml::from_str::<TlsConfig>("mode = \"self_signed\"").is_err(),
            "self_signed is gone"
        );
        assert!(
            toml::from_str::<TlsConfig>("mode = \"acme\"\n[acme]\nhttp01_port = 80").is_err(),
            "unknown (non-DNS) challenge settings are refused"
        );
    }
}
