//! D60 propagation (`docs/design/admin-ui.md` §4): this instance's view of the
//! config document. Loaded once at startup (bounded), then revalidated with a
//! conditional GET every `config_store.ttl` off every request path. A new
//! revision is opened, merged over the bootstrap, validated and published on a
//! `watch` channel; on any failure the previous one stays. Never blocks git.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use floe_config::secret::{GITHUB_MIRROR_TOKEN_ALIAS, set_alias};
use floe_config::{Config, RuntimeConfig};
use floe_store::Version;
use parking_lot::Mutex;
use tokio::sync::watch;

use super::{Changed, ConfigStore, InstanceStatus, Record};

/// How long startup waits for the config store.
pub const STARTUP_LOAD: Duration = Duration::from_secs(3);
/// At most this often a heartbeat is written without a change.
const HEARTBEAT_EVERY: Duration = Duration::from_mins(1);

/// What this instance runs.
#[derive(Debug, Clone)]
pub struct Applied {
    /// 0 = no document (built-in runtime defaults).
    pub revision: u64,
    /// Bootstrap ⊕ document.
    pub cfg: Arc<Config>,
}

/// Status for the overview and the heartbeat.
#[derive(Debug, Clone, Default)]
pub struct LiveStatus {
    pub applied_revision: u64,
    pub restart_required: Vec<String>,
    pub apply_error: Option<String>,
    pub last_check: Option<DateTime<Utc>>,
    pub check_error: Option<String>,
}

/// This instance's live config.
pub struct Live {
    bootstrap: Arc<Config>,
    store: Arc<ConfigStore>,
    tx: watch::Sender<Applied>,
    /// The runtime sections the process was built with (restart-required baseline).
    started: RuntimeConfig,
    started_at: DateTime<Utc>,
    known: Mutex<Option<Version>>,
    status: Mutex<LiveStatus>,
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live").field("status", &*self.status.lock()).finish_non_exhaustive()
    }
}

impl Live {
    /// Load the current document (bounded by [`STARTUP_LOAD`]) and build the
    /// effective config the process starts with. An unreachable store or an
    /// unopenable document starts the instance on the bootstrap's runtime
    /// sections (the built-in defaults) and records why.
    pub async fn start(bootstrap: Arc<Config>, store: Arc<ConfigStore>) -> Arc<Live> {
        let mut status = LiveStatus::default();
        let mut known = None;
        let mut initial: Option<(u64, Arc<Config>, RuntimeConfig)> = None;
        match tokio::time::timeout(STARTUP_LOAD, store.current()).await {
            Ok(Ok(Some((version, record)))) => {
                known = Some(version);
                match open_and_merge(&bootstrap, &store, &record) {
                    Ok((cfg, rt)) => initial = Some((record.revision, cfg, rt)),
                    Err(e) => {
                        tracing::error!(revision = record.revision, error = %e, "config store: cannot apply the current document; starting with the built-in runtime defaults");
                        status.apply_error = Some(format!("revision {}: {e:#}", record.revision));
                    }
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                tracing::error!(error = %e, "config store unreachable at startup; starting with the built-in runtime defaults");
                status.check_error = Some(format!("{e:#}"));
            }
            Err(_) => {
                tracing::error!(timeout = ?STARTUP_LOAD, "config store did not answer at startup; starting with the built-in runtime defaults");
                status.check_error = Some(format!("no answer within {STARTUP_LOAD:?} at startup"));
            }
        }
        status.last_check = Some(Utc::now());
        let (revision, cfg, started) = initial.unwrap_or_else(|| {
            // No document: the bootstrap's own runtime sections (defaults, or
            // what an embedding set programmatically), with the mirror token
            // read through the alias so a later document reaches it live.
            let mut c = (*bootstrap).clone();
            c.github_mirror.token_env = GITHUB_MIRROR_TOKEN_ALIAS.to_string();
            set_alias(GITHUB_MIRROR_TOKEN_ALIAS, Some(bootstrap.github_mirror.token.clone()));
            (0, Arc::new(c), RuntimeConfig::from_config(&bootstrap))
        });
        status.applied_revision = revision;
        let (tx, _) = watch::channel(Applied { revision, cfg });
        Arc::new(Live {
            bootstrap,
            store,
            tx,
            started,
            started_at: Utc::now(),
            known: Mutex::new(known),
            status: Mutex::new(status),
        })
    }

    /// The effective config now.
    pub fn current(&self) -> Applied {
        self.tx.borrow().clone()
    }

    /// Follow changes.
    pub fn subscribe(&self) -> watch::Receiver<Applied> {
        self.tx.subscribe()
    }

    pub fn status(&self) -> LiveStatus {
        self.status.lock().clone()
    }

    pub fn store(&self) -> &Arc<ConfigStore> {
        &self.store
    }

    pub fn bootstrap(&self) -> &Arc<Config> {
        &self.bootstrap
    }

    /// One conditional GET; apply a new revision. Returns the applied revision.
    pub async fn revalidate(&self) -> u64 {
        let known = self.known.lock().clone();
        let got = self.store.current_if_changed(known.as_ref()).await;
        let now = Utc::now();
        match got {
            Ok(Changed::Unchanged | Changed::Missing) => {
                let mut s = self.status.lock();
                s.last_check = Some(now);
                s.check_error = None;
            }
            Ok(Changed::Changed(version, record)) => {
                *self.known.lock() = Some(version);
                self.apply(&record);
                let mut s = self.status.lock();
                s.last_check = Some(now);
                s.check_error = None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "config store revalidation failed; keeping the applied revision");
                let mut s = self.status.lock();
                s.last_check = Some(now);
                s.check_error = Some(format!("{e:#}"));
            }
        }
        self.tx.borrow().revision
    }

    /// Apply a record (or keep the previous one and say why).
    fn apply(&self, record: &Record) {
        if record.revision == self.tx.borrow().revision {
            return;
        }
        match open_and_merge(&self.bootstrap, &self.store, record) {
            Ok((cfg, rt)) => {
                let restart = floe_config::runtime::restart_required(&self.started, &rt);
                tracing::info!(revision = record.revision, author = %record.author, restart_required = ?restart, "config applied");
                {
                    let mut s = self.status.lock();
                    s.applied_revision = record.revision;
                    s.restart_required = restart;
                    s.apply_error = None;
                }
                self.tx.send_replace(Applied {
                    revision: record.revision,
                    cfg,
                });
            }
            Err(e) => {
                tracing::error!(revision = record.revision, error = %e, "config revision not applied; keeping the previous one");
                self.status.lock().apply_error = Some(format!("revision {}: {e:#}", record.revision));
            }
        }
    }

    /// This instance's heartbeat record.
    pub fn instance_status(&self) -> InstanceStatus {
        let s = self.status();
        InstanceStatus {
            instance: floe_store::coord::instance_id().to_string(),
            version: crate::health::VERSION.to_string(),
            roles: roles(&self.bootstrap),
            started_at: self.started_at,
            seen_at: Utc::now(),
            applied_revision: s.applied_revision,
            restart_required: s.restart_required,
            apply_error: s.apply_error,
        }
    }

    /// The revalidation + heartbeat loop. Returns when draining.
    pub fn spawn(self: &Arc<Self>) {
        let live = self.clone();
        tokio::spawn(async move {
            let ttl = live.bootstrap.config_store.ttl;
            let mut last_beat: Option<tokio::time::Instant> = None;
            let mut beat_revision = u64::MAX;
            loop {
                if floe_wal::tasks::draining() {
                    return;
                }
                let revision = if ttl.is_zero() {
                    live.tx.borrow().revision
                } else {
                    live.revalidate().await
                };
                let due = last_beat.is_none_or(|t| t.elapsed() >= HEARTBEAT_EVERY);
                if due || revision != beat_revision {
                    if let Err(e) = live.store.heartbeat(&live.instance_status()).await {
                        tracing::debug!(error = %e, "config store heartbeat failed");
                    }
                    last_beat = Some(tokio::time::Instant::now());
                    beat_revision = revision;
                }
                let wait = if ttl.is_zero() { HEARTBEAT_EVERY } else { ttl };
                tokio::time::sleep(wait).await;
            }
        });
    }
}

fn roles(cfg: &Config) -> Vec<String> {
    if cfg.server.roles.is_empty() {
        return vec!["all".into()];
    }
    cfg.server
        .roles
        .iter()
        .map(|r| format!("{r:?}").to_lowercase())
        .collect()
}

/// Open a stored record's secrets, register the token alias, merge over the
/// bootstrap and validate.
fn open_and_merge(
    bootstrap: &Config,
    store: &ConfigStore,
    record: &Record,
) -> anyhow::Result<(Arc<Config>, RuntimeConfig)> {
    let rt = store.open_document(&record.document)?;
    let cfg = bootstrap.with_runtime(&rt)?;
    set_alias(GITHUB_MIRROR_TOKEN_ALIAS, Some(rt.github_mirror.token.clone()));
    Ok((Arc::new(cfg), rt))
}

/// The effective config for a one-shot command (`floe github sync`): the
/// current document applied over `bootstrap`, or `bootstrap` as is when there
/// is none. Fails when the document cannot be opened (a CLI should say so).
pub async fn effective_once(bootstrap: &Arc<Config>, store: &ConfigStore) -> anyhow::Result<Arc<Config>> {
    match store.current().await? {
        Some((_, record)) => Ok(open_and_merge(bootstrap, store, &record)?.0),
        None => Ok(bootstrap.clone()),
    }
}
