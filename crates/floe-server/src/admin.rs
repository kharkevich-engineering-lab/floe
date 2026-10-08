//! Admin endpoints: `PUT /{owner}/{repo}` (create), `DELETE /{owner}/{repo}`
//! (delete manifest + prefix objects), `PUT /{owner}/{repo}/api/head` (the
//! default branch), `GET /` (list repos, text/plain).

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use floe_git::ObjectFormat;

use crate::AppState;
use crate::error::ApiError;
use crate::repo::RepoRoute;

/// `PUT /{owner}/{repo}` — create repo. 201 on new, 409 if it exists.
pub async fn create(
    st: &AppState,
    route: &RepoRoute,
    headers: &HeaderMap,
    query: &str,
) -> Result<Response, ApiError> {
    let _principal = st.auth.require_write(headers).await.map_err(auth_err)?;
    let format = match query
        .split('&')
        .find_map(|part| part.strip_prefix("object_format="))
    {
        Some("sha256") => ObjectFormat::Sha256,
        Some("sha1") => ObjectFormat::Sha1,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "unsupported object format: {other}"
            )));
        }
        None => ObjectFormat::from(st.cfg.git.object_format),
    };
    match st.registry.create(&route.id, format).await {
        Ok(_h) => Ok((StatusCode::CREATED, "created").into_response()),
        Err(floe_wal::WalError::AlreadyExists) => {
            Ok((StatusCode::CONFLICT, "already exists").into_response())
        }
        Err(e) => Err(wal_err(e)),
    }
}

/// `DELETE /{owner}/{repo}` — admin-only deletion of the manifest and every object under the repo prefix.
pub async fn delete(
    st: &AppState,
    route: &RepoRoute,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let _principal = st.auth.require_admin(headers).await.map_err(auth_err)?;
    st.registry.delete(&route.id).await.map_err(wal_err)?;
    Ok((StatusCode::NO_CONTENT, "").into_response())
}

/// `PUT /{o}/{r}/api[-browser]/head` — point HEAD (the default branch) at an
/// existing branch. Body `{"branch": "master"}` (short or `refs/heads/…`).
/// Admin, like changing settings or policy (D24). A HEAD symref update is
/// published as an ordinary PUSH entry (no pack), so every instance sees it on
/// its next refs-level sync. `200 {head, previous, seq}`; `{…, unchanged:
/// true}` when HEAD already points there; `400` for a name that is not a
/// branch; `404` for a branch that does not exist.
///
/// Pushes adopt a default branch by themselves when HEAD names none
/// (`RepoHandle::publish_push_synced`); this is the repair for repositories
/// that ended up with a dangling HEAD before that, and the way to change it.
pub async fn set_head(
    st: &AppState,
    route: &RepoRoute,
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Result<Response, ApiError> {
    #[derive(serde::Deserialize)]
    struct Body {
        branch: String,
    }
    let principal = st.auth.require_admin(headers).await.map_err(auth_err)?;
    let bytes = crate::collect_body(body).await?;
    let req: Body = serde_json::from_slice(&bytes).map_err(|e| {
        ApiError::BadRequest(format!("body must be {{\"branch\": \"<name>\"}}: {e}"))
    })?;
    let branch = req.branch.trim();
    let target = if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    };
    if !target.starts_with("refs/heads/") || floe_git::validate_ref_name(&target).is_err() {
        return Err(ApiError::BadRequest(format!(
            "{branch:?} is not a branch name (HEAD can only point at refs/heads/…)"
        )));
    }
    let h = st.registry.open(&route.id).await.map_err(wal_err)?;
    let previous = {
        let _guard = h.sync_refs().await.map_err(wal_err)?;
        let view = h
            .local()
            .ref_view()
            .map_err(|e| ApiError::Internal(format!("refs: {e}")))?;
        if view.get(&target).is_none() {
            return Err(ApiError::NotFound(format!(
                "branch {target} does not exist in {}",
                route.id
            )));
        }
        view.head_target().to_string()
    };
    if previous == target {
        return Ok(axum::Json(serde_json::json!({
            "head": target, "previous": previous, "unchanged": true,
        }))
        .into_response());
    }
    let txn = floe_proto::v1::RefTransaction {
        updates: vec![floe_proto::v1::RefUpdate {
            name: "HEAD".to_string(),
            new_symbolic_target: target.clone(),
            ..Default::default()
        }],
        atomic: true,
        ..Default::default()
    };
    let meta = std::collections::HashMap::from([
        ("principal".to_string(), principal.name.clone()),
        ("agent".to_string(), "floe api head".to_string()),
    ]);
    let res = h.publish_push(None, txn, meta).await.map_err(wal_err)?;
    tracing::info!(repo = %route.id, head = %target, previous = %previous, seq = res.seq, principal = %principal.name, "default branch changed");
    Ok(axum::Json(serde_json::json!({
        "head": target, "previous": previous, "seq": res.seq,
    }))
    .into_response())
}

/// `GET /` — list repos as text/plain, one `owner/name` per line.
pub async fn list_repos(st: &AppState, headers: &HeaderMap) -> Result<Response, ApiError> {
    let _ = st.auth.require_read(headers).await.map_err(auth_err)?;
    let repos = st.registry.list().await.map_err(wal_err)?;
    let body = repos
        .into_iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

#[allow(clippy::needless_pass_by_value, reason = "used as a `map_err` adapter")]
fn auth_err(e: crate::auth::AuthError) -> ApiError {
    match e {
        crate::auth::AuthError::Invalid | crate::auth::AuthError::Unauthorized => {
            ApiError::Unauthorized
        }
        crate::auth::AuthError::Forbidden => ApiError::Forbidden,
        crate::auth::AuthError::Unavailable => {
            ApiError::ServiceUnavailable("auth provider unavailable".into())
        }
    }
}
#[allow(clippy::needless_pass_by_value, reason = "used as a `map_err` adapter")]
fn wal_err(e: floe_wal::WalError) -> ApiError {
    match &e {
        floe_wal::WalError::NotFound => ApiError::NotFound(e.to_string()),
        _ => ApiError::Internal(format!("wal: {e}")),
    }
}
