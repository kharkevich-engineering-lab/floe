//! The certificate status the admin API (`GET /api/v1/tls`), `/readyz` and the logs show.

use serde::Serialize;

use crate::resolver::CertResolver;

/// No secrets: domains, validity and issuer are public (any handshake shows them); errors are
/// the CA's / DNS provider's messages, which never carry the token or a key. The admin API
/// is the only place `last_error` is served.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct CertStatus {
    /// `off` | `files` | `acme`.
    pub mode: &'static str,
    /// Configured names (`acme`), or the SANs of the loaded certificate (`files`).
    pub domains: Vec<String>,
    /// A certificate is being presented.
    pub loaded: bool,
    /// RFC 3339.
    pub not_before: Option<String>,
    /// RFC 3339.
    pub not_after: Option<String>,
    pub issuer: Option<String>,
    /// SANs of the presented certificate.
    pub sans: Vec<String>,
    pub fingerprint: Option<String>,
    /// `files:<cert path>` or `bucket:<object>`.
    pub source: Option<String>,
    /// ACME directory URL (`acme`).
    pub directory: Option<String>,
    /// Last successful issuance (`acme`), or last reload (`files`).
    pub last_renewal_at: Option<String>,
    pub last_attempt_at: Option<String>,
    pub last_error: Option<String>,
    /// Consecutive failed attempts.
    pub failures: u32,
    pub next_attempt_at: Option<String>,
    /// Missing, or inside `renew_before`.
    pub renewal_due: bool,
}

fn rfc3339(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

impl CertStatus {
    /// The certificate facts of whatever `resolver` presents.
    pub fn from_resolver(mode: &'static str, resolver: &CertResolver) -> Self {
        let mut s = CertStatus {
            mode,
            ..CertStatus::default()
        };
        if let Some(l) = resolver.current() {
            s.loaded = true;
            s.not_before = rfc3339(l.info.not_before);
            s.not_after = rfc3339(l.info.not_after);
            s.issuer = Some(l.info.issuer.clone());
            s.sans.clone_from(&l.info.sans);
            s.fingerprint = Some(l.info.fingerprint.clone());
        }
        s
    }

    /// The public subset (`/readyz`, instance info): no error text, no schedule.
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode,
            "loaded": self.loaded,
            "not_after": self.not_after,
            "issuer": self.issuer,
        })
    }
}
