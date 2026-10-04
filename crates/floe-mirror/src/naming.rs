//! Forge repository → floe [`RepoId`] (§B.5): `Acme/Widgets` →
//! `<prefix>-acme/widgets`. floe identity is exactly two segments (D5, D26), so
//! the requested `<prefix>/<owner>/<repo>` becomes `<prefix>-<owner>/<repo>`.
//! Only this module encodes the rule. The mapping is computed once, at creation,
//! and stored in the state; a rename never changes it.

use floe_git::RepoId;

/// `RepoId` parts are 1..=100 characters (`floe_git::validate_part`).
const MAX_PART: usize = 100;

/// The floe owner for a forge owner: `<prefix>-<owner>`, lowercased (forge
/// names are case-insensitive, so lowercase is lossless for identity).
pub fn owner(prefix: &str, forge_owner: &str) -> String {
    format!("{prefix}-{}", forge_owner.to_ascii_lowercase())
}

/// The floe name for a forge name: lowercased; a leading `.` (`.github`), which
/// floe refuses, maps to `_.`.
pub fn base_name(forge_name: &str) -> String {
    let lower = forge_name.to_ascii_lowercase();
    match lower.strip_prefix('.') {
        Some(rest) => format!("_.{rest}"),
        None => lower,
    }
}

/// The candidate names for one repository, in order: the plain name, then the
/// collision form `<name>--<id>` (cut so that it fits in 100 characters; the id
/// keeps a cut name unique). A plain name longer than 100 has only the second.
pub fn candidates(prefix: &str, forge_owner: &str, forge_name: &str, id: &str) -> Vec<String> {
    let owner = owner(prefix, forge_owner);
    let base = base_name(forge_name);
    let mut out = Vec::with_capacity(2);
    if base.len() <= MAX_PART && RepoId::new(owner.clone(), base.clone()).is_ok() {
        out.push(base.clone());
    }
    let suffix = format!("--{id}");
    let keep = MAX_PART.saturating_sub(suffix.len());
    // Forge names are ASCII ([A-Za-z0-9._-]); `get` keeps a non-ASCII one from panicking.
    let cut = base.get(..keep.min(base.len())).unwrap_or(&base);
    let collision = format!("{cut}{suffix}");
    if RepoId::new(owner, collision.clone()).is_ok() && !out.contains(&collision) {
        out.push(collision);
    }
    out
}

/// `owner/name` → `RepoId` for a stored mapping (`gh-acme/widgets`).
pub fn parse(floe: &str) -> Option<RepoId> {
    let (o, n) = floe.split_once('/')?;
    RepoId::new(o, n).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_names_and_collisions() {
        let cases: &[(&str, &str, &str, &[&str])] = &[
            ("Acme", "Widgets", "1", &["widgets", "widgets--1"]),
            ("acme", ".github", "22", &["_.github", "_.github--22"]),
            ("a-b", "x.y_z", "3", &["x.y_z", "x.y_z--3"]),
        ];
        for (o, n, id, want) in cases {
            assert_eq!(candidates("gh", o, n, id), *want, "{o}/{n}");
            for name in *want {
                RepoId::new(owner("gh", o), *name).unwrap();
            }
        }
        assert_eq!(owner("gh", "Acme"), "gh-acme");
        // The longest owner is still a valid part (16 + 1 + 39).
        RepoId::new(owner("abcdefghijklmnop", &"o".repeat(39)), "x").unwrap();
    }

    #[test]
    fn hundred_character_names_fit() {
        let long = "n".repeat(100);
        let c = candidates("gh", "acme", &long, "123456789");
        assert_eq!(c.len(), 2);
        assert_eq!(c.first().map(String::len), Some(100));
        let collision = c.get(1).unwrap();
        assert_eq!(collision.len(), 100);
        assert!(collision.ends_with("--123456789"));
        for name in &c {
            RepoId::new("gh-acme", name.as_str()).unwrap();
        }
        assert_eq!(parse("gh-acme/widgets").unwrap().name(), "widgets");
        assert!(parse("nope").is_none());
    }
}
