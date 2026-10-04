# Spike: rmcp 3.5 stateless hello-world and HTTP request parts in the handler

Context: M0 spike (b) of `docs/design/code-intelligence.md` (§7.1, §13, R7). For whoever implements M4 (the MCP
endpoints): what was verified about `rmcp =3.5.0` before the design leans on it. Recorded 2026-10-04.

## The question

D55 mounts one `StreamableHttpService` on two exact routes, `/api/v1/mcp` and `/{owner}/{repo}/mcp`. The
per-repo route's `route_layer` validates the path repo and inserts `PathRepo(RepoId)` into the request
extensions; the handler must then tell a scoped call from a global one. That only works if rmcp passes the HTTP
request (or its extensions) through to the handler, because the service factory never sees the request.

## Answer: yes

- **Source (rmcp 3.5.0, `src/transport/streamable_http_server/tower.rs`).** The service consumes the body and
  inserts the remaining `http::request::Parts` into the JSON-RPC request's extensions on both stateless paths
  (`request.request.extensions_mut().insert(parts)`, the negotiated 2026-07-28 path and the legacy-version
  stateless path) and on the session paths. The type's own docs say so ("injects the remaining
  `http::request::Parts` into `crate::model::Extensions`, which is accessible through
  `crate::service::RequestContext`") and show `ctx.extensions.get::<http::request::Parts>()` and reading an
  axum `Extension` out of `Parts.extensions`.
- **Source (`src/service.rs`).** The serve loop swaps the request's extensions into
  `RequestContext { extensions, .. }` before calling the handler, so `call_tool(&self, req, ctx)` sees them.
  Tool-router handlers can also take `Extension<http::request::Parts>` as an argument.
- **Proof (`crates/floe-server/tests/mcp_spike.rs`, `--features mcp`, run by `just test-codeintel` in CI).** The
  D55 router shape, served in-process: a `tools/call` on `/api/v1/mcp` answers `global parts=true`, on
  `/acme/app/mcp` and `/acme/app.git/mcp` it answers `scoped:acme/app parts=true`. So the handler reads
  `ctx.extensions.get::<http::request::Parts>()?.extensions.get::<PathRepo>()`: present ⇒ scoped, absent ⇒ global.

## What the hello-world also confirmed

| Behaviour | Result |
|---|---|
| `StreamableHttpServerConfig::default().with_legacy_session_mode(false).with_json_response(true).with_stateless_protocol_metadata_required(true)` | compiles and serves 2026-07-28 `tools/call` with `application/json`, no `Mcp-Session-Id` |
| Per-request metadata (`_meta` `protocolVersion`, `clientInfo`, `clientCapabilities`; `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` headers) | required as the design says; a missing `MCP-Protocol-Version` is a 4xx, a mismatched `Mcp-Name` is 400 + `-32020` before the handler runs |
| `route_service` exactness | `/api/v1/mcp/anything` and `/acme/app/mcp/x` are 404; the static `/api/v1/mcp` wins over `/{owner}/{repo}/mcp` |
| GET on a stateless endpoint without an event store | 405 |
| Default `allowed_hosts` | loopback only (`localhost`, `127.0.0.1`, `::1`): a public `Host` is refused, so floe must set it from `server.public_url` (§7.1) |
| `ServerHandler::call_tool` | returns `Result<CallToolResponse, ErrorData>` in 3.5 (`CallToolResult::success(..).into()`), not `CallToolResult` as in 0.x examples |

## Not done in this spike

- The official MCP conformance suite (`@modelcontextprotocol/conformance`, a Node tool) against the endpoint. It
  belongs with M4, when there is a real endpoint to point it at; the hello-world proves the transport contract
  floe depends on.
- `request-state` (signed request state) is not enabled in the pinned feature set; snapshot handles and cursors
  are floe's own HMAC (D57), so nothing needs it yet.
