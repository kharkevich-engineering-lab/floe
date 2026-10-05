//! `server.tls.mode = "acme"`: certificates from an RFC 8555 directory (Let's Encrypt by
//! default) through DNS-01, with every piece of state in the bucket (principle I):
//!
//! | Object | What |
//! |---|---|
//! | `tls/acme/<dir>/account.json` | ACME account (id + key), the key sealed ([`crate::seal`]) |
//! | `tls/acme/<set>/cert.json` | chain PEM + sealed private key + `not_after` |
//! | `tls/acme/<set>/status.json` | last attempt / error / failures / next attempt (shared backoff) |
//! | `leases/tls-acme-<set>.pb` | the order lease: one instance orders or renews at a time |
//!
//! `<dir>` hashes the directory URL, `<set>` the directory + the sorted domain list, so a
//! changed `domains` (or staging → production) is a new certificate, never a mix-up.
//!
//! Every instance runs [`AcmeManager::run`]: revalidate `cert.json` with a conditional GET
//! (one ~15 ms request per `poll_interval`; 304 = nothing to do), install a changed one into
//! the [`CertResolver`] (hot swap). When the certificate is missing or within
//! `renew_before` of expiry, the instance that wins the lease orders a new one — narrated as
//! a task (D13) — and writes it; the others pick it up on their next poll. Failures back off
//! exponentially, recorded in `status.json` so a restart or another instance does not hammer
//! the CA (Let's Encrypt limits failed validations per hostname per hour).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use floe_config::AcmeConfig;
use floe_store::{DynStore, ObjectStoreExt, PutMode, StoreError, Version};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, BodyWrapper, BytesResponse, ChallengeType,
    HttpClient, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::dns::{DnsProvider, Propagation, TxtRecord, challenge_name};
use crate::resolver::{CertResolver, load_pem};
use crate::seal::SealKey;
use crate::status::CertStatus;
use crate::{Narration, Narrator};

/// The lease outlives a stalled holder by at most this; heartbeats renew it while ordering.
const LEASE_TTL: Duration = Duration::from_secs(90);
const LEASE_HEARTBEAT: Duration = Duration::from_secs(30);
/// While no certificate is loaded, poll at least this often (a fresh instance picks up the
/// certificate another instance just ordered within seconds).
const COLD_POLL: Duration = Duration::from_secs(10);
/// First retry after a failure; doubles per consecutive failure up to [`BACKOFF_MAX`].
const BACKOFF_BASE: Duration = Duration::from_mins(5);
const BACKOFF_MAX: Duration = Duration::from_hours(6);
/// A CA that says "rate limited" is not asked again for at least this long.
const RATE_LIMITED_MIN: Duration = Duration::from_hours(1);

#[derive(Debug, Serialize, Deserialize)]
struct StoredAccount {
    version: u32,
    directory: String,
    email: String,
    /// `instant_acme::AccountCredentials` JSON, sealed.
    credentials_sealed: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredCert {
    pub version: u32,
    pub directory: String,
    pub domains: Vec<String>,
    pub chain_pem: String,
    pub key_sealed: String,
    pub not_after: i64,
    pub issued_at: String,
    pub issued_by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoredStatus {
    #[serde(default)]
    pub last_attempt_at: Option<String>,
    #[serde(default)]
    pub last_success_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub failures: u32,
    /// Unix seconds; no attempt before.
    #[serde(default)]
    pub next_attempt_at: Option<i64>,
}

/// What one [`AcmeManager::tick`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// The certificate is valid and not due.
    Fresh,
    /// Due, but another instance holds the order lease.
    LeaseHeld,
    /// Due, but backing off after failures until the given unix time.
    BackingOff(i64),
    /// This instance ordered and installed a new certificate.
    Issued,
    /// This instance tried and failed (recorded in `status.json`).
    Failed(String),
}

/// Retry delay after `failures` consecutive failures.
pub fn backoff(failures: u32, rate_limited: bool) -> Duration {
    let exp = failures.saturating_sub(1).min(16);
    let d = BACKOFF_BASE
        .saturating_mul(1u32 << exp)
        .min(BACKOFF_MAX);
    if rate_limited { d.max(RATE_LIMITED_MIN) } else { d }
}

fn short_hash(parts: &[&str]) -> String {
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize().get(..8).unwrap_or_default())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn rfc3339(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

pub struct AcmeManager {
    cfg: AcmeConfig,
    directory: String,
    domains: Vec<String>,
    store: DynStore,
    seal: SealKey,
    dns: Arc<dyn DnsProvider>,
    propagation: Propagation,
    resolver: Arc<CertResolver>,
    http: reqwest::Client,
    holder: String,
    set_id: String,
    dir_id: String,
    state: Mutex<Local>,
}

impl std::fmt::Debug for AcmeManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeManager")
            .field("directory", &self.directory)
            .field("domains", &self.domains)
            .field("set_id", &self.set_id)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Local {
    cert_version: Option<Version>,
    status: StoredStatus,
    load_error: Option<String>,
}

impl AcmeManager {
    /// Wire everything from config + environment (fail closed: a missing token or storage
    /// key is an error at startup, not at the first renewal).
    pub fn from_env(
        cfg: &AcmeConfig,
        store: DynStore,
        resolver: Arc<CertResolver>,
    ) -> Result<Arc<Self>> {
        let seal = SealKey::from_env(&cfg.storage_key_env)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("floe/", env!("CARGO_PKG_VERSION"), " (acme)"))
            .timeout(Duration::from_mins(1))
            .build()
            .context("HTTP client")?;
        let dns: Arc<dyn DnsProvider> = match cfg.dns_provider {
            floe_config::DnsProvider::Cloudflare => Arc::new(
                crate::cloudflare::Cloudflare::from_config(&cfg.cloudflare, http.clone())?,
            ),
        };
        Self::new(cfg, store, resolver, seal, dns, http)
    }

    /// Explicit parts (tests: a Pebble directory, a fake DNS, a client trusting its CA).
    pub fn new(
        cfg: &AcmeConfig,
        store: DynStore,
        resolver: Arc<CertResolver>,
        seal: SealKey,
        dns: Arc<dyn DnsProvider>,
        http: reqwest::Client,
    ) -> Result<Arc<Self>> {
        let directory = cfg.directory_url().to_string();
        let mut sorted = cfg.domains.clone();
        sorted.sort();
        let mut set_parts = vec![directory.as_str()];
        set_parts.extend(sorted.iter().map(String::as_str));
        Ok(Arc::new(AcmeManager {
            propagation: Propagation::new(&cfg.resolvers, cfg.propagation_timeout)?,
            set_id: short_hash(&set_parts),
            dir_id: short_hash(&[directory.as_str()]),
            directory,
            domains: cfg.domains.clone(),
            cfg: cfg.clone(),
            store,
            seal,
            dns,
            resolver,
            http,
            holder: floe_store::coord::instance_id().to_string(),
            state: Mutex::new(Local::default()),
        }))
    }

    fn account_key(&self) -> String {
        format!("tls/acme/{}/account.json", self.dir_id)
    }
    pub fn cert_key(&self) -> String {
        format!("tls/acme/{}/cert.json", self.set_id)
    }
    fn status_key(&self) -> String {
        format!("tls/acme/{}/status.json", self.set_id)
    }
    fn lease_key(&self) -> String {
        format!("leases/tls-acme-{}.pb", self.set_id)
    }

    fn local(&self) -> std::sync::MutexGuard<'_, Local> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Revalidate `cert.json` (conditional GET) and install it when it changed.
    /// `Ok(true)` = a new certificate was installed.
    pub async fn refresh(&self) -> Result<bool> {
        let key = self.cert_key();
        let known = self.local().cert_version.clone();
        let got = match known {
            Some(v) => match self.store.get_if_changed(&key, &v).await {
                Ok(x) => x,
                Err(StoreError::NotFound { .. }) => None,
                Err(e) => return Err(e.into()),
            },
            None => self.store.get_bytes(&key).await?,
        };
        let Some((meta, body)) = got else {
            return Ok(false);
        };
        let installed = self.install(&body);
        let mut l = self.local();
        l.cert_version = Some(meta.version);
        match installed {
            Ok(()) => {
                l.load_error = None;
                Ok(true)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                l.load_error = Some(msg.clone());
                drop(l);
                tracing::error!(object = %key, error = %msg, "TLS certificate in the bucket cannot be loaded");
                Err(e)
            }
        }
    }

    fn install(&self, body: &[u8]) -> Result<()> {
        let c: StoredCert = serde_json::from_slice(body).context("cert.json")?;
        let key_pem = self
            .seal
            .open(self.aad("key").as_bytes(), &c.key_sealed)
            .context("opening the certificate's private key")?;
        let key_pem = String::from_utf8(key_pem).context("private key is not PEM")?;
        let loaded = load_pem(&c.chain_pem, &key_pem)?;
        self.resolver.set(loaded);
        Ok(())
    }

    fn aad(&self, field: &str) -> String {
        format!("{}#{field}", self.cert_key())
    }

    /// Whether the loaded certificate is missing or within `renew_before` of expiry.
    pub fn due(&self) -> bool {
        match self.resolver.current() {
            None => true,
            Some(l) => {
                let renew = i64::try_from(self.cfg.renew_before.as_secs()).unwrap_or(i64::MAX);
                now() >= l.info.not_after.saturating_sub(renew)
            }
        }
    }

    async fn read_status(&self) -> StoredStatus {
        match self.store.get_bytes(&self.status_key()).await {
            Ok(Some((_, b))) => serde_json::from_slice(&b).unwrap_or_default(),
            Ok(None) | Err(_) => StoredStatus::default(),
        }
    }

    async fn write_status(&self, s: &StoredStatus) {
        let body = serde_json::to_vec_pretty(s).unwrap_or_default();
        if let Err(e) = self
            .store
            .put_bytes(&self.status_key(), body, PutMode::Overwrite)
            .await
        {
            tracing::warn!(error = %e, "writing tls status.json failed");
        }
        self.local().status = s.clone();
    }

    /// One pass: revalidate, then order if due and nobody else is.
    pub async fn tick(&self, narrator: &dyn Narrator) -> Result<Tick> {
        if let Err(e) = self.refresh().await {
            tracing::debug!(error = %e, "tls refresh failed");
        }
        if !self.due() {
            return Ok(Tick::Fresh);
        }
        let status = self.read_status().await;
        self.local().status = status.clone();
        if let Some(l) = self.resolver.current()
            && let Some(err) = &status.last_error
        {
            tracing::warn!(
                not_after = %rfc3339(l.info.not_after),
                renew_before = ?self.cfg.renew_before,
                failures = status.failures,
                error = %err,
                "TLS certificate is within renew_before of expiry and renewal is failing"
            );
        }
        if let Some(at) = status.next_attempt_at
            && at > now()
        {
            return Ok(Tick::BackingOff(at));
        }
        let Some(lease) = floe_store::coord::try_acquire(
            self.store.clone(),
            &self.lease_key(),
            &self.holder,
            "tls-acme",
            LEASE_TTL,
        )
        .await?
        else {
            return Ok(Tick::LeaseHeld);
        };
        let guard = Arc::new(tokio::sync::Mutex::new(lease));
        let hb = floe_store::coord::LeaseGuard::spawn_heartbeat(guard.clone(), LEASE_HEARTBEAT, LEASE_TTL);
        let out = self.under_lease(narrator).await;
        hb.abort();
        let _ = hb.await;
        if let Ok(m) = Arc::try_unwrap(guard)
            && let Err(e) = m.into_inner().release().await
        {
            tracing::debug!(error = %e, "tls lease release failed (expires on its own)");
        }
        out
    }

    async fn under_lease(&self, narrator: &dyn Narrator) -> Result<Tick> {
        // Another instance may have finished an order between our poll and the lease.
        let _ = self.refresh().await;
        if !self.due() {
            return Ok(Tick::Fresh);
        }
        let mut status = self.read_status().await;
        if let Some(at) = status.next_attempt_at
            && at > now()
        {
            return Ok(Tick::BackingOff(at));
        }
        let what = if self.resolver.is_loaded() { "renewal" } else { "first order" };
        let task = narrator.begin(
            "tls-acme",
            &format!("ACME {what} for {} ({})", self.domains.join(", "), self.directory),
        );
        status.last_attempt_at = Some(rfc3339(now()));
        let result = self.order(task.as_ref()).await;
        match result {
            Ok(not_after) => {
                status.failures = 0;
                status.last_error = None;
                status.next_attempt_at = None;
                status.last_success_at = Some(rfc3339(now()));
                self.write_status(&status).await;
                metrics::counter!("floe_tls_acme_orders_total", "ok" => "true").increment(1);
                task.finish(Ok(format!("certificate issued, valid until {}", rfc3339(not_after))));
                Ok(Tick::Issued)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let rate_limited = msg.contains("rateLimited");
                status.failures = status.failures.saturating_add(1);
                let wait = backoff(status.failures, rate_limited);
                status.next_attempt_at =
                    Some(now().saturating_add(i64::try_from(wait.as_secs()).unwrap_or(i64::MAX)));
                status.last_error = Some(msg.clone());
                self.write_status(&status).await;
                metrics::counter!("floe_tls_acme_orders_total", "ok" => "false").increment(1);
                tracing::warn!(error = %msg, failures = status.failures, retry_in = ?wait, "ACME {what} failed");
                task.finish(Err(format!("{msg} (retry in {}s)", wait.as_secs())));
                Ok(Tick::Failed(msg))
            }
        }
    }

    fn account_builder(&self) -> instant_acme::AccountBuilder {
        Account::builder_with_http(Box::new(ReqwestHttp(self.http.clone())))
    }

    async fn account(&self, task: &dyn Narration) -> Result<Account> {
        let key = self.account_key();
        let aad = format!("{key}#credentials");
        if let Some((_, body)) = self.store.get_bytes(&key).await? {
            let stored: StoredAccount = serde_json::from_slice(&body).context("account.json")?;
            let creds = self
                .seal
                .open(aad.as_bytes(), &stored.credentials_sealed)
                .context("opening the ACME account key")?;
            let creds: AccountCredentials =
                serde_json::from_slice(&creds).context("ACME account credentials")?;
            let account = self
                .account_builder()
                .from_credentials(creds)
                .await
                .context("restoring the ACME account")?;
            if stored.email != self.cfg.email {
                let contact = format!("mailto:{}", self.cfg.email);
                account
                    .update_contacts(&[contact.as_str()])
                    .await
                    .context("updating the ACME account contact")?;
                let updated = StoredAccount {
                    email: self.cfg.email.clone(),
                    ..stored
                };
                self.store
                    .put_bytes(&key, serde_json::to_vec_pretty(&updated)?, PutMode::Overwrite)
                    .await?;
            }
            return Ok(account);
        }
        task.notice(&format!("registering an ACME account with {}", self.directory));
        let contact = format!("mailto:{}", self.cfg.email);
        let (account, creds) = self
            .account_builder()
            .create(
                &NewAccount {
                    contact: &[contact.as_str()],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.directory.clone(),
                None,
            )
            .await
            .context("registering the ACME account")?;
        let sealed = self
            .seal
            .seal(aad.as_bytes(), &serde_json::to_vec(&creds)?)?;
        let stored = StoredAccount {
            version: 1,
            directory: self.directory.clone(),
            email: self.cfg.email.clone(),
            credentials_sealed: sealed,
        };
        match self
            .store
            .put_bytes(&key, serde_json::to_vec_pretty(&stored)?, PutMode::Create)
            .await
        {
            Ok(_) | Err(StoreError::PreconditionFailed { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(account)
    }

    /// Order, validate over DNS-01, finalize, store, install. Returns the new `not_after`.
    async fn order(&self, task: &dyn Narration) -> Result<i64> {
        let account = self.account(task).await?;
        let ids: Vec<Identifier> = self
            .domains
            .iter()
            .map(|d| Identifier::Dns(d.clone()))
            .collect();
        let mut order = account
            .new_order(&NewOrder::new(&ids))
            .await
            .context("creating the ACME order")?;
        // Pass 1: the challenge values, published as TXT records.
        let mut wanted: Vec<(String, String)> = Vec::new();
        {
            let mut authzs = order.authorizations();
            while let Some(a) = authzs.next().await {
                let mut a = a.context("fetching an authorization")?;
                match a.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    s => anyhow::bail!("authorization for {} is {s:?}", a.identifier()),
                }
                let ident = a.identifier().to_string();
                let ch = a
                    .challenge(ChallengeType::Dns01)
                    .with_context(|| format!("the CA offers no dns-01 challenge for {ident}"))?;
                wanted.push((challenge_name(&ident), ch.key_authorization().dns_value()));
            }
        }
        let mut records: Vec<TxtRecord> = Vec::new();
        let published = self.publish(&wanted, &mut records, task).await;
        let validated = match published {
            Ok(()) => self.validate(&mut order, task).await,
            Err(e) => Err(e),
        };
        // Clean up whatever we created, success or not.
        for r in &records {
            if let Err(e) = self.dns.delete_txt(r).await {
                tracing::warn!(record = %r.fqdn, error = %e, "removing the challenge TXT record failed");
            }
        }
        validated?;
        task.notice("order ready; finalizing (new key + CSR)");
        let key_pem = order.finalize().await.context("finalizing the order")?;
        let chain = order
            .poll_certificate(&RetryPolicy::new().timeout(Duration::from_mins(2)))
            .await
            .context("downloading the certificate")?;
        let loaded = load_pem(&chain, &key_pem).context("the issued certificate")?;
        let not_after = loaded.info.not_after;
        let stored = StoredCert {
            version: 1,
            directory: self.directory.clone(),
            domains: self.domains.clone(),
            chain_pem: chain,
            key_sealed: self.seal.seal(self.aad("key").as_bytes(), key_pem.as_bytes())?,
            not_after,
            issued_at: rfc3339(now()),
            issued_by: self.holder.clone(),
        };
        let body = serde_json::to_vec_pretty(&stored)?;
        let meta = self
            .store
            .put_bytes(&self.cert_key(), body, PutMode::Overwrite)
            .await
            .context("writing cert.json")?;
        self.resolver.set(loaded);
        let mut l = self.local();
        l.cert_version = Some(meta.version);
        l.load_error = None;
        Ok(not_after)
    }

    async fn publish(
        &self,
        wanted: &[(String, String)],
        records: &mut Vec<TxtRecord>,
        task: &dyn Narration,
    ) -> Result<()> {
        for (fqdn, value) in wanted {
            task.notice(&format!("{}: TXT {fqdn}", self.dns.name()));
            records.push(self.dns.create_txt(fqdn, value).await?);
        }
        let mut names: Vec<&str> = wanted.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            let want: Vec<String> = wanted
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .collect();
            let ns = self.propagation.nameservers(name).await;
            task.notice(&format!(
                "waiting for {name} on {} nameserver(s) (up to {:?})",
                ns.len(),
                self.propagation.timeout
            ));
            self.propagation
                .wait(name, &want, &ns, |ok, of| {
                    tracing::debug!(%name, ok, of, "propagation round");
                })
                .await?;
        }
        Ok(())
    }

    async fn validate(&self, order: &mut instant_acme::Order, task: &dyn Narration) -> Result<()> {
        {
            let mut authzs = order.authorizations();
            while let Some(a) = authzs.next().await {
                let mut a = a?;
                if a.status != AuthorizationStatus::Pending {
                    continue;
                }
                let ident = a.identifier().to_string();
                a.challenge(ChallengeType::Dns01)
                    .with_context(|| format!("dns-01 challenge for {ident}"))?
                    .set_ready()
                    .await
                    .with_context(|| format!("asking the CA to validate {ident}"))?;
            }
        }
        task.notice("records visible; the CA is validating");
        let status = order
            .poll_ready(
                &RetryPolicy::new()
                    .initial_delay(Duration::from_secs(1))
                    .timeout(Duration::from_mins(3)),
            )
            .await
            .context("waiting for validation")?;
        if status != OrderStatus::Ready {
            let detail = order
                .state()
                .error
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            anyhow::bail!("order is {status:?} after validation {detail}");
        }
        Ok(())
    }

    /// Everything the status surfaces show (no secrets).
    pub fn status(&self) -> CertStatus {
        let l = self.local();
        let mut s = CertStatus::from_resolver("acme", &self.resolver);
        s.domains.clone_from(&self.domains);
        s.directory = Some(self.directory.clone());
        s.source = Some(format!("bucket:{}", self.cert_key()));
        s.last_renewal_at.clone_from(&l.status.last_success_at);
        s.last_attempt_at.clone_from(&l.status.last_attempt_at);
        s.last_error = l.status.last_error.clone().or_else(|| l.load_error.clone());
        s.failures = l.status.failures;
        s.next_attempt_at = l.status.next_attempt_at.map(rfc3339);
        drop(l);
        s.renewal_due = self.due();
        s
    }

    /// The poll loop every instance runs until the process exits.
    pub async fn run(self: Arc<Self>, narrator: Arc<dyn Narrator>) {
        loop {
            match self.tick(narrator.as_ref()).await {
                Ok(t) => tracing::debug!(tick = ?t, "tls acme pass"),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "tls acme pass failed"),
            }
            let wait = if self.resolver.is_loaded() {
                self.cfg.poll_interval
            } else {
                self.cfg.poll_interval.min(COLD_POLL)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// instant-acme over the reqwest client floe already carries (rustls, webpki roots), so the
/// ACME client brings no second HTTP/TLS stack.
struct ReqwestHttp(reqwest::Client);

impl HttpClient for ReqwestHttp {
    fn request(
        &self,
        req: http::Request<BodyWrapper<Bytes>>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>,
    > {
        let client = self.0.clone();
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let body = http_body_util::BodyExt::collect(body)
                .await
                .map(http_body_util::Collected::to_bytes)
                .unwrap_or_default();
            let rsp = client
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .map_err(|e| instant_acme::Error::Other(Box::new(e.without_url())))?;
            let status = rsp.status();
            let headers = rsp.headers().clone();
            let bytes = rsp
                .bytes()
                .await
                .map_err(|e| instant_acme::Error::Other(Box::new(e.without_url())))?;
            let mut out = http::Response::new(());
            *out.status_mut() = status;
            *out.headers_mut() = headers;
            let (parts, ()) = out.into_parts();
            Ok(BytesResponse {
                parts,
                body: Box::new(bytes),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1, false), Duration::from_mins(5));
        assert_eq!(backoff(2, false), Duration::from_mins(10));
        assert_eq!(backoff(3, false), Duration::from_mins(20));
        assert_eq!(backoff(40, false), BACKOFF_MAX);
        assert_eq!(backoff(1, true), RATE_LIMITED_MIN);
    }

    #[test]
    fn set_ids_change_with_domains_and_directory_not_order() {
        let a = short_hash(&["https://d", "a.example.com", "b.example.com"]);
        assert_eq!(a.len(), 16);
        assert_ne!(a, short_hash(&["https://d", "a.example.com"]));
        assert_ne!(a, short_hash(&["https://s", "a.example.com", "b.example.com"]));
    }
}
