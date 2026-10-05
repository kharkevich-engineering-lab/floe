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
//! Every instance runs [`AcmeManager::run`]: revalidate `cert.json` and `status.json` with
//! conditional GETs (~15 ms each per `poll_interval`; 304 = nothing to do), install a changed
//! certificate into the [`CertResolver`] (hot swap). When the certificate is missing or past
//! its renewal point ([`renew_at`]: `renew_before` ahead of expiry, never before two thirds of
//! the lifetime), the instance that wins the lease orders a new one — narrated as a task
//! (D13) — and writes it; the others pick it up on their next poll. Failures back off
//! exponentially, recorded in `status.json` so a restart or another instance does not hammer
//! the CA (Let's Encrypt limits failed validations per hostname per hour), and no order starts
//! within 12 h of a successful one while a certificate is loaded.

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
/// Guard against a renewal loop: while a certificate is loaded, no new order starts within
/// this long of the last successful one, whatever `due()` says. The lifetime clamp in
/// [`renew_at`] already keeps a fresh certificate from being due; this bounds the damage of
/// any future bug to two orders a day (Let's Encrypt allows 5 duplicate certificates a week).
const MIN_ORDER_SPACING: Duration = Duration::from_hours(12);
/// Never renew before this fraction of the lifetime has passed (certbot and Let's Encrypt's
/// ARI guidance renew around two thirds in).
const LIFETIME_NUM: i64 = 2;
const LIFETIME_DEN: i64 = 3;

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
/// `tls/acme/<set>/status.json`: the renewal state every instance shares.
pub struct StoredStatus {
    #[serde(default)]
    pub last_attempt_at: Option<String>,
    #[serde(default)]
    pub last_success_at: Option<String>,
    /// Unix seconds of `last_success_at` (the order-spacing guard).
    #[serde(default)]
    pub last_success_unix: Option<i64>,
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

/// When a certificate valid from `not_before` to `not_after` (unix seconds) is due for
/// renewal: `renew_before` ahead of expiry, but never before two thirds of its lifetime.
/// Without the clamp a `renew_before` longer than the CA's lifetime (60 days against a 6- or
/// 45-day profile) makes every freshly issued certificate due at once, and the lease holder
/// re-orders every poll until the CA's rate limits stop it.
pub fn renew_at(not_before: i64, not_after: i64, renew_before: Duration) -> i64 {
    let lifetime = not_after.saturating_sub(not_before).max(0);
    let window = not_after.saturating_sub(i64::try_from(renew_before.as_secs()).unwrap_or(i64::MAX));
    let floor = not_before.saturating_add(lifetime / LIFETIME_DEN * LIFETIME_NUM);
    window.max(floor)
}

/// Unix time before which no new order may start because one succeeded recently
/// (`None` = no restriction). Applies only while a certificate is loaded: a missing
/// certificate is always worth an order.
fn spacing_until(status: &StoredStatus, loaded: bool) -> Option<i64> {
    let last = status.last_success_unix.filter(|_| loaded)?;
    let until = last.saturating_add(i64::try_from(MIN_ORDER_SPACING.as_secs()).unwrap_or(i64::MAX));
    (until > now()).then_some(until)
}

/// Whether a certificate with `sans` is valid for the configured name `domain`: an exact SAN,
/// or (for a non-wildcard name) a `*.parent` SAN covering exactly one leftmost label. A
/// configured wildcard needs that exact wildcard SAN.
pub fn sans_cover(sans: &[String], domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    sans.iter().any(|san| {
        let san = san.to_ascii_lowercase();
        san == domain
            || (!domain.starts_with("*.")
                && san.strip_prefix("*.").is_some_and(|parent| {
                    domain
                        .split_once('.')
                        .is_some_and(|(label, rest)| !label.is_empty() && rest == parent)
                }))
    })
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
    status_version: Option<Version>,
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
                metrics::counter!("floe_tls_cert_refused_total").increment(1);
                tracing::error!(object = %key, error = %msg, "TLS certificate in the bucket refused; still presenting the current one");
                Err(e)
            }
        }
    }

    fn install(&self, body: &[u8]) -> Result<()> {
        let c: StoredCert = serde_json::from_slice(body).context("cert.json")?;
        // Refuse before opening anything: an object for other names or another CA is never
        // presented, whatever put it there.
        let mut want = self.domains.clone();
        want.sort();
        let mut have = c.domains.clone();
        have.sort();
        anyhow::ensure!(
            have == want && c.directory == self.directory,
            "cert.json is for {:?} from {} but this host is configured for {:?} from {}; refusing it",
            c.domains,
            c.directory,
            self.domains,
            self.directory
        );
        let key_pem = self
            .seal
            .open(self.aad("key").as_bytes(), &c.key_sealed)
            .context("opening the certificate's private key")?;
        let key_pem = String::from_utf8(key_pem).context("private key is not PEM")?;
        let loaded = load_pem(&c.chain_pem, &key_pem)?;
        let uncovered: Vec<&String> = self
            .domains
            .iter()
            .filter(|d| !sans_cover(&loaded.info.sans, d))
            .collect();
        anyhow::ensure!(
            uncovered.is_empty(),
            "cert.json's certificate (SANs {:?}) does not cover {uncovered:?}; refusing it",
            loaded.info.sans
        );
        self.resolver.set(loaded);
        Ok(())
    }

    fn aad(&self, field: &str) -> String {
        format!("{}#{field}", self.cert_key())
    }

    /// Whether the loaded certificate is missing or past its renewal point ([`renew_at`]).
    pub fn due(&self) -> bool {
        match self.resolver.current() {
            None => true,
            Some(l) => now() >= renew_at(l.info.not_before, l.info.not_after, self.cfg.renew_before),
        }
    }

    /// Revalidate `status.json` (conditional GET, like `cert.json`) so every instance shows
    /// the shared renewal state — a failure another instance recorded, and its clearing when
    /// some instance succeeds — not only the one that ordered.
    pub async fn refresh_status(&self) -> StoredStatus {
        let key = self.status_key();
        let known = self.local().status_version.clone();
        let got = match &known {
            Some(v) => self.store.get_if_changed(&key, v).await,
            None => self.store.get_bytes(&key).await,
        };
        let mut l = self.local();
        match got {
            Ok(Some((meta, b))) => {
                l.status = serde_json::from_slice(&b).unwrap_or_default();
                l.status_version = Some(meta.version);
            }
            // Unchanged (304): keep what we have.
            Ok(None) if known.is_some() => {}
            Ok(None) | Err(StoreError::NotFound { .. }) => {
                l.status = StoredStatus::default();
                l.status_version = None;
            }
            Err(e) => tracing::debug!(error = %e, "reading tls status.json failed"),
        }
        l.status.clone()
    }

    async fn write_status(&self, s: &StoredStatus) {
        let body = serde_json::to_vec_pretty(s).unwrap_or_default();
        let written = self
            .store
            .put_bytes(&self.status_key(), body, PutMode::Overwrite)
            .await;
        let mut l = self.local();
        l.status = s.clone();
        match written {
            Ok(meta) => l.status_version = Some(meta.version),
            Err(e) => {
                l.status_version = None;
                drop(l);
                tracing::warn!(error = %e, "writing tls status.json failed");
            }
        }
    }

    /// One pass: revalidate, then order if due and nobody else is.
    pub async fn tick(&self, narrator: &dyn Narrator) -> Result<Tick> {
        if let Err(e) = self.refresh().await {
            tracing::debug!(error = %e, "tls refresh failed");
        }
        let status = self.refresh_status().await;
        if !self.due() {
            return Ok(Tick::Fresh);
        }
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
        if let Some(until) = spacing_until(&status, self.resolver.is_loaded()) {
            tracing::warn!(until = %rfc3339(until), "TLS certificate looks due right after a successful order; holding off (renewal-loop guard)");
            return Ok(Tick::BackingOff(until));
        }
        self.with_lease(narrator, false).await
    }

    /// Order a certificate now, under the lease, ignoring `due()`, backoff and the spacing
    /// guard. For tests (Pebble renewal) and a future operator action; never called by the
    /// poll loop.
    pub async fn renew_now(&self, narrator: &dyn Narrator) -> Result<Tick> {
        self.with_lease(narrator, true).await
    }

    async fn with_lease(&self, narrator: &dyn Narrator, force: bool) -> Result<Tick> {
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
        let out = self.under_lease(narrator, force).await;
        hb.abort();
        let _ = hb.await;
        if let Ok(m) = Arc::try_unwrap(guard)
            && let Err(e) = m.into_inner().release().await
        {
            tracing::debug!(error = %e, "tls lease release failed (expires on its own)");
        }
        out
    }

    async fn under_lease(&self, narrator: &dyn Narrator, force: bool) -> Result<Tick> {
        // Another instance may have finished an order between our poll and the lease.
        let _ = self.refresh().await;
        let mut status = self.refresh_status().await;
        if !force {
            if !self.due() {
                return Ok(Tick::Fresh);
            }
            if let Some(at) = status.next_attempt_at
                && at > now()
            {
                return Ok(Tick::BackingOff(at));
            }
            if let Some(until) = spacing_until(&status, self.resolver.is_loaded()) {
                return Ok(Tick::BackingOff(until));
            }
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
                let issued = now();
                status.last_success_at = Some(rfc3339(issued));
                status.last_success_unix = Some(issued);
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
        // A refused cert.json counts as a renewal failure here: it is this instance's most
        // current problem, so it leads.
        s.last_error = l.load_error.clone().or_else(|| l.status.last_error.clone());
        s.failures = l
            .status
            .failures
            .saturating_add(u32::from(l.load_error.is_some()));
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
    use floe_store::memory::MemoryStore;

    const DAY: i64 = 86_400;

    #[test]
    fn renewal_point_is_clamped_to_two_thirds_of_the_lifetime() {
        let issued = 1_800_000_000;
        // 90 days, 30 days ahead: day 60 either way.
        assert_eq!(renew_at(issued, issued + 90 * DAY, Duration::from_hours(30 * 24)), issued + 60 * DAY);
        // 6-day profile with the maximum renew_before (60 days): day 4, not "already due".
        assert_eq!(renew_at(issued, issued + 6 * DAY, Duration::from_hours(60 * 24)), issued + 4 * DAY);
        // 45 days with 30 ahead: day 30 (the window would say day 15).
        assert_eq!(renew_at(issued, issued + 45 * DAY, Duration::from_hours(30 * 24)), issued + 30 * DAY);
        // 45 days with 60 ahead: still day 30.
        assert_eq!(renew_at(issued, issued + 45 * DAY, Duration::from_hours(60 * 24)), issued + 30 * DAY);
        // A short renew_before still wins when it is later than the floor.
        assert_eq!(renew_at(issued, issued + 90 * DAY, Duration::from_hours(24)), issued + 89 * DAY);
    }

    #[test]
    fn spacing_guard_holds_renewals_after_a_recent_success() {
        let recent = StoredStatus {
            last_success_unix: Some(now() - 60),
            ..StoredStatus::default()
        };
        let until = spacing_until(&recent, true).expect("held");
        assert!(until > now() && until <= now() + 12 * 3600);
        assert!(spacing_until(&recent, false).is_none(), "a missing certificate is always ordered");
        let old = StoredStatus {
            last_success_unix: Some(now() - 13 * 3600),
            ..StoredStatus::default()
        };
        assert!(spacing_until(&old, true).is_none());
        assert!(spacing_until(&StoredStatus::default(), true).is_none());
    }

    struct NoDns;
    #[async_trait::async_trait]
    impl DnsProvider for NoDns {
        fn name(&self) -> &'static str {
            "none"
        }
        async fn create_txt(&self, _: &str, _: &str) -> Result<TxtRecord> {
            anyhow::bail!("no DNS in unit tests")
        }
        async fn delete_txt(&self, _: &TxtRecord) -> Result<()> {
            Ok(())
        }
    }

    const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn instance(store: &DynStore, renew_before: Duration) -> (Arc<AcmeManager>, Arc<CertResolver>) {
        let resolver = CertResolver::new();
        let cfg = AcmeConfig {
            domains: vec!["floe.test".into()],
            email: "ops@floe.test".into(),
            renew_before,
            ..AcmeConfig::default()
        };
        let mgr = AcmeManager::new(
            &cfg,
            store.clone(),
            resolver.clone(),
            SealKey::parse(KEY).unwrap(),
            Arc::new(NoDns),
            reqwest::Client::new(),
        )
        .unwrap();
        (mgr, resolver)
    }

    /// A `floe.test` certificate valid from `from` to `to` (unix seconds).
    fn pem(from: i64, to: i64) -> (String, String) {
        pem_for(&["floe.test"], from, to)
    }

    fn pem_for(names: &[&str], from: i64, to: i64) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params =
            rcgen::CertificateParams::new(names.iter().map(ToString::to_string).collect::<Vec<_>>())
                .unwrap();
        let epoch = rcgen::date_time_ymd(1970, 1, 1);
        params.not_before = epoch + Duration::from_secs(u64::try_from(from).unwrap());
        params.not_after = epoch + Duration::from_secs(u64::try_from(to).unwrap());
        (params.self_signed(&key).unwrap().pem(), key.serialize_pem())
    }

    /// Write `cert.json` the way the orderer does.
    async fn publish(mgr: &AcmeManager, chain: &str, key: &str) {
        publish_as(mgr, chain, key, &mgr.domains, &mgr.directory).await;
    }

    /// Write `cert.json` recording `domains` / `directory` (which may not match the config).
    async fn publish_as(mgr: &AcmeManager, chain: &str, key: &str, domains: &[String], directory: &str) {
        let stored = StoredCert {
            version: 1,
            directory: directory.to_string(),
            domains: domains.to_vec(),
            chain_pem: chain.to_string(),
            key_sealed: mgr.seal.seal(mgr.aad("key").as_bytes(), key.as_bytes()).unwrap(),
            not_after: 0,
            issued_at: String::new(),
            issued_by: "test".into(),
        };
        mgr.store
            .put_bytes(&mgr.cert_key(), serde_json::to_vec(&stored).unwrap(), PutMode::Overwrite)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn freshly_issued_short_lived_certificates_are_not_due() {
        let store: DynStore = MemoryStore::shared();
        let (mgr, _) = instance(&store, Duration::from_hours(60 * 24));
        for days in [6, 45] {
            let (chain, key) = pem(now() - 60, now() + days * DAY);
            publish(&mgr, &chain, &key).await;
            assert!(mgr.refresh().await.unwrap());
            assert!(!mgr.due(), "a fresh {days}-day certificate with renew_before = 60d");
            assert_eq!(mgr.tick(&crate::LogNarrator).await.unwrap(), Tick::Fresh);
        }
        // Past two thirds of a 6-day lifetime it is due.
        let (chain, key) = pem(now() - 5 * DAY, now() + DAY);
        publish(&mgr, &chain, &key).await;
        assert!(mgr.refresh().await.unwrap());
        assert!(mgr.due());
    }

    #[test]
    fn sans_cover_is_wildcard_aware() {
        let sans = vec!["floe.test".to_string(), "*.floe.test".to_string()];
        assert!(sans_cover(&sans, "floe.test"));
        assert!(sans_cover(&sans, "git.floe.test"), "one label under the wildcard");
        assert!(sans_cover(&sans, "*.floe.test"));
        assert!(!sans_cover(&sans, "a.b.floe.test"), "a wildcard covers one label only");
        assert!(!sans_cover(&sans, "other.test"));
        let only_wild = vec!["*.floe.test".to_string()];
        assert!(!sans_cover(&only_wild, "floe.test"), "the apex is not under its wildcard");
        let only_host = vec!["git.floe.test".to_string()];
        assert!(!sans_cover(&only_host, "*.floe.test"), "a configured wildcard needs that SAN");
    }

    #[tokio::test]
    async fn a_cert_json_for_other_names_or_another_ca_is_refused() {
        let store: DynStore = MemoryStore::shared();
        let (mgr, resolver) = instance(&store, Duration::from_hours(30 * 24));
        let (chain, key) = pem(now() - 60, now() + 90 * DAY);
        publish(&mgr, &chain, &key).await;
        assert!(mgr.refresh().await.unwrap());
        let good = resolver.current().unwrap().info.fingerprint.clone();
        let other_dir = format!("{}-other", mgr.directory);
        let cases: Vec<(Vec<String>, String, (String, String))> = vec![
            // Recorded domains differ from the config.
            (vec!["evil.test".to_string()], mgr.directory.clone(), pem(now() - 60, now() + 90 * DAY)),
            // Another directory.
            (mgr.domains.clone(), other_dir, pem(now() - 60, now() + 90 * DAY)),
            // Recorded domains match, but the leaf does not cover them.
            (mgr.domains.clone(), mgr.directory.clone(), pem_for(&["evil.test"], now() - 60, now() + 90 * DAY)),
        ];
        for (domains, directory, (chain, key)) in cases {
            publish_as(&mgr, &chain, &key, &domains, &directory).await;
            let err = mgr.refresh().await.unwrap_err().to_string();
            assert!(err.contains("refusing"), "{err}");
            assert_eq!(resolver.current().unwrap().info.fingerprint, good, "current certificate kept");
            let st = mgr.status();
            assert_eq!(st.failures, 1, "counted as a renewal failure: {st:?}");
            assert!(st.last_error.as_deref().is_some_and(|e| e.contains("refusing")));
            assert!(!st.last_error.unwrap().contains("PRIVATE"), "no secrets in the error");
        }
        // A good object again clears it.
        let (chain, key) = pem(now() - 60, now() + 90 * DAY);
        publish(&mgr, &chain, &key).await;
        assert!(mgr.refresh().await.unwrap());
        assert_eq!(mgr.status().failures, 0);
    }

    #[tokio::test]
    async fn every_instance_follows_the_shared_status() {
        let store: DynStore = MemoryStore::shared();
        let (a, ra) = instance(&store, Duration::from_hours(30 * 24));
        // A presents an expiring certificate and another instance's renewal has been failing.
        let (old_chain, old_key) = pem(now() - 80 * DAY, now() + DAY);
        ra.set(load_pem(&old_chain, &old_key).unwrap());
        let failing = StoredStatus {
            failures: 3,
            last_error: Some("boom".into()),
            next_attempt_at: Some(now() + 3600),
            ..StoredStatus::default()
        };
        store
            .put_bytes(&a.status_key(), serde_json::to_vec(&failing).unwrap(), PutMode::Overwrite)
            .await
            .unwrap();
        assert!(matches!(a.tick(&crate::LogNarrator).await.unwrap(), Tick::BackingOff(_)));
        let st = a.status();
        assert_eq!((st.failures, st.last_error.as_deref()), (3, Some("boom")));

        // B renews: new cert.json, cleared status.json.
        let (b, _) = instance(&store, Duration::from_hours(30 * 24));
        let (chain, key) = pem(now() - 60, now() + 90 * DAY);
        publish(&b, &chain, &key).await;
        let ok = StoredStatus {
            last_success_at: Some(rfc3339(now())),
            last_success_unix: Some(now()),
            ..StoredStatus::default()
        };
        b.write_status(&ok).await;

        // A's next pass installs the certificate (fresh, so it returns early) and still drops
        // the stale failure state.
        assert_eq!(a.tick(&crate::LogNarrator).await.unwrap(), Tick::Fresh);
        let st = a.status();
        assert_eq!(st.failures, 0, "{st:?}");
        assert!(st.last_error.is_none() && st.next_attempt_at.is_none(), "{st:?}");
        assert!(st.last_renewal_at.is_some());
        assert!(!st.renewal_due);
    }

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
