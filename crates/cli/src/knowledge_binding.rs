// SPDX-License-Identifier: Apache-2.0
//! Explicit, revision-pinned links from code anchors to knowledge records.

use std::path::Path;

use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde::Deserialize;

use crate::{index, knowledge, knowledge_store, paths, registry, response, semantic, store};

const MAX_PROVENANCE_BYTES: usize = 4096;
const CODE_RELATIONS: [&str; 6] = [
    "documents",
    "constrains",
    "implements",
    "tests",
    "motivates",
    "observes",
];

#[derive(Debug, Clone)]
pub struct CodeAnchor {
    pub repo: String,
    pub repo_root_digest: String,
    pub snapshot_id: i64,
    pub snapshot_nonce: String,
    pub snapshot_digest: Option<String>,
    pub source_digest: String,
    pub path: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub cursor_line: u32,
    pub cursor_col: u32,
    pub symbol_uri: Option<String>,
    pub display_name: Option<String>,
    pub anchor_kind: String,
    pub precision: String,
}

#[derive(Debug, Clone)]
pub struct BindingInput {
    pub record_id: String,
    pub record_revision: u64,
    pub relation: String,
    pub author: String,
    pub rationale: String,
    pub provenance: String,
}

#[derive(Debug, Clone)]
pub struct RetractInput {
    pub author: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct BindingView {
    pub binding_id: i64,
    pub repo: String,
    pub snapshot_id: i64,
    pub snapshot_nonce: String,
    pub snapshot_digest: String,
    pub source_digest: String,
    pub path: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub cursor_line: u32,
    pub cursor_col: u32,
    pub symbol_uri: Option<String>,
    pub display_name: Option<String>,
    pub anchor_kind: String,
    pub precision: String,
    pub record_id: String,
    pub record_revision: u64,
    pub record_kind: String,
    pub record_title: String,
    pub record_body: String,
    pub record_status: String,
    pub relation: String,
    pub author: String,
    pub rationale: String,
    pub provenance: String,
    pub created_unix: i64,
    pub retracted: bool,
    pub retraction_author: Option<String>,
    pub retraction_reason: Option<String>,
    pub retracted_unix: Option<i64>,
    pub freshness: String,
    pub freshness_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AtPage {
    pub status: &'static str,
    pub unresolved_reason: Option<String>,
    pub path: String,
    pub line: u32,
    pub col: u32,
    pub bindings: Vec<BindingView>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
    pub truncated: bool,
    pub next_offset: Option<usize>,
    pub generation: u64,
    pub schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortRetraction {
    pub author: String,
    pub reason: String,
    pub created_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortCodeBinding {
    pub repo: String,
    pub repo_root_digest: String,
    pub snapshot_id: i64,
    pub snapshot_nonce: String,
    pub snapshot_digest: String,
    pub source_digest: String,
    pub path: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub cursor_line: u32,
    pub cursor_col: u32,
    pub symbol_uri: Option<String>,
    pub display_name: Option<String>,
    pub anchor_kind: String,
    pub precision: String,
    pub record_id: String,
    pub record_revision: u64,
    pub relation: String,
    pub author: String,
    pub rationale: String,
    pub provenance: String,
    pub created_unix: i64,
    pub retraction: Option<PortRetraction>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortCodeBindingLine {
    #[serde(rename = "type")]
    kind: String,
    binding: PortCodeBinding,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn port_binding_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PortCodeBinding> {
    Ok(PortCodeBinding {
        repo: row.get(0)?,
        repo_root_digest: row.get(1)?,
        snapshot_id: row.get(2)?,
        snapshot_nonce: row.get(3)?,
        snapshot_digest: row.get(4)?,
        source_digest: row.get(5)?,
        path: row.get(6)?,
        byte_start: nonnegative_sql_u64(row, 7)?,
        byte_end: nonnegative_sql_u64(row, 8)?,
        cursor_line: row.get(9)?,
        cursor_col: row.get(10)?,
        symbol_uri: row.get(11)?,
        display_name: row.get(12)?,
        anchor_kind: row.get(13)?,
        precision: row.get(14)?,
        record_id: row.get(15)?,
        record_revision: nonnegative_sql_u64(row, 16)?,
        relation: row.get(17)?,
        author: row.get(18)?,
        rationale: row.get(19)?,
        provenance: row.get(20)?,
        created_unix: row.get(21)?,
        retraction: match row.get::<_, Option<String>>(22)? {
            Some(author) => Some(PortRetraction {
                author,
                reason: row.get(23)?,
                created_unix: row.get(24)?,
            }),
            None => None,
        },
    })
}

fn nonnegative_sql_u64(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "negative code binding integer",
            )),
        )
    })
}

fn binding_line(binding: &PortCodeBinding) -> Result<String, String> {
    let b = binding;
    let retraction = b.retraction.as_ref().map_or_else(
        || "null".to_string(),
        |r| {
            format!(
                "{{\"author\":\"{}\",\"reason\":\"{}\",\"created_unix\":{}}}",
                crate::json::escape(&r.author),
                crate::json::escape(&r.reason),
                r.created_unix
            )
        },
    );
    Ok(format!(
        concat!(
            "{{\"type\":\"code_binding\",\"binding\":{{\"repo\":\"{}\",\"repo_root_digest\":\"{}\",",
            "\"snapshot_id\":{},\"snapshot_nonce\":\"{}\",\"snapshot_digest\":\"{}\",\"source_digest\":\"{}\",",
            "\"path\":\"{}\",\"byte_start\":{},\"byte_end\":{},\"cursor_line\":{},\"cursor_col\":{},",
            "\"symbol_uri\":{},\"display_name\":{},\"anchor_kind\":\"{}\",\"precision\":\"{}\",",
            "\"record_id\":\"{}\",\"record_revision\":{},\"relation\":\"{}\",\"author\":\"{}\",",
            "\"rationale\":\"{}\",\"provenance\":\"{}\",\"created_unix\":{},\"retraction\":{}}}}}"
        ),
        crate::json::escape(&b.repo), crate::json::escape(&b.repo_root_digest), b.snapshot_id,
        crate::json::escape(&b.snapshot_nonce), crate::json::escape(&b.snapshot_digest),
        crate::json::escape(&b.source_digest), crate::json::escape(&b.path), b.byte_start, b.byte_end,
        b.cursor_line, b.cursor_col, optional_string(b.symbol_uri.as_deref()), optional_string(b.display_name.as_deref()),
        crate::json::escape(&b.anchor_kind), crate::json::escape(&b.precision), crate::json::escape(&b.record_id),
        b.record_revision, crate::json::escape(&b.relation), crate::json::escape(&b.author),
        crate::json::escape(&b.rationale), crate::json::escape(&b.provenance), b.created_unix, retraction
    ))
}

pub(crate) fn export_lines(conn: &Connection) -> Result<Vec<String>, String> {
    let version = read_schema_version(conn)?;
    if version < 3 {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT b.repo,b.repo_root_digest,b.snapshot_id,b.snapshot_nonce,b.snapshot_digest,
                    b.source_digest,b.path,b.byte_start,b.byte_end,b.cursor_line,b.cursor_col,
                    b.symbol_uri,b.display_name,b.anchor_kind,b.precision,b.record_id,b.record_revision,
                    b.relation,b.author,b.rationale,b.provenance,b.created_unix,
                    t.author,t.reason,t.created_unix
             FROM code_bindings b LEFT JOIN code_binding_retractions t USING(binding_id)
             ORDER BY b.repo COLLATE BINARY,b.repo_root_digest COLLATE BINARY,b.path COLLATE BINARY,
                      b.byte_start,b.byte_end,b.record_id COLLATE BINARY,b.record_revision,
                      b.relation COLLATE BINARY,b.snapshot_nonce COLLATE BINARY,b.binding_id",
        )
        .map_err(|e| format!("cannot prepare code binding export: {e}"))?;
    let lines = stmt
        .query_map([], port_binding_from_row)
        .map_err(|e| format!("cannot read code binding export: {e}"))?
        .map(|row| {
            let binding = row.map_err(|e| format!("cannot read code binding export: {e}"))?;
            binding_line(&binding)
        })
        .collect();
    lines
}

pub(crate) fn binding_sort_key(
    binding: &PortCodeBinding,
) -> (
    String,
    String,
    String,
    u64,
    u64,
    String,
    u64,
    String,
    String,
) {
    (
        binding.repo.clone(),
        binding.repo_root_digest.clone(),
        binding.path.clone(),
        binding.byte_start,
        binding.byte_end,
        binding.record_id.clone(),
        binding.record_revision,
        binding.relation.clone(),
        binding.snapshot_nonce.clone(),
    )
}

pub(crate) fn parse_export_line(line: &str) -> Result<PortCodeBinding, String> {
    let parsed: PortCodeBindingLine =
        serde_json::from_str(line).map_err(|e| format!("invalid code binding export line: {e}"))?;
    if parsed.kind != "code_binding" {
        return Err("invalid code binding export line: type must be code_binding".to_string());
    }
    let binding = parsed.binding;
    let input = BindingInput {
        record_id: binding.record_id.clone(),
        record_revision: binding.record_revision,
        relation: binding.relation.clone(),
        author: binding.author.clone(),
        rationale: binding.rationale.clone(),
        provenance: binding.provenance.clone(),
    };
    validate_binding_input(&input).map_err(|e| format!("invalid code binding export line: {e}"))?;
    knowledge::validate_search_path(&binding.path)
        .map_err(|e| format!("invalid code binding export line: {e}"))?;
    if binding.repo.is_empty()
        || !valid_digest(&binding.repo_root_digest)
        || binding.snapshot_id <= 0
        || binding.snapshot_nonce.is_empty()
        || !valid_digest(&binding.snapshot_digest)
        || !valid_digest(&binding.source_digest)
        || binding.byte_start >= binding.byte_end
        || binding.byte_end > i64::MAX as u64
        || binding.cursor_line == 0
        || binding.cursor_col == 0
        || !matches!(binding.anchor_kind.as_str(), "semantic" | "syntax-fallback")
        || !matches!(binding.precision.as_str(), "Exact" | "Heuristic")
        || binding.created_unix < 0
        || binding.retraction.as_ref().is_some_and(|r| {
            r.author.is_empty()
                || r.author.len() > knowledge::MAX_AUTHOR_BYTES
                || r.reason.is_empty()
                || r.reason.len() > knowledge::MAX_RATIONALE_BYTES
                || r.created_unix < binding.created_unix
        })
    {
        return Err("invalid code binding export line: invalid anchor or retraction".to_string());
    }
    Ok(binding)
}

pub(crate) fn validate_import_bindings(
    bindings: &[PortCodeBinding],
    declared_repo: &str,
    target_repo: &str,
    target_root_digest: &str,
) -> Result<(), String> {
    for binding in bindings {
        if binding.repo != declared_repo
            || binding.repo != target_repo
            || binding.repo_root_digest != target_root_digest
        {
            return Err(
                "invalid import payload: code binding repository identity does not match target"
                    .to_string(),
            );
        }
    }
    Ok(())
}

pub(crate) fn import_binding(tx: &Transaction, incoming: &PortCodeBinding) -> Result<bool, String> {
    let start_i64 = i64::try_from(incoming.byte_start)
        .map_err(|_| "corrupt import file: code binding start out of range".to_string())?;
    let end_i64 = i64::try_from(incoming.byte_end)
        .map_err(|_| "corrupt import file: code binding end out of range".to_string())?;
    let revision_i64 = i64::try_from(incoming.record_revision)
        .map_err(|_| "corrupt import file: code binding revision out of range".to_string())?;
    let mut stmt = tx
        .prepare(
            "SELECT b.repo,b.repo_root_digest,b.snapshot_id,b.snapshot_nonce,b.snapshot_digest,
                    b.source_digest,b.path,b.byte_start,b.byte_end,b.cursor_line,b.cursor_col,
                    b.symbol_uri,b.display_name,b.anchor_kind,b.precision,b.record_id,b.record_revision,
                    b.relation,b.author,b.rationale,b.provenance,b.created_unix,
                    t.author,t.reason,t.created_unix
             FROM code_bindings b LEFT JOIN code_binding_retractions t USING(binding_id)
             WHERE b.repo=?1 AND b.repo_root_digest=?2 AND b.path=?3 AND b.byte_start=?4
               AND b.byte_end=?5 AND b.source_digest=?6 AND b.record_id=?7
               AND b.record_revision=?8 AND b.relation=?9",
        )
        .map_err(|e| format!("cannot prepare code binding merge: {e}"))?;
    let existing = stmt
        .query_map(
            params![
                incoming.repo,
                incoming.repo_root_digest,
                incoming.path,
                start_i64,
                end_i64,
                incoming.source_digest,
                incoming.record_id,
                revision_i64,
                incoming.relation,
            ],
            port_binding_from_row,
        )
        .map_err(|e| format!("cannot read code binding merge: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot read code binding merge: {e}"))?;
    drop(stmt);
    if let Some(found) = existing.first() {
        let mut found_identity = found.clone();
        found_identity.retraction = None;
        let mut incoming_identity = incoming.clone();
        incoming_identity.retraction = None;
        if existing.len() != 1 || found_identity != incoming_identity {
            return Err(format!(
                "import conflict: code binding for '{}:{}:{}' differs from stored provenance",
                incoming.path, incoming.byte_start, incoming.byte_end
            ));
        }
        if found.retraction.is_none() {
            if let Some(retraction) = &incoming.retraction {
                let binding_id: i64 = tx
                    .query_row(
                        "SELECT binding_id FROM code_bindings
                         WHERE repo=?1 AND repo_root_digest=?2 AND path=?3 AND byte_start=?4
                           AND byte_end=?5 AND source_digest=?6 AND record_id=?7
                           AND record_revision=?8 AND relation=?9",
                        params![
                            incoming.repo,
                            incoming.repo_root_digest,
                            incoming.path,
                            start_i64,
                            end_i64,
                            incoming.source_digest,
                            incoming.record_id,
                            revision_i64,
                            incoming.relation,
                        ],
                        |row| row.get(0),
                    )
                    .map_err(|e| format!("cannot find imported code binding: {e}"))?;
                tx.execute(
                    "INSERT INTO code_binding_retractions(binding_id,author,reason,created_unix)
                     VALUES (?1,?2,?3,?4)",
                    params![
                        binding_id,
                        retraction.author,
                        retraction.reason,
                        retraction.created_unix
                    ],
                )
                .map_err(|e| format!("cannot store imported code binding retraction: {e}"))?;
                return Ok(true);
            }
        } else if incoming.retraction.is_some() && found.retraction != incoming.retraction {
            return Err(format!(
                "import conflict: code binding retraction for '{}:{}:{}' differs",
                incoming.path, incoming.byte_start, incoming.byte_end
            ));
        }
        return Ok(false);
    }
    let endpoint: i64 = tx
        .query_row(
            "SELECT count(*) FROM records WHERE id=?1 AND revision=?2",
            params![incoming.record_id, revision_i64],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot verify code binding revision: {e}"))?;
    if endpoint != 1 {
        return Err(format!(
            "invalid import payload: code binding references unknown revision {}:{}",
            incoming.record_id, incoming.record_revision
        ));
    }
    tx.execute(
        "INSERT INTO code_bindings (
            repo,repo_root_digest,snapshot_id,snapshot_nonce,snapshot_digest,source_digest,
            path,byte_start,byte_end,cursor_line,cursor_col,symbol_uri,display_name,anchor_kind,
            precision,record_id,record_revision,relation,author,rationale,provenance,created_unix
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
        params![
            incoming.repo,
            incoming.repo_root_digest,
            incoming.snapshot_id,
            incoming.snapshot_nonce,
            incoming.snapshot_digest,
            incoming.source_digest,
            incoming.path,
            start_i64,
            end_i64,
            incoming.cursor_line,
            incoming.cursor_col,
            incoming.symbol_uri,
            incoming.display_name,
            incoming.anchor_kind,
            incoming.precision,
            incoming.record_id,
            revision_i64,
            incoming.relation,
            incoming.author,
            incoming.rationale,
            incoming.provenance,
            incoming.created_unix,
        ],
    )
    .map_err(|e| format!("cannot store imported code binding: {e}"))?;
    if let Some(retraction) = &incoming.retraction {
        tx.execute(
            "INSERT INTO code_binding_retractions(binding_id,author,reason,created_unix)
             VALUES (?1,?2,?3,?4)",
            params![
                tx.last_insert_rowid(),
                retraction.author,
                retraction.reason,
                retraction.created_unix
            ],
        )
        .map_err(|e| format!("cannot store imported code binding retraction: {e}"))?;
    }
    Ok(true)
}

fn code_index_connection(home: &Path, repo: &str) -> Result<Connection, String> {
    match store::open_read(&store::db_path(home, repo)) {
        store::IndexRead::Ready(conn) => Ok(conn),
        store::IndexRead::Missing => Err("no code index: index the repository first".to_string()),
        store::IndexRead::Outdated(version) => {
            Err(format!("code index schema v{version} is outdated"))
        }
        store::IndexRead::Ahead(version) => Err(format!(
            "code index schema v{version} is newer than this Riltra"
        )),
        store::IndexRead::Error(e) => Err(e),
    }
}

fn snapshot_digest(files: &std::collections::BTreeMap<String, (i64, String)>) -> String {
    let mut input = Vec::new();
    for (path, (_, digest)) in files {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
        input.extend_from_slice(digest.as_bytes());
        input.push(b'\n');
    }
    store::content_digest(&input)
}

#[allow(clippy::too_many_arguments)]
fn resolve_code_anchor(
    home: &Path,
    entry: &registry::RepoEntry,
    path: &str,
    line: u32,
    col: u32,
    symbol: Option<&str>,
    capture_snapshot: bool,
    cached_source: Option<&[u8]>,
) -> Result<CodeAnchor, String> {
    if line == 0 || col == 0 {
        return Err("anchor line and column must be at least 1".to_string());
    }
    let root = paths::canonical_root(Path::new(&entry.root))
        .map_err(|e| format!("cannot resolve repository root: {e}"))?;
    let path = semantic::validate_doc_path(&root, path)?;
    let conn = code_index_connection(home, &entry.name)?;
    let snapshot = store::latest_snapshot(&conn)?
        .ok_or_else(|| "no code snapshot: index the repository first".to_string())?;
    let anchor = index::resolve_edge_anchor_with_source(
        &conn,
        snapshot.id,
        &root,
        &path,
        line - 1,
        col - 1,
        symbol,
        cached_source,
    )?;
    let (byte_start, byte_end) = anchor
        .span_bytes
        .ok_or_else(|| "anchor unresolved: no source span".to_string())?;
    if byte_start >= byte_end {
        return Err("anchor unresolved: empty source span".to_string());
    }
    let (_, source_digest) = store::file_identity(&conn, snapshot.id, &path)?
        .ok_or_else(|| "anchor unresolved: path is not in the current code snapshot".to_string())?;
    let source = match cached_source {
        Some(bytes) => std::borrow::Cow::Borrowed(bytes),
        None => std::borrow::Cow::Owned(semantic::confined_read(
            &root,
            &path,
            crate::scan::MAX_FILE_BYTES,
        )?),
    };
    if store::content_digest(&source) != source_digest {
        return Err(
            "anchor unresolved: source changed; reindex before creating a binding".to_string(),
        );
    }
    let slice = source
        .get(byte_start..byte_end)
        .ok_or_else(|| "anchor unresolved: span is outside current source".to_string())?;
    std::str::from_utf8(slice)
        .map_err(|_| "anchor unresolved: span is not UTF-8 source text".to_string())?;
    Ok(CodeAnchor {
        repo: entry.name.clone(),
        repo_root_digest: store::content_digest(root.to_string_lossy().as_bytes()),
        snapshot_id: snapshot.id,
        snapshot_nonce: snapshot.nonce,
        snapshot_digest: if capture_snapshot {
            Some(snapshot_digest(&store::file_id_map(&conn, snapshot.id)?))
        } else {
            None
        },
        source_digest,
        path,
        byte_start,
        byte_end,
        cursor_line: line,
        cursor_col: col,
        symbol_uri: anchor.symbol_uri,
        display_name: anchor.display,
        anchor_kind: anchor.kind.as_str().to_string(),
        precision: anchor.precision,
    })
}

pub(crate) fn anchor_from_trace(
    conn: &Connection,
    entry: &registry::RepoEntry,
    root: &Path,
    trace: &index::TraceQueryOut,
) -> Result<CodeAnchor, String> {
    let anchor = &trace.anchor;
    let (byte_start, byte_end) = anchor
        .span_bytes
        .ok_or_else(|| "anchor unresolved: no source span".to_string())?;
    if byte_start >= byte_end || trace.repo != entry.name || trace.snapshot.repo != entry.name {
        return Err("anchor unresolved: code snapshot identity mismatch".to_string());
    }
    let root =
        paths::canonical_root(root).map_err(|e| format!("cannot resolve repository root: {e}"))?;
    if trace.snapshot.root != root.to_string_lossy() {
        return Err("anchor unresolved: code snapshot root mismatch".to_string());
    }
    let path = semantic::validate_doc_path(&root, &anchor.path)?;
    let (_, source_digest) = store::file_identity(conn, trace.snapshot.id, &path)?
        .ok_or_else(|| "anchor unresolved: path is not in the pinned code snapshot".to_string())?;
    Ok(CodeAnchor {
        repo: entry.name.clone(),
        repo_root_digest: store::content_digest(root.to_string_lossy().as_bytes()),
        snapshot_id: trace.snapshot.id,
        snapshot_nonce: trace.snapshot.nonce.clone(),
        snapshot_digest: None,
        source_digest,
        path,
        byte_start,
        byte_end,
        cursor_line: anchor.line1,
        cursor_col: anchor.col1,
        symbol_uri: anchor.symbol_uri.clone(),
        display_name: anchor.display.clone(),
        anchor_kind: anchor.kind.as_str().to_string(),
        precision: anchor.precision.clone(),
    })
}

fn validate_binding_input(input: &BindingInput) -> Result<(), String> {
    knowledge::validate_id(&input.record_id)?;
    if input.record_revision == 0 || input.record_revision > i64::MAX as u64 {
        return Err("record_revision must be in the positive SQLite integer range".to_string());
    }
    if !CODE_RELATIONS.contains(&input.relation.as_str()) {
        return Err(format!(
            "relation must be one of: {}",
            CODE_RELATIONS.join(", ")
        ));
    }
    if input.author.is_empty() || input.author.len() > knowledge::MAX_AUTHOR_BYTES {
        return Err(format!(
            "author must be 1..={} bytes",
            knowledge::MAX_AUTHOR_BYTES
        ));
    }
    if input.rationale.is_empty() || input.rationale.len() > knowledge::MAX_RATIONALE_BYTES {
        return Err(format!(
            "rationale must be 1..={} bytes",
            knowledge::MAX_RATIONALE_BYTES
        ));
    }
    if input.provenance.is_empty() || input.provenance.len() > MAX_PROVENANCE_BYTES {
        return Err(format!(
            "provenance must be 1..={MAX_PROVENANCE_BYTES} bytes"
        ));
    }
    Ok(())
}

fn parse_binding_input(bytes: &[u8]) -> Result<BindingInput, String> {
    let fields = crate::strict_object(
        bytes,
        &[
            "record_id",
            "record_revision",
            "relation",
            "author",
            "rationale",
            "provenance",
        ],
    )?;
    let input = BindingInput {
        record_id: crate::field_str(&fields, "record_id")?,
        record_revision: crate::field_u64(&fields, "record_revision")?,
        relation: crate::field_str(&fields, "relation")?,
        author: crate::field_str(&fields, "author")?,
        rationale: crate::field_str(&fields, "rationale")?,
        provenance: crate::field_str(&fields, "provenance")?,
    };
    validate_binding_input(&input)?;
    Ok(input)
}

fn parse_retract_input(bytes: &[u8]) -> Result<RetractInput, String> {
    let fields = crate::strict_object(bytes, &["author", "reason"])?;
    let input = RetractInput {
        author: crate::field_str(&fields, "author")?,
        reason: crate::field_str(&fields, "reason")?,
    };
    if input.author.is_empty() || input.author.len() > knowledge::MAX_AUTHOR_BYTES {
        return Err(format!(
            "author must be 1..={} bytes",
            knowledge::MAX_AUTHOR_BYTES
        ));
    }
    if input.reason.is_empty() || input.reason.len() > knowledge::MAX_RATIONALE_BYTES {
        return Err(format!(
            "reason must be 1..={} bytes",
            knowledge::MAX_RATIONALE_BYTES
        ));
    }
    Ok(input)
}

fn check_anchor_source(root: &Path, anchor: &CodeAnchor) -> Result<(), String> {
    let bytes = semantic::confined_read(root, &anchor.path, crate::scan::MAX_FILE_BYTES)
        .map_err(|e| format!("cannot verify code anchor: {e}"))?;
    if store::content_digest(&bytes) != anchor.source_digest {
        return Err("code anchor changed before binding was stored".to_string());
    }
    bytes
        .get(anchor.byte_start..anchor.byte_end)
        .ok_or_else(|| "code anchor span no longer binds source bytes".to_string())?;
    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn insert_binding(
    tx: &Transaction,
    anchor: &CodeAnchor,
    input: &BindingInput,
) -> Result<i64, String> {
    let revision = i64::try_from(input.record_revision)
        .map_err(|_| "record revision is out of range".to_string())?;
    let exists: i64 = tx
        .query_row(
            "SELECT count(*) FROM records WHERE id = ?1 AND revision = ?2",
            params![input.record_id, revision],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot verify knowledge revision: {e}"))?;
    if exists != 1 {
        return Err(format!(
            "unknown knowledge revision: '{}:{}'",
            input.record_id, input.record_revision
        ));
    }
    let duplicate: i64 = tx
        .query_row(
            "SELECT count(*) FROM code_bindings b
             WHERE b.repo = ?1 AND b.repo_root_digest = ?2 AND b.path = ?3
               AND b.byte_start = ?4 AND b.byte_end = ?5 AND b.source_digest = ?6
               AND b.record_id = ?7 AND b.record_revision = ?8 AND b.relation = ?9",
            params![
                anchor.repo,
                anchor.repo_root_digest,
                anchor.path,
                anchor.byte_start as i64,
                anchor.byte_end as i64,
                anchor.source_digest,
                input.record_id,
                revision,
                input.relation
            ],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot check duplicate code binding: {e}"))?;
    if duplicate != 0 {
        return Err("duplicate code binding".to_string());
    }
    tx.execute(
        "INSERT INTO code_bindings (
            repo, repo_root_digest, snapshot_id, snapshot_nonce, snapshot_digest,
            source_digest, path, byte_start, byte_end, cursor_line, cursor_col,
            symbol_uri, display_name, anchor_kind, precision, record_id,
            record_revision, relation, author, rationale, provenance, created_unix
         ) VALUES (
            ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22
         )",
        params![
            anchor.repo,
            anchor.repo_root_digest,
            anchor.snapshot_id,
            anchor.snapshot_nonce,
            anchor
                .snapshot_digest
                .as_deref()
                .ok_or("missing snapshot digest for binding")?,
            anchor.source_digest,
            anchor.path,
            anchor.byte_start as i64,
            anchor.byte_end as i64,
            anchor.cursor_line,
            anchor.cursor_col,
            anchor.symbol_uri,
            anchor.display_name,
            anchor.anchor_kind,
            anchor.precision,
            input.record_id,
            revision,
            input.relation,
            input.author,
            input.rationale,
            input.provenance,
            now_unix()
        ],
    )
    .map_err(|e| format!("cannot store code binding: {e}"))?;
    Ok(tx.last_insert_rowid())
}

fn create(
    conn: &mut Connection,
    root: &Path,
    anchor: &CodeAnchor,
    input: &BindingInput,
) -> Result<(i64, u64), String> {
    validate_binding_input(input)?;
    if anchor.repo.is_empty()
        || anchor.byte_start >= anchor.byte_end
        || anchor.byte_end > i64::MAX as usize
        || anchor.snapshot_id <= 0
        || anchor.cursor_line == 0
        || anchor.cursor_col == 0
    {
        return Err("invalid code anchor".to_string());
    }
    check_anchor_source(root, anchor)?;
    knowledge_store::revalidate_open_paths(conn)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    knowledge_store::ensure_code_binding_schema(&tx)?;
    let binding_id = insert_binding(&tx, anchor, input)?;
    check_anchor_source(root, anchor)?;
    let generation = knowledge_store::bump_generation(&tx)?;
    tx.commit()
        .map_err(|e| format!("cannot commit code binding: {e}"))?;
    Ok((binding_id, generation))
}

fn retract(conn: &mut Connection, binding_id: i64, input: &RetractInput) -> Result<u64, String> {
    if binding_id <= 0 {
        return Err("binding id must be positive".to_string());
    }
    if input.author.is_empty() || input.author.len() > knowledge::MAX_AUTHOR_BYTES {
        return Err(format!(
            "author must be 1..={} bytes",
            knowledge::MAX_AUTHOR_BYTES
        ));
    }
    if input.reason.is_empty() || input.reason.len() > knowledge::MAX_RATIONALE_BYTES {
        return Err(format!(
            "reason must be 1..={} bytes",
            knowledge::MAX_RATIONALE_BYTES
        ));
    }
    knowledge_store::revalidate_open_paths(conn)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    knowledge_store::ensure_code_binding_schema(&tx)?;
    let exists: i64 = tx
        .query_row(
            "SELECT count(*) FROM code_bindings WHERE binding_id = ?1",
            [binding_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot read code binding: {e}"))?;
    if exists != 1 {
        return Err(format!("unknown code binding id: {binding_id}"));
    }
    let retracted: i64 = tx
        .query_row(
            "SELECT count(*) FROM code_binding_retractions WHERE binding_id = ?1",
            [binding_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot read code binding retraction: {e}"))?;
    if retracted != 0 {
        return Err(format!("code binding already retracted: {binding_id}"));
    }
    tx.execute(
        "INSERT INTO code_binding_retractions(binding_id, author, reason, created_unix)
         VALUES (?1, ?2, ?3, ?4)",
        params![binding_id, input.author, input.reason, now_unix()],
    )
    .map_err(|e| format!("cannot retract code binding: {e}"))?;
    let generation = knowledge_store::bump_generation(&tx)?;
    tx.commit()
        .map_err(|e| format!("cannot commit code binding retraction: {e}"))?;
    Ok(generation)
}

pub(crate) fn verify_store_rows(
    conn: &Connection,
    repo: &registry::RepoEntry,
    croot: &Path,
) -> Result<u64, String> {
    let root_digest = store::content_digest(croot.to_string_lossy().as_bytes());
    let invalid: i64 = conn
        .query_row(
            "SELECT (SELECT count(*) FROM code_bindings b
             LEFT JOIN records r ON r.id=b.record_id AND r.revision=b.record_revision
             WHERE b.repo != ?1 OR b.repo_root_digest != ?2 OR r.id IS NULL
                OR b.snapshot_id <= 0 OR b.snapshot_nonce = ''
                OR length(b.repo_root_digest) != 16 OR b.repo_root_digest GLOB '*[^0-9a-f]*'
                OR length(b.snapshot_digest) != 16 OR b.snapshot_digest GLOB '*[^0-9a-f]*'
                OR length(b.source_digest) != 16 OR b.source_digest GLOB '*[^0-9a-f]*'
                OR b.byte_start < 0 OR b.byte_end <= b.byte_start
                OR b.cursor_line <= 0 OR b.cursor_col <= 0
                OR b.anchor_kind NOT IN ('semantic','syntax-fallback')
                OR b.precision NOT IN ('Exact','Heuristic')
                OR b.relation NOT IN ('documents','constrains','implements','tests','motivates','observes')
                OR b.author = '' OR b.rationale = '' OR b.provenance = '')
             + (SELECT count(*) FROM code_binding_retractions t
                LEFT JOIN code_bindings b USING(binding_id)
                WHERE b.binding_id IS NULL OR t.author='' OR t.reason=''
                   OR length(t.author) > 256 OR length(t.reason) > 65536
                   OR t.created_unix < b.created_unix)",
            params![repo.name, root_digest],
            |row| row.get(0),
        )
        .map_err(|e| format!("corrupt knowledge store: cannot verify code bindings: {e}"))?;
    if invalid != 0 {
        return Err("corrupt knowledge store: invalid code binding rows".to_string());
    }
    let mut stmt = conn
        .prepare("SELECT path FROM code_bindings ORDER BY binding_id")
        .map_err(|e| format!("corrupt knowledge store: cannot verify code binding paths: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("corrupt knowledge store: cannot verify code binding paths: {e}"))?;
    for path in rows {
        knowledge::validate_search_path(&path.map_err(|e| e.to_string())?)
            .map_err(|_| "corrupt knowledge store: invalid code binding path".to_string())?;
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM code_bindings", [], |row| row.get(0))
        .map_err(|e| format!("corrupt knowledge store: cannot count code bindings: {e}"))?;
    u64::try_from(count)
        .map_err(|_| "corrupt knowledge store: negative code binding count".to_string())
}

fn read_schema_version(conn: &Connection) -> Result<u32, String> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| format!("cannot read knowledge schema version: {e}"))
}

fn usize_from_sql(value: i64, column: usize) -> rusqlite::Result<usize> {
    usize::try_from(value).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "negative code binding span",
            )),
        )
    })
}

fn freshness(
    source: &Result<Vec<u8>, String>,
    source_digest: &str,
    byte_start: usize,
    byte_end: usize,
) -> (String, Option<String>) {
    match source {
        Err(reason) => ("unresolved".to_string(), Some(reason.clone())),
        Ok(bytes) if store::content_digest(bytes) != source_digest => (
            "stale".to_string(),
            Some("source digest changed; original anchor is retained".to_string()),
        ),
        Ok(bytes) if bytes.get(byte_start..byte_end).is_none() => (
            "unresolved".to_string(),
            Some("stored span is outside current source; no rebinding attempted".to_string()),
        ),
        Ok(_) => ("fresh".to_string(), None),
    }
}

fn pinned_anchor_unresolved_reason(
    anchor: &CodeAnchor,
    source: &Result<Vec<u8>, String>,
) -> Option<String> {
    let bytes = match source {
        Ok(bytes) => bytes,
        Err(reason) => return Some(reason.clone()),
    };
    if store::content_digest(bytes) != anchor.source_digest {
        return Some("anchor unresolved: source changed since pinned code snapshot".to_string());
    }
    let Some(span) = bytes.get(anchor.byte_start..anchor.byte_end) else {
        return Some("anchor unresolved: span is outside current source".to_string());
    };
    if std::str::from_utf8(span).is_err() {
        return Some("anchor unresolved: span is not UTF-8 source text".to_string());
    }
    None
}

fn binding_matches_pinned_anchor(binding: &BindingView, anchor: &CodeAnchor) -> bool {
    let same_span = binding.byte_start == anchor.byte_start && binding.byte_end == anchor.byte_end;
    let same_cursor =
        binding.cursor_line == anchor.cursor_line && binding.cursor_col == anchor.cursor_col;
    binding.repo == anchor.repo
        && binding.path == anchor.path
        && binding.source_digest == anchor.source_digest
        && (same_span || same_cursor)
        && binding.symbol_uri == anchor.symbol_uri
        && binding.anchor_kind == anchor.anchor_kind
}

fn binding_from_row(
    row: &rusqlite::Row<'_>,
    source: &Result<Vec<u8>, String>,
) -> rusqlite::Result<BindingView> {
    let source_digest: String = row.get(5)?;
    let byte_start = usize_from_sql(row.get(7)?, 7)?;
    let byte_end = usize_from_sql(row.get(8)?, 8)?;
    let (freshness, freshness_reason) = freshness(source, &source_digest, byte_start, byte_end);
    Ok(BindingView {
        binding_id: row.get(0)?,
        repo: row.get(1)?,
        snapshot_id: row.get(2)?,
        snapshot_nonce: row.get(3)?,
        snapshot_digest: row.get(4)?,
        source_digest,
        path: row.get(6)?,
        byte_start,
        byte_end,
        cursor_line: row.get::<_, i64>(9)?.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid code binding cursor line",
                )),
            )
        })?,
        cursor_col: row.get::<_, i64>(10)?.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid code binding cursor column",
                )),
            )
        })?,
        symbol_uri: row.get(11)?,
        display_name: row.get(12)?,
        anchor_kind: row.get(13)?,
        precision: row.get(14)?,
        record_id: row.get(15)?,
        record_revision: row.get::<_, i64>(16)?.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                16,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid code binding record revision",
                )),
            )
        })?,
        record_kind: row.get(17)?,
        record_title: row.get(18)?,
        record_body: row.get(19)?,
        record_status: row.get(20)?,
        relation: row.get(21)?,
        author: row.get(22)?,
        rationale: row.get(23)?,
        provenance: row.get(24)?,
        created_unix: row.get(25)?,
        retracted: row.get::<_, Option<String>>(26)?.is_some(),
        retraction_author: row.get(26)?,
        retraction_reason: row.get(27)?,
        retracted_unix: row.get(28)?,
        freshness,
        freshness_reason,
    })
}

pub(crate) fn empty_at(
    path: &str,
    line: u32,
    col: u32,
    generation: u64,
    schema_version: u32,
) -> AtPage {
    AtPage {
        status: "no-link-proven",
        unresolved_reason: None,
        path: path.to_string(),
        line,
        col,
        bindings: Vec::new(),
        total: 0,
        limit: knowledge::SEARCH_LIMIT_DEFAULT,
        offset: 0,
        truncated: false,
        next_offset: None,
        generation,
        schema_version,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn at(
    conn: &Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    path: &str,
    line: u32,
    col: u32,
    symbol: Option<&str>,
    limit: usize,
    offset: usize,
    generation: Option<u64>,
) -> Result<AtPage, String> {
    if line == 0 || col == 0 || !(1..=knowledge::SEARCH_LIMIT_MAX).contains(&limit) {
        return Err("invalid knowledge-at anchor or page bounds".to_string());
    }
    i64::try_from(offset).map_err(|_| "knowledge-at offset is out of range".to_string())?;
    let schema_version = read_schema_version(conn)?;
    if schema_version < 3 {
        let current_generation = knowledge_store::read_generation(conn)?;
        if let Some(expected) = generation {
            if expected != current_generation {
                return Err(format!(
                    "knowledge generation conflict: expected {expected} but store is at {current_generation}"
                ));
            }
        }
        let mut page = empty_at(path, line, col, current_generation, schema_version);
        page.status = "unavailable";
        page.limit = limit;
        page.offset = offset;
        return Ok(page);
    }
    let conn_tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("cannot begin knowledge-at read: {e}"))?;
    let current_generation = knowledge_store::read_generation(&conn_tx)?;
    if let Some(expected) = generation {
        if expected != current_generation {
            return Err(format!(
                "knowledge generation conflict: expected {expected} but store is at {current_generation}"
            ));
        }
    }
    let root = paths::canonical_root(Path::new(&entry.root))
        .map_err(|e| format!("cannot resolve repository root: {e}"))?;
    let path = semantic::validate_doc_path(&root, path)?;
    let repo_root_digest = store::content_digest(root.to_string_lossy().as_bytes());
    let current_source = semantic::confined_read(&root, &path, crate::scan::MAX_FILE_BYTES)
        .map_err(|e| format!("anchor source unavailable: {e}"));
    let (anchor, unresolved_reason) = match resolve_code_anchor(
        home,
        entry,
        &path,
        line,
        col,
        symbol,
        false,
        current_source.as_ref().ok().map(Vec::as_slice),
    ) {
        Ok(anchor) => (Some(anchor), None),
        Err(reason) => (None, Some(reason)),
    };
    let resolved = anchor.is_some();
    let (selection, selection_end) = match anchor.as_ref() {
        Some(anchor) => (
            i64::try_from(anchor.byte_start)
                .map_err(|_| "code anchor start is out of range".to_string())?,
            i64::try_from(anchor.byte_end)
                .map_err(|_| "code anchor end is out of range".to_string())?,
        ),
        None => (-1, -1),
    };
    let total: i64 = conn_tx
        .query_row(
            "SELECT count(*) FROM code_bindings b WHERE b.repo=?1 AND b.repo_root_digest=?2 AND b.path=?3
             AND ((b.byte_start=?4 AND b.byte_end=?5)
                  OR (b.cursor_line=?6 AND b.cursor_col=?7))",
            params![
                entry.name,
                repo_root_digest,
                path,
                selection,
                selection_end,
                line,
                col
            ],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot count code bindings: {e}"))?;
    let limit_i64 =
        i64::try_from(limit).map_err(|_| "knowledge-at limit out of range".to_string())?;
    let offset_i64 =
        i64::try_from(offset).map_err(|_| "knowledge-at offset out of range".to_string())?;
    let mut stmt = conn_tx
        .prepare(
            "SELECT b.binding_id,b.repo,b.snapshot_id,b.snapshot_nonce,b.snapshot_digest,b.source_digest,
                    b.path,b.byte_start,b.byte_end,b.cursor_line,b.cursor_col,b.symbol_uri,b.display_name,
                    b.anchor_kind,b.precision,b.record_id,b.record_revision,r.kind,r.title,r.body,r.status,
                    b.relation,b.author,b.rationale,b.provenance,b.created_unix,
                    t.author,t.reason,t.created_unix
             FROM code_bindings b
             JOIN records r ON r.id=b.record_id AND r.revision=b.record_revision
             LEFT JOIN code_binding_retractions t USING(binding_id)
             WHERE b.repo=?1 AND b.repo_root_digest=?2 AND b.path=?3
               AND ((b.byte_start=?4 AND b.byte_end=?5)
                    OR (b.cursor_line=?6 AND b.cursor_col=?7))
             ORDER BY b.binding_id LIMIT ?8 OFFSET ?9",
        )
        .map_err(|e| format!("cannot prepare code binding read: {e}"))?;
    let rows = stmt
        .query_map(
            params![
                entry.name,
                repo_root_digest,
                path,
                selection,
                selection_end,
                line,
                col,
                limit_i64,
                offset_i64
            ],
            |row| binding_from_row(row, &current_source),
        )
        .map_err(|e| format!("cannot read code bindings: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot read code bindings: {e}"))?;
    drop(stmt);
    drop(conn_tx);
    let total = usize::try_from(total)
        .map_err(|_| "corrupt knowledge store: negative binding count".to_string())?;
    let end = offset.saturating_add(rows.len());
    let truncated = end < total;
    let next_offset = truncated.then_some(end);
    let status = if !resolved {
        "unresolved"
    } else if rows.is_empty() && total > 0 {
        "page-empty"
    } else if rows.is_empty() {
        "no-link-proven"
    } else if rows.iter().all(|binding| binding.retracted) {
        "retracted"
    } else if rows.iter().any(|binding| binding.freshness == "unresolved") {
        "unresolved"
    } else if rows.iter().any(|binding| binding.freshness == "stale") {
        "stale"
    } else {
        "linked"
    };
    Ok(AtPage {
        status,
        unresolved_reason,
        path,
        line,
        col,
        bindings: rows,
        total,
        limit,
        offset,
        truncated,
        next_offset,
        generation: current_generation,
        schema_version,
    })
}

/// Read bindings for a code anchor already resolved from a pinned code snapshot.
/// The first knowledge read pins this deferred transaction; the code snapshot is
/// intentionally independent from this knowledge generation.
pub(crate) fn at_pinned(
    conn: &Connection,
    entry: &registry::RepoEntry,
    anchor: &CodeAnchor,
    limit: usize,
    generation: Option<u64>,
) -> Result<AtPage, String> {
    if anchor.repo != entry.name
        || anchor.byte_start >= anchor.byte_end
        || anchor.cursor_line == 0
        || anchor.cursor_col == 0
        || !(1..=knowledge::SEARCH_LIMIT_MAX).contains(&limit)
    {
        return Err("invalid pinned knowledge-at anchor or page bounds".to_string());
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("cannot begin knowledge-at read: {e}"))?;
    let current_generation = knowledge_store::read_generation(&tx)?;
    if let Some(expected) = generation {
        if expected != current_generation {
            return Err(format!(
                "knowledge generation conflict: expected {expected} but store is at {current_generation}"
            ));
        }
    }
    let schema_version = read_schema_version(&tx)?;
    if schema_version < 3 {
        let mut page = empty_at(
            &anchor.path,
            anchor.cursor_line,
            anchor.cursor_col,
            current_generation,
            schema_version,
        );
        page.status = "unavailable";
        page.limit = limit;
        return Ok(page);
    }
    let root = paths::canonical_root(Path::new(&entry.root))
        .map_err(|e| format!("cannot resolve repository root: {e}"))?;
    let root_digest = store::content_digest(root.to_string_lossy().as_bytes());
    if anchor.repo_root_digest != root_digest
        || semantic::validate_doc_path(&root, &anchor.path)? != anchor.path
    {
        return Err("pinned knowledge-at anchor is outside the registered repository".into());
    }
    let current_source = semantic::confined_read(&root, &anchor.path, crate::scan::MAX_FILE_BYTES)
        .map_err(|e| format!("anchor source unavailable: {e}"));
    let mut unresolved_reason = pinned_anchor_unresolved_reason(anchor, &current_source);
    let selection = i64::try_from(anchor.byte_start)
        .map_err(|_| "code anchor start is out of range".to_string())?;
    let selection_end = i64::try_from(anchor.byte_end)
        .map_err(|_| "code anchor end is out of range".to_string())?;
    let total: i64 = tx
        .query_row(
            "SELECT count(*) FROM code_bindings b WHERE b.repo=?1 AND b.repo_root_digest=?2 AND b.path=?3
             AND ((b.byte_start=?4 AND b.byte_end=?5)
                  OR (b.cursor_line=?6 AND b.cursor_col=?7))",
            params![
                entry.name,
                anchor.repo_root_digest,
                anchor.path,
                selection,
                selection_end,
                anchor.cursor_line,
                anchor.cursor_col
            ],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot count code bindings: {e}"))?;
    let limit_i64 =
        i64::try_from(limit).map_err(|_| "knowledge-at limit out of range".to_string())?;
    let mut stmt = tx
        .prepare(
            "SELECT b.binding_id,b.repo,b.snapshot_id,b.snapshot_nonce,b.snapshot_digest,b.source_digest,
                    b.path,b.byte_start,b.byte_end,b.cursor_line,b.cursor_col,b.symbol_uri,b.display_name,
                    b.anchor_kind,b.precision,b.record_id,b.record_revision,r.kind,r.title,r.body,r.status,
                    b.relation,b.author,b.rationale,b.provenance,b.created_unix,
                    t.author,t.reason,t.created_unix
             FROM code_bindings b
             JOIN records r ON r.id=b.record_id AND r.revision=b.record_revision
             LEFT JOIN code_binding_retractions t USING(binding_id)
             WHERE b.repo=?1 AND b.repo_root_digest=?2 AND b.path=?3
               AND ((b.byte_start=?4 AND b.byte_end=?5)
                    OR (b.cursor_line=?6 AND b.cursor_col=?7))
             ORDER BY b.binding_id LIMIT ?8",
        )
        .map_err(|e| format!("cannot prepare code binding read: {e}"))?;
    let mut rows = stmt
        .query_map(
            params![
                entry.name,
                anchor.repo_root_digest,
                anchor.path,
                selection,
                selection_end,
                anchor.cursor_line,
                anchor.cursor_col,
                limit_i64
            ],
            |row| binding_from_row(row, &current_source),
        )
        .map_err(|e| format!("cannot read code bindings: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot read code bindings: {e}"))?;
    drop(stmt);
    drop(tx);
    let total = usize::try_from(total)
        .map_err(|_| "corrupt knowledge store: negative binding count".to_string())?;
    if rows.len() < total.min(limit) {
        return Err("corrupt knowledge store: binding count does not match readable rows".into());
    }
    for binding in &mut rows {
        if !binding_matches_pinned_anchor(binding, anchor) {
            binding.freshness = "unresolved".to_string();
            binding.freshness_reason =
                Some("stored binding identity does not match pinned code anchor".to_string());
            unresolved_reason.get_or_insert_with(|| {
                "stored binding identity does not match pinned code anchor".to_string()
            });
        }
    }
    let truncated = rows.len() < total;
    let status = if unresolved_reason.is_some()
        || rows.iter().any(|binding| binding.freshness == "unresolved")
    {
        "unresolved"
    } else if rows.is_empty() {
        "no-link-proven"
    } else if rows.iter().all(|binding| binding.retracted) {
        "retracted"
    } else if rows.iter().any(|binding| binding.freshness == "stale") {
        "stale"
    } else {
        "linked"
    };
    Ok(AtPage {
        status,
        unresolved_reason,
        path: anchor.path.clone(),
        line: anchor.cursor_line,
        col: anchor.cursor_col,
        bindings: rows,
        total,
        limit,
        offset: 0,
        truncated,
        next_offset: truncated.then_some(limit),
        generation: current_generation,
        schema_version,
    })
}

fn optional_string(value: Option<&str>) -> String {
    value
        .map(|s| format!("\"{}\"", crate::json::escape(s)))
        .unwrap_or_else(|| "null".to_string())
}

fn binding_json(b: &BindingView) -> String {
    format!(
        concat!(
            "{{\"binding_id\":{},\"repo\":\"{}\",",
            "\"anchor\":{{\"path\":\"{}\",\"byte_start\":{},\"byte_end\":{},\"line\":{},\"col\":{},",
            "\"symbol_uri\":{},\"display_name\":{},\"kind\":\"{}\",\"precision\":\"{}\",",
            "\"snapshot_id\":{},\"snapshot_nonce\":\"{}\",\"snapshot_digest\":\"{}\",\"source_digest\":\"{}\"}},",
            "\"relation\":\"{}\",\"record_id\":\"{}\",\"record_revision\":{},",
            "\"record\":{{\"kind\":\"{}\",\"title\":\"{}\",\"body\":\"{}\",\"status\":\"{}\"}},",
            "\"author\":\"{}\",\"rationale\":\"{}\",\"provenance\":\"{}\",\"created_unix\":{},",
            "\"retracted\":{},\"retraction_author\":{},\"retraction_reason\":{},\"retracted_unix\":{},",
            "\"freshness\":\"{}\",\"freshness_reason\":{},\"verification\":\"Unverified\",\"support\":\"unverified\"}}"
        ),
        b.binding_id, crate::json::escape(&b.repo), crate::json::escape(&b.path), b.byte_start, b.byte_end,
        b.cursor_line, b.cursor_col, optional_string(b.symbol_uri.as_deref()), optional_string(b.display_name.as_deref()),
        crate::json::escape(&b.anchor_kind), crate::json::escape(&b.precision), b.snapshot_id,
        crate::json::escape(&b.snapshot_nonce), crate::json::escape(&b.snapshot_digest), crate::json::escape(&b.source_digest),
        crate::json::escape(&b.relation), crate::json::escape(&b.record_id), b.record_revision,
        crate::json::escape(&b.record_kind), crate::json::escape(&b.record_title), crate::json::escape(&b.record_body),
        crate::json::escape(&b.record_status), crate::json::escape(&b.author), crate::json::escape(&b.rationale),
        crate::json::escape(&b.provenance), b.created_unix, b.retracted, optional_string(b.retraction_author.as_deref()),
        optional_string(b.retraction_reason.as_deref()), b.retracted_unix.map_or_else(|| "null".to_string(), |n| n.to_string()),
        crate::json::escape(&b.freshness), optional_string(b.freshness_reason.as_deref())
    )
}

pub(crate) fn page_json(page: &AtPage) -> String {
    format!(
        "{{\"status\":\"{}\",\"unresolved_reason\":{},\"anchor\":{{\"path\":\"{}\",\"line\":{},\"col\":{}}},\"schema_version\":{},\"bindings\":[{}]}}",
        page.status, optional_string(page.unresolved_reason.as_deref()), crate::json::escape(&page.path),
        page.line, page.col, page.schema_version, page.bindings.iter().map(binding_json).collect::<Vec<_>>().join(",")
    )
}

fn position(value: &str, flag: &str, err: &mut String) -> Result<u32, i32> {
    match value.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => {
            err.push_str(&format!(
                "error: --{flag} must be a positive 32-bit integer\n"
            ));
            Err(2)
        }
    }
}

fn write_error_code(error: &str) -> i32 {
    if error.starts_with("invalid ")
        || error.starts_with("duplicate ")
        || error.starts_with("unknown knowledge revision")
        || error.starts_with("unknown code binding")
        || error.starts_with("code binding already retracted")
        || error.starts_with("author ")
        || error.starts_with("reason ")
        || error.starts_with("relation ")
        || error.starts_with("record_")
        || error.starts_with("rationale ")
        || error.starts_with("provenance ")
    {
        2
    } else {
        3
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_cli(
    repo: &str,
    path: &str,
    line: &str,
    col: &str,
    symbol: Option<&str>,
    input_flag: &str,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let line = match position(line, "line", err) {
        Ok(n) => n,
        Err(code) => return code,
    };
    let col = match position(col, "col", err) {
        Ok(n) => n,
        Err(code) => return code,
    };
    let bytes = match crate::knowledge_input(input_flag) {
        Ok(bytes) => bytes,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let input = match parse_binding_input(&bytes) {
        Ok(input) => input,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let (root, entry) = match crate::knowledge_repo(repo, err) {
        Ok(value) => value,
        Err(code) => return code,
    };
    let home = paths::config_dir();
    let anchor = match resolve_code_anchor(&home, &entry, path, line, col, symbol, true, None) {
        Ok(anchor) => anchor,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    // Captured by `resolve_code_anchor` for every write; checked before any
    // write so a missing digest can never leave a half-reported binding.
    let Some(snapshot_digest) = anchor.snapshot_digest.clone() else {
        err.push_str("error: snapshot digest was not captured for this anchor\n");
        return 3;
    };
    let mut conn = match knowledge_store::open_knowledge(&entry, true) {
        Ok(conn) => conn,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    match create(&mut conn, Path::new(&root), &anchor, &input) {
        Ok((binding_id, generation)) => {
            let result = format!(
                concat!(
                    "{{\"binding_id\":{},\"repo\":\"{}\",\"anchor\":{{\"path\":\"{}\",",
                    "\"byte_start\":{},\"byte_end\":{},\"line\":{},\"col\":{},\"symbol_uri\":{},\"display_name\":{},",
                    "\"kind\":\"{}\",\"precision\":\"{}\",\"snapshot_id\":{},\"snapshot_nonce\":\"{}\",",
                    "\"snapshot_digest\":\"{}\",\"source_digest\":\"{}\"}},\"relation\":\"{}\",",
                    "\"record_id\":\"{}\",\"record_revision\":{},\"author\":\"{}\",\"rationale\":\"{}\",",
                    "\"provenance\":\"{}\",\"verification\":\"Unverified\",\"support\":\"unverified\"}}"
                ),
                binding_id, crate::json::escape(repo), crate::json::escape(&anchor.path),
                anchor.byte_start, anchor.byte_end, anchor.cursor_line, anchor.cursor_col,
                optional_string(anchor.symbol_uri.as_deref()), optional_string(anchor.display_name.as_deref()),
                crate::json::escape(&anchor.anchor_kind), crate::json::escape(&anchor.precision),
                anchor.snapshot_id, crate::json::escape(&anchor.snapshot_nonce),
                crate::json::escape(&snapshot_digest), crate::json::escape(&anchor.source_digest),
                crate::json::escape(&input.relation), crate::json::escape(&input.record_id),
                input.record_revision, crate::json::escape(&input.author), crate::json::escape(&input.rationale),
                crate::json::escape(&input.provenance)
            );
            *out = response::knowledge_envelope(repo, generation, "bind", &result, None);
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            write_error_code(&e)
        }
    }
}

pub(crate) fn retract_cli(
    repo: &str,
    binding: &str,
    input_flag: &str,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let binding_id = match binding.parse::<i64>() {
        Ok(id) if id > 0 => id,
        _ => {
            err.push_str("error: --binding must be a positive integer\n");
            return 2;
        }
    };
    let bytes = match crate::knowledge_input(input_flag) {
        Ok(bytes) => bytes,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let input = match parse_retract_input(&bytes) {
        Ok(input) => input,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let (_root, mut conn) = match crate::knowledge_open(repo, true, err) {
        Ok(value) => value,
        Err(code) => return code,
    };
    match retract(&mut conn, binding_id, &input) {
        Ok(generation) => {
            let result = format!(
                "{{\"binding_id\":{},\"retracted\":true,\"retraction_author\":\"{}\",\"retraction_reason\":\"{}\"}}",
                binding_id, crate::json::escape(&input.author), crate::json::escape(&input.reason)
            );
            *out = response::knowledge_envelope(repo, generation, "unbind", &result, None);
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            write_error_code(&e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn at_cli(
    repo: &str,
    path: &str,
    line: &str,
    col: &str,
    symbol: Option<&str>,
    limit: Option<&str>,
    offset: Option<&str>,
    generation: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let line = match position(line, "line", err) {
        Ok(n) => n,
        Err(code) => return code,
    };
    let col = match position(col, "col", err) {
        Ok(n) => n,
        Err(code) => return code,
    };
    let (limit, offset, generation) =
        match crate::knowledge_page_args(limit, offset, generation, err) {
            Ok(values) => values,
            Err(code) => return code,
        };
    let (root, entry) = match crate::knowledge_repo(repo, err) {
        Ok(value) => value,
        Err(code) => return code,
    };
    let canon = match paths::canonical_root(Path::new(&root)) {
        Ok(root) => root,
        Err(e) => {
            err.push_str(&format!("error: cannot resolve repository root: {e}\n"));
            return 3;
        }
    };
    let path = match semantic::validate_doc_path(&canon, path) {
        Ok(path) => path,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let db = knowledge_store::knowledge_db_path(&paths::config_dir(), repo);
    let absent = match crate::knowledge_store_absent(&db) {
        Ok(absent) => absent,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    if absent {
        if let Err(e) = knowledge_store::check_missing_store_generation(generation) {
            err.push_str(&format!("error: {e}\n"));
            return if e.starts_with("knowledge generation conflict:") {
                3
            } else {
                2
            };
        }
        let mut page = empty_at(&path, line, col, 0, 0);
        page.limit = limit;
        page.offset = offset;
        let pagination = response::pagination_json(page.limit, 0, 0, false, None);
        *out = response::knowledge_envelope(
            repo,
            0,
            "at",
            &page_json(&page).to_string(),
            Some(pagination),
        );
        return 0;
    }
    let conn = match knowledge_store::open_knowledge(&entry, false) {
        Ok(conn) => conn,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let page = match at(
        &conn,
        &paths::config_dir(),
        &entry,
        &path,
        line,
        col,
        symbol,
        limit,
        offset,
        generation,
    ) {
        Ok(page) => page,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let pagination = response::pagination_json(
        page.limit,
        page.offset,
        page.total,
        page.truncated,
        page.next_offset,
    );
    *out = response::knowledge_envelope(
        repo,
        page.generation,
        "at",
        &page_json(&page).to_string(),
        Some(pagination),
    );
    0
}

#[cfg(test)]
mod review_regressions {
    fn anchor() -> super::CodeAnchor {
        super::CodeAnchor {
            repo: "r".to_string(),
            repo_root_digest: "root".to_string(),
            snapshot_id: 1,
            snapshot_nonce: "old-snapshot".to_string(),
            snapshot_digest: None,
            source_digest: "same-source".to_string(),
            path: "src/lib.rs".to_string(),
            byte_start: 4,
            byte_end: 7,
            cursor_line: 1,
            cursor_col: 5,
            symbol_uri: Some("scip pkg 1.0.0 lib/f().".to_string()),
            display_name: Some("f".to_string()),
            anchor_kind: "semantic".to_string(),
            precision: "Heuristic".to_string(),
        }
    }

    fn binding() -> super::BindingView {
        super::BindingView {
            binding_id: 1,
            repo: "r".to_string(),
            snapshot_id: 2,
            snapshot_nonce: "promoted-snapshot".to_string(),
            snapshot_digest: "snapshot-digest".to_string(),
            source_digest: "same-source".to_string(),
            path: "src/lib.rs".to_string(),
            byte_start: 4,
            byte_end: 7,
            cursor_line: 1,
            cursor_col: 5,
            symbol_uri: Some("scip pkg 1.0.0 lib/f().".to_string()),
            display_name: Some("f".to_string()),
            anchor_kind: "semantic".to_string(),
            precision: "Heuristic".to_string(),
            record_id: "decision".to_string(),
            record_revision: 1,
            record_kind: "decision".to_string(),
            record_title: "Decision".to_string(),
            record_body: "Body".to_string(),
            record_status: "active".to_string(),
            relation: "documents".to_string(),
            author: "test".to_string(),
            rationale: "rationale".to_string(),
            provenance: "test".to_string(),
            created_unix: 1,
            retracted: false,
            retraction_author: None,
            retraction_reason: None,
            retracted_unix: None,
            freshness: "fresh".to_string(),
            freshness_reason: None,
        }
    }

    #[test]
    fn empty_page_has_fixed_order_and_escaped_path() {
        let page = super::empty_at("a\"b\\c.rs", 1, 2, 0, 0);
        assert_eq!(super::page_json(&page).to_string(), "{\"status\":\"no-link-proven\",\"unresolved_reason\":null,\"anchor\":{\"path\":\"a\\\"b\\\\c.rs\",\"line\":1,\"col\":2},\"schema_version\":0,\"bindings\":[]}");
    }
    #[test]
    fn changed_anchor_is_a_state_error() {
        assert_eq!(
            super::write_error_code("code anchor changed before binding was stored"),
            3
        );
        assert_eq!(
            super::write_error_code("code anchor span no longer binds source bytes"),
            3
        );
    }

    #[test]
    fn pinned_identity_accepts_new_snapshot_with_same_source_and_symbol() {
        let anchor = anchor();
        let binding = binding();
        assert_ne!(anchor.snapshot_id, binding.snapshot_id);
        assert_ne!(anchor.snapshot_nonce, binding.snapshot_nonce);
        assert!(super::binding_matches_pinned_anchor(&binding, &anchor));
    }

    #[test]
    fn pinned_identity_rejects_different_symbol_at_same_span() {
        let anchor = anchor();
        let mut binding = binding();
        binding.symbol_uri = Some("scip pkg 1.0.0 lib/g().".to_string());
        assert!(!super::binding_matches_pinned_anchor(&binding, &anchor));
    }
}
