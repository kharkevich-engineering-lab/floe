//! tree-sitter extraction (`docs/design/code-intelligence.md` §5.6): the pinned tags query of
//! each language ([`crate::version::tags_query`]) → definitions, references and the outline
//! tree, then chunks ([`crate::chunk`]).
//!
//! Captures understood in a tags query (the tree-sitter tags convention plus two of ours):
//! `@definition.<kind>`, `@reference.<kind>`, `@name`, `@doc` (with `#strip!` and
//! `#select-adjacent!`), `@ignore`, `@scope.<kind>` (a container that is not a definition,
//! such as a Rust `impl` block) and `@receiver` (a Go method's receiver type, its container).
//! One tag per name node: when several patterns capture the same name, the earliest pattern
//! wins.
//!
//! Every file gets a deadline ([`ExtractOptions::timeout`], 200 ms) covering parse and query;
//! a file that misses it is flagged `parse_timeout` and kept for grep/read only.

mod spec;

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, ParseOptions, Parser, QueryCursor, QueryCursorOptions, Tree};

use crate::chunk::{self, ChunkDef, Source, SyntaxNode, Text};
use crate::lang::{self, Attributes, Classified, FileFlags, Lang};
use crate::{
    DOC_MAX_BYTES, Def, DefKind, Error, FileFacts, Range, Ref, RefKind, SIGNATURE_MAX_CHARS,
    def_flags, version,
};

use spec::{Role, Spec};

/// Extraction limits.
#[derive(Debug, Clone, Copy)]
pub struct ExtractOptions {
    /// Larger files are flagged `too_large` and not parsed (`codeintel.max_file_bytes`).
    pub max_file_bytes: u64,
    /// Deadline for parsing and querying one file.
    pub timeout: Duration,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        ExtractOptions {
            max_file_bytes: 1024 * 1024,
            timeout: Duration::from_millis(200),
        }
    }
}

/// A reusable extractor: compiled queries for every language in this build and one parser.
/// Not `Sync` (the parser is stateful); use one per worker thread.
pub struct Extractor {
    opts: ExtractOptions,
    specs: BTreeMap<Lang, Spec>,
    parser: Parser,
    cursor: QueryCursor,
}

/// A tag before ordering: one per name node.
struct Candidate {
    pattern: usize,
    role: Role,
    name_start: usize,
    name_end: usize,
    name_pos: (usize, usize),
    name_end_pos: (usize, usize),
    node_start: usize,
    node_end: usize,
    node_start_row: usize,
    node_end_row: usize,
    name: String,
    doc: String,
    receiver: Option<String>,
    exported: bool,
    test: bool,
    signature: String,
}

/// A container that is not a definition (`@scope.*`).
struct Scope {
    start: usize,
    end: usize,
    name: String,
    kind: String,
}

fn text_of(node: Node<'_>, src: &[u8]) -> String {
    String::from_utf8_lossy(src.get(node.byte_range()).unwrap_or_default()).into_owned()
}

fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default().to_string()
}

/// What `#strip!` leaves of a block comment's closing `*/`, and surrounding whitespace.
fn clean_doc(doc: &str) -> &str {
    doc.trim().trim_end_matches("*/").trim_end()
}

/// The source line holding the name (not the node's first line, which may be an annotation
/// such as Java's `@Override`), trimmed.
fn signature_of(name: Node<'_>, src: &[u8]) -> String {
    let at = name.start_byte();
    let head = src.get(..at).unwrap_or_default();
    let start = head.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let rest = src.get(start..).unwrap_or_default();
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    let line = String::from_utf8_lossy(rest.get(..end).unwrap_or_default());
    line.trim().chars().take(SIGNATURE_MAX_CHARS).collect()
}

/// Per-language "visible outside" heuristics (`def_flags::EXPORTED`).
fn is_exported(lang: Lang, node: Node<'_>, name: &str, src: &[u8]) -> bool {
    let child_kind_text = |kind: &str| {
        let mut c = node.walk();
        node.children(&mut c)
            .find(|k| k.kind() == kind)
            .map(|k| text_of(k, src))
    };
    match lang {
        Lang::Rust => child_kind_text("visibility_modifier").is_some_and(|v| v.starts_with("pub")),
        Lang::Go => name.chars().next().is_some_and(char::is_uppercase),
        Lang::Python => !name.starts_with('_'),
        Lang::Java => child_kind_text("modifiers")
            .is_some_and(|m| m.split_whitespace().any(|w| w == "public")),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            if matches!(
                node.kind(),
                "method_definition" | "method_signature" | "abstract_method_signature"
            ) {
                js_member_exported(node, name, src)
            } else {
                js_exported(node)
            }
        }
        Lang::Text => false,
    }
}

/// The `export_statement` a JS/TS declaration is exported by: its direct parent, or (for a
/// `const f = () => …` declarator) its declaration's parent. Nothing further up counts: a
/// function nested in an exported one is not exported.
fn export_statement_of(node: Node<'_>) -> Option<Node<'_>> {
    let parent = node.parent()?;
    if parent.kind() == "export_statement" {
        return Some(parent);
    }
    if matches!(
        parent.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        return parent.parent().filter(|g| g.kind() == "export_statement");
    }
    None
}

/// Exported, and every enclosing namespace exported too (an `export` inside a namespace that
/// is not itself exported is visible to that namespace only).
fn js_exported(node: Node<'_>) -> bool {
    let Some(export) = export_statement_of(node) else {
        return false;
    };
    let mut cur = export.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "internal_module" | "module") {
            // `export namespace X {}` parses as an export_statement around the module, or
            // around an expression_statement holding it.
            let wrapper = n
                .parent()
                .filter(|p| p.kind() == "expression_statement")
                .unwrap_or(n);
            if export_statement_of(wrapper).is_none()
                && wrapper
                    .parent()
                    .is_none_or(|p| p.kind() != "export_statement")
            {
                return false;
            }
        }
        cur = n.parent();
    }
    true
}

/// A class or interface member is visible outside the module when its type is exported and the
/// member is not `private`/`protected`/`#private`.
fn js_member_exported(node: Node<'_>, name: &str, src: &[u8]) -> bool {
    if name.starts_with('#') {
        return false;
    }
    let mut c = node.walk();
    let hidden = node.children(&mut c).any(|k| {
        k.kind() == "accessibility_modifier"
            && matches!(text_of(k, src).as_str(), "private" | "protected")
    });
    if hidden {
        return false;
    }
    // member → class_body / interface_body (object_type) → the type's declaration
    node.parent()
        .and_then(|body| body.parent())
        .is_some_and(js_exported)
}

/// A Rust attribute's path and arguments, whitespace removed: `#[tokio::test]` →
/// `("tokio::test", "")`, `#[cfg(not(test))]` → `("cfg", "(not(test))")`.
fn rust_attribute(text: &str) -> Option<(String, String)> {
    let inner = text.trim().strip_prefix("#[")?.strip_suffix(']')?;
    let compact: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
    let split = compact.find(['(', '=']).unwrap_or(compact.len());
    let (path, args) = compact.split_at(split);
    Some((path.to_string(), args.to_string()))
}

/// The attributes directly above a Rust item (doc comments in between are skipped).
fn rust_attributes(node: Node<'_>, src: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(p) = cur {
        match p.kind() {
            "attribute_item" => {
                if let Some(a) = rust_attribute(&text_of(p, src)) {
                    out.push(a);
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        cur = p.prev_sibling();
    }
    out
}

/// `#[test]`, `#[<runtime>::test]` (`tokio`, `async_std`, …) or `#[cfg(test)]`.
fn rust_is_test(node: Node<'_>, src: &[u8]) -> bool {
    rust_attributes(node, src).iter().any(|(path, args)| {
        path == "test" || path.ends_with("::test") || (path == "cfg" && args == "(test)")
    })
}

/// Per-language test-function heuristics (`def_flags::TEST`).
fn is_test(lang: Lang, path: &str, node: Node<'_>, name: &str, src: &[u8]) -> bool {
    match lang {
        Lang::Rust => rust_is_test(node, src),
        Lang::Go => {
            path.ends_with("_test.go")
                && ["Test", "Benchmark", "Example", "Fuzz"]
                    .iter()
                    .any(|p| name.starts_with(p))
        }
        Lang::Python => name.starts_with("test_") || name == "test",
        Lang::Java => {
            let mut c = node.walk();
            node.children(&mut c)
                .any(|k| k.kind() == "modifiers" && text_of(k, src).contains("@Test"))
        }
        _ => false,
    }
}

/// tree-sitter node wrapper for the chunker.
struct TsNode<'t>(Node<'t>);

impl SyntaxNode for TsNode<'_> {
    fn byte_range(&self) -> (usize, usize) {
        (self.0.start_byte(), self.0.end_byte())
    }
    fn attaches_forward(&self) -> bool {
        let k = self.0.kind();
        k.contains("comment") || k == "attribute_item" || k == "decorator"
    }
    fn children(&self) -> Vec<Self> {
        let mut c = self.0.walk();
        self.0.children(&mut c).map(TsNode).collect()
    }
}

impl Extractor {
    /// Compile every language of this build. Fails only if a pinned query does not compile
    /// against its grammar (a build defect the tests catch).
    pub fn new(opts: ExtractOptions) -> Result<Extractor, Error> {
        let mut specs = BTreeMap::new();
        for lang in Lang::ALL {
            if let Some(s) = Spec::new(lang)? {
                specs.insert(lang, s);
            }
        }
        Ok(Extractor {
            opts,
            specs,
            parser: Parser::new(),
            cursor: QueryCursor::new(),
        })
    }

    pub fn options(&self) -> ExtractOptions {
        self.opts
    }

    /// Extract one file. Never fails: unparseable input is flagged, not an error.
    pub fn extract(&mut self, path: &str, bytes: &[u8], attrs: &Attributes) -> FileFacts {
        let Classified { lang, mut flags } =
            lang::classify(path, bytes, attrs, self.opts.max_file_bytes);
        let lang = if self.specs.contains_key(&lang) {
            lang
        } else {
            Lang::Text
        };
        let mut facts = FileFacts {
            path: path.to_string(),
            lang,
            flags,
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            line_count: lang::line_count(bytes),
            extractor: version::extractor(lang),
            defs: Vec::new(),
            refs: Vec::new(),
            chunks: Vec::new(),
        };
        if flags.blocks_parsing() {
            return facts;
        }
        let text = Text::new(bytes);
        let src = Source { path, lang };
        let Some(spec) = self.specs.get(&lang) else {
            facts.chunks = chunk::chunk_windows(src, &text);
            return facts;
        };
        let deadline = Instant::now() + self.opts.timeout;
        let Some(tree) = parse(&mut self.parser, spec, bytes, deadline) else {
            flags.insert(FileFlags::PARSE_TIMEOUT);
            facts.flags = flags;
            return facts;
        };
        let Some((cands, scopes)) =
            run_query(&mut self.cursor, spec, lang, path, &tree, bytes, deadline)
        else {
            flags.insert(FileFlags::PARSE_TIMEOUT);
            facts.flags = flags;
            return facts;
        };
        let (defs, refs) = build_records(lang, cands, &scopes);
        let chunk_defs = chunk_defs(lang, &defs, &scopes);
        facts.chunks = if defs.is_empty() {
            chunk::chunk_windows(src, &text)
        } else {
            chunk::chunk_syntax(src, &text, &TsNode(tree.root_node()), &chunk_defs)
        };
        facts.defs = defs;
        facts.refs = refs;
        facts
    }
}

fn parse(parser: &mut Parser, spec: &Spec, bytes: &[u8], deadline: Instant) -> Option<Tree> {
    parser.set_language(&spec.language).ok()?;
    parser.reset();
    let mut progress = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    parser.parse_with_options(
        &mut |i, _| bytes.get(i..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )
}

/// Run the tags query; `None` when the deadline passed while matching.
fn run_query(
    cursor: &mut QueryCursor,
    spec: &Spec,
    lang: Lang,
    path: &str,
    tree: &Tree,
    src: &[u8],
    deadline: Instant,
) -> Option<(Vec<Candidate>, Vec<Scope>)> {
    let mut timed_out = false;
    let mut progress = |_: &tree_sitter::QueryCursorState| {
        if Instant::now() >= deadline {
            timed_out = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    // name range → candidate (one tag per name node; the earliest pattern wins)
    let mut by_name: BTreeMap<(usize, usize), Candidate> = BTreeMap::new();
    let mut scopes = Vec::new();
    {
        let mut matches = cursor.matches_with_options(
            &spec.query,
            tree.root_node(),
            src,
            QueryCursorOptions::new().progress_callback(&mut progress),
        );
        while let Some(m) = matches.next() {
            let mut name_node = None;
            let mut tag: Option<(Node<'_>, Role)> = None;
            let mut docs: Vec<Node<'_>> = Vec::new();
            let mut receiver = None;
            let mut adjacent = None;
            let info = spec.patterns.get(m.pattern_index);
            for cap in m.captures() {
                let role = spec
                    .roles
                    .get(cap.index as usize)
                    .copied()
                    .unwrap_or(Role::Other);
                if info.and_then(|i| i.doc_adjacent) == Some(cap.index) {
                    adjacent = Some(cap.node);
                }
                match role {
                    Role::Name => name_node = Some(cap.node),
                    Role::Doc => docs.push(cap.node),
                    Role::Receiver => receiver = Some(cap.node),
                    Role::Ignore => {
                        name_node = Some(cap.node);
                        tag = Some((cap.node, Role::Ignore));
                    }
                    Role::Def(_) | Role::Ref(_) | Role::Scope => tag = Some((cap.node, role)),
                    Role::Other => {}
                }
            }
            let (Some(name), Some((node, role))) = (name_node, tag) else {
                continue;
            };
            if role == Role::Scope {
                let raw = text_of(name, src);
                let base = raw.split('<').next().unwrap_or_default().trim().to_string();
                let kind = m
                    .captures()
                    .iter()
                    .filter_map(|c| spec.query.capture_names().get(c.index as usize))
                    .find_map(|n| n.strip_prefix("scope."))
                    .unwrap_or("scope")
                    .to_string();
                scopes.push(Scope {
                    start: node.start_byte(),
                    end: node.end_byte(),
                    name: base,
                    kind,
                });
                continue;
            }
            if name.has_error() {
                continue;
            }
            let key = (name.start_byte(), name.end_byte());
            if by_name
                .get(&key)
                .is_some_and(|c| c.pattern <= m.pattern_index)
            {
                continue;
            }
            // Doc comments: those adjacent to the definition (no blank line), stripped.
            let mut first_doc = 0;
            if let Some(adj) = adjacent
                && !docs.is_empty()
            {
                first_doc = docs.len();
                let mut row = adj.start_position().row;
                while first_doc > 0 {
                    let Some(d) = docs.get(first_doc - 1) else {
                        break;
                    };
                    if d.end_position().row + 1 >= row {
                        first_doc -= 1;
                        row = d.start_position().row;
                    } else {
                        break;
                    }
                }
            }
            let strip = info.and_then(|i| i.doc_strip.as_ref());
            let doc = docs
                .get(first_doc..)
                .unwrap_or_default()
                .iter()
                .map(|d| {
                    let t = text_of(*d, src);
                    match strip {
                        Some(re) => re.replace_all(&t, "").into_owned(),
                        None => t,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let name_text = text_of(name, src);
            let is_def = matches!(role, Role::Def(_));
            let cand = Candidate {
                pattern: m.pattern_index,
                role,
                name_start: name.start_byte(),
                name_end: name.end_byte(),
                name_pos: (name.start_position().row, name.start_position().column),
                name_end_pos: (name.end_position().row, name.end_position().column),
                node_start: node.start_byte().min(name.start_byte()),
                node_end: node.end_byte().max(name.end_byte()),
                node_start_row: node.start_position().row,
                node_end_row: node.end_position().row,
                exported: is_def && is_exported(lang, node, &name_text, src),
                test: is_def && is_test(lang, path, node, &name_text, src),
                signature: if is_def {
                    signature_of(name, src)
                } else {
                    String::new()
                },
                doc: truncate_bytes(clean_doc(&doc), DOC_MAX_BYTES),
                receiver: receiver.map(|r| text_of(r, src)),
                name: name_text,
            };
            by_name.insert(key, cand);
        }
    }
    if timed_out {
        return None;
    }
    Some((by_name.into_values().collect(), scopes))
}

/// An interval in the containment sweep: a definition (by index) or a scope.
#[derive(Clone, Copy)]
enum Item {
    Def(usize),
    Scope(usize),
}

/// Turn candidates (sorted by name position) into ordered definitions and references with
/// containers, parents, method kinds and enclosing definitions.
fn build_records(lang: Lang, cands: Vec<Candidate>, scopes: &[Scope]) -> (Vec<Def>, Vec<Ref>) {
    let mut defs: Vec<(Candidate, DefKind)> = Vec::new();
    let mut refs: Vec<(Candidate, RefKind)> = Vec::new();
    for c in cands {
        match c.role {
            Role::Def(k) => defs.push((c, k)),
            Role::Ref(k) => refs.push((c, k)),
            _ => {}
        }
    }

    // Containment sweep over definitions and scopes, outermost first.
    let mut items: Vec<(usize, usize, Item)> = defs
        .iter()
        .enumerate()
        .map(|(i, (c, _))| (c.node_start, c.node_end, Item::Def(i)))
        .chain(
            scopes
                .iter()
                .enumerate()
                .map(|(i, s)| (s.start, s.end, Item::Scope(i))),
        )
        .collect();
    items.sort_by_key(|&(s, e, it)| {
        (
            s,
            std::cmp::Reverse(e),
            match it {
                Item::Scope(i) => (0, i),
                Item::Def(i) => (1, i),
            },
        )
    });
    let mut parent: Vec<Option<usize>> = vec![None; defs.len()];
    let mut chain: Vec<Vec<Item>> = vec![Vec::new(); defs.len()];
    let mut stack: Vec<(usize, usize, Item)> = Vec::new();
    for &(s, e, it) in &items {
        while stack
            .last()
            .is_some_and(|&(ps, pe, _)| !(ps <= s && e <= pe))
        {
            stack.pop();
        }
        if let Item::Def(i) = it {
            if let Some(slot) = chain.get_mut(i) {
                *slot = stack.iter().map(|&(_, _, x)| x).collect();
            }
            if let Some(slot) = parent.get_mut(i) {
                *slot = stack.iter().rev().find_map(|&(_, _, x)| match x {
                    Item::Def(p) => Some(p),
                    Item::Scope(_) => None,
                });
            }
        }
        stack.push((s, e, it));
    }

    let name_of = |it: &Item| -> String {
        match *it {
            Item::Def(i) => defs.get(i).map(|(c, _)| c.name.clone()).unwrap_or_default(),
            Item::Scope(i) => scopes.get(i).map(|s| s.name.clone()).unwrap_or_default(),
        }
    };
    let out_defs: Vec<Def> = defs
        .iter()
        .enumerate()
        .map(|(i, (c, kind))| {
            let ch = chain.get(i).map(Vec::as_slice).unwrap_or_default();
            let mut container: Vec<String> = ch.iter().map(name_of).collect();
            if let Some(r) = &c.receiver {
                container.push(r.clone());
            }
            // A function directly inside a type (or an impl block) is a method.
            let enclosing_is_type = match ch.last() {
                Some(Item::Def(p)) => defs.get(*p).is_some_and(|(_, k)| k.is_type_like()),
                Some(Item::Scope(_)) => true,
                None => false,
            };
            // …and a "method" whose enclosing definition is a module is a function (the Rust
            // query's `declaration_list` pattern also matches `mod` bodies).
            let enclosing_is_module = matches!(
                ch.last(),
                Some(Item::Def(p)) if defs.get(*p).is_some_and(|(_, k)| *k == DefKind::Module)
            );
            let kind = match *kind {
                DefKind::Function if enclosing_is_type => DefKind::Method,
                DefKind::Method if enclosing_is_module => DefKind::Function,
                k => k,
            };
            let mut flags = 0;
            if c.exported {
                flags |= def_flags::EXPORTED;
            }
            // Inside a test module (`#[cfg(test)] mod tests`) everything is test code.
            let in_test_module = ch.iter().any(|it| {
                matches!(it, Item::Def(p)
                    if defs.get(*p).is_some_and(|(a, k)| *k == DefKind::Module && a.test))
            });
            if c.test || in_test_module {
                flags |= def_flags::TEST;
            }
            Def {
                ordinal: to_u32(i),
                name: c.name.clone(),
                kind,
                name_range: Range {
                    start_line: to_u32(c.name_pos.0 + 1),
                    start_col: to_u32(c.name_pos.1 + 1),
                    end_line: to_u32(c.name_end_pos.0 + 1),
                    end_col: to_u32(c.name_end_pos.1 + 1),
                },
                body_start_byte: to_u32(c.node_start),
                body_end_byte: to_u32(c.node_end),
                body_start_line: to_u32(c.node_start_row + 1),
                body_end_line: to_u32(c.node_end_row + 1),
                container: container.join(lang.separator()),
                parent: parent.get(i).copied().flatten().map(to_u32),
                signature: c.signature.clone(),
                doc: c.doc.clone(),
                flags,
            }
        })
        .collect();

    // Enclosing definition of each reference: the innermost definition containing its name.
    let out_refs = refs
        .iter()
        .map(|(c, kind)| {
            let enclosing = out_defs
                .iter()
                .filter(|d| {
                    d.body_start_byte as usize <= c.name_start
                        && c.name_end <= d.body_end_byte as usize
                })
                .max_by_key(|d| (d.body_start_byte, std::cmp::Reverse(d.body_end_byte)))
                .map(|d| d.ordinal);
            Ref {
                line: to_u32(c.name_pos.0 + 1),
                col: to_u32(c.name_pos.1 + 1),
                end_col: to_u32(c.name_end_pos.1 + 1),
                name: c.name.clone(),
                kind: *kind,
                enclosing,
            }
        })
        .collect();
    (out_defs, out_refs)
}

/// Definitions and scopes as the chunker wants them: sorted by `(start, Reverse(end))`.
fn chunk_defs(lang: Lang, defs: &[Def], scopes: &[Scope]) -> Vec<ChunkDef> {
    let mut out: Vec<ChunkDef> = defs
        .iter()
        .map(|d| ChunkDef {
            start: d.body_start_byte as usize,
            end: d.body_end_byte as usize,
            ordinal: Some(d.ordinal),
            kind: d.kind.as_str().to_string(),
            qualified: d.qualified(lang),
            signature: d.signature.clone(),
        })
        .chain(scopes.iter().map(|s| ChunkDef {
            start: s.start,
            end: s.end,
            ordinal: None,
            kind: s.kind.clone(),
            qualified: s.name.clone(),
            signature: String::new(),
        }))
        .collect();
    out.sort_by_key(|d| (d.start, std::cmp::Reverse(d.end), d.ordinal));
    out
}
