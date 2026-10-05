//! The Cloudflare DNS-01 provider against a mocked API v4 (axum): zone lookup from the
//! domain, explicit `zone_id`, TXT create and delete, error envelopes, and that the token
//! never leaks into an error.
// Integration tests fail by panicking; clippy.toml's allow-*-in-tests only reaches #[test] fns,
// not the helpers around them, so the panic-path lints are lifted for the whole test crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test code: a panic is how a test fails"
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use floe_tls::cloudflare::Cloudflare;
use floe_tls::dns::DnsProvider;
use serde_json::{Value, json};

const TOKEN: &str = "cf-test-token-0123456789";

#[derive(Default)]
struct Mock {
    /// (zone, id, name, content)
    records: Vec<(String, String, String, String)>,
    zone_queries: Vec<String>,
    next: u32,
}

type St = Arc<Mutex<Mock>>;

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

fn denied() -> (StatusCode, Json<Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(
            json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}], "result": null}),
        ),
    )
}

async fn zones(
    State(st): State<St>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    if !authed(&h) {
        return denied();
    }
    let name = q.get("name").cloned().unwrap_or_default();
    st.lock().unwrap().zone_queries.push(name.clone());
    let result = if name == "example.com" {
        json!([{"id": "zone123", "name": "example.com"}])
    } else {
        json!([])
    };
    (
        StatusCode::OK,
        Json(json!({"success": true, "errors": [], "result": result})),
    )
}

async fn create(
    State(st): State<St>,
    h: HeaderMap,
    Path(zone): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authed(&h) {
        return denied();
    }
    if body["type"] != "TXT" || body["ttl"] != 60 {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"success": false, "errors": [{"code": 9000, "message": "bad record"}], "result": null}),
            ),
        );
    }
    let mut m = st.lock().unwrap();
    m.next += 1;
    let id = format!("rec{}", m.next);
    m.records.push((
        zone,
        id.clone(),
        body["name"].as_str().unwrap().to_string(),
        body["content"].as_str().unwrap().to_string(),
    ));
    (
        StatusCode::OK,
        Json(json!({"success": true, "errors": [], "result": {"id": id}})),
    )
}

async fn remove(
    State(st): State<St>,
    h: HeaderMap,
    Path((zone, id)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    if !authed(&h) {
        return denied();
    }
    let mut m = st.lock().unwrap();
    let before = m.records.len();
    m.records.retain(|r| !(r.0 == zone && r.1 == id));
    if m.records.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({"success": false, "errors": [{"code": 81044, "message": "Record does not exist."}], "result": null}),
            ),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"success": true, "errors": [], "result": {"id": id}})),
    )
}

async fn mock() -> (String, St) {
    let st: St = Arc::default();
    let app = axum::Router::new()
        .route("/client/v4/zones", get(zones))
        .route("/client/v4/zones/{zone}/dns_records", post(create))
        .route("/client/v4/zones/{zone}/dns_records/{id}", delete(remove))
        .with_state(st.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/client/v4"), st)
}

#[tokio::test]
async fn zone_lookup_create_and_delete() {
    let (api, st) = mock().await;
    let cf = Cloudflare::new(&api, TOKEN, None, reqwest::Client::new());
    let r1 = cf
        .create_txt("_acme-challenge.git.example.com", "v1")
        .await
        .unwrap();
    let r2 = cf
        .create_txt("_acme-challenge.git.example.com", "v2")
        .await
        .unwrap();
    assert_eq!(r1.zone, "zone123");
    assert_ne!(
        r1.id, r2.id,
        "two values at one name are two records (wildcard + base)"
    );
    {
        let m = st.lock().unwrap();
        assert_eq!(
            m.zone_queries,
            vec!["git.example.com", "example.com"],
            "walks from the record's parent to the zone apex, then caches"
        );
        assert_eq!(m.records.len(), 2);
        assert!(
            m.records
                .iter()
                .all(|r| r.2 == "_acme-challenge.git.example.com")
        );
    }
    cf.delete_txt(&r1).await.unwrap();
    cf.delete_txt(&r1).await.unwrap(); // already gone = success
    cf.delete_txt(&r2).await.unwrap();
    assert!(st.lock().unwrap().records.is_empty());
}

#[tokio::test]
async fn explicit_zone_id_skips_the_lookup() {
    let (api, st) = mock().await;
    let cf = Cloudflare::new(&api, TOKEN, Some("zone123".into()), reqwest::Client::new());
    let r = cf
        .create_txt("_acme-challenge.example.com", "x")
        .await
        .unwrap();
    assert_eq!(r.zone, "zone123");
    assert!(st.lock().unwrap().zone_queries.is_empty());
}

#[tokio::test]
async fn errors_name_the_problem_never_the_token() {
    let (api, _) = mock().await;
    let bad = Cloudflare::new(&api, "wrong-token", None, reqwest::Client::new());
    let e = format!(
        "{:#}",
        bad.create_txt("_acme-challenge.example.com", "x")
            .await
            .unwrap_err()
    );
    assert!(e.contains("Authentication error"), "{e}");
    assert!(!e.contains("wrong-token"), "{e}");

    let cf = Cloudflare::new(&api, TOKEN, None, reqwest::Client::new());
    let e = format!(
        "{:#}",
        cf.create_txt("_acme-challenge.other.org", "x")
            .await
            .unwrap_err()
    );
    assert!(e.contains("no active Cloudflare zone"), "{e}");
    assert!(!e.contains(TOKEN));
    assert!(
        !format!("{cf:?}").contains(TOKEN),
        "Debug redacts the token"
    );
}
