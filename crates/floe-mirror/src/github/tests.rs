//! `GithubSource` against an in-process stub of the REST API on loopback.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use parking_lot::Mutex;

use super::*;

#[derive(Default)]
struct Stub {
    /// Per path: how many times it was asked.
    hits: Mutex<BTreeMap<String, u32>>,
    /// Send `Link` on 304s of page 1.
    link_on_304: std::sync::atomic::AtomicBool,
    /// Rate-limit every listing request with `retry-after`.
    limited: std::sync::atomic::AtomicBool,
    /// `x-ratelimit-remaining` reported on every response.
    remaining: AtomicU32,
}

fn repo_json(id: u64, owner: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "name": name, "owner": {"login": owner}, "private": false,
        "archived": false, "fork": false, "default_branch": "main",
        "pushed_at": "2026-10-04T00:00:00Z", "size": 1, "extra": "dropped"
    })
}

fn respond(
    st: &Stub,
    path: &str,
    headers: &HeaderMap,
    etag: &str,
    body: serde_json::Value,
    link: Option<String>,
) -> Response {
    *st.hits.lock().entry(path.to_string()).or_default() += 1;
    let remaining = st.remaining.load(Ordering::SeqCst).to_string();
    let not_modified = headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    let mut h = HeaderMap::new();
    h.insert("x-ratelimit-remaining", remaining.parse().unwrap());
    h.insert("x-ratelimit-reset", "2000000000".parse().unwrap());
    h.insert("etag", etag.parse().unwrap());
    if not_modified {
        if st.link_on_304.load(Ordering::SeqCst)
            && let Some(l) = &link
        {
            h.insert("link", l.parse().unwrap());
        }
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    if let Some(l) = link {
        h.insert("link", l.parse().unwrap());
    }
    (StatusCode::OK, h, axum::Json(body)).into_response()
}

async fn serve(stub: Arc<Stub>) -> String {
    async fn user(State(st): State<Arc<Stub>>, h: HeaderMap) -> Response {
        respond(&st, "/user", &h, "\"u1\"", serde_json::json!({"login": "me"}), None)
    }
    async fn repos(State(st): State<Arc<Stub>>, h: HeaderMap, uri: axum::http::Uri) -> Response {
        if st.limited.load(Ordering::SeqCst) {
            return (
                StatusCode::FORBIDDEN,
                [("retry-after", "120")],
                axum::Json(serde_json::json!({"message": "slow down"})),
            )
                .into_response();
        }
        let page2 = uri.query().is_some_and(|q| q.contains("page=2"));
        let base = format!("http://{}", h.get("host").and_then(|v| v.to_str().ok()).unwrap_or(""));
        if page2 {
            respond(&st, "/user/repos?page=2", &h, "\"p2\"", serde_json::json!([repo_json(2, "me", "b")]), None)
        } else {
            let link = format!("<{base}/user/repos?page=2>; rel=\"next\"");
            respond(&st, "/user/repos", &h, "\"p1\"", serde_json::json!([repo_json(1, "me", "a")]), Some(link))
        }
    }
    async fn by_id(State(st): State<Arc<Stub>>, Path(id): Path<String>) -> Response {
        *st.hits.lock().entry(format!("/repositories/{id}")).or_default() += 1;
        match id.as_str() {
            "404" => (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"message": "Not Found"}))).into_response(),
            "403" => (
                StatusCode::FORBIDDEN,
                [("x-ratelimit-remaining", "4000")],
                axum::Json(serde_json::json!({"message": "Resource not accessible by personal access token"})),
            )
                .into_response(),
            _ => (
                StatusCode::FORBIDDEN,
                [("x-ratelimit-remaining", "4000")],
                axum::Json(serde_json::json!({"message": "You have exceeded a secondary rate limit"})),
            )
                .into_response(),
        }
    }
    let app = Router::new()
        .route("/user", get(user))
        .route("/user/repos", get(repos))
        .route("/repositories/{id}", get(by_id))
        .with_state(stub);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn source(api: &str, min_rate: u32) -> GithubSource {
    let cfg = GithubMirrorConfig {
        api_url: api.to_string(),
        min_rate_remaining: min_rate,
        ..GithubMirrorConfig::default()
    };
    let mut s = GithubSource::new(&cfg).unwrap();
    s.fixed_token = Some("t0ken".into());
    s
}

fn me() -> Selection {
    Selection {
        users: vec!["@me".into()],
        ..Selection::default()
    }
}

#[tokio::test]
async fn paginates_and_reuses_etags_with_and_without_link_on_304() {
    let stub = Arc::new(Stub {
        remaining: AtomicU32::new(4000),
        ..Stub::default()
    });
    let api = serve(stub.clone()).await;
    let src = source(&api, 200);
    let mut cache = HttpCache::default();
    let d = src.discover(&me(), &mut cache).await.unwrap();
    assert!(d.complete);
    assert_eq!(d.login.as_deref(), Some("me"));
    let names: Vec<String> = d.repos.iter().map(RemoteRepo::full_name).collect();
    assert_eq!(names, ["me/a", "me/b"]);
    assert_eq!(d.stats.not_modified, 0);

    // Second pass: every page a 304; page 1's 304 carries no Link, so the
    // cached `next` drives pagination.
    let d = src.discover(&me(), &mut cache).await.unwrap();
    assert!(d.complete);
    assert_eq!(d.repos.len(), 2);
    assert_eq!(d.stats.not_modified, 3);
    // And with a Link on the 304.
    stub.link_on_304.store(true, Ordering::SeqCst);
    let d = src.discover(&me(), &mut cache).await.unwrap();
    assert_eq!(d.repos.len(), 2);
    assert_eq!(stub.hits.lock().get("/user/repos?page=2"), Some(&3));
}

#[tokio::test]
async fn rate_limits_end_the_discovery_incomplete() {
    let stub = Arc::new(Stub {
        remaining: AtomicU32::new(4000),
        ..Stub::default()
    });
    let api = serve(stub.clone()).await;
    let mut cache = HttpCache::default();
    // retry-after on a listing.
    stub.limited.store(true, Ordering::SeqCst);
    let d = source(&api, 200).discover(&me(), &mut cache).await.unwrap();
    assert!(!d.complete);
    assert!(d.stats.rate_limited_until.is_some());
    // The floor: remaining below min_rate_remaining stops the pass.
    stub.limited.store(false, Ordering::SeqCst);
    stub.remaining.store(100, Ordering::SeqCst);
    let d = source(&api, 200).discover(&me(), &mut cache).await.unwrap();
    assert!(!d.complete);
    assert!(
        d.stats.rate_limited_until.is_some(),
        "the loop sleeps until the reset"
    );
}

#[test]
fn only_api_url_gets_the_token() {
    let src = source("https://api.github.com", 0);
    assert!(src.on_api("https://api.github.com/user/repos?page=2"));
    assert!(!src.on_api("https://api.github.com.evil.example/user/repos"));
    assert!(!src.on_api("https://evil.example/user/repos"));
}

#[tokio::test]
async fn lookups_distinguish_gone_forbidden_and_rate_limited() {
    let stub = Arc::new(Stub::default());
    let api = serve(stub).await;
    let src = source(&api, 0);
    let mut cache = HttpCache::default();
    assert_eq!(src.lookup("404", &mut cache).await.unwrap(), Lookup::Gone);
    assert_eq!(src.lookup("403", &mut cache).await.unwrap(), Lookup::Forbidden);
    // A secondary limit without headers is a limit (≥ 60 s), never Forbidden.
    let before = std::time::SystemTime::now();
    match src.lookup("1", &mut cache).await {
        Err(SourceError::RateLimited { until }) => {
            assert!(until >= before + std::time::Duration::from_secs(59));
        }
        other => panic!("{other:?}"),
    }
}
