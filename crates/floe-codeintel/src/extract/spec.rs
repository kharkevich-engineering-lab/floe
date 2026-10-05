//! A language's compiled grammar and tags query, with each capture's role.

use regex::Regex;
use tree_sitter::{Language, Parser, Query, QueryPredicateArg};

use crate::{DefKind, Error, Lang, RefKind, version};

/// What a capture means to the extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Name,
    Doc,
    Ignore,
    Receiver,
    Def(DefKind),
    Ref(RefKind),
    Scope,
    /// Captures used only by predicates.
    Other,
}

/// Per-pattern doc-comment handling (`#strip!`, `#select-adjacent!`).
#[derive(Debug, Default)]
pub(super) struct PatternInfo {
    pub doc_strip: Option<Regex>,
    pub doc_adjacent: Option<u32>,
}

pub(super) struct Spec {
    pub language: Language,
    pub query: Query,
    pub roles: Vec<Role>,
    pub patterns: Vec<PatternInfo>,
}

/// The grammar of a language in this build.
fn grammar(lang: Lang) -> Option<Language> {
    match lang {
        #[cfg(feature = "lang-rust")]
        Lang::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        #[cfg(feature = "lang-go")]
        Lang::Go => Some(tree_sitter_go::LANGUAGE.into()),
        #[cfg(feature = "lang-python")]
        Lang::Python => Some(tree_sitter_python::LANGUAGE.into()),
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        #[cfg(feature = "lang-typescript")]
        Lang::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        #[cfg(feature = "lang-java")]
        Lang::Java => Some(tree_sitter_java::LANGUAGE.into()),
        _ => None,
    }
}

fn role_of(lang: Lang, capture: &str) -> Result<Role, Error> {
    let unknown = || Error::Query {
        lang,
        message: format!("unknown capture @{capture}"),
    };
    Ok(match capture {
        "name" => Role::Name,
        "doc" => Role::Doc,
        "ignore" => Role::Ignore,
        "receiver" => Role::Receiver,
        c => {
            if let Some(kind) = c.strip_prefix("definition.") {
                Role::Def(DefKind::from_capture(kind).ok_or_else(unknown)?)
            } else if let Some(kind) = c.strip_prefix("reference.") {
                Role::Ref(RefKind::from_capture(kind))
            } else if c.starts_with("scope.") {
                Role::Scope
            } else if c.starts_with('_') {
                Role::Other
            } else {
                return Err(unknown());
            }
        }
    })
}

impl Spec {
    /// The spec of `lang`, or `None` when this build has no grammar for it.
    pub fn new(lang: Lang) -> Result<Option<Spec>, Error> {
        let (Some(language), Some(source)) = (grammar(lang), version::tags_query(lang)) else {
            return Ok(None);
        };
        Parser::new()
            .set_language(&language)
            .map_err(|e| Error::Language {
                lang,
                message: e.to_string(),
            })?;
        let query = Query::new(&language, source).map_err(|e| Error::Query {
            lang,
            message: e.to_string(),
        })?;
        let roles = query
            .capture_names()
            .iter()
            .map(|c| role_of(lang, c))
            .collect::<Result<Vec<_>, _>>()?;
        let doc_index = roles.iter().position(|r| *r == Role::Doc);
        let mut patterns = Vec::with_capacity(query.pattern_count());
        for i in 0..query.pattern_count() {
            let mut info = PatternInfo::default();
            for p in query.general_predicates(i) {
                let doc_first = matches!(
                    (p.args.first(), doc_index),
                    (Some(QueryPredicateArg::Capture(c)), Some(d)) if *c as usize == d
                );
                if !doc_first {
                    continue;
                }
                match (p.operator.as_ref(), p.args.get(1)) {
                    ("select-adjacent!", Some(QueryPredicateArg::Capture(c))) => {
                        info.doc_adjacent = Some(*c);
                    }
                    ("strip!", Some(QueryPredicateArg::String(re))) => {
                        // Multi-line: a block comment's every line is cleaned, not only the first.
                        info.doc_strip =
                            Some(Regex::new(&format!("(?m){re}")).map_err(|e| Error::Query {
                                lang,
                                message: format!("#strip! regex: {e}"),
                            })?);
                    }
                    _ => {}
                }
            }
            patterns.push(info);
        }
        Ok(Some(Spec {
            language,
            query,
            roles,
            patterns,
        }))
    }
}
