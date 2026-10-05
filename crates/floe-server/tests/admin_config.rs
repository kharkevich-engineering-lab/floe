//! D60–D62: the admin API over HTTP — authorization on every route, fail-closed
//! publish (400, nothing written), CAS (409), history/rollback, schema, the
//! mirror's pause/resume as config changes, and a second instance applying a
//! revision by revalidation.
// Integration tests fail by panicking; clippy.toml's allow-*-in-tests only reaches #[test] fns,
// not the helpers around them, so the panic-path lints are lifted for the whole test crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "test code: a panic is how a test fails"
)]
mod harness;

use harness::Server;
use serde_json::{Value, json};

type TestResult = anyhow::Result<()>;

const ADMIN: &str = "Bearer admin-token";
const WRITER: &str = "Bearer writer-token";

async fn start() -> anyhow::Result<Server> {
    Server::start_with_tweak(|c| {
        c.server.auth.mode = floe_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            floe_config::StaticToken {
                principal: "writer".into(),
                token: "writer-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
            floe_config::StaticToken {
                principal: "admin".into(),
                token: "admin-token".into(),
                token_env: None,
                write: true,
                admin: true,
            },
        ];
    })
    .await
}

async fn call(
    server: &Server,
    method: reqwest::Method,
    path: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> anyhow::Result<(u16, Value)> {
    let mut r = reqwest::Client::new()
        .request(method, format!("{}{path}", server.base_url))
        .header("Accept", "application/json");
    if let Some(a) = auth {
        r = r.header("Authorization", a);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await?;
    Ok((
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    ))
}

const GETS: &[&str] = &[
    "/api/v1/admin/config",
    "/api/v1/admin/config/schema",
    "/api/v1/admin/config/history",
    "/api/v1/admin/overview",
    "/api/v1/admin/mirror",
    "/api/v1/admin/catalog",
    "/api-browser/v1/admin/config",
];
const POSTS: &[&str] = &[
    "/api/v1/admin/config/validate",
    "/api/v1/admin/config/rollback",
    "/api/v1/admin/mirror/preview",
    "/api/v1/admin/mirror/test",
    "/api/v1/admin/mirror/sync",
    "/api/v1/admin/mirror/pause",
    "/api/v1/admin/mirror/resume",
    "/api/v1/admin/catalog/test",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_admin_route_needs_an_admin() -> TestResult {
    let server = start().await?;
    for path in GETS {
        let (s, _) = call(&server, reqwest::Method::GET, path, None, None).await?;
        assert_eq!(s, 401, "GET {path} without a credential");
        let (s, _) = call(&server, reqwest::Method::GET, path, Some(WRITER), None).await?;
        assert_eq!(s, 403, "GET {path} as a writer");
        let (s, body) = call(&server, reqwest::Method::GET, path, Some(ADMIN), None).await?;
        assert_eq!(s, 200, "GET {path} as admin: {body}");
    }
    let (s, _) = call(
        &server,
        reqwest::Method::PUT,
        "/api/v1/admin/config",
        Some(WRITER),
        Some(json!({"document": {}})),
    )
    .await?;
    assert_eq!(s, 403);
    for path in POSTS {
        let (s, _) = call(&server, reqwest::Method::POST, path, None, Some(json!({}))).await?;
        assert_eq!(s, 401, "POST {path} without a credential");
        let (s, _) = call(
            &server,
            reqwest::Method::POST,
            path,
            Some(WRITER),
            Some(json!({})),
        )
        .await?;
        assert_eq!(s, 403, "POST {path} as a writer");
    }
    // The SPA's switch.
    let (_, me) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/me",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(me["admin"], true);
    let (_, me) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/me",
        Some(WRITER),
        None,
    )
    .await?;
    assert_eq!(me["admin"], false);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_fails_closed_and_is_cas() -> TestResult {
    let server = start().await?;
    let put = |doc: Value, base: Option<u64>| {
        let server = &server;
        async move {
            call(
                server,
                reqwest::Method::PUT,
                "/api/v1/admin/config",
                Some(ADMIN),
                Some(json!({"document": doc, "message": "t", "base_revision": base})),
            )
            .await
        }
    };
    // Invalid: 400 with the field, nothing published.
    let (s, body) = put(json!({"github_mirror": {"enabled": true, "private_visible_to_all_readers": true, "include": ["nope"]}}), Some(0)).await?;
    assert_eq!(s, 400, "{body}");
    assert_eq!(body["errors"][0]["path"], "github_mirror.include", "{body}");
    let (s, body) = put(json!({"server": {"listen": "0.0.0.0:1"}}), Some(0)).await?;
    assert_eq!(s, 400, "{body}");
    let (_, cur) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/config",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(cur["revision"], 0, "nothing was published");
    assert_eq!(
        cur["document"]["github_mirror"]["token"]["env"],
        "FLOE_GITHUB_TOKEN"
    );
    // A secret value without FLOE_CONFIG_KEY on the instance: refused.
    let (s, body) = put(
        json!({"events": {"webhook_secret": {"value": "s"}}}),
        Some(0),
    )
    .await?;
    assert_eq!(s, 400, "{body}");
    assert_eq!(body["errors"][0]["path"], "events.webhook_secret");
    // Valid.
    let (s, body) = put(
        json!({"events": {"webhook_url": "https://hooks.example/x"}}),
        Some(0),
    )
    .await?;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(body["restart_required"][0], "events.webhook_url");
    // Stale base: 409 with the current revision.
    let (s, body) = put(json!({}), Some(0)).await?;
    assert_eq!(s, 409, "{body}");
    assert_eq!(body["revision"], 1);
    // Validate does not write.
    let (s, body) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/config/validate",
        Some(ADMIN),
        Some(json!({"document": {"events": {"webhook_url": "ftp://x"}}})),
    )
    .await?;
    assert_eq!(s, 200);
    assert_eq!(body["ok"], false, "{body}");
    // History and rollback.
    let (s, _) = put(json!({"events": {"sweep_interval": "1m"}}), Some(1)).await?;
    assert_eq!(s, 200);
    let (_, h) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/config/history",
        Some(ADMIN),
        None,
    )
    .await?;
    let revs: Vec<u64> = h["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["revision"].as_u64().unwrap())
        .collect();
    assert_eq!(revs, vec![2, 1]);
    let (s, r1) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/config/revisions/1",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(s, 200);
    assert_eq!(
        r1["document"]["events"]["webhook_url"],
        "https://hooks.example/x"
    );
    let (s, body) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/config/rollback",
        Some(ADMIN),
        Some(json!({"revision": 1, "base_revision": 2})),
    )
    .await?;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["revision"], 3);
    let (_, cur) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/config",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(cur["rolled_back_from"], 1);
    assert_eq!(cur["author"], "admin");
    assert_eq!(
        cur["applied"]["revision"], 3,
        "the publishing instance applies at once"
    );
    let (s, _) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/config/revisions/99",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(s, 404);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_pause_resume_and_a_second_instance() -> TestResult {
    let server = start().await?;
    let (_, schema) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/config/schema",
        Some(ADMIN),
        None,
    )
    .await?;
    let token = &schema["properties"]["github_mirror"]["properties"]["token"];
    assert_eq!(token["x-floe"]["format"], "secret", "{schema}");
    assert!(schema["properties"]["catalog"]["properties"]["uri"].is_object());

    let (s, body) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/mirror/pause",
        Some(ADMIN),
        Some(json!({"full_name": "Acme/Widgets"})),
    )
    .await?;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["revision"], 1);
    let (_, cur) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/config",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(
        cur["document"]["github_mirror"]["exclude"],
        json!(["Acme/Widgets"])
    );
    assert_eq!(cur["message"], "pause Acme/Widgets");
    // Pausing twice changes nothing.
    let (_, body) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/mirror/pause",
        Some(ADMIN),
        Some(json!({"full_name": "acme/widgets"})),
    )
    .await?;
    assert_eq!(body["unchanged"], true);
    let (s, _) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/mirror/pause",
        Some(ADMIN),
        Some(json!({"full_name": "acme/*"})),
    )
    .await?;
    assert_eq!(s, 400);

    // Another instance on the same bucket picks the revision up by revalidating.
    let sibling = server.start_sibling_with(|_| {}).await?;
    assert_eq!(
        sibling.state.config.current().revision,
        1,
        "started on the current revision"
    );
    let (s, _) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/mirror/resume",
        Some(ADMIN),
        Some(json!({"full_name": "acme/widgets"})),
    )
    .await?;
    assert_eq!(s, 200);
    assert_eq!(sibling.state.config.revalidate().await, 2);
    assert!(
        sibling
            .state
            .config
            .current()
            .cfg
            .github_mirror
            .exclude
            .is_empty()
    );

    // The overview reports this instance and the config revision.
    let (_, ov) = call(
        &server,
        reqwest::Method::GET,
        "/api/v1/admin/overview",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(ov["config"]["revision"], 2, "{ov}");
    assert_eq!(ov["instance"]["applied_revision"], 2, "{ov}");
    assert_eq!(ov["catalog"]["enabled"], false);
    // "Sync now" writes the request object the mirror supervisors poll.
    let (s, _) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/mirror/sync",
        Some(ADMIN),
        None,
    )
    .await?;
    assert_eq!(s, 200);
    assert!(!server.store.is_empty());
    // Preview with nothing known yet: an empty selection, no forge call.
    let (s, pv) = call(
        &server,
        reqwest::Method::POST,
        "/api/v1/admin/mirror/preview",
        Some(ADMIN),
        Some(json!({"section": {"include": ["acme/*"]}})),
    )
    .await?;
    assert_eq!(s, 200, "{pv}");
    assert_eq!(pv["known"], 0);
    Ok(())
}

/// D61 / security review: a test endpoint never sends an env var the caller
/// names, never sends the stored token to another URL, and never probes an
/// IMDS-style or fragment URL; nothing reflects a probed body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_endpoints_cannot_exfiltrate_secrets() -> TestResult {
    let server = start().await?;
    let post = |path: &'static str, body: Value| {
        let server = &server;
        async move { call(server, reqwest::Method::POST, path, Some(ADMIN), Some(body)).await }
    };
    let (s, b) = post(
        "/api/v1/admin/mirror/test",
        json!({"token": {"env": "FLOE_CONFIG_KEY"}}),
    )
    .await?;
    assert_eq!(s, 400, "{b}");
    let (s, b) = post(
        "/api/v1/admin/mirror/test",
        json!({"api_url": "https://evil.example", "token": {"env": "AWS_SECRET_ACCESS_KEY"}}),
    )
    .await?;
    assert_eq!(s, 400, "{b}");
    let (s, b) = post(
        "/api/v1/admin/mirror/test",
        json!({"api_url": "https://evil.example"}),
    )
    .await?;
    assert_eq!(
        s, 400,
        "the stored token only goes to the configured api_url: {b}"
    );
    let (s, b) = post(
        "/api/v1/admin/mirror/test",
        json!({"api_url": "http://169.254.169.254", "token": {"value": "x"}}),
    )
    .await?;
    assert_eq!(s, 400, "{b}");
    let (s, b) = post(
        "/api/v1/admin/mirror/preview",
        json!({"discover": true, "section": {"api_url": "https://evil.example", "token": {"env": "FLOE_CONFIG_KEY"}}}),
    )
    .await?;
    assert_eq!(s, 400, "{b}");
    for uri in [
        "http://169.254.169.254/latest/meta-data",
        "https://catalog.example/iceberg#x",
    ] {
        let (s, b) = post(
            "/api/v1/admin/catalog/test",
            json!({"section": {"uri": uri, "warehouse": "wh"}}),
        )
        .await?;
        assert_eq!(s, 400, "{uri}: {b}");
    }
    // A reachable-but-refusing probe answers a class, not a body.
    let (s, b) = post(
        "/api/v1/admin/catalog/test",
        json!({"section": {"uri": format!("{}/nothing", server.base_url), "warehouse": "wh"}}),
    )
    .await?;
    assert_eq!(s, 200, "{b}");
    assert!(
        b.get("body").is_none() && b.get("error_class").is_some(),
        "{b}"
    );
    // Publishing an env reference outside the host's allowlist is a 400.
    let (s, b) = call(
        &server,
        reqwest::Method::PUT,
        "/api/v1/admin/config",
        Some(ADMIN),
        Some(json!({"document": {"events": {"webhook_secret": {"env": "FLOE_CONFIG_KEY"}}}})),
    )
    .await?;
    assert_eq!(s, 400, "{b}");
    assert_eq!(b["errors"][0]["path"], "events.webhook_secret", "{b}");
    Ok(())
}
