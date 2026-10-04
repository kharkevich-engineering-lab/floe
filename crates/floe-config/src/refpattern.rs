//! `[upstream] follow` ref patterns (D48): the refs a repository keeps equal to
//! upstream's. They are handed to `git fetch` verbatim as refspec sources, so they
//! use **git refspec glob semantics** (not POLICY.md's doublestar dialect):
//!
//! - exact: `refs/heads/main`;
//! - glob: exactly one `*`, matching any characters **including `/`**
//!   (`refs/heads/*` matches `refs/heads/feat/x`);
//! - negative: a leading `^` (exact or glob) excludes (git ≥ 2.29).
//!
//! `refs/archive/` (where follow keeps rewritten tips) and `refs/follow/` (the
//! scratch's copy of upstream) are reserved: never followed, whatever the patterns
//! say. Pure string code, here rather than in floe-git because config validation
//! needs it and floe-git already depends on this crate.

use std::fmt;

/// Namespaces follow never touches: archived tips and the fetch scratch.
pub const RESERVED: [&str; 2] = ["refs/archive/", "refs/follow/"];

/// A parsed, validated, non-empty `follow` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefPatterns {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    negative: bool,
    /// Without the `^`.
    pattern: String,
}

/// Why an entry of `follow` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefPatternError {
    pub entry: String,
    pub reason: &'static str,
}

impl fmt::Display for RefPatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "upstream.follow entry {:?}: {} (patterns are git refspec sources: refs/heads/main, refs/heads/*, ^refs/heads/x/*)",
            self.entry, self.reason
        )
    }
}

impl std::error::Error for RefPatternError {}

impl RefPatterns {
    /// Parse a **non-empty** list (an empty `follow` is "follow off" and is
    /// checked by the caller before this).
    pub fn parse(list: &[String]) -> Result<RefPatterns, RefPatternError> {
        let mut entries = Vec::with_capacity(list.len());
        for raw in list {
            let err = |reason| RefPatternError {
                entry: raw.clone(),
                reason,
            };
            let (negative, pattern) = match raw.strip_prefix('^') {
                Some(p) => (true, p),
                None => (false, raw.as_str()),
            };
            if !pattern.starts_with("refs/") {
                return Err(err("must start with refs/"));
            }
            if RESERVED.iter().any(|r| pattern.starts_with(r)) {
                return Err(err("refs/archive/ and refs/follow/ are reserved"));
            }
            if pattern.matches('*').count() > 1 {
                return Err(err("at most one `*`"));
            }
            check_refspec_pattern(pattern).map_err(err)?;
            entries.push(Entry {
                negative,
                pattern: pattern.to_string(),
            });
        }
        if !entries.iter().any(|e| !e.negative) {
            return Err(RefPatternError {
                entry: list.join(", "),
                reason: "needs at least one positive (non-`^`) entry",
            });
        }
        Ok(RefPatterns { entries })
    }

    /// Whether follow keeps `name` equal to upstream's: a positive entry matches,
    /// no negative entry does, and the name is outside the reserved namespaces.
    pub fn matches(&self, name: &str) -> bool {
        if RESERVED.iter().any(|r| name.starts_with(r)) {
            return false;
        }
        let mut positive = false;
        for e in &self.entries {
            if glob_match(&e.pattern, name) {
                if e.negative {
                    return false;
                }
                positive = true;
            }
        }
        positive
    }

    /// The positive entries without a `*` (one ref each): the fetch leaves out the
    /// ones upstream does not advertise (git fails a fetch of a missing exact ref).
    pub fn exact(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|e| !e.negative && !e.pattern.contains('*'))
            .map(|e| e.pattern.as_str())
    }

    /// The fetch refspecs: positive `+<src>:refs/follow/<src minus refs/>` (a `*`
    /// carries through), negative `^<src>`, then `^refs/archive/*` and
    /// `^refs/follow/*` so an upstream that is itself a floe never feeds its archive
    /// into ours. Exact entries in `skip` are left out. Empty when no positive
    /// refspec is left (nothing to fetch).
    pub fn refspecs(&self, skip: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.negative || !skip.contains(&e.pattern.as_str()))
            .map(|e| {
                if e.negative {
                    format!("^{}", e.pattern)
                } else {
                    format!("+{}:{}", e.pattern, scratch_ref(&e.pattern))
                }
            })
            .collect();
        if !out.iter().any(|r| r.starts_with('+')) {
            return Vec::new();
        }
        out.extend(RESERVED.iter().map(|r| format!("^{r}*")));
        out
    }
}

/// `refs/heads/main` → `refs/follow/heads/main`: the fetch scratch's copy of an
/// upstream ref (also for patterns: `refs/heads/*` → `refs/follow/heads/*`).
pub fn scratch_ref(name: &str) -> String {
    format!("refs/follow/{}", name.strip_prefix("refs/").unwrap_or(name))
}

/// Git refspec glob: at most one `*`, matching any run of characters (`/` included).
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post)
        }
    }
}

/// `git check-ref-format --refspec-pattern` (one `*` allowed; the count is checked
/// by the caller), in Rust: no subprocess per settings publish. `floe-git` tests it
/// against git itself on a corpus.
pub fn check_refspec_pattern(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name == "@" {
        return Err("empty or `@`");
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("starts or ends with `/`");
    }
    if name.ends_with('.') {
        return Err("ends with `.`");
    }
    if name.contains("..") {
        return Err("contains `..`");
    }
    if name.contains("@{") {
        return Err("contains `@{`");
    }
    if name.contains("//") {
        return Err("contains `//`");
    }
    if name
        .chars()
        .any(|c| c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '[' | '\\'))
    {
        return Err("contains a control character, space, or one of ~^:?[\\");
    }
    for comp in name.split('/') {
        if comp.starts_with('.') {
            return Err("a component starts with `.`");
        }
        #[allow(clippy::case_sensitive_file_extension_comparisons)] // git's rule is case-sensitive
        if comp.ends_with(".lock") {
            return Err("a component ends with `.lock`");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(list: &[&str]) -> Result<RefPatterns, RefPatternError> {
        RefPatterns::parse(&list.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn ref_patterns_parse_and_match() {
        // (patterns, name, matches)
        let cases: &[(&[&str], &str, bool)] = &[
            (&["refs/heads/main"], "refs/heads/main", true),
            (&["refs/heads/main"], "refs/heads/main2", false),
            (&["refs/heads/*"], "refs/heads/feat/x", true),
            (&["refs/heads/*"], "refs/tags/v1", false),
            (&["refs/heads/feat-*"], "refs/heads/feat-a", true),
            (&["refs/heads/feat-*"], "refs/heads/fix-a", false),
            (&["refs/*/main"], "refs/heads/main", true),
            (&["refs/*/main"], "refs/heads/x/main", true),
            (&["refs/*/main"], "refs/heads/maint", false),
            (
                &["refs/heads/*", "^refs/heads/dependabot/*"],
                "refs/heads/dependabot/npm",
                false,
            ),
            (
                &["refs/heads/*", "^refs/heads/dependabot/*"],
                "refs/heads/main",
                true,
            ),
            (
                &["refs/heads/*", "^refs/heads/wip"],
                "refs/heads/wip",
                false,
            ),
            // Reserved namespaces are never followed, whatever the patterns say.
            (&["refs/*"], "refs/archive/1/refs/heads/main", false),
            (&["refs/*"], "refs/follow/heads/main", false),
            (&["refs/*"], "refs/pull/1/head", true),
            (&["refs/*"], "HEAD", false),
        ];
        for (list, name, want) in cases {
            let pats = p(list).unwrap();
            assert_eq!(pats.matches(name), *want, "{list:?} vs {name}");
        }

        for bad in [
            &["heads/main"][..],
            &["refs/heads/**"],
            &["refs/*/x/*"],
            &["refs/archive/*"],
            &["^refs/follow/x", "refs/heads/*"],
            &["refs/heads/a..b"],
            &["refs/heads/a b"],
            &["refs/heads/a:b"],
            &["refs/heads/x.lock"],
            &["refs/heads/.x"],
            &["refs/heads/"],
            &["refs/heads//x"],
            &["refs/heads/a@{1}"],
            &["refs/heads/x."],
            &["refs/heads/[ab]"],
            // Negative-only: nothing to follow.
            &["^refs/heads/x"],
        ] {
            assert!(p(bad).is_err(), "{bad:?} must be refused");
        }
        let e = p(&["refs/heads/a b"]).unwrap_err().to_string();
        assert!(e.contains("refs/heads/a b"), "{e}");
    }

    #[test]
    fn refspecs_carry_the_glob_and_exclude_reserved() {
        let pats = p(&["refs/heads/*", "refs/tags/v1", "^refs/heads/dependabot/*"]).unwrap();
        assert_eq!(
            pats.refspecs(&[]),
            vec![
                "+refs/heads/*:refs/follow/heads/*",
                "+refs/tags/v1:refs/follow/tags/v1",
                "^refs/heads/dependabot/*",
                "^refs/archive/*",
                "^refs/follow/*",
            ]
        );
        assert_eq!(pats.exact().collect::<Vec<_>>(), vec!["refs/tags/v1"]);
        assert_eq!(pats.refspecs(&["refs/tags/v1"]).len(), 4);
        // Every positive skipped: nothing to fetch.
        let only = p(&["refs/heads/main"]).unwrap();
        assert!(only.refspecs(&["refs/heads/main"]).is_empty());
    }
}
