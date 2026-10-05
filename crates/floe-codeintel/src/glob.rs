//! Path globs (gitattributes / gitignore flavour) for `.gitattributes` rules and
//! `codeintel.exclude`: `*` and `?` stop at `/`, `**` crosses directories (`**/` matches zero
//! or more leading directories), `[abc]`, `[a-z]`, `[!a]` classes.

fn tail(s: &[u8], n: usize) -> &[u8] {
    s.get(n..).unwrap_or_default()
}

/// Whether `pattern` matches the whole of `path` (repository-relative, `/`-separated).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    matches(pattern.as_bytes(), path.as_bytes())
}

fn matches(p: &[u8], t: &[u8]) -> bool {
    let Some((&c, rest)) = p.split_first() else {
        return t.is_empty();
    };
    match c {
        b'*' if rest.first() == Some(&b'*') => {
            let after = tail(rest, 1);
            if let Some(after_slash) = after.strip_prefix(b"/") {
                // `**/`: zero or more whole directories.
                if matches(after_slash, t) {
                    return true;
                }
                t.iter()
                    .enumerate()
                    .any(|(i, &b)| b == b'/' && matches(after_slash, tail(t, i + 1)))
            } else {
                // `**` anywhere else (`dir/**`, `a**`): anything, `/` included.
                (0..=t.len()).any(|i| matches(after, tail(t, i)))
            }
        }
        b'*' => {
            for i in 0..=t.len() {
                if matches(rest, tail(t, i)) {
                    return true;
                }
                if t.get(i) == Some(&b'/') {
                    break;
                }
            }
            false
        }
        b'?' => t.first().is_some_and(|&b| b != b'/') && matches(rest, tail(t, 1)),
        b'[' => match class(rest, t.first().copied()) {
            Some((true, after)) => matches(after, tail(t, 1)),
            Some((false, _)) => false,
            // No closing `]`: a literal `[`.
            None => t.first() == Some(&b'[') && matches(rest, tail(t, 1)),
        },
        _ => t.first() == Some(&c) && matches(rest, tail(t, 1)),
    }
}

/// Match one byte against a class body (`p` starts after `[`). Returns whether it matched and
/// the pattern after `]`, or `None` when the class is not closed.
fn class(p: &[u8], b: Option<u8>) -> Option<(bool, &[u8])> {
    let (negate, mut p) = match p.first() {
        Some(b'!' | b'^') => (true, tail(p, 1)),
        _ => (false, p),
    };
    let mut hit = false;
    let mut first = true;
    loop {
        let (&c, rest) = p.split_first()?;
        if c == b']' && !first {
            let ok = b.is_some_and(|b| b != b'/' && hit != negate);
            return Some((ok, rest));
        }
        first = false;
        if let (Some(b'-'), Some(&hi)) = (rest.first(), rest.get(1))
            && hi != b']'
        {
            if b.is_some_and(|b| (c..=hi).contains(&b)) {
                hit = true;
            }
            p = tail(rest, 2);
        } else {
            if b == Some(c) {
                hit = true;
            }
            p = rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn stars_and_double_stars() {
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "src/lib.rs"));
        assert!(glob_match("**/*.rs", "src/lib.rs"));
        assert!(glob_match("**/*.rs", "lib.rs"));
        assert!(glob_match("**/vendor/**", "vendor/x/y.go"));
        assert!(glob_match("**/vendor/**", "a/b/vendor/y.go"));
        assert!(!glob_match("**/vendor/**", "a/vendors/y.go"));
        assert!(glob_match("third_party/**", "third_party/a/b.c"));
        assert!(!glob_match("third_party/**", "src/third_party/a.c"));
        assert!(glob_match("**/*.min.js", "web/dist/app.min.js"));
        assert!(glob_match("src/**/test_*.py", "src/a/b/test_x.py"));
        assert!(glob_match("src/**/test_*.py", "src/test_x.py"));
    }

    #[test]
    fn question_marks_and_classes() {
        assert!(glob_match("file?.txt", "file1.txt"));
        assert!(!glob_match("file?.txt", "file/.txt"));
        assert!(glob_match("[abc].go", "b.go"));
        assert!(!glob_match("[!abc].go", "b.go"));
        assert!(glob_match("[a-c][0-9].go", "c7.go"));
        assert!(glob_match("[]].txt", "].txt"));
        assert!(glob_match("[.txt", "[.txt"));
        assert!(!glob_match("a", "ab"));
        assert!(glob_match("", ""));
    }
}
