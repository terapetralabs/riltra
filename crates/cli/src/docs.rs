// SPDX-License-Identifier: Apache-2.0
//! Markdown section and link parser for `riltra docs` (Stage 7, Task 1).
//!
//! Pure: reads nothing from disk. The rules are the Stage 7 plan's D2
//! (sections and links) and D3 (suggested record id).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::index::{self, AnchorKind, EdgeAnchor, GraphQueryError};
use crate::knowledge::{self, EvidenceFreshness};
use crate::{
    edges, git, knowledge_binding, knowledge_store, paths, registry, response, scan, semantic,
    store,
};

/// One section of a Markdown document: a heading and the bytes up to the
/// next heading of any level, or the end of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Section {
    pub section_id: String,
    pub heading: Option<String>,
    pub heading_path: Vec<String>,
    pub level: u8,
    pub line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    /// `store::content_digest` of the section's bytes, as `capture_evidence` computes it.
    pub digest: String,
    pub links: Vec<Link>,
}

/// One inline link found outside fences and code spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub line: usize,
    pub text: String,
    pub target: String,
    pub kind: LinkKind,
}

/// What a link target points at. Only `Path` links are resolved further.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkKind {
    External,
    SameDocument,
    /// The path leaves the repository root lexically.
    OutsideRepo,
    Path {
        path: String,
        fragment: Option<String>,
        line: Option<usize>,
        end_line: Option<usize>,
    },
}

/// Splits a document into sections and collects its inline links.
pub(crate) fn parse_document(doc_path: &str, text: &str) -> Vec<Section> {
    struct Head {
        line: usize,
        start: usize,
        level: u8,
        text: String,
    }
    struct Spec {
        line: usize,
        start: usize,
        level: u8,
        heading: Option<String>,
        heading_path: Vec<String>,
    }

    let mut heads: Vec<Head> = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    // (fence character, opening run length) while inside a fence.
    let mut fence: Option<(u8, usize)> = None;
    let mut offset = 0usize;
    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let line_start = offset;
        offset += raw.len();
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if let Some((ch, len)) = fence {
            if closes_fence(content, ch, len) {
                fence = None;
            }
            continue;
        }
        if let Some(opening) = opens_fence(content) {
            fence = Some(opening);
            continue;
        }
        if let Some((level, title)) = parse_heading(content) {
            heads.push(Head {
                line: line_no,
                start: line_start,
                level,
                text: title,
            });
        }
        scan_links(doc_path, content, line_no, &mut links);
    }

    let mut specs: Vec<Spec> = Vec::new();
    let first_start = heads.first().map_or(text.len(), |h| h.start);
    if !text[..first_start].trim().is_empty() {
        specs.push(Spec {
            line: 1,
            start: 0,
            level: 0,
            heading: None,
            heading_path: Vec::new(),
        });
    }
    let mut stack: Vec<(u8, String)> = Vec::new();
    for h in heads {
        while stack.last().is_some_and(|(level, _)| *level >= h.level) {
            stack.pop();
        }
        stack.push((h.level, h.text.clone()));
        specs.push(Spec {
            line: h.line,
            start: h.start,
            level: h.level,
            heading: Some(h.text),
            heading_path: stack.iter().map(|(_, t)| t.clone()).collect(),
        });
    }

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<Section> = Vec::with_capacity(specs.len());
    for (idx, spec) in specs.iter().enumerate() {
        let byte_end = specs.get(idx + 1).map_or(text.len(), |n| n.start);
        let next_line = specs.get(idx + 1).map_or(usize::MAX, |n| n.line);
        let base = if spec.level == 0 {
            "(preamble)".to_string()
        } else {
            spec.heading_path.join(" / ")
        };
        let k = counts.entry(base.clone()).or_insert(0);
        *k += 1;
        out.push(Section {
            section_id: format!("{base}@{k}"),
            heading: spec.heading.clone(),
            heading_path: spec.heading_path.clone(),
            level: spec.level,
            line: spec.line,
            byte_start: spec.start,
            byte_end,
            digest: store::content_digest(&text.as_bytes()[spec.start..byte_end]),
            links: links
                .iter()
                .filter(|l| l.line >= spec.line && l.line < next_line)
                .cloned()
                .collect(),
        });
    }
    out
}

/// The suggested knowledge record id for one section (D3).
pub(crate) fn suggested_record_id(
    doc_path: &str,
    heading: Option<&str>,
    section_id: &str,
) -> String {
    // Budget: "doc" (3) + "-" + 57 + "-" + 57 + "-" + 8 hex = 128 bytes at most.
    const SLUG_CAP: usize = 57;
    let file_start = doc_path.rfind('/').map_or(0, |slash| slash + 1);
    let stem = match doc_path[file_start..].rfind('.') {
        Some(dot) => &doc_path[..file_start + dot],
        None => doc_path,
    };
    let doc_slug = trim_cut(&slug(stem), SLUG_CAP);
    let heading_slug = trim_cut(&slug(heading.unwrap_or("preamble")), SLUG_CAP);
    let hash = store::content_digest(format!("{doc_path}\0{section_id}").as_bytes());
    let mut id = String::from("doc");
    if !doc_slug.is_empty() {
        id.push('-');
        id.push_str(&doc_slug);
    }
    if !heading_slug.is_empty() {
        id.push('-');
        id.push_str(&heading_slug);
    }
    id.push('-');
    id.push_str(&hash[..8]);
    id
}

/// Lowercase ASCII letters and digits are kept; every other byte run is one `-`.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b.to_ascii_lowercase() as char);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Cuts an ASCII slug to `max` bytes and drops a trailing `-` left by the cut.
fn trim_cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    s[..max].trim_end_matches('-').to_string()
}

/// Strips up to three spaces of indent; `None` for a deeper indent.
fn after_indent(s: &str) -> Option<&str> {
    let spaces = s.len() - s.trim_start_matches(' ').len();
    (spaces <= 3).then(|| &s[spaces..])
}

fn run_len(s: &str, ch: u8) -> usize {
    s.bytes().take_while(|b| *b == ch).count()
}

/// A fence opens on a run of three or more backticks or tildes.
fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let rest = after_indent(line)?;
    let ch = *rest.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let n = run_len(rest, ch);
    (n >= 3).then_some((ch, n))
}

/// A closing fence is a run of the same character, at least as long, then only whitespace.
fn closes_fence(line: &str, ch: u8, len: usize) -> bool {
    let Some(rest) = after_indent(line) else {
        return false;
    };
    let n = run_len(rest, ch);
    n >= len && rest[n..].chars().all(char::is_whitespace)
}

/// ATX headings only: 1 to 6 `#`, then a space, a tab or the end of the line.
fn parse_heading(line: &str) -> Option<(u8, String)> {
    let rest = after_indent(line)?;
    let hashes = run_len(rest, b'#');
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &rest[hashes..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let mut title = after.trim();
    let stripped = title.trim_end_matches('#');
    if stripped.len() < title.len()
        && (stripped.is_empty() || stripped.ends_with(' ') || stripped.ends_with('\t'))
    {
        title = stripped.trim_end();
    }
    Some((hashes as u8, title.to_string()))
}

/// Cuts `s` to at most `max` bytes on a char boundary.
fn cut_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Finds inline links on one non-fence line, skipping code spans and images.
fn scan_links(doc_path: &str, line: &str, line_no: usize, out: &mut Vec<Link>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let run = run_len(&line[i..], b'`');
                let mut j = i + run;
                let mut close = None;
                while j < bytes.len() {
                    if bytes[j] == b'`' {
                        let r = run_len(&line[j..], b'`');
                        if r == run {
                            close = Some(j + r);
                            break;
                        }
                        j += r;
                    } else {
                        j += 1;
                    }
                }
                i = close.unwrap_or(i + run);
            }
            b'[' if i == 0 || bytes[i - 1] != b'!' => {
                match parse_link_at(doc_path, line, i, line_no) {
                    Some((link, end)) => {
                        out.push(link);
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
}

/// Parses `[text](target)` starting at the `[` at byte `i`. Returns the link and the byte after it.
fn parse_link_at(doc_path: &str, line: &str, i: usize, line_no: usize) -> Option<(Link, usize)> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut j = i;
    let close = loop {
        if j >= bytes.len() {
            return None;
        }
        match bytes[j] {
            b'\\' => j += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let open = close + 2;
    let mut k = open;
    let mut parens = 0usize;
    let end_paren = loop {
        if k >= bytes.len() {
            return None;
        }
        match bytes[k] {
            b'(' => parens += 1,
            b')' => {
                if parens == 0 {
                    break k;
                }
                parens -= 1;
            }
            _ => {}
        }
        k += 1;
    };
    let inner = line[open..end_paren].trim_start_matches([' ', '\t']);
    let target = match inner.strip_prefix('<') {
        Some(angled) => &angled[..angled.find('>')?],
        None => inner.split([' ', '\t']).next().unwrap_or(""),
    };
    let kind = classify(doc_path, target)?;
    let link = Link {
        line: line_no,
        text: cut_bytes(&line[i + 1..close], 200),
        target: cut_bytes(target, 512),
        kind,
    };
    Some((link, end_paren + 1))
}

/// Classifies a link target. `None` means the target is not a reference (for example, empty).
fn classify(doc_path: &str, target: &str) -> Option<LinkKind> {
    if target.is_empty() {
        return None;
    }
    if target.starts_with('#') {
        return Some(LinkKind::SameDocument);
    }
    if has_scheme(target) {
        return Some(LinkKind::External);
    }
    let (path_part, frag) = target.split_once('#').unwrap_or((target, ""));
    let Some(path) = resolve(doc_path, path_part) else {
        return Some(LinkKind::OutsideRepo);
    };
    if path.is_empty() {
        return None;
    }
    let (line, end_line, fragment) = parse_fragment(frag);
    Some(LinkKind::Path {
        path,
        fragment,
        line,
        end_line,
    })
}

/// `[A-Za-z][A-Za-z0-9+.-]*:` before the first `/` or `#`.
fn has_scheme(target: &str) -> bool {
    let Some(colon) = target.find(':') else {
        return false;
    };
    let prefix = &target[..colon];
    if prefix.contains('/') || prefix.contains('#') {
        return false;
    }
    let mut chars = prefix.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// Resolves a path lexically. A leading `/` is repository-root relative; otherwise the
/// document's directory is the base. `None` when the path leaves the root.
fn resolve(doc_path: &str, path_part: &str) -> Option<String> {
    let mut stack: Vec<&str> = Vec::new();
    if !path_part.starts_with('/') {
        let dir = doc_path.rsplit_once('/').map_or("", |(d, _)| d);
        for seg in dir.split('/') {
            push_segment(&mut stack, seg)?;
        }
    }
    for seg in path_part.split('/') {
        push_segment(&mut stack, seg)?;
    }
    Some(stack.join("/"))
}

fn push_segment<'a>(stack: &mut Vec<&'a str>, seg: &'a str) -> Option<()> {
    match seg {
        "" | "." => Some(()),
        ".." => stack.pop().map(|_| ()),
        s => {
            stack.push(s);
            Some(())
        }
    }
}

/// `L<n>` or `L<n>-L<m>` with n >= 1 and m >= n. Any other fragment is kept as text.
fn parse_fragment(frag: &str) -> (Option<usize>, Option<usize>, Option<String>) {
    if frag.is_empty() {
        return (None, None, None);
    }
    match parse_lines(frag) {
        Some((n, m)) => (Some(n), m, None),
        None => (None, None, Some(frag.to_string())),
    }
}

fn parse_lines(frag: &str) -> Option<(usize, Option<usize>)> {
    let rest = frag.strip_prefix('L')?;
    let (first, second) = match rest.split_once("-L") {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    let n = parse_line_number(first)?;
    let m = match second {
        Some(b) => {
            let m = parse_line_number(b)?;
            if m < n {
                return None;
            }
            Some(m)
        }
        None => None,
    };
    Some((n, m))
}

fn parse_line_number(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<usize>().ok().filter(|&v| v >= 1)
}

// ---------------------------------------------------------------------------
// `riltra docs scan` (Stage 7, Task 2): D0, D1, D3, D4, D5, D8 and D9.
// ---------------------------------------------------------------------------

const SCHEMA_VERSION: u32 = 1;
const MAX_DOCUMENTS: usize = 200;
const MAX_SECTIONS: usize = 500;
const MAX_LINKS: usize = 50;
const MAX_PROPOSALS: usize = 500;
const MAX_STALE: usize = 20;
const MAX_CANDIDATES: usize = 5;
const MAX_PATHS: usize = 20;
const MAX_REPORT_BYTES: usize = 262_144;
/// Binding provenance cap (`docs-ingest:<doc>#<section_id>@<digest>`), D4.
const PROVENANCE_MAX: usize = 4_096;
const DEF_ROLE: u32 = 1;
const DISCLAIMER: &str = "Document sections are the author's claims. Proposals come only from explicit links to indexed definitions; riltra wrote nothing, and a proposal or binding is not a verified relationship.";
const LINK_STATUSES: [&str; 12] = [
    "outside-repo",
    "no-index",
    "not-indexed",
    "source-changed",
    "file",
    "no-definition-at-line",
    "ambiguous-line",
    "definition",
    "anchor-unproven",
    "anchor-unavailable",
    "external",
    "same-document",
];

/// A failure with its exit code (2 usage, 3 repository, data or storage).
type Fail = (i32, String);

fn storage(e: impl std::fmt::Display) -> Fail {
    (3, e.to_string())
}

fn is_markdown(path: &str) -> bool {
    path.ends_with(".md") || path.ends_with(".markdown")
}

/// A document to scan: its validated relative path and whether it was named with `--path`.
struct DocIn {
    path: String,
    explicit: bool,
}

/// The documents to read (D0). Walked documents are capped at 200; the rest are counted.
fn discover(root: &Path, paths_in: &[String]) -> Result<(Vec<DocIn>, usize), Fail> {
    if paths_in.is_empty() {
        let (plan, _) = scan::plan_walk(root).map_err(storage)?;
        let mut found: Vec<String> = plan
            .into_iter()
            .filter(|f| matches!(f.action, scan::PlanAction::Unsupported(_)) && is_markdown(&f.rel))
            .map(|f| f.rel)
            .collect();
        found.sort();
        let omitted = found.len().saturating_sub(MAX_DOCUMENTS);
        found.truncate(MAX_DOCUMENTS);
        let docs = found
            .into_iter()
            .map(|path| DocIn {
                path,
                explicit: false,
            })
            .collect();
        return Ok((docs, omitted));
    }
    if paths_in.len() > MAX_PATHS {
        return Err((2, format!("at most {MAX_PATHS} --path values")));
    }
    let mut set = BTreeSet::new();
    for raw in paths_in {
        if !is_markdown(raw) {
            return Err((
                2,
                format!("--path must name a Markdown file (.md or .markdown): '{raw}'"),
            ));
        }
        let rel = semantic::validate_snapshot_rel(raw)
            .map_err(|m| (2, format!("invalid --path: {m}")))?;
        set.insert(rel);
    }
    let docs = set
        .into_iter()
        .map(|path| DocIn {
            path,
            explicit: true,
        })
        .collect();
    Ok((docs, 0))
}

/// What the file system holds at a document path (D0).
enum Content {
    Read(Vec<u8>),
    Missing,
    TooLarge,
    Unreadable(String),
}

fn read_doc(root: &Path, doc: &DocIn) -> Result<Content, Fail> {
    match semantic::confined_read_classified(root, &doc.path, scan::MAX_FILE_BYTES) {
        Ok(bytes) => Ok(Content::Read(bytes)),
        Err(semantic::ConfinedReadError::NotFound(_)) => Ok(Content::Missing),
        Err(semantic::ConfinedReadError::Path(m)) if doc.explicit => {
            Err((2, format!("invalid --path: {m}")))
        }
        Err(semantic::ConfinedReadError::Path(m)) => Ok(Content::Unreadable(m)),
        Err(semantic::ConfinedReadError::Io(m)) => {
            let big = root
                .join(&doc.path)
                .metadata()
                .map(|meta| meta.len() > scan::MAX_FILE_BYTES)
                .unwrap_or(false);
            Ok(if big {
                Content::TooLarge
            } else {
                Content::Unreadable(m)
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Proof of a definition anchor (D3). Shared with ingest (Task 3).
// ---------------------------------------------------------------------------

/// The result of proving one syntax definition against the semantic facts.
pub(crate) enum Proof {
    /// The anchor resolves to the definition's own occurrence.
    Proven(EdgeAnchor),
    /// No proof: the reason becomes the link's `anchor-unproven` reason.
    Unproven(String),
}

/// The unqualified part of a stored definition name (after the last `::` or `.`).
fn unqualified(name: &str) -> &str {
    let last = name.rsplit("::").next().unwrap_or(name);
    last.rsplit('.').next().unwrap_or(last)
}

/// 0-based line and byte column of a byte offset in the live source.
fn position(source: &[u8], offset: usize) -> (u32, u32) {
    let before = &source[..offset];
    let line = before.iter().filter(|b| **b == b'\n').count();
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(offset - line_start).unwrap_or(u32::MAX),
    )
}

/// Whether the import behind a fact belongs to this snapshot and passes the provenance rules.
fn import_usable(
    conn: &Connection,
    snapshot_id: i64,
    import_id: i64,
    cache: &mut BTreeMap<i64, bool>,
) -> Result<bool, GraphQueryError> {
    if let Some(known) = cache.get(&import_id) {
        return Ok(*known);
    }
    let usable = store::semantic_import_by_id(conn, import_id)?.is_some_and(|(_, import)| {
        import.snapshot_id == snapshot_id && edges::import_provenance_ok(&import)
    });
    cache.insert(import_id, usable);
    Ok(usable)
}

/// Proves that `definition` (a syntax row of `validated`) owns one occurrence, as D3 states.
/// The resolver is never called at the definition's own start, where a return-type reference
/// can be the narrowest occurrence. Scan and ingest both call this on the same transaction.
pub(crate) fn prove_anchor(
    conn: &Connection,
    snapshot_id: i64,
    canon: &Path,
    validated: &str,
    definition: &store::SymbolRow,
    source: &[u8],
) -> Result<Proof, GraphQueryError> {
    let Some(name) = definition
        .name
        .as_deref()
        .map(unqualified)
        .filter(|n| !n.is_empty())
    else {
        return Ok(Proof::Unproven("the definition has no name".to_string()));
    };
    let defs = store::symbols_at(conn, snapshot_id, validated)?;
    let facts = store::semantic_facts_at(conn, snapshot_id, validated)?;
    let width = definition.byte_end.saturating_sub(definition.byte_start);
    let descendants: Vec<(usize, usize)> = defs
        .iter()
        .filter(|d| {
            d.byte_start >= definition.byte_start
                && d.byte_end <= definition.byte_end
                && d.byte_end.saturating_sub(d.byte_start) < width
        })
        .map(|d| (d.byte_start, d.byte_end))
        .collect();
    let mut usable = BTreeMap::new();
    let mut owned = Vec::new();
    let mut nested_same_name = false;
    for fact in &facts {
        if fact.roles & DEF_ROLE == 0 || fact.symbol_uri.is_none() {
            continue;
        }
        if source.get(fact.byte_start..fact.byte_end) != Some(name.as_bytes()) {
            continue;
        }
        if !(definition.byte_start <= fact.byte_start && fact.byte_end <= definition.byte_end) {
            continue;
        }
        if !import_usable(conn, snapshot_id, fact.import_id, &mut usable)? {
            continue;
        }
        let nested = descendants
            .iter()
            .any(|(s, e)| *s <= fact.byte_start && fact.byte_end <= *e);
        if nested {
            nested_same_name = true;
        } else {
            owned.push(fact);
        }
    }
    if owned.len() > 1 {
        return Ok(Proof::Unproven(
            "two or more occurrences own this definition".to_string(),
        ));
    }
    if let Some(fact) = owned.first() {
        let (line0, col0) = position(source, fact.byte_start);
        let anchor = index::resolve_edge_anchor_classified(
            conn,
            snapshot_id,
            canon,
            validated,
            line0,
            col0,
            None,
            Some(source),
        )?;
        let same = anchor.kind == AnchorKind::Semantic
            && anchor.span_bytes == Some((fact.byte_start, fact.byte_end))
            && anchor.symbol_uri == fact.symbol_uri;
        return Ok(if same {
            Proof::Proven(anchor)
        } else {
            Proof::Unproven("the occurrence does not resolve to itself".to_string())
        });
    }
    if nested_same_name {
        return Ok(Proof::Unproven(
            "a same-named occurrence sits in a nested definition; the import does not cover this one"
                .to_string(),
        ));
    }
    let anchor = index::resolve_edge_anchor_classified(
        conn,
        snapshot_id,
        canon,
        validated,
        definition.start_line.saturating_sub(1),
        definition.start_col.saturating_sub(1),
        None,
        Some(source),
    )?;
    if anchor.kind == AnchorKind::SyntaxFallback
        && anchor.span_bytes == Some((definition.byte_start, definition.byte_end))
    {
        Ok(Proof::Proven(anchor))
    } else {
        Ok(Proof::Unproven(
            "no occurrence owns this definition, and its position is not a syntax fallback"
                .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Existing records (D5) and the per-document facts.
// ---------------------------------------------------------------------------

/// One stored evidence row of a record, on the document being scanned.
struct Row {
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    digest: String,
    freshness: &'static str,
}

/// A head record that cites the document.
struct Hit {
    id: String,
    revision: u64,
    kind: String,
    title: String,
    status: String,
    rows: Vec<Row>,
}

struct Knowledge {
    status: String,
    reason: Option<String>,
    generation: Option<u64>,
    hits: BTreeMap<String, Vec<Hit>>,
}

fn freshness_name(state: &EvidenceFreshness) -> &'static str {
    match state {
        EvidenceFreshness::Fresh => "fresh",
        EvidenceFreshness::Stale { .. } => "stale",
        EvidenceFreshness::Unresolved { .. } => "unresolved",
    }
}

/// Head records citing each document, read in one transaction (D5). A store that cannot be
/// opened is a status, not a failure; a read that fails once it is open is exit 3.
fn read_knowledge(
    home: &Path,
    repo: &str,
    root: &Path,
    docs: &[String],
) -> Result<Knowledge, Fail> {
    let mut hits = BTreeMap::new();
    if !knowledge_store::knowledge_db_path(home, repo).is_file() {
        return Ok(Knowledge {
            status: "no-store".to_string(),
            reason: None,
            generation: None,
            hits,
        });
    }
    let reg = registry::load(&home.join("registry.json")).map_err(storage)?;
    let Some(entry) = registry::find(&reg, repo) else {
        return Err((3, format!("unknown repository: '{repo}'")));
    };
    let mut conn = match knowledge_store::open_knowledge_at(home, entry, false) {
        Ok(conn) => conn,
        Err(e) if e.starts_with("forward knowledge schema version") => {
            return Ok(Knowledge {
                status: "schema-ahead".to_string(),
                reason: None,
                generation: None,
                hits,
            })
        }
        Err(e) => {
            return Ok(Knowledge {
                status: "unavailable".to_string(),
                reason: Some(e),
                generation: None,
                hits,
            })
        }
    };
    let tx = conn.transaction().map_err(storage)?;
    let generation = knowledge_store::read_generation(&tx).map_err(storage)?;
    for doc in docs {
        let mut list = Vec::new();
        for record in knowledge_store::records_citing_path(&tx, doc).map_err(storage)? {
            let rows: Vec<Row> = record
                .evidence
                .iter()
                .filter(|e| e.path == *doc)
                .map(|e| Row {
                    byte_start: e.byte_start,
                    byte_end: e.byte_end,
                    digest: e.digest.clone(),
                    freshness: freshness_name(&knowledge_store::evidence_freshness(e, root)),
                })
                .collect();
            if rows.is_empty() {
                continue;
            }
            list.push(Hit {
                id: record.id,
                revision: record.revision,
                kind: record.kind,
                title: record.title,
                status: record.status,
                rows,
            });
        }
        if !list.is_empty() {
            hits.insert(doc.clone(), list);
        }
    }
    tx.commit().map_err(storage)?;
    Ok(Knowledge {
        status: "ready".to_string(),
        reason: None,
        generation: Some(generation),
        hits,
    })
}

/// D4: whether a section can be ingested, and the first reason it cannot.
fn ingest_refusal(doc: &str, section: &Section) -> Option<&'static str> {
    let title = match &section.heading {
        Some(heading) => heading.as_str(),
        None => doc.rsplit('/').next().unwrap_or(doc),
    };
    if title.is_empty() {
        return Some("empty-heading");
    }
    if title.len() > knowledge::MAX_TITLE_BYTES {
        return Some("heading-too-long");
    }
    if section.byte_end - section.byte_start > knowledge::MAX_BODY_BYTES {
        return Some("section-too-large");
    }
    let provenance = format!(
        "docs-ingest:{doc}#{}@{}",
        section.section_id, section.digest
    );
    if provenance.len() > PROVENANCE_MAX {
        return Some("provenance-too-long");
    }
    None
}

// ---------------------------------------------------------------------------
// Report shapes (D9). Field order is key order.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    repo: String,
    snapshot: Option<SnapshotOut>,
    index_freshness: Freshness,
    knowledge: KnowledgeOut,
    documents: Vec<DocOut>,
    proposals: Vec<Proposal>,
    omissions: Vec<Omission>,
    next_actions: Vec<NextAction>,
    bounds: Bounds,
    disclaimer: &'static str,
}

#[derive(Serialize)]
struct SnapshotOut {
    nonce: String,
    /// The semantic-import generation (`store::semantic_generation`), read in the same D1
    /// transaction as the snapshot. It is the counter `checks` and `map` report, and it moves
    /// when a SCIP import commits, which can change the anchors a scan proves.
    generation: i64,
}

#[derive(Serialize)]
struct Freshness {
    status: String,
    reasons: Vec<String>,
}

#[derive(Serialize)]
struct KnowledgeOut {
    status: String,
    reason: Option<String>,
    generation: Option<u64>,
}

#[derive(Serialize)]
struct DocOut {
    path: String,
    status: &'static str,
    reason: Option<String>,
    bytes: Option<usize>,
    digest: Option<String>,
    git_head: Option<String>,
    git_branch: Option<String>,
    link_counts: BTreeMap<&'static str, usize>,
    sections: Vec<SectionOut>,
    stale_records: Vec<StaleOut>,
}

#[derive(Serialize)]
struct SectionOut {
    section_id: String,
    heading: Option<String>,
    level: u8,
    line: usize,
    byte_start: usize,
    byte_end: usize,
    bytes: usize,
    digest: String,
    ingest_refusal: Option<&'static str>,
    recorded: Option<RecordedOut>,
    links: Vec<LinkOut>,
}

#[derive(Serialize, Clone)]
struct RecordedOut {
    id: String,
    revision: u64,
}

#[derive(Serialize, Clone)]
struct LinkOut {
    line: usize,
    text: String,
    target: String,
    path: Option<String>,
    fragment: Option<String>,
    target_line: Option<usize>,
    target_end_line: Option<usize>,
    status: &'static str,
    reason: Option<String>,
    exists: Option<bool>,
    definition: Option<DefOut>,
    anchor: Option<AnchorOut>,
    candidates: Vec<CandOut>,
}

#[derive(Serialize, Clone)]
struct DefOut {
    name: Option<String>,
    kind: String,
    line: u32,
    col: u32,
    byte_start: usize,
    byte_end: usize,
}

#[derive(Serialize, Clone)]
struct AnchorOut {
    kind: &'static str,
    precision: String,
    line: u32,
    col: u32,
    byte_start: usize,
    byte_end: usize,
    symbol_uri: Option<String>,
    display_name: Option<String>,
    source_digest: String,
}

#[derive(Serialize, Clone)]
struct CandOut {
    name: Option<String>,
    kind: String,
    line: u32,
    col: u32,
}

#[derive(Serialize, Clone)]
struct TargetOut {
    path: String,
    symbol: Option<String>,
    definition: DefOut,
    anchor: AnchorOut,
}

#[derive(Serialize, Clone)]
struct Proposal {
    doc: String,
    section_id: String,
    section_digest: String,
    suggested_record_id: String,
    target: TargetOut,
    relation: &'static str,
    state: &'static str,
    ingest_refusal: Option<&'static str>,
}

#[derive(Serialize, Clone)]
struct StaleOut {
    id: String,
    revision: u64,
    kind: String,
    title: String,
    status: String,
    evidence: Vec<EvidenceOut>,
}

#[derive(Serialize, Clone)]
struct EvidenceOut {
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    freshness: &'static str,
}

#[derive(Serialize)]
struct Omission {
    section: &'static str,
    reason: &'static str,
    omitted_count: Option<usize>,
}

#[derive(Serialize)]
struct NextAction {
    reason: &'static str,
    command: Option<String>,
}

#[derive(Serialize)]
struct Bounds {
    documents: usize,
    sections_per_document: usize,
    links_per_section: usize,
    proposals: usize,
    stale_records_per_document: usize,
    candidates_per_link: usize,
    report_bytes: usize,
}

/// Omission counts by (list, reason). Every count here is known, so none is `null`.
#[derive(Default)]
struct Omissions(BTreeMap<(&'static str, &'static str), usize>);

impl Omissions {
    fn add(&mut self, section: &'static str, reason: &'static str, count: usize) {
        if count > 0 {
            *self.0.entry((section, reason)).or_insert(0) += count;
        }
    }

    fn rows(&self) -> Vec<Omission> {
        self.0
            .iter()
            .map(|((section, reason), count)| Omission {
                section,
                reason,
                omitted_count: Some(*count),
            })
            .collect()
    }
}

fn json_len<T: Serialize + ?Sized>(value: &T) -> Result<usize, String> {
    serde_json::to_string(value)
        .map(|s| s.len())
        .map_err(|e| format!("docs scan report cannot be serialized: {e}"))
}

/// Running size of the report, kept exact by subtracting each removed item's own length.
struct Budget {
    size: usize,
    om: usize,
}

impl Budget {
    fn refresh(&mut self, omissions: &Omissions) -> Result<(), String> {
        let rows = json_len(&omissions.rows())?;
        self.size = self.size.saturating_sub(self.om) + rows;
        self.om = rows;
        Ok(())
    }
}

/// Removes the last item of a list and counts it as a `json_budget` omission (D8).
fn drop_last<T: Serialize>(
    list: &mut Vec<T>,
    budget: &mut Budget,
    omissions: &mut Omissions,
    section: &'static str,
) -> Result<(), String> {
    let Some(item) = list.pop() else {
        return Ok(());
    };
    let comma = usize::from(!list.is_empty());
    budget.size = budget.size.saturating_sub(json_len(&item)? + comma);
    omissions.add(section, "json_budget", 1);
    budget.refresh(omissions)
}

/// Fits the report to `max_bytes` (D8), removing in the fixed order and counting each removal.
fn fit_with(
    mut report: Report,
    mut omissions: Omissions,
    max_bytes: usize,
) -> Result<Report, String> {
    report.omissions = omissions.rows();
    let mut budget = Budget {
        size: json_len(&report)?,
        om: json_len(&report.omissions)?,
    };
    // 1. Links of sections: last document first, last link first.
    for doc in report.documents.iter_mut().rev() {
        for section in doc.sections.iter_mut().rev() {
            while budget.size > max_bytes && !section.links.is_empty() {
                drop_last(&mut section.links, &mut budget, &mut omissions, "links")?;
            }
        }
    }
    // 2. Proposals, last first.
    while budget.size > max_bytes && !report.proposals.is_empty() {
        drop_last(
            &mut report.proposals,
            &mut budget,
            &mut omissions,
            "proposals",
        )?;
    }
    // 3. Sections, last document first.
    for doc in report.documents.iter_mut().rev() {
        while budget.size > max_bytes && !doc.sections.is_empty() {
            drop_last(&mut doc.sections, &mut budget, &mut omissions, "sections")?;
        }
    }
    // 4. Stale records.
    for doc in report.documents.iter_mut().rev() {
        while budget.size > max_bytes && !doc.stale_records.is_empty() {
            drop_last(
                &mut doc.stale_records,
                &mut budget,
                &mut omissions,
                "stale_records",
            )?;
        }
    }
    // 5. Documents after the first, last first.
    while budget.size > max_bytes && report.documents.len() > 1 {
        drop_last(
            &mut report.documents,
            &mut budget,
            &mut omissions,
            "documents",
        )?;
    }
    // 6. The remainder: at most three next actions.
    report.omissions = omissions.rows();
    if json_len(&report)? > max_bytes {
        report.next_actions.truncate(3);
    }
    report.omissions = omissions.rows();
    if json_len(&report)? > max_bytes {
        return Err("docs scan report exceeds byte budget".to_string());
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// The scan itself.
// ---------------------------------------------------------------------------

/// The index as the scan reads it: an open read transaction with a snapshot, or nothing.
struct Index<'c> {
    conn: Option<&'c Connection>,
    snapshot: Option<store::Snapshot>,
    freshness: Freshness,
}

/// A link target's index facts, read once per path.
struct Target {
    exists: bool,
    indexed: Option<String>,
    live: Result<Vec<u8>, String>,
    symbols: Vec<store::SymbolRow>,
}

struct Ctx<'c> {
    canon: PathBuf,
    index: Option<(&'c Connection, i64)>,
    targets: HashMap<String, Rc<Target>>,
}

/// One evaluated link: its listed form, and the proposal it earns.
struct LinkEval {
    out: LinkOut,
    proposal: Option<TargetOut>,
    omitted_candidates: usize,
}

fn bare_link(link: &Link) -> LinkOut {
    LinkOut {
        line: link.line,
        text: link.text.clone(),
        target: link.target.clone(),
        path: None,
        fragment: None,
        target_line: None,
        target_end_line: None,
        status: "",
        reason: None,
        exists: None,
        definition: None,
        anchor: None,
        candidates: Vec::new(),
    }
}

fn settle(mut out: LinkOut, status: &'static str, reason: Option<String>) -> LinkEval {
    out.status = status;
    out.reason = reason;
    LinkEval {
        out,
        proposal: None,
        omitted_candidates: 0,
    }
}

fn def_out(def: &store::SymbolRow) -> DefOut {
    DefOut {
        name: def.name.clone(),
        kind: def.kind.clone(),
        line: def.start_line,
        col: def.start_col,
        byte_start: def.byte_start,
        byte_end: def.byte_end,
    }
}

fn anchor_out(anchor: &EdgeAnchor, digest: &str) -> AnchorOut {
    let (byte_start, byte_end) = anchor.span_bytes.unwrap_or((0, 0));
    AnchorOut {
        kind: anchor.kind.as_str(),
        precision: anchor.precision.clone(),
        line: anchor.line1,
        col: anchor.col1,
        byte_start,
        byte_end,
        symbol_uri: anchor.symbol_uri.clone(),
        display_name: anchor.display.clone(),
        source_digest: digest.to_string(),
    }
}

impl Ctx<'_> {
    /// The index facts of one path, read once and shared by every link to it.
    fn target(&mut self, path: &str) -> Result<Rc<Target>, Fail> {
        if let Some(found) = self.targets.get(path) {
            return Ok(Rc::clone(found));
        }
        let (conn, sid) = self
            .index
            .ok_or_else(|| (3, "internal: target read without an index".to_string()))?;
        let indexed = store::file_identity(conn, sid, path)
            .map_err(storage)?
            .map(|(_, digest)| digest);
        let symbols = if indexed.is_some() {
            store::symbols_at(conn, sid, path).map_err(storage)?
        } else {
            Vec::new()
        };
        let live = semantic::confined_read_classified(&self.canon, path, scan::MAX_FILE_BYTES)
            .map_err(semantic::ConfinedReadError::into_message);
        let exists = paths::contains(&self.canon, &self.canon.join(path));
        let found = Rc::new(Target {
            exists,
            indexed,
            live,
            symbols,
        });
        self.targets.insert(path.to_string(), Rc::clone(&found));
        Ok(found)
    }

    /// D3: one link's status, and the proposal it earns when proven.
    fn evaluate(&mut self, link: &Link) -> Result<LinkEval, Fail> {
        let (path, fragment, line, end_line) = match &link.kind {
            LinkKind::External => return Ok(settle(bare_link(link), "external", None)),
            LinkKind::SameDocument => return Ok(settle(bare_link(link), "same-document", None)),
            LinkKind::OutsideRepo => {
                return Ok(settle(
                    bare_link(link),
                    "outside-repo",
                    Some("path leaves the repository root".to_string()),
                ))
            }
            LinkKind::Path {
                path,
                fragment,
                line,
                end_line,
            } => (path, fragment, line, end_line),
        };
        let mut out = bare_link(link);
        out.path = Some(path.clone());
        out.fragment = fragment.clone();
        out.target_line = *line;
        out.target_end_line = *end_line;
        if let Err(reason) = semantic::validate_snapshot_rel(path) {
            return Ok(settle(out, "outside-repo", Some(reason)));
        }
        let Some((conn, sid)) = self.index else {
            return Ok(settle(out, "no-index", None));
        };
        let target = self.target(path)?;
        let Some(digest) = target.indexed.clone() else {
            out.exists = Some(target.exists);
            return Ok(settle(out, "not-indexed", None));
        };
        let source: &[u8] = match &target.live {
            Err(m) => return Ok(settle(out, "source-changed", Some(m.clone()))),
            Ok(bytes) if store::content_digest(bytes) != digest => {
                return Ok(settle(
                    out,
                    "source-changed",
                    Some("source changed since the index; reindex".to_string()),
                ))
            }
            Ok(bytes) => bytes.as_slice(),
        };
        let Some(line) = *line else {
            return Ok(settle(out, "file", None));
        };
        let line1 = u32::try_from(line).unwrap_or(u32::MAX);
        let defs: Vec<&store::SymbolRow> = target
            .symbols
            .iter()
            .filter(|s| s.start_line == line1)
            .collect();
        match defs.len() {
            0 => Ok(settle(out, "no-definition-at-line", None)),
            n if n > 1 => {
                out.candidates = defs
                    .iter()
                    .take(MAX_CANDIDATES)
                    .map(|s| CandOut {
                        name: s.name.clone(),
                        kind: s.kind.clone(),
                        line: s.start_line,
                        col: s.start_col,
                    })
                    .collect();
                let mut evaluated = settle(out, "ambiguous-line", None);
                evaluated.omitted_candidates = n.saturating_sub(MAX_CANDIDATES);
                Ok(evaluated)
            }
            _ => {
                let def = defs[0];
                match prove_anchor(conn, sid, &self.canon, path, def, source) {
                    Err(GraphQueryError::Anchor(reason)) => {
                        Ok(settle(out, "anchor-unavailable", Some(reason)))
                    }
                    Err(GraphQueryError::Store(m)) => Err(storage(m)),
                    Ok(Proof::Unproven(reason)) => Ok(settle(out, "anchor-unproven", Some(reason))),
                    Ok(Proof::Proven(anchor)) => {
                        let definition = def_out(def);
                        let anchor = anchor_out(&anchor, &digest);
                        let symbol = anchor.symbol_uri.clone();
                        out.definition = Some(definition.clone());
                        out.anchor = Some(anchor.clone());
                        let mut evaluated = settle(out, "definition", None);
                        evaluated.proposal = Some(TargetOut {
                            path: path.clone(),
                            symbol,
                            definition,
                            anchor,
                        });
                        Ok(evaluated)
                    }
                }
            }
        }
    }
}

/// Accumulators shared across documents.
#[derive(Default)]
struct Acc {
    omissions: Omissions,
    pending: Vec<Pending>,
    source_changed: bool,
    stale: bool,
}

struct Pending {
    key: (String, usize, usize, String),
    proposal: Proposal,
}

fn recorded_for(hits: &[Hit], section: &Section) -> Option<RecordedOut> {
    hits.iter().find_map(|hit| {
        hit.rows
            .iter()
            .any(|r| {
                r.freshness == "fresh"
                    && r.byte_start == Some(section.byte_start as i64)
                    && r.byte_end == Some(section.byte_end as i64)
                    && r.digest == section.digest
            })
            .then(|| RecordedOut {
                id: hit.id.clone(),
                revision: hit.revision,
            })
    })
}

fn scan_document(
    ctx: &mut Ctx<'_>,
    acc: &mut Acc,
    knowledge: &Knowledge,
    doc: &DocIn,
    content: Content,
    git_info: &git::GitInfo,
) -> Result<DocOut, Fail> {
    let hits: &[Hit] = match knowledge.hits.get(&doc.path) {
        Some(found) => found.as_slice(),
        None => &[],
    };
    let mut stale_all: Vec<StaleOut> = hits
        .iter()
        .filter(|h| h.rows.iter().any(|r| r.freshness != "fresh"))
        .map(|h| StaleOut {
            id: h.id.clone(),
            revision: h.revision,
            kind: h.kind.clone(),
            title: h.title.clone(),
            status: h.status.clone(),
            evidence: h
                .rows
                .iter()
                .map(|r| EvidenceOut {
                    byte_start: r.byte_start,
                    byte_end: r.byte_end,
                    freshness: r.freshness,
                })
                .collect(),
        })
        .collect();
    if !stale_all.is_empty() {
        acc.stale = true;
    }
    acc.omissions.add(
        "stale_records",
        "item_limit",
        stale_all.len().saturating_sub(MAX_STALE),
    );
    stale_all.truncate(MAX_STALE);

    let (status, reason, bytes, digest, text): (
        &'static str,
        Option<String>,
        Option<usize>,
        Option<String>,
        Option<String>,
    ) = match content {
        Content::Read(raw) => match String::from_utf8(raw) {
            Ok(text) => (
                "ok",
                None,
                Some(text.len()),
                Some(store::content_digest(text.as_bytes())),
                Some(text),
            ),
            Err(_) => ("not-utf8", None, None, None, None),
        },
        Content::Missing => ("missing", None, None, None, None),
        Content::TooLarge => ("too-large", None, None, None, None),
        Content::Unreadable(m) => ("unreadable", Some(m), None, None, None),
    };

    let mut counts: BTreeMap<&'static str, usize> =
        LINK_STATUSES.iter().map(|s| (*s, 0usize)).collect();
    let mut sections_out = Vec::new();
    if let Some(text) = &text {
        let parsed = parse_document(&doc.path, text);
        for (idx, section) in parsed.iter().enumerate() {
            let mut evaluated = Vec::with_capacity(section.links.len());
            for link in &section.links {
                let eval = ctx.evaluate(link)?;
                *counts.entry(eval.out.status).or_insert(0) += 1;
                if eval.out.status == "source-changed" {
                    acc.source_changed = true;
                }
                evaluated.push(eval);
            }
            if idx >= MAX_SECTIONS {
                continue;
            }
            let refusal = ingest_refusal(&doc.path, section);
            let total_links = evaluated.len();
            let mut links = Vec::new();
            for eval in evaluated.into_iter().take(MAX_LINKS) {
                acc.omissions
                    .add("candidates", "item_limit", eval.omitted_candidates);
                let LinkEval { out, proposal, .. } = eval;
                if let Some(target) = proposal {
                    let suggested = suggested_record_id(
                        &doc.path,
                        section.heading.as_deref(),
                        &section.section_id,
                    );
                    acc.pending.push(Pending {
                        key: (
                            doc.path.clone(),
                            section.byte_start,
                            out.line,
                            target.path.clone(),
                        ),
                        proposal: Proposal {
                            doc: doc.path.clone(),
                            section_id: section.section_id.clone(),
                            section_digest: section.digest.clone(),
                            suggested_record_id: suggested,
                            target,
                            relation: "documents",
                            state: "proposed",
                            ingest_refusal: refusal,
                        },
                    });
                }
                links.push(out);
            }
            acc.omissions
                .add("links", "item_limit", total_links.saturating_sub(MAX_LINKS));
            sections_out.push(SectionOut {
                section_id: section.section_id.clone(),
                heading: section.heading.clone(),
                level: section.level,
                line: section.line,
                byte_start: section.byte_start,
                byte_end: section.byte_end,
                bytes: section.byte_end - section.byte_start,
                digest: section.digest.clone(),
                ingest_refusal: refusal,
                recorded: recorded_for(hits, section),
                links,
            });
        }
        acc.omissions.add(
            "sections",
            "item_limit",
            parsed.len().saturating_sub(MAX_SECTIONS),
        );
    }

    Ok(DocOut {
        path: doc.path.clone(),
        status,
        reason,
        bytes,
        digest,
        git_head: git_info.head.clone(),
        git_branch: git_info.branch.clone(),
        link_counts: counts,
        sections: sections_out,
        stale_records: stale_all,
    })
}

/// Builds the report from the documents and one index view (D1 to D9).
#[allow(clippy::too_many_arguments)]
fn assemble(
    home: &Path,
    repo: &str,
    root: &Path,
    docs: &[DocIn],
    contents: Vec<Content>,
    doc_omitted: usize,
    git_info: &git::GitInfo,
    index: Index<'_>,
) -> Result<Report, Fail> {
    let doc_paths: Vec<String> = docs.iter().map(|d| d.path.clone()).collect();
    let knowledge = read_knowledge(home, repo, root, &doc_paths)?;
    let mut acc = Acc::default();
    acc.omissions.add("documents", "item_limit", doc_omitted);
    let snapshot_out = match (index.conn, index.snapshot.as_ref()) {
        (Some(conn), Some(s)) => Some(SnapshotOut {
            nonce: s.nonce.clone(),
            generation: store::semantic_generation(conn).map_err(storage)?,
        }),
        _ => None,
    };
    let link_index = match (index.conn, index.snapshot.as_ref()) {
        (Some(conn), Some(snapshot)) => Some((conn, snapshot.id)),
        _ => None,
    };
    let mut ctx = Ctx {
        canon: root.to_path_buf(),
        index: link_index,
        targets: HashMap::new(),
    };
    let mut documents = Vec::with_capacity(docs.len());
    for (doc, content) in docs.iter().zip(contents) {
        documents.push(scan_document(
            &mut ctx, &mut acc, &knowledge, doc, content, git_info,
        )?);
    }

    let mut pending = std::mem::take(&mut acc.pending);
    pending.sort_by(|a, b| a.key.cmp(&b.key));
    acc.omissions.add(
        "proposals",
        "item_limit",
        pending.len().saturating_sub(MAX_PROPOSALS),
    );
    pending.truncate(MAX_PROPOSALS);
    let proposals: Vec<Proposal> = pending.into_iter().map(|p| p.proposal).collect();

    let mut next_actions = Vec::new();
    if index.snapshot.is_none() || acc.source_changed {
        next_actions.push(NextAction {
            reason: "no usable index, or a linked source changed: reindex",
            command: Some(format!("riltra index --repo {repo}")),
        });
    }
    if acc.stale {
        next_actions.push(NextAction {
            reason:
                "a stored record cites bytes that changed: re-ingest it with its expected revision",
            command: Some(format!("riltra docs ingest --repo {repo} --input -")),
        });
    }
    next_actions.push(NextAction {
        reason: "review each proposal; riltra wrote nothing and verified nothing",
        command: None,
    });

    let report = Report {
        schema_version: SCHEMA_VERSION,
        repo: repo.to_string(),
        snapshot: snapshot_out,
        index_freshness: index.freshness,
        knowledge: KnowledgeOut {
            status: knowledge.status,
            reason: knowledge.reason,
            generation: knowledge.generation,
        },
        documents,
        proposals,
        omissions: Vec::new(),
        next_actions,
        bounds: Bounds {
            documents: MAX_DOCUMENTS,
            sections_per_document: MAX_SECTIONS,
            links_per_section: MAX_LINKS,
            proposals: MAX_PROPOSALS,
            stale_records_per_document: MAX_STALE,
            candidates_per_link: MAX_CANDIDATES,
            report_bytes: MAX_REPORT_BYTES,
        },
        disclaimer: DISCLAIMER,
    };
    fit_with(report, acc.omissions, MAX_REPORT_BYTES).map_err(|m| (3, m))
}

/// The `docs scan` document for one registered repository (D0 to D9). Storage is opened
/// read-only; a missing or unusable index is reported, never created. Exit 2 for a usage
/// error, 3 for a repository, data or storage failure.
pub fn scan(
    home: &Path,
    repo: &str,
    root: &Path,
    paths_in: &[String],
) -> Result<String, (i32, String)> {
    let canon = paths::canonical_root(root).map_err(storage)?;
    let (docs, doc_omitted) = discover(&canon, paths_in)?;
    let mut contents = Vec::with_capacity(docs.len());
    for doc in &docs {
        contents.push(read_doc(&canon, doc)?);
    }
    let git_info = git::fingerprint(&canon);
    let report = match store::open_read(&store::db_path(home, repo)) {
        store::IndexRead::Missing => assemble(
            home,
            repo,
            &canon,
            &docs,
            contents,
            doc_omitted,
            &git_info,
            Index {
                conn: None,
                snapshot: None,
                freshness: Freshness {
                    status: "no-index".to_string(),
                    reasons: Vec::new(),
                },
            },
        )?,
        store::IndexRead::Outdated(_) => assemble(
            home,
            repo,
            &canon,
            &docs,
            contents,
            doc_omitted,
            &git_info,
            Index {
                conn: None,
                snapshot: None,
                freshness: Freshness {
                    status: "unknown".to_string(),
                    reasons: vec!["schema-outdated".to_string()],
                },
            },
        )?,
        store::IndexRead::Ahead(_) => assemble(
            home,
            repo,
            &canon,
            &docs,
            contents,
            doc_omitted,
            &git_info,
            Index {
                conn: None,
                snapshot: None,
                freshness: Freshness {
                    status: "unknown".to_string(),
                    reasons: vec!["schema-ahead".to_string()],
                },
            },
        )?,
        store::IndexRead::Error(message) => {
            return Err((3, format!("cannot read index: {message}")))
        }
        store::IndexRead::Ready(mut conn) => {
            let tx = conn.transaction().map_err(storage)?;
            let snapshot = store::latest_snapshot(&tx).map_err(storage)?;
            let freshness = match &snapshot {
                Some(s) => {
                    let (status, reasons) = index::evaluate_snapshot(&tx, repo, &canon, s);
                    Freshness { status, reasons }
                }
                None => Freshness {
                    status: "unknown".to_string(),
                    reasons: vec!["no-snapshot".to_string()],
                },
            };
            let built = assemble(
                home,
                repo,
                &canon,
                &docs,
                contents,
                doc_omitted,
                &git_info,
                Index {
                    conn: Some(&*tx),
                    snapshot,
                    freshness,
                },
            )?;
            tx.commit().map_err(storage)?;
            built
        }
    };
    serde_json::to_string(&report).map_err(storage)
}

// ---------------------------------------------------------------------------
// Ingest (D6): reviewed proposals become candidate records. Input is parsed and
// every item is validated before any write; each item then commits in its own
// transaction, in input order.
// ---------------------------------------------------------------------------

const INGEST_ITEMS_MAX: usize = 50;
const INGEST_BINDINGS_MAX: usize = 20;
const TOP_KEYS: [&str; 1] = ["items"];
const ITEM_KEYS: [&str; 8] = [
    "doc",
    "section_id",
    "section_digest",
    "record_id",
    "kind",
    "author",
    "expected_revision",
    "bindings",
];
const BINDING_KEYS: [&str; 3] = ["target", "relation", "rationale"];
const TARGET_KEYS: [&str; 4] = ["path", "symbol", "definition", "anchor"];
const DEFINITION_KEYS: [&str; 6] = ["name", "kind", "line", "col", "byte_start", "byte_end"];
const ANCHOR_KEYS: [&str; 9] = [
    "kind",
    "precision",
    "line",
    "col",
    "byte_start",
    "byte_end",
    "symbol_uri",
    "display_name",
    "source_digest",
];

/// One item as parsed from the input, before any check against the store.
struct IngestItem {
    doc: String,
    section_id: String,
    section_digest: String,
    record_id: String,
    kind: String,
    author: String,
    expected_revision: Option<u64>,
    bindings: Vec<IngestBinding>,
}

struct IngestBinding {
    target: IngestTarget,
    relation: String,
    rationale: String,
}

/// The reviewed proposal target, copied whole from the scan (D3).
struct IngestTarget {
    path: String,
    symbol: Option<String>,
    definition: IngestDefinition,
    anchor: IngestAnchor,
}

struct IngestDefinition {
    name: String,
    kind: String,
    line: u32,
    col: u32,
    byte_start: usize,
    byte_end: usize,
}

struct IngestAnchor {
    kind: String,
    precision: String,
    line: u32,
    col: u32,
    byte_start: usize,
    byte_end: usize,
    symbol_uri: Option<String>,
    display_name: Option<String>,
    source_digest: String,
}

/// One item that passed every check: the payloads the writers receive.
struct Prepared {
    record_id: String,
    kind: String,
    author: String,
    expected_revision: Option<u64>,
    title: String,
    body: String,
    doc: String,
    section_id: String,
    section_digest: String,
    byte_start: usize,
    byte_end: usize,
    bindings: Vec<PreparedBinding>,
}

struct PreparedBinding {
    anchor: knowledge_binding::CodeAnchor,
    relation: String,
    rationale: String,
    provenance: String,
}

fn section_changed(doc: &str, section_id: &str) -> Fail {
    (
        2,
        format!("section-changed: {doc} {section_id}; scan again"),
    )
}

fn required<'a>(
    obj: &'a [(String, knowledge::JVal)],
    key: &str,
    path: &str,
) -> Result<&'a knowledge::JVal, String> {
    knowledge::find(obj, key).ok_or_else(|| format!("missing required field `{path}.{key}`"))
}

fn nullable_str(
    obj: &[(String, knowledge::JVal)],
    key: &str,
    path: &str,
) -> Result<Option<String>, String> {
    match required(obj, key, path)? {
        knowledge::JVal::Null => Ok(None),
        knowledge::JVal::Str(s) => Ok(Some(s.clone())),
        _ => Err(format!(
            "type mismatch: field `{path}.{key}` must be a string or null"
        )),
    }
}

fn u32_field(obj: &[(String, knowledge::JVal)], key: &str, path: &str) -> Result<u32, String> {
    let n = knowledge::req_int_non_negative(required(obj, key, path)?, &format!("{path}.{key}"))?;
    u32::try_from(n).map_err(|_| format!("field `{path}.{key}` is out of range"))
}

fn usize_field(obj: &[(String, knowledge::JVal)], key: &str, path: &str) -> Result<usize, String> {
    let n = knowledge::req_int_non_negative(required(obj, key, path)?, &format!("{path}.{key}"))?;
    usize::try_from(n).map_err(|_| format!("field `{path}.{key}` is out of range"))
}

/// Parses the whole input (D6): size, duplicate keys at every depth, unknown keys at every
/// object level, item and binding counts, and distinct record ids across items.
fn parse_ingest(input: &[u8]) -> Result<Vec<IngestItem>, Fail> {
    if input.len() > knowledge::MAX_MUTATION_JSON_BYTES {
        return Err((
            2,
            format!(
                "mutation input too large: exceeds the {} byte limit",
                knowledge::MAX_MUTATION_JSON_BYTES
            ),
        ));
    }
    let text = std::str::from_utf8(input)
        .map_err(|_| (2, "invalid ingest input: not UTF-8".to_string()))?;
    let value =
        knowledge::parse_dupless(text).map_err(|e| (2, format!("invalid ingest input: {e}")))?;
    let top = knowledge::as_obj(&value, "$").map_err(|e| (2, e))?;
    knowledge::check_known(top, &TOP_KEYS, "$").map_err(|e| (2, e))?;
    let items = match knowledge::find(top, "items") {
        Some(knowledge::JVal::Arr(items)) => items,
        Some(_) => {
            return Err((
                2,
                "type mismatch: field `$.items` must be an array".to_string(),
            ))
        }
        None => return Err((2, "missing required field `$.items`".to_string())),
    };
    if items.is_empty() || items.len() > INGEST_ITEMS_MAX {
        return Err((2, format!("items must hold 1..={INGEST_ITEMS_MAX} entries")));
    }
    let mut parsed = Vec::with_capacity(items.len());
    let mut ids = BTreeSet::new();
    for (index, value) in items.iter().enumerate() {
        let item = parse_item(value, index).map_err(|e| (2, e))?;
        if !ids.insert(item.record_id.clone()) {
            return Err((
                2,
                format!("duplicate record id in input: '{}'", item.record_id),
            ));
        }
        parsed.push(item);
    }
    Ok(parsed)
}

fn parse_item(value: &knowledge::JVal, index: usize) -> Result<IngestItem, String> {
    let path = format!("items[{index}]");
    let obj = knowledge::as_obj(value, &path)?;
    knowledge::check_known(obj, &ITEM_KEYS, &path)?;
    let expected_revision = match knowledge::find(obj, "expected_revision") {
        None => None,
        Some(v) => {
            let n = knowledge::req_int_non_negative(v, &format!("{path}.expected_revision"))?;
            if n < 1 {
                return Err(format!(
                    "field `{path}.expected_revision` must be at least 1"
                ));
            }
            Some(n as u64)
        }
    };
    let bindings_value = match knowledge::find(obj, "bindings") {
        Some(knowledge::JVal::Arr(list)) => list,
        Some(_) => {
            return Err(format!(
                "type mismatch: field `{path}.bindings` must be an array"
            ))
        }
        None => return Err(format!("missing required field `{path}.bindings`")),
    };
    if bindings_value.len() > INGEST_BINDINGS_MAX {
        return Err(format!(
            "field `{path}.bindings` holds at most {INGEST_BINDINGS_MAX} entries"
        ));
    }
    let mut bindings = Vec::with_capacity(bindings_value.len());
    for (b, binding) in bindings_value.iter().enumerate() {
        bindings.push(parse_binding(binding, &format!("{path}.bindings[{b}]"))?);
    }
    Ok(IngestItem {
        doc: knowledge::req_str(obj, "doc", &path)?,
        section_id: knowledge::req_str(obj, "section_id", &path)?,
        section_digest: knowledge::req_str(obj, "section_digest", &path)?,
        record_id: knowledge::req_str(obj, "record_id", &path)?,
        kind: knowledge::req_str(obj, "kind", &path)?,
        author: knowledge::req_str(obj, "author", &path)?,
        expected_revision,
        bindings,
    })
}

fn parse_binding(value: &knowledge::JVal, path: &str) -> Result<IngestBinding, String> {
    let obj = knowledge::as_obj(value, path)?;
    knowledge::check_known(obj, &BINDING_KEYS, path)?;
    let target_path = format!("{path}.target");
    let t = knowledge::as_obj(required(obj, "target", path)?, &target_path)?;
    knowledge::check_known(t, &TARGET_KEYS, &target_path)?;
    let def_path = format!("{target_path}.definition");
    let d = knowledge::as_obj(required(t, "definition", &target_path)?, &def_path)?;
    knowledge::check_known(d, &DEFINITION_KEYS, &def_path)?;
    let anchor_path = format!("{target_path}.anchor");
    let a = knowledge::as_obj(required(t, "anchor", &target_path)?, &anchor_path)?;
    knowledge::check_known(a, &ANCHOR_KEYS, &anchor_path)?;
    Ok(IngestBinding {
        target: IngestTarget {
            path: knowledge::req_str(t, "path", &target_path)?,
            symbol: nullable_str(t, "symbol", &target_path)?,
            definition: IngestDefinition {
                name: knowledge::req_str(d, "name", &def_path)?,
                kind: knowledge::req_str(d, "kind", &def_path)?,
                line: u32_field(d, "line", &def_path)?,
                col: u32_field(d, "col", &def_path)?,
                byte_start: usize_field(d, "byte_start", &def_path)?,
                byte_end: usize_field(d, "byte_end", &def_path)?,
            },
            anchor: IngestAnchor {
                kind: knowledge::req_str(a, "kind", &anchor_path)?,
                precision: knowledge::req_str(a, "precision", &anchor_path)?,
                line: u32_field(a, "line", &anchor_path)?,
                col: u32_field(a, "col", &anchor_path)?,
                byte_start: usize_field(a, "byte_start", &anchor_path)?,
                byte_end: usize_field(a, "byte_end", &anchor_path)?,
                symbol_uri: nullable_str(a, "symbol_uri", &anchor_path)?,
                display_name: nullable_str(a, "display_name", &anchor_path)?,
                source_digest: knowledge::req_str(a, "source_digest", &anchor_path)?,
            },
        },
        relation: knowledge::req_str(obj, "relation", path)?,
        rationale: knowledge::req_str(obj, "rationale", path)?,
    })
}

/// The knowledge store opened read-only for preflight. A missing store means no
/// record exists yet; nothing is created here.
fn existing_store(home: &Path, entry: &registry::RepoEntry) -> Result<Option<Connection>, Fail> {
    if !knowledge_store::knowledge_db_path(home, &entry.name).is_file() {
        return Ok(None);
    }
    knowledge_store::open_knowledge_at(home, entry, false)
        .map(Some)
        .map_err(storage)
}

fn head_revision(conn: &Connection, id: &str) -> Result<Option<u64>, Fail> {
    let head: Option<i64> = conn
        .query_row(
            "SELECT current_revision FROM heads WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| storage(format!("cannot read record head: {e}")))?;
    head.map(|h| {
        u64::try_from(h).map_err(|_| storage("corrupt knowledge store: head out of range"))
    })
    .transpose()
}

fn stored_status(conn: &Connection, id: &str, revision: u64) -> Result<String, Fail> {
    conn.query_row(
        "SELECT status FROM records WHERE id = ?1 AND revision = ?2",
        rusqlite::params![id, revision as i64],
        |row| row.get(0),
    )
    .map_err(|e| storage(format!("cannot read record status: {e}")))
}

fn stored_kind(conn: &Connection, id: &str, revision: u64) -> Result<String, Fail> {
    conn.query_row(
        "SELECT kind FROM records WHERE id = ?1 AND revision = ?2",
        rusqlite::params![id, revision as i64],
        |row| row.get(0),
    )
    .map_err(|e| storage(format!("cannot read record kind: {e}")))
}

/// Re-proves one binding on the latest snapshot and checks both reviewed identities (D6).
fn prove_binding(
    home: &Path,
    entry: &registry::RepoEntry,
    canon: &Path,
    target: &IngestTarget,
) -> Result<knowledge_binding::CodeAnchor, Fail> {
    semantic::validate_snapshot_rel(&target.path)
        .map_err(|m| (2, format!("invalid target path '{}': {m}", target.path)))?;
    if target.anchor.line == 0 || target.anchor.col == 0 {
        return Err((
            2,
            "invalid anchor position: line and column must be at least 1".to_string(),
        ));
    }
    // The resolver checks the live source first: a changed target refuses with the
    // resolver's own `source changed` message before any identity comparison.
    let anchor = knowledge_binding::resolve_code_anchor(
        home,
        entry,
        &target.path,
        target.anchor.line,
        target.anchor.col,
        target.symbol.as_deref(),
        true,
        None,
    )
    .map_err(|e| {
        // The reviewed symbol no longer owns the position: the target changed.
        if e.contains("--symbol disagrees with the position identity") {
            (
                2,
                format!(
                    "binding-target-changed: {}:{}; scan again",
                    target.path, target.anchor.line
                ),
            )
        } else {
            (knowledge_binding::write_error_code(&e), e)
        }
    })?;
    let changed = || {
        (
            2,
            format!(
                "binding-target-changed: {}:{}; scan again",
                target.path, target.definition.line
            ),
        )
    };
    let conn = match store::open_read(&store::db_path(home, &entry.name)) {
        store::IndexRead::Ready(conn) => conn,
        _ => return Err((3, "no code index: index the repository first".to_string())),
    };
    let snapshot = store::latest_snapshot(&conn)
        .map_err(storage)?
        .ok_or_else(|| {
            (
                3,
                "no code snapshot: index the repository first".to_string(),
            )
        })?;
    let defs = store::symbols_at(&conn, snapshot.id, &target.path).map_err(storage)?;
    let starting: Vec<&store::SymbolRow> = defs
        .iter()
        .filter(|d| d.start_line == target.definition.line)
        .collect();
    if starting.len() != 1 {
        return Err(changed());
    }
    let row = starting[0];
    let def = &target.definition;
    if row.name.as_deref() != Some(def.name.as_str())
        || row.kind != def.kind
        || row.start_col != def.col
        || row.byte_start != def.byte_start
        || row.byte_end != def.byte_end
    {
        return Err(changed());
    }
    let source =
        semantic::confined_read(canon, &target.path, scan::MAX_FILE_BYTES).map_err(|m| (3, m))?;
    match prove_anchor(&conn, snapshot.id, canon, &target.path, row, &source) {
        Err(GraphQueryError::Anchor(_)) | Ok(Proof::Unproven(_)) => return Err(changed()),
        Err(GraphQueryError::Store(m)) => return Err(storage(m)),
        Ok(Proof::Proven(edge)) => {
            let a = &target.anchor;
            let same_edge = edge.kind.as_str() == a.kind
                && edge.precision == a.precision
                && edge.line1 == a.line
                && edge.col1 == a.col
                && edge.span_bytes == Some((a.byte_start, a.byte_end))
                && edge.symbol_uri == a.symbol_uri
                && edge.display == a.display_name;
            let same_anchor = anchor.anchor_kind == a.kind
                && anchor.precision == a.precision
                && anchor.cursor_line == a.line
                && anchor.cursor_col == a.col
                && anchor.byte_start == a.byte_start
                && anchor.byte_end == a.byte_end
                && anchor.symbol_uri == a.symbol_uri
                && anchor.display_name == a.display_name
                && anchor.source_digest == a.source_digest;
            if !(same_edge && same_anchor) {
                return Err(changed());
            }
        }
    }
    Ok(anchor)
}

/// Validates one item against the read-only store and the live sources (D6, step 1).
fn prepare_item(
    home: &Path,
    entry: &registry::RepoEntry,
    canon: &Path,
    existing: Option<&Connection>,
    item: IngestItem,
) -> Result<Prepared, Fail> {
    let IngestItem {
        doc,
        section_id,
        section_digest,
        record_id,
        kind,
        author,
        expected_revision,
        bindings,
    } = item;
    semantic::validate_snapshot_rel(&doc)
        .map_err(|m| (2, format!("invalid doc path '{doc}': {m}")))?;
    if !is_markdown(&doc) {
        return Err((2, format!("doc must be a Markdown file: '{doc}'")));
    }
    let raw = semantic::confined_read_classified(canon, &doc, scan::MAX_FILE_BYTES)
        .map_err(|_| section_changed(&doc, &section_id))?;
    let text = String::from_utf8(raw).map_err(|_| section_changed(&doc, &section_id))?;
    let sections = parse_document(&doc, &text);
    let section = sections
        .iter()
        .find(|s| s.section_id == section_id)
        .ok_or_else(|| section_changed(&doc, &section_id))?;
    if section.digest != section_digest {
        return Err(section_changed(&doc, &section_id));
    }
    if let Some(reason) = ingest_refusal(&doc, section) {
        return Err((2, format!("section not ingestable: {reason}")));
    }
    let title = match &section.heading {
        Some(heading) => heading.clone(),
        None => doc.rsplit('/').next().unwrap_or(&doc).to_string(),
    };
    let body = text
        .get(section.byte_start..section.byte_end)
        .ok_or_else(|| section_changed(&doc, &section_id))?
        .to_string();
    if kind != "decision" && kind != "constraint" {
        return Err((2, format!("invalid record kind: '{kind}'")));
    }
    if author.is_empty() || author.len() > knowledge::MAX_AUTHOR_BYTES {
        return Err((
            2,
            format!("author must be 1..={} bytes", knowledge::MAX_AUTHOR_BYTES),
        ));
    }
    knowledge::validate_id(&record_id).map_err(|m| (2, m))?;
    match expected_revision {
        None => knowledge::validate_new(&knowledge::NewRecord {
            id: record_id.clone(),
            kind: kind.clone(),
            title: title.clone(),
            body: body.clone(),
            status: "active".to_string(),
            author: author.clone(),
        })
        .map_err(|m| (2, m))?,
        Some(_) => knowledge::validate_update(&knowledge::UpdateRecord {
            title: title.clone(),
            body: body.clone(),
            status: "active".to_string(),
            author: author.clone(),
            superseded_by: None,
            reason: None,
            confirm: false,
        })
        .map_err(|m| (2, m))?,
    }
    match (expected_revision, existing) {
        (None, Some(conn)) => {
            if head_revision(conn, &record_id)?.is_some() {
                return Err((2, format!("duplicate record id: '{record_id}'")));
            }
        }
        (None, None) => {}
        (Some(expected), existing) => {
            let head = match existing {
                Some(conn) => head_revision(conn, &record_id)?,
                None => None,
            };
            let Some(head) = head else {
                return Err((3, format!("unknown record id: '{record_id}'")));
            };
            if head != expected {
                return Err((
                    3,
                    format!("revision conflict for '{record_id}': expected {expected} but head is {head}"),
                ));
            }
            if let Some(conn) = existing {
                let status = stored_status(conn, &record_id, head)?;
                if status != "active" {
                    return Err((2, format!("record-not-active: {record_id} is {status}")));
                }
                let stored = stored_kind(conn, &record_id, head)?;
                if stored != kind {
                    return Err((2, format!("kind-mismatch: {record_id} is {stored}")));
                }
            }
        }
    }
    let provenance = format!("docs-ingest:{doc}#{section_id}@{section_digest}");
    let mut seen = BTreeSet::new();
    let mut prepared = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let placeholder = knowledge_binding::BindingInput {
            record_id: record_id.clone(),
            record_revision: 1,
            relation: binding.relation.clone(),
            author: author.clone(),
            rationale: binding.rationale.clone(),
            provenance: provenance.clone(),
        };
        knowledge_binding::validate_binding_input(&placeholder).map_err(|m| (2, m))?;
        let anchor = prove_binding(home, entry, canon, &binding.target)?;
        let key = (
            anchor.path.clone(),
            anchor.byte_start,
            anchor.byte_end,
            anchor.source_digest.clone(),
            binding.relation.clone(),
        );
        if !seen.insert(key) {
            return Err((2, format!("duplicate code binding in item {record_id}")));
        }
        prepared.push(PreparedBinding {
            anchor,
            relation: binding.relation,
            rationale: binding.rationale,
            provenance: provenance.clone(),
        });
    }
    Ok(Prepared {
        record_id,
        kind,
        author,
        expected_revision,
        title,
        body,
        doc,
        section_id,
        section_digest,
        byte_start: section.byte_start,
        byte_end: section.byte_end,
        bindings: prepared,
    })
}

/// Writes one item in its own transaction (D6, step 2). Returns the rendered item and the
/// generation after its commit.
fn write_item(
    conn: &mut Connection,
    canon: &Path,
    item: &Prepared,
) -> Result<(String, u64, u64), Fail> {
    knowledge_store::revalidate_open_paths(conn).map_err(storage)?;
    let evidence = [knowledge::EvidenceRef {
        path: item.doc.clone(),
        byte_start: Some(item.byte_start as i64),
        byte_end: Some(item.byte_end as i64),
        expected_digest: Some(item.section_digest.clone()),
    }];
    let captured = knowledge_store::capture_evidence(conn, &evidence).map_err(|e| {
        if e.contains("digest changed") || e.contains("binds no bytes") {
            section_changed(&item.doc, &item.section_id)
        } else {
            (3, e)
        }
    })?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| storage(format!("cannot begin knowledge write: {e}")))?;
    knowledge_store::ensure_lifecycle_schema(&tx).map_err(storage)?;
    if !item.bindings.is_empty() {
        knowledge_store::ensure_code_binding_schema(&tx).map_err(storage)?;
    }
    let (revision, action) = match item.expected_revision {
        None => {
            let record = knowledge::NewRecord {
                id: item.record_id.clone(),
                kind: item.kind.clone(),
                title: item.title.clone(),
                body: item.body.clone(),
                status: "active".to_string(),
                author: item.author.clone(),
            };
            let rev = knowledge_store::create_in_tx(&tx, &record, None, &captured)
                .map_err(|e| (knowledge_binding::write_error_code(&e), e))?;
            (rev, "created")
        }
        Some(expected) => {
            let update = knowledge::UpdateRecord {
                title: item.title.clone(),
                body: item.body.clone(),
                status: "active".to_string(),
                author: item.author.clone(),
                superseded_by: None,
                reason: None,
                confirm: false,
            };
            match knowledge_store::update_in_tx(
                &tx,
                &item.record_id,
                expected,
                &update,
                None,
                &captured,
            )
            .map_err(|e| (knowledge_binding::write_error_code(&e), e))?
            {
                knowledge_store::UpdateOutcome::Written(rev) => (rev, "updated"),
                knowledge_store::UpdateOutcome::Unchanged(_) => {
                    return Err((
                        3,
                        "internal: docs ingest re-confirmed without a write".to_string(),
                    ))
                }
            }
        }
    };
    let mut binding_ids = Vec::with_capacity(item.bindings.len());
    for b in &item.bindings {
        let input = knowledge_binding::BindingInput {
            record_id: item.record_id.clone(),
            record_revision: revision,
            relation: b.relation.clone(),
            author: item.author.clone(),
            rationale: b.rationale.clone(),
            provenance: b.provenance.clone(),
        };
        let id = knowledge_binding::insert_in_tx(&tx, canon, &b.anchor, &input)
            .map_err(|e| (knowledge_binding::write_error_code(&e), e))?;
        binding_ids.push(id.to_string());
    }
    // A store without the binding tables has no earlier bindings to count.
    let has_bindings: i64 = tx
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'code_bindings'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| storage(format!("cannot read store tables: {e}")))?;
    let earlier: i64 = if has_bindings == 0 {
        0
    } else {
        tx.query_row(
            "SELECT count(*) FROM code_bindings b
             WHERE b.record_id = ?1 AND b.record_revision < ?2
               AND NOT EXISTS (SELECT 1 FROM code_binding_retractions r WHERE r.binding_id = b.binding_id)",
            rusqlite::params![item.record_id, revision as i64],
            |row| row.get(0),
        )
        .map_err(|e| storage(format!("cannot count earlier bindings: {e}")))?
    };
    let generation = knowledge_store::bump_generation(&tx).map_err(storage)?;
    tx.commit()
        .map_err(|e| storage(format!("cannot commit knowledge write: {e}")))?;
    let digest = &captured[0].digest;
    let git_head = match &captured[0].git_head {
        Some(head) => format!("\"{}\"", crate::json::escape(head)),
        None => "null".to_string(),
    };
    let rendered = format!(
        "{{\"record_id\":\"{}\",\"revision\":{},\"action\":\"{}\",\"evidence\":{{\"path\":\"{}\",\"byte_start\":{},\"byte_end\":{},\"digest\":\"{}\",\"git_head\":{}}},\"bindings\":[{}],\"earlier_revision_bindings\":{}}}",
        crate::json::escape(&item.record_id),
        revision,
        action,
        crate::json::escape(&item.doc),
        item.byte_start,
        item.byte_end,
        crate::json::escape(digest),
        git_head,
        binding_ids.join(","),
        earlier,
    );
    Ok((rendered, generation, revision))
}

/// Writes every prepared item, one transaction each, and returns the rendered items with
/// the last generation. A failure names the items already committed.
fn write_all(
    home: &Path,
    entry: &registry::RepoEntry,
    canon: &Path,
    prepared: &[Prepared],
) -> Result<(Vec<String>, u64), Fail> {
    let mut conn = knowledge_store::open_knowledge_at(home, entry, true).map_err(storage)?;
    let mut rendered = Vec::with_capacity(prepared.len());
    let mut committed: Vec<String> = Vec::new();
    let mut generation = 0;
    for item in prepared {
        match write_item(&mut conn, canon, item) {
            Ok((json, gen, revision)) => {
                rendered.push(json);
                generation = gen;
                committed.push(format!("{}@{}", item.record_id, revision));
            }
            Err((code, message)) => {
                let message = if committed.is_empty() {
                    message
                } else {
                    format!("{message}; committed: {}", committed.join(", "))
                };
                return Err((code, message));
            }
        }
    }
    Ok((rendered, generation))
}

/// `riltra docs ingest --repo <name> --input -` (D6). Writes candidate records and their
/// code bindings; nothing is confirmed or verified.
pub fn ingest(home: &Path, repo: &str, root: &Path, input: &[u8]) -> Result<String, Fail> {
    let items = parse_ingest(input)?;
    let canon = paths::canonical_root(root).map_err(storage)?;
    let entry = registry::RepoEntry {
        name: repo.to_string(),
        root: root.to_string_lossy().into_owned(),
        added_unix: 0,
    };
    let existing = existing_store(home, &entry)?;
    let mut prepared = Vec::with_capacity(items.len());
    for item in items {
        prepared.push(prepare_item(home, &entry, &canon, existing.as_ref(), item)?);
    }
    drop(existing);
    let (rendered, generation) = write_all(home, &entry, &canon, &prepared)?;
    let result = format!(
        "{{\"items\":[{}],\"confirm_state\":\"candidate\",\"verification\":\"Unverified\"}}",
        rendered.join(",")
    );
    Ok(response::knowledge_envelope(
        repo,
        generation,
        "docs-ingest",
        &result,
        None,
    ))
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn doc_out(path: &str) -> DocOut {
        DocOut {
            path: path.to_string(),
            status: "ok",
            reason: None,
            bytes: Some(1),
            digest: Some("d".repeat(64)),
            git_head: None,
            git_branch: None,
            link_counts: BTreeMap::new(),
            sections: Vec::new(),
            stale_records: Vec::new(),
        }
    }

    fn link(n: usize) -> LinkOut {
        LinkOut {
            line: n,
            text: "x".repeat(200),
            target: "pkg/calc.py".to_string(),
            path: Some("pkg/calc.py".to_string()),
            fragment: None,
            target_line: None,
            target_end_line: None,
            status: "file",
            reason: None,
            exists: None,
            definition: None,
            anchor: None,
            candidates: Vec::new(),
        }
    }

    fn section(id: usize, links: usize) -> SectionOut {
        SectionOut {
            section_id: format!("S{id}@1"),
            heading: Some(format!("S{id}")),
            level: 1,
            line: 1,
            byte_start: 0,
            byte_end: 10,
            bytes: 10,
            digest: "e".repeat(64),
            ingest_refusal: None,
            recorded: None,
            links: (0..links).map(link).collect(),
        }
    }

    fn report(documents: Vec<DocOut>) -> Report {
        Report {
            schema_version: SCHEMA_VERSION,
            repo: "r".to_string(),
            snapshot: None,
            index_freshness: Freshness {
                status: "fresh".to_string(),
                reasons: Vec::new(),
            },
            knowledge: KnowledgeOut {
                status: "no-store".to_string(),
                reason: None,
                generation: None,
            },
            documents,
            proposals: Vec::new(),
            omissions: Vec::new(),
            next_actions: Vec::new(),
            bounds: Bounds {
                documents: MAX_DOCUMENTS,
                sections_per_document: MAX_SECTIONS,
                links_per_section: MAX_LINKS,
                proposals: MAX_PROPOSALS,
                stale_records_per_document: MAX_STALE,
                candidates_per_link: MAX_CANDIDATES,
                report_bytes: MAX_REPORT_BYTES,
            },
            disclaimer: DISCLAIMER,
        }
    }

    /// The D8 removal order, by list name.
    const LISTS: [&str; 5] = [
        "links",
        "proposals",
        "sections",
        "stale_records",
        "documents",
    ];

    fn proposal(i: usize) -> Proposal {
        let symbol = "scip-kt test pkg 1.0.0 pkg/calc.py/add().".to_string();
        Proposal {
            doc: format!("docs/d{i}.md"),
            section_id: "S0@1".to_string(),
            section_digest: "e".repeat(64),
            suggested_record_id: format!("doc-d{i}-s0-0123abcd"),
            target: TargetOut {
                path: "pkg/calc.py".to_string(),
                symbol: Some(symbol.clone()),
                definition: DefOut {
                    name: Some("add".to_string()),
                    kind: "function".to_string(),
                    line: 1,
                    col: 5,
                    byte_start: 0,
                    byte_end: 30,
                },
                anchor: AnchorOut {
                    kind: "semantic",
                    precision: "exact".to_string(),
                    line: 1,
                    col: 5,
                    byte_start: 4,
                    byte_end: 7,
                    symbol_uri: Some(symbol),
                    display_name: Some("add".to_string()),
                    source_digest: "f".repeat(64),
                },
            },
            relation: "documents",
            state: "proposed",
            ingest_refusal: None,
        }
    }

    fn stale(d: usize) -> StaleOut {
        StaleOut {
            id: format!("record-{d}"),
            revision: 1,
            kind: "decision".to_string(),
            title: "a stored record title".to_string(),
            status: "active".to_string(),
            evidence: vec![EvidenceOut {
                byte_start: Some(0),
                byte_end: Some(10),
                freshness: "stale",
            }],
        }
    }

    /// Three documents, two sections of two links each, a proposal per document, a stale
    /// record in the last two documents, and four next actions.
    fn base() -> Report {
        let mut documents = Vec::new();
        for d in 0..3 {
            let mut doc = doc_out(&format!("docs/d{d}.md"));
            doc.sections = vec![section(0, 2), section(1, 2)];
            if d > 0 {
                doc.stale_records = vec![stale(d)];
            }
            documents.push(doc);
        }
        let mut r = report(documents);
        r.proposals = (0..3).map(proposal).collect();
        r.next_actions = (0..4)
            .map(|i| NextAction {
                reason: "a next action",
                command: Some(format!("riltra step {i}")),
            })
            .collect();
        r
    }

    /// The state after the first `stages` lists of D8 are emptied (stage 5 keeps only the first
    /// document and three next actions), with the omission rows that emptying records.
    fn done(stages: usize) -> (Report, Omissions) {
        let mut r = base();
        let mut om = Omissions::default();
        let links: usize = r
            .documents
            .iter()
            .flat_map(|d| d.sections.iter())
            .map(|s| s.links.len())
            .sum();
        let sections: usize = r.documents.iter().map(|d| d.sections.len()).sum();
        let stale_total: usize = r.documents.iter().map(|d| d.stale_records.len()).sum();
        if stages >= 1 {
            for doc in r.documents.iter_mut() {
                for s in doc.sections.iter_mut() {
                    s.links.clear();
                }
            }
            om.add("links", "json_budget", links);
        }
        if stages >= 2 {
            om.add("proposals", "json_budget", r.proposals.len());
            r.proposals.clear();
        }
        if stages >= 3 {
            for doc in r.documents.iter_mut() {
                doc.sections.clear();
            }
            om.add("sections", "json_budget", sections);
        }
        if stages >= 4 {
            for doc in r.documents.iter_mut() {
                doc.stale_records.clear();
            }
            om.add("stale_records", "json_budget", stale_total);
        }
        if stages >= 5 {
            om.add("documents", "json_budget", r.documents.len() - 1);
            r.documents.truncate(1);
            r.next_actions.truncate(3);
        }
        r.omissions = om.rows();
        (r, om)
    }

    /// The expected report when the budget forces exactly one removal from list `step`
    /// (1-based), starting from the state where the earlier lists are already empty.
    fn expected_step(step: usize) -> Report {
        let (mut r, mut om) = done(step - 1);
        match step {
            1 => {
                let doc = r
                    .documents
                    .iter_mut()
                    .rev()
                    .find(|d| d.sections.iter().any(|s| !s.links.is_empty()))
                    .unwrap();
                let section = doc
                    .sections
                    .iter_mut()
                    .rev()
                    .find(|s| !s.links.is_empty())
                    .unwrap();
                section.links.pop();
            }
            2 => {
                r.proposals.pop();
            }
            3 => {
                let doc = r
                    .documents
                    .iter_mut()
                    .rev()
                    .find(|d| !d.sections.is_empty())
                    .unwrap();
                doc.sections.pop();
            }
            4 => {
                let doc = r
                    .documents
                    .iter_mut()
                    .rev()
                    .find(|d| !d.stale_records.is_empty())
                    .unwrap();
                doc.stale_records.pop();
            }
            _ => {
                r.documents.pop();
            }
        }
        om.add(LISTS[step - 1], "json_budget", 1);
        r.omissions = om.rows();
        r
    }

    #[test]
    fn json_budget_trims_in_fixed_order() {
        // Each step in turn is the one that shrinks: the budget is one byte under the state
        // where every earlier list is empty, so exactly one item of that list goes.
        for step in 1..=5 {
            let (before, _) = done(step - 1);
            let budget = json_len(&before).unwrap() - 1;
            let fitted = fit_with(base(), Omissions::default(), budget).unwrap();
            let expected = expected_step(step);
            assert_eq!(
                serde_json::to_string(&fitted).unwrap(),
                serde_json::to_string(&expected).unwrap(),
                "step {step} ({})",
                LISTS[step - 1]
            );
        }
    }

    #[test]
    fn json_budget_remainder_is_header_and_one_document() {
        let (remainder, _) = done(5);
        let size = json_len(&remainder).unwrap();
        let fitted = fit_with(base(), Omissions::default(), size).unwrap();
        assert_eq!(
            serde_json::to_string(&fitted).unwrap(),
            serde_json::to_string(&remainder).unwrap()
        );
        assert_eq!(fitted.documents.len(), 1);
        assert!(fitted.documents[0].sections.is_empty());
        assert!(fitted.documents[0].stale_records.is_empty());
        assert!(fitted.next_actions.len() <= 3);
        let err = fit_with(base(), Omissions::default(), size - 1)
            .err()
            .unwrap();
        assert_eq!(err, "docs scan report exceeds byte budget");
    }

    #[test]
    fn json_budget_remainder_is_small() {
        let long = format!("docs/{}/{}.md", "d".repeat(180), "p".repeat(40));
        let documents: Vec<DocOut> = (0..200)
            .map(|d| {
                let mut doc = doc_out(&format!("{long}{d}"));
                doc.sections = (0..500).map(|s| section(s, 0)).collect();
                doc
            })
            .collect();
        let fitted = fit_with(report(documents), Omissions::default(), 16_384).unwrap();
        assert!(json_len(&fitted).unwrap() < 16_384);
    }

    #[test]
    fn validate_doc_path_refuses_absent_file() {
        // Pins why `docs scan --path` does not call `validate_doc_path`: it refuses an
        // absent file as an escape, while the scan must report that file as missing.
        let root = std::env::temp_dir();
        assert!(semantic::validate_doc_path(&root, "docs/gone-riltra-test.md").is_err());
        assert!(semantic::validate_snapshot_rel("docs/gone-riltra-test.md").is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge;

    fn path_kind(path: &str) -> LinkKind {
        LinkKind::Path {
            path: path.to_string(),
            fragment: None,
            line: None,
            end_line: None,
        }
    }

    #[test]
    fn sections_split_at_every_heading_level() {
        let text = "# A\nx\n## B\ny\n# C\n";
        let s = parse_document("docs/g.md", text);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].byte_start, s[0].byte_end, s[0].line), (0, 6, 1));
        assert_eq!((s[1].byte_start, s[1].byte_end, s[1].line), (6, 13, 3));
        assert_eq!(s[1].heading_path, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(s[1].level, 2);
        assert_eq!((s[2].byte_start, s[2].byte_end, s[2].line), (13, 17, 5));
        let ids: Vec<&str> = s.iter().map(|x| x.section_id.as_str()).collect();
        assert_eq!(ids, vec!["A@1", "A / B@1", "C@1"]);
    }

    #[test]
    fn preamble_only_when_not_blank() {
        let blank = parse_document("docs/g.md", "\n\n# A\n");
        assert_eq!(blank.len(), 1);
        assert_eq!(blank[0].section_id, "A@1");

        let intro = parse_document("docs/g.md", "intro\n# A\n");
        assert_eq!(intro.len(), 2);
        assert_eq!(intro[0].section_id, "(preamble)@1");
        assert_eq!(intro[0].level, 0);
        assert_eq!(intro[0].heading, None);
        assert_eq!(intro[0].line, 1);
    }

    #[test]
    fn section_ids_are_unique_for_duplicate_headings() {
        let s = parse_document("docs/g.md", "# A\n## Notes\nx\n## Notes\n");
        let ids: Vec<&str> = s.iter().map(|x| x.section_id.as_str()).collect();
        assert_eq!(ids, vec!["A@1", "A / Notes@1", "A / Notes@2"]);
    }

    #[test]
    fn parser_ignores_fenced_code_images_and_prose_names() {
        let text = r#"# Doc
```md
# not
[x](a.rs#L1)
```
~~~
# also not
[y](a.rs#L2)
~~~
Inline `[z](a.rs#L9)` here
![i](a.rs)
[r][1]
https://e.x
prose parse_input
[ok](src/a.rs#L3)
"#;
        let s = parse_document("README.md", text);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].links.len(), 1);
        match &s[0].links[0].kind {
            LinkKind::Path { path, line, .. } => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(*line, Some(3));
            }
            other => panic!("expected a path link, got {other:?}"),
        }
        assert!(s[0].links.iter().all(|l| !l.target.contains("e.x")));
    }

    #[test]
    fn link_targets_resolve_lexically() {
        let text = "# Links\n\
            [a](../../src/a.rs)\n\
            [b](/src/b.rs#L2-L5)\n\
            [c](../../../o.rs)\n\
            [d](#top)\n\
            [e](mailto:x@y)\n\
            [f](<src/c d.rs>)\n\
            [g](src/a.rs#intro)\n\
            [h](src/a.rs#L0)\n";
        let s = parse_document("docs/guide/x.md", text);
        assert_eq!(s.len(), 1);
        let l = &s[0].links;
        assert_eq!(l.len(), 8);
        assert_eq!(l[0].kind, path_kind("src/a.rs"));
        assert_eq!(
            l[1].kind,
            LinkKind::Path {
                path: "src/b.rs".to_string(),
                fragment: None,
                line: Some(2),
                end_line: Some(5),
            }
        );
        assert_eq!(l[1].target, "/src/b.rs#L2-L5");
        assert_eq!(l[2].kind, LinkKind::OutsideRepo);
        assert_eq!(l[3].kind, LinkKind::SameDocument);
        assert_eq!(l[4].kind, LinkKind::External);
        assert_eq!(l[5].target, "src/c d.rs");
        assert_eq!(l[5].kind, path_kind("docs/guide/src/c d.rs"));
        assert_eq!(
            l[6].kind,
            LinkKind::Path {
                path: "docs/guide/src/a.rs".to_string(),
                fragment: Some("intro".to_string()),
                line: None,
                end_line: None,
            }
        );
        assert_eq!(
            l[7].kind,
            LinkKind::Path {
                path: "docs/guide/src/a.rs".to_string(),
                fragment: Some("L0".to_string()),
                line: None,
                end_line: None,
            }
        );
    }

    #[test]
    fn crlf_spans_exclude_carriage_returns_from_text() {
        let text = "# A\r\nx\r\n## B\r\n";
        let s = parse_document("docs/g.md", text);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].heading.as_deref(), Some("A"));
        assert_eq!(s[0].byte_end, 8);
        assert_eq!(text.as_bytes()[8], b'#');
        assert_eq!(s[0].line, 1);
        assert_eq!(s[1].line, 3);
    }

    #[test]
    fn record_id_is_valid_for_non_ascii_heading() {
        let id = suggested_record_id("docs/adr/ä.md", Some("Überblick"), "Überblick@1");
        assert!(knowledge::validate_id(&id).is_ok(), "{id}");
        let long = "x".repeat(600);
        let id = suggested_record_id("docs/adr/a.md", Some(&long), &format!("{long}@1"));
        assert!(id.len() <= 128, "{}", id.len());
        assert!(knowledge::validate_id(&id).is_ok(), "{id}");
        let a = suggested_record_id("docs/a.md", Some("H"), "H@1");
        let b = suggested_record_id("docs/a.md", Some("H"), "H@2");
        assert_ne!(a, b);
    }

    #[test]
    fn record_id_hashes_full_identity_before_cutting() {
        let a = suggested_record_id("docs/a-b.md", Some("Storage"), "Storage@1");
        let b = suggested_record_id("docs/a_b.md", Some("Storage"), "Storage@1");
        assert_ne!(a, b);

        let p1 = format!("docs/{}x.md", "a".repeat(200));
        let p2 = format!("docs/{}y.md", "a".repeat(200));
        let i1 = suggested_record_id(&p1, Some("S"), "S@1");
        let i2 = suggested_record_id(&p2, Some("S"), "S@1");
        assert_ne!(i1, i2);
        for id in [&i1, &i2] {
            assert!(id.len() <= 128, "{}", id.len());
            assert!(knowledge::validate_id(id).is_ok(), "{id}");
            assert!(id[id.len() - 8..].chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}

#[cfg(test)]
mod ingest_tests {
    use super::*;
    use crate::testutil::tmpdir;

    #[test]
    fn ingest_capture_refuses_digest_changed_after_validation() {
        let home = tmpdir("ingest-unit-home");
        let root = tmpdir("ingest-unit-root");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), "# A\nfirst body\n").unwrap();
        let canon = paths::canonical_root(&root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: canon.to_string_lossy().into_owned(),
            added_unix: 1,
        };
        let text = std::fs::read_to_string(root.join("docs/a.md")).unwrap();
        let section = parse_document("docs/a.md", &text).remove(0);
        let prepared = Prepared {
            record_id: "a-rec".to_string(),
            kind: "decision".to_string(),
            author: "tester".to_string(),
            expected_revision: None,
            title: "A".to_string(),
            body: text[section.byte_start..section.byte_end].to_string(),
            doc: "docs/a.md".to_string(),
            section_id: section.section_id.clone(),
            section_digest: section.digest.clone(),
            byte_start: section.byte_start,
            byte_end: section.byte_end,
            bindings: Vec::new(),
        };
        let mut conn = knowledge_store::open_knowledge_at(&home, &entry, true).unwrap();
        let before = knowledge_store::read_generation(&conn).unwrap();
        // The file changes after validation and before the write.
        std::fs::write(root.join("docs/a.md"), "# A\nsecond body\n").unwrap();
        let err = write_item(&mut conn, &canon, &prepared).unwrap_err();
        assert_eq!(err.0, 2, "{}", err.1);
        assert!(err.1.contains("section-changed"), "{}", err.1);
        assert!(knowledge_store::show_record(&conn, "a-rec", None).is_err());
        assert_eq!(knowledge_store::read_generation(&conn).unwrap(), before);
    }
}

#[cfg(test)]
mod ingest_partial_tests {
    use super::*;
    use crate::testutil::tmpdir;

    /// Preflight for one single-section document, as `ingest` runs it.
    fn prepare_for(
        home: &Path,
        root: &Path,
        canon: &Path,
        entry: &registry::RepoEntry,
        doc: &str,
        record: &str,
    ) -> Prepared {
        let text = std::fs::read_to_string(root.join(doc)).unwrap();
        let section = parse_document(doc, &text).remove(0);
        let item = IngestItem {
            doc: doc.to_string(),
            section_id: section.section_id.clone(),
            section_digest: section.digest.clone(),
            record_id: record.to_string(),
            kind: "decision".to_string(),
            author: "tester".to_string(),
            expected_revision: None,
            bindings: Vec::new(),
        };
        prepare_item(home, entry, canon, None, item).unwrap()
    }

    #[test]
    fn write_all_names_committed_items_when_a_later_item_fails() {
        let home = tmpdir("ingest-partial-home");
        let root = tmpdir("ingest-partial-root");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), "# A\nalpha\n").unwrap();
        std::fs::write(root.join("docs/b.md"), "# B\nbravo\n").unwrap();
        let canon = paths::canonical_root(&root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: canon.to_string_lossy().into_owned(),
            added_unix: 1,
        };
        let first = prepare_for(&home, &root, &canon, &entry, "docs/a.md", "a-rec");
        let second = prepare_for(&home, &root, &canon, &entry, "docs/b.md", "b-rec");
        // Item 2's section changes after preflight: its expected digest refuses the capture
        // inside its own write transaction, after item 1 has committed.
        std::fs::write(root.join("docs/b.md"), "# B\nchanged\n").unwrap();
        let err = write_all(&home, &entry, &canon, &[first, second]).unwrap_err();
        assert_eq!(err.0, 2, "{}", err.1);
        assert!(err.1.contains("section-changed"), "{}", err.1);
        assert!(err.1.contains("committed: a-rec@1"), "{}", err.1);
        let conn = knowledge_store::open_knowledge_at(&home, &entry, false).unwrap();
        assert!(knowledge_store::show_record(&conn, "a-rec", None).is_ok());
        assert!(knowledge_store::show_record(&conn, "b-rec", None).is_err());
        assert_eq!(knowledge_store::read_generation(&conn).unwrap(), 1);
    }
}
