//! `server.tls.mode = "files"`: the operator's chain and key, reloaded without a restart when
//! either file changes (mtime/size polled every few seconds — no inotify dependency, works on
//! bind mounts and Kubernetes secret volumes, whose atomic symlink swap changes the target) or
//! on `SIGHUP`. A reload that does not parse keeps the current certificate and logs why.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::resolver::{CertResolver, load_pem};
use crate::status::CertStatus;

/// How often the files are checked for a change.
pub const POLL: Duration = Duration::from_secs(5);

type Stamp = Option<(SystemTime, u64)>;

fn stamp(p: &Path) -> Stamp {
    std::fs::metadata(p)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))
}

#[derive(Debug)]
pub struct FilesCert {
    cert: PathBuf,
    key: PathBuf,
    resolver: Arc<CertResolver>,
    seen: Mutex<(Stamp, Stamp)>,
    last: Mutex<(Option<String>, Option<String>)>,
}

impl FilesCert {
    /// Load once (fail closed at startup: unreadable or mismatched files are fatal).
    pub fn load(cert: &Path, key: &Path, resolver: Arc<CertResolver>) -> Result<Arc<Self>> {
        let f = Arc::new(FilesCert {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
            resolver,
            seen: Mutex::new((None, None)),
            last: Mutex::new((None, None)),
        });
        f.reload()?;
        Ok(f)
    }

    /// Read both files and install them.
    pub fn reload(&self) -> Result<()> {
        let stamps = (stamp(&self.cert), stamp(&self.key));
        let res = self.read_pair();
        if let Ok(mut s) = self.seen.lock() {
            *s = stamps;
        }
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match res {
            Ok(l) => {
                self.resolver.set(l);
                if let Ok(mut last) = self.last.lock() {
                    *last = (Some(now), None);
                }
                Ok(())
            }
            Err(e) => {
                if let Ok(mut last) = self.last.lock() {
                    last.1 = Some(format!("{e:#}"));
                }
                Err(e)
            }
        }
    }

    fn read_pair(&self) -> Result<crate::resolver::Loaded> {
        let c = std::fs::read_to_string(&self.cert)
            .with_context(|| format!("reading server.tls.cert {}", self.cert.display()))?;
        let k = std::fs::read_to_string(&self.key)
            .with_context(|| format!("reading server.tls.key {}", self.key.display()))?;
        load_pem(&c, &k)
    }

    /// Reload if either file's mtime or size moved since the last read.
    pub fn reload_if_changed(&self) -> Option<Result<()>> {
        let now = (stamp(&self.cert), stamp(&self.key));
        let changed = self.seen.lock().is_ok_and(|s| *s != now);
        changed.then(|| self.reload())
    }

    pub fn status(&self) -> CertStatus {
        let mut s = CertStatus::from_resolver("files", &self.resolver);
        s.domains.clone_from(&s.sans);
        s.source = Some(format!("files:{}", self.cert.display()));
        if let Ok(last) = self.last.lock() {
            s.last_renewal_at.clone_from(&last.0);
            s.last_error.clone_from(&last.1);
        }
        s
    }

    /// Watch loop: file changes every [`POLL`], plus `SIGHUP` (unix).
    pub async fn watch(self: Arc<Self>) {
        #[cfg(unix)]
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
        loop {
            #[cfg(unix)]
            let forced = if let Some(h) = hup.as_mut() {
                tokio::select! {
                    _ = h.recv() => true,
                    () = tokio::time::sleep(POLL) => false,
                }
            } else {
                tokio::time::sleep(POLL).await;
                false
            };
            #[cfg(not(unix))]
            let forced = {
                tokio::time::sleep(POLL).await;
                false
            };
            let r = if forced {
                tracing::info!("SIGHUP: reloading the TLS certificate files");
                Some(self.reload())
            } else {
                self.reload_if_changed()
            };
            if let Some(Err(e)) = r {
                tracing::error!(error = %format!("{e:#}"), "TLS certificate reload failed; still presenting the previous certificate");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::tests::pair;

    #[test]
    fn reloads_on_change_and_keeps_the_old_cert_on_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path) = (dir.path().join("c.pem"), dir.path().join("k.pem"));
        let (c1, k1) = pair(&["a.example.com"]);
        std::fs::write(&cert_path, &c1).unwrap();
        std::fs::write(&key_path, &k1).unwrap();
        let resolver = CertResolver::new();
        let files = FilesCert::load(&cert_path, &key_path, resolver.clone()).unwrap();
        let fp1 = resolver.current().unwrap().info.fingerprint.clone();
        assert!(files.reload_if_changed().is_none(), "unchanged");
        let (c2, k2) = pair(&["b.example.com", "c.example.com"]);
        std::fs::write(&cert_path, &c2).unwrap();
        std::fs::write(&key_path, &k2).unwrap();
        files.reload_if_changed().expect("size changed").unwrap();
        assert_ne!(resolver.current().unwrap().info.fingerprint, fp1);
        assert_eq!(files.status().domains, vec!["b.example.com", "c.example.com"]);
        std::fs::write(&key_path, "not a key, and a different length entirely").unwrap();
        assert!(files.reload_if_changed().unwrap().is_err());
        assert_eq!(resolver.current().unwrap().info.sans, vec!["b.example.com", "c.example.com"]);
        assert!(files.status().last_error.is_some());
        // Startup is fail-closed.
        assert!(FilesCert::load(&cert_path, &key_path, CertResolver::new()).is_err());
    }
}
