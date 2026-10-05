//! D62 — the admin API (`/api/v1/admin/*` and `/api-browser/v1/admin/*`,
//! `web/API.md` §4 "Admin", `docs/design/admin-ui.md` §7): the runtime config
//! document (D60) with its history, the GitHub mirror's controls, the catalog's
//! status. Every route — reads included — needs an admin principal (D24's rule).
//! Every answer is `Cache-Control: no-store`.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use floe_config::{GithubMirrorConfig, RuntimeConfig, Secret};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::Principal;
use crate::config_store::{self, PublishError, PublishRequest, redact};
use crate::error::ApiError;

/// Bound on outbound test calls (GitHub `/user`, the catalog's `/v1/config`).
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Register the admin routes under `/api/v1/admin` and `/api-browser/v1/admin`.
pub fn routes(mut r: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    for base in ["/api/v1/admin", "/api-browser/v1/admin"] {
        r = r
            .route(&format!("{base}/config"), get(config_get).put(config_put))
            .route(&format!("{base}/config/validate"), post(config_validate))
            .route(&format!("{base}/config/schema"), get(config_schema))
            .route(&format!("{base}/config/history"), get(config_history))
            .route(&format!("{base}/config/revisions/{{n}}"), get(config_revision))
            .route(&format!("{base}/config/rollback"), post(config_rollback))
            .route(&format!("{base}/overview"), get(overview))
            .route(&format!("{base}/mirror"), get(mirror_status))
            .route(&format!("{base}/mirror/preview"), post(mirror_preview))
            .route(&format!("{base}/mirror/test"), post(mirror_test))
            .route(&format!("{base}/mirror/sync"), post(mirror_sync))
            .route(&format!("{base}/mirror/pause"), post(mirror_pause))
            .route(&format!("{base}/mirror/resume"), post(mirror_resume))
            .route(&format!("{base}/catalog"), get(catalog_status))
            .route(&format!("{base}/catalog/test"), post(catalog_test));
    }
    r
}

// ---- plumbing -----------------------------------------------------------------

/// A refusal, already rendered (boxed: a `Response` is too large for an `Err`).
type Refusal = Box<Response>;

/// An admin principal, else the refusal: no credential is a 401 (a browser
/// signs in, a script learns its token is missing), a non-admin a 403.
async fn admin(st: &AppState, headers: &HeaderMap) -> Result<Principal, Refusal> {
    match st.auth.authenticate(headers).await {
        Ok(p) if p.admin => Ok(p),
        Ok(p) if p.anonymous => Err(Box::new(ApiError::Unauthorized.into_response())),
        Ok(_) => Err(Box::new(ApiError::Forbidden.into_response())),
        Err(e) => Err(Box::new(crate::web::api::auth_err(e).into_response())),
    }
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn ok(v: &Value) -> Response {
    no_store(axum::Json(v).into_response())
}

fn fail(status: StatusCode, v: &Value) -> Response {
    no_store((status, axum::Json(v)).into_response())
}

fn bad_request(message: &str) -> Response {
    fail(
        StatusCode::BAD_REQUEST,
        &json!({"error": message, "errors": []}),
    )
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, Refusal> {
    let parsed = if body.is_empty() {
        serde_json::from_value(json!({}))
    } else {
        serde_json::from_slice(body)
    };
    parsed.map_err(|e| Box::new(bad_request(&format!("JSON body: {e}"))))
}

fn publish_error(e: PublishError) -> Response {
    match e {
        PublishError::Invalid(errors) => fail(
            StatusCode::BAD_REQUEST,
            &json!({"error": "invalid config document; nothing was published", "errors": errors}),
        ),
        PublishError::Conflict { current } => fail(
            StatusCode::CONFLICT,
            &json!({"error": format!("the config changed meanwhile: the current revision is {current}"), "revision": current}),
        ),
        PublishError::NotFound(n) => fail(
            StatusCode::NOT_FOUND,
            &json!({"error": format!("no revision {n}")}),
        ),
        PublishError::Gone(n) => fail(
            StatusCode::GONE,
            &json!({"error": format!("revision {n} is no longer kept by the bucket")}),
        ),
        PublishError::Store(e) => fail(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error": format!("config store: {e:#}")}),
        ),
    }
}

fn store_error(e: &anyhow::Error) -> Response {
    fail(
        StatusCode::SERVICE_UNAVAILABLE,
        &json!({"error": format!("config store: {e:#}")}),
    )
}

/// The audit trail beyond the history record (D62): a log line is written by
/// the store; with the catalog on, a lossy `sync_runs` row.
fn audit(st: &AppState, record: &config_store::Record) {
    let now = chrono::Utc::now();
    st.recorder.record_sync_run(floe_catalog::SyncRun {
        source: Some("admin".into()),
        finished_at: now,
        outcome: "published".into(),
        detail: Some(format!(
            "config revision {} by {}: {}",
            record.revision, record.author, record.message
        )),
        ..floe_catalog::SyncRun::new("config", record.updated_at)
    });
}

fn record_json(r: &config_store::Record) -> Value {
    json!({
        "revision": r.revision,
        "updated_at": r.updated_at,
        "author": r.author,
        "message": r.message,
        "rolled_back_from": r.rolled_back_from,
        "document": redact(&r.document),
        "diff": r.diff,
    })
}

// ---- config -----------------------------------------------------------------

async fn config_get(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let cs = st.config.store();
    let current = match cs.current().await {
        Ok(c) => c,
        Err(e) => return store_error(&e),
    };
    let live = st.config.status();
    let mut body = match &current {
        Some((_, r)) => record_json(r),
        None => json!({
            "revision": 0,
            "updated_at": null,
            "author": "",
            "message": "",
            "rolled_back_from": null,
            "document": redact(&RuntimeConfig::default().to_json().unwrap_or_default()),
            "diff": [],
        }),
    };
    if let Some(o) = body.as_object_mut() {
        o.insert("history_mode".into(), json!(cs.history_mode()));
        o.insert("location".into(), json!(cs.location()));
        o.insert("sealing_key".into(), json!(cs.has_key()));
        o.insert("key_env".into(), json!(cs.key_env()));
        o.insert(
            "applied".into(),
            json!({
                "revision": live.applied_revision,
                "restart_required": live.restart_required,
                "apply_error": live.apply_error,
            }),
        );
    }
    let revision = current.as_ref().map_or(0, |(_, r)| r.revision);
    let mut resp = ok(&body);
    if let Ok(v) = HeaderValue::from_str(&format!("\"{revision}\"")) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

#[derive(Deserialize)]
struct PutBody {
    document: Value,
    #[serde(default)]
    message: String,
    base_revision: Option<u64>,
}

async fn config_put(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match admin(&st, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let req: PutBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let published = st
        .config
        .store()
        .publish(
            &PublishRequest {
                document: &req.document,
                author: &principal.name,
                message: &req.message,
                base_revision: req.base_revision,
                rolled_back_from: None,
            },
            st.config.bootstrap(),
        )
        .await;
    finish_publish(&st, published).await
}

async fn finish_publish(
    st: &Arc<AppState>,
    published: Result<config_store::Published, PublishError>,
) -> Response {
    match published {
        Ok(p) => {
            audit(st, &p.record);
            // Apply here now; every other instance within `config_store.ttl`.
            st.config.revalidate().await;
            ok(&json!({
                "revision": p.record.revision,
                "diff": p.record.diff,
                "restart_required": p.restart_required,
            }))
        }
        Err(e) => publish_error(e),
    }
}

#[derive(Deserialize)]
struct ValidateBody {
    document: Value,
}

async fn config_validate(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let req: ValidateBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let cs = st.config.store();
    let current = match cs.current().await {
        Ok(c) => c,
        Err(e) => return store_error(&e),
    };
    match cs.prepare(&req.document, current.as_ref().map(|(_, r)| r), st.config.bootstrap()) {
        Ok(p) => ok(&json!({"ok": true, "errors": [], "diff": p.diff, "restart_required": p.restart_required})),
        Err(errors) => ok(&json!({"ok": false, "errors": errors, "diff": [], "restart_required": []})),
    }
}

async fn config_schema(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    ok(&config_store::schema::document_schema())
}

#[derive(Deserialize)]
struct HistoryQuery {
    before: Option<u64>,
    n: Option<usize>,
}

async fn config_history(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    match st
        .config
        .store()
        .history(q.before, q.n.unwrap_or(20))
        .await
    {
        Ok(entries) => ok(&json!({"entries": entries})),
        Err(e) => store_error(&e),
    }
}

async fn config_revision(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(n): Path<u64>,
) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    match st.config.store().revision(n).await {
        Ok(r) => ok(&record_json(&r)),
        Err(e) => publish_error(e),
    }
}

#[derive(Deserialize)]
struct RollbackBody {
    revision: u64,
    #[serde(default)]
    message: String,
    base_revision: Option<u64>,
}

async fn config_rollback(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match admin(&st, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let req: RollbackBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let published = st
        .config
        .store()
        .rollback(
            req.revision,
            &principal.name,
            &req.message,
            req.base_revision,
            st.config.bootstrap(),
        )
        .await;
    finish_publish(&st, published).await
}

// ---- overview -----------------------------------------------------------------

async fn overview(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let cs = st.config.store();
    let (current, instances, mirror) = tokio::join!(cs.current(), cs.instances(), mirror_summary(&st));
    let live = st.config.status();
    let applied = st.config.current();
    let config = match current {
        Ok(Some((_, r))) => json!({
            "revision": r.revision, "updated_at": r.updated_at, "author": r.author,
            "message": r.message, "error": null,
        }),
        Ok(None) => json!({"revision": 0, "updated_at": null, "author": "", "message": "", "error": null}),
        Err(e) => json!({"revision": null, "error": format!("{e:#}")}),
    };
    let me = st.config.instance_status();
    ok(&json!({
        "config": config,
        "store": {"location": cs.location(), "history_mode": cs.history_mode(), "sealing_key": cs.has_key(), "key_env": cs.key_env()},
        "instance": {
            "id": me.instance,
            "version": me.version,
            "roles": me.roles,
            "started_at": me.started_at,
            "applied_revision": live.applied_revision,
            "restart_required": live.restart_required,
            "apply_error": live.apply_error,
            "last_check": live.last_check,
            "check_error": live.check_error,
        },
        "instances": instances.unwrap_or_default(),
        "mirror": mirror,
        "catalog": catalog_json(&st, &applied.cfg.catalog),
    }))
}

async fn mirror_summary(st: &AppState) -> Value {
    let cfg = st.config.current().cfg;
    let gm = &cfg.github_mirror;
    let (state, lease) = tokio::join!(
        floe_mirror::state::load(st.store.as_ref(), "github"),
        floe_mirror::lease_holder(&st.store, "github")
    );
    let lease = lease.map(|(holder, expires)| {
        json!({"holder": holder, "expires_at": chrono::DateTime::<chrono::Utc>::from(expires)})
    });
    match state {
        Ok(s) => {
            let mut counts = serde_json::Map::new();
            for e in s.repos.values() {
                let n = counts.get(e.status.as_str()).and_then(Value::as_u64).unwrap_or(0);
                counts.insert(e.status.as_str().to_string(), json!(n + 1));
            }
            json!({
                "enabled": gm.enabled,
                "lease": lease,
                "token_login": s.token_login,
                "last_pass": s.last_pass,
                "counts": counts,
                "repos": s.repos.len(),
                "error": null,
            })
        }
        Err(e) => json!({"enabled": gm.enabled, "lease": lease, "error": e.to_string()}),
    }
}

fn catalog_json(st: &AppState, c: &floe_config::CatalogConfig) -> Value {
    let up = st.catalog.as_ref().map(|w| w.is_up());
    let auth = serde_json::to_value(c.auth).unwrap_or_default();
    json!({
        "compiled": cfg!(feature = "catalog"),
        "enabled": c.enabled,
        "running": st.catalog.is_some(),
        "up": up,
        "tail": st.catalog_tail.is_some(),
        "uri": c.uri,
        "warehouse": c.warehouse,
        "namespace": c.namespace,
        "auth": auth,
    })
}

// ---- mirror -----------------------------------------------------------------

fn is_paused(gm: &GithubMirrorConfig, full_name: &str) -> bool {
    gm.exclude.iter().any(|g| g.eq_ignore_ascii_case(full_name))
}

async fn mirror_status(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let cfg = st.config.current().cfg;
    let gm = &cfg.github_mirror;
    let summary = mirror_summary(&st).await;
    let state = match floe_mirror::state::load(st.store.as_ref(), "github").await {
        Ok(s) => s,
        Err(e) => return store_error(&anyhow::anyhow!("{e}")),
    };
    let repos: Vec<Value> = state
        .repos
        .iter()
        .map(|(id, e)| {
            json!({
                "id": id,
                "full_name": e.full_name,
                "floe": e.floe,
                "status": e.status.as_str(),
                "private": e.private,
                "archived": e.archived,
                "fork": e.fork,
                "size_kb": e.size_kb,
                "pushed_at": e.pushed_at,
                "last_seen": e.last_seen,
                "missing_since": e.missing_since,
                "last_error": e.last_error,
                "paused": is_paused(gm, &e.full_name),
            })
        })
        .collect();
    ok(&json!({"summary": summary, "repos": repos}))
}

/// The candidate `github_mirror` section of a request (`{"section": {...}}`),
/// over the applied one for missing keys; a redacted or sealed token means
/// "the applied token".
fn candidate_section(st: &AppState, section: Option<&Value>) -> Result<GithubMirrorConfig, Refusal> {
    let applied = st.config.current().cfg.github_mirror.clone();
    let Some(section) = section else {
        return Ok(applied);
    };
    let mut base = serde_json::to_value(&applied).unwrap_or_default();
    if let (Some(b), Some(s)) = (base.as_object_mut(), section.as_object()) {
        for (k, v) in s {
            b.insert(k.clone(), v.clone());
        }
    }
    let doc: RuntimeConfig = serde_json::from_value(json!({ "github_mirror": base }))
        .map_err(|e| Box::new(bad_request(&format!("github_mirror: {e}"))))?;
    let mut gm = doc.github_mirror;
    if matches!(gm.token, Secret::Redacted(_) | Secret::Sealed(_)) {
        gm.token = applied.token;
    }
    Ok(gm)
}

/// Why `select` decides what it decides (`floe_mirror::select`'s rule order).
fn selection_reason(gm: &GithubMirrorConfig, r: &floe_mirror::RemoteRepo, explicit: bool) -> String {
    use floe_mirror::select::glob_match;
    let full = r.full_name();
    if let Some(g) = gm.exclude.iter().find(|g| glob_match(g, &full)) {
        return if g.eq_ignore_ascii_case(&full) {
            "paused (exact exclude entry)".into()
        } else {
            format!("excluded by {g}")
        };
    }
    if r.private && !gm.include_private {
        return "private (include_private is off)".into();
    }
    if !explicit {
        if r.archived && gm.skip_archived {
            return "archived (skip_archived)".into();
        }
        if r.fork && gm.skip_forks {
            return "fork (skip_forks)".into();
        }
        match gm.include.iter().find(|g| glob_match(g, &full)) {
            None => return "no include glob matches".into(),
            Some(g) => {
                let limit = gm.max_repo_size.as_u64();
                if limit > 0 && r.size_kb.saturating_mul(1024) > limit {
                    return "larger than max_repo_size".into();
                }
                return format!("included by {g}");
            }
        }
    }
    let limit = gm.max_repo_size.as_u64();
    if limit > 0 && r.size_kb.saturating_mul(1024) > limit {
        return "larger than max_repo_size".into();
    }
    "explicit (repos)".into()
}

#[derive(Deserialize)]
struct PreviewBody {
    section: Option<Value>,
    #[serde(default)]
    discover: bool,
}

async fn mirror_preview(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let req: PreviewBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let gm = match candidate_section(&st, req.section.as_ref()) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    let state = match floe_mirror::state::load(st.store.as_ref(), "github").await {
        Ok(s) => s,
        Err(e) => return store_error(&anyhow::anyhow!("{e}")),
    };
    let mut repos = Vec::new();
    let (mut selected, mut too_large, mut out) = (0usize, 0usize, 0usize);
    for (id, e) in &state.repos {
        let remote = e.to_remote(id);
        let explicit = floe_mirror::select::is_explicit(&gm, &e.full_name);
        let verdict = floe_mirror::select::select(&gm, &remote, explicit);
        let v = match verdict {
            floe_mirror::select::Verdict::In => {
                selected += 1;
                "in"
            }
            floe_mirror::select::Verdict::TooLarge => {
                too_large += 1;
                "too-large"
            }
            floe_mirror::select::Verdict::Out => {
                out += 1;
                "out"
            }
        };
        repos.push(json!({
            "full_name": e.full_name, "floe": e.floe, "status": e.status.as_str(),
            "verdict": v, "reason": selection_reason(&gm, &remote, explicit),
        }));
    }
    let mut body = json!({
        "known": repos.len(), "selected": selected, "too_large": too_large, "out": out,
        "repos": repos, "plan": null,
    });
    if req.discover {
        let plan = match discover(&st, gm).await {
            Ok(p) => p,
            Err(e) => json!({"error": format!("{e:#}")}),
        };
        if let Some(o) = body.as_object_mut() {
            o.insert("plan".into(), plan);
        }
    }
    ok(&body)
}

/// A real dry-run pass against the forge with a candidate section: no lease,
/// no writes (`PassOptions::dry_run`).
async fn discover(st: &Arc<AppState>, gm: GithubMirrorConfig) -> anyhow::Result<Value> {
    let alias = format!("FLOE_CONFIG_PREVIEW_TOKEN_{}", uuid::Uuid::new_v4().simple());
    floe_config::secret::set_alias(&alias, Some(gm.token.clone()));
    let result = async {
        let mut cfg = (*st.config.current().cfg).clone();
        cfg.github_mirror = gm;
        cfg.github_mirror.token_env.clone_from(&alias);
        let source = Arc::new(floe_mirror::github::GithubSource::new(&cfg.github_mirror)?);
        let target = floe_mirror::WalTarget::new(
            st.registry.clone(),
            st.store.clone(),
            Box::new(|_: &floe_git::RepoId| {}),
        );
        let m = floe_mirror::Mirror {
            cfg: Arc::new(cfg),
            source,
            target: Arc::new(target),
            store: st.store.clone(),
        };
        let r = tokio::time::timeout(
            Duration::from_mins(2),
            floe_mirror::reconcile_once(&m, floe_mirror::PassOptions { dry_run: true }, None),
        )
        .await
        .map_err(|_| anyhow::anyhow!("the dry run did not finish within 2 minutes"))??;
        Ok::<_, anyhow::Error>(json!({
            "summary": r.summary(), "discovered": r.discovered, "complete": r.complete,
            "lines": r.plan, "rate_remaining": r.api.rate_remaining,
        }))
    }
    .await;
    floe_config::secret::set_alias(&alias, None);
    result
}

#[derive(Deserialize)]
struct TestBody {
    api_url: Option<String>,
    token: Option<Secret>,
}

async fn mirror_test(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let req: TestBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let applied = st.config.current().cfg.github_mirror.clone();
    let api = req.api_url.unwrap_or(applied.api_url).trim_end_matches('/').to_string();
    if !(api.starts_with("https://") || api.starts_with("http://localhost") || api.starts_with("http://127.0.0.1")) {
        return bad_request("api_url must be https:// (or a loopback http URL)");
    }
    let token = match req.token {
        None | Some(Secret::Redacted(_) | Secret::Sealed(_)) => applied.token.reveal(),
        Some(s) => s.reveal(),
    };
    let Some(token) = token else {
        return ok(&json!({"ok": false, "message": "no token: the reference is unset on this instance, or nothing was entered"}));
    };
    let client = match reqwest::Client::builder().timeout(TEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, &json!({"error": e.to_string()})),
    };
    let resp = client
        .get(format!("{api}/user"))
        .header("Authorization", format!("Bearer {}", token.trim()))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("floe/", env!("CARGO_PKG_VERSION")))
        .send()
        .await;
    match resp {
        Err(e) => ok(&json!({"ok": false, "message": format!("GitHub unreachable: {e}")})),
        Ok(r) => {
            let status = r.status().as_u16();
            let h = |k: &str| r.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
            let scopes = h("x-oauth-scopes");
            let remaining = h("x-ratelimit-remaining").and_then(|v| v.parse::<u64>().ok());
            let body: Value = r.json().await.unwrap_or(Value::Null);
            let login = body.get("login").and_then(Value::as_str).map(str::to_string);
            ok(&json!({
                "ok": (200..300).contains(&status),
                "status": status,
                "login": login,
                "scopes": scopes,
                "rate_remaining": remaining,
                "message": body.get("message").and_then(Value::as_str),
            }))
        }
    }
}

async fn mirror_sync(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal = match admin(&st, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match crate::mirror::request_sync(&st.store, &principal.name).await {
        Ok(()) => ok(&json!({"ok": true, "message": "requested: the mirror loop runs a pass within about a minute"})),
        Err(e) => store_error(&e),
    }
}

#[derive(Deserialize)]
struct PauseBody {
    full_name: String,
}

async fn mirror_pause(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    pause_or_resume(st, headers, body, true).await
}

async fn mirror_resume(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    pause_or_resume(st, headers, body, false).await
}

/// Pause = the exact `owner/name` in `github_mirror.exclude` (the mirror's
/// frozen state: data kept, follow stopped); resume removes it. A config change.
async fn pause_or_resume(st: Arc<AppState>, headers: HeaderMap, body: Bytes, pause: bool) -> Response {
    let principal = match admin(&st, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let req: PauseBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let name = req.full_name.trim().to_string();
    if name.matches('/').count() != 1 || name.contains('*') || name.starts_with('/') || name.ends_with('/') {
        return bad_request("full_name must be \"owner/name\"");
    }
    let cs = st.config.store();
    let current = match cs.current().await {
        Ok(c) => c,
        Err(e) => return store_error(&e),
    };
    let (base, mut doc) = match &current {
        Some((_, r)) => match RuntimeConfig::from_json(&r.document) {
            Ok(d) => (r.revision, d),
            Err(e) => return store_error(&e),
        },
        None => (0, RuntimeConfig::from_config(st.config.bootstrap())),
    };
    let ex = &mut doc.github_mirror.exclude;
    let had = ex.iter().any(|g| g.eq_ignore_ascii_case(&name));
    if pause == had {
        return ok(&json!({"revision": base, "diff": [], "restart_required": [], "unchanged": true}));
    }
    if pause {
        ex.push(name.clone());
    } else {
        ex.retain(|g| !g.eq_ignore_ascii_case(&name));
    }
    let document = match doc.to_json() {
        Ok(d) => d,
        Err(e) => return store_error(&e),
    };
    let message = format!("{} {name}", if pause { "pause" } else { "resume" });
    let published = cs
        .publish(
            &PublishRequest {
                document: &document,
                author: &principal.name,
                message: &message,
                base_revision: None,
                rolled_back_from: None,
            },
            st.config.bootstrap(),
        )
        .await;
    finish_publish(&st, published).await
}

// ---- catalog ----------------------------------------------------------------

async fn catalog_status(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let cfg = st.config.current().cfg;
    let mut v = catalog_json(&st, &cfg.catalog);
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "restart_required".into(),
            json!(st
                .config
                .status()
                .restart_required
                .iter()
                .any(|p| p.starts_with("catalog"))),
        );
    }
    ok(&v)
}

#[derive(Deserialize)]
struct CatalogTestBody {
    section: Option<Value>,
}

/// `GET {uri}/v1/config?warehouse=…` (Iceberg REST) with the configured
/// bearer; `OAuth2` client credentials and `SigV4` (D63) are the writer's own
/// seams, so those probes go unauthenticated and say so.
async fn catalog_test(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = admin(&st, &headers).await {
        return *r;
    }
    let req: CatalogTestBody = match parse(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let applied = st.config.current().cfg.catalog.clone();
    let cat = match req.section {
        None => applied,
        Some(section) => {
            let mut base = serde_json::to_value(&applied).unwrap_or_default();
            if let (Some(b), Some(s)) = (base.as_object_mut(), section.as_object()) {
                for (k, v) in s {
                    b.insert(k.clone(), v.clone());
                }
            }
            match serde_json::from_value::<RuntimeConfig>(json!({"catalog": base})) {
                Ok(d) => d.catalog,
                Err(e) => return bad_request(&format!("catalog: {e}")),
            }
        }
    };
    let Some(uri) = cat.uri.as_deref().filter(|u| !u.trim().is_empty()) else {
        return ok(&json!({"ok": false, "message": "catalog.uri is not set"}));
    };
    let mut url = format!("{}/v1/config", uri.trim_end_matches('/'));
    if let Some(w) = cat.warehouse.as_deref().filter(|w| !w.is_empty()) {
        url.push_str("?warehouse=");
        url.push_str(&urlencode(w));
    }
    let client = match reqwest::Client::builder().timeout(TEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, &json!({"error": e.to_string()})),
    };
    let mut rq = client.get(&url).header("Accept", "application/json");
    let auth = match cat.auth {
        floe_config::CatalogAuth::None => "none",
        floe_config::CatalogAuth::Bearer if cat.token_env.is_some() => "bearer",
        floe_config::CatalogAuth::Bearer => "oauth2",
        floe_config::CatalogAuth::Sigv4 => "sigv4",
    };
    if auth == "bearer"
        && let Some(var) = &cat.token_env
    {
        match floe_config::secret::env_var(var) {
            Some(t) => rq = rq.header("Authorization", format!("Bearer {}", t.trim())),
            None => {
                return ok(&json!({"ok": false, "auth": auth, "message": format!("catalog.token_env names {var}, which is unset on this instance")}));
            }
        }
    }
    match rq.send().await {
        Err(e) => ok(&json!({"ok": false, "auth": auth, "url": url, "message": format!("unreachable: {e}")})),
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let excerpt: String = text.chars().take(500).collect();
            let note = match auth {
                "oauth2" => Some("OAuth2 client credentials are exchanged by the writer; this probe was unauthenticated"),
                "sigv4" => Some("the writer signs every catalog request (SigV4, D63); this probe was unsigned, so a 403 here only proves the endpoint answers"),
                _ => None,
            };
            ok(&json!({
                "ok": (200..300).contains(&status),
                "auth": auth,
                "url": url,
                "status": status,
                "body": excerpt,
                "message": note,
            }))
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
