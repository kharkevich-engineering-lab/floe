//! M1 acceptance (`docs/design/code-intelligence.md` §13): golden fixtures per language
//! (definitions, references, outline, chunks), the per-file deadline, minified and text
//! files, and determinism (same input ⇒ byte-identical records).
//!
//! Goldens live in `tests/golden/<fixture>.golden`. To re-bless after an intended change, run
//! with `FLOE_BLESS=1`; a mismatch also writes the actual dump to
//! `$CARGO_TARGET_TMPDIR/golden/` so CI can upload it.
#![cfg(feature = "extract")]
// Integration tests fail by panicking; clippy.toml's allow-*-in-tests only reaches #[test] fns,
// not the helpers around them, so the panic-path lints are lifted for the whole test crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "test code: a panic is how a test fails"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use floe_codeintel::chunk::Text;
use floe_codeintel::extract::{ExtractOptions, Extractor};
use floe_codeintel::version::{self, CHUNK_BUDGET_NWS};
use floe_codeintel::{Attributes, Def, DefKind, FileFacts, FileFlags, Lang, RefKind, def_flags};

/// (fixture file, repository path it is extracted as, language)
const FIXTURES: [(&str, &str, Lang); 8] = [
    ("router.rs", "src/router.rs", Lang::Rust),
    ("server.go", "server/server.go", Lang::Go),
    ("models.py", "app/models.py", Lang::Python),
    ("api.ts", "web/src/api.ts", Lang::TypeScript),
    ("App.tsx", "web/src/App.tsx", Lang::Tsx),
    ("util.js", "lib/util.js", Lang::JavaScript),
    (
        "Store.java",
        "src/main/java/com/acme/Store.java",
        Lang::Java,
    ),
    ("notes.md", "docs/notes.md", Lang::Text),
];

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn extractor() -> Extractor {
    Extractor::new(ExtractOptions::default()).expect("every pinned query compiles")
}

fn extract(file: &str) -> (String, FileFacts) {
    let (_, path, _) = FIXTURES.iter().find(|f| f.0 == file).unwrap();
    let src = fixture(file);
    let facts = extractor().extract(path, src.as_bytes(), &Attributes::default());
    (src, facts)
}

/// The one definition named `name` of `kind`.
fn def<'a>(f: &'a FileFacts, name: &str, kind: DefKind) -> &'a Def {
    let found: Vec<&Def> = f
        .defs
        .iter()
        .filter(|d| d.name == name && d.kind == kind)
        .collect();
    assert_eq!(found.len(), 1, "{name} ({kind:?}) in\n{}", f.render());
    found[0]
}

/// 1-based (line, column) of the word `name` on the first line containing `line_needle`.
fn at(src: &str, line_needle: &str, name: &str) -> (u32, u32) {
    let (i, line) = src
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains(line_needle))
        .unwrap_or_else(|| panic!("no line with {line_needle:?}"));
    let word = |b: Option<u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
    let col = line
        .match_indices(name)
        .map(|(c, _)| c)
        .find(|&c| {
            !word(c.checked_sub(1).map(|p| line.as_bytes()[p]))
                && !word(line.as_bytes().get(c + name.len()).copied())
        })
        .unwrap_or_else(|| panic!("{name:?} not a word on {line:?}"));
    (
        u32::try_from(i + 1).unwrap(),
        u32::try_from(col + 1).unwrap(),
    )
}

fn assert_def_at(src: &str, d: &Def, line_needle: &str) {
    let (line, col) = at(src, line_needle, &d.name);
    assert_eq!(
        (d.name_range.start_line, d.name_range.start_col),
        (line, col),
        "{}",
        d.name
    );
    assert_eq!(d.name_range.end_line, line);
    assert_eq!(
        d.name_range.end_col,
        col + u32::try_from(d.name.len()).unwrap()
    );
}

fn parent_name(f: &FileFacts, d: &Def) -> Option<String> {
    d.parent.map(|p| f.defs[p as usize].name.clone())
}

/// Whether there is a reference `name` of `kind` enclosed by the definition named `encl`.
fn has_ref(f: &FileFacts, name: &str, kind: RefKind, encl: Option<&str>) -> bool {
    f.refs.iter().any(|r| {
        r.name == name
            && r.kind == kind
            && r.enclosing.map(|e| f.defs[e as usize].name.as_str()) == encl
    })
}

/// Invariants every extraction must hold, whatever the language.
fn check_invariants(src: &str, f: &FileFacts) {
    assert_eq!(f.extractor, version::extractor(f.lang));
    assert_eq!(f.size, u64::try_from(src.len()).unwrap());
    // Outline order and a well-formed tree.
    for (i, d) in f.defs.iter().enumerate() {
        assert_eq!(d.ordinal as usize, i);
        if let Some(p) = d.parent {
            assert!(p < d.ordinal, "parent precedes child: {}", d.name);
            let p = &f.defs[p as usize];
            assert!(p.body_start_byte <= d.body_start_byte && d.body_end_byte <= p.body_end_byte);
        }
        assert!(d.body_start_byte <= d.body_end_byte && d.body_end_byte as usize <= src.len());
        // The signature is the line holding the name.
        let line = src
            .lines()
            .nth(d.name_range.start_line as usize - 1)
            .unwrap();
        let want: String = line.trim().chars().take(240).collect();
        assert_eq!(want, d.signature);
    }
    for w in f.defs.windows(2) {
        assert!(
            (w[0].name_range.start_line, w[0].name_range.start_col)
                < (w[1].name_range.start_line, w[1].name_range.start_col)
        );
    }
    for w in f.refs.windows(2) {
        assert!((w[0].line, w[0].col) < (w[1].line, w[1].col));
    }
    // Chunks: in order, disjoint, within the file and the budget (single lines excepted),
    // and every definition starts inside some chunk.
    let text = Text::new(src.as_bytes());
    for w in f.chunks.windows(2) {
        assert!(w[0].end_byte <= w[1].start_byte || f.chunks.iter().all(|c| c.kind == "window"));
    }
    for c in &f.chunks {
        assert!(c.start_byte < c.end_byte && c.end_byte as usize <= src.len());
        let nws = text.nws(c.start_byte as usize, c.end_byte as usize);
        assert!(
            nws <= CHUNK_BUDGET_NWS || c.start_line == c.end_line,
            "{c:?}"
        );
        assert!(
            c.header
                .starts_with(&format!("// path: {}  lang: {}", f.path, f.lang.name()))
        );
        if let Some(d) = c.def {
            assert_eq!(c.symbol, f.defs[d as usize].qualified(f.lang));
        }
    }
    for d in &f.defs {
        let s = d.body_start_byte;
        assert!(
            f.chunks.iter().any(|c| c.start_byte <= s && s < c.end_byte),
            "no chunk covers the start of {}",
            d.name
        );
    }
}

#[test]
fn golden_fixtures() {
    let bless = std::env::var_os("FLOE_BLESS").is_some();
    let golden_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let actual_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("golden");
    let mut ex = extractor();
    let mut failed = Vec::new();
    for (file, path, lang) in FIXTURES {
        let src = fixture(file);
        let facts = ex.extract(path, src.as_bytes(), &Attributes::default());
        assert_eq!(facts.lang, lang.effective(), "{file}");
        check_invariants(&src, &facts);
        let got = facts.render();
        let golden = golden_dir.join(format!("{file}.golden"));
        if bless {
            std::fs::create_dir_all(&golden_dir).unwrap();
            std::fs::write(&golden, &got).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&golden).unwrap_or_default();
        if want != got {
            std::fs::create_dir_all(&actual_dir).unwrap();
            std::fs::write(actual_dir.join(format!("{file}.golden")), &got).unwrap();
            failed.push(format!("{file}:\n{got}"));
        }
    }
    assert!(
        failed.is_empty(),
        "golden mismatch (actual dumps in {}; FLOE_BLESS=1 re-blesses):\n{}",
        actual_dir.display(),
        failed.join("\n")
    );
}

#[test]
fn rust_definitions_references_and_outline() {
    let (src, f) = extract("router.rs");
    assert_eq!(f.lang, Lang::Rust);
    let max = def(&f, "MAX_ROUTES", DefKind::Const);
    assert_def_at(&src, max, "pub const MAX_ROUTES");
    assert_eq!(max.flags & def_flags::EXPORTED, def_flags::EXPORTED);
    let router = def(&f, "Router", DefKind::Struct);
    assert_def_at(&src, router, "pub struct Router");
    assert_eq!(router.signature, "pub struct Router {");
    assert_eq!(router.container, "");
    def(&f, "Method", DefKind::Enum);
    let handler = def(&f, "Handler", DefKind::Trait);
    let handle = def(&f, "handle", DefKind::Method);
    assert_eq!(handle.container, "Handler");
    assert_eq!(parent_name(&f, handle).as_deref(), Some("Handler"));
    assert_eq!(handle.parent, Some(handler.ordinal));
    // Methods of an impl block carry the type as container (the impl is a scope, not a def).
    let new = def(&f, "new", DefKind::Method);
    assert_eq!((new.container.as_str(), new.parent), ("Router", None));
    assert_eq!(new.flags & def_flags::EXPORTED, def_flags::EXPORTED);
    assert_eq!(new.qualified(Lang::Rust), "Router::new");
    let route = def(&f, "route", DefKind::Method);
    assert_def_at(&src, route, "pub fn route(");
    assert_eq!(route.body_start_line, at(&src, "pub fn route(", "pub").0);
    assert_eq!(route.body_end_line, route.body_start_line + 3);
    def(&f, "route", DefKind::Macro);
    let util = def(&f, "util", DefKind::Module);
    let normalize = def(&f, "normalize", DefKind::Function);
    assert_eq!(
        (normalize.container.as_str(), normalize.parent),
        ("util", Some(util.ordinal))
    );
    let test = def(&f, "routes_nothing", DefKind::Function);
    assert_eq!(test.flags, def_flags::TEST, "#[test], not pub");

    assert!(has_ref(&f, "handle", RefKind::Call, Some("route")));
    assert!(has_ref(&f, "get", RefKind::Call, Some("route")));
    assert!(
        has_ref(&f, "new", RefKind::Call, Some("new")),
        "HashMap::new()"
    );
    assert!(
        has_ref(&f, "new", RefKind::Call, Some("routes_nothing")),
        "Router::new()"
    );
    assert!(has_ref(&f, "assert", RefKind::Call, Some("routes_nothing")));
    assert!(has_ref(&f, "Router", RefKind::Implementation, None));
    let r = f.refs.iter().find(|r| r.name == "handle").unwrap();
    let (line, col) = at(&src, "Some(h.handle(path))", "handle");
    assert_eq!((r.line, r.col, r.end_col), (line, col, col + 6));

    // The doc comment above the struct joins its chunk; the impl block is one chunk.
    let c = f
        .chunks
        .iter()
        .find(|c| c.def == Some(router.ordinal))
        .unwrap();
    assert!(
        src.get(c.start_byte as usize..)
            .unwrap()
            .starts_with("/// A route table.")
    );
    assert!(c.header.contains("in: Router  sig: pub struct Router {"));
    // A small impl block is one chunk, labelled with its type.
    let c = f.chunks.iter().find(|c| c.kind == "impl").unwrap();
    assert_eq!((c.symbol.as_str(), c.def), ("Router", None));
    assert!(
        src.get(c.start_byte as usize..)
            .unwrap()
            .starts_with("impl Router {")
    );
    assert!(c.header.ends_with("in: Router\n"));
    // `#[test]` is part of the test function's chunk.
    let c = f
        .chunks
        .iter()
        .find(|c| c.def == Some(test.ordinal))
        .unwrap();
    assert!(
        src.get(c.start_byte as usize..)
            .unwrap()
            .starts_with("#[test]")
    );
}

#[test]
fn go_definitions_docs_and_receivers() {
    let (src, f) = extract("server.go");
    let port = def(&f, "DefaultPort", DefKind::Const);
    assert_def_at(&src, port, "const DefaultPort");
    def(&f, "Server", DefKind::Struct);
    def(&f, "Handler", DefKind::Interface);
    def(&f, "Port", DefKind::Type);
    let new = def(&f, "NewServer", DefKind::Function);
    assert_eq!(new.doc, "NewServer builds a server.");
    assert_eq!(new.flags & def_flags::EXPORTED, def_flags::EXPORTED);
    let serve = def(&f, "Serve", DefKind::Method);
    assert_def_at(&src, serve, "func (s *Server) Serve");
    assert_eq!(serve.container, "Server", "the receiver type");
    assert_eq!(serve.doc, "Serve answers one path.");
    let helper = def(&f, "helper", DefKind::Function);
    assert_eq!(helper.flags & def_flags::EXPORTED, 0);
    assert!(has_ref(&f, "helper", RefKind::Call, Some("Serve")));
    assert!(has_ref(&f, "TrimSpace", RefKind::Call, Some("NewServer")));
    assert!(has_ref(&f, "ToUpper", RefKind::Call, Some("helper")));
    assert!(has_ref(&f, "Server", RefKind::Type, Some("NewServer")));
}

#[test]
fn python_classes_methods_and_nesting() {
    let (src, f) = extract("models.py");
    let version = def(&f, "VERSION", DefKind::Const);
    assert_def_at(&src, version, "VERSION =");
    let user = def(&f, "User", DefKind::Class);
    let init = def(&f, "__init__", DefKind::Method);
    assert_eq!(
        (init.container.as_str(), init.parent),
        ("User", Some(user.ordinal))
    );
    assert_eq!(init.flags & def_flags::EXPORTED, 0, "underscore");
    let to_json = def(&f, "to_json", DefKind::Method);
    assert_def_at(&src, to_json, "def to_json");
    let secret = def(&f, "_secret", DefKind::Method);
    let inner = def(&f, "inner", DefKind::Function);
    assert_eq!(inner.container, "User._secret");
    assert_eq!(inner.parent, Some(secret.ordinal));
    let load = def(&f, "load", DefKind::Function);
    assert_eq!(load.flags, def_flags::EXPORTED);
    let t = def(&f, "test_load", DefKind::Function);
    assert_eq!(t.flags, def_flags::EXPORTED | def_flags::TEST);
    assert!(has_ref(&f, "dumps", RefKind::Call, Some("to_json")));
    assert!(has_ref(&f, "User", RefKind::Call, Some("load")));
    assert!(has_ref(&f, "load", RefKind::Call, Some("test_load")));
    assert!(has_ref(&f, "inner", RefKind::Call, Some("_secret")));
}

#[test]
fn typescript_and_tsx() {
    let (src, f) = extract("api.ts");
    assert_eq!(f.lang, Lang::TypeScript);
    let repo = def(&f, "Repo", DefKind::Interface);
    assert_def_at(&src, repo, "export interface Repo");
    assert_eq!(repo.flags, def_flags::EXPORTED);
    def(&f, "RepoId", DefKind::Type);
    def(&f, "Visibility", DefKind::Enum);
    let get = def(&f, "getRepo", DefKind::Function);
    assert_eq!(get.flags, def_flags::EXPORTED);
    // The doc comment above an `export` declaration is the declaration's.
    assert_eq!(get.doc, "Fetches one repository.");
    let parse = def(&f, "parse", DefKind::Function);
    assert_eq!(parse.flags, 0);
    let class = def(&f, "RepoImpl", DefKind::Class);
    let full = def(&f, "fullName", DefKind::Method);
    assert_eq!(full.parent, Some(class.ordinal));
    assert_eq!(full.container, "RepoImpl");
    assert_eq!(full.doc, "The full name.");
    assert!(
        !f.defs.iter().any(|d| d.name == "constructor"),
        "constructors are not definitions (upstream #not-eq?)"
    );
    def(&f, "join", DefKind::Function);
    def(&f, "Legacy", DefKind::Module);
    let old = def(&f, "old", DefKind::Function);
    assert_eq!(old.container, "Legacy");
    assert_eq!(
        old.flags, 0,
        "exported from a namespace that is not exported"
    );
    assert!(has_ref(&f, "fetch", RefKind::Call, Some("getRepo")));
    assert!(has_ref(&f, "parse", RefKind::Call, Some("getRepo")));
    assert!(has_ref(&f, "RepoImpl", RefKind::Type, Some("parse")));
    assert!(has_ref(&f, "join", RefKind::Call, Some("fullName")));
    assert!(has_ref(&f, "RepoId", RefKind::Type, Some("getRepo")));

    let (_, f) = extract("App.tsx");
    assert_eq!(f.lang, Lang::Tsx);
    def(&f, "Props", DefKind::Type);
    let app = def(&f, "App", DefKind::Function);
    assert_eq!(app.flags, def_flags::EXPORTED);
    let header = def(&f, "Header", DefKind::Function);
    assert_eq!(header.flags, def_flags::EXPORTED);
    assert!(has_ref(&f, "useState", RefKind::Call, Some("App")));
}

#[test]
fn javascript_docs_classes_and_require() {
    let (src, f) = extract("util.js");
    let join = def(&f, "joinAll", DefKind::Function);
    assert_def_at(&src, join, "function joinAll");
    assert_eq!(join.doc, "Joins two segments.");
    let cache = def(&f, "Cache", DefKind::Class);
    let get = def(&f, "get", DefKind::Method);
    assert_eq!(
        (get.container.as_str(), get.parent),
        ("Cache", Some(cache.ordinal))
    );
    def(&f, "double", DefKind::Function);
    assert!(
        !f.refs.iter().any(|r| r.name == "require"),
        "upstream #not-match?"
    );
    assert!(has_ref(&f, "join", RefKind::Call, Some("joinAll")));
    assert!(
        has_ref(&f, "Map", RefKind::Type, None) || has_ref(&f, "Map", RefKind::Type, Some("Cache"))
    );
}

fn closes_sig(f: &FileFacts) -> &str {
    &f.defs.iter().find(|d| d.name == "close").unwrap().signature
}

#[test]
fn java_classes_methods_and_interfaces() {
    let (src, f) = extract("Store.java");
    let store = def(&f, "Store", DefKind::Class);
    assert_def_at(&src, store, "public class Store");
    assert_eq!(store.flags, def_flags::EXPORTED);
    let ctor = def(&f, "Store", DefKind::Method);
    assert_eq!(ctor.parent, Some(store.ordinal));
    let get = def(&f, "get", DefKind::Method);
    assert_eq!(
        (get.container.as_str(), get.flags),
        ("Store", def_flags::EXPORTED)
    );
    // The signature is the name's line, not the `@Override` above it.
    assert_eq!(closes_sig(&f), "public void close() {");
    let put = def(&f, "put", DefKind::Method);
    assert_eq!(put.flags, 0);
    let mode = def(&f, "Mode", DefKind::Enum);
    assert_eq!(mode.container, "Store");
    let closeable = def(&f, "Closeable", DefKind::Interface);
    let closes: Vec<&Def> = f.defs.iter().filter(|d| d.name == "close").collect();
    assert_eq!(closes.len(), 2);
    assert_eq!(closes[1].parent, Some(closeable.ordinal));
    assert!(has_ref(
        &f,
        "Closeable",
        RefKind::Implementation,
        Some("Store")
    ));
    assert!(has_ref(&f, "get", RefKind::Call, Some("get")));
    assert!(has_ref(&f, "clear", RefKind::Call, Some("close")));
}

#[test]
fn text_files_are_windowed_without_definitions() {
    let (_, f) = extract("notes.md");
    assert_eq!(f.lang, Lang::Text);
    assert_eq!(f.extractor, "ts-tags/1;cast/2;text");
    assert!(f.defs.is_empty() && f.refs.is_empty());
    assert_eq!(f.chunks.len(), 1);
    assert_eq!(
        (
            f.chunks[0].kind.as_str(),
            f.chunks[0].start_line,
            f.chunks[0].end_line
        ),
        ("window", 1, 3)
    );
    // Code without definitions is windowed too.
    let script = (0..120).fold(String::new(), |s, i| s + &format!("print({i})\n"));
    let f = extractor().extract("scripts/run.py", script.as_bytes(), &Attributes::default());
    assert_eq!(f.lang, Lang::Python);
    assert!(f.defs.is_empty());
    assert_eq!(
        f.chunks
            .iter()
            .map(|c| (c.start_line, c.end_line))
            .collect::<Vec<_>>(),
        [(1, 50), (41, 90), (81, 120)]
    );
    assert!(has_ref(&f, "print", RefKind::Call, None));
}

#[test]
fn the_deadline_flags_parse_timeout_and_keeps_the_file_grep_only() {
    // A deadline that has always passed: deterministic, whatever the machine.
    let mut ex = Extractor::new(ExtractOptions {
        timeout: Duration::ZERO,
        ..ExtractOptions::default()
    })
    .unwrap();
    let big = (0..2000).fold(String::new(), |s, i| {
        s + &format!("fn f{i}(x: u32) -> u32 {{ x + {i} }}\n")
    });
    let f = ex.extract("src/big.rs", big.as_bytes(), &Attributes::default());
    assert!(f.flags.contains(FileFlags::PARSE_TIMEOUT), "{:?}", f.flags);
    assert!(f.defs.is_empty() && f.refs.is_empty() && f.chunks.is_empty());
    assert_eq!(f.line_count, 2000);
    // The same extractor with a sane deadline parses it.
    let f = extractor().extract("src/big.rs", big.as_bytes(), &Attributes::default());
    assert!(!f.flags.contains(FileFlags::PARSE_TIMEOUT));
    assert_eq!(f.defs.len(), 2000);
}

#[test]
fn minified_vendored_generated_and_large_files_are_not_parsed() {
    let mut ex = extractor();
    let min = format!("function a(){{{}}}\n", "x=1;".repeat(200));
    let f = ex.extract("web/app.js", min.as_bytes(), &Attributes::default());
    assert!(f.flags.contains(FileFlags::MINIFIED));
    assert!(f.defs.is_empty() && f.chunks.is_empty());
    let f = ex.extract(
        "vendor/x/lib.go",
        b"package x\nfunc A() {}\n",
        &Attributes::default(),
    );
    assert!(f.flags.contains(FileFlags::VENDORED) && f.defs.is_empty());
    let attrs = Attributes::parse("gen/** linguist-generated\n");
    let f = ex.extract("gen/api.rs", b"pub fn a() {}\n", &attrs);
    assert!(f.flags.contains(FileFlags::GENERATED) && f.defs.is_empty());
    let mut small = Extractor::new(ExtractOptions {
        max_file_bytes: 8,
        ..ExtractOptions::default()
    })
    .unwrap();
    let f = small.extract("src/a.rs", b"pub fn a() {}\n", &Attributes::default());
    assert!(f.flags.contains(FileFlags::TOO_LARGE) && f.defs.is_empty());
    let f = ex.extract("img/logo.rs", b"\0\0\0", &Attributes::default());
    assert!(f.flags.contains(FileFlags::BINARY) && f.chunks.is_empty());
}

#[test]
fn extraction_is_deterministic() {
    let inputs: Vec<(String, String)> = FIXTURES
        .iter()
        .map(|(file, path, _)| (path.to_string(), fixture(file)))
        .collect();
    // One extractor, in order; another, in reverse order (parser and cursor reuse); a fresh
    // extractor per file. All must render identically, chunk hashes included.
    let mut a = extractor();
    let first: Vec<String> = inputs
        .iter()
        .map(|(p, s)| a.extract(p, s.as_bytes(), &Attributes::default()).render())
        .collect();
    let mut b = extractor();
    let mut second: Vec<String> = inputs
        .iter()
        .rev()
        .map(|(p, s)| b.extract(p, s.as_bytes(), &Attributes::default()).render())
        .collect();
    second.reverse();
    let third: Vec<String> = inputs
        .iter()
        .map(|(p, s)| {
            extractor()
                .extract(p, s.as_bytes(), &Attributes::default())
                .render()
        })
        .collect();
    assert_eq!(first, second);
    assert_eq!(first, third);
    // A different path changes only the chunk headers and hashes, never the facts.
    let (path, src) = &inputs[0];
    let x = a.extract(path, src.as_bytes(), &Attributes::default());
    let y = a.extract("other/router.rs", src.as_bytes(), &Attributes::default());
    assert_eq!(x.defs, y.defs);
    assert_eq!(x.refs, y.refs);
    assert_ne!(x.chunks[0].hash, y.chunks[0].hash);
    assert_eq!(
        x.chunks
            .iter()
            .map(|c| (c.start_byte, c.end_byte))
            .collect::<Vec<_>>(),
        y.chunks
            .iter()
            .map(|c| (c.start_byte, c.end_byte))
            .collect::<Vec<_>>()
    );
}

#[test]
fn shard_meta_extractors_cover_every_language_of_the_build() {
    let all = version::extractors();
    for (_, _, lang) in FIXTURES {
        assert_eq!(
            all.get(lang.effective().name()),
            Some(&version::extractor(lang))
        );
    }
    assert_eq!(all.len(), 8, "seven grammars and text: {all:?}");
}

fn extract_src(path: &str, src: &str) -> FileFacts {
    extractor().extract(path, src.as_bytes(), &Attributes::default())
}

#[test]
fn block_doc_comments_are_cleaned_on_every_line() {
    let f = extract_src(
        "lib/a.js",
        "/**\n * First line.\n * Second line.\n */\nfunction f() {}\n",
    );
    assert_eq!(
        def(&f, "f", DefKind::Function).doc,
        "First line.\nSecond line."
    );
}

#[test]
fn rust_test_attributes_are_parsed_not_grepped() {
    let src = "#[test]\n#[should_panic]\nfn a() {}\n\n#[cfg(not(test))]\nfn b() {}\n\n\
               #[tokio::test(flavor = \"multi_thread\")]\nasync fn c() {}\n\n/// doc\n#[test]\nfn d() {}\n\n\
               #[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n\n#[cfg(feature = \"test\")]\nfn e() {}\n";
    let f = extract_src("src/t.rs", src);
    let test = |name: &str, kind: DefKind| def(&f, name, kind).flags & def_flags::TEST != 0;
    assert!(
        test("a", DefKind::Function),
        "#[test] above #[should_panic]"
    );
    assert!(!test("b", DefKind::Function), "#[cfg(not(test))]");
    assert!(test("c", DefKind::Function), "#[tokio::test(...)]");
    assert!(test("d", DefKind::Function), "doc comment between");
    assert!(test("tests", DefKind::Module), "#[cfg(test)] mod");
    assert!(
        test("helper", DefKind::Function),
        "inside a #[cfg(test)] mod"
    );
    assert!(
        !test("e", DefKind::Function),
        "a feature named test is not cfg(test)"
    );
}

#[test]
fn only_direct_exports_are_exported() {
    let f = extract_src(
        "web/x.ts",
        "export function outer() {\n  function inner() {}\n  return inner;\n}\n\
         export namespace Pub {\n  export function g() {}\n}\n\
         namespace Priv {\n  export function h() {}\n}\n\
         export const k = () => 1;\n\
         export class C {\n  pub() {}\n  private hid() {}\n  #own() {}\n}\n\
         class D {\n  m() {}\n}\n",
    );
    let exported = |name: &str| def(&f, name, DefKind::Function).flags & def_flags::EXPORTED != 0;
    assert!(exported("outer"));
    assert!(!exported("inner"), "nested in an exported function");
    assert!(exported("g"), "exported from an exported namespace");
    assert!(
        !exported("h"),
        "exported from a namespace that is not exported"
    );
    assert!(exported("k"));
    let method = |name: &str| def(&f, name, DefKind::Method).flags & def_flags::EXPORTED != 0;
    assert!(method("pub"), "a public member of an exported class");
    assert!(!method("hid"), "a private member");
    assert!(!method("m"), "a member of a class that is not exported");
}
