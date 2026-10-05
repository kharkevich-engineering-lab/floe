//! `GET /api/v1/admin/config/schema` (D62): a JSON Schema (draft 2020-12) of
//! the config document, so the admin GUI renders its forms instead of
//! hard-coding them. Built from the document's defaults (every key, its type
//! and default) plus a table of annotations (title, help, format, enum, group);
//! liveness comes from [`floe_config::runtime::RESTART_ONLY`]. A key without an
//! annotation still renders, as a plain field of its inferred type.

use floe_config::RuntimeConfig;
use floe_config::runtime::{RESTART_ONLY, SECRET_PATHS};
use serde_json::{Map, Value, json};

/// `(path, title, help, format, group)`. `format` ∈ `""`, `duration`,
/// `bytesize`, `secret`, `glob`, `url`, `env`, `refpattern`.
const ANNOTATIONS: &[(&str, &str, &str, &str, &str)] = &[
    // --- github_mirror ---
    (
        "github_mirror.enabled",
        "Mirror GitHub",
        "Run the mirror on the fleet's maintain hosts (one at a time, under a bucket lease).",
        "",
        "General",
    ),
    (
        "github_mirror.api_url",
        "API URL",
        "REST base: https://api.github.com, or https://ghe.example.com/api/v3 for GitHub Enterprise Server.",
        "url",
        "Credential",
    ),
    (
        "github_mirror.git_url",
        "Git URL",
        "Base of clone and LFS URLs; follow sends the token only to this host.",
        "url",
        "Credential",
    ),
    (
        "github_mirror.token",
        "Token",
        "A PAT (classic `repo`, or fine-grained Contents:read + Metadata:read). Entered values are sealed and never shown again; an env reference keeps it out of the bucket.",
        "secret",
        "Credential",
    ),
    (
        "github_mirror.users",
        "Users",
        "Owners whose repositories are mirrored; \"@me\" = the token's user (private included).",
        "",
        "Sources",
    ),
    (
        "github_mirror.orgs",
        "Organisations",
        "Every repository the token can see in these organisations.",
        "",
        "Sources",
    ),
    (
        "github_mirror.starred",
        "Starred by",
        "Users whose stars are mirrored (\"@me\" allowed).",
        "",
        "Sources",
    ),
    (
        "github_mirror.repos",
        "Explicit repositories",
        "owner/name entries that bypass include and the archived/fork skips.",
        "",
        "Sources",
    ),
    (
        "github_mirror.include",
        "Include",
        "owner/name globs (`*` stops at `/`); case-insensitive.",
        "glob",
        "Selection",
    ),
    (
        "github_mirror.exclude",
        "Exclude",
        "Same syntax; wins over everything. An exact owner/name here is a paused repository.",
        "glob",
        "Selection",
    ),
    (
        "github_mirror.skip_archived",
        "Skip archived",
        "Do not start mirroring archived repositories.",
        "",
        "Selection",
    ),
    (
        "github_mirror.skip_forks",
        "Skip forks",
        "Do not start mirroring forks.",
        "",
        "Selection",
    ),
    (
        "github_mirror.include_private",
        "Include private",
        "Mirror private repositories the token can read.",
        "",
        "Selection",
    ),
    (
        "github_mirror.private_visible_to_all_readers",
        "Private repositories are readable by every floe reader",
        "floe has no per-repository read ACL: acknowledge that every reader of this floe reads every mirrored private repository.",
        "",
        "Selection",
    ),
    (
        "github_mirror.max_repo_size",
        "Max repository size",
        "Larger repositories are never auto-created (handoff to floe import). 0 = no limit.",
        "bytesize",
        "Selection",
    ),
    (
        "github_mirror.lfs",
        "LFS read-through",
        "Write upstream.lfs so LFS objects read through from GitHub.",
        "",
        "Repositories",
    ),
    (
        "github_mirror.follow",
        "Followed refs",
        "Ref patterns written into each repository's upstream.follow.",
        "refpattern",
        "Repositories",
    ),
    (
        "github_mirror.on_rewrite",
        "On rewrite",
        "archive = keep rewritten tips under refs/archive/; refuse = fast-forward only.",
        "",
        "Repositories",
    ),
    (
        "github_mirror.read_only",
        "Read-only",
        "Publish a deny-all policy so only follow moves refs.",
        "",
        "Repositories",
    ),
    (
        "github_mirror.interval",
        "Discovery interval",
        "How often the mirror lists GitHub. 0 = only at startup.",
        "duration",
        "Schedule",
    ),
    (
        "github_mirror.follow_interval",
        "Follow backstop",
        "Written into upstream.follow_interval; pushes are nudged sooner.",
        "duration",
        "Schedule",
    ),
    (
        "github_mirror.max_new_per_pass",
        "New repositories per pass",
        "Bound on creations per pass.",
        "",
        "Schedule",
    ),
    (
        "github_mirror.min_rate_remaining",
        "Rate-limit floor",
        "Stop a pass when x-ratelimit-remaining drops below this.",
        "",
        "Schedule",
    ),
    (
        "github_mirror.gone_after",
        "Gone after",
        "How long a repository must be missing before it is frozen as gone/forbidden.",
        "duration",
        "Schedule",
    ),
    (
        "github_mirror.lease_ttl",
        "Lease TTL",
        "TTL of the fleet-wide mirror lease.",
        "duration",
        "Schedule",
    ),
    // --- catalog ---
    (
        "catalog.enabled",
        "Write audit tables",
        "Iceberg tables from the WAL (needs a binary built with --features catalog).",
        "",
        "General",
    ),
    (
        "catalog.uri",
        "Catalog URI",
        "Iceberg REST catalog base URL.",
        "url",
        "Connection",
    ),
    (
        "catalog.warehouse",
        "Warehouse",
        "Warehouse identifier (S3 Tables: the table bucket).",
        "",
        "Connection",
    ),
    (
        "catalog.namespace",
        "Namespace",
        "Created when absent (create_tables).",
        "",
        "Connection",
    ),
    (
        "catalog.auth",
        "Authentication",
        "none; bearer (token_env or credential_env); sigv4 (every request signed with the s3_* credentials, D63).",
        "",
        "Authentication",
    ),
    (
        "catalog.sigv4_service",
        "SigV4 service",
        "Signing name: s3 for RustFS /iceberg, s3tables for AWS S3 Tables.",
        "",
        "Authentication",
    ),
    (
        "catalog.sigv4_region",
        "SigV4 region",
        "Unset = s3_region.",
        "",
        "Authentication",
    ),
    (
        "catalog.token_env",
        "Bearer token env var",
        "auth = bearer: env var holding a bearer token (never the value).",
        "env",
        "Authentication",
    ),
    (
        "catalog.credential_env",
        "OAuth2 credential env var",
        "auth = bearer: env var holding client_id:client_secret.",
        "env",
        "Authentication",
    ),
    (
        "catalog.s3_endpoint",
        "Data-file S3 endpoint",
        "When the catalog does not vend credentials.",
        "url",
        "Data files",
    ),
    (
        "catalog.s3_region",
        "Data-file S3 region",
        "",
        "",
        "Data files",
    ),
    (
        "catalog.s3_access_key_env",
        "Access key env var",
        "",
        "env",
        "Data files",
    ),
    (
        "catalog.s3_secret_key_env",
        "Secret key env var",
        "",
        "env",
        "Data files",
    ),
    (
        "catalog.s3_path_style",
        "Path-style addressing",
        "RustFS / MinIO.",
        "",
        "Data files",
    ),
    (
        "catalog.flush_interval",
        "Flush interval",
        "Max age of buffered rows before a commit.",
        "duration",
        "Tuning",
    ),
    (
        "catalog.flush_rows",
        "Flush rows",
        "Commit a table once this many rows are buffered.",
        "",
        "Tuning",
    ),
    (
        "catalog.max_buffer_rows",
        "Max buffered rows",
        "Beyond it durable appends fail fast and telemetry is dropped.",
        "",
        "Tuning",
    ),
    (
        "catalog.commit_timeout",
        "Commit timeout",
        "",
        "duration",
        "Tuning",
    ),
    (
        "catalog.backfill",
        "Backfill",
        "A repository that predates the catalog starts at its retained log start instead of its head.",
        "",
        "Tuning",
    ),
    (
        "catalog.create_tables",
        "Create tables",
        "Create the namespace and tables when missing.",
        "",
        "Tuning",
    ),
    // --- events ---
    (
        "events.webhook_url",
        "Webhook URL",
        "Each batch of ref events is POSTed as a JSON array (docs/EVENTS.md).",
        "url",
        "Webhook",
    ),
    (
        "events.webhook_secret",
        "Webhook secret",
        "X-Floe-Signature: sha256=<HMAC>. Entered values are sealed and never shown again.",
        "secret",
        "Webhook",
    ),
    (
        "events.sweep_interval",
        "Sweep interval",
        "Backstop sweep over every repository. 0 = off.",
        "duration",
        "Webhook",
    ),
    // --- codeintel (D52; restart-only) ---
    (
        "codeintel.enabled",
        "Code intelligence",
        "Index what this fleet maintains and serve it (needs a binary built with --features codeintel).",
        "",
        "General",
    ),
    (
        "codeintel.default_enabled",
        "Index repositories by default",
        "A repository's settings ([codeintel] enabled) override this per repository.",
        "",
        "General",
    ),
    (
        "codeintel.refs",
        "Indexed refs",
        "HEAD or ref patterns under refs/ whose tips are indexed; refs/archive/* and refs/follow/* never are.",
        "refpattern",
        "Selection",
    ),
    (
        "codeintel.exclude",
        "Exclude",
        "Path globs never indexed (on top of .gitattributes linguist-vendored/-generated).",
        "glob",
        "Selection",
    ),
    (
        "codeintel.max_file_bytes",
        "Max file size",
        "Larger files are not indexed (read and grep through git only).",
        "bytesize",
        "Selection",
    ),
    (
        "codeintel.require_catalog",
        "Require the catalog",
        "Write facts and embeddings to the code.* Iceberg tables (needs catalog.enabled); off = standalone.",
        "",
        "Storage",
    ),
    (
        "codeintel.cache_bytes",
        "Shard cache",
        "Local shard cache, counted inside cache.max_bytes.",
        "bytesize",
        "Storage",
    ),
    (
        "codeintel.retention",
        "Retention",
        "GC keeps retired generations this long; at least mcp.snapshot_ttl + 1h.",
        "duration",
        "Storage",
    ),
    (
        "codeintel.head_ttl",
        "Head revalidation",
        "A cached head.pb older than this is revalidated before use.",
        "duration",
        "Storage",
    ),
    // --- mcp (D55; restart-only; the HMAC key is server.auth.mcp_handle_secret) ---
    (
        "mcp.enabled",
        "MCP endpoints",
        "Serve /api/v1/mcp and /{owner}/{repo}/mcp (needs --features mcp, codeintel.enabled and server.auth.mcp_handle_secret or session_secret).",
        "",
        "General",
    ),
    (
        "mcp.allowed_origins",
        "Allowed origins",
        "Browser origins allowed to call the endpoints; empty = server.public_url.",
        "url",
        "Access",
    ),
    (
        "mcp.snapshot_ttl",
        "Snapshot lifetime",
        "Lifetime of a pinned snapshot handle (D57).",
        "duration",
        "Handles",
    ),
    (
        "mcp.min_handle_ttl",
        "Minimum handle lifetime",
        "pin refuses a retired generation with less lifetime left.",
        "duration",
        "Handles",
    ),
];

const ENUMS: &[(&str, &[&str])] = &[
    ("github_mirror.on_rewrite", &["archive", "refuse"]),
    ("catalog.auth", &["none", "bearer", "sigv4"]),
    ("codeintel.embed.provider", &["none", "local", "http"]),
    ("mcp.query_log", &["off", "sampled", "all"]),
];

/// Optional keys whose default is `null`, and the type they take when set.
const NULLABLE_STRINGS: &[&str] = &[
    "catalog.uri",
    "catalog.warehouse",
    "catalog.token_env",
    "catalog.credential_env",
    "catalog.s3_endpoint",
    "catalog.sigv4_region",
    "events.webhook_url",
];

fn live(path: &str) -> bool {
    !RESTART_ONLY
        .iter()
        .any(|r| path == *r || path.starts_with(&format!("{r}.")))
}

fn secret_schema(nullable: bool) -> Value {
    let variants = json!([
        {"type": "object", "title": "env reference", "properties": {"env": {"type": "string"}}, "required": ["env"], "additionalProperties": false},
        {"type": "object", "title": "value (input only; sealed)", "properties": {"value": {"type": "string"}}, "required": ["value"], "additionalProperties": false},
        {"type": "object", "title": "keep the stored value", "properties": {"redacted": {"const": true}}, "required": ["redacted"], "additionalProperties": false},
        {"type": "object", "title": "sealed", "properties": {"sealed": {"type": "string"}}, "required": ["sealed"], "additionalProperties": false}
    ]);
    let mut v = variants.as_array().cloned().unwrap_or_default();
    if nullable {
        v.push(json!({"type": "null"}));
    }
    json!({ "oneOf": v })
}

/// A table of the document (a section, or a sub-table such as `codeintel.embed`).
fn object_schema(path: &str, body: &Value) -> Value {
    let mut props = Map::new();
    if let Some(fields) = body.as_object() {
        for (key, default) in fields {
            let p = format!("{path}.{key}");
            let schema = if default.is_object() && !SECRET_PATHS.contains(&p.as_str()) {
                object_schema(&p, default)
            } else {
                property(&p, default)
            };
            props.insert(key.clone(), schema);
        }
    }
    json!({
        "type": "object",
        "title": section_title(path),
        "additionalProperties": false,
        "properties": props,
        "x-floe": {"live": live(path)},
    })
}

fn property(path: &str, default: &Value) -> Value {
    let ann = ANNOTATIONS.iter().find(|a| a.0 == path);
    let (title, help, format, group) = ann.map_or_else(
        || (path.rsplit('.').next().unwrap_or(path), "", "", "Other"),
        |a| (a.1, a.2, a.3, a.4),
    );
    let mut p = if SECRET_PATHS.contains(&path) {
        secret_schema(default.is_null())
    } else {
        match default {
            Value::Bool(_) => json!({"type": "boolean"}),
            Value::Number(n) if n.is_u64() => json!({"type": "integer", "minimum": 0}),
            Value::Number(_) => json!({"type": "number"}),
            Value::Array(_) => json!({"type": "array", "items": {"type": "string"}}),
            Value::Null if NULLABLE_STRINGS.contains(&path) => json!({"type": ["string", "null"]}),
            _ => json!({"type": "string"}),
        }
    };
    if let Some(obj) = p.as_object_mut() {
        obj.insert("title".into(), json!(title));
        if !help.is_empty() {
            obj.insert("description".into(), json!(help));
        }
        obj.insert("default".into(), default.clone());
        if let Some((_, values)) = ENUMS.iter().find(|e| e.0 == path) {
            obj.insert("enum".into(), json!(values));
        }
        let fmt = if format.is_empty() && SECRET_PATHS.contains(&path) {
            "secret"
        } else {
            format
        };
        obj.insert(
            "x-floe".into(),
            json!({"format": fmt, "group": group, "live": live(path)}),
        );
    }
    p
}

/// The schema of the config document.
pub fn document_schema() -> Value {
    let defaults = RuntimeConfig::default().to_json().unwrap_or_default();
    let mut sections = Map::new();
    if let Some(top) = defaults.as_object() {
        for (section, body) in top {
            sections.insert(section.clone(), object_schema(section, body));
        }
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "floe:config-document",
        "title": "floe runtime configuration (D60)",
        "type": "object",
        "additionalProperties": false,
        "properties": sections,
    })
}

fn section_title(section: &str) -> &str {
    match section {
        "github_mirror" => "GitHub mirroring",
        "catalog" => "Catalog (Iceberg audit tables)",
        "events" => "Events webhook",
        "codeintel" => "Code intelligence",
        "codeintel.embed" => "Embeddings",
        "codeintel.catalog" => "Code tables (Iceberg)",
        "mcp" => "MCP endpoints",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key of the document is in the schema, and every annotation names a key.
    #[test]
    fn schema_covers_the_document() {
        let s = document_schema();
        let defaults = RuntimeConfig::default().to_json().unwrap();
        fn walk(schema: &Value, doc: &Value, path: &str) {
            for (key, v) in doc.as_object().unwrap() {
                let node = schema["properties"].get(key);
                assert!(node.is_some(), "{path}.{key} missing from the schema");
                let p = format!("{path}.{key}");
                if v.is_object() && !SECRET_PATHS.contains(&p.as_str()) {
                    assert_eq!(node.unwrap()["type"], "object", "{p}");
                    walk(node.unwrap(), v, &p);
                }
            }
        }
        for (section, body) in defaults.as_object().unwrap() {
            walk(&s["properties"][section], body, section);
        }
        for (path, ..) in ANNOTATIONS {
            let found = path.split('.').try_fold(&defaults, |v, part| v.get(part));
            assert!(found.is_some(), "stale annotation {path}");
        }
        // Nested tables and runtime code-intel sections (D52, D55): restart-only, typed.
        let ci = &s["properties"]["codeintel"];
        assert_eq!(ci["x-floe"]["live"], false);
        assert_eq!(
            ci["properties"]["embed"]["properties"]["provider"]["enum"][0],
            "none"
        );
        assert_eq!(
            ci["properties"]["retention"]["x-floe"]["format"],
            "duration"
        );
        assert_eq!(
            s["properties"]["mcp"]["properties"]["enabled"]["x-floe"]["live"],
            false
        );
        assert!(
            s["properties"]["mcp"]["properties"]
                .get("handle_secret")
                .is_none()
        );
        let token = &s["properties"]["github_mirror"]["properties"]["token"];
        assert_eq!(token["x-floe"]["format"], "secret");
        assert_eq!(token["x-floe"]["live"], true);
        assert_eq!(
            s["properties"]["catalog"]["properties"]["uri"]["x-floe"]["live"],
            false
        );
        assert_eq!(
            s["properties"]["github_mirror"]["properties"]["git_url"]["x-floe"]["live"],
            false
        );
        assert_eq!(
            s["properties"]["github_mirror"]["properties"]["on_rewrite"]["enum"][0],
            "archive"
        );
        assert_eq!(
            s["properties"]["catalog"]["properties"]["uri"]["type"][1],
            "null"
        );
    }
}
