//! Forge repository → floe [`RepoId`] (§B.5): `Acme/Widgets` → `acme/widgets`.
//! The floe repository has the forge's own `<owner>/<name>`, lowercased (forge
//! names are case-insensitive, so lowercase is lossless for identity); no prefix.
//! There is exactly one candidate: when that name is taken by a repository that
//! is not this mirror's, the forge repository is skipped as a `conflict`, never
//! renamed around it (the applier, §B.7.1). Only this module encodes the rule.
//! The mapping is computed once, at creation, and stored in the state; a rename
//! at the forge never changes it.

use floe_git::RepoId;

/// The floe owner for a forge owner: lowercased.
pub fn owner(forge_owner: &str) -> String {
    forge_owner.to_ascii_lowercase()
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

/// The floe repository for a forge repository, or `None` when floe cannot hold
/// that name (`RepoId` refuses it: a reserved or over-long owner, for example).
pub fn floe_id(forge_owner: &str, forge_name: &str) -> Option<RepoId> {
    RepoId::new(owner(forge_owner), base_name(forge_name)).ok()
}

/// `owner/name` → `RepoId` for a stored mapping (`acme/widgets`).
pub fn parse(floe: &str) -> Option<RepoId> {
    let (o, n) = floe.split_once('/')?;
    RepoId::new(o, n).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_names_one_to_one() {
        let cases: &[(&str, &str, &str)] = &[
            ("Acme", "Widgets", "acme/widgets"),
            ("acme", ".github", "acme/_.github"),
            ("a-b", "x.y_z", "a-b/x.y_z"),
        ];
        for (o, n, want) in cases {
            assert_eq!(
                floe_id(o, n).map(|id| id.to_string()).as_deref(),
                Some(*want),
                "{o}/{n}"
            );
        }
        assert_eq!(owner("Acme"), "acme");
        // The longest GitHub names (39-character owner, 100-character name) fit.
        floe_id(&"o".repeat(39), &"n".repeat(100)).unwrap();
        assert_eq!(parse("acme/widgets").unwrap().name(), "widgets");
        assert!(parse("nope").is_none());
    }
}
