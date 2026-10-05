//! M0 spike (b), `docs/design/code-intelligence.md` §7.1 and §13: an rmcp 3.5 stateless
//! (MCP 2026-07-28) hello-world mounted the way D55 mounts the real endpoints — one
//! `StreamableHttpService` on two exact routes, `/api/v1/mcp` and `/{owner}/{repo}/mcp`, with a
//! `route_layer` that puts the validated path repo into the request extensions.
//!
//! The question the design rests on: does rmcp hand the HTTP request's `http::request::Parts`
//! (and so the extensions an axum layer inserted) to the handler? Yes — rmcp 3.5.0 inserts the
//! `Parts` into the JSON-RPC request's extensions on both stateless paths
//! (`transport/streamable_http_server/tower.rs`), and the service loop moves those into
//! `RequestContext.extensions`. This test proves it end to end; the findings are recorded in
//! `docs/design/spikes/rmcp-request-parts.md`. Runs only with `--features mcp`
//! (`just test-codeintel`).
#![cfg(feature = "mcp")]
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

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http_body_util::BodyExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use tower::ServiceExt;

/// What the per-repo route tells the handler (floe's will be a validated `RepoId`).
#[derive(Clone, Debug)]
struct PathRepo(String);

#[derive(Clone, Default)]
struct Hello;

impl ServerHandler for Hello {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let parts = ctx.extensions.get::<http::request::Parts>();
        let scope = parts
            .and_then(|p| p.extensions.get::<PathRepo>())
            .map_or_else(|| "global".to_string(), |r| format!("scoped:{}", r.0));
        let text = format!("{} {scope} parts={}", request.name, parts.is_some());
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
}

async fn path_repo(
    Path((owner, repo)): Path<(String, String)>,
    mut req: Request,
    next: Next,
) -> Response {
    let repo = repo.strip_suffix(".git").unwrap_or(&repo);
    if owner.starts_with('.') || repo.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    req.extensions_mut()
        .insert(PathRepo(format!("{owner}/{repo}")));
    next.run(req).await
}

fn app() -> Router {
    let cfg = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_stateless_protocol_metadata_required(true)
        .with_sse_keep_alive(None);
    let svc: StreamableHttpService<Hello, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(Hello), LocalSessionManager::default().into(), cfg);
    let scoped = Router::new()
        .route_service("/{owner}/{repo}/mcp", svc.clone())
        .route_layer(from_fn(path_repo));
    Router::new()
        .route_service("/api/v1/mcp", svc)
        .merge(scoped)
}

fn call(uri: &str, tool: &str) -> Request {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "spike", "version": "0"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    http::Request::post(uri)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", tool)
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(req: Request) -> (StatusCode, http::HeaderMap, String) {
    let res = app().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn tool_text(uri: &str) -> String {
    let (status, headers, body) = send(call(uri, "hello")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        headers.get("mcp-session-id").is_none(),
        "stateless: no session id"
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {v}"))
        .to_string()
}

#[tokio::test]
async fn the_handler_sees_the_http_parts_and_the_path_repo() {
    assert_eq!(tool_text("/api/v1/mcp").await, "hello global parts=true");
    assert_eq!(
        tool_text("/acme/app/mcp").await,
        "hello scoped:acme/app parts=true"
    );
    assert_eq!(
        tool_text("/acme/app.git/mcp").await,
        "hello scoped:acme/app parts=true"
    );
}

#[tokio::test]
async fn routes_are_exact_and_post_only() {
    let (status, _, _) = send(call("/api/v1/mcp/anything", "hello")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing below the endpoint");
    let (status, _, _) = send(call("/acme/app/mcp/x", "hello")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let get = http::Request::get("/api/v1/mcp")
        .header("host", "localhost")
        .header("accept", "text/event-stream")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(get).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn stateless_requests_must_carry_the_protocol_metadata() {
    let mut req = call("/api/v1/mcp", "hello");
    req.headers_mut().remove("mcp-protocol-version");
    let (status, _, body) = send(req).await;
    assert!(status.is_client_error(), "{status}: {body}");
    // Mismatched SEP-2243 header: rmcp answers 400 + -32020 before the handler runs.
    let mut req = call("/api/v1/mcp", "hello");
    req.headers_mut()
        .insert("mcp-name", http::HeaderValue::from_static("other"));
    let (status, _, body) = send(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("-32020"), "{body}");
}

#[tokio::test]
async fn host_validation_defaults_to_loopback() {
    let mut req = call("/api/v1/mcp", "hello");
    req.headers_mut()
        .insert("host", http::HeaderValue::from_static("evil.example.com"));
    let (status, _, _) = send(req).await;
    assert!(
        status.is_client_error(),
        "rmcp's default allowed_hosts is loopback; floe sets it from server.public_url"
    );
}
