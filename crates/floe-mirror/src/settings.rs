//! The `[upstream]` table the mirror owns (§B.7.1): render it, hash it
//! canonically, and merge it into a repository's settings document without
//! touching the operator's other sections.

use floe_config::{GithubMirrorConfig, OnRewrite};
use sha2::{Digest, Sha256};

use crate::source::{RemoteRepo, Source};
use crate::state::Status;

/// Settings author of every publish the mirror makes (the "who wrote it" rule).
pub const AUTHOR: &str = "github-mirror";

/// `follow_interval` written for an archived repository (nothing moves there).
const ARCHIVED_FOLLOW_INTERVAL: &str = "24h";

/// The `upstream.source` marker: `<kind>:<id>`.
pub fn marker(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

/// The table for one repository in one status: frozen statuses follow nothing
/// and drop `lfs` (no authenticated read-through for a repository the mirror
/// no longer selects or reaches); archived repositories keep following at a
/// long interval.
pub fn render(
    source: &dyn Source,
    cfg: &GithubMirrorConfig,
    r: &RemoteRepo,
    status: Status,
) -> toml::Table {
    let mut t = toml::Table::new();
    t.insert("source".into(), marker(source.kind(), &r.id).into());
    t.insert("git".into(), source.git_url(r).into());
    if cfg.lfs
        && !status.frozen()
        && let Some(lfs) = source.lfs_url(r)
    {
        t.insert("lfs".into(), lfs.into());
    }
    let follow: Vec<toml::Value> = if status.frozen() {
        Vec::new()
    } else {
        cfg.follow.iter().map(|p| p.as_str().into()).collect()
    };
    t.insert("follow".into(), toml::Value::Array(follow));
    let on_rewrite = match cfg.on_rewrite {
        OnRewrite::Archive => "archive",
        OnRewrite::Refuse => "refuse",
    };
    t.insert("on_rewrite".into(), on_rewrite.into());
    if let Some(b) = &r.default_branch {
        t.insert("head".into(), format!("refs/heads/{b}").into());
    }
    let interval = if r.archived {
        ARCHIVED_FOLLOW_INTERVAL.to_string()
    } else {
        humantime::format_duration(cfg.follow_interval).to_string()
    };
    t.insert("follow_interval".into(), interval.into());
    t
}

/// sha256 (hex) of the table's canonical TOML (`toml::Table` is key-ordered).
pub fn hash(t: &toml::Table) -> String {
    let text = toml::to_string(t).unwrap_or_default();
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The `[upstream]` table of a settings document (empty when none or unparsable).
pub fn upstream_of(settings_toml: &str) -> toml::Table {
    settings_toml
        .parse::<toml::Table>()
        .ok()
        .and_then(|mut doc| match doc.remove("upstream") {
            Some(toml::Value::Table(t)) => Some(t),
            _ => None,
        })
        .unwrap_or_default()
}

/// The marker in a settings document (`upstream.source`), if any.
pub fn source_of(settings_toml: &str) -> Option<String> {
    upstream_of(settings_toml)
        .get("source")
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

/// `settings_toml` with `[upstream]` replaced by `upstream`; every other section
/// is kept (re-serialized through `toml::Table`).
pub fn merge(settings_toml: &str, upstream: &toml::Table) -> anyhow::Result<String> {
    let mut doc: toml::Table = if settings_toml.trim().is_empty() {
        toml::Table::new()
    } else {
        settings_toml.parse()?
    };
    doc.insert("upstream".into(), toml::Value::Table(upstream.clone()));
    Ok(toml::to_string(&doc)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSource;

    fn repo() -> RemoteRepo {
        RemoteRepo {
            id: "42".into(),
            owner: "Acme".into(),
            name: "Widgets".into(),
            private: true,
            archived: false,
            fork: false,
            disabled: false,
            default_branch: Some("main".into()),
            pushed_at: None,
            size_kb: 1,
        }
    }

    #[test]
    fn renders_freezes_and_hashes_stably() {
        let src = FakeSource::new("https://github.com");
        let cfg = GithubMirrorConfig::default();
        let t = render(&src, &cfg, &repo(), Status::Active);
        let text = toml::to_string(&t).unwrap();
        assert!(text.contains("source = \"github:42\""), "{text}");
        assert!(
            text.contains("git = \"https://github.com/Acme/Widgets.git\""),
            "{text}"
        );
        assert!(text.contains("head = \"refs/heads/main\""), "{text}");
        assert!(text.contains("follow_interval = \"10m\""), "{text}");
        assert!(text.contains("refs/tags/*"), "{text}");
        // The rendered table is a valid [upstream] for the host config.
        let doc = merge("", &t).unwrap();
        floe_config::Config::default().with_settings(&doc).unwrap();

        let frozen = render(&src, &cfg, &repo(), Status::Gone);
        assert_eq!(frozen.get("follow"), Some(&toml::Value::Array(vec![])));
        floe_config::Config::default()
            .with_settings(&merge("", &frozen).unwrap())
            .unwrap();
        let mut archived = repo();
        archived.archived = true;
        let a = render(&src, &cfg, &archived, Status::Active);
        assert_eq!(a.get("follow_interval").and_then(|v| v.as_str()), Some("24h"));

        assert_eq!(hash(&t), hash(&t.clone()));
        assert_ne!(hash(&t), hash(&frozen));
        // Round trip through a document keeps the hash.
        assert_eq!(hash(&upstream_of(&doc)), hash(&t));
        assert_eq!(source_of(&doc).as_deref(), Some("github:42"));
    }

    #[test]
    fn merge_preserves_other_sections() {
        let src = FakeSource::new("https://github.com");
        let t = render(&src, &GithubMirrorConfig::default(), &repo(), Status::Active);
        let before = "[bundles]\nmain_only = true\n\n[upstream]\nfollow = []\ngit = \"https://x/y\"\n";
        let after = merge(before, &t).unwrap();
        let doc: toml::Table = after.parse().unwrap();
        assert_eq!(
            doc.get("bundles")
                .and_then(|b| b.get("main_only"))
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(hash(&upstream_of(&after)), hash(&t));
    }
}
