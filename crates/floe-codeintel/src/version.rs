//! Extractor and chunker version strings (`docs/design/code-intelligence.md` §4.1, §8.3).
//!
//! Blob-intrinsic facts are keyed by `(blob_sha, extractor)`, and a shard records one extractor
//! string per language (`ShardMeta.extractors`), so bumping one language re-extracts only that
//! language and a chunker bump re-chunks everything once. A language's string is
//!
//! ```text
//! ts-tags/1;cast/1;rust=0.24.2@<sha256 of the pinned tags query, 8 hex>
//! ```
//!
//! The grammar version and the query digest change by themselves when the grammar pin or the
//! query file changes. [`EXTRACTOR_FAMILY`] must be bumped by hand when the extraction code or
//! the tree-sitter runtime changes what is emitted; [`CHUNKER_ID`] when chunking does.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::Lang;

/// The extraction code's version (tree-sitter tags queries → records). Bump on any change
/// to what `extract` emits for an unchanged grammar and query, including a tree-sitter
/// runtime upgrade ([`TREE_SITTER_VERSION`]).
pub const EXTRACTOR_FAMILY: &str = "ts-tags/1";
/// The tree-sitter runtime the extraction code was written against (pinned in Cargo.toml).
pub const TREE_SITTER_VERSION: &str = "0.27.0";
/// The chunker's version (§5.7); part of every extractor string.
pub const CHUNKER_ID: &str = "cast/1";
/// The chunk budget, in non-whitespace characters (≈ 400–500 code tokens).
pub const CHUNK_BUDGET_NWS: usize = 1500;
/// The chunker string (`code.blobs.chunker`, `ShardMeta.chunker`); also the first input of
/// every chunk hash.
pub const CHUNKER: &str = "cast/1;budget=1500nws";

/// The pinned grammar version of a language (exact pins in Cargo.toml).
pub fn grammar_version(lang: Lang) -> Option<&'static str> {
    Some(match lang {
        Lang::Rust => "0.24.2",
        Lang::Go | Lang::Python | Lang::JavaScript => "0.25.0",
        Lang::TypeScript | Lang::Tsx => "0.23.2",
        Lang::Java => "0.23.5",
        Lang::Text => return None,
    })
}

/// The pinned tags query of a language (`queries/*.scm`). TypeScript and TSX use the
/// JavaScript query followed by the TypeScript one, as upstream configures them.
pub fn tags_query(lang: Lang) -> Option<&'static str> {
    Some(match lang {
        Lang::Rust => include_str!("../queries/rust.scm"),
        Lang::Go => include_str!("../queries/go.scm"),
        Lang::Python => include_str!("../queries/python.scm"),
        Lang::TypeScript | Lang::Tsx => concat!(
            include_str!("../queries/javascript.scm"),
            include_str!("../queries/typescript.scm")
        ),
        Lang::JavaScript => include_str!("../queries/javascript.scm"),
        Lang::Java => include_str!("../queries/java.scm"),
        Lang::Text => return None,
    })
}

/// The extractor string facts of `lang` are built with in this build. A language without a
/// grammar in this build is extracted as `text`.
pub fn extractor(lang: Lang) -> String {
    let lang = lang.effective();
    match (grammar_version(lang), tags_query(lang)) {
        (Some(v), Some(q)) => {
            let digest = Sha256::digest(q.as_bytes());
            let sha8 = hex::encode(digest.get(..4).unwrap_or_default());
            format!(
                "{EXTRACTOR_FAMILY};{CHUNKER_ID};{}={v}@{sha8}",
                lang.name()
            )
        }
        _ => format!("{EXTRACTOR_FAMILY};{CHUNKER_ID};{}", Lang::Text.name()),
    }
}

/// `ShardMeta.extractors`: language name → extractor string, for every language this build
/// extracts (plus `text`).
pub fn extractors() -> BTreeMap<&'static str, String> {
    Lang::ALL
        .iter()
        .filter(|l| l.has_grammar() || **l == Lang::Text)
        .map(|&l| (l.name(), extractor(l)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_string_names_the_budget() {
        assert_eq!(CHUNKER, format!("{CHUNKER_ID};budget={CHUNK_BUDGET_NWS}nws"));
    }

    /// The version constants must follow the exact pins in Cargo.toml: a grammar bump that
    /// left its string unchanged would serve facts of the old grammar as current.
    #[test]
    fn versions_follow_the_cargo_pins() {
        let manifest = include_str!("../Cargo.toml");
        let pin = |krate: &str| {
            let line = manifest
                .lines()
                .find(|l| l.starts_with(&format!("{krate} = ")))
                .unwrap_or_else(|| panic!("{krate} not in Cargo.toml"));
            let v = line.split("version = \"=").nth(1).unwrap();
            v.split('"').next().unwrap().to_string()
        };
        assert_eq!(pin("tree-sitter"), TREE_SITTER_VERSION, "bump EXTRACTOR_FAMILY too");
        for (krate, lang) in [
            ("tree-sitter-rust", Lang::Rust),
            ("tree-sitter-go", Lang::Go),
            ("tree-sitter-python", Lang::Python),
            ("tree-sitter-typescript", Lang::TypeScript),
            ("tree-sitter-typescript", Lang::Tsx),
            ("tree-sitter-javascript", Lang::JavaScript),
            ("tree-sitter-java", Lang::Java),
        ] {
            assert_eq!(Some(pin(krate).as_str()), grammar_version(lang), "{krate}");
        }
    }

    #[test]
    fn extractor_strings_have_the_designed_shape() {
        let text = extractor(Lang::Text);
        assert_eq!(text, "ts-tags/1;cast/1;text");
        let all = extractors();
        assert_eq!(all.get("text"), Some(&text));
        for l in Lang::ALL {
            let e = extractor(l);
            if l.has_grammar() {
                let prefix = format!("ts-tags/1;cast/1;{}=", l.name());
                assert!(e.starts_with(&prefix), "{e}");
                let sha8 = e.rsplit('@').next().unwrap();
                assert_eq!(sha8.len(), 8, "{e}");
                assert_eq!(all.get(l.name()), Some(&e));
            } else {
                assert_eq!(e, text);
            }
        }
        // TypeScript and TSX share a query but are separate languages.
        if Lang::TypeScript.has_grammar() {
            assert_ne!(extractor(Lang::TypeScript), extractor(Lang::Tsx));
            assert_ne!(extractor(Lang::TypeScript), extractor(Lang::JavaScript));
        }
    }
}
