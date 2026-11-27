// SPDX-License-Identifier: Apache-2.0
//! M5 SCIP importer: bounded streaming import of `index.scip` payloads into
//! the snapshot-scoped semantic tables. Whole-message parsing is banned:
//! the top-level `Index` is consumed field-by-field (metadata once, then one
//! document at a time) with byte/record/nesting budgets enforced mid-stream.
//! Every failure aborts before commit; the caller owns the transaction.
//!
//! Wire format: canonical `proto/scip.proto` (sourcegraph/scip, fetched
//! 2026-10-16). Bindings in `scip_proto.rs` generated once with
//! `protobuf-codegen 3.7.2` (pure-Rust, developer-side only):
//!   cargo run --manifest-path $TEMP/scip-gen/Cargo.toml
//! (see `src/main.rs` there: `Codegen::new().pure().out_dir(..)`).
//! Regenerate only when the pinned proto changes; never at build time.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use protobuf::{CodedInputStream, Message};

use crate::json::escape;
use crate::scip_proto::{
    Document, Metadata, Occurrence, PositionEncoding, SymbolInformation, TextEncoding,
};
use crate::semantic::{self, Evidence, ProviderId, ProviderSymbol};
use crate::store;

/// Decode budgets. Every one is enforced DURING the streaming loop and
/// recorded in diagnostics; exceeding any aborts the import.
pub struct ImportLimits {
    /// Refuse files larger than this before reading field one.
    pub max_bytes: u64,
    /// Abort after this many documents (also bounds `external_symbols`).
    pub max_documents: usize,
    /// Abort after this many occurrences in total.
    pub max_occurrences: usize,
    /// Abort a document with more occurrences than this.
    pub max_occurrences_per_doc: usize,
    /// Protobuf recursion limit for nested messages.
    pub max_nesting: u32,
    /// Decoded-bytes budget (sum of length-delimited field sizes): the
    /// portable memory proxy. Buffers are released per document.
    pub max_mem_bytes: u64,
}

impl Default for ImportLimits {
    fn default() -> ImportLimits {
        ImportLimits {
            max_bytes: 64 * 1024 * 1024,
            max_documents: 100_000,
            max_occurrences: 5_000_000,
            max_occurrences_per_doc: 200_000,
            max_nesting: 32,
            max_mem_bytes: 512 * 1024 * 1024,
        }
    }
}

pub struct ImportOutcome {
    pub import_id: i64,
    pub documents: usize,
    pub definitions: usize,
    pub references: usize,
    pub skipped: Vec<String>,
}

/// Streaming statistics for the mid-stream-abort proof test.
/// Read by tests; the production path reports counts via ImportOutcome.
#[allow(dead_code)]
pub struct StreamStats {
    pub documents: usize,
    pub occurrences: usize,
}

/// Top-level Index field visitor over any `Read`. Reads field tags
/// (metadata=1, documents=2, external_symbols=3), dispatches sub-messages
/// with `push_limit`, skips unknown fields. The caller processes each
/// document before the next is read: whole-message buffering never happens.
/// One streaming unit: metadata arrives first and exactly once.
pub enum StreamItem {
    Metadata(Metadata),
    Document(Document),
    External(SymbolInformation),
}

/// Top-level Index field visitor over any `Read`. Reads field tags
/// (metadata=1, documents=2, external_symbols=3), dispatches sub-messages
/// with `push_limit`, skips unknown fields. The caller processes each
/// document before the next is read: whole-message buffering never happens.
pub(crate) fn stream_documents<R: Read>(
    reader: R,
    limits: &ImportLimits,
    visit: &mut dyn FnMut(StreamItem) -> Result<(), String>,
) -> Result<StreamStats, String> {
    let mut reader = reader;
    let mut stream = CodedInputStream::new(&mut reader);
    stream.set_recursion_limit(limits.max_nesting);
    let mut seen_metadata = false;
    let mut documents = 0usize;
    let mut occurrences = 0usize;
    let mut externals = 0usize;
    let mut mem: u64 = 0;
    loop {
        if stream.pos() > limits.max_bytes {
            return Err("scip stream exceeds max_bytes".to_string());
        }
        let tag = stream
            .read_raw_tag_or_eof()
            .map_err(|e| format!("scip framing error: {e}"))?;
        let Some(tag) = tag else { break };
        match (tag >> 3, tag & 7) {
            (1, 2) => {
                if seen_metadata || documents > 0 || externals > 0 {
                    return Err("scip metadata must be first and appear once".to_string());
                }
                let mut metadata = Metadata::new();
                read_delimited(&mut stream, &mut mem, limits.max_mem_bytes, |s| {
                    metadata.merge_from(s)
                })?;
                seen_metadata = true;
                visit(StreamItem::Metadata(metadata))?;
            }
            (2, 2) => {
                if !seen_metadata {
                    return Err("scip document before metadata".to_string());
                }
                documents += 1;
                if documents > limits.max_documents {
                    return Err("scip document count exceeds limit".to_string());
                }
                let mut doc = Document::new();
                read_delimited(&mut stream, &mut mem, limits.max_mem_bytes, |s| {
                    doc.merge_from(s)
                })?;
                let n = doc.occurrences.len();
                if n > limits.max_occurrences_per_doc {
                    return Err("scip per-document occurrences exceed limit".to_string());
                }
                occurrences += n;
                if occurrences > limits.max_occurrences {
                    return Err("scip total occurrences exceed limit".to_string());
                }
                visit(StreamItem::Document(doc))?;
            }
            (3, 2) => {
                if !seen_metadata {
                    return Err("scip external_symbols before metadata".to_string());
                }
                externals += 1;
                if documents + externals > limits.max_documents {
                    return Err("scip external_symbols exceed limit".to_string());
                }
                let mut info = SymbolInformation::new();
                read_delimited(&mut stream, &mut mem, limits.max_mem_bytes, |s| {
                    info.merge_from(s)
                })?;
                visit(StreamItem::External(info))?;
            }
            _ => skip_wire(&mut stream, tag & 7)?,
        }
    }
    if !seen_metadata {
        return Err("scip index missing metadata".to_string());
    }
    Ok(StreamStats {
        documents,
        occurrences,
    })
}

/// Read one length-delimited sub-message under the decoded-bytes budget.
fn read_delimited(
    stream: &mut CodedInputStream,
    mem: &mut u64,
    cap: u64,
    fill: impl FnOnce(&mut CodedInputStream) -> protobuf::Result<()>,
) -> Result<(), String> {
    let len = stream
        .read_raw_varint64()
        .map_err(|e| format!("scip length error: {e}"))?;
    *mem = mem.saturating_add(len);
    if *mem > cap {
        return Err("scip decoded-bytes budget exceeded".to_string());
    }
    let old = stream
        .push_limit(len)
        .map_err(|e| format!("scip limit error: {e}"))?;
    let result = fill(stream).map_err(|e| format!("scip decode error: {e}"));
    stream.pop_limit(old);
    result
}

/// Skip one field by wire type. Groups (3/4) never occur in proto3 SCIP and
/// are rejected rather than guessed through.
fn skip_wire(stream: &mut CodedInputStream, wire: u32) -> Result<(), String> {
    match wire {
        0 => {
            stream
                .read_raw_varint64()
                .map_err(|e| format!("scip skip error: {e}"))?;
            Ok(())
        }
        1 => skip_bytes(stream, 8),
        2 => {
            let len = stream
                .read_raw_varint64()
                .map_err(|e| format!("scip skip error: {e}"))?;
            let mut left = len;
            while left > 0 {
                let chunk = left.min(1 << 20);
                skip_bytes(stream, chunk as u32)?;
                left -= chunk;
            }
            Ok(())
        }
        5 => skip_bytes(stream, 4),
        _ => Err("scip unsupported wire type".to_string()),
    }
}

fn skip_bytes(stream: &mut CodedInputStream, n: u32) -> Result<(), String> {
    stream
        .skip_raw_bytes(n)
        .map_err(|e| format!("scip skip error: {e}"))
}

/// Character-offset basis of one document's ranges. A specified encoding is
/// honored exactly; Unspecified is rejected. Guessing the producer's string
/// width corrupts every span on the page.
fn char_encoding(doc: &Document) -> Result<CharEncoding, String> {
    match doc.position_encoding.enum_value() {
        Ok(PositionEncoding::UTF8CodeUnitOffsetFromLineStart) => Ok(CharEncoding::Utf8),
        Ok(PositionEncoding::UTF16CodeUnitOffsetFromLineStart) => Ok(CharEncoding::Utf16),
        Ok(PositionEncoding::UTF32CodeUnitOffsetFromLineStart) => Ok(CharEncoding::Utf32),
        _ => Err("unspecified position encoding".to_string()),
    }
}

/// Unspecified-encoding fallback marker: accept ONLY on lines that are
/// ASCII, where UTF-8/16/32 offsets are provably identical (no assumption
/// made). Real producers omit the field (scip-typescript 0.4.0, Tier B):
/// ASCII lines import exactly, non-ASCII lines skip with a reason.
fn ascii_line(line: &str) -> bool {
    line.is_ascii()
}

#[derive(Clone, Copy)]
enum CharEncoding {
    Utf8,
    Utf16,
    Utf32,
}

/// Map a producer character offset to a byte offset within one line.
/// Errors on overshoot and mid-character landings: never round, never clamp.
fn byte_offset(line: &str, char_off: usize, enc: CharEncoding) -> Result<usize, String> {
    match enc {
        CharEncoding::Utf8 => {
            if char_off > line.len() {
                return Err("character offset past line end".to_string());
            }
            if !line.is_char_boundary(char_off) {
                return Err("offset lands mid-character".to_string());
            }
            Ok(char_off)
        }
        CharEncoding::Utf16 => {
            let mut units = 0usize;
            for (i, ch) in line.char_indices() {
                if units == char_off {
                    return Ok(i);
                }
                units += ch.len_utf16();
                if units > char_off {
                    return Err("offset lands mid-surrogate-pair".to_string());
                }
            }
            if units == char_off {
                return Ok(line.len());
            }
            Err("character offset past line end".to_string())
        }
        CharEncoding::Utf32 => {
            let mut count = 0usize;
            for (i, _) in line.char_indices() {
                if count == char_off {
                    return Ok(i);
                }
                count += 1;
            }
            if count == char_off {
                return Ok(line.len());
            }
            Err("character offset past line end".to_string())
        }
    }
}

/// Decoded source with an optional raw-offset map. UTF-8 files map
/// identically (no table); UTF-16 maps every decoded byte to the raw byte
/// index of its character's first unit, so spans index the RAW file —
/// BOM, surrogates, and LE/BE included. Only char-boundary offsets are
/// ever translated (guaranteed by `byte_offset`).
struct DecodedSource {
    text: String,
    raw_map: Option<Vec<usize>>,
}

/// Task 4 CLI positions are 0-based UTF-8 byte columns in decoded text.
/// Reuse the importer decoder so UTF-16 disk bytes (BOM included) keep their
/// original raw offsets; producer occurrence column encodings are irrelevant.
pub(crate) fn query_byte_offset(
    raw: &[u8],
    encoding: &str,
    line0: u32,
    col0: u32,
) -> Result<usize, String> {
    let encoding = match encoding {
        "UTF8" => TextEncoding::UTF8,
        "UTF16" => TextEncoding::UTF16,
        _ => return Err("unsupported source text encoding".to_string()),
    };
    let decoded = decode_source(raw, encoding, "anchor")?;
    let mut start = 0;
    let line = decoded
        .text
        .split('\n')
        .nth(line0 as usize)
        .ok_or("position outside source")?;
    for line in decoded.text.split('\n').take(line0 as usize) {
        start += line.len() + 1;
    }
    let line = line.strip_suffix('\r').unwrap_or(line);
    let col = col0 as usize;
    if col > line.len() || !line.is_char_boundary(col) {
        return Err("position outside source or mid-character".to_string());
    }
    let offset = start + col;
    Ok(match decoded.raw_map {
        Some(map) => map.get(offset).copied().unwrap_or(raw.len()),
        None => offset,
    })
}

/// Decode on-disk bytes per the index `text_document_encoding`. UTF-16
/// requires a BOM (LE `FF FE` / BE `FE FF`); BOM-less or odd-length input
/// is rejected rather than guessed. Digests always cover the RAW bytes
/// (matching the syntax snapshot).
fn decode_source(raw: &[u8], enc: TextEncoding, rel: &str) -> Result<DecodedSource, String> {
    match enc {
        TextEncoding::UTF8 => String::from_utf8(raw.to_vec())
            .map(|text| DecodedSource {
                text,
                raw_map: None,
            })
            .map_err(|_| format!("{rel}: not UTF-8")),
        TextEncoding::UTF16 => {
            let (words, swap, bom) = match raw {
                [0xFF, 0xFE, rest @ ..] => (rest, false, 2usize),
                [0xFE, 0xFF, rest @ ..] => (rest, true, 2usize),
                _ => return Err(format!("UTF-16 without BOM in '{rel}'")),
            };
            if words.len() % 2 != 0 {
                return Err(format!("odd-length UTF-16 in '{rel}'"));
            }
            let units: Vec<u16> = words
                .chunks(2)
                .map(|c| {
                    let v = u16::from_le_bytes([c[0], c[1]]);
                    if swap {
                        v.swap_bytes()
                    } else {
                        v
                    }
                })
                .collect();
            let mut text = String::new();
            let mut raw_map = Vec::new();
            let mut i = 0;
            while i < units.len() {
                let raw_start = bom + i * 2;
                let first = units[i];
                let ch = if (0xD800..0xDC00).contains(&first) {
                    let second = *units
                        .get(i + 1)
                        .ok_or_else(|| format!("dangling surrogate in '{rel}'"))?;
                    if !(0xDC00..0xE000).contains(&second) {
                        return Err(format!("dangling surrogate in '{rel}'"));
                    }
                    i += 2;
                    char::from_u32(
                        0x10000 + (u32::from(first - 0xD800) << 10) + u32::from(second - 0xDC00),
                    )
                    .ok_or_else(|| format!("invalid UTF-16 in '{rel}'"))?
                } else if (0xDC00..0xE000).contains(&first) {
                    return Err(format!("dangling surrogate in '{rel}'"));
                } else {
                    i += 1;
                    char::from_u32(u32::from(first))
                        .ok_or_else(|| format!("invalid UTF-16 in '{rel}'"))?
                };
                let decoded_start = text.len();
                text.push(ch);
                raw_map.extend(std::iter::repeat_n(raw_start, text.len() - decoded_start));
            }
            Ok(DecodedSource {
                text,
                raw_map: Some(raw_map),
            })
        }
        _ => Err(format!("unsupported text encoding in '{rel}'")),
    }
}

/// Parse `Metadata.project_root` through the shared URI decoder.
/// Must canonicalize-equal the registered repo root (checked by caller).
fn parse_project_root(s: &str) -> Result<PathBuf, String> {
    crate::semantic::uri_path(s)
}

/// Split a symbol URI on single spaces; double spaces are escaped literals.
/// Returns `None` for document-scoped `local` symbols and malformed URIs.
/// A `local ...` symbol is document-scoped: definition and reference must
/// share the document path to link. Anything else is a global URI.
pub fn is_local_symbol(symbol: &str) -> bool {
    symbol == "local" || symbol.starts_with("local ")
}

fn split_symbol(symbol: &str) -> Option<Vec<String>> {
    if symbol == "local" || symbol.starts_with("local ") {
        return None;
    }
    let bytes = symbol.as_bytes();
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            if bytes.get(i + 1) == Some(&b' ') {
                cur.push(' ');
                i += 2;
            } else {
                tokens.push(std::mem::take(&mut cur));
                i += 1;
            }
        } else {
            cur.push(bytes[i] as char);
            i += 1;
        }
    }
    tokens.push(cur);
    if tokens.len() < 5 {
        return None; // scheme + 3 package parts + >= 1 descriptor
    }
    Some(tokens)
}

/// External package triple from a global symbol URI, for `ExternalMeta`.
/// `'.'` is the empty placeholder per the symbol grammar.
fn external_meta(symbol: &str) -> Option<semantic::ExternalMeta> {
    let tokens = split_symbol(symbol)?;
    Some(semantic::ExternalMeta {
        package_manager: tokens[1].clone(),
        package: tokens[2].clone(),
        version: tokens[3].clone(),
    })
}

/// FNV-1a reader wrapper: digests the exact producer bytes while the
/// streaming parser consumes them. Same constants as `store::content_digest`.
struct HashRead<R: Read> {
    inner: R,
    hash: u64,
}

impl<R: Read> HashRead<R> {
    fn new(inner: R) -> HashRead<R> {
        HashRead {
            inner,
            hash: 0xcbf29ce484222325,
        }
    }

    fn digest(&self) -> String {
        format!("{:016x}", self.hash)
    }
}

impl<R: Read> Read for HashRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        for byte in &buf[..n] {
            self.hash ^= *byte as u64;
            self.hash = self.hash.wrapping_mul(0x100000001b3);
        }
        Ok(n)
    }
}

type DefinitionRange = Result<(u32, usize, u32, usize), usize>;

struct ImportState<'a> {
    conn: &'a rusqlite::Connection,
    snapshot_id: i64,
    import_id: i64,
    repo_root: PathBuf,
    canonical_root: PathBuf,
    file_ids: BTreeMap<String, (i64, String)>,
    text_encoding: TextEncoding,
    provider: ProviderId,
    defined_global: BTreeSet<String>,
    /// Definition occurrences of every global symbol in the whole artifact:
    /// `(document, range)`, or the occurrence index when the range is unusable.
    definition_places: BTreeMap<String, BTreeSet<(String, DefinitionRange)>>,
    external_meta: BTreeMap<String, (String, String)>,
    documents: usize,
    definitions: usize,
    references: usize,
    skipped: Vec<String>,
    extra_skips: usize,
}

impl ImportState<'_> {
    fn skip(&mut self, reason: String) {
        if self.skipped.len() < 32 {
            self.skipped.push(reason);
        } else {
            self.extra_skips += 1;
        }
    }
}

/// Resolve one occurrence range: typed form wins, packed `range` is the
/// fallback (3 elements single-line, 4 elements two-line). Anything else is
/// skipped with a reason, never guessed.
fn occurrence_span(occ: &Occurrence) -> Result<(u32, usize, u32, usize), String> {
    if let Some(typed) = occ.typed_range.as_ref() {
        use crate::scip_proto::occurrence::Typed_range;
        return match typed {
            Typed_range::SingleLineRange(r) => Ok((
                r.line as u32,
                r.start_character as usize,
                r.line as u32,
                r.end_character as usize,
            )),
            Typed_range::MultiLineRange(r) => Ok((
                r.start_line as u32,
                r.start_character as usize,
                r.end_line as u32,
                r.end_character as usize,
            )),
        };
    }
    match occ.range.as_slice() {
        [l, c1, c2] => Ok((*l as u32, *c1 as usize, *l as u32, *c2 as usize)),
        [l1, c1, l2, c2] => Ok((*l1 as u32, *c1 as usize, *l2 as u32, *c2 as usize)),
        _ => Err("occurrence has no usable range".to_string()),
    }
}

fn relationships_json(relationships: &[semantic::Relationship]) -> String {
    if relationships.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::from("[");
    for (i, rel) in relationships.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"target\":\"{}\",\"is_reference\":{},\"is_implementation\":{},\"is_type_definition\":{},\"is_definition\":{}}}",
            escape(&rel.target),
            rel.is_reference,
            rel.is_implementation,
            rel.is_type_definition,
            rel.is_definition
        ));
    }
    out.push(']');
    out
}

/// The single Evidence→row conversion: every stored fact passes through the
/// shared contract type, never around it.
fn evidence_to_fact(
    snapshot_id: i64,
    import_id: i64,
    file_id: Option<i64>,
    rel: &str,
    digest: &str,
    evidence: &Evidence,
) -> store::SemanticFact {
    let span = &evidence.location.span;
    store::SemanticFact {
        snapshot_id,
        import_id,
        file_id,
        rel_path: rel.to_string(),
        file_digest: digest.to_string(),
        symbol_uri: evidence.symbol.as_ref().map(|s| s.uri.clone()),
        display_name: evidence
            .symbol
            .as_ref()
            .and_then(|s| s.display_name.clone()),
        roles: evidence.roles.raw,
        start_line: span.start_line,
        start_col: span.start_col,
        end_line: span.end_line,
        end_col: span.end_col,
        byte_start: span.byte_start,
        byte_end: span.byte_end,
        // M5 honesty rule: never Exact without producer attestation.
        precision: semantic::Precision::Heuristic.as_str().to_string(),
        relationships: relationships_json(&evidence.relationships),
        external: evidence.external.is_some(),
    }
}

/// Per-document outcome: processed (with def/ref counts) or skipped.
enum DocOutcome {
    Done {
        definitions: usize,
        references: usize,
    },
    Skipped,
}

fn process_document(state: &mut ImportState, doc: Document) -> Result<DocOutcome, String> {
    let rel = semantic::validate_doc_path(&state.repo_root, &doc.relative_path)?;
    // Counted before anything can skip the document: a second definition in a
    // skipped file still makes the symbol ambiguous.
    for (index, occ) in doc.occurrences.iter().enumerate() {
        if occ.symbol_roles & 0x1 != 0 && !occ.symbol.is_empty() && !is_local_symbol(&occ.symbol) {
            state
                .definition_places
                .entry(occ.symbol.clone())
                .or_default()
                .insert((rel.clone(), occurrence_span(occ).map_err(|_| index)));
        }
    }
    // Conflict check first: contradictory identity fails the whole import.
    let mut symbols: BTreeMap<&str, &SymbolInformation> = BTreeMap::new();
    for info in &doc.symbols {
        if info.symbol.is_empty() {
            continue;
        }
        if let Some(prev) = symbols.get(info.symbol.as_str()) {
            if prev.display_name != info.display_name {
                return Err(format!(
                    "conflicting duplicate symbol '{}': '{}' vs '{}'",
                    info.symbol, prev.display_name, info.display_name
                ));
            }
            continue;
        }
        symbols.insert(info.symbol.as_str(), info);
    }
    let raw =
        match crate::semantic::confined_read(&state.repo_root, &rel, crate::scan::MAX_FILE_BYTES) {
            Ok(raw) => raw,
            Err(e) => {
                state.skip(format!("{rel}: {e}"));
                return Ok(DocOutcome::Skipped);
            }
        };
    let decoded = match decode_source(&raw, state.text_encoding, &rel) {
        Ok(decoded) => decoded,
        Err(e) => {
            state.skip(format!("{rel}: {e}"));
            return Ok(DocOutcome::Skipped);
        }
    };
    let digest = store::content_digest(&raw);
    let file_id = match state.file_ids.get(&rel) {
        None => None,
        Some((id, snapshot_digest)) if snapshot_digest == &digest => Some(*id),
        Some(_) => {
            state.skip(format!("{rel}: disk differs from snapshot digest"));
            return Ok(DocOutcome::Skipped);
        }
    };
    let lines: Vec<&str> = decoded.text.split('\n').collect();
    let (enc, unspecified) = match char_encoding(&doc) {
        Ok(enc) => (enc, false),
        Err(_) => (CharEncoding::Utf8, true),
    };
    // Translate decoded-text offsets to raw-file offsets. UTF-8 maps
    // identically; UTF-16 walks the boundary table (BOM/surrogates shift
    // every span past them). The table covers exactly the decoded bytes;
    // offset == len means end-of-text and maps to end-of-raw.
    let to_raw = |off: usize| -> usize {
        match &decoded.raw_map {
            Some(map) if off < map.len() => map[off],
            Some(_) => raw.len(),
            None => off,
        }
    };
    let mut line_starts = Vec::with_capacity(lines.len());
    let mut decoded_starts = Vec::with_capacity(lines.len());
    let mut offset = 0usize;
    for line in &lines {
        line_starts.push(to_raw(offset));
        decoded_starts.push(offset);
        offset += line.len() + 1;
    }
    let mut facts = Vec::new();
    let mut definitions = 0usize;
    let mut references = 0usize;
    for occ in &doc.occurrences {
        let (sl, sc, el, ec) = match occurrence_span(occ) {
            Ok(span) => span,
            Err(e) => {
                state.skip(format!("{rel}: {e}"));
                continue;
            }
        };
        if sl as usize >= lines.len() || el as usize >= lines.len() {
            state.skip(format!("{rel}: range past end of file"));
            continue;
        }
        let start_line = lines[sl as usize]
            .strip_suffix('\r')
            .unwrap_or(lines[sl as usize]);
        let end_line = lines[el as usize]
            .strip_suffix('\r')
            .unwrap_or(lines[el as usize]);
        if unspecified && !(ascii_line(start_line) && ascii_line(end_line)) {
            state.skip(format!("{rel}: unspecified encoding on non-ASCII line"));
            continue;
        }
        let (byte_start, byte_end) = match (
            byte_offset(start_line, sc, enc),
            byte_offset(end_line, ec, enc),
        ) {
            (Ok(a), Ok(b)) => (
                to_raw(decoded_starts[sl as usize] + a),
                to_raw(decoded_starts[el as usize] + b),
            ),
            _ => {
                state.skip(format!("{rel}: bad character offset"));
                continue;
            }
        };
        if byte_start > byte_end {
            state.skip(format!("{rel}: disordered range"));
            continue;
        }
        if crate::semantic::validate_range(sl, sc as u32, el, ec as u32).is_err() {
            state.skip(format!("{rel}: disordered range"));
            continue;
        }
        if occ.symbol.is_empty() {
            continue; // Highlight-only occurrence: no identity, no fact.
        }
        let global = split_symbol(&occ.symbol);
        if global.is_none() {
            // Document-scoped local: resolvable only within this document.
            if !symbols.contains_key(occ.symbol.as_str()) {
                state.skip(format!("{rel}: dangling local symbol"));
                continue;
            }
        }
        let roles = semantic::Roles::from_scip_bits(occ.symbol_roles as u32);
        if roles.definition {
            definitions += 1;
        } else {
            references += 1;
        }
        let mut relationships = Vec::new();
        if let Some(info) = symbols.get(occ.symbol.as_str()) {
            for proto_rel in &info.relationships {
                relationships.push(semantic::Relationship {
                    target: proto_rel.symbol.clone(),
                    is_reference: proto_rel.is_reference,
                    is_implementation: proto_rel.is_implementation,
                    is_type_definition: proto_rel.is_type_definition,
                    is_definition: proto_rel.is_definition,
                });
            }
        }
        let evidence = Evidence {
            location: semantic::Location {
                path: rel.clone(),
                digest: digest.clone(),
                span: semantic::SpanBytes {
                    start_line: sl,
                    start_col: sc as u32,
                    end_line: el,
                    end_col: ec as u32,
                    byte_start,
                    byte_end,
                },
            },
            symbol: Some(ProviderSymbol {
                uri: occ.symbol.clone(),
                display_name: symbols
                    .get(occ.symbol.as_str())
                    .map(|info| info.display_name.clone()),
            }),
            roles,
            relationships,
            external: None,
            precision: semantic::Precision::Heuristic,
            verification: semantic::Verification::Unverified,
            provider: state.provider.clone(),
        };
        facts.push(evidence_to_fact(
            state.snapshot_id,
            state.import_id,
            file_id,
            &rel,
            &digest,
            &evidence,
        ));
    }
    if !facts.is_empty() {
        store::insert_semantic_facts(state.conn, &facts)?;
    }
    // Register global identities for the post-pass external resolution.
    // Locals stay document-scoped: they never enter the global set.
    for uri in symbols.keys() {
        if split_symbol(uri).is_some() {
            state.defined_global.insert(uri.to_string());
        }
    }
    Ok(DocOutcome::Done {
        definitions,
        references,
    })
}

pub fn import_file(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    repo_root: &Path,
    scip_path: &Path,
    limits: &ImportLimits,
) -> Result<ImportOutcome, String> {
    let file_len = std::fs::metadata(scip_path)
        .map_err(|e| format!("cannot read SCIP file: {e}"))?
        .len();
    if file_len > limits.max_bytes {
        return Err("scip file exceeds max_bytes".to_string());
    }
    // Imports bind the live latest snapshot only: never a stale id.
    let latest = store::latest_snapshot(conn)?
        .ok_or_else(|| "no snapshot to bind the import to".to_string())?;
    if latest.id != snapshot_id {
        return Err("semantic import binds the latest snapshot only".to_string());
    }
    let canonical_root = repo_root
        .canonicalize()
        .map_err(|e| format!("repository root unavailable: {e}"))?;
    let file_ids = store::file_id_map(conn, snapshot_id)?;
    // Insert the import row first so per-document facts can reference it.
    // Verification is Unverified by M5 rule (no producer attestation exists).
    let import_id = store::insert_semantic_import(
        conn,
        &store::SemanticImport {
            snapshot_id,
            provider_name: "scip-import".to_string(),
            provider_version: String::new(),
            capability: "definitions,references".to_string(),
            project_root: repo_root.to_string_lossy().to_string(),
            text_encoding: "UTF8".to_string(),
            artifact_id: String::new(),
            source_revision: format!(
                "head={}|branch={}|dirty={}|untracked={}|cfg={}|nonce={}",
                latest.head.as_deref().unwrap_or("-"),
                latest.branch.as_deref().unwrap_or("-"),
                latest.dirty,
                latest.untracked,
                latest.config_fingerprint,
                latest.nonce
            ),
            claimed_source_digest: String::new(),
            content_identity: store::content_identity(conn, snapshot_id)?,
            external_symbols: "[]".to_string(),
            verification: "Unverified".to_string(),
        },
    )?;
    let mut state = ImportState {
        conn,
        snapshot_id,
        import_id,
        repo_root: repo_root.to_path_buf(),
        canonical_root,
        file_ids,
        text_encoding: TextEncoding::UTF8,
        provider: ProviderId {
            name: "scip-import".to_string(),
            version: String::new(),
        },
        defined_global: BTreeSet::new(),
        definition_places: BTreeMap::new(),
        external_meta: BTreeMap::new(),
        documents: 0,
        definitions: 0,
        references: 0,
        skipped: Vec::new(),
        extra_skips: 0,
    };
    let file = std::fs::File::open(scip_path).map_err(|e| format!("cannot read SCIP file: {e}"))?;
    let mut hashed = HashRead::new(file);
    stream_documents(&mut hashed, limits, &mut |item| match item {
        StreamItem::Metadata(md) => {
            let claimed = parse_project_root(&md.project_root)?;
            let canonical_claimed = claimed
                .canonicalize()
                .map_err(|_| format!("project_root does not resolve: '{}'", md.project_root))?;
            if canonical_claimed != state.canonical_root {
                return Err(format!(
                    "project_root '{}' is not the repository",
                    md.project_root
                ));
            }
            if let Some(tool) = md.tool_info.as_ref() {
                if !tool.name.is_empty() {
                    state.provider.name = tool.name.clone();
                }
                state.provider.version = tool.version.clone();
            }
            state.text_encoding = md
                .text_document_encoding
                .enum_value()
                .map_err(|_| "unknown text encoding".to_string())?;
            Ok(())
        }
        StreamItem::Document(doc) => match process_document(&mut state, doc)? {
            DocOutcome::Done {
                definitions,
                references,
            } => {
                state.documents += 1;
                state.definitions += definitions;
                state.references += references;
                Ok(())
            }
            DocOutcome::Skipped => Ok(()),
        },
        StreamItem::External(info) => {
            if info.symbol.is_empty() || external_meta(&info.symbol).is_none() {
                state.skip("external entry without usable symbol".to_string());
                return Ok(());
            }
            state.external_meta.insert(
                info.symbol.clone(),
                (info.display_name.clone(), String::new()),
            );
            Ok(())
        }
    })?;
    // Post-pass: a global symbol never defined by any document is external.
    // Local symbols were already scoped per document at insert time.
    let distinct: Vec<Option<String>> = conn
        .prepare("SELECT DISTINCT symbol_uri FROM semantic_facts WHERE import_id = ?1")
        .map_err(|e| e.to_string())?
        .query_map([import_id], |row| row.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<Option<String>>, _>>()
        .map_err(|e| e.to_string())?;
    for uri in distinct.into_iter().flatten() {
        if uri.starts_with("local ") || uri == "local" {
            continue;
        }
        if !state.defined_global.contains(&uri) {
            conn.execute(
                "UPDATE semantic_facts SET external = 1 WHERE import_id = ?1 AND symbol_uri = ?2",
                rusqlite::params![import_id, uri],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    // Fill in producer metadata discovered during the stream.
    let external_json = {
        let mut out = String::from("[");
        let mut first = true;
        let mut metas: Vec<(&String, &(String, String))> = state.external_meta.iter().collect();
        metas.sort_by(|a, b| a.0.cmp(b.0));
        for (uri, (display, _)) in metas {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&format!(
                "{{\"symbol\":\"{}\",\"display_name\":\"{}\"}}",
                escape(uri),
                escape(display)
            ));
        }
        out.push(']');
        out
    };
    let artifact_id = hashed.digest();
    let ambiguous: BTreeMap<String, usize> = state
        .definition_places
        .iter()
        .filter(|(_, places)| places.len() > 1)
        .map(|(uri, places)| (uri.clone(), places.len()))
        .collect();
    crate::identity::record(conn, &artifact_id, &ambiguous)?;
    let text_encoding = match state.text_encoding {
        TextEncoding::UTF8 => "UTF8",
        TextEncoding::UTF16 => "UTF16",
        _ => "Unspecified",
    };
    conn.execute(
        "UPDATE semantic_imports SET provider_name = ?1, provider_version = ?2, text_encoding = ?3, artifact_id = ?4, external_symbols = ?5 WHERE id = ?6",
        rusqlite::params![
            state.provider.name,
            state.provider.version,
            text_encoding,
            artifact_id,
            external_json,
            import_id
        ],
    )
    .map_err(|e| e.to_string())?;
    // Identical bytes already on this snapshot revalidate that import in place:
    // the oldest one keeps its id and takes this attempt's facts, and any other
    // duplicate goes. Every edge is re-derived below, so none can dangle.
    let duplicates = store::semantic_import_duplicates(conn, snapshot_id, import_id)?;
    let import_id = match duplicates.split_first() {
        Some((&keep, older)) => {
            for &other in older {
                store::delete_semantic_import(conn, snapshot_id, other)?;
            }
            store::replace_semantic_import(conn, snapshot_id, keep, import_id)?;
            keep
        }
        None => import_id,
    };
    let graph_was_proven = store::graph_proof_current(conn, snapshot_id)?;
    let generation = store::semantic_generation(conn)?;
    store::meta_set(conn, "semantic_generation", &(generation + 1).to_string())?;
    // Task7 source-proof validation may reparse digest-matching source during
    // derivation. Certification shares this transaction with facts and edges;
    // any failure rolls back all of them, including the generation counter.
    if graph_was_proven && duplicates.is_empty() {
        crate::edges::upsert_semantic_edges(conn, snapshot_id, import_id)?;
        store::mark_graph_proof(conn, snapshot_id)?;
    } else {
        // A single refreshed import cannot certify untouched legacy rows.
        crate::edges::rederive_after_carry(conn, snapshot_id)?;
    }
    if state.extra_skips > 0 {
        state.skipped.push(format!("…({} more)", state.extra_skips));
    }
    Ok(ImportOutcome {
        import_id,
        documents: state.documents,
        definitions: state.definitions,
        references: state.references,
        skipped: state.skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::super::scip_proto::occurrence;
    use super::super::scip_proto::{
        Document, Index, Metadata, MultiLineRange, Occurrence, PositionEncoding, SingleLineRange,
        SymbolInformation, TextEncoding, ToolInfo,
    };
    use super::*;
    use crate::store::{
        content_digest, insert_file, insert_snapshot, latest_snapshot, open_memory, FileRow,
        Snapshot,
    };
    use crate::testutil::tmpdir;
    use protobuf::{EnumOrUnknown, Message};

    const SCHEME_PKG: &str = "scip-test test test-pkg 1.0.0";

    fn tool() -> ToolInfo {
        let mut t = ToolInfo::new();
        t.name = "scip-test-producer".to_string();
        t.version = "0.0.1".to_string();
        t
    }

    fn metadata_for(root: &Path) -> Metadata {
        let mut md = Metadata::new();
        md.tool_info = protobuf::MessageField::some(tool());
        md.project_root = format!("file://{}", root.to_string_lossy().replace('\\', "/"));
        md.text_document_encoding = EnumOrUnknown::new(TextEncoding::UTF8);
        md
    }

    fn encode_index(
        root: &Path,
        docs: Vec<Document>,
        externals: Vec<SymbolInformation>,
    ) -> Vec<u8> {
        let mut index = Index::new();
        index.metadata = protobuf::MessageField::some(metadata_for(root));
        index.documents = docs;
        index.external_symbols = externals;
        index.write_to_bytes().unwrap()
    }

    fn single_line(symbol: &str, roles: i32, line: i32, start: i32, end: i32) -> Occurrence {
        let mut occ = Occurrence::new();
        occ.typed_range = Some(occurrence::Typed_range::SingleLineRange(SingleLineRange {
            line,
            start_character: start,
            end_character: end,
            ..Default::default()
        }));
        occ.symbol = symbol.to_string();
        occ.symbol_roles = roles;
        occ
    }

    fn symbol_info(symbol: &str, display: &str) -> SymbolInformation {
        let mut info = SymbolInformation::new();
        info.symbol = symbol.to_string();
        info.display_name = display.to_string();
        info
    }

    fn utf8_doc(rel: &str) -> Document {
        let mut doc = Document::new();
        doc.relative_path = rel.to_string();
        doc.position_encoding =
            EnumOrUnknown::new(PositionEncoding::UTF8CodeUnitOffsetFromLineStart);
        doc
    }

    struct Setup {
        dir: PathBuf,
        conn: rusqlite::Connection,
        snapshot_id: i64,
    }

    /// Repo dir with `files` + snapshot row in an in-memory DB.
    /// `indexed`: (relpath, content) pairs that get syntax `files` rows.
    /// `extra`: pairs created on disk only (no syntax row → NULL file_id).
    fn setup(indexed: &[(&str, &str)], extra: &[(&str, &[u8])]) -> Setup {
        let dir = tmpdir("scip-t4");
        for (rel, content) in indexed {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        for (rel, content) in extra {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        let conn = open_memory().unwrap();
        let mut snap = Snapshot {
            id: 0,
            repo: "t".to_string(),
            root: dir.to_string_lossy().to_string(),
            created_unix: 1,
            head: Some("abc".to_string()),
            branch: Some("main".to_string()),
            dirty: false,
            untracked: 0,
            parser_version: 1,
            grammar_versions: "g".to_string(),
            config_fingerprint: "c".to_string(),
            pre_fingerprint: "p".to_string(),
            post_fingerprint: "p".to_string(),
            status_at_write: "fresh".to_string(),
            nonce: "n1".to_string(),
        };
        let snapshot_id = insert_snapshot(&conn, &snap).unwrap();
        snap.id = snapshot_id;
        for (rel, content) in indexed {
            insert_file(
                &conn,
                snapshot_id,
                &FileRow {
                    path: rel.to_string(),
                    language: "typescript".to_string(),
                    bytes: content.len() as u64,
                    digest: content_digest(content.as_bytes()),
                    errors: 0,
                },
            )
            .unwrap();
        }
        Setup {
            dir,
            conn,
            snapshot_id,
        }
    }

    fn write_scip(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    const A_TS: &str = "export function foo() {}\nfoo();\n";
    const B_TS: &str = "import \"./a\";\nfoo();\n";

    fn foo_symbol() -> String {
        format!("{SCHEME_PKG} src/a.ts/foo().")
    }

    fn ab_docs() -> (Document, Document) {
        let mut a = utf8_doc("src/a.ts");
        a.occurrences = vec![
            single_line(&foo_symbol(), 0x1, 0, 16, 19),
            single_line(&foo_symbol(), 0x0, 1, 0, 3),
        ];
        a.symbols = vec![symbol_info(&foo_symbol(), "foo")];
        let mut b = utf8_doc("src/b.ts");
        b.occurrences = vec![single_line(&foo_symbol(), 0x0, 1, 0, 3)];
        (a, b)
    }

    #[test]
    fn valid_import_counts_and_binds_snapshot() {
        let s = setup(&[("src/a.ts", A_TS), ("src/b.ts", B_TS)], &[]);
        let (a, b) = ab_docs();
        let bytes = encode_index(&s.dir, vec![a, b], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(out.documents, 2);
        assert_eq!(out.definitions, 1);
        assert_eq!(out.references, 2);
        assert!(out.skipped.is_empty());
        let snap = latest_snapshot(&s.conn).unwrap().unwrap();
        assert_eq!(snap.id, s.snapshot_id, "import must not mint snapshots");
        let facts =
            crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &foo_symbol()).unwrap();
        assert_eq!(facts.len(), 3);
        assert!(facts.iter().any(|f| f.file_id.is_some()));
        let def = facts.iter().find(|f| f.roles & 0x1 != 0).unwrap();
        assert_eq!((def.byte_start, def.byte_end), (16, 19));
    }

    #[test]
    fn import_verification_is_unverified_current_unreachable() {
        let s = setup(&[("src/a.ts", A_TS)], &[]);
        let (a, _) = ab_docs();
        let bytes = encode_index(&s.dir, vec![a], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        let row: String = s
            .conn
            .query_row(
                "SELECT verification FROM semantic_imports WHERE id = ?1",
                [out.import_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row, "Unverified");
    }

    #[test]
    fn truncated_file_errors_with_clean_rollback() {
        let s = setup(&[("src/a.ts", A_TS)], &[]);
        let (a, _) = ab_docs();
        let bytes = encode_index(&s.dir, vec![a], vec![]);
        // Every cut corrupts: inside metadata, mid-payload, and last byte.
        // (A single proportional cut is alignment-fragile against the
        // unique tmpdir path embedded in project_root.)
        for cut in [10, bytes.len() / 2, bytes.len() - 1] {
            let mut cut_bytes = bytes.clone();
            cut_bytes.truncate(cut.max(1));
            let path = write_scip(&s.dir, "index.scip", &cut_bytes);
            assert!(
                import_file(
                    &s.conn,
                    s.snapshot_id,
                    &s.dir,
                    &path,
                    &ImportLimits::default()
                )
                .is_err(),
                "cut at {cut} must fail"
            );
        }
        assert_eq!(crate::store::semantic_generation(&s.conn).unwrap(), 0);
    }

    #[test]
    fn traversal_paths_rejected_before_any_write() {
        let s = setup(&[("src/a.ts", A_TS)], &[]);
        let mut evil = utf8_doc("../../evil.py");
        evil.occurrences = vec![single_line(&foo_symbol(), 0x1, 0, 0, 3)];
        let bytes = encode_index(&s.dir, vec![evil], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        assert!(import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default()
        )
        .is_err());
        assert_eq!(crate::store::semantic_generation(&s.conn).unwrap(), 0);
    }

    #[test]
    fn metadata_not_first_is_rejected() {
        // Hand-assembled stream: document field (2) before metadata (1).
        let s = setup(&[("src/a.ts", A_TS)], &[]);
        let (a, _) = ab_docs();
        let mut doc = Document::new();
        doc.relative_path = "src/a.ts".to_string();
        let doc_bytes = doc.write_to_bytes().unwrap();
        let md_bytes = metadata_for(&s.dir).write_to_bytes().unwrap();
        let mut raw = vec![0x12u8];
        raw.extend(varint(doc_bytes.len() as u64));
        raw.extend(&doc_bytes);
        raw.push(0x0Au8);
        raw.extend(varint(md_bytes.len() as u64));
        raw.extend(&md_bytes);
        let _ = a;
        let path = write_scip(&s.dir, "index.scip", &raw);
        assert!(import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default()
        )
        .is_err());
    }

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
        out
    }

    #[test]
    fn unspecified_encoding_ascii_exact_nonscii_skipped() {
        // Producer omits position_encoding (scip-typescript behavior):
        // ASCII lines import exactly; non-ASCII lines skip, never guess.
        let ascii = "foo();\n";
        let mixed = "// caf\u{e9}\nfoo();\n";
        let s = setup(&[("src/a.ts", ascii), ("src/b.ts", mixed)], &[]);
        let sym_a = format!("{SCHEME_PKG} src/a.ts/foo().");
        let sym_b = format!("{SCHEME_PKG} src/b.ts/foo().");
        let mut da = Document::new();
        da.relative_path = "src/a.ts".to_string();
        da.occurrences = vec![single_line(&sym_a, 0x1, 0, 0, 3)];
        da.symbols = vec![symbol_info(&sym_a, "foo")];
        let mut db = Document::new();
        db.relative_path = "src/b.ts".to_string();
        db.occurrences = vec![single_line(&sym_b, 0x1, 0, 3, 7)]; // on the é line
        db.symbols = vec![symbol_info(&sym_b, "foo")];
        // No position_encoding set on either document: Unspecified.
        let bytes = encode_index(&s.dir, vec![da, db], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(out.documents, 2);
        assert_eq!(out.definitions, 1);
        assert!(
            crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym_a)
                .unwrap()
                .len()
                == 1
        );
        assert!(
            crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym_b)
                .unwrap()
                .is_empty()
        );
        assert!(out.skipped.iter().any(|r| r.contains("non-ASCII")));
    }

    #[test]
    fn conflicting_duplicate_symbols_rejected() {
        let s = setup(&[("src/a.ts", A_TS)], &[]);
        let mut a = utf8_doc("src/a.ts");
        a.occurrences = vec![single_line(&foo_symbol(), 0x1, 0, 16, 19)];
        a.symbols = vec![
            symbol_info(&foo_symbol(), "one"),
            symbol_info(&foo_symbol(), "two"),
        ];
        let bytes = encode_index(&s.dir, vec![a], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        assert!(import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default()
        )
        .is_err());
    }

    /// Same bytes, three encodings, non-BMP char before the target on one
    /// line: all three must resolve to identical byte spans.
    #[test]
    fn position_encodings_agree_on_byte_spans() {
        let content = "\u{1F680}foo();\n"; // 🚀 = 4B UTF-8, 2u UTF-16, 1u UTF-32
        for (enc, start, label) in [
            (PositionEncoding::UTF8CodeUnitOffsetFromLineStart, 4, "utf8"),
            (
                PositionEncoding::UTF16CodeUnitOffsetFromLineStart,
                2,
                "utf16",
            ),
            (
                PositionEncoding::UTF32CodeUnitOffsetFromLineStart,
                1,
                "utf32",
            ),
        ] {
            let s = setup(&[("src/e.ts", content)], &[]);
            let mut doc = Document::new();
            doc.relative_path = "src/e.ts".to_string();
            doc.position_encoding = EnumOrUnknown::new(enc);
            let sym = format!("{SCHEME_PKG} src/e.ts/foo().");
            doc.occurrences = vec![single_line(&sym, 0x1, 0, start, start + 3)];
            doc.symbols = vec![symbol_info(&sym, "foo")];
            let bytes = encode_index(&s.dir, vec![doc], vec![]);
            let path = write_scip(&s.dir, "index.scip", &bytes);
            let out = import_file(
                &s.conn,
                s.snapshot_id,
                &s.dir,
                &path,
                &ImportLimits::default(),
            )
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(out.definitions, 1, "{label}");
            let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym).unwrap();
            assert_eq!(facts.len(), 1, "{label}");
            assert_eq!((facts[0].byte_start, facts[0].byte_end), (4, 7), "{label}");
        }
    }

    #[test]
    fn utf16_text_encoding_decodes_on_disk_bytes() {
        // On-disk UTF-16LE with BOM; producer positions in UTF-16 units.
        let text = "foo();\n";
        let mut raw = vec![0xFFu8, 0xFE];
        for u in text.encode_utf16() {
            raw.extend(&u.to_le_bytes());
        }
        let s = setup(&[], &[("src/u.ts", &raw)]);
        // Register the file in the syntax snapshot with the RAW on-disk
        // digest (semantic digests always cover raw bytes, like M4).
        insert_file(
            &s.conn,
            s.snapshot_id,
            &FileRow {
                path: "src/u.ts".to_string(),
                language: "typescript".to_string(),
                bytes: raw.len() as u64,
                digest: content_digest(&raw),
                errors: 0,
            },
        )
        .unwrap();
        let mut doc = Document::new();
        doc.relative_path = "src/u.ts".to_string();
        doc.position_encoding =
            EnumOrUnknown::new(PositionEncoding::UTF16CodeUnitOffsetFromLineStart);
        let sym = format!("{SCHEME_PKG} src/u.ts/foo().");
        doc.occurrences = vec![single_line(&sym, 0x1, 0, 0, 3)];
        doc.symbols = vec![symbol_info(&sym, "foo")];
        let mut index = Index::new();
        let mut md = metadata_for(&s.dir);
        md.text_document_encoding = EnumOrUnknown::new(TextEncoding::UTF16);
        index.metadata = protobuf::MessageField::some(md);
        index.documents = vec![doc];
        let path = write_scip(&s.dir, "index.scip", &index.write_to_bytes().unwrap());
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(out.definitions, 1);
        let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym).unwrap();
        assert_eq!(facts.len(), 1);
        // Raw-file consistency: digest covers raw bytes (BOM included) and
        // spans index raw offsets (decoded "foo" at 0..3 lives at raw 2..8).
        assert_eq!(facts[0].file_digest, content_digest(&raw));
        assert_eq!((facts[0].byte_start, facts[0].byte_end), (2, 8));
    }

    /// Byte offsets index the RAW file, not the decoded text. UTF-16LE with
    /// BOM + a surrogate pair: decoded math and raw math must be told
    /// apart, or every span past the BOM/emoji is wrong on disk.
    #[test]
    fn utf16_raw_offsets_include_bom_and_surrogates() {
        // "a🚀c": raw = BOM(2) + a(2) + rocket-pair(4) + c(2); decoded c = 5.
        let text = "a\u{1F680}c\n";
        let mut raw = vec![0xFFu8, 0xFE];
        for u in text.encode_utf16() {
            raw.extend(&u.to_le_bytes());
        }
        let s = setup(&[], &[("src/e.ts", &raw)]);
        let mut doc = Document::new();
        doc.relative_path = "src/e.ts".to_string();
        doc.position_encoding =
            EnumOrUnknown::new(PositionEncoding::UTF16CodeUnitOffsetFromLineStart);
        let sym = format!("{SCHEME_PKG} src/e.ts/c().");
        // c at UTF-16 units 3..4 (a=1, rocket=2).
        doc.occurrences = vec![single_line(&sym, 0x1, 0, 3, 4)];
        doc.symbols = vec![symbol_info(&sym, "c")];
        let mut index = Index::new();
        let mut md = metadata_for(&s.dir);
        md.text_document_encoding = EnumOrUnknown::new(TextEncoding::UTF16);
        index.metadata = protobuf::MessageField::some(md);
        index.documents = vec![doc];
        let path = write_scip(&s.dir, "index.scip", &index.write_to_bytes().unwrap());
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(out.definitions, 1);
        let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].file_digest, content_digest(&raw));
        assert_eq!(
            (facts[0].byte_start, facts[0].byte_end),
            (8, 10),
            "raw offsets, not decoded (5..6)"
        );
    }

    #[test]
    fn utf16be_raw_offsets_match() {
        let text = "ab\n";
        let mut raw = vec![0xFEu8, 0xFF];
        for u in text.encode_utf16() {
            raw.extend(&u.to_be_bytes());
        }
        let s = setup(&[], &[("src/b.ts", &raw)]);
        let mut doc = Document::new();
        doc.relative_path = "src/b.ts".to_string();
        doc.position_encoding =
            EnumOrUnknown::new(PositionEncoding::UTF16CodeUnitOffsetFromLineStart);
        let sym = format!("{SCHEME_PKG} src/b.ts/ab().");
        doc.occurrences = vec![single_line(&sym, 0x1, 0, 0, 2)];
        doc.symbols = vec![symbol_info(&sym, "ab")];
        let mut index = Index::new();
        let mut md = metadata_for(&s.dir);
        md.text_document_encoding = EnumOrUnknown::new(TextEncoding::UTF16);
        index.metadata = protobuf::MessageField::some(md);
        index.documents = vec![doc];
        let path = write_scip(&s.dir, "index.scip", &index.write_to_bytes().unwrap());
        import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!((facts[0].byte_start, facts[0].byte_end), (2, 6));
    }

    /// A huge source file with a tiny SCIP payload: the document skips on
    /// the size guard before its bytes ever enter RAM. Same cap as syntax.
    #[test]
    fn huge_source_file_skips_document_within_budget() {
        use crate::scan::MAX_FILE_BYTES;
        let s = setup(&[], &[]);
        let big = vec![b'x'; MAX_FILE_BYTES as usize + 1];
        std::fs::write(s.dir.join("big.ts"), &big).unwrap();
        insert_file(
            &s.conn,
            s.snapshot_id,
            &FileRow {
                path: "big.ts".to_string(),
                language: "typescript".to_string(),
                bytes: big.len() as u64,
                digest: content_digest(&big),
                errors: 0,
            },
        )
        .unwrap();
        let sym = format!("{SCHEME_PKG} big.ts/x.");
        let mut doc = utf8_doc("big.ts");
        doc.occurrences = vec![single_line(&sym, 0x1, 0, 0, 1)];
        doc.symbols = vec![symbol_info(&sym, "x")];
        let bytes = encode_index(&s.dir, vec![doc], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(out.definitions, 0);
        assert_eq!(out.documents, 0);
        assert!(
            out.skipped.iter().any(|r| r.contains("too large")),
            "size reason recorded: {:?}",
            out.skipped
        );
        assert!(
            crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unindexed_doc_stores_null_file_id_and_stays_queryable() {
        let s = setup(&[], &[("notes.other", b"data for notes")]);
        let mut doc = utf8_doc("notes.other");
        let sym = format!("{SCHEME_PKG} notes.other/thing`.");
        doc.occurrences = vec![single_line(&sym, 0x1, 0, 0, 4)];
        doc.symbols = vec![symbol_info(&sym, "thing")];
        let bytes = encode_index(&s.dir, vec![doc], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].file_id, None, "no fabricated syntax identity");
        assert_eq!(facts[0].rel_path, "notes.other");
    }

    #[test]
    fn full_role_bitset_roundtrips_verbatim() {
        let s = setup(&[("src/a.ts", A_TS)], &[]);
        let mut a = utf8_doc("src/a.ts");
        // Import|Read|Generated|Test|ForwardDefinition, no Definition bit.
        a.occurrences = vec![single_line(
            &foo_symbol(),
            0x2 | 0x8 | 0x10 | 0x20 | 0x40,
            1,
            0,
            3,
        )];
        let bytes = encode_index(&s.dir, vec![a], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(out.definitions, 0);
        assert_eq!(out.references, 1);
        let facts =
            crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &foo_symbol()).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].roles, 0x2 | 0x8 | 0x10 | 0x20 | 0x40);
    }

    #[test]
    fn external_symbol_metadata_creates_no_facts() {
        let s = setup(&[("src/b.ts", B_TS)], &[]);
        let ext = "scip-typescript npm tslib 2.0.0 lib/helper.ts/foo().".to_string();
        let mut info = SymbolInformation::new();
        info.symbol = ext.clone();
        info.display_name = "foo".to_string();
        let mut b = utf8_doc("src/b.ts");
        b.occurrences = vec![single_line(&ext, 0x0, 1, 0, 3)];
        let bytes = encode_index(&s.dir, vec![b], vec![info]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        // Exactly the one reference occurrence: metadata adds zero rows.
        assert_eq!(out.references, 1);
        let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &ext).unwrap();
        assert_eq!(facts.len(), 1);
        assert!(facts[0].external);
        let row: String = s
            .conn
            .query_row(
                "SELECT verification FROM semantic_imports WHERE id = ?1",
                [out.import_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row, "Unverified");
    }

    /// Streaming proof: over-cap input aborts having consumed strictly less
    /// than the whole payload. Whole-message parsing would read everything.
    /// (`&mut &[u8]` implements `Read`, so the remainder is observable.
    /// The payload deliberately dwarfs the stream's internal buffer so an
    /// abort cannot have pre-read the tail.)
    #[test]
    fn over_cap_aborts_before_consuming_all_input() {
        let dir = tmpdir("scip-stream");
        let mut docs = Vec::new();
        for i in 0..200 {
            let mut doc = utf8_doc(&format!("src/f{i}.ts"));
            doc.occurrences = (0..50)
                .map(|l| single_line(&format!("{SCHEME_PKG} s#L{l}."), 0x0, l, 0, 1))
                .collect();
            docs.push(doc);
        }
        let bytes = encode_index(&dir, docs, vec![]);
        assert!(bytes.len() > 64 * 1024, "proof needs buffer dwarfing");
        let limits = ImportLimits {
            max_occurrences: 60,
            ..ImportLimits::default()
        };
        let mut slice: &[u8] = &bytes;
        let mut seen = 0;
        let err = stream_documents(&mut slice, &limits, &mut |_| {
            seen += 1;
            Ok(())
        });
        assert!(err.is_err());
        assert!(seen < 200, "must abort mid-stream, saw {seen}/200 docs");
        assert!(!slice.is_empty(), "aborted stream must leave unread tail");
    }

    fn packed_occurrence(symbol: &str, roles: i32, range: &[i32]) -> Occurrence {
        let mut occ = Occurrence::new();
        occ.symbol = symbol.to_string();
        occ.symbol_roles = roles;
        occ.range = range.to_vec();
        occ
    }

    /// Deprecated packed ranges decode exactly like typed ones; malformed
    /// shapes skip the occurrence, never the document.
    #[test]
    fn packed_range_forms_decode() {
        let content = "aa\nbb\ncc\n";
        let s = setup(&[("src/p.ts", content)], &[]);
        let sym = format!("{SCHEME_PKG} src/p.ts/x.");
        let mut doc = utf8_doc("src/p.ts");
        doc.occurrences = vec![
            packed_occurrence(&sym, 0x1, &[0, 0, 2]), // single-line packed
            packed_occurrence(&sym, 0x0, &[1, 0, 2, 1]), // two-line packed
            packed_occurrence(&sym, 0x0, &[9, 9]),    // malformed: skipped
        ];
        doc.symbols = vec![symbol_info(&sym, "x")];
        let bytes = encode_index(&s.dir, vec![doc], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        let out = import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(out.definitions, 1);
        assert_eq!(out.references, 1);
        let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym).unwrap();
        assert_eq!(facts.len(), 2);
        let single = facts.iter().find(|f| f.roles & 0x1 != 0).unwrap();
        assert_eq!((single.byte_start, single.byte_end), (0, 2));
        let multi = facts.iter().find(|f| f.roles & 0x1 == 0).unwrap();
        assert_eq!((multi.start_line, multi.end_line), (1, 2));
        assert_eq!((multi.byte_start, multi.byte_end), (3, 7));
    }

    #[test]
    fn multiline_typed_and_empty_ranges() {
        let content = "aa\nbb\n";
        let s = setup(&[("src/m.ts", content)], &[]);
        let sym = format!("{SCHEME_PKG} src/m.ts/x.");
        let mut doc = utf8_doc("src/m.ts");
        let mut multi = Occurrence::new();
        multi.symbol = sym.clone();
        multi.typed_range = Some(occurrence::Typed_range::MultiLineRange(MultiLineRange {
            start_line: 0,
            start_character: 1,
            end_line: 1,
            end_character: 1,
            ..Default::default()
        }));
        let mut empty = Occurrence::new();
        empty.symbol = sym.clone();
        empty.typed_range = Some(occurrence::Typed_range::SingleLineRange(SingleLineRange {
            line: 1,
            start_character: 0,
            end_character: 0,
            ..Default::default()
        }));
        // Multi-line encoding of a single-line span: accepted (spec MUST).
        let mut compact = Occurrence::new();
        compact.symbol = sym.clone();
        compact.typed_range = Some(occurrence::Typed_range::MultiLineRange(MultiLineRange {
            start_line: 0,
            start_character: 0,
            end_line: 0,
            end_character: 2,
            ..Default::default()
        }));
        doc.occurrences = vec![multi, empty, compact];
        doc.symbols = vec![symbol_info(&sym, "x")];
        let bytes = encode_index(&s.dir, vec![doc], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        let facts = crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &sym).unwrap();
        assert_eq!(facts.len(), 3, "empty ranges are allowed occurrences");
        let empty_fact = facts
            .iter()
            .find(|f| f.byte_start == f.byte_end)
            .expect("empty span stored");
        assert_eq!((empty_fact.start_line, empty_fact.byte_start), (1, 3));
    }

    /// Relationships are stored verbatim (including `is_definition`
    /// overrides and `is_reference` closures) and never traversed here.
    #[test]
    fn relationships_stored_verbatim_not_traversed() {
        let s = setup(&[("src/a.ts", A_TS)], &[]);
        let mut a = utf8_doc("src/a.ts");
        a.occurrences = vec![single_line(&foo_symbol(), 0x1, 0, 16, 19)];
        let mut with_rel = SymbolInformation::new();
        with_rel.symbol = foo_symbol();
        with_rel.display_name = "foo".to_string();
        let mut rel = crate::scip_proto::Relationship::new();
        rel.symbol = format!("{SCHEME_PKG} src/base.ts/Animal#sound().");
        rel.is_reference = true;
        rel.is_implementation = true;
        with_rel.relationships = vec![rel];
        a.symbols = vec![with_rel];
        let bytes = encode_index(&s.dir, vec![a], vec![]);
        let path = write_scip(&s.dir, "index.scip", &bytes);
        import_file(
            &s.conn,
            s.snapshot_id,
            &s.dir,
            &path,
            &ImportLimits::default(),
        )
        .unwrap();
        let facts =
            crate::store::semantic_facts_for(&s.conn, s.snapshot_id, &foo_symbol()).unwrap();
        assert_eq!(facts.len(), 1);
        assert!(facts[0].relationships.contains("Animal#sound()"));
        assert!(facts[0].relationships.contains("\"is_reference\":true"));
        assert!(facts[0]
            .relationships
            .contains("\"is_implementation\":true"));
    }
}
