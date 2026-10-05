//! Chunking (`docs/design/code-intelligence.md` §5.7): cAST-style split-then-merge over the
//! syntax tree, line windows without one, and the chunk hash.
//!
//! 1. The file's top-level nodes are the first units; a node over the budget
//!    ([`CHUNK_BUDGET_NWS`] non-whitespace characters) is split at its children, recursively
//!    (an oversized leaf at line boundaries).
//! 2. A unit that contains a definition stands alone (with the comments and attributes directly
//!    above it);
//!    adjacent units without one (imports, constants, statements of a split body) merge up to
//!    the budget. A split node's pieces never merge with its siblings.
//! 3. Files without definitions are cut into [`WINDOW_LINES`]-line windows overlapping by
//!    [`WINDOW_OVERLAP`] lines.
//! 4. Each chunk carries a header (`// path: …  lang: …  in: …  sig: …`) that is embedded
//!    with the body but is not part of the stored byte range. The path is in the header, so
//!    the text an embedder receives always names where it came from (§6.5 privacy gate).
//! 5. `hash = sha256(chunker ‖ header ‖ body)`, each field length-prefixed: identical code at
//!    the same path in 100 mirrors is embedded once.
//! 6. When a container (an `impl` block, a class) is split, its header and closing brace join
//!    the first and last member chunk instead of becoming chunks of their own.

use sha2::{Digest, Sha256};

use crate::version::{CHUNK_BUDGET_NWS, CHUNKER};
use crate::{Chunk, Lang};

/// Lines per window for files without definitions.
pub const WINDOW_LINES: usize = 50;
/// Overlap between consecutive windows.
pub const WINDOW_OVERLAP: usize = 10;

/// A file's bytes with the indexes the chunker needs: non-whitespace prefix counts and line
/// starts.
pub struct Text<'a> {
    src: &'a [u8],
    nws_prefix: Vec<u32>,
    line_starts: Vec<usize>,
}

fn is_counted(b: u8) -> bool {
    // Non-whitespace characters: skip ASCII whitespace and UTF-8 continuation bytes.
    !b.is_ascii_whitespace() && (b & 0xC0) != 0x80
}

impl<'a> Text<'a> {
    pub fn new(src: &'a [u8]) -> Text<'a> {
        let mut nws_prefix = Vec::with_capacity(src.len() + 1);
        let mut n = 0u32;
        nws_prefix.push(0);
        let mut line_starts = Vec::new();
        if !src.is_empty() {
            line_starts.push(0);
        }
        for (i, &b) in src.iter().enumerate() {
            if is_counted(b) {
                n = n.saturating_add(1);
            }
            nws_prefix.push(n);
            if b == b'\n' && i + 1 < src.len() {
                line_starts.push(i + 1);
            }
        }
        Text {
            src,
            nws_prefix,
            line_starts,
        }
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.src
    }

    /// Non-whitespace characters in `start..end`.
    pub fn nws(&self, start: usize, end: usize) -> usize {
        let at = |i: usize| self.nws_prefix.get(i).copied().unwrap_or(0);
        usize::try_from(at(end).saturating_sub(at(start))).unwrap_or(usize::MAX)
    }

    /// The 1-based line holding byte `byte`.
    pub fn line_of(&self, byte: usize) -> u32 {
        let n = self.line_starts.partition_point(|&s| s <= byte).max(1);
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Number of lines.
    pub fn lines(&self) -> usize {
        self.line_starts.len()
    }

    /// Byte range of line `idx` (0-based), without its `\n`.
    fn line_range(&self, idx: usize) -> (usize, usize) {
        let start = self.line_starts.get(idx).copied().unwrap_or(self.src.len());
        let end = match self.line_starts.get(idx + 1) {
            Some(&next) => next.saturating_sub(1),
            None if self.src.last() == Some(&b'\n') => self.src.len() - 1,
            None => self.src.len(),
        };
        (start, end.max(start))
    }
}

/// A syntax node as the chunker sees it. Children are asked for only when a node is over
/// the budget, so a tree-sitter node can be wrapped without copying the tree.
pub trait SyntaxNode: Sized {
    /// Byte range, end exclusive.
    fn byte_range(&self) -> (usize, usize);
    /// Comments and attributes: when directly above a definition they join its chunk.
    fn attaches_forward(&self) -> bool;
    fn children(&self) -> Vec<Self>;
}

/// A definition (or a scope such as a Rust `impl` block, with no ordinal) as the chunker needs
/// it. The slice handed to the chunker must be sorted by `(start, Reverse(end))`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDef {
    /// The definition node's byte range, end exclusive.
    pub start: usize,
    pub end: usize,
    /// The definition's ordinal; `None` for a scope.
    pub ordinal: Option<u32>,
    pub kind: String,
    /// Qualified name (`Router::route`).
    pub qualified: String,
    pub signature: String,
}

/// Where a chunk comes from (for its header).
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub path: &'a str,
    pub lang: Lang,
}

/// The header embedded in front of a chunk body.
pub fn header(src: Source<'_>, symbol: &str, signature: &str) -> String {
    let mut h = format!("// path: {}  lang: {}", src.path, src.lang.name());
    if !symbol.is_empty() {
        h.push_str("  in: ");
        h.push_str(symbol);
    }
    if !signature.is_empty() {
        h.push_str("  sig: ");
        h.push_str(signature);
    }
    h.push('\n');
    h
}

/// `sha256(chunker ‖ header ‖ body)`, the embedding key. Each field is length-prefixed
/// (u64 little-endian) so no split of the same bytes between fields can collide.
pub fn chunk_hash(header: &str, body: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    for field in [CHUNKER.as_bytes(), header.as_bytes(), body] {
        h.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
        h.update(field);
    }
    h.finalize().into()
}

/// Glue at the edges of a split container (an `impl Foo {` header, the closing `}`) at most
/// this many non-whitespace characters joins the first/last member chunk instead of
/// becoming a chunk of its own.
const EDGE_GLUE_MAX_NWS: usize = CHUNK_BUDGET_NWS / 4;

#[derive(Debug, Clone, Copy)]
struct Piece {
    start: usize,
    end: usize,
    comment: bool,
}

enum Item {
    /// A node within the budget: mergeable unless it holds a definition.
    Single { piece: Piece, def: bool },
    /// The final ranges of an oversized node holding a definition (never merged with
    /// siblings; the comments above it attach to its first range).
    Group { ranges: Vec<(usize, usize)> },
}

struct Ctx<'t, 'd> {
    text: &'t Text<'t>,
    defs: &'d [ChunkDef],
}

impl Ctx<'_, '_> {
    /// Whether `start..end` holds a whole definition.
    fn def_bearing(&self, start: usize, end: usize) -> bool {
        let from = self.defs.partition_point(|d| d.start < start);
        self.defs
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take_while(|d| d.start < end)
            .any(|d| d.end <= end)
    }

    /// An oversized leaf (a giant literal or comment) cut at line boundaries.
    fn split_lines(&self, start: usize, end: usize, out: &mut Vec<Item>) {
        let mut cur: Option<(usize, usize, usize)> = None; // (start, end, nws)
        let first = usize::try_from(self.text.line_of(start)).unwrap_or(1) - 1;
        for idx in first..self.text.lines() {
            let (ls, le) = self.text.line_range(idx);
            if ls >= end {
                break;
            }
            let (ls, le) = (ls.max(start), le.min(end));
            let n = self.text.nws(ls, le);
            if n == 0 {
                continue;
            }
            match cur {
                Some((cs, ce, cn)) if cn + n > CHUNK_BUDGET_NWS => {
                    out.push(single(cs, ce));
                    cur = Some((ls, le, n));
                }
                Some((cs, _, cn)) => cur = Some((cs, le, cn + n)),
                None => cur = Some((ls, le, n)),
            }
        }
        if let Some((cs, ce, _)) = cur {
            out.push(single(cs, ce));
        }
    }

    /// The final ranges covering `node`'s children. `edges`: `node` is a split container,
    /// so small glue before its first and after its last member unit joins that unit.
    fn level<N: SyntaxNode>(&self, node: &N, edges: bool) -> Vec<(usize, usize)> {
        let mut items = Vec::new();
        self.collect(node, &mut items);
        self.merge(items, edges)
    }

    /// The units under `node`. An oversized child that holds a definition is chunked on its
    /// own (a sealed group); one without is flattened into this level, so a split body's
    /// header and first statements merge.
    fn collect<N: SyntaxNode>(&self, node: &N, items: &mut Vec<Item>) {
        for kid in node.children() {
            let (s, e) = kid.byte_range();
            let n = self.text.nws(s, e);
            if e <= s || n == 0 {
                continue;
            }
            let def = self.def_bearing(s, e);
            if n <= CHUNK_BUDGET_NWS {
                items.push(Item::Single {
                    piece: Piece {
                        start: s,
                        end: e,
                        comment: kid.attaches_forward(),
                    },
                    def,
                });
            } else if def {
                let ranges = self.level(&kid, true);
                if ranges.is_empty() {
                    let mut lines = Vec::new();
                    self.split_lines(s, e, &mut lines);
                    items.push(Item::Group {
                        ranges: self.merge(lines, false),
                    });
                } else {
                    items.push(Item::Group { ranges });
                }
            } else {
                let before = items.len();
                self.collect(&kid, items);
                if items.len() == before {
                    self.split_lines(s, e, items);
                }
            }
        }
    }

    /// Comments directly above `next` (no blank line between) move into the next unit.
    fn take_attached_comments(&self, acc: &mut Vec<Piece>, next: usize) -> Option<usize> {
        let mut boundary = next;
        let mut lead = None;
        while let Some(last) = acc.last() {
            let gap = self
                .text
                .bytes()
                .get(last.end..boundary)
                .unwrap_or_default();
            let newlines = gap.iter().fold(0usize, |n, &b| n + usize::from(b == b'\n'));
            if !last.comment || newlines > 1 {
                break;
            }
            boundary = last.start;
            lead = Some(last.start);
            acc.pop();
        }
        lead
    }

    fn merge(&self, items: Vec<Item>, edges: bool) -> Vec<(usize, usize)> {
        fn flush(acc: &mut Vec<Piece>, out: &mut Vec<(usize, usize)>) {
            if let (Some(f), Some(l)) = (acc.first(), acc.last()) {
                out.push((f.start, l.end));
            }
            acc.clear();
        }
        let mut out = Vec::new();
        let mut acc: Vec<Piece> = Vec::new();
        let mut units = 0usize;
        // Leading edge glue: everything before the first unit, when small.
        let small = |acc: &[Piece]| match (acc.first(), acc.last()) {
            (Some(f), Some(l)) => self.text.nws(f.start, l.end) <= EDGE_GLUE_MAX_NWS,
            _ => false,
        };
        for item in items {
            match item {
                Item::Single { piece, def: false } => {
                    // Measured over the merged span, gaps included (they are whitespace in a
                    // syntax tree, but the budget must hold whatever the caller's nodes are).
                    if let Some(first) = acc.first()
                        && self.text.nws(first.start, piece.end) > CHUNK_BUDGET_NWS
                    {
                        flush(&mut acc, &mut out);
                    }
                    acc.push(piece);
                }
                Item::Single { piece, def: true } => {
                    let lead = if edges && units == 0 && small(&acc) {
                        let lead = acc.first().map(|p| p.start);
                        acc.clear();
                        lead
                    } else {
                        self.take_attached_comments(&mut acc, piece.start)
                    };
                    flush(&mut acc, &mut out);
                    out.push((lead.unwrap_or(piece.start), piece.end));
                    units += 1;
                }
                Item::Group { ranges } => {
                    let first = ranges.first().map_or(0, |r| r.0);
                    let lead = if edges && units == 0 && small(&acc) {
                        let lead = acc.first().map(|p| p.start);
                        acc.clear();
                        lead
                    } else {
                        self.take_attached_comments(&mut acc, first)
                    };
                    flush(&mut acc, &mut out);
                    units += 1;
                    for (i, (s, e)) in ranges.into_iter().enumerate() {
                        let s = if i == 0 { lead.unwrap_or(s) } else { s };
                        out.push((s, e));
                    }
                }
            }
        }
        // Trailing edge glue (a closing brace) joins the last unit.
        if edges
            && units > 0
            && small(&acc)
            && let (Some(last), Some(end)) = (out.last_mut(), acc.last().map(|p| p.end))
        {
            last.1 = end;
            acc.clear();
        }
        flush(&mut acc, &mut out);
        out
    }
}

fn single(start: usize, end: usize) -> Item {
    Item::Single {
        piece: Piece {
            start,
            end,
            comment: false,
        },
        def: false,
    }
}

fn make_chunk(
    src: Source<'_>,
    text: &Text<'_>,
    (start, end): (usize, usize),
    def: Option<&ChunkDef>,
    window: bool,
) -> Chunk {
    let body = text.bytes().get(start..end).unwrap_or_default();
    let (symbol, kind, signature) = match def {
        Some(d) => (
            d.qualified.as_str(),
            d.kind.clone(),
            if start <= d.start {
                d.signature.as_str()
            } else {
                ""
            },
        ),
        None if window => ("", "window".to_string(), ""),
        None => ("", "block".to_string(), ""),
    };
    let header = header(src, symbol, signature);
    let hash = chunk_hash(&header, body);
    let nws = text.nws(start, end);
    Chunk {
        start_byte: u32::try_from(start).unwrap_or(u32::MAX),
        end_byte: u32::try_from(end).unwrap_or(u32::MAX),
        start_line: text.line_of(start),
        end_line: text.line_of(end.saturating_sub(1).max(start)),
        symbol: symbol.to_string(),
        kind,
        def: def.and_then(|d| d.ordinal),
        tokens_est: u32::try_from(nws.div_ceil(3)).unwrap_or(u32::MAX),
        header,
        hash,
    }
}

/// The definition a chunk belongs to: the innermost definition containing it, or the scope
/// it is exactly (a whole `impl` block); else the first definition it contains (a split
/// container's member that took the container's header); else the containing scope.
fn owner(defs: &[ChunkDef], start: usize, end: usize) -> Option<&ChunkDef> {
    let containing = defs
        .iter()
        .filter(|d| d.start <= start && end <= d.end)
        .max_by_key(|d| (d.start, std::cmp::Reverse(d.end)));
    let contained = || {
        defs.iter()
            .find(|d| start <= d.start && d.end <= end && (d.start, d.end) != (start, end))
    };
    match containing {
        Some(d) if d.ordinal.is_some() || (d.start, d.end) == (start, end) => Some(d),
        Some(scope) => contained().filter(|d| d.ordinal.is_some()).or(Some(scope)),
        None => contained(),
    }
}

/// Chunk a parsed file. `defs` sorted by `(start, Reverse(end))`. The root is always split
/// so that top-level definitions are separate units.
pub fn chunk_syntax<N: SyntaxNode>(
    src: Source<'_>,
    text: &Text<'_>,
    root: &N,
    defs: &[ChunkDef],
) -> Vec<Chunk> {
    let ctx = Ctx { text, defs };
    let (s, e) = root.byte_range();
    let mut ranges = ctx.level(root, false);
    if ranges.is_empty() && text.nws(s, e) > 0 {
        let mut items = Vec::new();
        ctx.split_lines(s, e, &mut items);
        ranges = ctx.merge(items, false);
    }
    ranges
        .into_iter()
        .map(|r| make_chunk(src, text, r, owner(defs, r.0, r.1), false))
        .collect()
}

/// Line windows ([`WINDOW_LINES`] lines, [`WINDOW_OVERLAP`] overlap) for files without
/// definitions or without a grammar. Whitespace-only windows are skipped.
pub fn chunk_windows(src: Source<'_>, text: &Text<'_>) -> Vec<Chunk> {
    let total = text.lines();
    let mut out = Vec::new();
    let mut first = 0;
    while first < total {
        let last = (first + WINDOW_LINES).min(total) - 1;
        let start = text.line_range(first).0;
        let end = text.line_range(last).1;
        if text.nws(start, end) > 0 {
            out.push(make_chunk(src, text, (start, end), None, true));
        }
        if last + 1 >= total {
            break;
        }
        first += WINDOW_LINES - WINDOW_OVERLAP;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy syntax tree: a node is a byte range with children.
    #[derive(Clone)]
    struct Toy {
        range: (usize, usize),
        comment: bool,
        kids: Vec<Toy>,
    }

    impl SyntaxNode for Toy {
        fn byte_range(&self) -> (usize, usize) {
            self.range
        }
        fn attaches_forward(&self) -> bool {
            self.comment
        }
        fn children(&self) -> Vec<Toy> {
            self.kids.clone()
        }
    }

    fn leaf(src: &str, needle: &str) -> Toy {
        let s = src.find(needle).unwrap();
        Toy {
            range: (s, s + needle.len()),
            comment: needle.starts_with("//"),
            kids: Vec::new(),
        }
    }

    const SRC: Source<'static> = Source {
        path: "src/a.rs",
        lang: Lang::Rust,
    };

    #[test]
    fn windows_overlap_and_skip_blank_files() {
        let src = (1..=100).fold(String::new(), |s, i| s + &format!("line {i}\n"));
        let text = Text::new(src.as_bytes());
        let w = chunk_windows(SRC, &text);
        let lines: Vec<_> = w.iter().map(|c| (c.start_line, c.end_line)).collect();
        assert_eq!(lines, [(1, 50), (41, 90), (81, 100)]);
        assert!(w.iter().all(|c| c.kind == "window" && c.def.is_none()));
        let want = (1..=50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            src.get(w[0].start_byte as usize..w[0].end_byte as usize),
            Some(want.as_str())
        );
        assert!(chunk_windows(SRC, &Text::new(b"  \n\n\t\n")).is_empty());
        assert!(chunk_windows(SRC, &Text::new(b"")).is_empty());
        let one = chunk_windows(SRC, &Text::new(b"x"));
        assert_eq!(
            (one[0].start_line, one[0].end_line, one[0].end_byte),
            (1, 1, 1)
        );
    }

    #[test]
    fn the_hash_covers_chunker_header_path_and_body() {
        let text = Text::new(b"fn a() {}\n");
        let a = chunk_windows(SRC, &text);
        let again = chunk_windows(SRC, &text);
        assert_eq!(a, again);
        let other_path = chunk_windows(
            Source {
                path: "src/b.rs",
                lang: Lang::Rust,
            },
            &text,
        );
        assert_ne!(a[0].hash, other_path[0].hash);
        assert_eq!(a[0].header, "// path: src/a.rs  lang: rust\n");
        let mut h = Sha256::new();
        for field in [
            &b"cast/2;budget=1500nws"[..],
            b"// path: src/a.rs  lang: rust\n",
            b"fn a() {}",
        ] {
            h.update(u64::try_from(field.len()).unwrap().to_le_bytes());
            h.update(field);
        }
        let want: [u8; 32] = h.finalize().into();
        assert_eq!(a[0].hash, want);
        // Length prefixes: moving bytes between header and body changes the hash.
        assert_ne!(
            chunk_hash("// h\n", b"x"),
            chunk_hash("// h", b"\nx"),
            "fields are length-prefixed"
        );
        assert_eq!(
            header(SRC, "Router::route", "pub fn route(&self)"),
            "// path: src/a.rs  lang: rust  in: Router::route  sig: pub fn route(&self)\n"
        );
    }

    #[test]
    fn definitions_are_units_glue_merges_and_comments_attach() {
        let src = "use a;\nuse b;\n\n// doc for f\nfn f() {}\n\nconst X: u8 = 1;\nconst Y: u8 = 2;\nfn g() {}\n";
        let kids = [
            "use a;",
            "use b;",
            "// doc for f",
            "fn f() {}",
            "const X: u8 = 1;",
            "const Y: u8 = 2;",
            "fn g() {}",
        ]
        .map(|n| leaf(src, n))
        .to_vec();
        let root = Toy {
            range: (0, src.len()),
            comment: false,
            kids,
        };
        let def = |needle: &str, ordinal: u32, name: &str| {
            let s = src.find(needle).unwrap();
            ChunkDef {
                start: s,
                end: s + needle.len(),
                ordinal: Some(ordinal),
                kind: "function".into(),
                qualified: name.into(),
                signature: needle.into(),
            }
        };
        let defs = [def("fn f() {}", 0, "f"), def("fn g() {}", 1, "g")];
        let text = Text::new(src.as_bytes());
        let chunks = chunk_syntax(SRC, &text, &root, &defs);
        let bodies: Vec<&str> = chunks
            .iter()
            .map(|c| src.get(c.start_byte as usize..c.end_byte as usize).unwrap())
            .collect();
        assert_eq!(
            bodies,
            [
                "use a;\nuse b;",
                "// doc for f\nfn f() {}",
                "const X: u8 = 1;\nconst Y: u8 = 2;",
                "fn g() {}"
            ]
        );
        let kinds: Vec<_> = chunks.iter().map(|c| (c.kind.as_str(), c.def)).collect();
        assert_eq!(
            kinds,
            [
                ("block", None),
                ("function", Some(0)),
                ("block", None),
                ("function", Some(1))
            ]
        );
        assert!(chunks[1].header.ends_with("in: f  sig: fn f() {}\n"));
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (4, 5));
    }

    #[test]
    fn oversized_definitions_split_within_the_budget() {
        // fn big() { s0; s1; … } with 400 statements of 10 non-whitespace characters each.
        let stmts: Vec<String> = (0..400).map(|i| format!("    st{i:07};\n")).collect();
        let src = format!("fn big() {{\n{}}}\nfn small() {{}}\n", stmts.concat());
        let body_open = src.find('{').unwrap();
        let mut stmt_nodes = Vec::new();
        let mut at = body_open + 2; // after "{\n"
        for s in &stmts {
            let start = at + 4;
            stmt_nodes.push(Toy {
                range: (start, start + s.trim().len()),
                comment: false,
                kids: Vec::new(),
            });
            at += s.len();
        }
        let close = src.find("}\nfn small").unwrap();
        let mut block_kids = vec![Toy {
            range: (body_open, body_open + 1),
            comment: false,
            kids: Vec::new(),
        }];
        block_kids.extend(stmt_nodes);
        block_kids.push(Toy {
            range: (close, close + 1),
            comment: false,
            kids: Vec::new(),
        });
        let big = Toy {
            range: (0, close + 1),
            comment: false,
            kids: vec![
                Toy {
                    range: (0, body_open - 1),
                    comment: false,
                    kids: Vec::new(),
                },
                Toy {
                    range: (body_open, close + 1),
                    comment: false,
                    kids: block_kids,
                },
            ],
        };
        let small_at = src.find("fn small").unwrap();
        let small = Toy {
            range: (small_at, small_at + "fn small() {}".len()),
            comment: false,
            kids: Vec::new(),
        };
        let root = Toy {
            range: (0, src.len()),
            comment: false,
            kids: vec![big, small],
        };
        let defs = [
            ChunkDef {
                start: 0,
                end: close + 1,
                ordinal: Some(0),
                kind: "function".into(),
                qualified: "big".into(),
                signature: "fn big() {".into(),
            },
            ChunkDef {
                start: small_at,
                end: small_at + 13,
                ordinal: Some(1),
                kind: "function".into(),
                qualified: "small".into(),
                signature: "fn small() {}".into(),
            },
        ];
        let text = Text::new(src.as_bytes());
        let chunks = chunk_syntax(SRC, &text, &root, &defs);
        assert!(chunks.len() >= 3, "{}", chunks.len());
        for c in &chunks {
            assert!(text.nws(c.start_byte as usize, c.end_byte as usize) <= CHUNK_BUDGET_NWS);
        }
        // Every piece of the split body belongs to `big`; only the first carries its signature.
        let (last, split) = chunks.split_last().unwrap();
        assert!(split.iter().all(|c| c.def == Some(0) && c.symbol == "big"));
        assert!(split[0].header.contains("sig: fn big() {"));
        assert!(split.iter().skip(1).all(|c| !c.header.contains("sig:")));
        assert_eq!((last.def, last.kind.as_str()), (Some(1), "function"));
        // Contiguous coverage of the body, in order.
        for w in split.windows(2) {
            assert!(w[0].end_byte <= w[1].start_byte);
        }
        assert_eq!(split[0].start_byte, 0);
        assert_eq!(split.last().unwrap().end_byte as usize, close + 1);
    }

    /// A split container's header and braces join its first and last member chunks: no
    /// `impl Foo`, `{` or `}` chunk of its own.
    #[test]
    fn split_containers_keep_header_and_braces_with_their_members() {
        let body = |n: usize| "x".repeat(n);
        let m1 = format!("    fn a() {{ {} }}\n", body(800));
        let m2 = format!("    fn b() {{ {} }}\n", body(800));
        let src = format!("impl Foo {{\n{m1}{m2}}}\n");
        let at = |needle: &str| src.find(needle).unwrap();
        let leaf = |s: usize, e: usize| Toy {
            range: (s, e),
            comment: false,
            kids: Vec::new(),
        };
        let open = at("{\n");
        let (a, b) = (at("fn a"), at("fn b"));
        let (a_end, b_end) = (a + m1.trim().len(), b + m2.trim().len());
        let close = src.rfind('}').unwrap();
        let block = Toy {
            range: (open, close + 1),
            comment: false,
            kids: vec![
                leaf(open, open + 1),
                leaf(a, a_end),
                leaf(b, b_end),
                leaf(close, close + 1),
            ],
        };
        let imp = Toy {
            range: (0, close + 1),
            comment: false,
            kids: vec![leaf(0, "impl Foo".len()), block],
        };
        let root = Toy {
            range: (0, src.len()),
            comment: false,
            kids: vec![imp],
        };
        let def = |start: usize, end: usize, ordinal: u32, name: &str| ChunkDef {
            start,
            end,
            ordinal: Some(ordinal),
            kind: "method".into(),
            qualified: name.into(),
            signature: String::new(),
        };
        let defs = [
            ChunkDef {
                start: 0,
                end: close + 1,
                ordinal: None,
                kind: "impl".into(),
                qualified: "Foo".into(),
                signature: String::new(),
            },
            def(a, a_end, 0, "Foo::a"),
            def(b, b_end, 1, "Foo::b"),
        ];
        let text = Text::new(src.as_bytes());
        let chunks = chunk_syntax(SRC, &text, &root, &defs);
        let spans: Vec<(usize, usize)> = chunks
            .iter()
            .map(|c| (c.start_byte as usize, c.end_byte as usize))
            .collect();
        assert_eq!(spans, [(0, a_end), (b, close + 1)], "{chunks:?}");
        assert_eq!(chunks[0].def, Some(0));
        assert_eq!(chunks[1].def, Some(1));
    }
}
