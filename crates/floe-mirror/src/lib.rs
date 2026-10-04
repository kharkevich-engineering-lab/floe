//! `floe-mirror` (D49, `docs/design/github-mirror.md` §B): discovers
//! repositories on a forge, creates a floe repository per forge repository with
//! an `[upstream]` table, and keeps those tables current as the forge changes.
//! **The mirror decides; follow (D48) moves the bytes**: no git transfer happens
//! here, and nothing is ever deleted.
//!
//! One reconciler runs fleet-wide, under `leases/mirror-<kind>.pb`, on a
//! `maintain` host ([`run_loop`]) or from `floe github sync` ([`run_once_leased`]).
//! Its only durable state is `mirror/<kind>/state.json` (CAS) plus a disposable
//! HTTP cache, both in the bucket. A pass:
//!
//! 1. load state and the HTTP cache;
//! 2. discover ([`Source::discover`]) and look up up to 50 missing ids;
//! 3. plan ([`plan::plan`], pure);
//! 4. apply ([`apply::step`]) in order, saving the state every ≤ 10 steps, checking
//!    the lease and drain between steps.

pub mod apply;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
pub mod github;
pub mod http_cache;
pub mod naming;
pub mod plan;
pub mod select;
pub mod settings;
pub mod source;
pub mod state;
pub mod target;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use floe_config::Config;
use floe_store::DynStore;
use floe_store::coord::{self, LeaseGuard};

pub use http_cache::HttpCache;
pub use source::{ApiStats, Discovery, Lookup, RemoteRepo, Selection, Source, SourceError};
pub use state::{MirrorState, RepoEntry, Status};
pub use target::{CreateOutcome, ExistingRepo, Nudge, PublishOutcome, Target, WalTarget};

/// The state is written after at most this many applied steps.
const SAVE_EVERY_STEPS: usize = 10;
/// First pass this long after start (jittered), so a restart storm does not
/// hit the forge at once.
const FIRST_TICK: Duration = Duration::from_secs(10);
/// The longest a rate limit makes the loop sleep.
const MAX_RATE_SLEEP: Duration = Duration::from_hours(1);

/// Everything a pass needs.
pub struct Mirror {
    pub cfg: Arc<Config>,
    pub source: Arc<dyn Source>,
    pub target: Arc<dyn Target>,
    pub store: DynStore,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PassOptions {
    /// Plan only: no lease, no writes (not even the HTTP cache).
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PassReport {
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// `ok` | `incomplete` | `failed`.
    pub outcome: &'static str,
    pub complete: bool,
    pub discovered: usize,
    pub lookups: usize,
    pub created: u32,
    pub published: u32,
    pub nudged: u32,
    pub errors: u32,
    /// One human line per planned step (`create gh-acme/widgets ← Acme/Widgets …`).
    pub plan: Vec<String>,
    /// `(floe repository or forge name, change)` for inventory telemetry.
    pub changes: Vec<(String, String)>,
    /// Entries per status after the pass.
    pub statuses: BTreeMap<&'static str, usize>,
    pub api: ApiStats,
}

impl PassReport {
    /// One line for logs and the CLI.
    pub fn summary(&self) -> String {
        format!(
            "{}: discovered {}, looked up {}, created {}, published {}, nudged {}, errors {}",
            self.outcome,
            self.discovered,
            self.lookups,
            self.created,
            self.published,
            self.nudged,
            self.errors
        )
    }
}

/// `leases/mirror-<kind>.pb` at the bucket root.
pub fn lease_key(kind: &str) -> String {
    format!("leases/mirror-{kind}.pb")
}

/// The selection a `[github_mirror]` section names.
pub fn selection(cfg: &floe_config::GithubMirrorConfig) -> Selection {
    Selection {
        users: cfg.users.clone(),
        orgs: cfg.orgs.clone(),
        starred: cfg.starred.clone(),
        repos: cfg.repos.clone(),
    }
}

/// Why a pass stopped before its end.
#[derive(Debug, thiserror::Error)]
pub enum PassError {
    #[error("the mirror lease was lost; pass aborted")]
    LeaseLost,
    #[error("draining; pass aborted")]
    Draining,
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// One pass (§B.8). `lost` is the lease's released flag: set ⇒ the pass stops
/// before its next step. A dry run plans and returns without writing anything.
pub async fn reconcile_once(
    m: &Mirror,
    opts: PassOptions,
    lost: Option<&AtomicBool>,
) -> Result<PassReport, PassError> {
    let gm = &m.cfg.github_mirror;
    let kind = m.source.kind();
    let started = Utc::now();
    let mut report = PassReport {
        started_at: Some(started),
        outcome: "failed",
        ..PassReport::default()
    };
    let loaded = state::load(m.store.as_ref(), kind)
        .await
        .map_err(|e| anyhow::anyhow!("loading the mirror state: {e}"))?;
    let mut cache = HttpCache::load(m.store.as_ref(), kind).await;

    let sel = selection(gm);
    let discovery = m.source.discover(&sel, &mut cache).await;
    let discovery = match discovery {
        Ok(d) => d,
        Err(e) => {
            if !opts.dry_run {
                cache.save(m.store.as_ref(), kind).await;
            }
            if matches!(e, SourceError::NoToken(_) | SourceError::Unauthorized) {
                tracing::error!(source = kind, token_env = %gm.token_env, error = %e, "mirror pass failed: check the token");
            }
            return Err(e.into());
        }
    };
    report.discovered = discovery.repos.len();
    report.api = discovery.stats.clone();
    let mut complete = discovery.complete;

    let mut lookups = BTreeMap::new();
    for id in plan::lookup_candidates(&loaded, &discovery, started) {
        match m.source.lookup(&id, &mut cache).await {
            Ok(l) => {
                lookups.insert(id, l);
            }
            Err(SourceError::RateLimited { until }) => {
                report.api.rate_limited_until = Some(until);
                complete = false;
                break;
            }
            Err(e @ (SourceError::Unauthorized | SourceError::NoToken(_))) => return Err(e.into()),
            Err(e) => tracing::warn!(id, error = %e, "lookup failed; retried next pass"),
        }
    }
    report.lookups = lookups.len();

    let p = plan::plan(
        &loaded,
        &plan::PlanInput {
            cfg: gm,
            source: m.source.as_ref(),
            now: started,
            discovery: &discovery,
            lookups: &lookups,
        },
    );
    report.plan = describe(m, &p);
    report.complete = complete;
    if opts.dry_run {
        report.outcome = if complete { "ok" } else { "incomplete" };
        report.statuses = count_statuses(&p.state);
        report.finished_at = Some(Utc::now());
        return Ok(report);
    }

    let holder = coord::instance_id();
    let mut st = p.state;
    let ctx = apply::Ctx {
        cfg: gm,
        source: m.source.as_ref(),
        target: m.target.as_ref(),
    };
    let mut since_save = 0usize;
    for action in &p.actions {
        let Some(mut e) = st.repos.get(&action.id).cloned() else {
            continue;
        };
        let mut finished = true;
        for s in &action.steps {
            if lost.is_some_and(|f| f.load(Ordering::SeqCst)) {
                return Err(PassError::LeaseLost);
            }
            if floe_wal::tasks::draining() {
                st.repos.insert(action.id.clone(), e);
                save(m, &mut st, holder).await?;
                return Err(PassError::Draining);
            }
            let mut out = apply::Outcome::default();
            let r = apply::step(&ctx, &action.id, &mut e, s, &mut out).await;
            since_save += 1;
            report.created += u32::from(out.created);
            report.published += u32::from(out.published);
            report.nudged += u32::from(out.nudged);
            metrics::counter!("floe_mirror_actions_total", "source" => kind, "action" => s.name())
                .increment(1);
            match r {
                Ok(apply::Flow::Continue) => {}
                Ok(apply::Flow::Stop) => {
                    finished = false;
                    break;
                }
                Err(err) => {
                    tracing::warn!(id = %action.id, repo = %e.full_name, step = s.name(), error = %format!("{err:#}"), "mirror step failed; retried next pass");
                    e.status = Status::Error;
                    e.last_error = Some(format!("{}: {err:#}", s.name()));
                    report.errors += 1;
                    finished = false;
                    break;
                }
            }
        }
        if finished {
            e.last_error = None;
        }
        st.repos.insert(action.id.clone(), e);
        if since_save >= SAVE_EVERY_STEPS {
            save(m, &mut st, holder).await?;
            since_save = 0;
        }
    }

    report.changes = p
        .changes
        .iter()
        .map(|(id, c)| {
            let name = st.repos.get(id).map_or_else(
                || id.clone(),
                |e| e.floe.clone().unwrap_or_else(|| e.full_name.clone()),
            );
            (name, c.clone())
        })
        .collect();
    report.outcome = if complete { "ok" } else { "incomplete" };
    report.finished_at = Some(Utc::now());
    st.last_pass = Some(state::LastPass {
        started_at: report.started_at,
        finished_at: report.finished_at,
        complete,
        created: report.created,
        updated: report.published,
        errors: report.errors,
    });
    save(m, &mut st, holder).await?;
    if complete {
        cache.retain_touched();
    }
    cache.save(m.store.as_ref(), kind).await;
    report.statuses = count_statuses(&st);
    for (status, n) in &report.statuses {
        #[allow(clippy::cast_precision_loss)] // repository counts
        metrics::gauge!("floe_mirror_repos", "source" => kind, "status" => *status).set(*n as f64);
    }
    Ok(report)
}

async fn save(m: &Mirror, st: &mut MirrorState, holder: &str) -> Result<(), PassError> {
    state::save(m.store.as_ref(), m.source.kind(), st, holder)
        .await
        .map_err(|e| anyhow::anyhow!("saving the mirror state: {e}").into())
}

fn count_statuses(st: &MirrorState) -> BTreeMap<&'static str, usize> {
    let mut out: BTreeMap<&'static str, usize> =
        Status::ALL.iter().map(|s| (s.as_str(), 0)).collect();
    for e in st.repos.values() {
        *out.entry(e.status.as_str()).or_default() += 1;
    }
    out
}

/// Human lines for a plan (`--dry-run` and logs).
fn describe(m: &Mirror, p: &plan::Plan) -> Vec<String> {
    let gm = &m.cfg.github_mirror;
    let mut lines = Vec::new();
    for a in &p.actions {
        let Some(e) = p.state.repos.get(&a.id) else {
            continue;
        };
        let r = e.to_remote(&a.id);
        let floe = e.floe.clone().unwrap_or_else(|| {
            let name = naming::candidates(&gm.prefix, &r.owner, &r.name, &a.id)
                .into_iter()
                .next()
                .unwrap_or_default();
            format!("{}/{name}", naming::owner(&gm.prefix, &r.owner))
        });
        for s in &a.steps {
            lines.push(match s {
                plan::Step::Create { allow_create: true } => format!(
                    "create {floe} ← {} ({}, {:.1} MiB)",
                    e.full_name,
                    if e.private { "private" } else { "public" },
                    kib_to_mib(e.size_kb)
                ),
                plan::Step::Create {
                    allow_create: false,
                } => format!(
                    "too-large {} ({:.1} MiB): adopt {floe} if imported with [upstream] source = \"{}\"",
                    e.full_name,
                    kib_to_mib(e.size_kb),
                    settings::marker(m.source.kind(), &a.id)
                ),
                plan::Step::PutPolicy => format!("policy {floe} (read-only)"),
                plan::Step::Publish { reason } => format!("publish {floe} [upstream] ({reason})"),
                plan::Step::Nudge => format!("nudge {floe}"),
            });
        }
    }
    lines
}

#[allow(clippy::cast_precision_loss)] // display only
fn kib_to_mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

/// The current holder of the mirror lease and its expiry, if held.
pub async fn lease_holder(store: &DynStore, kind: &str) -> Option<(String, SystemTime)> {
    let (_, lease) = coord::get_message::<floe_proto::v1::Lease>(store.as_ref(), &lease_key(kind))
        .await
        .ok()??;
    let expires = lease.expires_at.as_ref().map(floe_proto::time::to_system)?;
    Some((lease.holder, expires))
}

/// `floe github sync --once`: acquire the lease (waiting up to `lease_ttl`), run
/// one pass, release. `Ok(None)` = the lease is held elsewhere.
pub async fn run_once_leased(m: &Mirror) -> Result<Option<PassReport>, PassError> {
    let ttl = m.cfg.github_mirror.lease_ttl;
    let kind = m.source.kind();
    let Some(guard) = coord::acquire(
        m.store.clone(),
        &lease_key(kind),
        coord::instance_id(),
        "github-mirror",
        ttl,
        ttl,
    )
    .await
    .map_err(|e| anyhow::anyhow!("acquiring the mirror lease: {e}"))?
    else {
        return Ok(None);
    };
    let flag = guard.released_flag();
    let guard = Arc::new(tokio::sync::Mutex::new(guard));
    let hb = LeaseGuard::spawn_heartbeat(guard.clone(), ttl / 3, ttl);
    let r = reconcile_once(m, PassOptions::default(), Some(&flag)).await;
    release(guard, hb).await;
    r.map(Some)
}

/// Stop the heartbeat and release the lease (best effort).
async fn release(guard: Arc<tokio::sync::Mutex<LeaseGuard>>, hb: tokio::task::JoinHandle<()>) {
    hb.abort();
    let _ = hb.await;
    match Arc::try_unwrap(guard) {
        Ok(g) => {
            if let Err(e) = g.into_inner().release().await {
                tracing::debug!(error = %e, "mirror lease release failed; it expires");
            }
        }
        // Unreachable once the heartbeat task is gone; dropping releases best effort.
        Err(arc) => drop(arc),
    }
}

/// The reconcile loop (§B.8): on a `maintain` host, forever. Holds the lease
/// across passes while it can, so exactly one host reconciles; a host without
/// the lease sleeps `interval` and tries again. `interval = 0` runs one pass at
/// startup. Returns when draining (D31 phase 1), after releasing the lease.
/// `on_pass` sees every finished pass (telemetry).
pub async fn run_loop(m: Arc<Mirror>, on_pass: Box<dyn Fn(&PassReport) + Send + Sync>) {
    let gm = &m.cfg.github_mirror;
    let kind = m.source.kind();
    if m.cfg.maintenance.follow_interval.is_zero() {
        tracing::warn!(
            "github mirror on a host with maintenance.follow_interval = 0: nudges still run follow here, but no backstop round ever runs"
        );
    }
    let interval = gm.interval;
    let ttl = gm.lease_ttl;
    if !sleep_or_drain(jitter(FIRST_TICK), None).await {
        return;
    }
    loop {
        let acquired = coord::try_acquire(
            m.store.clone(),
            &lease_key(kind),
            coord::instance_id(),
            "github-mirror",
            ttl,
        )
        .await;
        let guard = match acquired {
            Ok(Some(g)) => g,
            Ok(None) => {
                metrics::counter!("floe_mirror_pass_total", "source" => kind, "outcome" => "lease-held").increment(1);
                if interval.is_zero() || !sleep_or_drain(jitter(interval), None).await {
                    return;
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "mirror lease unavailable");
                if !sleep_or_drain(jitter(interval.max(ttl)), None).await {
                    return;
                }
                continue;
            }
        };
        let flag = guard.released_flag();
        let guard = Arc::new(tokio::sync::Mutex::new(guard));
        let hb = LeaseGuard::spawn_heartbeat(guard.clone(), ttl / 3, ttl);
        loop {
            let t0 = Instant::now();
            let result = reconcile_once(&m, PassOptions::default(), Some(&flag)).await;
            metrics::histogram!("floe_mirror_pass_seconds", "source" => kind)
                .record(t0.elapsed().as_secs_f64());
            let mut wait = jitter(interval);
            match &result {
                Ok(r) => {
                    tracing::info!(source = kind, elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX), complete = r.complete, "mirror pass {}", r.summary());
                    metrics::counter!("floe_mirror_pass_total", "source" => kind, "outcome" => r.outcome).increment(1);
                    if let Some(until) = r.api.rate_limited_until {
                        let d = until
                            .duration_since(SystemTime::now())
                            .unwrap_or_default()
                            .min(MAX_RATE_SLEEP);
                        wait = wait.max(d);
                    }
                    on_pass(r);
                }
                Err(PassError::Draining) => break,
                Err(e) => {
                    tracing::warn!(source = kind, error = %e, "mirror pass failed");
                    metrics::counter!("floe_mirror_pass_total", "source" => kind, "outcome" => "failed").increment(1);
                    if let PassError::Source(SourceError::RateLimited { until }) = e {
                        let d = until
                            .duration_since(SystemTime::now())
                            .unwrap_or_default()
                            .min(MAX_RATE_SLEEP);
                        wait = wait.max(d);
                    }
                }
            }
            if interval.is_zero() || flag.load(Ordering::SeqCst) {
                break;
            }
            if !sleep_or_drain(wait, Some(&flag)).await || flag.load(Ordering::SeqCst) {
                break;
            }
        }
        let lost = flag.load(Ordering::SeqCst);
        release(guard, hb).await;
        if floe_wal::tasks::draining() || interval.is_zero() {
            return;
        }
        if !lost {
            // Left the inner loop on drain.
            return;
        }
        tracing::warn!("mirror lease lost; retrying after one interval");
        if !sleep_or_drain(jitter(interval), None).await {
            return;
        }
    }
}

/// Sleep `d` in short steps; `false` when draining began (or `lost` was set).
async fn sleep_or_drain(d: Duration, lost: Option<&AtomicBool>) -> bool {
    let deadline = tokio::time::Instant::now() + d;
    loop {
        if floe_wal::tasks::draining() {
            return false;
        }
        if lost.is_some_and(|f| f.load(Ordering::SeqCst)) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return true;
        }
        tokio::time::sleep(deadline.saturating_duration_since(now).min(Duration::from_secs(1))).await;
    }
}

/// ±10 %.
fn jitter(d: Duration) -> Duration {
    use rand::Rng;
    let f: f64 = rand::rng().random_range(0.9..1.1);
    d.mul_f64(f)
}

#[cfg(test)]
mod tests;
