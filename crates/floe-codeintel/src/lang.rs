//! Language detection and file classification (`docs/design/code-intelligence.md` §5.2 step 3,
//! §5.6): path/shebang → [`Lang`]; `.gitattributes` `linguist-*` overrides; binary, vendored,
//! generated, minified and too-large files are flagged ([`FileFlags`], the `code.blobs.flags`
//! bits) and are never parsed.

use crate::glob::glob_match;

/// Languages with a grammar (M1), plus `Text` for everything else (grep, read and line-window
/// chunks only). C/C++, C#, Ruby, Kotlin, PHP, Bash and Protobuf are later grammars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {
    Rust,
    Go,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Java,
    Text,
}

impl Lang {
    /// Every language, `Text` last.
    pub const ALL: [Lang; 8] = [
        Lang::Rust,
        Lang::Go,
        Lang::Python,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::JavaScript,
        Lang::Java,
        Lang::Text,
    ];

    /// The `lang` column value.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Java => "java",
            Lang::Text => "text",
        }
    }

    /// From a `lang` value or a linguist language name (case-insensitive).
    pub fn from_name(name: &str) -> Option<Lang> {
        Some(match name.to_ascii_lowercase().as_str() {
            "rust" => Lang::Rust,
            "go" | "golang" => Lang::Go,
            "python" => Lang::Python,
            "typescript" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "javascript" | "jsx" => Lang::JavaScript,
            "java" => Lang::Java,
            "text" => Lang::Text,
            _ => return None,
        })
    }

    /// Whether this build has the language's grammar (its `lang-*` feature).
    pub fn has_grammar(self) -> bool {
        match self {
            Lang::Rust => cfg!(feature = "lang-rust"),
            Lang::Go => cfg!(feature = "lang-go"),
            Lang::Python => cfg!(feature = "lang-python"),
            Lang::TypeScript | Lang::Tsx => cfg!(feature = "lang-typescript"),
            Lang::JavaScript => cfg!(feature = "lang-javascript"),
            Lang::Java => cfg!(feature = "lang-java"),
            Lang::Text => false,
        }
    }

    /// The language facts are extracted as in this build: itself, or `Text` without a grammar.
    #[must_use]
    pub fn effective(self) -> Lang {
        if self.has_grammar() { self } else { Lang::Text }
    }

    /// Separator of qualified names (`a::b` in Rust, `a.b` elsewhere).
    pub fn separator(self) -> &'static str {
        match self {
            Lang::Rust => "::",
            _ => ".",
        }
    }

    /// Detect from the path (extension, then well-known names) and, failing that, a shebang
    /// in `head` (the first bytes of the file).
    pub fn detect(path: &str, head: &[u8]) -> Lang {
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let by_ext = match ext.as_str() {
            "rs" => Some(Lang::Rust),
            "go" => Some(Lang::Go),
            "py" | "pyi" | "pyw" => Some(Lang::Python),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" => Some(Lang::Tsx),
            "js" | "mjs" | "cjs" | "jsx" => Some(Lang::JavaScript),
            "java" => Some(Lang::Java),
            _ => None,
        };
        if let Some(l) = by_ext {
            return l;
        }
        shebang(head).unwrap_or(Lang::Text)
    }
}

fn shebang(head: &[u8]) -> Option<Lang> {
    let line = head.strip_prefix(b"#!")?;
    let end = line.iter().position(|&b| b == b'\n').unwrap_or(line.len());
    let line = String::from_utf8_lossy(line.get(..end).unwrap_or_default());
    // `#!/usr/bin/env -S python3 -u` → the interpreter is the first word that is not env/a flag.
    let interp = line
        .split_whitespace()
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .find(|w| *w != "env" && !w.starts_with('-'))?;
    if interp.starts_with("python") {
        Some(Lang::Python)
    } else if matches!(interp, "node" | "nodejs" | "bun") {
        Some(Lang::JavaScript)
    } else if matches!(interp, "deno" | "ts-node" | "tsx") {
        Some(Lang::TypeScript)
    } else {
        None
    }
}

/// `code.blobs.flags` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FileFlags(pub u32);

impl FileFlags {
    pub const BINARY: u32 = 1;
    pub const GENERATED: u32 = 1 << 1;
    pub const VENDORED: u32 = 1 << 2;
    pub const TOO_LARGE: u32 = 1 << 3;
    pub const MINIFIED: u32 = 1 << 4;
    pub const PARSE_TIMEOUT: u32 = 1 << 5;

    const NAMES: [(u32, &'static str); 6] = [
        (Self::BINARY, "binary"),
        (Self::GENERATED, "generated"),
        (Self::VENDORED, "vendored"),
        (Self::TOO_LARGE, "too_large"),
        (Self::MINIFIED, "minified"),
        (Self::PARSE_TIMEOUT, "parse_timeout"),
    ];

    pub fn contains(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
    pub fn insert(&mut self, bit: u32) {
        self.0 |= bit;
    }
    /// Names of the set bits, in bit order.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(b, _)| self.contains(*b))
            .map(|(_, n)| *n)
            .collect()
    }
    /// Files never parsed or chunked: kept for grep/read (or skipped) only.
    pub fn blocks_parsing(self) -> bool {
        self.0
            & (Self::BINARY
                | Self::GENERATED
                | Self::VENDORED
                | Self::TOO_LARGE
                | Self::MINIFIED
                | Self::PARSE_TIMEOUT)
            != 0
    }
}

/// Binary detection window (a NUL byte in it ⇒ binary), as git does.
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Mean line length above which a file is minified (§5.6).
pub const MINIFIED_MEAN_LINE_BYTES: usize = 300;
/// How much of the head is searched for generated-file markers.
const GENERATED_SNIFF_BYTES: usize = 1024;

/// Directory names whose contents are vendored (a subset of linguist's vendor.yml).
const VENDOR_DIRS: [&str; 7] = [
    "vendor",
    "node_modules",
    "third_party",
    "third-party",
    "bower_components",
    "Godeps",
    ".yarn",
];

/// File-name globs of generated files (code generators, lock files, minified bundles).
const GENERATED_NAMES: [&str; 13] = [
    "*_pb2.py",
    "*_pb2_grpc.py",
    "*.pb.go",
    "*.pb.cc",
    "*.pb.h",
    "*.min.js",
    "*.min.css",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "go.sum",
    "*.generated.*",
];

/// Header markers of generated files (Go's convention, `@generated`, .NET, protoc).
const GENERATED_MARKERS: [&str; 3] = ["DO NOT EDIT", "@generated", "<auto-generated"];

fn builtin_vendored(path: &str) -> bool {
    let mut dirs = path.split('/');
    dirs.next_back();
    dirs.any(|d| VENDOR_DIRS.contains(&d))
}

fn builtin_generated(path: &str, bytes: &[u8]) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    if GENERATED_NAMES.iter().any(|g| glob_match(g, name)) {
        return true;
    }
    let head = bytes.get(..GENERATED_SNIFF_BYTES).unwrap_or(bytes);
    let head = String::from_utf8_lossy(head);
    GENERATED_MARKERS.iter().any(|m| head.contains(m))
}

/// Number of lines (a final line without `\n` counts).
pub fn line_count(bytes: &[u8]) -> u32 {
    let newlines = bytes
        .iter()
        .fold(0usize, |n, &b| n + usize::from(b == b'\n'));
    let partial = usize::from(bytes.last().is_some_and(|&b| b != b'\n'));
    u32::try_from(newlines + partial).unwrap_or(u32::MAX)
}

fn is_minified(bytes: &[u8]) -> bool {
    let lines = usize::try_from(line_count(bytes))
        .unwrap_or(usize::MAX)
        .max(1);
    bytes.len() / lines > MINIFIED_MEAN_LINE_BYTES
}

/// What one `.gitattributes` rule says about one attribute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Setting<T> {
    /// Not mentioned: earlier rules stand.
    #[default]
    Unmentioned,
    /// `!attr`: back to the built-in heuristics.
    Unset,
    Set(T),
}

impl<T: Copy> Setting<T> {
    fn apply(self, to: &mut Option<T>) {
        match self {
            Setting::Unmentioned => {}
            Setting::Unset => *to = None,
            Setting::Set(v) => *to = Some(v),
        }
    }
    fn mentioned(self) -> bool {
        !matches!(self, Setting::Unmentioned)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AttrRule {
    pattern: String,
    vendored: Setting<bool>,
    generated: Setting<bool>,
    language: Setting<Lang>,
}

/// The `linguist-*` attributes of a repository's root `.gitattributes` (nested
/// `.gitattributes` files are not consulted yet).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes {
    rules: Vec<AttrRule>,
}

/// What `.gitattributes` says about one path (`None` = nothing; use the heuristics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttrValues {
    pub vendored: Option<bool>,
    pub generated: Option<bool>,
    pub language: Option<Lang>,
}

fn bool_attr(token: &str, attr: &str) -> Setting<bool> {
    if let Some(rest) = token.strip_prefix(attr) {
        return match rest {
            "" | "=true" => Setting::Set(true),
            "=false" => Setting::Set(false),
            _ => Setting::Unmentioned,
        };
    }
    if token.strip_prefix('-') == Some(attr) {
        return Setting::Set(false);
    }
    if token.strip_prefix('!') == Some(attr) {
        return Setting::Unset;
    }
    Setting::Unmentioned
}

impl Attributes {
    /// Parse a `.gitattributes` document; lines without a `linguist-*` attribute are ignored.
    pub fn parse(text: &str) -> Attributes {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let Some(pattern) = words.next() else {
                continue;
            };
            let mut rule = AttrRule {
                pattern: pattern.to_string(),
                ..AttrRule::default()
            };
            for w in words {
                let vendored = bool_attr(w, "linguist-vendored");
                let generated = bool_attr(w, "linguist-generated");
                if vendored.mentioned() {
                    rule.vendored = vendored;
                } else if generated.mentioned() {
                    rule.generated = generated;
                } else if let Some(name) = w.strip_prefix("linguist-language=") {
                    rule.language = Lang::from_name(name).map_or(Setting::Unset, Setting::Set);
                } else if w == "!linguist-language" {
                    rule.language = Setting::Unset;
                }
            }
            if rule.vendored.mentioned() || rule.generated.mentioned() || rule.language.mentioned()
            {
                rules.push(rule);
            }
        }
        Attributes { rules }
    }

    /// The attributes of `path`; the last matching rule that mentions an attribute wins.
    pub fn lookup(&self, path: &str) -> AttrValues {
        let mut out = AttrValues::default();
        for r in &self.rules {
            if !rule_matches(&r.pattern, path) {
                continue;
            }
            r.vendored.apply(&mut out.vendored);
            r.generated.apply(&mut out.generated);
            r.language.apply(&mut out.language);
        }
        out
    }
}

/// gitattributes pattern semantics: without a `/` the pattern matches the file name at any
/// depth; with one it is anchored at the repository root.
fn rule_matches(pattern: &str, path: &str) -> bool {
    let anchored = pattern.trim_end_matches('/');
    if anchored.contains('/') {
        glob_match(anchored.trim_start_matches('/'), path)
    } else {
        let name = path.rsplit('/').next().unwrap_or(path);
        glob_match(anchored, name)
    }
}

/// The classification of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    /// The detected language (before the build's grammars are considered).
    pub lang: Lang,
    pub flags: FileFlags,
}

/// Classify a file: language and the flags that keep it from being parsed.
pub fn classify(path: &str, bytes: &[u8], attrs: &Attributes, max_file_bytes: u64) -> Classified {
    let a = attrs.lookup(path);
    let head = bytes.get(..BINARY_SNIFF_BYTES).unwrap_or(bytes);
    let lang = a.language.unwrap_or_else(|| Lang::detect(path, head));
    let mut flags = FileFlags::default();
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_file_bytes {
        flags.insert(FileFlags::TOO_LARGE);
    }
    if head.contains(&0) {
        flags.insert(FileFlags::BINARY);
    }
    if a.vendored.unwrap_or_else(|| builtin_vendored(path)) {
        flags.insert(FileFlags::VENDORED);
    }
    if a.generated
        .unwrap_or_else(|| builtin_generated(path, bytes))
    {
        flags.insert(FileFlags::GENERATED);
    }
    if !flags.contains(FileFlags::BINARY) && is_minified(bytes) {
        flags.insert(FileFlags::MINIFIED);
    }
    Classified { lang, flags }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_and_shebang() {
        assert_eq!(Lang::detect("src/main.rs", b""), Lang::Rust);
        assert_eq!(Lang::detect("cmd/x/main.go", b""), Lang::Go);
        assert_eq!(Lang::detect("a/b.PY", b""), Lang::Python);
        assert_eq!(Lang::detect("types.d.ts", b""), Lang::TypeScript);
        assert_eq!(Lang::detect("App.tsx", b""), Lang::Tsx);
        assert_eq!(Lang::detect("App.jsx", b""), Lang::JavaScript);
        assert_eq!(Lang::detect("x.mjs", b""), Lang::JavaScript);
        assert_eq!(Lang::detect("A.java", b""), Lang::Java);
        assert_eq!(Lang::detect("README.md", b""), Lang::Text);
        assert_eq!(Lang::detect("Makefile", b"all:\n"), Lang::Text);
        assert_eq!(
            Lang::detect("bin/tool", b"#!/usr/bin/env python3\nprint()\n"),
            Lang::Python
        );
        assert_eq!(
            Lang::detect("bin/tool", b"#!/usr/bin/env -S node --x\n"),
            Lang::JavaScript
        );
        assert_eq!(Lang::detect("bin/tool", b"#!/bin/sh\n"), Lang::Text);
        for l in Lang::ALL {
            assert_eq!(Lang::from_name(l.name()), Some(l));
        }
    }

    #[test]
    fn flags_binary_vendored_generated_minified_too_large() {
        let none = Attributes::default();
        let c = classify("src/lib.rs", b"fn main() {}\n", &none, 1024);
        assert_eq!((c.lang, c.flags), (Lang::Rust, FileFlags::default()));
        assert!(!c.flags.blocks_parsing());
        let c = classify("img.png", b"\x89PNG\0\0", &none, 1024);
        assert!(c.flags.contains(FileFlags::BINARY));
        let c = classify("vendor/github.com/x/y.go", b"package y\n", &none, 1024);
        assert!(c.flags.contains(FileFlags::VENDORED));
        let c = classify("a/node_modules/x/index.js", b"x\n", &none, 1024);
        assert!(c.flags.contains(FileFlags::VENDORED));
        assert!(
            !classify("vendor.go", b"package v\n", &none, 1024)
                .flags
                .contains(FileFlags::VENDORED)
        );
        let c = classify("api.pb.go", b"package api\n", &none, 1024);
        assert!(c.flags.contains(FileFlags::GENERATED));
        let gen_go = b"// Code generated by stringer. DO NOT EDIT.\n\npackage x\n";
        assert!(
            classify("x_string.go", gen_go, &none, 1024)
                .flags
                .contains(FileFlags::GENERATED)
        );
        let long = format!("var a={};\n", "1+".repeat(400));
        let c = classify("bundle.js", long.as_bytes(), &none, 1 << 20);
        assert!(c.flags.contains(FileFlags::MINIFIED), "{:?}", c.flags);
        let c = classify("big.rs", &[b'a'; 2048], &none, 1024);
        assert!(c.flags.contains(FileFlags::TOO_LARGE));
        assert!(c.flags.blocks_parsing());
        assert_eq!(
            FileFlags(0b11_0001).names(),
            ["binary", "minified", "parse_timeout"]
        );
    }

    #[test]
    fn gitattributes_linguist_overrides() {
        let attrs = Attributes::parse(
            "# comment\n\
             *.rs text eol=lf\n\
             vendor/** -linguist-vendored\n\
             gen/** linguist-generated=true\n\
             gen/keep.go !linguist-generated\n\
             *.tmpl linguist-language=Go\n\
             third/*.c linguist-vendored\n\
             docs/** linguist-generated linguist-vendored=false\n",
        );
        let none = 1 << 20;
        // `-linguist-vendored` beats the vendor/ heuristic.
        let c = classify("vendor/lib.go", b"package lib\n", &attrs, none);
        assert!(!c.flags.contains(FileFlags::VENDORED));
        let c = classify("gen/a.go", b"package gen\n", &attrs, none);
        assert!(c.flags.contains(FileFlags::GENERATED));
        // `!attr` returns to the heuristics (nothing generated about it).
        let c = classify("gen/keep.go", b"package gen\n", &attrs, none);
        assert!(!c.flags.contains(FileFlags::GENERATED));
        assert_eq!(classify("x/y/page.tmpl", b"", &attrs, none).lang, Lang::Go);
        assert!(
            classify("third/z.c", b"int x;\n", &attrs, none)
                .flags
                .contains(FileFlags::VENDORED)
        );
        assert!(
            !classify("src/third/z.c", b"int x;\n", &attrs, none)
                .flags
                .contains(FileFlags::VENDORED)
        );
        let v = attrs.lookup("docs/a.md");
        assert_eq!((v.generated, v.vendored), (Some(true), Some(false)));
        assert_eq!(attrs.lookup("src/lib.rs"), AttrValues::default());
    }

    #[test]
    fn counts_lines() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a"), 1);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
    }
}
