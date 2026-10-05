//! Which discovered repositories are mirrored (§B.4), pure. The first rule that
//! decides wins:
//!
//! 1. `exclude` matches ⇒ out (explicit `repos` too);
//! 2. private and not `include_private` ⇒ out (explicit too: a safety rule);
//! 3. not explicit, and archived ∧ `skip_archived` or fork ∧ `skip_forks` ⇒ out;
//! 4. not explicit, and no `include` glob matches ⇒ out;
//! 5. larger than `max_repo_size` ⇒ too large (selected, never auto-created);
//! 6. otherwise in.

use floe_config::GithubMirrorConfig;

use crate::source::RemoteRepo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    In,
    TooLarge,
    Out,
}

/// Whether `full_name` is one of the explicit `repos` (case-insensitive).
pub fn is_explicit(cfg: &GithubMirrorConfig, full_name: &str) -> bool {
    cfg.repos.iter().any(|r| r.eq_ignore_ascii_case(full_name))
}

/// The verdict for one repository; `explicit` = named in `repos` (by its current
/// or, after a transfer, its recorded name).
pub fn select(cfg: &GithubMirrorConfig, r: &RemoteRepo, explicit: bool) -> Verdict {
    let full = r.full_name();
    if cfg.exclude.iter().any(|g| glob_match(g, &full)) {
        return Verdict::Out;
    }
    if r.private && !cfg.include_private {
        return Verdict::Out;
    }
    if !explicit {
        if (r.archived && cfg.skip_archived) || (r.fork && cfg.skip_forks) {
            return Verdict::Out;
        }
        if !cfg.include.iter().any(|g| glob_match(g, &full)) {
            return Verdict::Out;
        }
    }
    let limit = cfg.max_repo_size.as_u64();
    if limit > 0 && r.size_kb.saturating_mul(1024) > limit {
        return Verdict::TooLarge;
    }
    Verdict::In
}

/// `owner/name` glob: `*` matches any run of characters except `/`; ASCII
/// case-insensitive. No other metacharacters.
pub fn glob_match(glob: &str, s: &str) -> bool {
    let g: Vec<u8> = glob.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let t: Vec<u8> = s.bytes().map(|b| b.to_ascii_lowercase()).collect();
    matches_from(&g, &t)
}

fn matches_from(g: &[u8], t: &[u8]) -> bool {
    match g.split_first() {
        None => t.is_empty(),
        Some((b'*', rest)) => {
            // Try every split that does not cross a '/'.
            let mut i = 0;
            loop {
                if matches_from(rest, t.get(i..).unwrap_or_default()) {
                    return true;
                }
                match t.get(i) {
                    Some(b'/') | None => return false,
                    Some(_) => i += 1,
                }
            }
        }
        Some((c, rest)) => match t.split_first() {
            Some((d, trest)) if c == d => matches_from(rest, trest),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(full: &str) -> RemoteRepo {
        let (owner, name) = full.split_once('/').unwrap();
        RemoteRepo {
            id: "1".into(),
            owner: owner.into(),
            name: name.into(),
            private: false,
            archived: false,
            fork: false,
            disabled: false,
            default_branch: Some("main".into()),
            pushed_at: None,
            size_kb: 10,
        }
    }

    #[test]
    fn globs_stop_at_slash_and_ignore_case() {
        assert!(glob_match("*/*", "Acme/Widgets"));
        assert!(glob_match("acme/*", "ACME/x"));
        assert!(glob_match("*/w*s", "a/widgets"));
        assert!(!glob_match("acme/*", "other/x"));
        assert!(!glob_match("*", "a/b"));
        assert!(!glob_match("a*/b", "a/x/b"));
    }

    /// The §B.4 table, row for row.
    #[test]
    fn selection_table() {
        let mut cfg = GithubMirrorConfig {
            include: vec!["acme/*".into()],
            exclude: vec!["*/secret".into()],
            include_private: false,
            ..GithubMirrorConfig::default()
        };
        // explicit, exclude ⇒ out
        assert_eq!(select(&cfg, &repo("acme/secret"), true), Verdict::Out);
        // explicit, private with include_private = false ⇒ out
        let mut p = repo("acme/p");
        p.private = true;
        assert_eq!(select(&cfg, &p, true), Verdict::Out);
        // explicit, archived fork outside include ⇒ in
        let mut a = repo("other/a");
        a.archived = true;
        a.fork = true;
        assert_eq!(select(&cfg, &a, true), Verdict::In);
        // listed, archived (skip) ⇒ out
        let mut b = repo("acme/b");
        b.archived = true;
        assert_eq!(select(&cfg, &b, false), Verdict::Out);
        // listed (starred) fork ⇒ out
        let mut f = repo("acme/f");
        f.fork = true;
        assert_eq!(select(&cfg, &f, false), Verdict::Out);
        // listed, include no match ⇒ out
        assert_eq!(select(&cfg, &repo("other/x"), false), Verdict::Out);
        // listed, everything passes ⇒ in
        assert_eq!(select(&cfg, &repo("acme/x"), false), Verdict::In);
        // too large, explicit too
        let mut big = repo("acme/big");
        big.size_kb = 3 * 1024 * 1024;
        assert_eq!(select(&cfg, &big, false), Verdict::TooLarge);
        assert_eq!(select(&cfg, &big, true), Verdict::TooLarge);
        cfg.max_repo_size = floe_config::ByteSize::b(0);
        assert_eq!(select(&cfg, &big, false), Verdict::In);
    }

    #[test]
    fn default_config_selects_owned_non_archived_non_forks() {
        let cfg = GithubMirrorConfig::default();
        let mut p = repo("me/private");
        p.private = true;
        assert_eq!(select(&cfg, &p, false), Verdict::In);
        let mut f = repo("me/fork");
        f.fork = true;
        assert_eq!(select(&cfg, &f, false), Verdict::Out);
        assert!(is_explicit(
            &GithubMirrorConfig {
                repos: vec!["Acme/W".into()],
                ..GithubMirrorConfig::default()
            },
            "acme/w"
        ));
    }
}
