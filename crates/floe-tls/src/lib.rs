//! TLS for floe's listener (D39, D59): `server.tls.mode = "off" | "files" | "acme"`.
//!
//! * [`resolver`] — the rustls certificate resolver every mode presents through; swapping its
//!   certificate is how renewals and file reloads take effect without a restart.
//! * [`files`] — the operator's chain + key, reloaded on change or `SIGHUP`.
//! * [`acme`] — RFC 8555 through DNS-01 ([`dns`], [`cloudflare`]), state in the bucket under
//!   `tls/`, one orderer at a time under a store lease, private keys sealed ([`seal`]).
//! * [`status`] — what `/api/v1/tls`, `/readyz` and the metrics show.

pub mod acme;
pub mod cloudflare;
pub mod dns;
pub mod files;
pub mod resolver;
pub mod seal;
pub mod status;

use std::sync::Arc;

use anyhow::Result;
use floe_config::{Config, TlsMode};
use floe_store::DynStore;

pub use acme::AcmeManager;
pub use files::FilesCert;
pub use resolver::CertResolver;
pub use status::CertStatus;

/// Long work is a task (D13): the server narrates an ACME order through its task registry;
/// tests and tools log it.
pub trait Narrator: Send + Sync {
    fn begin(&self, kind: &str, summary: &str) -> Box<dyn Narration>;
}

/// One narrated unit of work.
pub trait Narration: Send + Sync {
    fn notice(&self, text: &str);
    fn finish(self: Box<Self>, result: Result<String, String>);
}

/// A [`Narrator`] that only logs.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogNarrator;

struct LogNarration(String);

impl Narrator for LogNarrator {
    fn begin(&self, kind: &str, summary: &str) -> Box<dyn Narration> {
        tracing::info!(kind, "{summary}");
        Box::new(LogNarration(kind.to_string()))
    }
}

impl Narration for LogNarration {
    fn notice(&self, text: &str) {
        tracing::info!(kind = %self.0, "{text}");
    }
    fn finish(self: Box<Self>, result: Result<String, String>) {
        match result {
            Ok(s) => tracing::info!(kind = %self.0, "{s}"),
            Err(e) => tracing::warn!(kind = %self.0, "{e}"),
        }
    }
}

/// Where the presented certificate comes from.
#[derive(Debug)]
pub enum Source {
    Files(Arc<FilesCert>),
    Acme(Arc<AcmeManager>),
}

/// The listener's TLS: a rustls config whose certificate is swappable, and its source.
pub struct Tls {
    pub resolver: Arc<CertResolver>,
    pub server_config: Arc<rustls::ServerConfig>,
    pub source: Source,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tls")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl Tls {
    /// `None` when `mode = "off"`. Fail closed: unreadable files, a missing token or storage
    /// key are startup errors. In `acme` mode nothing is fetched here; [`Tls::spawn`] starts
    /// the loop that loads (or orders) the certificate.
    pub fn load(cfg: &Config, store: DynStore) -> Result<Option<Arc<Self>>> {
        let t = &cfg.server.tls;
        let resolver = CertResolver::new();
        let source = match t.mode {
            TlsMode::Off => return Ok(None),
            TlsMode::Files => {
                let (Some(c), Some(k)) = (&t.cert, &t.key) else {
                    anyhow::bail!("server.tls.cert and server.tls.key are required in files mode");
                };
                Source::Files(FilesCert::load(c, k, resolver.clone())?)
            }
            TlsMode::Acme => Source::Acme(AcmeManager::from_env(&t.acme, store, resolver.clone())?),
        };
        let server_config = Arc::new(resolver::server_config(resolver.clone())?);
        Ok(Some(Arc::new(Tls {
            resolver,
            server_config,
            source,
        })))
    }

    /// Start the background loop (file watch / ACME poll + renew).
    pub fn spawn(&self, narrator: Arc<dyn Narrator>) {
        match &self.source {
            Source::Files(f) => {
                tokio::spawn(f.clone().watch());
            }
            Source::Acme(a) => {
                tokio::spawn(a.clone().run(narrator));
            }
        }
    }

    /// A certificate is loaded (in `acme` mode `/readyz` is 503 until it is).
    pub fn ready(&self) -> bool {
        self.resolver.is_loaded()
    }

    pub fn status(&self) -> CertStatus {
        match &self.source {
            Source::Files(f) => f.status(),
            Source::Acme(a) => a.status(),
        }
    }
}
