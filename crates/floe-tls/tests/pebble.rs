//! The ACME flow end to end against Pebble (letsencrypt/pebble) with pebble-challtestsrv as
//! the DNS: account registration, an order for a name and its wildcard, DNS-01 records
//! through a [`DnsProvider`], the propagation check against the fake DNS, finalize, the
//! sealed state in the bucket, a second instance picking the certificate up by conditional
//! GET, a renewal hot-swapped into a running resolver, and a real TLS handshake that chains
//! to Pebble's root.
//!
//! `#[ignore]`d: it needs the two services. CI runs it in its own job (`acme-pebble`, both
//! arches) with:
//!
//! ```text
//! FLOE_TEST_PEBBLE_DIRECTORY=https://localhost:14000/dir
//! FLOE_TEST_PEBBLE_CA=<pebble.minica.pem from the image>   # trusts the ACME API
//! FLOE_TEST_PEBBLE_MANAGEMENT=https://localhost:15000      # /roots/0 = the issuing root
//! FLOE_TEST_CHALLTESTSRV=http://127.0.0.1:8055
//! FLOE_TEST_PEBBLE_DNS=127.0.0.1:8053
//! cargo test -p floe-tls --test pebble -- --ignored
//! ```
//! Without them the test prints why and passes (so `just test-slow` stays usable).
// Integration tests fail by panicking; clippy.toml's allow-*-in-tests only reaches #[test] fns,
// not the helpers around them, so the panic-path lints are lifted for the whole test crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a panic is how a test fails"
)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use floe_config::AcmeConfig;
use floe_store::{DynStore, ObjectStoreExt, memory::MemoryStore};
use floe_tls::LogNarrator;
use floe_tls::acme::{AcmeManager, Tick};
use floe_tls::dns::{DnsProvider, TxtRecord};
use floe_tls::resolver::{CertResolver, cert_info, provider, server_config};
use floe_tls::seal::SealKey;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

struct Env {
    directory: String,
    ca: String,
    management: String,
    challtestsrv: String,
    dns: String,
}

fn env() -> Option<Env> {
    let v = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    Some(Env {
        directory: v("FLOE_TEST_PEBBLE_DIRECTORY")?,
        ca: v("FLOE_TEST_PEBBLE_CA")?,
        management: v("FLOE_TEST_PEBBLE_MANAGEMENT")?,
        challtestsrv: v("FLOE_TEST_CHALLTESTSRV")?,
        dns: v("FLOE_TEST_PEBBLE_DNS")?,
    })
}

/// pebble-challtestsrv's management API as the DNS provider.
struct ChallTestSrv {
    base: String,
    http: reqwest::Client,
}

#[async_trait]
impl DnsProvider for ChallTestSrv {
    fn name(&self) -> &'static str {
        "challtestsrv"
    }
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        self.http
            .post(format!("{}/set-txt", self.base))
            .json(&serde_json::json!({"host": format!("{fqdn}."), "value": value}))
            .send()
            .await?
            .error_for_status()?;
        Ok(TxtRecord {
            fqdn: fqdn.into(),
            value: value.into(),
            id: String::new(),
            zone: String::new(),
        })
    }
    async fn delete_txt(&self, r: &TxtRecord) -> anyhow::Result<()> {
        self.http
            .post(format!("{}/clear-txt", self.base))
            .json(&serde_json::json!({"host": format!("{}.", r.fqdn)}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

fn manager(
    e: &Env,
    store: &DynStore,
    resolver: Arc<CertResolver>,
    http: &reqwest::Client,
    renew_before: Duration,
) -> Arc<AcmeManager> {
    let cfg = AcmeConfig {
        domains: vec!["floe.test".into(), "*.floe.test".into()],
        email: "ops@floe.test".into(),
        directory: e.directory.clone(),
        renew_before,
        resolvers: vec![e.dns.clone()],
        propagation_timeout: Duration::from_mins(1),
        poll_interval: Duration::from_secs(1),
        ..AcmeConfig::default()
    };
    let dns = Arc::new(ChallTestSrv {
        base: e.challtestsrv.clone(),
        http: reqwest::Client::new(),
    });
    AcmeManager::new(
        &cfg,
        store.clone(),
        resolver,
        SealKey::parse(KEY).unwrap(),
        dns,
        http.clone(),
    )
    .unwrap()
}

/// Serve `resolver` on a loopback port; return the leaf fingerprint a client trusting
/// `root_pem` sees for SNI `name`.
async fn handshake(resolver: Arc<CertResolver>, root_pem: &str, name: &str) -> String {
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(resolver).unwrap()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut tls = acceptor.accept(sock).await.unwrap();
        let mut byte = [0u8; 1];
        tls.read_exact(&mut byte).await.unwrap();
        tls.write_all(b"k").await.unwrap();
        tls.shutdown().await.unwrap();
    });
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut root_pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let cfg = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(
            rustls::pki_types::ServerName::try_from(name.to_string()).unwrap(),
            tcp,
        )
        .await
        .expect("the issued chain verifies against Pebble's root");
    tls.write_all(b"x").await.unwrap();
    let mut b = [0u8; 1];
    tls.read_exact(&mut b).await.unwrap();
    let der = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
    cert_info(&der).unwrap().fingerprint
}

#[tokio::test]
#[ignore = "needs Pebble + pebble-challtestsrv (CI job acme-pebble)"]
async fn pebble_dns01_order_share_and_renew() {
    let Some(pebble) = env() else {
        eprintln!("FLOE_TEST_PEBBLE_* not set; skipping the Pebble ACME test");
        return;
    };
    let ca = std::fs::read(&pebble.ca).unwrap();
    let http = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .build()
        .unwrap();
    let store: DynStore = MemoryStore::shared();

    // Instance A orders the first certificate.
    let ra = CertResolver::new();
    let inst_a = manager(&pebble, &store, ra.clone(), &http, Duration::from_hours(24));
    assert!(inst_a.due(), "nothing loaded yet");
    assert!(!inst_a.status().loaded);
    let tick = inst_a.tick(&LogNarrator).await.unwrap();
    assert_eq!(tick, Tick::Issued, "{:?}", inst_a.status());
    let la = ra.current().expect("installed");
    let mut sans = la.info.sans.clone();
    sans.sort();
    assert_eq!(sans, vec!["*.floe.test", "floe.test"]);
    // Pebble's validity depends on its profile (90 days, or 6 for `shortlived`); one day of
    // renew_before is inside either.
    eprintln!(
        "issued: {} .. {} by {}",
        la.info.not_before, la.info.not_after, la.info.issuer
    );
    assert!(
        !inst_a.due(),
        "a fresh certificate is not due with renew_before = 1 day"
    );
    let st = inst_a.status();
    assert!(
        st.loaded && st.last_error.is_none() && st.last_renewal_at.is_some(),
        "{st:?}"
    );

    // Bucket state: sealed keys only.
    let (_, cert_json) = store.get_bytes(&inst_a.cert_key()).await.unwrap().unwrap();
    let cert_json = String::from_utf8(cert_json.to_vec()).unwrap();
    assert!(cert_json.contains("BEGIN CERTIFICATE"));
    assert!(
        !cert_json.contains("PRIVATE KEY"),
        "the private key is sealed"
    );
    let mut accounts = 0;
    let keys: Vec<String> = {
        let mut stream = store.list("tls/", None);
        let mut found = Vec::new();
        while let Some(meta) = stream.next().await {
            found.push(meta.unwrap().key);
        }
        found
    };
    for key in &keys {
        if key.ends_with("account.json") {
            accounts += 1;
            let (_, body) = store.get_bytes(key).await.unwrap().unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(
                !body.contains("key_pkcs8"),
                "the account key is sealed: {body}"
            );
        }
    }
    assert_eq!(accounts, 1, "{keys:?}");

    // The issued chain verifies against Pebble's root, for the name and the wildcard.
    let root = http
        .get(format!("{}/roots/0", pebble.management))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let fp = handshake(ra.clone(), &root, "floe.test").await;
    assert_eq!(fp, la.info.fingerprint);
    handshake(ra.clone(), &root, "git.floe.test").await;

    // Instance B (same bucket) picks it up by conditional GET; it does not order.
    let rb = CertResolver::new();
    let inst_b = manager(&pebble, &store, rb.clone(), &http, Duration::from_hours(24));
    assert_eq!(inst_b.tick(&LogNarrator).await.unwrap(), Tick::Fresh);
    assert_eq!(rb.current().unwrap().info.fingerprint, la.info.fingerprint);
    assert!(
        !inst_b.refresh().await.unwrap(),
        "unchanged object: 304, nothing installed"
    );

    // Instance C has the widest renew_before config allows (60 days), longer than some CA
    // profiles' lifetime: the fresh certificate is still not due (renewal never starts before
    // two thirds of the lifetime), so no re-order loop. A forced renewal then swaps in on A
    // and B at their next revalidation — no restart.
    let rc = CertResolver::new();
    let inst_c = manager(
        &pebble,
        &store,
        rc.clone(),
        &http,
        Duration::from_hours(60 * 24),
    );
    assert_eq!(
        inst_c.tick(&LogNarrator).await.unwrap(),
        Tick::Fresh,
        "fresh is never due"
    );
    assert_eq!(inst_c.renew_now(&LogNarrator).await.unwrap(), Tick::Issued);
    assert_eq!(
        inst_c.tick(&LogNarrator).await.unwrap(),
        Tick::Fresh,
        "a just-issued certificate does not trigger another order"
    );
    let renewed = rc.current().unwrap().info.fingerprint.clone();
    assert_ne!(renewed, la.info.fingerprint);
    assert!(inst_a.refresh().await.unwrap());
    assert_eq!(ra.current().unwrap().info.fingerprint, renewed);
    assert_eq!(handshake(ra.clone(), &root, "floe.test").await, renewed);
    assert!(inst_b.refresh().await.unwrap());
    assert_eq!(rb.current().unwrap().info.fingerprint, renewed);
    // Every instance sees the shared status of C's success.
    let shared = inst_b.refresh_status().await;
    assert_eq!(shared.failures, 0);
    assert!(shared.last_success_unix.is_some());
}
