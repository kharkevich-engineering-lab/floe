//! Code intelligence core (D52, `docs/design/code-intelligence.md` §2.1).
//!
//! A pure library: every function maps bytes to records, with no store, no async runtime and
//! no I/O, so the indexer, the CLI and the tests run the same code. This crate holds
//! milestone M1:
//!
//! * [`lang`] — path/shebang → [`Lang`], `.gitattributes` linguist rules, and the
//!   binary/vendored/generated/minified/too-large classification ([`FileFlags`]).
//! * [`extract`] (feature `extract`) — tree-sitter tags queries per language → definitions,
//!   references and the outline tree, under a per-file parse/query deadline.
//! * [`chunk`] — cAST-style chunking over the syntax tree (line windows without one) and the
//!   chunk hash `sha256(chunker ‖ header ‖ body)`.
//! * [`version`] — the per-language extractor strings (`ShardMeta.extractors`).
//!
//! Positions are 1-based lines and 1-based UTF-8 byte columns everywhere; end columns are
//! exclusive. Output is deterministic: the same bytes and path give identical records.

pub mod chunk;
#[cfg(feature = "extract")]
pub mod extract;
mod glob;
pub mod lang;
pub mod version;

use std::fmt::Write as _;

pub use glob::glob_match;
pub use lang::{Attributes, FileFlags, Lang};

/// How much an answer can be trusted (`precision` on every hit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Precision {
    /// Name-based, from tree-sitter tags queries: overloaded names may resolve wrongly.
    Syntactic,
    /// From compiler-grade data (SCIP, milestone M12).
    Precise,
}

impl Precision {
    pub fn as_str(self) -> &'static str {
        match self {
            Precision::Syntactic => "syntactic",
            Precision::Precise => "precise",
        }
    }
}

/// A source range: 1-based lines, 1-based UTF-8 byte columns, end column exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Range {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// Definition kinds (`code.symbols.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DefKind {
    Function,
    Method,
    Class,
    Struct,
    Interface,
    Trait,
    Enum,
    Module,
    Const,
    Type,
    Macro,
    Field,
}

impl DefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DefKind::Function => "function",
            DefKind::Method => "method",
            DefKind::Class => "class",
            DefKind::Struct => "struct",
            DefKind::Interface => "interface",
            DefKind::Trait => "trait",
            DefKind::Enum => "enum",
            DefKind::Module => "module",
            DefKind::Const => "const",
            DefKind::Type => "type",
            DefKind::Macro => "macro",
            DefKind::Field => "field",
        }
    }

    /// The `@definition.<suffix>` capture suffixes a tags query may use.
    pub fn from_capture(suffix: &str) -> Option<DefKind> {
        Some(match suffix {
            "function" => DefKind::Function,
            "method" => DefKind::Method,
            "class" => DefKind::Class,
            "struct" => DefKind::Struct,
            "interface" => DefKind::Interface,
            "trait" => DefKind::Trait,
            "enum" => DefKind::Enum,
            "module" => DefKind::Module,
            "constant" | "const" => DefKind::Const,
            "type" => DefKind::Type,
            "macro" => DefKind::Macro,
            "field" => DefKind::Field,
            _ => return None,
        })
    }

    /// Kinds that make a function nested directly inside them a method.
    pub fn is_type_like(self) -> bool {
        matches!(
            self,
            DefKind::Class | DefKind::Struct | DefKind::Interface | DefKind::Trait | DefKind::Enum
        )
    }
}

/// Reference kinds (`code.refs.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefKind {
    Call,
    Type,
    Implementation,
    Import,
    Field,
    Other,
}

impl RefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RefKind::Call => "call",
            RefKind::Type => "type",
            RefKind::Implementation => "implementation",
            RefKind::Import => "import",
            RefKind::Field => "field",
            RefKind::Other => "other",
        }
    }

    /// The `@reference.<suffix>` capture suffixes: `class` (a constructor call, `new X`)
    /// counts as a use of the type.
    pub fn from_capture(suffix: &str) -> RefKind {
        match suffix {
            "call" => RefKind::Call,
            "type" | "class" => RefKind::Type,
            "implementation" => RefKind::Implementation,
            "import" => RefKind::Import,
            "field" => RefKind::Field,
            _ => RefKind::Other,
        }
    }
}

/// `Def::flags` bits (`code.symbols.flags`).
pub mod def_flags {
    /// Visible outside its module/package (`pub`, capitalised, `export`, `public`, no `_`).
    pub const EXPORTED: u32 = 1;
    /// A test function.
    pub const TEST: u32 = 2;
    /// Marked deprecated (reserved; not detected yet).
    pub const DEPRECATED: u32 = 4;
}

/// A definition (`@definition.*`, precision syntactic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    /// Index within the file, in outline order (by name position).
    pub ordinal: u32,
    pub name: String,
    pub kind: DefKind,
    /// The name's range.
    pub name_range: Range,
    /// The whole definition node, in bytes (end exclusive) and lines.
    pub body_start_byte: u32,
    pub body_end_byte: u32,
    pub body_start_line: u32,
    pub body_end_line: u32,
    /// Enclosing definitions and scopes, outermost first, joined by the language's separator
    /// (`server::Router`, `pkg.Outer`). Empty at top level.
    pub container: String,
    /// The innermost enclosing definition (outline tree).
    pub parent: Option<u32>,
    /// The source line holding the name, trimmed, at most [`SIGNATURE_MAX_CHARS`] characters.
    pub signature: String,
    /// Doc comment (where the language's query captures one), at most [`DOC_MAX_BYTES`].
    pub doc: String,
    /// [`def_flags`] bits.
    pub flags: u32,
}

/// `Def::signature` bound (characters).
pub const SIGNATURE_MAX_CHARS: usize = 240;
/// `Def::doc` bound (bytes, cut at a character boundary).
pub const DOC_MAX_BYTES: usize = 1024;

impl Def {
    /// `container` + separator + `name` (`server::Router::route`).
    pub fn qualified(&self, lang: Lang) -> String {
        if self.container.is_empty() {
            self.name.clone()
        } else {
            format!("{}{}{}", self.container, lang.separator(), self.name)
        }
    }
}

/// A reference (`@reference.*`, name-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub line: u32,
    pub col: u32,
    /// Exclusive end column of the name (on `line`).
    pub end_col: u32,
    pub name: String,
    pub kind: RefKind,
    /// The innermost definition containing the reference (call-graph edges).
    pub enclosing: Option<u32>,
}

/// A semantic chunk (`code.chunks`); the embedding key is `hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start_byte: u32,
    /// Exclusive.
    pub end_byte: u32,
    pub start_line: u32,
    pub end_line: u32,
    /// Qualified name of the definition (or scope, such as a Rust `impl` block) the chunk
    /// belongs to; empty for top-level glue.
    pub symbol: String,
    /// The definition's kind (`impl` for a scope), `block` for merged top-level glue, `window`
    /// for line windows.
    pub kind: String,
    /// Ordinal of that definition (`None` for a scope, glue or a window).
    pub def: Option<u32>,
    /// Rough token count (non-whitespace characters / 3).
    pub tokens_est: u32,
    /// The header embedded with the body (`// path: …  lang: …  in: …  sig: …\n`); not part
    /// of the byte range.
    pub header: String,
    /// `sha256(chunker ‖ header ‖ body)`.
    pub hash: [u8; 32],
}

/// Everything extracted from one file at one extractor version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub path: String,
    /// The language the facts were extracted as (`text` when there is no grammar).
    pub lang: Lang,
    pub flags: FileFlags,
    pub size: u64,
    pub line_count: u32,
    /// The language's extractor string ([`version::extractor`]).
    pub extractor: String,
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    pub chunks: Vec<Chunk>,
}

impl FileFacts {
    /// A stable, human-readable dump of every record: the golden-test format, and a cheap
    /// determinism check (equal dumps ⇔ equal records).
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "file {} lang={} flags={} size={} lines={}",
            self.path,
            self.lang.name(),
            self.flags.names().join(","),
            self.size,
            self.line_count
        );
        let _ = writeln!(s, "extractor {}", self.extractor);
        for d in &self.defs {
            let r = d.name_range;
            let _ = writeln!(
                s,
                "def #{} {} {} @{}:{}-{}:{} body={}-{} parent={} in={:?} flags={} sig={:?} doc={:?}",
                d.ordinal,
                d.kind.as_str(),
                d.name,
                r.start_line,
                r.start_col,
                r.end_line,
                r.end_col,
                d.body_start_line,
                d.body_end_line,
                d.parent.map_or_else(|| "-".to_string(), |p| p.to_string()),
                d.container,
                d.flags,
                d.signature,
                d.doc
            );
        }
        for r in &self.refs {
            let _ = writeln!(
                s,
                "ref {} {} @{}:{}-{} in={}",
                r.kind.as_str(),
                r.name,
                r.line,
                r.col,
                r.end_col,
                r.enclosing
                    .map_or_else(|| "-".to_string(), |p| p.to_string())
            );
        }
        for c in &self.chunks {
            let _ = writeln!(
                s,
                "chunk lines={}-{} bytes={}-{} kind={} def={} symbol={:?} tokens={} hash={}",
                c.start_line,
                c.end_line,
                c.start_byte,
                c.end_byte,
                c.kind,
                c.def.map_or_else(|| "-".to_string(), |p| p.to_string()),
                c.symbol,
                c.tokens_est,
                hex::encode(c.hash)
            );
        }
        s
    }
}

/// Errors building an extractor (a pinned query that does not compile against its grammar,
/// or uses a capture this crate does not know). Extraction itself never fails: a file that
/// cannot be parsed in time is flagged `parse_timeout` and kept for grep/read.
#[derive(Debug)]
pub enum Error {
    Query { lang: Lang, message: String },
    Language { lang: Lang, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Query { lang, message } => {
                write!(f, "tags query for {}: {message}", lang.name())
            }
            Error::Language { lang, message } => {
                write!(f, "grammar for {}: {message}", lang.name())
            }
        }
    }
}

impl std::error::Error for Error {}
