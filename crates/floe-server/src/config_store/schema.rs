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
        "Copy repositories from GitHub into floe and keep them up to date. Runs on one maintenance host at a time.",
        "",
        "General",
    ),
    (
        "github_mirror.api_url",
        "API URL",
        "Where floe asks GitHub which repositories exist: https://api.github.com, or https://ghe.example.com/api/v3 for GitHub Enterprise Server.",
        "url",
        "Credential",
    ),
    (
        "github_mirror.git_url",
        "Git URL",
        "Where repositories are cloned from. The token is only ever sent to this host.",
        "url",
        "Credential",
    ),
    (
        "github_mirror.token",
        "Token",
        "A personal access token: classic with the repo scope, or fine-grained with read access to contents and metadata. A typed value is encrypted on save and never shown again; an environment variable keeps it out of storage entirely.",
        "secret",
        "Credential",
    ),
    (
        "github_mirror.users",
        "Users",
        "GitHub users whose repositories are copied. Use @me for the token’s own account, private repositories included.",
        "",
        "Sources",
    ),
    (
        "github_mirror.orgs",
        "Organizations",
        "Every repository the token can see in these organizations.",
        "",
        "Sources",
    ),
    (
        "github_mirror.starred",
        "Starred by",
        "Copy the repositories these users have starred (@me allowed).",
        "",
        "Sources",
    ),
    (
        "github_mirror.repos",
        "Explicit repositories",
        "Always copied, one owner/name per line — even if archived, a fork or outside the include patterns.",
        "",
        "Sources",
    ),
    (
        "github_mirror.include",
        "Include",
        "Only repositories matching one of these owner/name patterns are copied. * matches within one path segment; case does not matter.",
        "glob",
        "Selection",
    ),
    (
        "github_mirror.exclude",
        "Exclude",
        "Never copied, whatever else matches. An exact owner/name here pauses that repository.",
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
        "floe has no per-repository read permissions: confirm that everyone who can read this floe may read every copied private repository.",
        "",
        "Selection",
    ),
    (
        "github_mirror.max_repo_size",
        "Max repository size",
        "Larger repositories are skipped (import them by hand). 0 means no limit.",
        "bytesize",
        "Selection",
    ),
    (
        "github_mirror.lfs",
        "LFS read-through",
        "Fetch missing Git LFS files from GitHub on demand.",
        "",
        "Repositories",
    ),
    (
        "github_mirror.follow",
        "Followed refs",
        "Which branches and tags are kept in sync, as ref patterns.",
        "refpattern",
        "Repositories",
    ),
    (
        "github_mirror.on_rewrite",
        "On rewrite",
        "When GitHub history is rewritten (a force push): archive keeps the old commits under refs/archive/; refuse accepts fast-forwards only.",
        "",
        "Repositories",
    ),
    (
        "github_mirror.read_only",
        "Read-only",
        "Refuse pushes to copied repositories, so they only change by syncing from GitHub.",
        "",
        "Repositories",
    ),
    (
        "github_mirror.interval",
        "Discovery interval",
        "How often floe looks for new repositories on GitHub. 0 means only at startup.",
        "duration",
        "Advanced",
    ),
    (
        "github_mirror.follow_interval",
        "Follow backstop",
        "Longest time between syncs of one repository; GitHub pushes trigger a sync sooner.",
        "duration",
        "Advanced",
    ),
    (
        "github_mirror.max_new_per_pass",
        "New repositories per pass",
        "At most this many repositories are added per discovery run.",
        "",
        "Advanced",
    ),
    (
        "github_mirror.min_rate_remaining",
        "Rate-limit floor",
        "Pause discovery when fewer GitHub API requests than this remain.",
        "",
        "Advanced",
    ),
    (
        "github_mirror.gone_after",
        "Gone after",
        "How long a repository must be missing on GitHub before floe stops syncing it.",
        "duration",
        "Advanced",
    ),
    (
        "github_mirror.lease_ttl",
        "Lease TTL",
        "How long one host holds the right to run the mirror before another may take over.",
        "duration",
        "Advanced",
    ),
    // --- catalog ---
    (
        "catalog.enabled",
        "Write audit tables",
        "Record every push and ref change in Apache Iceberg audit tables. Needs a floe build with the catalog feature.",
        "",
        "General",
    ),
    (
        "catalog.uri",
        "Catalog URI",
        "The base URL of your Iceberg REST catalog.",
        "url",
        "Connection",
    ),
    (
        "catalog.warehouse",
        "Warehouse",
        "The warehouse to write to. For AWS S3 Tables, the table bucket ARN.",
        "",
        "Connection",
    ),
    (
        "catalog.namespace",
        "Namespace",
        "The namespace the audit tables live in; created if it does not exist.",
        "",
        "Connection",
    ),
    (
        "catalog.auth",
        "Authentication",
        "How floe signs in to the catalog: none, a bearer token or OAuth2 client credentials, or AWS Signature V4 using the storage credentials below.",
        "",
        "Authentication",
    ),
    (
        "catalog.sigv4_service",
        "SigV4 service",
        "The service name to sign for: s3 for RustFS, s3tables for AWS S3 Tables.",
        "",
        "Authentication",
    ),
    (
        "catalog.sigv4_region",
        "SigV4 region",
        "Leave empty to use the storage region.",
        "",
        "Authentication",
    ),
    (
        "catalog.token_env",
        "Bearer token env var",
        "The environment variable that holds the bearer token on every host — never the token itself.",
        "env",
        "Authentication",
    ),
    (
        "catalog.credential_env",
        "OAuth2 credential env var",
        "Alternatively, the environment variable that holds OAuth2 client credentials as client_id:client_secret.",
        "env",
        "Authentication",
    ),
    (
        "catalog.s3_endpoint",
        "Data-file S3 endpoint",
        "Only needed when the catalog does not hand out storage credentials itself.",
        "url",
        "Storage",
    ),
    (
        "catalog.s3_region",
        "Data-file S3 region",
        "Region of the storage that holds the table data files.",
        "",
        "Storage",
    ),
    (
        "catalog.s3_access_key_env",
        "Access key env var",
        "The environment variable that holds the storage access key ID.",
        "env",
        "Storage",
    ),
    (
        "catalog.s3_secret_key_env",
        "Secret key env var",
        "The environment variable that holds the storage secret access key.",
        "env",
        "Storage",
    ),
    (
        "catalog.s3_path_style",
        "Path-style addressing",
        "Turn on for RustFS and MinIO.",
        "",
        "Storage",
    ),
    (
        "catalog.flush_interval",
        "Flush interval",
        "Buffered rows are written at least this often.",
        "duration",
        "Advanced",
    ),
    (
        "catalog.flush_rows",
        "Flush rows",
        "Write a table as soon as this many rows are waiting.",
        "",
        "Advanced",
    ),
    (
        "catalog.max_buffer_rows",
        "Max buffered rows",
        "Beyond this, audit writes fail fast and telemetry rows are dropped.",
        "",
        "Advanced",
    ),
    (
        "catalog.commit_timeout",
        "Commit timeout",
        "Give up on a table write after this long.",
        "duration",
        "Advanced",
    ),
    (
        "catalog.backfill",
        "Backfill",
        "Record the retained history of repositories that existed before the catalog was turned on, not just new changes.",
        "",
        "Advanced",
    ),
    (
        "catalog.create_tables",
        "Create tables",
        "Create the namespace and tables if they do not exist.",
        "",
        "Advanced",
    ),
    // --- events ---
    (
        "events.webhook_url",
        "Webhook URL",
        "Every batch of ref changes is sent here as a JSON array in a POST request.",
        "url",
        "Webhook",
    ),
    (
        "events.webhook_secret",
        "Webhook secret",
        "Used to sign each delivery (the X-Floe-Signature header), so the receiver can verify it came from floe. A typed value is encrypted on save and never shown again.",
        "secret",
        "Webhook",
    ),
    (
        "events.sweep_interval",
        "Sweep interval",
        "How often floe re-checks every repository for changes it might have missed. 0 turns it off.",
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
        "Lifetime of a pinned snapshot handle.",
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

/// Groups the admin UI folds away by default (`x-floe.advanced`).
const ADVANCED_GROUPS: &[&str] = &["Advanced"];

/// Fields that only apply for some values of a sibling key (`x-floe.when`:
/// `{sibling: [values…]}`), so the admin UI shows them only then.
fn show_when(path: &str) -> Option<Value> {
    match path {
        "catalog.token_env" | "catalog.credential_env" => Some(json!({"auth": ["bearer"]})),
        "catalog.sigv4_service" | "catalog.sigv4_region" => Some(json!({"auth": ["sigv4"]})),
        "github_mirror.private_visible_to_all_readers" => Some(json!({"include_private": [true]})),
        _ => None,
    }
}

/// A one-sentence, plain-language summary of a section (the schema's `description`).
fn section_description(section: &str) -> Option<&'static str> {
    match section {
        "github_mirror" => Some(
            "Copies repositories from GitHub or GitHub Enterprise into floe and keeps them in sync.",
        ),
        "catalog" => Some(
            "Writes an audit trail of every push and ref change to Apache Iceberg tables in your catalog.",
        ),
        "events" => Some(
            "Sends every ref change to a webhook of your choice, signed so the receiver can verify it.",
        ),
        "codeintel" => Some("Indexes hosted code for navigation and search."),
        "mcp" => Some("Serves the code index to agents over the Model Context Protocol."),
        _ => None,
    }
}

/// The section's groups in annotation order (the order the UI presents them in).
fn section_groups(section: &str) -> Vec<&'static str> {
    let prefix = format!("{section}.");
    let mut groups: Vec<&'static str> = Vec::new();
    for a in ANNOTATIONS {
        if a.0.strip_prefix(&prefix).is_some_and(|k| !k.contains('.')) && !groups.contains(&a.4) {
            groups.push(a.4);
        }
    }
    groups
}

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
    let mut node = json!({
        "type": "object",
        "title": section_title(path),
        "additionalProperties": false,
        "properties": props,
        "x-floe": {"live": live(path), "groups": section_groups(path)},
    });
    if let (Some(obj), Some(d)) = (node.as_object_mut(), section_description(path)) {
        obj.insert("description".into(), json!(d));
    }
    node
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
        let mut x = json!({
            "format": fmt,
            "group": group,
            "live": live(path),
            "advanced": ADVANCED_GROUPS.contains(&group),
            "order": ANNOTATIONS.iter().position(|a| a.0 == path).unwrap_or(ANNOTATIONS.len()),
        });
        if let (Some(xo), Some(w)) = (x.as_object_mut(), show_when(path)) {
            xo.insert("when".into(), w);
        }
        obj.insert("x-floe".into(), x);
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
        "title": "floe runtime configuration",
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

    /// Asserts every key of `doc` (recursively) has a schema node.
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

    /// Every key of the document is in the schema, and every annotation names a key.
    #[test]
    fn schema_covers_the_document() {
        let s = document_schema();
        let defaults = RuntimeConfig::default().to_json().unwrap();
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
        // Presentation hints for the admin UI: conditional fields, folded groups, order.
        let cat = &s["properties"]["catalog"];
        assert_eq!(
            cat["properties"]["token_env"]["x-floe"]["when"]["auth"][0],
            "bearer"
        );
        assert_eq!(
            cat["properties"]["sigv4_region"]["x-floe"]["when"]["auth"][0],
            "sigv4"
        );
        assert_eq!(cat["properties"]["flush_rows"]["x-floe"]["advanced"], true);
        assert_eq!(cat["properties"]["uri"]["x-floe"]["advanced"], false);
        assert_eq!(cat["x-floe"]["groups"][0], "General");
        assert!(cat["description"].is_string());
        // Help text is plain language: no decision numbers.
        let text = s.to_string();
        assert!(
            !regex_like_decision(&text),
            "a help text cites a decision number"
        );
    }

    /// Whether `text` contains a decision reference such as `D63`.
    fn regex_like_decision(text: &str) -> bool {
        let b = text.as_bytes();
        b.windows(3).enumerate().any(|(i, w)| {
            w[0] == b'D'
                && w[1].is_ascii_digit()
                && w[2].is_ascii_digit()
                && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
        })
    }
}
