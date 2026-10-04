//! The listener's certificate: a rustls [`ResolvesServerCert`] whose answer is swapped in
//! place. A renewal (ACME) or a file change (`files` mode) replaces the `CertifiedKey`; every
//! *new* handshake gets the new one, established connections keep theirs — no restart, no
//! dropped connection.

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::Digest;

/// What the status surfaces show about a certificate. Public facts only: everything here is
/// visible to anyone who completes a handshake.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CertInfo {
    /// Subject alternative names (DNS) of the leaf.
    pub sans: Vec<String>,
    pub issuer: String,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds.
    pub not_after: i64,
    /// `sha256:<hex>` of the leaf DER.
    pub fingerprint: String,
}

/// A parsed, checked chain + key, ready to present.
pub struct Loaded {
    pub key: Arc<CertifiedKey>,
    pub info: CertInfo,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// The provider every floe listener uses (ring, as before D59).
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Parse a PEM chain (leaf first) and its private key; refuse a key that does not belong to
/// the leaf, an empty chain, or a leaf that does not parse as X.509.
pub fn load_pem(cert_pem: &str, key_pem: &str) -> Result<Loaded> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .context("parsing TLS certificate PEM")?;
    let leaf = certs
        .first()
        .context("TLS certificate PEM holds no certificate")?
        .clone();
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .context("parsing TLS private key PEM")?
        .context("TLS key PEM holds no private key")?;
    let signing = provider()
        .key_provider
        .load_private_key(key)
        .context("unsupported TLS private key")?;
    let ck = CertifiedKey::new(certs, signing);
    ck.keys_match()
        .context("the TLS private key does not belong to the certificate")?;
    let info = cert_info(&leaf)?;
    Ok(Loaded {
        key: Arc::new(ck),
        info,
    })
}

/// Expiry, issuer, SANs and fingerprint of a DER certificate.
pub fn cert_info(der: &CertificateDer<'_>) -> Result<CertInfo> {
    let (_, cert) = x509_parser::parse_x509_certificate(der.as_ref())
        .map_err(|e| anyhow::anyhow!("TLS certificate is not valid X.509: {e}"))?;
    let mut sans = Vec::new();
    if let Ok(Some(ext)) = cert.subject_alternative_name() {
        for n in &ext.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(d) = n {
                sans.push((*d).to_string());
            }
        }
    }
    Ok(CertInfo {
        sans,
        issuer: cert.issuer().to_string(),
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        fingerprint: format!("sha256:{}", hex::encode(sha2::Sha256::digest(der.as_ref()))),
    })
}

/// The swappable certificate. Empty until the first load: a handshake then fails (no
/// certificate to offer) and `/readyz` says 503, which is the contract in `acme` mode.
#[derive(Default)]
pub struct CertResolver {
    current: RwLock<Option<Arc<Loaded>>>,
}

impl std::fmt::Debug for CertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertResolver")
            .field("current", &self.current().map(|l| l.info.clone()))
            .finish()
    }
}

impl CertResolver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Present `loaded` from the next handshake on.
    pub fn set(&self, loaded: Loaded) {
        let fp = loaded.info.fingerprint.clone();
        let not_after = loaded.info.not_after;
        let mut g = match self.current.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *g = Some(Arc::new(loaded));
        drop(g);
        metrics::gauge!("floe_tls_cert_not_after_seconds").set(i64_to_f64(not_after));
        tracing::info!(fingerprint = %fp, not_after, "TLS certificate installed");
    }

    pub fn current(&self) -> Option<Arc<Loaded>> {
        match self.current.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.current().is_some()
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.current().map(|l| l.key.clone())
    }
}

/// A rustls server config presenting whatever `resolver` holds, ALPN `h2` + `http/1.1`.
pub fn server_config(resolver: Arc<CertResolver>) -> Result<rustls::ServerConfig> {
    let mut sc = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    sc.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(sc)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "unix seconds as a metrics gauge; exact below 2^53"
)]
pub(crate) fn i64_to_f64(v: i64) -> f64 {
    v as f64
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A self-issued pair for tests only (rcgen is a dev-dependency).
    pub(crate) fn pair(names: &[&str]) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params =
            rcgen::CertificateParams::new(names.iter().map(ToString::to_string).collect::<Vec<_>>())
                .unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn resolver_swaps_certificates_in_place() {
        let r = CertResolver::new();
        assert!(!r.is_loaded());
        let (c1, k1) = pair(&["a.example.com"]);
        let (c2, k2) = pair(&["b.example.com"]);
        let l1 = load_pem(&c1, &k1).unwrap();
        let fp1 = l1.info.fingerprint.clone();
        assert_eq!(l1.info.sans, vec!["a.example.com"]);
        assert!(l1.info.not_after > l1.info.not_before);
        r.set(l1);
        let held = r.current().unwrap(); // a connection mid-handshake holds this one
        r.set(load_pem(&c2, &k2).unwrap());
        let now = r.current().unwrap();
        assert_ne!(now.info.fingerprint, fp1, "new handshakes see the new certificate");
        assert_eq!(held.info.fingerprint, fp1, "the old one stays valid for its holder");
        assert!(server_config(r).is_ok());
    }

    #[test]
    fn mismatched_or_empty_material_is_refused() {
        let (c1, _) = pair(&["a.example.com"]);
        let (_, k2) = pair(&["b.example.com"]);
        assert!(
            load_pem(&c1, &k2).unwrap_err().to_string().contains("does not belong"),
            "key of another certificate"
        );
        assert!(load_pem("", &k2).is_err());
        assert!(load_pem(&c1, "").is_err());
    }

    /// Client trusting exactly the certificate it expects, then the next one.
    async fn fingerprint_seen(addr: std::net::SocketAddr, trust_pem: &str) -> Option<String> {
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut trust_pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        let cfg = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("floe.test").unwrap();
        let mut tls = conn.connect(name, tcp).await.ok()?;
        tls.write_all(b"x").await.ok()?;
        let mut b = [0u8; 1];
        tls.read_exact(&mut b).await.ok()?;
        let der = tls.get_ref().1.peer_certificates()?.first()?.clone();
        Some(cert_info(&der).unwrap().fingerprint)
    }

    #[tokio::test]
    async fn handshakes_pick_up_a_swapped_certificate() {
        let r = CertResolver::new();
        let (c1, k1) = pair(&["floe.test"]);
        r.set(load_pem(&c1, &k1).unwrap());
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(r.clone()).unwrap()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(s).await {
                        let mut b = [0u8; 1];
                        let _ = tls.read_exact(&mut b).await;
                        let _ = tls.write_all(b"k").await;
                        let _ = tls.shutdown().await;
                    }
                });
            }
        });
        let fp1 = fingerprint_seen(addr, &c1).await.expect("first certificate served");
        let (c2, k2) = pair(&["floe.test"]);
        r.set(load_pem(&c2, &k2).unwrap());
        assert!(fingerprint_seen(addr, &c1).await.is_none(), "old certificate no longer offered");
        let fp2 = fingerprint_seen(addr, &c2).await.expect("swapped certificate served");
        assert_ne!(fp1, fp2);
    }
}
