// SPDX-License-Identifier: Apache-2.0
//! M3 atomic indexing: full rebuilds from internal parser facts into a staging
//! database, promoted atomically; plus freshness evaluation with reasons.
//! Queries never trust a timestamp alone — every verdict names its evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::git;
use crate::scan::{self, PlanAction};
use crate::store;
use crate::syntax::{classify, grammar_version, language_name, parse_source, FileKind};
use rusqlite::{params, OptionalExtension};

pub const DEFAULT_SEARCH_LIMIT: usize = 50;
pub const MAX_SEARCH_LIMIT: usize = 1000;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct IndexSummary {
    pub snapshot_id: i64,
    pub status_at_write: String,
    pub pre_fingerprint: String,
    pub post_fingerprint: String,
    pub files_discovered: usize,
    pub processed: usize,
    pub definitions: usize,
    pub imports: usize,
    pub calls: usize,
    pub errors: u32,
    pub skipped_ignored: usize,
    pub skipped_large: usize,
    pub skipped_binary: usize,
    pub skipped_symlink: usize,
    pub skipped_escape: usize,
    pub skipped_unreadable: usize,
    pub unsupported_total: usize,
    pub mode: String,
    pub reused: usize,
    pub reparsed: usize,
    pub added: usize,
    pub removed: usize,
    pub fallback_reason: String,
    pub timings_ms: PhaseMs,
}

/// Milliseconds per build phase, for auditable before/after comparison.
/// No telemetry: one small struct per `index` run, reported in its JSON.
pub struct PhaseMs {
    pub diff: u64,
    pub ingest: u64,
    pub verify: u64,
    pub promote: u64,
}

pub struct Freshness {
    pub status: String,
    pub reasons: Vec<String>,
    pub snapshot: Option<store::Snapshot>,
}

/// Hash of everything that influences scan output except file contents:
/// ignore rules, size policy, parser and grammar versions.
pub fn config_fingerprint(root: &Path) -> String {
    let (rules, info) = crate::ignore::load_rules(root);
    let material = format!(
        "rules={}|file={}|maxbytes={}|parser={}|grammars={}",
        rules.join("\n"),
        info.file_present,
        scan::MAX_FILE_BYTES,
        store::PARSER_VERSION,
        store::grammar_versions()
    );
    store::content_digest(material.as_bytes())
}

/// Point-in-time input description: git state plus config. Compared before
/// and after a build, and against the stored snapshot at query time.
pub fn input_fingerprint(root: &Path) -> String {
    let g = git::fingerprint(root);
    format!(
        "present={}|valid={}|head={}|branch={}|dirty={}|untracked={}|cfg={}",
        g.present,
        g.valid,
        g.head.as_deref().unwrap_or("-"),
        g.branch.as_deref().unwrap_or("-"),
        g.dirty,
        g.untracked,
        config_fingerprint(root)
    )
}

/// Staging setup shared by full and incremental builds: everything before
/// transaction creation. The 3-line transaction-open + snapshot insert stays
/// inline at each caller (a returned `Transaction` cannot borrow a returned
/// `Connection`).
struct StagingMeta {
    canon_root: PathBuf,
    git_state: git::GitInfo,
    cfg: String,
    pre: String,
    base: store::PromoteBase,
}

fn prepare_staging(
    home: &Path,
    name: &str,
    root: &Path,
) -> Result<(rusqlite::Connection, PathBuf, PathBuf, StagingMeta), String> {
    let db = store::db_path(home, name);
    let staging = store::staging_path(home, name);
    if let Some(parent) = db.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create index dir: {e}"))?;
        }
    }
    if staging.exists() && std::fs::remove_file(&staging).is_err() {
        return Err("stale staging file is locked by another build".to_string());
    }
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve root: {e}"))?;
    let git_state = git::fingerprint(&canon_root);
    let cfg = config_fingerprint(&canon_root);
    let pre = input_fingerprint(&canon_root);
    // Writer-coordination base: the live revision this staging build starts
    // from. Promotion aborts unless it still matches (semantic-changed).
    let base = if db.is_file() {
        store::open_live(&db)
            .and_then(|live| store::live_base(&live))
            .unwrap_or(store::PromoteBase {
                nonce: String::new(),
                generation: 0,
            })
    } else {
        store::PromoteBase {
            nonce: String::new(),
            generation: 0,
        }
    };
    let conn = store::open_live(&staging)?;
    Ok((
        conn,
        db,
        staging,
        StagingMeta {
            canon_root,
            git_state,
            cfg,
            pre,
            base,
        },
    ))
}

/// Shared build epilogue, first half: source verification, freshness
/// verdict, snapshot finish, commit. Both pipelines end here, so neither
/// can drift apart.
fn finish_snapshot_tx(
    tx: rusqlite::Transaction<'_>,
    snapshot_id: i64,
    summary: &mut IndexSummary,
    meta: &StagingMeta,
) -> Result<(), String> {
    let t_verify = Instant::now();
    let post = input_fingerprint(&meta.canon_root);
    // Content evidence at write time: re-verify every stored digest against
    // current source before claiming fresh. A mid-build content flip that git
    // cannot see still fails here. `fresh` requires all three agreements.
    let content_verified =
        verify_snapshot_content(&tx, snapshot_id, &meta.canon_root).unwrap_or(false);
    let status = if summary.pre_fingerprint == post && content_verified {
        "fresh"
    } else {
        "stale"
    };
    store::finish_snapshot(&tx, snapshot_id, &post, status)?;
    tx.commit().map_err(|e| e.to_string())?;
    summary.timings_ms.verify = t_verify.elapsed().as_millis() as u64;
    summary.status_at_write = status.to_string();
    summary.post_fingerprint = post;
    Ok(())
}

/// Shared build epilogue, second half: atomic promotion plus staging
/// cleanup. Split from the first half so callers drop the staging
/// connection first — on Windows an open handle blocks file removal, which
/// left corrupt staging leftovers behind (regression-tested).
fn promote_live(db: &Path, staging: &Path, base: &store::PromoteBase) -> Result<u64, String> {
    let t_promote = Instant::now();
    let live = match store::open_live(db) {
        Ok(live) => live,
        Err(_) if !store::is_readable_db(db) => {
            // Corrupt live database with a complete staging build ready:
            // everything is re-derivable from source, so replace wholesale.
            std::fs::remove_file(db)
                .map_err(|e| format!("live index is corrupt and cannot be replaced: {e}"))?;
            store::open_live(db)?
        }
        Err(e) => return Err(e),
    };
    store::promote_staging(&live, staging, base)?;
    drop(live);
    let _ = std::fs::remove_file(staging);
    Ok(t_promote.elapsed().as_millis() as u64)
}

/// One fresh snapshot row from build metadata. The nonce is minted by the
/// caller so the carry-over binding can name the new revision up front.
fn new_snapshot_row(name: &str, meta: &StagingMeta, nonce: &str) -> store::Snapshot {
    store::Snapshot {
        id: 0,
        repo: name.to_string(),
        root: meta.canon_root.to_string_lossy().to_string(),
        created_unix: now_unix(),
        head: meta.git_state.head.clone(),
        branch: meta.git_state.branch.clone(),
        dirty: meta.git_state.dirty,
        untracked: meta.git_state.untracked,
        parser_version: store::PARSER_VERSION,
        grammar_versions: store::grammar_versions(),
        config_fingerprint: meta.cfg.clone(),
        pre_fingerprint: meta.pre.clone(),
        post_fingerprint: String::new(),
        status_at_write: "stale".to_string(),
        nonce: nonce.to_string(),
    }
}

/// Carry live semantic imports into the staging snapshot (both pipelines).
/// `src` must already be attached to the live database. Facts carry only
/// through digest-verified file rows; import rows rebind to the new snapshot
/// while keeping their original content identity (locatability, never proof).
/// No live snapshot at all (cold start): nothing to carry.
fn carry_semantic(
    tx: &rusqlite::Transaction<'_>,
    snapshot_id: i64,
    meta: &StagingMeta,
    nonce: &str,
) -> Result<store::CarriedTotals, String> {
    let old_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM src.snapshots ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some(old_id) = old_id else {
        return Ok(store::CarriedTotals {
            imports: 0,
            facts: 0,
        });
    };
    let content = store::content_identity(tx, snapshot_id)?;
    let live = crate::semantic::LiveState {
        head: meta.git_state.head.clone().unwrap_or("-".to_string()),
        branch: meta.git_state.branch.clone().unwrap_or("-".to_string()),
        dirty: meta.git_state.dirty,
        untracked: meta.git_state.untracked,
        config_fingerprint: meta.cfg.clone(),
        snapshot_nonce: nonce.to_string(),
        content_identity: content,
    };
    store::carry_semantic_facts(
        tx,
        snapshot_id,
        old_id,
        &crate::semantic::revision_string(&live),
        &|rel| {
            let mut disk_path = meta.canon_root.clone();
            for part in rel.split('/') {
                disk_path.push(part);
            }
            std::fs::read(&disk_path)
                .ok()
                .map(|bytes| store::content_digest(&bytes))
        },
    )
}
/// Full rebuild into staging, then atomic promotion. Facts stream from the
/// internal parser straight into one transaction — the whole repository is
/// never buffered, and readers never see a half-written snapshot.
/// Production routes through `build`; this stays as the M3-compatible entry
/// point and the test oracle for parity.
#[allow(dead_code)]
pub fn build_full(home: &Path, name: &str, root: &Path) -> Result<IndexSummary, String> {
    build_full_inner(home, name, root, "full", "none")
}

/// Shared full-rebuild body; `build` sets mode/reason for cold, forced, and
/// fallback invocations.
fn build_full_inner(
    home: &Path,
    name: &str,
    root: &Path,
    mode: &str,
    fallback_reason: &str,
) -> Result<IndexSummary, String> {
    let (mut conn, db, staging, meta) = prepare_staging(home, name, root)?;
    // ATTACH before the write transaction starts (same rule as the
    // incremental path: attaching inside one reports the source locked).
    // A readable live database that refuses the attach aborts the build
    // rather than silently wiping imports a rebuild cannot re-derive.
    let mut carried = false;
    if db.is_file() {
        if store::attach_src(&conn, &db).is_err() {
            if store::is_readable_db(&db) {
                return Err("live index unreadable for semantic carry".to_string());
            }
        } else {
            carried = true;
        }
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let nonce = store::new_nonce();
    let snapshot_id = store::insert_snapshot(&tx, &new_snapshot_row(name, &meta, &nonce))?;
    let t_diff = Instant::now();
    let (plan, _) = scan::plan_walk(&meta.canon_root)?;
    let diff_ms = t_diff.elapsed().as_millis() as u64;
    let mut summary = IndexSummary {
        snapshot_id,
        status_at_write: "stale".to_string(),
        pre_fingerprint: meta.pre.clone(),
        post_fingerprint: String::new(),
        files_discovered: 0,
        processed: 0,
        definitions: 0,
        imports: 0,
        calls: 0,
        errors: 0,
        skipped_ignored: 0,
        skipped_large: 0,
        skipped_binary: 0,
        skipped_symlink: 0,
        skipped_escape: 0,
        skipped_unreadable: 0,
        unsupported_total: 0,
        mode: mode.to_string(),
        reused: 0,
        reparsed: 0,
        added: 0,
        removed: 0,
        fallback_reason: fallback_reason.to_string(),
        timings_ms: PhaseMs {
            diff: diff_ms,
            ingest: 0,
            verify: 0,
            promote: 0,
        },
    };
    let t_ingest = Instant::now();
    for item in &plan {
        ingest_file(&tx, snapshot_id, item, &mut summary)?;
    }
    summary.timings_ms.ingest = t_ingest.elapsed().as_millis() as u64;
    summary.reparsed = summary.processed;
    if carried {
        carry_semantic(&tx, snapshot_id, &meta, &nonce)?;
    }
    // Graph lifecycle: re-derive from this snapshot's own facts after the
    // semantic carry-over rebound every import. Edges are never copied.
    crate::edges::rederive_after_carry(&tx, snapshot_id)?;
    finish_snapshot_tx(tx, snapshot_id, &mut summary, &meta)?;
    if carried {
        store::detach_src(&conn)?;
    }
    drop(conn);
    summary.timings_ms.promote = promote_live(&db, &staging, &meta.base)?;
    Ok(summary)
}

/// One file through the shared ingest path: skip counting, content gates,
/// parse, fact inserts. Used by full builds for every file and by
/// incremental builds for added/modified files only. Returns whether the
/// file was actually parsed (gate-skipped files cost a hash, not a parse).
fn ingest_file(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    item: &scan::PlannedFile,
    summary: &mut IndexSummary,
) -> Result<bool, String> {
    summary.files_discovered += 1;
    if let PlanAction::Skip(reason) = &item.action {
        match *reason {
            "ignored" => summary.skipped_ignored += 1,
            "symlink-not-followed" => summary.skipped_symlink += 1,
            _ => summary.skipped_unreadable += 1,
        }
        return Ok(false);
    }
    // Content gates before the extension verdict, mirroring scan render:
    // a huge or binary `.xyz` counts as skipped, not unsupported.
    let bytes = match std::fs::read(&item.full) {
        Ok(b) => b,
        Err(_) => {
            summary.skipped_unreadable += 1;
            return Ok(false);
        }
    };
    if bytes.len() as u64 > scan::MAX_FILE_BYTES {
        summary.skipped_large += 1;
        return Ok(false);
    }
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        summary.skipped_binary += 1;
        return Ok(false);
    }
    match &item.action {
        PlanAction::Parse(lang) => {
            let facts = parse_source(*lang, &bytes);
            crate::progress::tick();
            let file_id = store::insert_file(
                conn,
                snapshot_id,
                &store::FileRow {
                    path: item.rel.clone(),
                    language: language_name(*lang).to_string(),
                    bytes: bytes.len() as u64,
                    digest: store::content_digest(&bytes),
                    errors: facts.errors,
                },
            )?;
            let grammar = grammar_version(*lang).to_string();
            let language = language_name(*lang).to_string();
            for definition in &facts.definitions {
                store::insert_symbol(
                    conn,
                    snapshot_id,
                    &store::SymbolRow {
                        file_id,
                        kind: definition.kind.to_string(),
                        name: definition.name.clone(),
                        start_line: definition.span.start_line,
                        start_col: definition.span.start_col,
                        end_line: definition.span.end_line,
                        end_col: definition.span.end_col,
                        byte_start: definition.byte_start,
                        byte_end: definition.byte_end,
                        provider: "syntax".to_string(),
                        grammar: grammar.clone(),
                        language: language.clone(),
                    },
                )?;
            }
            for import in &facts.imports {
                store::insert_import(
                    conn,
                    snapshot_id,
                    &store::ImportRow {
                        file_id,
                        text: import.text.clone(),
                        start_line: import.span.start_line,
                        start_col: import.span.start_col,
                        end_line: import.span.end_line,
                        end_col: import.span.end_col,
                        byte_start: import.byte_start,
                        byte_end: import.byte_end,
                        provider: "syntax".to_string(),
                        grammar: grammar.clone(),
                        language: language.clone(),
                    },
                )?;
            }
            for call in &facts.calls {
                let span_cols =
                    |span: &Option<crate::syntax::CalleeSpan>| -> Option<store::SpanCols> {
                        span.as_ref().map(|s| store::SpanCols {
                            start_line: s.start_line,
                            start_col: s.start_col,
                            end_line: s.end_line,
                            end_col: s.end_col,
                            byte_start: s.byte_start,
                            byte_end: s.byte_end,
                        })
                    };
                store::insert_call(
                    conn,
                    snapshot_id,
                    &store::CallRow {
                        file_id,
                        name: call.name.clone(),
                        start_line: call.span.start_line,
                        start_col: call.span.start_col,
                        end_line: call.span.end_line,
                        end_col: call.span.end_col,
                        byte_start: call.byte_start,
                        byte_end: call.byte_end,
                        callee: span_cols(&call.callee),
                        tcallee: span_cols(&call.tcallee),
                        unresolved: 1,
                        provider: "syntax".to_string(),
                        grammar: grammar.clone(),
                        language: language.clone(),
                    },
                )?;
            }
            for occurrence in &facts.reference_occurrences {
                let Some(name) = occurrence.name.clone() else {
                    continue;
                };
                store::insert_reference_occurrence(
                    conn,
                    snapshot_id,
                    &store::ReferenceOccurrenceRow {
                        file_id,
                        name,
                        role: occurrence.kind.to_string(),
                        start_line: occurrence.span.start_line,
                        start_col: occurrence.span.start_col,
                        end_line: occurrence.span.end_line,
                        end_col: occurrence.span.end_col,
                        byte_start: occurrence.byte_start,
                        byte_end: occurrence.byte_end,
                        provider: "syntax".to_string(),
                        grammar: grammar.clone(),
                        language: language.clone(),
                    },
                )?;
            }
            summary.processed += 1;
            summary.definitions += facts.definitions.len();
            summary.imports += facts.imports.len();
            summary.calls += facts.calls.len();
            summary.errors += facts.errors;
            return Ok(true);
        }
        PlanAction::Unsupported(_) => summary.unsupported_total += 1,
        PlanAction::Skip(_) => {} // unreachable here; listed for exhaustiveness
    }
    Ok(false)
}

/// Per-file classification of a fresh walk against a stored snapshot:
/// owned paths only, no lifetime coupling to the plan.
struct FileDiff {
    /// (rel path, old file rowid) with byte-identical current content.
    unchanged: Vec<(String, i64)>,
    /// Rel paths to run through `ingest_file` (modified, added, and
    /// gate-skipped files alike — ingest counts skips exactly like full).
    /// Asserted by unit tests; production routes the single ordered walk by
    /// plan-minus-unchanged instead of a second set.
    #[allow(dead_code)]
    ingest_rels: Vec<String>,
    /// Stored paths with no current `Parse` item (deleted, now-ignored,
    /// now-unsupported): facts dropped, nothing ingested.
    removed: Vec<String>,
    /// Stored paths whose current item fails the content gates (now too
    /// large, binary, or unreadable): routed through ingest for skip
    /// counting, old facts dropped.
    dropped: Vec<String>,
}

/// Compare a fresh walk against stored (file id, digest) rows. Gate logic
/// mirrors `ingest_file` (as `content_diff` already does): only Parse items
/// passing every gate with a matching digest count as unchanged.
fn diff_files(
    stored: &BTreeMap<String, (i64, String)>,
    plan: &[scan::PlannedFile],
    _canon_root: &Path,
) -> FileDiff {
    let mut unchanged = Vec::new();
    let mut ingest_rels = Vec::new();
    let mut removed = Vec::new();
    let mut dropped = Vec::new();
    let mut seen_parse = BTreeSet::new();
    for item in plan {
        if !matches!(item.action, scan::PlanAction::Parse(_)) {
            continue;
        }
        seen_parse.insert(item.rel.clone());
        let bytes = match std::fs::read(&item.full) {
            Ok(b) => b,
            Err(_) => {
                ingest_rels.push(item.rel.clone());
                if stored.contains_key(&item.rel) {
                    dropped.push(item.rel.clone());
                }
                continue;
            }
        };
        if bytes.len() as u64 > scan::MAX_FILE_BYTES
            || bytes.contains(&0)
            || std::str::from_utf8(&bytes).is_err()
        {
            ingest_rels.push(item.rel.clone());
            if stored.contains_key(&item.rel) {
                dropped.push(item.rel.clone());
            }
            continue;
        }
        match stored.get(&item.rel) {
            Some((fid, digest)) if *digest == store::content_digest(&bytes) => {
                unchanged.push((item.rel.clone(), *fid));
            }
            _ => ingest_rels.push(item.rel.clone()),
        }
    }
    for path in stored.keys() {
        if !seen_parse.contains(path) {
            removed.push(path.clone());
        }
    }
    FileDiff {
        unchanged,
        ingest_rels,
        removed,
        dropped,
    }
}

/// Incremental rebuild: copy digest-verified files set-based from the old
/// snapshot, parse only the rest. `stored` and `old` must come from one
/// live read transaction (see `build`); the copy itself runs inside the
/// staging transaction over the ATTACHed live database, so all source reads
/// share that transaction's consistent view — one revision, never a mix.
/// A source row vanishing under us (concurrent replacement) copies zero
/// rows, and the file falls through to ingest from current source.
fn build_incremental(
    home: &Path,
    name: &str,
    root: &Path,
    old: &store::Snapshot,
    stored: &BTreeMap<String, (i64, String)>,
    live_db: &Path,
) -> Result<IndexSummary, String> {
    let (mut conn, db, staging, meta) = prepare_staging(home, name, root)?;
    let t_diff = Instant::now();
    let (plan, _) = scan::plan_walk(&meta.canon_root)?;
    let diff = diff_files(stored, &plan, &meta.canon_root);
    let diff_ms = t_diff.elapsed().as_millis() as u64;
    // ATTACH on the staging connection before its write transaction starts:
    // attaching inside an active write transaction reports the source as
    // locked. Reads still share the transaction's consistent view below.
    if store::attach_src(&conn, live_db).is_err() {
        // Live database unreadable mid-refresh with staging prepared: drop
        // everything and take the explicit full-rebuild recovery.
        drop(conn);
        let _ = std::fs::remove_file(&staging);
        return build_full_inner(home, name, root, "fallback-full", "db-unreadable");
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let nonce = store::new_nonce();
    let snapshot_id = store::insert_snapshot(&tx, &new_snapshot_row(name, &meta, &nonce))?;
    // Pin the copy to the exact revision classification saw: nonces are
    // never reused across rebuilds, so a mismatch means the live database
    // was replaced under us. Roll back and rebuild fully from source.
    let live_nonce: Option<String> = tx
        .query_row(
            "SELECT nonce FROM src.snapshots WHERE id = ?1",
            [old.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if live_nonce.as_deref() != Some(old.nonce.as_str()) {
        drop(tx);
        drop(conn);
        let _ = std::fs::remove_file(&staging);
        return build_full_inner(home, name, root, "fallback-full", "revision-changed");
    }
    store::create_file_map(&tx)?;
    let unchanged: BTreeSet<&str> = diff.unchanged.iter().map(|(p, _)| p.as_str()).collect();
    let mut summary = IndexSummary {
        snapshot_id,
        status_at_write: "stale".to_string(),
        pre_fingerprint: meta.pre.clone(),
        post_fingerprint: String::new(),
        files_discovered: 0,
        processed: 0,
        definitions: 0,
        imports: 0,
        calls: 0,
        errors: 0,
        skipped_ignored: 0,
        skipped_large: 0,
        skipped_binary: 0,
        skipped_symlink: 0,
        skipped_escape: 0,
        skipped_unreadable: 0,
        unsupported_total: 0,
        mode: "incremental".to_string(),
        reused: 0,
        reparsed: 0,
        added: 0,
        removed: diff.removed.len() + diff.dropped.len(),
        fallback_reason: "none".to_string(),
        timings_ms: PhaseMs {
            diff: diff_ms,
            ingest: 0,
            verify: 0,
            promote: 0,
        },
    };
    let t_ingest = Instant::now();
    for item in &plan {
        if matches!(item.action, scan::PlanAction::Parse(_))
            && unchanged.contains(item.rel.as_str())
        {
            // `reparsed` counts files actually parsed; gate-skipped files
            // cost a hash, not a parse, so they never inflate it. The stored
            // digest pins the copy: same bytes or nothing.
            if let Some((old_fid, digest)) = stored.get(item.rel.as_str()) {
                summary.files_discovered += 1;
                match store::copy_file_row(&tx, snapshot_id, old.id, *old_fid, &item.rel, digest)? {
                    Some(new_fid) => {
                        store::record_file_map(&tx, *old_fid, new_fid)?;
                        summary.reused += 1;
                        summary.processed += 1;
                        continue;
                    }
                    None => {
                        // Source row vanished or mismatched under us: ingest
                        // from current source (ingest counts the file; undo
                        // our increment).
                        if ingest_file(&tx, snapshot_id, item, &mut summary)? {
                            summary.reparsed += 1;
                        }
                        summary.files_discovered -= 1;
                        continue;
                    }
                }
            }
        }
        let was_stored = stored.contains_key(&item.rel);
        if ingest_file(&tx, snapshot_id, item, &mut summary)? {
            summary.reparsed += 1;
            if !was_stored {
                summary.added += 1;
            }
        }
    }
    // One joined bulk copy per fact table: per-file filtered child SELECTs
    // would scan the whole snapshot once per file (quadratic at MMSP scale).
    let totals = store::copy_mapped_facts(&tx, snapshot_id, old.id)?;
    summary.definitions += totals.symbols as usize;
    summary.imports += totals.imports as usize;
    summary.calls += totals.calls as usize;
    summary.errors += totals.errors;
    summary.timings_ms.ingest = t_ingest.elapsed().as_millis() as u64;
    // Detach after commit: detaching inside the write transaction reports
    // the source locked, and the connection is dropped right below anyway.
    carry_semantic(&tx, snapshot_id, &meta, &nonce)?;
    // Same graph rule as the full path: re-derive, never copy edges.
    crate::edges::rederive_after_carry(&tx, snapshot_id)?;
    finish_snapshot_tx(tx, snapshot_id, &mut summary, &meta)?;
    store::detach_src(&conn)?;
    drop(conn);
    summary.timings_ms.promote = promote_live(&db, &staging, &meta.base)?;
    Ok(summary)
}

/// Full rebuild with an explicit mode and fallback reason. `build_full`
/// stays the plain M3 entry point (`"full"` / `"none"`).
fn fallback_full(
    home: &Path,
    name: &str,
    root: &Path,
    reason: &str,
) -> Result<IndexSummary, String> {
    build_full_inner(home, name, root, "fallback-full", reason)
}

/// Build the live source state for a stored snapshot: git/config fields
/// from the row, content identity recomputed over its files rows.
fn live_state_for(
    conn: &rusqlite::Connection,
    snapshot: &store::Snapshot,
) -> Result<crate::semantic::LiveState, String> {
    Ok(crate::semantic::LiveState {
        head: snapshot.head.clone().unwrap_or("-".to_string()),
        branch: snapshot.branch.clone().unwrap_or("-".to_string()),
        dirty: snapshot.dirty,
        untracked: snapshot.untracked,
        config_fingerprint: snapshot.config_fingerprint.clone(),
        snapshot_nonce: snapshot.nonce.clone(),
        content_identity: store::content_identity(conn, snapshot.id)?,
    })
}

fn canonical_repo_root(root: &Path) -> Result<PathBuf, String> {
    root.canonicalize()
        .map_err(|e| format!("cannot resolve root: {e}"))
}

/// Per-import semantic source check: every file an import's facts cover
/// must still digest-match raw source bytes. M4 freshness only sees files
/// with syntax rows, so unindexed (`file_id IS NULL`) documents need this
/// own check — otherwise an edited unindexed file keeps reading Unverified.
/// Missing/unreadable/oversize/escaping files count as changed (Stale),
/// never as matching. Keeps import-granular invalidation: one import's
/// drift never taints another's. No new evaluator: digests + the shared
/// confined reader only.
fn import_source_changed(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    import_id: i64,
    canon_root: &Path,
) -> Result<bool, String> {
    let mut statement = conn
        .prepare("SELECT DISTINCT rel_path, file_digest FROM semantic_facts WHERE snapshot_id = ?1 AND import_id = ?2")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![snapshot_id, import_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (rel, digest) = row.map_err(|e| e.to_string())?;
        match crate::semantic::confined_read(canon_root, &rel, crate::scan::MAX_FILE_BYTES) {
            Ok(bytes) if store::content_digest(&bytes) == digest => {}
            _ => return Ok(true),
        }
    }
    Ok(false)
}

pub struct SemanticImportOutcome {
    pub snapshot_id: i64,
    pub import_id: i64,
    pub provider_name: String,
    pub provider_version: String,
    pub documents: usize,
    pub definitions: usize,
    pub references: usize,
    pub skipped: Vec<String>,
    pub verification: String,
}

/// Import one SCIP file against the live latest snapshot. Never triggers a
/// syntax rebuild: one `BEGIN IMMEDIATE` transaction, rolled back on any
/// error with the syntax snapshot byte-identical. No new snapshot row is
/// minted by the import itself.
pub fn semantic_import(
    home: &Path,
    name: &str,
    root: &Path,
    scip_path: &Path,
    claimed: Option<&str>,
) -> Result<SemanticImportOutcome, String> {
    let canon = canonical_repo_root(root)?;
    let db = store::db_path(home, name);
    let conn = store::open_live(&db)?;
    conn.execute_batch("BEGIN IMMEDIATE;")
        .map_err(|e| e.to_string())?;
    let outcome = (|| -> Result<SemanticImportOutcome, String> {
        let latest = store::latest_snapshot(&conn)?
            .ok_or_else(|| "no snapshot: index the repository first".to_string())?;
        if canon.to_string_lossy() != latest.root {
            return Err("repository root moved".to_string());
        }
        let out = crate::scip::import_file(
            &conn,
            latest.id,
            &canon,
            scip_path,
            &crate::scip::ImportLimits::default(),
        )?;
        if let Some(claim) = claimed {
            conn.execute(
                "UPDATE semantic_imports SET claimed_source_digest = ?1 WHERE id = ?2",
                params![claim, out.import_id],
            )
            .map_err(|e| e.to_string())?;
        }
        let row = store::semantic_import_by_id(&conn, out.import_id)?
            .ok_or_else(|| "import vanished".to_string())?;
        // Same source-freshness gate as query/status: a source that already
        // moved (dirty worktree included) reports Stale, never Unverified.
        let (source_status, _) = evaluate_snapshot(&conn, name, &canon, &latest);
        let drifted = import_source_changed(&conn, latest.id, out.import_id, &canon)?;
        let verification = if source_status == "fresh" && !drifted {
            row.1.verification.clone()
        } else {
            "Stale".to_string()
        };
        Ok(SemanticImportOutcome {
            snapshot_id: latest.id,
            import_id: out.import_id,
            provider_name: row.1.provider_name.clone(),
            provider_version: row.1.provider_version.clone(),
            documents: out.documents,
            definitions: out.definitions,
            references: out.references,
            skipped: out.skipped,
            verification,
        })
    })();
    match outcome {
        Ok(out) => {
            conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;
            Ok(out)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

pub struct SemanticStatusRow {
    pub import_id: i64,
    pub provider_name: String,
    pub provider_version: String,
    pub capability: String,
    pub verification: String,
    pub artifact_id: String,
    pub claimed_source_digest: String,
    pub content_match: bool,
    pub revision_match: bool,
    pub limitation: String,
}

/// Per-provider import status against the live latest snapshot, each passed
/// through `verify_live`. Syntax freshness is reported separately by
/// `index status`; this answers only the semantic binding question.
pub fn semantic_status(
    home: &Path,
    name: &str,
    root: &Path,
) -> Result<(Option<store::Snapshot>, Vec<SemanticStatusRow>), String> {
    let canon = canonical_repo_root(root)?;
    let db = store::db_path(home, name);
    let mut conn = store::open_live(&db)?;
    // One read transaction across snapshot, facts, imports, nonce, and the
    // freshness verdict: every answer describes exactly one revision, even
    // if a rebuild or import commits mid-query (WAL snapshot isolation).
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let latest = store::latest_snapshot(&tx)?;
    let Some(snapshot) = latest else {
        return Ok((None, Vec::new()));
    };
    // Source freshness from the M3-M4 evaluator on the same transaction:
    // a source that moved after the snapshot (no rebuild yet) downgrades
    // every import to Stale instead of serving old rows as matching.
    let (source_status, _) = evaluate_snapshot(&tx, name, &canon, &snapshot);
    let source_fresh = source_status == "fresh";
    let live = live_state_for(&tx, &snapshot)?;
    let mut rows = Vec::new();
    for (id, import) in store::semantic_imports_for(&tx, snapshot.id)? {
        let bound = crate::semantic::verify_live(&import, &live);
        // Import-granular drift: this import's own covered files moved, even
        // where the snapshot-wide freshness is blind (unindexed documents).
        let drifted = import_source_changed(&tx, snapshot.id, id, &canon)?;
        let verification = if source_fresh && !drifted {
            bound
        } else {
            crate::semantic::Verification::Stale
        };
        // Every stored row came through the SCIP importer, so its capability is
        // the scip-import transport's. The producer name and version are
        // artifact metadata; riltra checks them against no registry.
        let limitation = match crate::semantic::resolve(crate::semantic::SCIP_IMPORT) {
            Ok(_) => "single-hop def/ref only (M6 owns traversal); Current unreachable without independent producer attestation; producer name and version are artifact metadata, not checked by riltra".to_string(),
            Err(e) => e,
        };
        rows.push(SemanticStatusRow {
            import_id: id,
            provider_name: import.provider_name.clone(),
            provider_version: import.provider_version.clone(),
            capability: import.capability.clone(),
            verification: verification.as_str().to_string(),
            artifact_id: import.artifact_id.clone(),
            claimed_source_digest: import.claimed_source_digest.clone(),
            content_match: import.content_identity == live.content_identity,
            revision_match: import.source_revision == crate::semantic::revision_string(&live),
            limitation,
        });
    }
    Ok((Some(snapshot), rows))
}

pub struct SemanticHit {
    pub symbol_uri: Option<String>,
    pub display_name: Option<String>,
    pub path: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub roles: u32,
    pub precision: String,
    pub verification: String,
}

pub struct SemanticQueryOut {
    pub definitions: Vec<SemanticHit>,
    pub references: Vec<SemanticHit>,
    pub provider_name: String,
    pub provider_version: String,
    pub verification: String,
    pub server_diagnostics: Vec<String>,
    pub fallback: Option<String>,
}

/// One positioned lookup: 1-based UI coordinates plus the refs switch.
/// Bundles the query tail so `semantic_query`/`semantic_query_lsp` stay
/// under the argument-count lint without losing explicitness.
pub struct SpanQuery<'a> {
    pub rel: &'a str,
    pub line1: u32,
    pub col1: u32,
    pub refs: bool,
}

fn hit_from_fact(fact: &store::SemanticFact, verification: &str) -> SemanticHit {
    SemanticHit {
        symbol_uri: fact.symbol_uri.clone(),
        display_name: fact.display_name.clone(),
        path: fact.rel_path.clone(),
        start_line: fact.start_line,
        start_col: fact.start_col,
        end_line: fact.end_line,
        end_col: fact.end_col,
        byte_start: fact.byte_start,
        byte_end: fact.byte_end,
        roles: fact.roles,
        precision: fact.precision.clone(),
        verification: verification.to_string(),
    }
}

/// One semantic hit from one provider fact. The fact's own precision and
/// verification travel with it: a fact the adapter marked stale or
/// heuristic never reaches the caller as a current, exact fact.
fn lsp_hit_from_fact(fact: &crate::semantic::Evidence) -> SemanticHit {
    let span = &fact.location.span;
    SemanticHit {
        symbol_uri: None,
        display_name: None,
        path: fact.location.path.clone(),
        start_line: span.start_line,
        start_col: span.start_col,
        end_line: span.end_line,
        end_col: span.end_col,
        byte_start: span.byte_start,
        byte_end: span.byte_end,
        roles: fact.roles.raw,
        precision: fact.precision.as_str().to_string(),
        verification: fact.verification.as_str().to_string(),
    }
}

/// The verdict of an LSP answer: the weakest fact in it. A fact the adapter
/// marked stale makes the whole answer stale, never the other way round.
fn lsp_answer_verification(definitions: &[SemanticHit], references: &[SemanticHit]) -> String {
    if definitions
        .iter()
        .chain(references.iter())
        .any(|hit| hit.verification == crate::semantic::Verification::Stale.as_str())
    {
        crate::semantic::Verification::Stale.as_str().to_string()
    } else {
        crate::semantic::Verification::Unverified
            .as_str()
            .to_string()
    }
}

/// The reason of a failed LSP query. The server may have said why in its
/// diagnostics, and those must not be lost: they travel with the reason.
fn lsp_fallback_reason(error: &str, diagnostics: &[String]) -> String {
    if diagnostics.is_empty() {
        return error.to_string();
    }
    format!("{error}; server diagnostics: {}", diagnostics.join(" | "))
}

/// Per-import verdict memo: (verification, provider_name, provider_version).
/// One query resolves each import once no matter how many of its facts
/// match; the map is created fresh per query — never shared across requests,
/// so no cross-request cache can go stale. Guards are identical to the
/// unmemoized path: same binding check, same drift check, same values.
fn import_verdict(
    tx: &rusqlite::Connection,
    snapshot: &store::Snapshot,
    live: &crate::semantic::LiveState,
    source_fresh: bool,
    canon: &Path,
    import_id: i64,
    memo: &mut BTreeMap<i64, (String, String, String)>,
) -> Result<(String, String, String), String> {
    if let Some(hit) = memo.get(&import_id) {
        return Ok(hit.clone());
    }
    let import = store::semantic_import_by_id(tx, import_id)?
        .ok_or_else(|| "import vanished".to_string())?;
    let bound = crate::semantic::verify_live(&import.1, live);
    let drifted = import_source_changed(tx, snapshot.id, import_id, canon)?;
    let verification = if source_fresh && !drifted {
        bound
    } else {
        crate::semantic::Verification::Stale
    };
    let verdict = (
        verification.as_str().to_string(),
        import.1.provider_name.clone(),
        import.1.provider_version.clone(),
    );
    memo.insert(import_id, verdict.clone());
    Ok(verdict)
}

/// Single-hop semantic query over stored SCIP facts. Go to Definition
/// resolves the enclosing occurrence to its symbol identity and returns the
/// Definition-role rows; `--refs` returns the reference-role rows sharing
/// that identity. Local symbols never leave their document. No identity →
/// explicit unresolved error, never a guess. No traversal (M6 boundary).
pub fn semantic_query(
    home: &Path,
    name: &str,
    root: &Path,
    query: &SpanQuery<'_>,
) -> Result<SemanticQueryOut, String> {
    let canon = canonical_repo_root(root)?;
    let validated = crate::semantic::validate_doc_path(&canon, query.rel)?;
    let db = store::db_path(home, name);
    let mut conn = store::open_live(&db)?;
    // One read transaction across snapshot, facts, imports, nonce, and the
    // freshness verdict (same discipline as `evaluate`): no concurrent
    // rebuild or import can mix revisions inside one answer.
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let snapshot = store::latest_snapshot(&tx)?
        .ok_or_else(|| "no snapshot: index the repository first".to_string())?;
    let (source_status, _) = evaluate_snapshot(&tx, name, &canon, &snapshot);
    let source_fresh = source_status == "fresh";
    let live = live_state_for(&tx, &snapshot)?;
    let line0 = query.line1.saturating_sub(1);
    let col0 = query.col1.saturating_sub(1);
    let facts = store::semantic_facts_at(&tx, snapshot.id, &validated)?;
    // Narrowest enclosing occurrence wins (nested spans resolve inward).
    // Half-open [start, end): the end edge itself is outside the symbol.
    let mut anchor: Option<&store::SemanticFact> = None;
    for fact in &facts {
        let inside = (fact.start_line, fact.start_col) <= (line0, col0)
            && (line0, col0) < (fact.end_line, fact.end_col);
        if !inside {
            continue;
        }
        let width = fact.byte_end.saturating_sub(fact.byte_start);
        match &anchor {
            Some(a) if a.byte_end.saturating_sub(a.byte_start) <= width => {}
            _ => anchor = Some(fact),
        }
    }
    let anchor = anchor.ok_or_else(|| "no semantic occurrence here".to_string())?;
    let uri = anchor
        .symbol_uri
        .clone()
        .ok_or_else(|| "occurrence has no symbol identity: unresolved".to_string())?;
    let local = uri == "local" || uri.starts_with("local ");
    let mut definitions = Vec::new();
    let mut references = Vec::new();
    let mut verification = "Unverified".to_string();
    let mut memo = BTreeMap::new();
    for fact in store::semantic_facts_for(&tx, snapshot.id, &uri)? {
        if local && fact.rel_path != validated {
            continue;
        }
        // Identity scope includes the producing import: the same URI from
        // another import is different evidence, never merged.
        if fact.import_id != anchor.import_id {
            continue;
        }
        let (vstr, _, _) = import_verdict(
            &tx,
            &snapshot,
            &live,
            source_fresh,
            &canon,
            fact.import_id,
            &mut memo,
        )?;
        verification = vstr.clone();
        if fact.roles & 0x1 != 0 {
            if !query.refs {
                definitions.push(hit_from_fact(&fact, &verification));
            }
        } else if query.refs {
            references.push(hit_from_fact(&fact, &verification));
        }
    }
    let (_, provider_name, provider_version) = import_verdict(
        &tx,
        &snapshot,
        &live,
        source_fresh,
        &canon,
        anchor.import_id,
        &mut memo,
    )?;
    Ok(SemanticQueryOut {
        definitions,
        references,
        provider_name,
        provider_version,
        verification,
        server_diagnostics: Vec::new(),
        fallback: None,
    })
}

/// M6 edge-query anchor: semantic identity when provable, syntax fallback
/// otherwise. The endpoint is the single key both directions query by.
#[derive(Debug)]
pub struct EdgeAnchor {
    pub kind: AnchorKind,
    pub path: String,
    pub line1: u32,
    pub col1: u32,
    pub symbol_uri: Option<String>,
    pub import_id: Option<i64>,
    pub doc: Option<String>,
    /// Display name backing the caller-candidate union (anchor's own name).
    pub display: Option<String>,
    /// Source spelling used only for heuristic caller-candidate selection.
    pub candidate_display: Option<String>,
    /// Syntax definition byte range for fallback anchors; `None` for semantic.
    pub span_bytes: Option<(usize, usize)>,
    pub precision: String,
    pub uncertainty: Option<String>,
    /// The anchor is a C++ data member. A member is not callable, so no call
    /// is matched to it by name; its references are name occurrences.
    pub data_member: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    Semantic,
    SyntaxFallback,
}

impl AnchorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AnchorKind::Semantic => "semantic",
            AnchorKind::SyntaxFallback => "syntax-fallback",
        }
    }
}

/// The queryable endpoint for an anchor. `None` is unreachable here:
/// anchors without identity fail in `resolve_edge_anchor`, never fabricate.
pub fn anchor_endpoint(anchor: &EdgeAnchor) -> Option<store::EdgeEndpoint> {
    match anchor.kind {
        AnchorKind::Semantic => Some(store::EdgeEndpoint::Scip {
            import_id: anchor.import_id?,
            symbol_uri: anchor.symbol_uri.clone()?,
            doc: anchor.doc.clone(),
        }),
        AnchorKind::SyntaxFallback => {
            let (bs, be) = anchor.span_bytes?;
            Some(store::EdgeEndpoint::SyntaxDef {
                path: anchor.path.clone(),
                byte_start: bs,
                byte_end: be,
            })
        }
    }
}

/// Byte offset of 0-based (line, byte-col) in source; `None` past the end.
/// Columns follow the tree-sitter byte convention, never clamped or guessed.
fn byte_offset_of(source: &[u8], line0: u32, col0: u32) -> Option<usize> {
    let mut start = 0usize;
    for _ in 0..line0 {
        let rel = source.get(start..)?.iter().position(|b| *b == b'\n')?;
        start += rel + 1;
    }
    let end = source
        .get(start..)
        .and_then(|tail| tail.iter().position(|b| *b == b'\n').map(|p| start + p))
        .unwrap_or(source.len());
    let off = start.checked_add(col0 as usize)?;
    if off > end {
        return None;
    }
    Some(off)
}

/// Recover old UTF8 byte positions from indexed syntax, never from changed
/// live bytes or producer-native SCIP columns. Sparse/unindexed stale lines
/// without a unique recorded base remain unresolved.
fn snapshot_byte_offset(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    file_id: i64,
    line0: u32,
    col0: u32,
) -> Result<Option<usize>, String> {
    if line0 == 0 {
        return Ok(Some(col0 as usize));
    }
    let mut bases = BTreeSet::new();
    // A data member is not a base for the byte-offset recovery either: the
    // E7 Slice-1 field symbols must not change how a stale position is
    // resolved. The symbol set is the one of the previous parser version.
    let mut stmt = tx.prepare("SELECT start_line, start_col, byte_start, end_line, end_col, byte_end FROM symbols WHERE snapshot_id = ?1 AND file_id = ?2 AND kind <> 'field' AND (start_line = ?3 OR end_line = ?3) UNION ALL SELECT start_line, start_col, byte_start, end_line, end_col, byte_end FROM calls WHERE snapshot_id = ?1 AND file_id = ?2 AND (start_line = ?3 OR end_line = ?3) UNION ALL SELECT start_line, start_col, byte_start, end_line, end_col, byte_end FROM imports WHERE snapshot_id = ?1 AND file_id = ?2 AND (start_line = ?3 OR end_line = ?3)").map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query(params![snapshot_id, file_id, line0 as i64 + 1])
        .map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        for base in [0, 3] {
            if row.get::<_, i64>(base).map_err(|e| e.to_string())? != line0 as i64 + 1 {
                continue;
            }
            let col = row.get::<_, i64>(base + 1).map_err(|e| e.to_string())?;
            let offset = row.get::<_, i64>(base + 2).map_err(|e| e.to_string())?;
            if col > 0 && offset >= col - 1 {
                bases.insert((offset - (col - 1)) as usize);
            }
        }
    }
    Ok(if bases.len() == 1 {
        bases
            .first()
            .and_then(|start| start.checked_add(col0 as usize))
    } else {
        None
    })
}

/// Resolve one edge-query anchor: semantic identity when a stored occurrence
/// proves it, else the nearest enclosing syntax definition with explicit
/// uncertainty. `line0`/`col0` are 0-based (M5 convention). Ambiguous or
/// contradicted anchors fail loudly — never an arbitrary pick.
pub fn resolve_edge_anchor(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    canon: &Path,
    validated: &str,
    line0: u32,
    col0: u32,
    symbol: Option<&str>,
) -> Result<EdgeAnchor, String> {
    resolve_edge_anchor_with_source(tx, snapshot_id, canon, validated, line0, col0, symbol, None)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_edge_anchor_with_source(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    canon: &Path,
    validated: &str,
    line0: u32,
    col0: u32,
    symbol: Option<&str>,
    cached_source: Option<&[u8]>,
) -> Result<EdgeAnchor, String> {
    let facts = store::semantic_facts_at(tx, snapshot_id, validated)?;
    let source = cached_source.map(std::borrow::Cow::Borrowed).or_else(|| {
        crate::semantic::confined_read(canon, validated, crate::scan::MAX_FILE_BYTES)
            .ok()
            .map(std::borrow::Cow::Owned)
    });
    let current_digest = source.as_ref().map(|s| store::content_digest(s));
    let indexed_file = store::file_identity(tx, snapshot_id, validated)?;
    let indexed = indexed_file.as_ref();
    let snapshot_offset = if let Some((id, _)) = indexed {
        snapshot_byte_offset(tx, snapshot_id, *id, line0, col0)?
    } else {
        None
    };
    let mut positions = BTreeMap::new();
    let mut best_width = usize::MAX;
    let mut best: Vec<&store::SemanticFact> = Vec::new();
    let mut provenance: BTreeMap<i64, Option<String>> = BTreeMap::new();
    for fact in &facts {
        let encoding = match provenance.get(&fact.import_id) {
            Some(encoding) => encoding.clone(),
            None => {
                let encoding = store::semantic_import_by_id(tx, fact.import_id)?
                    .filter(|(_, import)| {
                        import.snapshot_id == snapshot_id
                            && crate::edges::import_provenance_ok(import)
                    })
                    .map(|(_, import)| import.text_encoding);
                provenance.insert(fact.import_id, encoding.clone());
                encoding
            }
        };
        let Some(encoding) = encoding else {
            continue;
        };
        if fact.symbol_uri.is_none() {
            continue;
        }
        let offset = if current_digest.as_deref() == Some(fact.file_digest.as_str()) {
            *positions.entry(encoding.clone()).or_insert_with(|| {
                source.as_ref().and_then(|raw| {
                    crate::scip::query_byte_offset(raw, &encoding, line0, col0).ok()
                })
            })
        } else if encoding == "UTF8"
            && indexed.is_some_and(|(_, digest)| *digest == fact.file_digest)
            && fact.start_line == fact.end_line
            && fact.start_line == line0
        {
            snapshot_offset
        } else {
            None
        };
        let inside = offset.is_some_and(|off| fact.byte_start <= off && off < fact.byte_end);
        if !inside {
            continue;
        }
        let width = fact.byte_end.saturating_sub(fact.byte_start);
        if width < best_width {
            best_width = width;
            best.clear();
        }
        if width == best_width {
            best.push(fact);
        }
    }
    if let Some(want) = symbol {
        if best.is_empty() || !best.iter().any(|f| f.symbol_uri.as_deref() == Some(want)) {
            return Err(
                "anchor unresolved: --symbol disagrees with the position identity".to_string(),
            );
        }
        best.retain(|f| f.symbol_uri.as_deref() == Some(want));
    }
    if !best.is_empty() {
        let mut uris = BTreeSet::new();
        for fact in &best {
            match fact.symbol_uri.as_deref() {
                Some(uri) => {
                    uris.insert(uri.to_string());
                }
                None => {
                    return Err("occurrence has no symbol identity: unresolved".to_string());
                }
            }
        }
        if uris.len() > 1 {
            let mut list: Vec<&str> = uris.iter().map(String::as_str).collect();
            list.sort();
            return Err(format!("ambiguous anchor identities: {}", list.join(", ")));
        }
        let uri = uris.into_iter().next().unwrap_or_default();
        let mut imports = BTreeSet::new();
        for fact in &best {
            imports.insert(fact.import_id);
        }
        if imports.len() > 1 {
            return Err(format!(
                "ambiguous anchor: same identity from {} imports, resolve with a rebuild",
                imports.len()
            ));
        }
        let import_id = best[0].import_id;
        let local = uri == "local" || uri.starts_with("local ");
        return Ok(EdgeAnchor {
            kind: AnchorKind::Semantic,
            path: validated.to_string(),
            line1: line0 + 1,
            col1: col0 + 1,
            symbol_uri: Some(uri),
            import_id: Some(import_id),
            doc: if local {
                Some(validated.to_string())
            } else {
                None
            },
            display: best[0].display_name.clone(),
            candidate_display: best[0]
                .display_name
                .clone()
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    let fact = best[0];
                    if current_digest.as_deref() != Some(fact.file_digest.as_str()) {
                        return None;
                    }
                    let bytes = source.as_ref()?.get(fact.byte_start..fact.byte_end)?;
                    let name = std::str::from_utf8(bytes).ok()?;
                    (!name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '$'))
                    .then(|| name.to_owned())
                }),
            span_bytes: Some((best[0].byte_start, best[0].byte_end)),
            precision: best[0].precision.clone(),
            uncertainty: None,
            data_member: false,
        });
    }
    if symbol.is_some() {
        return Err("anchor unresolved: no semantic occurrence here".to_string());
    }
    // Syntax fallback: nearest enclosing definition by byte containment.
    let source = source.ok_or("anchor unresolved: source unavailable")?;
    // Unsupported files are refused before parsing, so they never have an index
    // row. The reason names that refusal instead of a changed source.
    let kind = classify(Path::new(validated));
    let refusal = match &kind {
        FileKind::Supported(_) => None,
        FileKind::CudaExperimental => Some("CUDA sources are never parsed or indexed"),
        FileKind::UnsupportedExtension => Some("files of this type are never parsed or indexed"),
    };
    if let Some(detail) = refusal {
        return Err(format!(
            "anchor unresolved: {}; {detail}",
            scan::unsupported_reason(&kind)
        ));
    }
    if !indexed.is_some_and(|(_, digest)| *digest == store::content_digest(&source)) {
        return Err(
            "anchor unresolved: source-changed; reindex before syntax fallback".to_string(),
        );
    }
    let off = byte_offset_of(&source, line0, col0)
        .ok_or_else(|| "anchor unresolved: position outside the source".to_string())?;
    let mut best_width = usize::MAX;
    let mut best: Vec<&store::SymbolRow> = Vec::new();
    let defs = store::symbols_at(tx, snapshot_id, validated)?;
    for def in &defs {
        // A data member's symbol spans its NAME only, so it is the narrowest
        // definition exactly when the position is on that name; anywhere else
        // in the class body the enclosing definition stays the anchor.
        if def.byte_start <= off && off < def.byte_end {
            let width = def.byte_end.saturating_sub(def.byte_start);
            if width < best_width {
                best_width = width;
                best.clear();
            }
            if width == best_width {
                best.push(def);
            }
        }
    }
    let mut keys = BTreeSet::new();
    for def in &best {
        keys.insert((
            def.byte_start,
            def.byte_end,
            def.kind.clone(),
            def.name.clone(),
        ));
    }
    if keys.len() > 1 {
        return Err("ambiguous anchor: overlapping definitions".to_string());
    }
    let def = best
        .into_iter()
        .next()
        .ok_or_else(|| "no anchor here".to_string())?;
    let data_member = def.kind == "field";
    let uncertainty = match (&def.name, data_member) {
        (Some(name), true) => format!(
            "syntax-only anchor: data member '{name}' has no call edges; its name occurrences are listed by search --occurrences"
        ),
        _ => "syntax-only anchor: no semantic identity at this position".to_string(),
    };
    Ok(EdgeAnchor {
        kind: AnchorKind::SyntaxFallback,
        path: validated.to_string(),
        line1: line0 + 1,
        col1: col0 + 1,
        symbol_uri: None,
        import_id: None,
        doc: None,
        display: def.name.clone(),
        candidate_display: def.name.clone(),
        span_bytes: Some((def.byte_start, def.byte_end)),
        precision: "Heuristic".to_string(),
        uncertainty: Some(uncertainty),
        data_member,
    })
}

/// Rust `crate::` paths are relative to the importer's crate source root, not
/// the repository root. Conventional Cargo layouts put that root at the last
/// `src` component, so a workspace crate resolves from `crates/<crate>/src`.
/// Only the enclosing conventional root is used: custom target paths and
/// `src/bin` are not resolved as separate crate roots. A path with no `src`
/// component yields no candidate rather than a repo-root guess.
fn rust_crate_src_root(importer: &str) -> Option<&str> {
    if let Some((head, _)) = importer.rsplit_once("/src/") {
        return Some(&importer[..head.len() + "/src".len()]);
    }
    importer.strip_prefix("src/").map(|_| "src")
}

/// Direct import candidates are deliberately conservative, never identity proof.
/// Unrecognized bindings remain missing coverage rather than a name-based link.
fn import_mentions_path(importer: &str, language: &str, text: &str, target: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    if language == "python" {
        let module = match words.as_slice() {
            ["from", module, "import", ..] => *module,
            ["import", module, ..] => *module,
            _ => return false,
        };
        let parent = importer.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let leading = module.chars().take_while(|c| *c == '.').count();
        let module = if leading == 0 {
            module.replace('.', "/")
        } else {
            let mut dirs: Vec<&str> = parent.split('/').filter(|s| !s.is_empty()).collect();
            for _ in 1..leading {
                if dirs.pop().is_none() {
                    return false;
                }
            }
            dirs.push(&module[leading..]);
            dirs.join("/").replace('.', "/")
        };
        return target == format!("{module}.py") || target == format!("{module}/__init__.py");
    }
    // Only literal relative imports/includes; require() is never an import row.
    if matches!(language, "javascript" | "typescript" | "c" | "cpp") {
        let literal = text.split(['\'', '"']).nth(1);
        if let Some(module) = literal.filter(|s| !s.is_empty()) {
            if !module.starts_with('.') && !matches!(language, "c" | "cpp") {
                return false;
            }
            let mut dirs: Vec<&str> = importer
                .rsplit_once('/')
                .map(|(p, _)| p.split('/').collect())
                .unwrap_or_default();
            for part in module.split('/') {
                match part {
                    "." | "" => {}
                    ".." => {
                        if dirs.pop().is_none() {
                            return false;
                        }
                    }
                    p => dirs.push(p),
                }
            }
            let path = dirs.join("/");
            return target == path
                || [".js", ".ts", ".tsx", ".jsx", "/index.js", "/index.ts"]
                    .iter()
                    .any(|ext| target == format!("{path}{ext}"));
        }
    }
    if language == "rust" {
        if let Some(module) = text.trim().strip_prefix("use crate::") {
            let module = module
                .split([';', '{', '*'])
                .next()
                .unwrap_or("")
                .trim_end_matches(':');
            let parts: Vec<&str> = module.split("::").collect();
            let Some(src_root) = rust_crate_src_root(importer) else {
                return false;
            };
            return (1..=parts.len()).any(|n| {
                let path = parts[..n].join("/");
                target == format!("{src_root}/{path}.rs")
                    || target == format!("{src_root}/{path}/mod.rs")
            });
        }
    }
    false
}

/// The quoted literal of a C/C++ `#include "..."` directive, with its
/// separators normalised to `/`. An angle-bracket include names a root or
/// system header, and a repository path suffix can never establish which
/// one the compiler opens.
fn cpp_include_literal(text: &str) -> Option<String> {
    let rest = text.split_once('"')?.1;
    let literal = rest.split_once('"')?.0;
    if literal.is_empty() {
        return None;
    }
    // Every repository path uses '/', so a Windows-authored include must
    // resolve the same way on every host.
    Some(literal.replace('\\', "/"))
}

/// Every component-boundary suffix of `path` that still carries a
/// directory component: the forms an `#include` literal may take for a
/// header that lives under an include root the repository does not
/// record. A bare filename is excluded — it is the one form a system
/// header can shadow.
fn suffix_anchors(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    if path.contains('/') {
        out.push(path);
    }
    for (at, _) in path.match_indices('/') {
        let suffix = &path[at + 1..];
        if suffix.contains('/') {
            out.push(suffix);
        }
    }
    out
}

/// The one indexed path the literal names, or `None` when it names none
/// or several. Ambiguity is never resolved arbitrarily.
fn unique_suffix_match<'a>(
    literal: &str,
    paths: impl Iterator<Item = &'a String>,
) -> Option<&'a str> {
    let needle = format!("/{literal}");
    let mut found: Option<&'a str> = None;
    for path in paths {
        if path == literal || path.ends_with(&needle) {
            if found.is_some() {
                return None;
            }
            found = Some(path.as_str());
        }
    }
    found
}

/// The name a call site records for a definition. A call keeps only its
/// terminal name (`run` in `w.run()`), while an out-of-line C++ definition
/// keeps its qualified text (`Widget::run`), so the part after the last `::`
/// is what the call sites are matched against.
fn called_name(definition: &str) -> &str {
    definition.rsplit("::").next().unwrap_or(definition)
}

/// Syntax-level inbound total of one indexed definition, read from the index
/// alone: the selection `callers` makes for the same definition, but the anchor
/// is the stored byte range, so the live source is never consulted. This is the
/// count as of the index (stale when the index is stale).
pub(crate) fn indexed_inbound_total(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    path: &str,
    name: &str,
    byte_start: usize,
    byte_end: usize,
) -> Result<usize, String> {
    let anchor = EdgeAnchor {
        kind: AnchorKind::SyntaxFallback,
        path: path.to_string(),
        line1: 1,
        col1: 1,
        symbol_uri: None,
        import_id: None,
        doc: None,
        display: Some(name.to_string()),
        candidate_display: Some(name.to_string()),
        span_bytes: Some((byte_start, byte_end)),
        precision: "Heuristic".to_string(),
        uncertainty: None,
        // shortcut: data_member stays false because `changes`, the only caller, answers a field before this lookup; set it from the definition kind if another caller ever passes one.
        data_member: false,
    };
    let endpoint = anchor_endpoint(&anchor).ok_or("anchor has no endpoint")?;
    let candidates = caller_candidate_paths(tx, snapshot_id, &anchor)?;
    let selection = store::EdgeSelection {
        snapshot_id,
        endpoint: &endpoint,
        inbound: true,
        candidate_name: anchor.candidate_display.as_deref().map(called_name),
        candidate_paths: &candidates.paths,
        candidate_aliases: &candidates.aliases,
        syntax_aliases: &[],
    };
    Ok(store::selected_edge_counts(tx, &selection)?.total)
}

struct CallerCandidates {
    paths: Vec<String>,
    aliases: Vec<(String, String)>,
}

/// The snapshot-wide inputs every caller seed reads. Loaded once per query.
pub(crate) struct CallerInputs {
    id_map: BTreeMap<String, (i64, String)>,
    imports: Vec<store::ImportRow>,
}

impl CallerInputs {
    pub(crate) fn load(tx: &rusqlite::Connection, snapshot_id: i64) -> Result<Self, String> {
        Ok(Self {
            id_map: store::file_id_map(tx, snapshot_id)?,
            imports: store::imports_for(tx, snapshot_id)?,
        })
    }
}

fn caller_candidate_paths(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    anchor: &EdgeAnchor,
) -> Result<CallerCandidates, String> {
    let inputs = CallerInputs::load(tx, snapshot_id)?;
    caller_candidate_paths_with(tx, snapshot_id, &inputs, anchor)
}

fn caller_candidate_paths_with(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    inputs: &CallerInputs,
    anchor: &EdgeAnchor,
) -> Result<CallerCandidates, String> {
    let id_map = &inputs.id_map;
    let paths: BTreeMap<i64, &str> = id_map
        .iter()
        .map(|(p, (id, _))| (*id, p.as_str()))
        .collect();
    let mut linked = BTreeSet::from([anchor.path.clone()]);
    let mut aliases = BTreeSet::new();
    if let Some(src_root) = rust_crate_src_root(&anchor.path) {
        // Fully qualified crate:: calls need no `use` row to connect their file.
        // shortcut: one src root can contain separate targets; upgrade when Cargo target metadata is available.
        linked.extend(
            id_map
                .keys()
                .filter(|path| path.ends_with(".rs") && rust_crate_src_root(path) == Some(src_root))
                .cloned(),
        );
    }
    // Local SCIP identities never gain cross-document name candidates.
    if anchor.doc.is_none() {
        let imports = &inputs.imports;
        if let Some(target_name) = anchor.candidate_display.as_deref().map(called_name) {
            for import in imports {
                if import.language == "python" {
                    if let (Some(path), Some(text)) =
                        (paths.get(&import.file_id), import.text.as_deref())
                    {
                        if import_mentions_path(path, "python", text, &anchor.path) {
                            if let Some(alias) = python_import_alias(text, target_name) {
                                aliases.insert(((*path).to_string(), alias.to_string()));
                            }
                        }
                    }
                }
            }
        }
        let mut targets = vec![anchor.path.clone()];
        targets.extend(declaring_headers(
            tx,
            snapshot_id,
            anchor,
            imports,
            id_map,
            &paths,
        )?);
        for target in &targets {
            linked.insert(target.clone());
            link_includers(&mut linked, imports, id_map, &paths, target);
        }
    }
    Ok(CallerCandidates {
        paths: linked.into_iter().collect(),
        aliases: aliases.into_iter().collect(),
    })
}

/// A narrow syntax candidate for `from module import symbol as alias`.
/// Module-to-file matching is checked by the caller before accepting it.
fn python_import_alias<'a>(text: &'a str, symbol: &str) -> Option<&'a str> {
    let words: Vec<_> = text.split_whitespace().collect();
    match words.as_slice() {
        ["from", _, "import", imported, "as", alias]
            if *imported == symbol
                && is_python_identifier(imported)
                && is_python_identifier(alias) =>
        {
            Some(alias)
        }
        _ => None,
    }
}

fn is_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Add every file whose imports name `target`.
fn link_includers(
    linked: &mut BTreeSet<String>,
    imports: &[store::ImportRow],
    id_map: &BTreeMap<String, (i64, String)>,
    paths: &BTreeMap<i64, &str>,
    target: &str,
) {
    for import in imports {
        if let (Some(path), Some(text)) = (paths.get(&import.file_id), import.text.as_deref()) {
            if import_mentions_path(path, &import.language, text, target) {
                linked.insert((*path).to_string());
            }
        }
    }
    // A C/C++ include is written against an include root the repository
    // does not record, so its literal rarely equals the target's
    // repository-relative path. A unique path suffix may widen this
    // candidate scope; it stays a candidate and never an identity.
    // shortcut: a unique suffix is not proof of the compiler's choice —
    // a literal the compiler resolves through another include root stays
    // a candidate only; upgrade by reading the build system's roots.
    let anchors = suffix_anchors(target);
    if anchors.is_empty() {
        return;
    }
    let mut resolved: BTreeMap<String, Option<&str>> = BTreeMap::new();
    for import in imports {
        let path = match paths.get(&import.file_id) {
            Some(path) => *path,
            None => continue,
        };
        if !matches!(import.language.as_str(), "c" | "cpp") {
            continue;
        }
        let literal = match import.text.as_deref().and_then(cpp_include_literal) {
            Some(literal) => literal,
            None => continue,
        };
        if !anchors.contains(&literal.as_str()) {
            continue;
        }
        let hit = match resolved.get(literal.as_str()) {
            Some(hit) => *hit,
            None => {
                let hit = unique_suffix_match(&literal, id_map.keys());
                resolved.insert(literal.clone(), hit);
                hit
            }
        };
        if hit == Some(target) {
            linked.insert(path.to_string());
        }
    }
}

/// The closest qualifier of an out-of-line C++ definition name:
/// `Widget` for `Widget::run`, `Problem` for `PatchMatch::Problem::Print`,
/// `Foo` for `Foo<T>::bar`. `None` for an unqualified name.
fn qualifier_of(definition: Option<&str>) -> Option<&str> {
    let mut parts = definition?.rsplit("::");
    parts.next()?;
    let owner = parts.next()?;
    let owner = owner.split('<').next().unwrap_or(owner).trim();
    (!owner.is_empty()).then_some(owner)
}

/// Headers that declare the class of an out-of-line C++ definition. A call to
/// `Widget::run` is written in files that include the header declaring
/// `Widget`, not in files that include `widget.cc`, so the headers that the
/// definition's file includes and that hold a class or struct of the
/// qualifier's name widen the candidate scope. A candidate, never an identity.
fn declaring_headers(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    anchor: &EdgeAnchor,
    imports: &[store::ImportRow],
    id_map: &BTreeMap<String, (i64, String)>,
    paths: &BTreeMap<i64, &str>,
) -> Result<Vec<String>, String> {
    let (Some(owner), Some((anchor_id, _))) = (
        qualifier_of(anchor.display.as_deref()),
        id_map.get(&anchor.path),
    ) else {
        return Ok(Vec::new());
    };
    let mut included = BTreeSet::new();
    for import in imports
        .iter()
        .filter(|i| i.file_id == *anchor_id && matches!(i.language.as_str(), "c" | "cpp"))
    {
        if let Some(literal) = import.text.as_deref().and_then(cpp_include_literal) {
            if let Some(hit) = unique_suffix_match(&literal, id_map.keys()) {
                included.insert(hit.to_string());
            }
        }
    }
    if included.is_empty() {
        return Ok(Vec::new());
    }
    let mut headers = BTreeSet::new();
    for symbol in store::symbols_for(tx, snapshot_id)? {
        if matches!(symbol.kind.as_str(), "class" | "struct")
            && symbol.name.as_deref() == Some(owner)
        {
            if let Some(path) = paths.get(&symbol.file_id) {
                if included.contains(*path) && *path != anchor.path {
                    headers.insert((*path).to_string());
                }
            }
        }
    }
    Ok(headers.into_iter().collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDir {
    Callers,
    Callees,
}
impl EdgeDir {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Callees => "callees",
        }
    }
}

pub struct EdgeRequest<'a> {
    pub anchor: SpanQuery<'a>,
    pub symbol: Option<&'a str>,
    pub direction: EdgeDir,
    pub limit: usize,
    pub offset: usize,
}

pub struct EdgeEvidence {
    pub digest: String,
    pub callee_span: Option<(usize, usize)>,
    pub terminal_span: Option<(usize, usize)>,
    pub reference_span: Option<(usize, usize)>,
    pub imports: Vec<(i64, store::SemanticImport)>,
}

/// How many definitions share a POTENTIAL_CALL edge's callee name, and where they
/// sit relative to the caller. A count, never a resolution: no target is chosen.
pub struct CandidateLabel {
    pub count: usize,
    pub scope: &'static str,
}

pub struct EdgeHitOut {
    pub edge: store::EdgeRow,
    pub verification: String,
    pub evidence: EdgeEvidence,
    /// Some only for POTENTIAL_CALL edges; derived at read time, never stored.
    pub candidate: Option<CandidateLabel>,
}

pub struct CompletenessOut {
    pub status: String,
    pub covered: usize,
    pub total: usize,
    /// These diagnostic counters overlap; they are not a coverage percentage.
    pub denominator: BTreeMap<&'static str, usize>,
    pub unresolved: Vec<String>,
}

pub struct EdgeQueryOut {
    pub repo: String,
    pub snapshot: store::Snapshot,
    pub revision: String,
    pub direction: EdgeDir,
    pub anchor: EdgeAnchor,
    pub freshness: String,
    pub freshness_reasons: Vec<String>,
    pub verification: String,
    pub completeness: CompletenessOut,
    pub edges: Vec<EdgeHitOut>,
    pub truncated: bool,
    pub limit: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

/// Shared query-local verification context; no cache survives this read txn.
struct EdgeRead<'a> {
    tx: &'a rusqlite::Connection,
    snapshot: &'a store::Snapshot,
    live: &'a crate::semantic::LiveState,
    canon: &'a Path,
    source_fresh: bool,
    memo: BTreeMap<i64, (String, String, String)>,
    /// Definition paths per callee name: one query per distinct name.
    definitions: BTreeMap<String, Vec<String>>,
    /// The snapshot-wide caller inputs, loaded once per query on first use.
    inputs: Option<CallerInputs>,
    /// Caller-linked paths per (definition path, name): one linkage per pair.
    linked: BTreeMap<(String, String), BTreeSet<String>>,
}

impl EdgeRead<'_> {
    fn verdict(&mut self, id: i64) -> Result<String, String> {
        Ok(import_verdict(
            self.tx,
            self.snapshot,
            self.live,
            self.source_fresh,
            self.canon,
            id,
            &mut self.memo,
        )?
        .0)
    }

    fn hit(&mut self, edge: store::EdgeRow) -> Result<EdgeHitOut, String> {
        let stored: serde_json::Value = serde_json::from_str(&edge.evidence)
            .map_err(|e| format!("invalid stored edge evidence: {e}"))?;
        let mut ids = BTreeSet::new();
        for endpoint in [&edge.caller, &edge.callee].into_iter().flatten() {
            if let store::EdgeEndpoint::Scip { import_id, .. } = endpoint {
                ids.insert(*import_id);
            }
        }
        if let Some(value) = stored.get("import") {
            ids.insert(
                value
                    .as_i64()
                    .filter(|id| *id > 0)
                    .ok_or("invalid edge producer import")?,
            );
        }
        // Legacy unresolved edges have no proven producer import. Do not infer
        // one from overlapping facts (especially when several imports overlap).
        let mut imports = Vec::new();
        let mut verification = if self.source_fresh {
            "Unverified"
        } else {
            "Stale"
        }
        .to_string();
        for id in ids {
            if self.verdict(id)? == "Stale" {
                verification = "Stale".to_string();
            }
            let (_, import) =
                store::semantic_import_by_id(self.tx, id)?.ok_or("edge import missing")?;
            if import.snapshot_id != self.snapshot.id
                || !crate::edges::import_provenance_ok(&import)
            {
                return Err("edge has invalid import provenance".to_string());
            }
            imports.push((id, import));
        }
        let digest: String = self
            .tx
            .query_row(
                "SELECT digest FROM files WHERE snapshot_id = ?1 AND id = ?2 AND path = ?3",
                params![self.snapshot.id, edge.file_id, edge.caller_path],
                |r| r.get(0),
            )
            .map_err(|e| format!("edge source identity: {e}"))?;
        if stored["path"].as_str() != Some(edge.caller_path.as_str())
            || stored["span"]
                != serde_json::json!([edge.caller_span_bytes.0, edge.caller_span_bytes.1])
        {
            return Err("edge evidence disagrees with stored source location".to_string());
        }
        let reference_span = match stored.get("ref") {
            Some(v) => Some((
                v.get(0)
                    .and_then(|x| x.as_u64())
                    .ok_or("invalid reference span")? as usize,
                v.get(1)
                    .and_then(|x| x.as_u64())
                    .ok_or("invalid reference span")? as usize,
            )),
            None => None,
        };
        type CallSpans = (Option<(usize, usize)>, Option<(usize, usize)>);
        let spans: Option<CallSpans> = self.tx.query_row("SELECT callee_byte_start, callee_byte_end, tcallee_byte_start, tcallee_byte_end FROM calls WHERE snapshot_id = ?1 AND file_id = ?2 AND byte_start = ?3 AND byte_end = ?4 ORDER BY id LIMIT 1", params![self.snapshot.id, edge.file_id, edge.caller_span_bytes.0 as i64, edge.caller_span_bytes.1 as i64], |r| {
            let (a, b, c, d): (Option<i64>, Option<i64>, Option<i64>, Option<i64>) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            Ok((a.zip(b).map(|(a,b)| (a as usize,b as usize)), c.zip(d).map(|(a,b)| (a as usize,b as usize))))
        }).optional().map_err(|e| e.to_string())?;
        let evidence_span = |key: &str| -> Option<(usize, usize)> {
            let pair = stored.get(key)?.as_array()?;
            Some((
                pair.first()?.as_u64()? as usize,
                pair.get(1)?.as_u64()? as usize,
            ))
        };
        // A factory argument has no call row at its own span: its evidence
        // carries the spans instead.
        let (callee_span, terminal_span) = match spans {
            Some(found) => found,
            None => (evidence_span("callee_span"), evidence_span("terminal_span")),
        };
        let candidate = if edge.edge_kind == "POTENTIAL_CALL" {
            Some(self.candidate(&edge)?)
        } else {
            None
        };
        Ok(EdgeHitOut {
            edge,
            verification,
            candidate,
            evidence: EdgeEvidence {
                digest,
                callee_span,
                terminal_span,
                reference_span,
                imports,
            },
        })
    }

    /// Counts the function and method definitions with the callee's terminal
    /// name and places them against the caller file. Name equality only.
    fn candidate(&mut self, edge: &store::EdgeRow) -> Result<CandidateLabel, String> {
        if !self.definitions.contains_key(&edge.callee_name) {
            let found = definition_paths(self.tx, self.snapshot.id, &edge.callee_name)?;
            self.definitions.insert(edge.callee_name.clone(), found);
        }
        let defs = self
            .definitions
            .get(&edge.callee_name)
            .cloned()
            .ok_or_else(|| "callee definitions not recorded".to_string())?;
        let caller = edge.caller_path.as_str();
        let scope = if defs.is_empty() {
            "none"
        } else if defs.iter().any(|d| d == caller) {
            "same-file"
        } else if self.linked_to_caller(&defs, &edge.callee_name, caller)? {
            "imported-file"
        } else {
            "other-file"
        };
        Ok(CandidateLabel {
            count: defs.len(),
            scope,
        })
    }

    /// The snapshot-wide caller inputs, loaded on first use in this query.
    fn inputs(&mut self) -> Result<&CallerInputs, String> {
        if self.inputs.is_none() {
            self.inputs = Some(CallerInputs::load(self.tx, self.snapshot.id)?);
        }
        self.inputs
            .as_ref()
            .ok_or_else(|| "caller inputs not loaded".to_string())
    }

    /// Whether a counted definition's file links to the caller file by the
    /// linkage `callers` uses: the caller paths of a minimal anchor for it.
    fn linked_to_caller(
        &mut self,
        defs: &[String],
        name: &str,
        caller: &str,
    ) -> Result<bool, String> {
        let (tx, snapshot_id) = (self.tx, self.snapshot.id);
        for def in defs {
            let key = (def.clone(), name.to_string());
            if !self.linked.contains_key(&key) {
                // shortcut: C++ declaring-header linkage is not applied (no qualifier), upgrade when qualified names reach this label
                let anchor = minimal_anchor(def, name, None);
                let inputs = self.inputs()?;
                let found = caller_candidate_paths_with(tx, snapshot_id, inputs, &anchor)?;
                self.linked
                    .insert(key.clone(), found.paths.into_iter().collect());
            }
            let Some(paths) = self.linked.get(&key) else {
                return Err("caller linkage not recorded".to_string());
            };
            if paths.contains(caller) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The minimal anchor of a syntax definition on the name-seed path: its file and
/// callable name, with no position, owner or document.
fn minimal_anchor(path: &str, name: &str, span: Option<(usize, usize)>) -> EdgeAnchor {
    EdgeAnchor {
        kind: AnchorKind::SyntaxFallback,
        path: path.to_string(),
        line1: 1,
        col1: 1,
        symbol_uri: None,
        import_id: None,
        doc: None,
        display: None,
        candidate_display: Some(name.to_string()),
        span_bytes: span,
        precision: "Heuristic".to_string(),
        uncertainty: None,
        data_member: false,
    }
}

/// Paths of the function and method definitions whose terminal name is `name`,
/// one entry per definition. Matched in SQL with bound parameters only.
fn definition_paths(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    name: &str,
) -> Result<Vec<String>, String> {
    let mut statement = tx
        .prepare(
            "SELECT f.path FROM symbols s JOIN files f ON f.id = s.file_id AND f.snapshot_id = s.snapshot_id WHERE s.snapshot_id = ?1 AND s.kind IN ('function', 'method') AND (s.name = ?2 OR substr(s.name, -(length(?2) + 2)) = '::' || ?2)",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![snapshot_id, name], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

struct QueryCoverage {
    denominator: BTreeMap<&'static str, usize>,
    reasons: BTreeSet<String>,
    verification: String,
}

impl EdgeRead<'_> {
    /// Shared Task4/Task5 coverage and import memo, still inside one read txn.
    fn coverage(&mut self) -> Result<QueryCoverage, String> {
        let mut stale = 0;
        let mut provider_paths = BTreeSet::new();
        for (id, import) in store::semantic_imports_for(self.tx, self.snapshot.id)? {
            if !crate::edges::import_provenance_ok(&import) {
                continue;
            }
            if self.verdict(id)? == "Stale" {
                stale += 1;
            }
            let mut stmt = self.tx.prepare("SELECT DISTINCT rel_path FROM semantic_facts WHERE snapshot_id = ?1 AND import_id = ?2 AND symbol_uri IS NOT NULL").map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![self.snapshot.id, id], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            for row in rows {
                provider_paths.insert(row.map_err(|e| e.to_string())?);
            }
        }
        let files = store::file_id_map(self.tx, self.snapshot.id)?;
        let missing = files
            .keys()
            .filter(|p| !provider_paths.contains(*p))
            .count();
        let (walk, skips) = scan::plan_walk(self.canon)?;
        let unsupported = walk
            .iter()
            .filter(|f| matches!(f.action, PlanAction::Unsupported(_)))
            .count();
        let skipped = skips.escape
            + skips.unreadable
            + walk
                .iter()
                .filter(|f| {
                    matches!(f.action,PlanAction::Skip(reason) if reason!="ignored")
                        || (matches!(f.action, PlanAction::Parse(_)) && !files.contains_key(&f.rel))
                })
                .count();
        let parse_errors = self
            .tx
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND errors > 0",
                params![self.snapshot.id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| e.to_string())? as usize;
        let unknown = store::snapshot_edge_counts(self.tx, self.snapshot.id)?;
        let denominator = BTreeMap::from([
            ("observed_edges", 0),
            ("unsupported_files", unsupported),
            ("missing_provider_files", missing),
            ("dynamic_calls", unknown.dynamic_calls),
            ("ambiguous_calls", unknown.ambiguous_calls),
            ("truncated_branches", 0),
            ("unresolved_owner", unknown.unresolved_owner),
            ("unresolved_target", unknown.unresolved_target),
            ("stale_imports", stale),
            ("skipped_files", skipped),
            ("parse_error_files", parse_errors),
        ]);
        let mut reasons = BTreeSet::new();
        if unknown.unresolved > 0 {
            reasons.insert("unresolved-relations".to_string());
        }
        if !self.source_fresh {
            reasons.insert("source-not-fresh".to_string());
        }
        Ok(QueryCoverage {
            denominator,
            reasons,
            verification: if !self.source_fresh || stale > 0 {
                "Stale"
            } else {
                "Unverified"
            }
            .to_string(),
        })
    }
}

pub fn edge_query(
    home: &Path,
    name: &str,
    root: &Path,
    request: &EdgeRequest<'_>,
) -> Result<EdgeQueryOut, String> {
    edge_query_inner(home, name, root, request, || {})
}

// A query-local callback allows deterministic promotion tests at the actual
// query boundary. Production supplies a no-op; no global hook or sleep.
fn edge_query_inner(
    home: &Path,
    name: &str,
    root: &Path,
    request: &EdgeRequest<'_>,
    pinned: impl FnOnce(),
) -> Result<EdgeQueryOut, String> {
    let (canon, validated) = validated_edge_anchor(root, request)?;
    let mut conn = store::open_live(&store::db_path(home, name))?;
    edge_query_conn(&mut conn, name, &canon, &validated, request, pinned)
}

/// Bounds and containment checks shared by the CLI path (before its migrating
/// open) and the MCP read-only path: identical refusals in both worlds.
pub(crate) fn validated_edge_anchor(
    root: &Path,
    request: &EdgeRequest<'_>,
) -> Result<(PathBuf, String), String> {
    if request.anchor.line1 == 0
        || request.anchor.col1 == 0
        || !(1..=MAX_SEARCH_LIMIT).contains(&request.limit)
        || request.offset > i64::MAX as usize - MAX_SEARCH_LIMIT
    {
        return Err("invalid edge query bounds".to_string());
    }
    let canon = canonical_repo_root(root)?;
    let validated = crate::semantic::validate_doc_path(&canon, request.anchor.rel)?;
    Ok((canon, validated))
}

/// The edge query body on a caller-provided connection: the CLI passes its
/// migrating `open_live` connection, MCP passes the guard-validated read-only
/// connection. One read transaction pins snapshot, verdict, and rows.
pub(crate) fn edge_query_conn(
    conn: &mut rusqlite::Connection,
    name: &str,
    canon: &Path,
    validated: &str,
    request: &EdgeRequest<'_>,
    pinned: impl FnOnce(),
) -> Result<EdgeQueryOut, String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let snapshot = store::latest_snapshot(&tx)?.ok_or("no snapshot: index the repository first")?;
    pinned();
    store::require_graph_proof(&tx, snapshot.id)?;
    let (freshness, mut freshness_reasons) = evaluate_snapshot(&tx, name, canon, &snapshot);
    freshness_reasons.sort();
    freshness_reasons.dedup();
    let live = live_state_for(&tx, &snapshot)?;
    let revision = crate::semantic::revision_string(&live);
    let anchor = resolve_edge_anchor(
        &tx,
        snapshot.id,
        canon,
        validated,
        request.anchor.line1 - 1,
        request.anchor.col1 - 1,
        request.symbol,
    )?;
    let endpoint = anchor_endpoint(&anchor).ok_or("anchor has no endpoint")?;
    let aliases = if request.direction == EdgeDir::Callees {
        crate::edges::syntax_aliases(&tx, snapshot.id, &endpoint)?
    } else {
        Vec::new()
    };
    // Callers are matched by name only for a callable anchor: a data member
    // takes no call by name, so a same-named method call is never its caller.
    let by_name = request.direction == EdgeDir::Callers && !anchor.data_member;
    let candidates = if by_name {
        caller_candidate_paths(&tx, snapshot.id, &anchor)?
    } else {
        CallerCandidates {
            paths: Vec::new(),
            aliases: Vec::new(),
        }
    };
    let selection = store::EdgeSelection {
        snapshot_id: snapshot.id,
        endpoint: &endpoint,
        inbound: request.direction == EdgeDir::Callers,
        candidate_name: if by_name {
            anchor.candidate_display.as_deref().map(called_name)
        } else {
            None
        },
        candidate_paths: &candidates.paths,
        candidate_aliases: &candidates.aliases,
        syntax_aliases: &aliases,
    };
    let counts = store::selected_edge_counts(&tx, &selection)?;
    let page = if selection.candidate_name.is_some() || !aliases.is_empty() {
        store::selected_edge_page(&tx, &selection, request.limit as i64, request.offset as i64)?
    } else if selection.inbound {
        store::edges_inbound(
            &tx,
            snapshot.id,
            &endpoint,
            request.limit as i64,
            request.offset as i64,
        )?
    } else {
        store::edges_outbound(
            &tx,
            snapshot.id,
            &endpoint,
            request.limit as i64,
            request.offset as i64,
        )?
    };
    let next = request.offset + page.len();
    let truncated = next < counts.total;
    let mut read = EdgeRead {
        tx: &tx,
        snapshot: &snapshot,
        live: &live,
        canon,
        source_fresh: freshness == "fresh",
        memo: BTreeMap::new(),
        definitions: BTreeMap::new(),
        inputs: None,
        linked: BTreeMap::new(),
    };
    let mut verification = if read.source_fresh {
        "Unverified"
    } else {
        "Stale"
    }
    .to_string();
    if let Some(id) = anchor.import_id {
        if read.verdict(id)? == "Stale" {
            verification = "Stale".to_string();
        }
    }
    let coverage = read.coverage()?;
    if coverage.verification == "Stale" {
        verification = "Stale".to_string();
    }
    let mut edges = Vec::with_capacity(page.len());
    let mut reasons = BTreeSet::new();
    for edge in page {
        let hit = read.hit(edge)?;
        if hit.verification == "Stale" {
            verification = "Stale".to_string();
        }
        if let Some(reason) = &hit.edge.unresolved {
            reasons.insert(reason.clone());
        }
        edges.push(hit);
    }
    let mut denominator = coverage.denominator;
    reasons.extend(coverage.reasons);
    denominator.insert("observed_edges", counts.total);
    denominator.insert(
        "truncated_branches",
        usize::from(truncated || request.offset > 0),
    );
    for (key, count) in &denominator {
        if *key != "observed_edges" && *count > 0 {
            reasons.insert((*key).to_string());
        }
    }
    if anchor.uncertainty.is_some() {
        reasons.insert("syntax-anchor-uncertain".to_string());
    }
    if counts.total == 0 {
        reasons.insert("empty-result-not-coverage-proof".to_string());
    }
    if freshness != "fresh" {
        reasons.insert("source-not-fresh".to_string());
    }
    let complete = reasons.is_empty() && !truncated && edges.len() == counts.total;
    Ok(EdgeQueryOut {
        repo: name.to_string(),
        snapshot,
        revision,
        direction: request.direction,
        anchor,
        freshness,
        freshness_reasons,
        verification,
        completeness: CompletenessOut {
            status: if complete { "complete" } else { "partial" }.to_string(),
            covered: edges.len(),
            total: counts.total,
            denominator,
            unresolved: reasons.into_iter().collect(),
        },
        edges,
        truncated,
        limit: request.limit,
        offset: request.offset,
        next_offset: truncated.then_some(next),
    })
}

pub struct TraceRequest<'a> {
    pub source: SpanQuery<'a>,
    pub symbol: Option<&'a str>,
    pub destination: Option<SpanQuery<'a>>,
    pub depth: usize,
    pub limit: usize,
    pub offset: usize,
    /// Impact only: add the `summary` object (over all traversal paths).
    pub summary: bool,
    /// With `summary`: count test-file paths too.
    pub include_tests: bool,
}

/// Labelled path heuristic, not a language analysis: a directory named `test`,
/// `tests`, `__tests__` or `spec`, or a file name matching a listed test pattern.
/// Other conventions (`.tsx`, `_spec.rb`) are not covered.
pub(crate) fn is_test_path(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or("");
    let in_test_dir = parts
        .iter()
        .any(|d| matches!(*d, "test" | "tests" | "__tests__" | "spec"));
    let test_file = (name.starts_with("test_") && name.ends_with(".py"))
        || name.ends_with("_test.go")
        || name.ends_with(".test.ts")
        || name.ends_with(".spec.ts")
        || name.ends_with("Test.java");
    in_test_dir || test_file
}

/// One traversal path as the summary counts it.
struct PathRow {
    distance: usize,
    basis: &'static str,
    file: String,
    test: bool,
}

/// A path's dependent file: the caller file of its LAST edge.
fn dependent_file(edges: &[store::EdgeRow]) -> String {
    edges
        .last()
        .map_or_else(String::new, |e| e.caller_path.clone())
}

/// One traversal path as the summary counts it: distance is its edge count.
fn path_row(path: &crate::edges::TracePath) -> PathRow {
    let file = dependent_file(&path.edges);
    PathRow {
        distance: path.edges.len(),
        basis: path.basis,
        test: is_test_path(&file),
        file,
    }
}

/// Impact's summary (`--summary`): counts over all traversal paths, before paging.
pub struct SummaryOut {
    pub include_tests: bool,
    pub total: usize,
    /// (distance, paths, proven, unproven, candidate-name), ascending by distance.
    pub by_depth: Vec<(usize, usize, usize, usize, usize)>,
    pub distinct_files: usize,
    pub tests: usize,
    /// The per-node cap or another budget may have lowered the counts.
    pub candidates_complete: bool,
    pub verdict: &'static str,
    pub unresolved: Vec<String>,
}

fn summarize(
    rows: &[PathRow],
    include_tests: bool,
    candidates_complete: bool,
    verdict: &'static str,
    unresolved: &[String],
) -> SummaryOut {
    let counted: Vec<&PathRow> = rows.iter().filter(|r| include_tests || !r.test).collect();
    // (paths, proven, unproven, candidate-name) per distance.
    let mut depths: BTreeMap<usize, (usize, usize, usize, usize)> = BTreeMap::new();
    for r in &counted {
        let e = depths.entry(r.distance).or_default();
        e.0 += 1;
        match r.basis {
            "proven" => e.1 += 1,
            "unproven" => e.2 += 1,
            _ => e.3 += 1,
        }
    }
    SummaryOut {
        include_tests,
        total: counted.len(),
        by_depth: depths
            .into_iter()
            .map(|(d, (n, p, u, c))| (d, n, p, u, c))
            .collect(),
        distinct_files: counted
            .iter()
            .map(|r| r.file.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        tests: rows.iter().filter(|r| r.test).count(),
        candidates_complete,
        verdict,
        unresolved: unresolved.to_vec(),
    }
}

pub struct TracePathOut {
    pub edges: Vec<EdgeHitOut>,
    pub cycle: bool,
    pub truncated: bool,
    pub unresolved: Vec<String>,
    pub verification: String,
    pub basis: &'static str,
}

/// Impact's candidate summary. `total` is `Some` only when every candidate was counted.
pub struct CandidatesOut {
    pub nodes: usize,
    pub returned: usize,
    pub complete: bool,
    pub total: Option<usize>,
    pub per_node_limit: usize,
}

pub struct TraceQueryOut {
    pub impact_verdict: Option<&'static str>,
    pub candidates: Option<CandidatesOut>,
    pub summary: Option<SummaryOut>,
    pub repo: String,
    pub snapshot: store::Snapshot,
    pub revision: String,
    pub anchor: EdgeAnchor,
    pub destination: Option<EdgeAnchor>,
    pub freshness: String,
    pub freshness_reasons: Vec<String>,
    pub verification: String,
    pub completeness: CompletenessOut,
    pub total_exact: bool,
    pub paths: Vec<TracePathOut>,
    pub bridges: Vec<crate::edges::OwnerBridge>,
    pub nodes: usize,
    pub edge_visits: usize,
    pub cycle: bool,
    pub truncated: bool,
    pub unresolved: Vec<String>,
    pub depth: usize,
    pub limit: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

pub fn trace_query(
    home: &Path,
    name: &str,
    root: &Path,
    request: &TraceRequest<'_>,
) -> Result<TraceQueryOut, String> {
    trace_query_inner(home, name, root, request, || {})
}

fn trace_query_inner(
    home: &Path,
    name: &str,
    root: &Path,
    request: &TraceRequest<'_>,
    pinned: impl FnOnce(),
) -> Result<TraceQueryOut, String> {
    graph_query_inner(home, name, root, request, pinned, false, false)
}

pub fn impact_query(
    home: &Path,
    name: &str,
    root: &Path,
    request: &TraceRequest<'_>,
) -> Result<TraceQueryOut, String> {
    if request.destination.is_some() {
        return Err("impact does not accept a destination".to_string());
    }
    graph_query_inner(home, name, root, request, || {}, true, true)
}

fn graph_query_inner(
    home: &Path,
    name: &str,
    root: &Path,
    request: &TraceRequest<'_>,
    pinned: impl FnOnce(),
    impact: bool,
    candidates: bool,
) -> Result<TraceQueryOut, String> {
    let started = Instant::now();
    let (canon, rel, dest_path) = validated_trace_anchor(root, request)?;
    let mut conn = store::open_live(&store::db_path(home, name))?;
    graph_query_conn(
        &mut conn,
        name,
        &canon,
        &rel,
        dest_path.as_deref(),
        request,
        started,
        pinned,
        impact,
        candidates,
    )
}

/// Bounds and containment checks shared by the CLI path (before its migrating
/// open) and the MCP read-only path: identical refusals in both worlds.
pub(crate) fn validated_trace_anchor(
    root: &Path,
    request: &TraceRequest<'_>,
) -> Result<(PathBuf, String, Option<String>), String> {
    if !(1..=crate::edges::TRACE_MAX_DEPTH).contains(&request.depth)
        || !(1..=MAX_SEARCH_LIMIT).contains(&request.limit)
        || request.offset > i64::MAX as usize - MAX_SEARCH_LIMIT
        || request.source.line1 == 0
        || request.source.col1 == 0
        || request
            .destination
            .as_ref()
            .is_some_and(|d| d.line1 == 0 || d.col1 == 0)
    {
        return Err("invalid trace bounds".to_string());
    }
    let canon = canonical_repo_root(root)?;
    let rel = crate::semantic::validate_doc_path(&canon, request.source.rel)?;
    let dest_path = request
        .destination
        .as_ref()
        .map(|d| crate::semantic::validate_doc_path(&canon, d.rel))
        .transpose()?;
    Ok((canon, rel, dest_path))
}

/// The trace/impact body on a caller-provided connection (see
/// `edge_query_conn`); `started` keeps the time budget anchored to entry,
/// except that time spent inside the `pinned` hook is not charged to it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn graph_query_conn(
    conn: &mut rusqlite::Connection,
    name: &str,
    canon: &Path,
    rel: &str,
    dest_path: Option<&str>,
    request: &TraceRequest<'_>,
    started: Instant,
    pinned: impl FnOnce(),
    impact: bool,
    candidates: bool,
) -> Result<TraceQueryOut, String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let snapshot = store::latest_snapshot(&tx)?.ok_or("no snapshot: index the repository first")?;
    // `pinned` is a test seam that runs with the snapshot already pinned. Its
    // duration is not query work, so it must not consume the traversal budget.
    let hook_started = Instant::now();
    pinned();
    let started = started + hook_started.elapsed();
    store::require_graph_proof(&tx, snapshot.id)?;
    let anchor = resolve_edge_anchor(
        &tx,
        snapshot.id,
        canon,
        rel,
        request.source.line1 - 1,
        request.source.col1 - 1,
        request.symbol,
    )?;
    let destination = request
        .destination
        .as_ref()
        .zip(dest_path)
        .map(|(d, p)| {
            resolve_edge_anchor(&tx, snapshot.id, canon, p, d.line1 - 1, d.col1 - 1, None)
        })
        .transpose()?;
    let start = anchor_endpoint(&anchor).ok_or("source has no identity")?;
    let target = destination.as_ref().and_then(anchor_endpoint);
    let (freshness, mut freshness_reasons) = evaluate_snapshot(&tx, name, canon, &snapshot);
    freshness_reasons.sort();
    freshness_reasons.dedup();
    let live = live_state_for(&tx, &snapshot)?;
    let revision = crate::semantic::revision_string(&live);
    let mut read = EdgeRead {
        tx: &tx,
        snapshot: &snapshot,
        live: &live,
        canon,
        source_fresh: freshness == "fresh",
        memo: BTreeMap::new(),
        definitions: BTreeMap::new(),
        inputs: None,
        linked: BTreeMap::new(),
    };
    let coverage = read.coverage()?;
    let mut verification = coverage.verification;
    for a in [&anchor, destination.as_ref().unwrap_or(&anchor)] {
        if let Some(id) = a.import_id {
            if read.verdict(id)? == "Stale" {
                verification = "Stale".to_string();
            }
        }
    }
    let mut budget = crate::edges::TraceBudget::new(request.depth, started);
    let traversal = if impact {
        if candidates {
            budget.candidates = request.limit;
            // Loaded once per query: every node's seed reads the same inputs.
            let inputs = CallerInputs::load(&tx, snapshot.id)?;
            let seed_of = |endpoint: &store::EdgeEndpoint| {
                candidate_seed(&tx, snapshot.id, &inputs, &anchor, &start, endpoint)
            };
            crate::edges::traverse_impact(&tx, snapshot.id, &start, &budget, Some(&seed_of))?
        } else {
            crate::edges::traverse_impact(&tx, snapshot.id, &start, &budget, None)?
        }
    } else {
        crate::edges::traverse(&tx, snapshot.id, &start, target.as_ref(), &budget)?
    };
    let total = traversal.paths.len();
    let candidate_summary = candidates.then(|| CandidatesOut {
        nodes: traversal.candidate_nodes,
        returned: traversal.candidates_returned,
        complete: traversal.candidates_complete,
        total: traversal
            .candidates_complete
            .then_some(traversal.candidates_returned),
        per_node_limit: budget.candidates,
    });
    let impact_verdict = impact.then(|| {
        crate::edges::impact_verdict(
            &traversal,
            verification == "Stale"
                || freshness != "fresh"
                || anchor.uncertainty.is_some()
                || !coverage.reasons.is_empty()
                || coverage
                    .denominator
                    .iter()
                    .any(|(k, v)| *k != "observed_edges" && *v > 0)
                || request.offset > 0
                || total > request.limit,
        )
    });
    // Summary rows come from all traversal paths, before the page is cut.
    let summary_rows: Option<Vec<PathRow>> =
        (impact && request.summary).then(|| traversal.paths.iter().map(path_row).collect());
    let mut paths = Vec::new();
    for path in traversal
        .paths
        .into_iter()
        .skip(request.offset)
        .take(request.limit)
    {
        let mut hits = Vec::new();
        let mut path_verification = if read.source_fresh {
            "Unverified"
        } else {
            "Stale"
        }
        .to_string();
        for edge in path.edges {
            let hit = read.hit(edge)?;
            if hit.verification == "Stale" {
                path_verification = "Stale".to_string();
                verification = "Stale".to_string();
            }
            hits.push(hit);
        }
        paths.push(TracePathOut {
            edges: hits,
            cycle: path.cycle,
            truncated: path.truncated,
            unresolved: path.unresolved,
            verification: path_verification,
            basis: path.basis,
        });
    }
    let next = request.offset + paths.len();
    let more = next < total;
    let truncated = traversal.truncated || more;
    let mut reasons = coverage.reasons;
    if impact {
        reasons.insert("graph-completeness-unproven".to_string());
        if impact_verdict == Some("UNKNOWN") {
            reasons.insert("insufficient-impact-evidence".to_string());
        }
    }
    reasons.extend(traversal.unresolved.iter().cloned());
    if anchor.uncertainty.is_some() {
        reasons.insert("syntax-anchor-uncertain".to_string());
    }
    if destination
        .as_ref()
        .is_some_and(|a| a.uncertainty.is_some())
    {
        reasons.insert("destination-anchor-uncertain".to_string());
    }
    if total == 0 {
        reasons.insert("empty-result-not-coverage-proof".to_string());
    }
    let mut denominator = coverage.denominator;
    denominator.insert("observed_edges", traversal.edges);
    denominator.insert(
        "truncated_branches",
        usize::from(truncated || request.offset > 0),
    );
    for (key, count) in &denominator {
        if *key != "observed_edges" && *count > 0 {
            reasons.insert((*key).to_string());
        }
    }
    let complete = reasons.is_empty() && !truncated && paths.len() == total;
    let candidates_complete = traversal.candidates_complete;
    let summary = summary_rows.zip(impact_verdict).map(|(rows, verdict)| {
        let unresolved: Vec<String> = reasons.iter().cloned().collect();
        summarize(
            &rows,
            request.include_tests,
            candidates_complete,
            verdict,
            &unresolved,
        )
    });
    Ok(TraceQueryOut {
        impact_verdict,
        candidates: candidate_summary,
        summary,
        repo: name.to_string(),
        snapshot,
        revision,
        anchor,
        destination,
        freshness,
        freshness_reasons,
        verification,
        completeness: CompletenessOut {
            status: if complete { "complete" } else { "partial" }.to_string(),
            covered: paths.len(),
            total,
            denominator,
            unresolved: reasons.into_iter().collect(),
        },
        total_exact: !traversal.truncated,
        paths,
        bridges: traversal.bridges,
        nodes: traversal.nodes,
        edge_visits: traversal.edges,
        cycle: traversal.cycle,
        truncated,
        unresolved: traversal.unresolved.into_iter().collect(),
        depth: request.depth,
        limit: request.limit,
        offset: request.offset,
        next_offset: more.then_some(next),
    })
}

/// The name seed of a syntax definition on the impact walk. The anchor gets the
/// seed `callers` builds for it; another definition gets the same selection from
/// its own name and file. A data-member anchor takes no call by name (`NoName`);
/// other data fields reached on the walk are not callable, so they get `Missing`;
/// no derivable name is `Missing`, a gap the answer states.
fn candidate_seed(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    inputs: &CallerInputs,
    anchor: &EdgeAnchor,
    start: &store::EdgeEndpoint,
    endpoint: &store::EdgeEndpoint,
) -> Result<crate::edges::NameSeed, String> {
    use crate::edges::NameSeed;
    if crate::edges::unify(endpoint, start) {
        if anchor.data_member {
            return Ok(NameSeed::NoName);
        }
        let Some(display) = anchor.candidate_display.as_deref() else {
            return Ok(NameSeed::Missing);
        };
        let found = caller_candidate_paths_with(tx, snapshot_id, inputs, anchor)?;
        return Ok(NameSeed::Named(crate::edges::CandidateSeed {
            name: called_name(display).to_string(),
            paths: found.paths,
            aliases: found.aliases,
        }));
    }
    let store::EdgeEndpoint::SyntaxDef {
        path,
        byte_start,
        byte_end,
    } = endpoint
    else {
        return Ok(NameSeed::Missing);
    };
    let symbol = store::symbols_at(tx, snapshot_id, path)?
        .into_iter()
        .find(|s| {
            s.byte_start == *byte_start
                && s.byte_end == *byte_end
                && (s.kind == "function" || s.kind == "method")
        });
    let Some(name) = symbol.and_then(|s| s.name) else {
        return Ok(NameSeed::Missing);
    };
    // shortcut: C++ owner/declaring-header linkage is not applied to non-anchor nodes, upgrade when proven-reached nodes need it
    let minimal = minimal_anchor(path, &name, Some((*byte_start, *byte_end)));
    let found = caller_candidate_paths_with(tx, snapshot_id, inputs, &minimal)?;
    Ok(NameSeed::Named(crate::edges::CandidateSeed {
        name: called_name(&name).to_string(),
        paths: found.paths,
        aliases: found.aliases,
    }))
}

/// Single-hop live LSP query with an explicit server path. Ephemeral
/// evidence only: nothing is persisted, every answer is `Unverified` unless
/// the adapter could PROVE that the source it was answered about is no
/// longer the source on disk (`Stale`). Any spawn/query failure becomes an
/// explicit fallback payload (exit 0), never a fabricated relationship.
pub fn semantic_query_lsp(
    root: &Path,
    query: &SpanQuery<'_>,
    server_exe: &str,
) -> Result<SemanticQueryOut, String> {
    let canon = canonical_repo_root(root)?;
    let validated = crate::semantic::validate_doc_path(&canon, query.rel)?;
    let spec = crate::lsp::ServerSpec::new(server_exe, &canon)?;
    let mut session = match crate::lsp::spawn(spec) {
        Ok(session) => session,
        Err(e) => {
            return Ok(SemanticQueryOut {
                definitions: Vec::new(),
                references: Vec::new(),
                provider_name: "lsp-subprocess".to_string(),
                provider_version: String::new(),
                verification: "Unverified".to_string(),
                server_diagnostics: Vec::new(),
                fallback: Some(e),
            });
        }
    };
    let answer = if query.refs {
        session.references(&validated, query.line1, query.col1)
    } else {
        session.definition(&validated, query.line1, query.col1)
    };
    let answer = match answer {
        Ok(answer) => answer,
        Err(e) => {
            // The server failed, but it may have said why: its diagnostics
            // travel with the fallback reason instead of being dropped.
            let diagnostics = session.shutdown();
            let reason = lsp_fallback_reason(&e, &diagnostics);
            return Ok(SemanticQueryOut {
                definitions: Vec::new(),
                references: Vec::new(),
                provider_name: "lsp-subprocess".to_string(),
                provider_version: String::new(),
                verification: "Unverified".to_string(),
                server_diagnostics: Vec::new(),
                fallback: Some(reason),
            });
        }
    };
    let version = answer.server_version.clone();
    let mut out = SemanticQueryOut {
        definitions: Vec::new(),
        references: Vec::new(),
        provider_name: "lsp-subprocess".to_string(),
        provider_version: version,
        verification: "Unverified".to_string(),
        server_diagnostics: answer.diagnostics.clone(),
        fallback: None,
    };
    for fact in &answer.facts {
        let hit = lsp_hit_from_fact(fact);
        if query.refs {
            out.references.push(hit);
        } else {
            out.definitions.push(hit);
        }
    }
    // The answer's own verdict: the weakest fact in it. A fact the adapter
    // marked stale makes the whole answer stale, never the other way round.
    out.verification = lsp_answer_verification(&out.definitions, &out.references);
    session.shutdown();
    Ok(out)
}

/// Dispatcher: cold start and `--full` go straight to full rebuilds;
/// otherwise reuse is attempted only when the previous snapshot is provably
/// compatible (root, parser, grammars, config, HEAD, branch). Dirty
/// worktrees and untracked-count changes with the same HEAD/branch stay
/// incremental — per-file digests decide. A stale previous snapshot still
/// allows reuse: every reused file is independently digest-verified.
///
/// A `semantic-changed` promotion abort (a semantic import committed while
/// staging was in flight) retries the whole build exactly once from a fresh
/// base, which then includes the import. A second advance errors out rather
/// than looping.
pub fn build(home: &Path, name: &str, root: &Path, full: bool) -> Result<IndexSummary, String> {
    match build_once(home, name, root, full) {
        Err(e) if e.contains("semantic-changed") => {
            fallback_full(home, name, root, "semantic-changed")
        }
        other => other,
    }
}

fn build_once(home: &Path, name: &str, root: &Path, full: bool) -> Result<IndexSummary, String> {
    if full {
        return build_full_inner(home, name, root, "full", "forced-full");
    }
    let db = store::db_path(home, name);
    if !db.exists() {
        return build_full_inner(home, name, root, "full", "cold-start");
    }
    // Classification inputs (snapshot row + id map) come from one live read
    // transaction: mutually consistent by construction.
    let mut live = match store::open_live(&db) {
        Ok(conn) => conn,
        Err(_) => return fallback_full(home, name, root, "db-unreadable"),
    };
    let (old, stored) = {
        let read_tx = live.transaction().map_err(|e| e.to_string())?;
        let old = match store::latest_snapshot(&read_tx)? {
            Some(s) => s,
            None => return fallback_full(home, name, root, "no-snapshot"),
        };
        let stored = store::file_id_map(&read_tx, old.id)?;
        (old, stored)
    };
    // Classification inputs are owned from here on; drop the live handle so
    // the staging transaction below is the only reader of the live file.
    drop(live);
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve root: {e}"))?;
    if canon_root.to_string_lossy() != old.root {
        return fallback_full(home, name, root, "root-moved");
    }
    if old.parser_version != store::PARSER_VERSION {
        return fallback_full(home, name, root, "parser-changed");
    }
    if old.grammar_versions != store::grammar_versions() {
        return fallback_full(home, name, root, "grammar-changed");
    }
    if old.config_fingerprint != config_fingerprint(&canon_root) {
        return fallback_full(home, name, root, "config-changed");
    }
    let live_git = git::fingerprint(&canon_root);
    if live_git.head != old.head {
        return fallback_full(home, name, root, "head-changed");
    }
    if live_git.branch != old.branch {
        return fallback_full(home, name, root, "branch-changed");
    }
    build_incremental(home, name, root, &old, &stored, &db)
}

/// Re-verify every stored file digest of a snapshot against current source.
/// Uses exactly the comparison the query-time evaluator runs, so build
/// admission and verification can never disagree: legitimately skipped files
/// (too large, binary, unreadable-and-absent) confirm nothing but incriminate
/// nothing, while added, removed, changed, or newly ineligible files fail.
/// There is no vacuous `true` for empty snapshots: a source appearing
/// afterwards reports `file-added`.
pub fn verify_snapshot_content(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    canon_root: &Path,
) -> Result<bool, String> {
    Ok(content_diff(conn, snapshot_id, canon_root)?.is_empty())
}

/// Compare stored file digests against a fresh content walk. Only parsed
/// files are stored, so only they are compared; ignore changes surface via
/// the config fingerprint instead.
fn content_diff(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    canon_root: &Path,
) -> Result<Vec<String>, String> {
    let stored: BTreeMap<String, String> = store::files_of(conn, snapshot_id)?
        .into_iter()
        .map(|f| (f.path, f.digest))
        .collect();
    let (plan, _) = scan::plan_walk(canon_root)?;
    let mut reasons = Vec::new();
    let mut seen = BTreeMap::new();
    for item in &plan {
        if !matches!(item.action, scan::PlanAction::Parse(_)) {
            continue;
        }
        // Mirror the build's skip decisions exactly: files the build
        // legitimately skipped (too large, binary, unreadable-and-absent)
        // must not incriminate a snapshot as changed.
        if std::fs::metadata(&item.full)
            .map(|m| m.len() > scan::MAX_FILE_BYTES)
            .unwrap_or(false)
        {
            if stored.contains_key(&item.rel) && !reasons.iter().any(|r| r == "content-changed") {
                reasons.push("content-changed".to_string());
            }
            continue;
        }
        let bytes = match std::fs::read(&item.full) {
            Ok(b) => b,
            Err(_) => {
                if stored.contains_key(&item.rel) {
                    if !item.full.exists() {
                        if !reasons.iter().any(|r| r == "file-removed") {
                            reasons.push("file-removed".to_string());
                        }
                    } else if !reasons.iter().any(|r| r == "content-changed") {
                        reasons.push("content-changed".to_string());
                    }
                }
                continue;
            }
        };
        if bytes.len() as u64 > scan::MAX_FILE_BYTES {
            if stored.contains_key(&item.rel) && !reasons.iter().any(|r| r == "content-changed") {
                reasons.push("content-changed".to_string());
            }
            continue;
        }
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            if stored.contains_key(&item.rel) && !reasons.iter().any(|r| r == "content-changed") {
                reasons.push("content-changed".to_string());
            }
            continue;
        }
        seen.insert(item.rel.clone(), store::content_digest(&bytes));
    }
    for (path, digest) in &seen {
        match stored.get(path) {
            None => {
                if !reasons.iter().any(|r| r == "file-added") {
                    reasons.push("file-added".to_string());
                }
            }
            Some(old) if old != digest && !reasons.iter().any(|r| r == "content-changed") => {
                reasons.push("content-changed".to_string());
            }
            _ => {}
        }
    }
    for path in stored.keys() {
        if !seen.contains_key(path) && !reasons.iter().any(|r| r == "file-removed") {
            reasons.push("file-removed".to_string());
        }
    }
    Ok(reasons)
}

/// Evaluate freshness against live repository state. Every verdict carries
/// reasons; `fresh` is only returned when nothing contradicts it.
pub fn evaluate(home: &Path, name: &str, root: &Path) -> Freshness {
    let db = store::db_path(home, name);
    if !db.exists() {
        return Freshness {
            status: "unknown".to_string(),
            reasons: vec!["no-index".to_string()],
            snapshot: None,
        };
    }
    let mut conn = match store::open_live(&db) {
        Ok(c) => c,
        Err(_) => {
            return Freshness {
                status: "unknown".to_string(),
                reasons: vec!["db-unreadable".to_string()],
                snapshot: None,
            };
        }
    };
    // One read transaction across snapshot read and verdict inputs: the
    // verdict always describes exactly the revision it names.
    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(_) => {
            return Freshness {
                status: "unknown".to_string(),
                reasons: vec!["db-unreadable".to_string()],
                snapshot: None,
            };
        }
    };
    let freshness = evaluate_tx(&tx, root);
    let _ = tx.commit();
    freshness
}

/// Freshness verdict read over an already-open connection, with the same
/// outcomes as `evaluate`. The CLI calls it on its migrating open; the MCP
/// read path calls it on a read-only connection. Callers hold one read
/// transaction across this plus their fact reads, so verdict and results
/// always describe one revision.
pub fn evaluate_tx(conn: &rusqlite::Connection, root: &Path) -> Freshness {
    let snapshot = match store::latest_snapshot(conn) {
        Ok(Some(s)) => s,
        _ => {
            return Freshness {
                status: "unknown".to_string(),
                reasons: vec!["no-snapshot".to_string()],
                snapshot: None,
            };
        }
    };
    let canon_root = match root.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            return Freshness {
                status: "unknown".to_string(),
                reasons: vec!["root-unreadable".to_string()],
                snapshot: Some(snapshot),
            };
        }
    };
    let (status, reasons) = evaluate_snapshot(conn, "", &canon_root, &snapshot);
    Freshness {
        status,
        reasons,
        snapshot: Some(snapshot),
    }
}

/// Compare one snapshot row against live state on an already-open
/// connection. Callers hold a read transaction across this plus their fact
/// reads so verdict and results always describe one revision.
pub fn evaluate_snapshot(
    conn: &rusqlite::Connection,
    name: &str,
    canon_root: &Path,
    snapshot: &store::Snapshot,
) -> (String, Vec<String>) {
    evaluate_snapshot_with(conn, name, canon_root, snapshot, &git::fingerprint)
}

/// `evaluate_snapshot` with the fingerprint source passed in. `changes` passes its
/// hardened fingerprint, so its freshness check runs no plain `git status`.
pub fn evaluate_snapshot_with(
    conn: &rusqlite::Connection,
    name: &str,
    canon_root: &Path,
    snapshot: &store::Snapshot,
    fingerprint: &dyn Fn(&Path) -> git::GitInfo,
) -> (String, Vec<String>) {
    let _ = name;
    let mut reasons = Vec::new();
    if canon_root.to_string_lossy() != snapshot.root {
        reasons.push("root-moved".to_string());
    }
    let live = fingerprint(canon_root);
    if live.dirty || snapshot.dirty {
        reasons.push("dirty".to_string());
    }
    if snapshot.pre_fingerprint != snapshot.post_fingerprint {
        reasons.push("changed-during-index".to_string());
    }
    if live.head != snapshot.head {
        reasons.push("head-changed".to_string());
    }
    if live.branch != snapshot.branch {
        reasons.push("branch-changed".to_string());
    }
    if snapshot.parser_version != store::PARSER_VERSION {
        reasons.push("parser-changed".to_string());
    }
    if snapshot.grammar_versions != store::grammar_versions() {
        reasons.push("grammar-changed".to_string());
    }
    if snapshot.config_fingerprint != config_fingerprint(canon_root) {
        reasons.push("config-changed".to_string());
    }
    match content_diff(conn, snapshot.id, canon_root) {
        Ok(mut diff) => reasons.append(&mut diff),
        Err(_) => reasons.push("content-unverifiable".to_string()),
    }
    let status = if reasons.is_empty() { "fresh" } else { "stale" };
    (status.to_string(), reasons)
}

/// Candidates a non-resolved by-name answer lists; `truncated` past this.
pub const TARGET_CANDIDATE_LIMIT: usize = 20;

/// A by-name anchor: a definition name, optionally narrowed by path and kind.
pub struct TargetQuery<'a> {
    pub target: &'a str,
    pub in_path: Option<&'a str>,
    pub kind: Option<&'a str>,
}

/// The position a by-name anchor resolves to, or the document that answers instead.
pub enum TargetResolution {
    One { path: String, line: u32, col: u32 },
    Doc(String),
}

/// Resolve a by-name anchor on the latest snapshot of an open read connection.
/// A unique exact name resolves; a substring match never does, so it is always
/// `ambiguous` (or `not-found`), even when it is the only hit.
pub fn resolve_target(
    conn: &rusqlite::Connection,
    repo: &str,
    query: &TargetQuery,
) -> Result<TargetResolution, String> {
    let snapshot = match store::latest_snapshot(conn) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Ok(TargetResolution::Doc(no_index_target_document(repo, query))),
        Err(_) => return Err("index database unreadable; rebuild with `index`".to_string()),
    };
    let (exact_total, exact) = store::exact_symbols(
        conn,
        snapshot.id,
        query.target,
        query.in_path,
        query.kind,
        TARGET_CANDIDATE_LIMIT,
    )?;
    if exact_total == 1 {
        let Some(hit) = exact.first() else {
            return Err("index database unreadable; rebuild with `index`".to_string());
        };
        return Ok(TargetResolution::One {
            path: hit.path.clone(),
            line: hit.start_line,
            col: hit.start_col,
        });
    }
    if exact_total > 1 {
        let doc = target_document(
            repo,
            query,
            "ambiguous",
            exact_total,
            true,
            &exact,
            "exact-name",
        );
        return Ok(TargetResolution::Doc(doc));
    }
    // Substring matches never resolve. Only true substrings of the name count
    // (LIKE, not FTS token prefixes), and the filters run in SQL, so `total` is
    // every match that passes them and `not-found` means no match at all.
    let (total, matches) = store::substring_symbols(
        conn,
        snapshot.id,
        query.target,
        query.in_path,
        query.kind,
        TARGET_CANDIDATE_LIMIT,
    )?;
    if total == 0 {
        return Ok(TargetResolution::Doc(target_document(
            repo,
            query,
            "not-found",
            0,
            true,
            &[],
            "name-contains",
        )));
    }
    let doc = target_document(
        repo,
        query,
        "ambiguous",
        total,
        true,
        &matches,
        "name-contains",
    );
    Ok(TargetResolution::Doc(doc))
}

/// The answer when no code index exists yet: nothing to resolve, nothing created.
pub fn no_index_target_document(repo: &str, query: &TargetQuery) -> String {
    target_document(repo, query, "no-index", 0, true, &[], "name-contains")
}

/// The non-resolved by-name document; key order is fixed for CLI and MCP alike.
fn target_document(
    repo: &str,
    query: &TargetQuery,
    status: &str,
    total: usize,
    total_exact: bool,
    hits: &[store::Hit],
    reason: &str,
) -> String {
    let candidates: Vec<String> = hits
        .iter()
        .take(TARGET_CANDIDATE_LIMIT)
        .map(|hit| {
            format!(
                "{{\"path\":\"{}\",\"line\":{},\"col\":{},\"kind\":\"{}\",\"reason\":\"{reason}\"}}",
                crate::json::escape(&hit.path),
                hit.start_line,
                hit.start_col,
                crate::json::escape(&hit.kind)
            )
        })
        .collect();
    format!(
        "{{\"schema_version\":1,\"repo\":\"{}\",\"status\":\"{status}\",\"target\":\"{}\",\"in\":{},\"kind\":{},\"total\":{total},\"total_exact\":{total_exact},\"candidates\":[{}],\"truncated\":{}}}",
        crate::json::escape(repo),
        crate::json::escape(query.target),
        json_optional(query.in_path),
        json_optional(query.kind),
        candidates.join(","),
        total > TARGET_CANDIDATE_LIMIT
    )
}

fn json_optional(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{}\"", crate::json::escape(value)),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod target_tests {
    use super::{no_index_target_document, target_document, TargetQuery, TARGET_CANDIDATE_LIMIT};
    use crate::store::Hit;

    fn hit(path: &str, kind: &str, line: u32) -> Hit {
        Hit {
            snapshot_id: 1,
            path: path.to_string(),
            kind: kind.to_string(),
            name: Some("save".to_string()),
            start_line: line,
            start_col: 1,
            end_line: line,
            end_col: 9,
            byte_start: 0,
            byte_end: 8,
            provider: "syntax".to_string(),
            grammar: "g".to_string(),
            language: "python".to_string(),
        }
    }

    #[test]
    fn document_keeps_key_order_and_escapes_the_target_and_paths() {
        let query = TargetQuery {
            target: "a\"b\\c",
            in_path: Some("d\"e.py"),
            kind: None,
        };
        let hits = [hit("d\"e.py", "method", 3)];
        let doc = target_document("r", &query, "ambiguous", 2, false, &hits, "exact-name");
        assert_eq!(
            doc,
            "{\"schema_version\":1,\"repo\":\"r\",\"status\":\"ambiguous\",\"target\":\"a\\\"b\\\\c\",\"in\":\"d\\\"e.py\",\"kind\":null,\"total\":2,\"total_exact\":false,\"candidates\":[{\"path\":\"d\\\"e.py\",\"line\":3,\"col\":1,\"kind\":\"method\",\"reason\":\"exact-name\"}],\"truncated\":false}"
        );
    }

    #[test]
    fn document_is_truncated_past_the_candidate_limit() {
        let query = TargetQuery {
            target: "save",
            in_path: None,
            kind: None,
        };
        let hits: Vec<Hit> = (1..=21).map(|line| hit("a.py", "function", line)).collect();
        let doc = target_document("r", &query, "ambiguous", 21, true, &hits, "exact-name");
        assert!(doc.contains("\"total\":21,\"total_exact\":true"), "{doc}");
        assert!(doc.ends_with("\"truncated\":true}"), "{doc}");
        assert_eq!(
            doc.matches("\"reason\"").count(),
            TARGET_CANDIDATE_LIMIT,
            "{doc}"
        );
    }

    #[test]
    fn no_index_document_is_empty_and_exact() {
        let query = TargetQuery {
            target: "save",
            in_path: None,
            kind: Some("method"),
        };
        assert_eq!(
            no_index_target_document("r", &query),
            "{\"schema_version\":1,\"repo\":\"r\",\"status\":\"no-index\",\"target\":\"save\",\"in\":null,\"kind\":\"method\",\"total\":0,\"total_exact\":true,\"candidates\":[],\"truncated\":false}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build, build_full, config_fingerprint, cpp_include_literal, diff_files, evaluate,
        evaluate_tx, import_mentions_path, input_fingerprint, lsp_answer_verification,
        lsp_fallback_reason, lsp_hit_from_fact, rust_crate_src_root, suffix_anchors,
        unique_suffix_match,
    };
    use crate::store;
    use crate::testutil::{git_repo, tmpdir};

    fn plain_repo(tag: &str) -> std::path::PathBuf {
        let dir = tmpdir(tag);
        std::fs::write(dir.join("a.py"), b"def f():\n    return g()\n").unwrap();
        dir
    }

    #[test]
    fn a_failed_query_reports_the_diagnostics_of_the_server() {
        // The server may have said why it failed; the reason must not lose it.
        assert_eq!(
            lsp_fallback_reason("server error: refused", &[]),
            "server error: refused"
        );
        let with = lsp_fallback_reason(
            "server error: refused",
            &["diagnostic: error[pp_file_not_found] main.rs:4".to_string()],
        );
        assert!(with.contains("pp_file_not_found"), "{with}");
        assert!(with.starts_with("server error: refused"), "{with}");
    }

    #[test]
    fn the_answer_verdict_is_the_weakest_fact() {
        // The answer's verdict is the weakest fact in it: one stale fact
        // makes the whole answer stale, never the other way round. (A mixed
        // answer cannot be produced through the adapter — a changed queried
        // file marks every fact stale — so the rule is proven here.)
        let hit = |verification: &str| crate::index::SemanticHit {
            symbol_uri: None,
            display_name: None,
            path: "a.rs".to_string(),
            start_line: 1,
            start_col: 2,
            end_line: 1,
            end_col: 5,
            byte_start: 10,
            byte_end: 13,
            roles: 0,
            precision: "Heuristic".to_string(),
            verification: verification.to_string(),
        };
        assert_eq!(lsp_answer_verification(&[], &[]), "Unverified");
        assert_eq!(
            lsp_answer_verification(&[hit("Unverified")], &[]),
            "Unverified"
        );
        assert_eq!(lsp_answer_verification(&[hit("Stale")], &[]), "Stale");
        assert_eq!(
            lsp_answer_verification(&[hit("Unverified")], &[hit("Stale")]),
            "Stale",
            "the weakest fact decides the answer's verdict"
        );
    }

    #[test]
    fn lsp_hits_carry_the_facts_own_precision_and_verification() {
        // A provider fact is never presented as more certain than it is: the
        // hit travels with the fact's own precision and verification, so a
        // fact the adapter marked stale cannot reach the caller as current.
        let mut fact = crate::semantic::Evidence::lsp_location(
            "a.rs",
            "digest",
            crate::semantic::SpanBytes {
                start_line: 1,
                start_col: 2,
                end_line: 1,
                end_col: 5,
                byte_start: 10,
                byte_end: 13,
            },
        );
        let hit = lsp_hit_from_fact(&fact);
        assert_eq!(hit.precision, "Heuristic");
        assert_eq!(hit.verification, "Unverified");
        fact.verification = crate::semantic::Verification::Stale;
        assert_eq!(lsp_hit_from_fact(&fact).verification, "Stale");
        fact.verification = crate::semantic::Verification::Unverified;
        fact.precision = crate::semantic::Precision::Exact;
        assert_eq!(lsp_hit_from_fact(&fact).precision, "Exact");
    }

    #[test]
    fn an_lsp_answer_is_as_certain_as_its_weakest_fact() {
        // The answer's own verdict never overstates the facts it carries.
        let fact = crate::semantic::Evidence::lsp_location(
            "a.rs",
            "digest",
            crate::semantic::SpanBytes {
                start_line: 1,
                start_col: 2,
                end_line: 1,
                end_col: 5,
                byte_start: 10,
                byte_end: 13,
            },
        );
        let plain = lsp_hit_from_fact(&fact);
        assert_eq!(lsp_answer_verification(&[plain], &[]), "Unverified");
        let mut stale = lsp_hit_from_fact(&fact);
        stale.verification = "Stale".to_string();
        assert_eq!(lsp_answer_verification(&[stale], &[]), "Stale");
        // One stale fact is enough: the answer cannot be more certain than
        // its weakest part.
        let other = lsp_hit_from_fact(&fact);
        let mut stale_again = lsp_hit_from_fact(&fact);
        stale_again.verification = "Stale".to_string();
        assert_eq!(lsp_answer_verification(&[other], &[stale_again]), "Stale");
        assert_eq!(lsp_answer_verification(&[], &[]), "Unverified");
    }

    #[test]
    fn skipped_files_do_not_self_invalidate_freshness() {
        // Large, binary, and unsupported files are legitimately absent from
        // the snapshot; evaluate must mirror the build's skip decisions
        // instead of crying file-added (MMSP glew.h regression).
        let home = tmpdir("idx-skips-home");
        let repo = tmpdir("idx-skips");
        std::fs::write(repo.join("a.py"), b"def f():\n    pass\n").unwrap();
        std::fs::write(repo.join("big.py"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        std::fs::write(repo.join("bin.py"), b"\x00\x01").unwrap();
        std::fs::write(repo.join("note.xyz"), b"plain\n").unwrap();
        let summary = build_full(&home, "r", &repo).unwrap();
        assert_eq!(summary.processed, 1);
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(
            fresh.status, "fresh",
            "skips must not self-invalidate: {:?}",
            fresh.reasons
        );
    }

    #[test]
    fn noop_refresh_reparses_zero_files() {
        let home = tmpdir("idx-noop-home");
        let repo = plain_repo("idx-noop");
        let first = build_full(&home, "r", &repo).unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.mode, "incremental");
        assert_eq!(second.reparsed, 0);
        assert_eq!(second.added, 0);
        assert_eq!(second.removed, 0);
        assert_eq!(second.reused, first.processed);
        assert_eq!(second.processed, first.processed);
        assert_eq!(second.status_at_write, "fresh");
        assert_eq!(second.fallback_reason, "none");
    }

    #[test]
    fn evaluate_tx_on_readonly_connection_matches_evaluate() {
        let home = tmpdir("idx-eval-tx-home");
        let repo = plain_repo("idx-eval-tx");
        build_full(&home, "r", &repo).unwrap();
        let expected = evaluate(&home, "r", &repo);
        let db = store::db_path(&home, "r");
        let store::IndexRead::Ready(mut conn) = store::open_read(&db) else {
            panic!("expected a ready read connection for the built index");
        };
        let tx = conn.transaction().unwrap();
        let got = evaluate_tx(&tx, &repo);
        let _ = tx.commit();
        assert_eq!(got.status, expected.status);
        assert_eq!(got.reasons, expected.reasons);
        assert_eq!(
            got.snapshot.as_ref().map(|s| s.id),
            expected.snapshot.as_ref().map(|s| s.id)
        );
    }

    #[test]
    fn one_file_delta_reparses_only_that_file() {
        let home = tmpdir("idx-delta-home");
        let repo = tmpdir("idx-delta");
        std::fs::write(repo.join("a.py"), b"def f():\n    pass\n").unwrap();
        std::fs::write(repo.join("b.py"), b"def g():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        std::fs::write(repo.join("b.py"), b"def g():\n    return 1\n").unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.mode, "incremental");
        assert_eq!(second.reparsed, 1);
        assert_eq!(second.reused, 1);
        assert_eq!(second.status_at_write, "fresh");
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let files = store::files_of(&conn, snap.id).unwrap();
        assert_eq!(files.len(), 2);
        let (total, _) = store::search_symbols(&conn, snap.id, "Worker", 10, 0).unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn add_remove_rename_yield_exact_facts() {
        let home = tmpdir("idx-arr-home");
        let repo = tmpdir("idx-arr");
        std::fs::write(repo.join("a.py"), b"def alpha():\n    pass\n").unwrap();
        std::fs::write(repo.join("b.py"), b"def beta():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        std::fs::remove_file(repo.join("a.py")).unwrap();
        std::fs::rename(repo.join("b.py"), repo.join("c.py")).unwrap();
        std::fs::write(repo.join("d.py"), b"def delta():\n    pass\n").unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.removed, 2, "a.py deleted, b.py renamed away");
        assert_eq!(second.status_at_write, "fresh");
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let files = store::files_of(&conn, snap.id).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["c.py", "d.py"]);
        let (total, _) = store::search_symbols(&conn, snap.id, "alpha", 10, 0).unwrap();
        assert_eq!(total, 0, "deleted facts must not survive");
        let (total, _) = store::search_symbols(&conn, snap.id, "beta", 10, 0).unwrap();
        assert_eq!(total, 1, "renamed facts move with the path");
    }

    #[test]
    fn repeated_changes_do_not_duplicate_facts() {
        let home = tmpdir("idx-dup-home");
        let repo = plain_repo("idx-dup");
        build_full(&home, "r", &repo).unwrap();
        for i in 0..3 {
            std::fs::write(repo.join("a.py"), format!("def f{i}():\n    pass\n")).unwrap();
            build(&home, "r", &repo, false).unwrap();
        }
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        assert_eq!(store::files_of(&conn, snap.id).unwrap().len(), 1);
        let (total, hits) = store::search_symbols(&conn, snap.id, "f", 10, 0).unwrap();
        assert_eq!(total, 1, "exactly one current definition: {hits:?}");
        assert_eq!(hits[0].name.as_deref(), Some("f2"));
    }

    #[test]
    fn empty_repo_stays_fresh_and_empty_file_is_eligible() {
        let home = tmpdir("idx-empty-home");
        let repo = tmpdir("idx-empty");
        let first = build_full(&home, "r", &repo).unwrap();
        assert_eq!(first.status_at_write, "fresh");
        assert_eq!(first.processed, 0);
        std::fs::write(repo.join("a.py"), b"").unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.processed, 1, "zero-byte files are eligible");
        assert_eq!(second.status_at_write, "fresh");
    }

    #[test]
    fn incremental_matches_independent_full_rebuild() {
        // Guard test (Task 5): PASS-first-run expected. Any FAIL stops the
        // line — core parity broken, never loosen normalization to hide it.
        use std::collections::BTreeMap;
        type Span9 = (u32, u32, u32, u32, usize, usize);
        type SymRow = (
            String,
            String,
            Option<String>,
            Span9,
            String,
            String,
            String,
        );
        type ImpRow = (String, Option<String>, Span9, String, String, String);
        type CallRow = (String, Option<String>, Span9, i64, String, String, String);
        #[derive(Debug, PartialEq, PartialOrd, Ord, Eq)]
        struct Facts {
            files: Vec<(String, String, u64, String, u32)>,
            symbols: Vec<SymRow>,
            occurrences: Vec<SymRow>,
            imports: Vec<ImpRow>,
            calls: Vec<CallRow>,
        }
        fn dump(conn: &rusqlite::Connection, snap_id: i64) -> Facts {
            let files = store::files_of(conn, snap_id).unwrap();
            let mut id_map = BTreeMap::new();
            let map = store::file_id_map(conn, snap_id).unwrap();
            for (path, (id, _)) in &map {
                id_map.insert(*id, path.clone());
            }
            let path_of = |id: i64| id_map.get(&id).cloned().unwrap_or_default();
            // Symbols and reference occurrences share one row shape: the kind
            // (or role), the name, the span and the provenance.
            let named_spans = |table: &str, kind_column: &str| -> Vec<SymRow> {
                let mut st = conn
                    .prepare(&format!("SELECT file_id, {kind_column}, name, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language FROM {table} WHERE snapshot_id = ?1"))
                    .unwrap();
                let rows = st
                    .query_map([snap_id], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, i64>(3)? as u32,
                            row.get::<_, i64>(4)? as u32,
                            row.get::<_, i64>(5)? as u32,
                            row.get::<_, i64>(6)? as u32,
                            row.get::<_, i64>(7)? as usize,
                            row.get::<_, i64>(8)? as usize,
                            row.get::<_, String>(9)?,
                            row.get::<_, String>(10)?,
                            row.get::<_, String>(11)?,
                        ))
                    })
                    .unwrap();
                let mut named = Vec::new();
                for r in rows {
                    let r = r.unwrap();
                    named.push((
                        path_of(r.0),
                        r.1,
                        r.2,
                        (r.3, r.4, r.5, r.6, r.7, r.8),
                        r.9,
                        r.10,
                        r.11,
                    ));
                }
                named
            };
            let mut symbols = named_spans("symbols", "kind");
            let mut occurrences = named_spans("reference_occurrences", "role");
            let mut imports = Vec::new();
            {
                let mut st = conn
                    .prepare("SELECT file_id, text, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language FROM imports WHERE snapshot_id = ?1")
                    .unwrap();
                let rows = st
                    .query_map([snap_id], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)? as u32,
                            row.get::<_, i64>(3)? as u32,
                            row.get::<_, i64>(4)? as u32,
                            row.get::<_, i64>(5)? as u32,
                            row.get::<_, i64>(6)? as usize,
                            row.get::<_, i64>(7)? as usize,
                            row.get::<_, String>(8)?,
                            row.get::<_, String>(9)?,
                            row.get::<_, String>(10)?,
                        ))
                    })
                    .unwrap();
                for r in rows {
                    let r = r.unwrap();
                    imports.push((
                        path_of(r.0),
                        r.1,
                        (r.2, r.3, r.4, r.5, r.6, r.7),
                        r.8,
                        r.9,
                        r.10,
                    ));
                }
            }
            let mut calls = Vec::new();
            {
                let mut st = conn
                    .prepare("SELECT file_id, name, start_line, start_col, end_line, end_col, byte_start, byte_end, unresolved, provider, grammar, language FROM calls WHERE snapshot_id = ?1")
                    .unwrap();
                let rows = st
                    .query_map([snap_id], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)? as u32,
                            row.get::<_, i64>(3)? as u32,
                            row.get::<_, i64>(4)? as u32,
                            row.get::<_, i64>(5)? as u32,
                            row.get::<_, i64>(6)? as usize,
                            row.get::<_, i64>(7)? as usize,
                            row.get::<_, i64>(8)?,
                            row.get::<_, String>(9)?,
                            row.get::<_, String>(10)?,
                            row.get::<_, String>(11)?,
                        ))
                    })
                    .unwrap();
                for r in rows {
                    let r = r.unwrap();
                    calls.push((
                        path_of(r.0),
                        r.1,
                        (r.2, r.3, r.4, r.5, r.6, r.7),
                        r.8,
                        r.9,
                        r.10,
                        r.11,
                    ));
                }
            }
            let mut file_vec: Vec<(String, String, u64, String, u32)> = files
                .into_iter()
                .map(|f| (f.path, f.language, f.bytes, f.digest, f.errors))
                .collect();
            file_vec.sort();
            symbols.sort();
            occurrences.sort();
            imports.sort();
            calls.sort();
            Facts {
                files: file_vec,
                symbols,
                occurrences,
                imports,
                calls,
            }
        }
        let home_b = tmpdir("idx-par-b-home");
        let home_c = tmpdir("idx-par-c-home");
        let repo = tmpdir("idx-par");
        std::fs::write(repo.join("a.py"), b"def alpha():\n    pass\n").unwrap();
        std::fs::write(
            repo.join("b.py"),
            b"import os\ndef beta():\n    return os.name\n",
        )
        .unwrap();
        std::fs::write(repo.join("c.js"), b"function gamma() { return 1; }\n").unwrap();
        // A C++ data member: the parity check must cover the field facts the
        // unchanged file carries through the incremental copy path.
        std::fs::write(
            repo.join("fields.cpp"),
            b"struct S {\n  int value = 1;\n};\nint read(S s) {\n  return s.value;\n}\n",
        )
        .unwrap();
        // A C++ file that changes in stage 1: its occurrences are extracted
        // again, while the unchanged file's are copied.
        std::fs::write(
            repo.join("moving.cpp"),
            b"struct M {\n  int v = 0;\n};\nint get(M* m) {\n  return m->v;\n}\n",
        )
        .unwrap();
        std::fs::write(repo.join("bin.py"), b"\x00\x01").unwrap();
        std::fs::write(repo.join("note.xyz"), b"plain\n").unwrap();
        build_full(&home_b, "r", &repo).unwrap();
        // Stage 1: modify. Stage 2: add + delete. Stage 3: rename.
        std::fs::write(
            repo.join("b.py"),
            b"import sys\ndef beta():\n    return sys.argv\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("moving.cpp"),
            b"struct M {\n  int v = 0;\n};\nint get(M* m) {\n  int* p = &m->v;\n  return m->v + *p;\n}\n",
        )
        .unwrap();
        build(&home_b, "r", &repo, false).unwrap();
        std::fs::write(repo.join("d.py"), b"def delta():\n    pass\n").unwrap();
        std::fs::remove_file(repo.join("c.js")).unwrap();
        build(&home_b, "r", &repo, false).unwrap();
        std::fs::rename(repo.join("a.py"), repo.join("z.py")).unwrap();
        let inc = build(&home_b, "r", &repo, false).unwrap();
        assert_eq!(inc.status_at_write, "fresh");
        build_full(&home_c, "r", &repo).unwrap();
        let read = |home: &std::path::Path| {
            let db = store::db_path(home, "r");
            let conn = store::open_live(&db).unwrap();
            let snap = store::latest_snapshot(&conn).unwrap().unwrap();
            dump(&conn, snap.id)
        };
        assert_eq!(read(&home_b), read(&home_c), "incremental must equal full");
        // The comparison above is not vacuous: the unchanged file's rows were
        // copied and the changed file's rows were extracted again.
        let occurrences = read(&home_b).occurrences;
        let roles_in = |path: &str| -> Vec<String> {
            occurrences
                .iter()
                .filter(|o| o.0 == path)
                .map(|o| o.1.clone())
                .collect()
        };
        assert_eq!(roles_in("fields.cpp"), vec!["member-access"]);
        assert_eq!(
            roles_in("moving.cpp"),
            vec!["address-of-binding", "member-access"]
        );
    }

    #[test]
    fn reuse_reads_see_single_revision() {
        // Mechanism pin (Review Focus 2): a held read transaction keeps
        // seeing the old revision while a second connection commits a new
        // snapshot row. The refresh copy inherits this by reading the live
        // database inside transactions, never bare autocommit reads.
        fn snap_with_nonce(nonce: &str) -> store::Snapshot {
            store::Snapshot {
                id: 0,
                repo: "r".to_string(),
                root: "C:\\r".to_string(),
                created_unix: 1,
                head: None,
                branch: None,
                dirty: false,
                untracked: 0,
                parser_version: store::PARSER_VERSION,
                grammar_versions: store::grammar_versions(),
                config_fingerprint: "c".to_string(),
                pre_fingerprint: "p".to_string(),
                post_fingerprint: "p".to_string(),
                status_at_write: "fresh".to_string(),
                nonce: nonce.to_string(),
            }
        }
        let dir = tmpdir("idx-singlerev");
        let path = dir.join("s.db");
        {
            let conn = store::open_live(&path).unwrap();
            store::insert_snapshot(&conn, &snap_with_nonce("nonce-a")).unwrap();
        }
        let mut reader = store::open_live(&path).unwrap();
        let read_tx = reader.transaction().unwrap();
        let before = store::latest_snapshot(&read_tx).unwrap().unwrap();
        {
            let writer = store::open_live(&path).unwrap();
            store::insert_snapshot(&writer, &snap_with_nonce("nonce-b")).unwrap();
        }
        let during = store::latest_snapshot(&read_tx).unwrap().unwrap();
        assert_eq!(before.nonce, during.nonce);
        assert_eq!(during.nonce, "nonce-a");
        drop(read_tx);
        let after = store::latest_snapshot(&reader).unwrap().unwrap();
        assert_eq!(after.nonce, "nonce-b");
    }

    #[test]
    fn symlink_at_known_path_is_not_reused() {
        // Review Focus 3: a path replaced by a symlink must lose its old
        // facts; the link target is never served under the old path.
        let home = tmpdir("idx-symlink-home");
        let repo = tmpdir("idx-symlink");
        std::fs::write(repo.join("a.py"), b"def alpha():\n    pass\n").unwrap();
        std::fs::write(repo.join("b.py"), b"def beta():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        std::fs::remove_file(repo.join("a.py")).unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink("b.py", repo.join("a.py")).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(repo.join("b.py"), repo.join("a.py")).is_ok();
        if !made {
            return; // no symlink privilege on this machine; scan tests cover it
        }
        let second = build(&home, "r", &repo, false).unwrap();
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let files = store::files_of(&conn, snap.id).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["b.py"]);
        let (total, _) = store::search_symbols(&conn, snap.id, "alpha", 10, 0).unwrap();
        assert_eq!(total, 0, "symlink must not serve target facts as a.py");
        let (total, hits) = store::search_symbols(&conn, snap.id, "beta", 10, 0).unwrap();
        assert_eq!(total, 1, "target keeps exactly one fact set: {hits:?}");
        let _ = second;
    }

    #[test]
    fn incompatible_snapshot_falls_back_with_reason() {
        let home = tmpdir("idx-fb-home");
        let repo = plain_repo("idx-fb");
        build_full(&home, "r", &repo).unwrap();
        let db = store::db_path(&home, "r");
        let fabricate = |sql: &str| {
            let conn = store::open_live(&db).unwrap();
            conn.execute(sql, []).unwrap();
        };
        let rebuild_expect = |reason: &str| {
            let summary = build(&home, "r", &repo, false).unwrap();
            assert_eq!(summary.mode, "fallback-full", "for {reason}");
            assert_eq!(summary.fallback_reason, reason, "for {reason}");
            assert_eq!(summary.status_at_write, "fresh");
        };
        fabricate("UPDATE snapshots SET parser_version = 99");
        rebuild_expect("parser-changed");
        fabricate("UPDATE snapshots SET grammar_versions = 'x'");
        rebuild_expect("grammar-changed");
        fabricate("UPDATE snapshots SET head = 'deadbeef'");
        rebuild_expect("head-changed");
        fabricate("UPDATE snapshots SET branch = 'other'");
        rebuild_expect("branch-changed");
        // An ignore rule matching nothing still changes the config.
        std::fs::write(repo.join(".riltraignore"), "zzz-no-match\n").unwrap();
        rebuild_expect("config-changed");
    }

    #[test]
    fn dirty_worktree_with_same_head_stays_incremental() {
        let Some(repo) = git_repo("idx-gitdirty", "main") else {
            return;
        };
        let home = tmpdir("idx-gitdirty-home");
        std::fs::write(repo.join("a.py"), b"def f():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        // Dirty the worktree without touching HEAD: content change only.
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.mode, "incremental");
        // The build itself saw a consistent source; query time still reports
        // the dirty worktree honestly stale (M3 dirty-reason semantics).
        assert_eq!(second.status_at_write, "fresh");
        let eval = evaluate(&home, "r", &repo);
        assert_eq!(eval.status, "stale");
        assert!(eval.reasons.iter().any(|r| r == "dirty"));
    }

    #[test]
    fn ignored_files_are_never_reused() {
        let home = tmpdir("idx-ign-home");
        let repo = tmpdir("idx-ign");
        std::fs::write(repo.join("a.py"), b"def alpha():\n    pass\n").unwrap();
        std::fs::write(repo.join("b.py"), b"def beta():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        std::fs::write(repo.join(".riltraignore"), "b.py\n").unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.mode, "fallback-full");
        assert_eq!(second.fallback_reason, "config-changed");
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let files = store::files_of(&conn, snap.id).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.py");
        let (total, _) = store::search_symbols(&conn, snap.id, "beta", 10, 0).unwrap();
        assert_eq!(total, 0, "now-ignored facts must be gone");
    }

    #[test]
    fn stale_snapshot_still_reuses_verified_files() {
        let home = tmpdir("idx-stalereuse-home");
        let repo = plain_repo("idx-stalereuse");
        build_full(&home, "r", &repo).unwrap();
        // Label the stored snapshot stale the way a raced build would,
        // without changing any source: reuse must still be allowed because
        // every reused file is independently digest-verified.
        let db = store::db_path(&home, "r");
        {
            let conn = store::open_live(&db).unwrap();
            conn.execute(
                "UPDATE snapshots SET status_at_write = 'stale', post_fingerprint = 'raced'",
                [],
            )
            .unwrap();
        }
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.mode, "incremental");
        assert!(second.reused > 0, "unchanged files must be reused");
        assert_eq!(second.reparsed, 0);
        assert_eq!(second.status_at_write, "fresh");
    }

    #[test]
    fn ineligible_transition_drops_facts() {
        let home = tmpdir("idx-inelig-home");
        let repo = tmpdir("idx-inelig");
        std::fs::write(repo.join("a.py"), b"def alpha():\n    pass\n").unwrap();
        std::fs::write(repo.join("b.py"), b"def beta():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        std::fs::write(repo.join("a.py"), b"\x00\x01").unwrap();
        std::fs::write(repo.join("b.py"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.skipped_binary, 1);
        assert_eq!(second.skipped_large, 1);
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        assert!(store::files_of(&conn, snap.id).unwrap().is_empty());
        for name in ["alpha", "beta"] {
            let (total, _) = store::search_symbols(&conn, snap.id, name, 10, 0).unwrap();
            assert_eq!(total, 0, "{name} facts must be gone");
        }
    }

    #[cfg(unix)]
    #[test]
    fn ineligible_unreadable_drops_facts() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmpdir("idx-inelig-u-home");
        let repo = tmpdir("idx-inelig-u");
        std::fs::write(repo.join("c.py"), b"def gamma():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        std::fs::set_permissions(repo.join("c.py"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.skipped_unreadable, 1);
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        assert!(store::files_of(&conn, snap.id).unwrap().is_empty());
        let (total, _) = store::search_symbols(&conn, snap.id, "gamma", 10, 0).unwrap();
        assert_eq!(total, 0, "unreadable facts must be gone");
    }

    #[test]
    fn two_repos_stay_isolated_under_incremental() {
        let home = tmpdir("idx-iso-home");
        let a = tmpdir("idx-iso-a");
        let b = tmpdir("idx-iso-b");
        std::fs::write(a.join("x.py"), b"def only_in_a():\n    pass\n").unwrap();
        std::fs::write(b.join("x.py"), b"def only_in_b():\n    pass\n").unwrap();
        build(&home, "ra", &a, false).unwrap();
        build(&home, "rb", &b, false).unwrap();
        std::fs::write(a.join("x.py"), b"def only_in_a():\n    return 1\n").unwrap();
        let ra = build(&home, "ra", &a, false).unwrap();
        let rb = build(&home, "rb", &b, false).unwrap();
        assert_eq!(ra.mode, "incremental");
        assert_eq!(rb.mode, "incremental");
        assert_eq!(rb.reparsed, 0);
        for (repo, symbol) in [("ra", "only_in_b"), ("rb", "only_in_a")] {
            let db = store::db_path(&home, repo);
            let conn = store::open_live(&db).unwrap();
            let snap = store::latest_snapshot(&conn).unwrap().unwrap();
            let (total, _) = store::search_symbols(&conn, snap.id, symbol, 10, 0).unwrap();
            assert_eq!(total, 0, "{symbol} must not leak into {repo}");
        }
    }

    #[test]
    fn case_only_rename_has_no_duplicates() {
        // Guard; Windows CI proves the case-insensitive half.
        let home = tmpdir("idx-case-home");
        let repo = tmpdir("idx-case");
        std::fs::write(repo.join("a.py"), b"def alpha():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        std::fs::rename(repo.join("a.py"), repo.join("A.py")).unwrap();
        let second = build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.removed, 1);
        assert_eq!(second.added, 1);
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        assert_eq!(store::files_of(&conn, snap.id).unwrap().len(), 1);
        let (total, _) = store::search_symbols(&conn, snap.id, "alpha", 10, 0).unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn replaced_revision_falls_back_instead_of_mixing() {
        // Reviewer Critical 1, deterministic: drive `build_incremental`
        // with a superseded revision. The nonce guard must refuse the copy
        // and rebuild fully rather than mix facts across revisions.
        use super::build_incremental;
        let home = tmpdir("idx-revchg-home");
        let repo = plain_repo("idx-revchg");
        build_full(&home, "r", &repo).unwrap();
        let db = store::db_path(&home, "r");
        let (old_a, map_a) = {
            let conn = store::open_live(&db).unwrap();
            let snap = store::latest_snapshot(&conn).unwrap().unwrap();
            let map = store::file_id_map(&conn, snap.id).unwrap();
            (snap, map)
        };
        // Promote an unrelated revision B the normal way.
        std::fs::write(repo.join("b.py"), b"def g():\n    pass\n").unwrap();
        build(&home, "r", &repo, false).unwrap();
        // Now replay classification inputs from revision A against live B.
        let summary = build_incremental(&home, "r", &repo, &old_a, &map_a, &db).unwrap();
        assert_eq!(summary.mode, "fallback-full");
        assert_eq!(summary.fallback_reason, "revision-changed");
        assert_eq!(summary.status_at_write, "fresh");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let files = store::files_of(&conn, snap.id).unwrap();
        assert_eq!(files.len(), 2, "fallback rebuilds current source whole");
    }

    #[test]
    fn reuse_window_mutation_is_caught() {
        use super::verify_snapshot_content;
        let home = tmpdir("idx-rwindow-home");
        let repo = plain_repo("idx-rwindow");
        build_full(&home, "r", &repo).unwrap();
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let canon = repo.canonicalize().unwrap();
        let stored = store::file_id_map(&conn, snap.id).unwrap();
        let (plan, _) = crate::scan::plan_walk(&canon).unwrap();
        let diff = diff_files(&stored, &plan, &canon);
        assert_eq!(diff.unchanged.len(), 1);
        assert!(
            diff.ingest_rels.is_empty(),
            "clean tree diffs to all-unchanged"
        );
        // Mutate inside the window between the reuse read and final verification.
        std::fs::write(repo.join("a.py"), b"def f():\n    return h()\n").unwrap();
        assert!(!verify_snapshot_content(&conn, snap.id, &canon).unwrap());
        let (plan2, _) = crate::scan::plan_walk(&canon).unwrap();
        let diff2 = diff_files(&stored, &plan2, &canon);
        assert_eq!(diff2.ingest_rels, vec!["a.py".to_string()]);
    }

    #[test]
    fn skipped_files_do_not_break_write_time_fresh() {
        use super::verify_snapshot_content;
        // One indexed file plus supported-extension files skipped since the
        // start (too large / binary): with no changes and all indexed files
        // matching, the build must still claim fresh.
        let home = tmpdir("idx-wverify-home");
        let repo = tmpdir("idx-wverify");
        std::fs::write(repo.join("a.py"), b"def f():\n    pass\n").unwrap();
        std::fs::write(repo.join("big.py"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        std::fs::write(repo.join("bin.py"), b"\x00\x01").unwrap();
        let summary = build_full(&home, "r", &repo).unwrap();
        assert_eq!(summary.processed, 1);
        assert_eq!(
            summary.status_at_write, "fresh",
            "longstanding skips must not break write-time verification"
        );
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let canon = repo.canonicalize().unwrap();
        assert!(verify_snapshot_content(&conn, snap.id, &canon).unwrap());
        // A previously indexed file turning ineligible must still be caught.
        std::fs::write(repo.join("a.py"), vec![b'y'; 1024 * 1024 + 1]).unwrap();
        assert!(
            !verify_snapshot_content(&conn, snap.id, &canon).unwrap(),
            "indexed-then-ineligible must fail verification"
        );
    }

    #[test]
    fn empty_snapshot_does_not_verify_new_sources() {
        use super::verify_snapshot_content;
        // A snapshot that stored nothing must not verify new sources as
        // matching: the empty check is not evidence of agreement.
        let home = tmpdir("idx-vempty-home");
        let repo = tmpdir("idx-vempty");
        std::fs::write(repo.join("note.xyz"), b"plain\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let canon = repo.canonicalize().unwrap();
        assert!(verify_snapshot_content(&conn, snap.id, &canon).unwrap());
        std::fs::write(repo.join("a.py"), b"def f():\n    pass\n").unwrap();
        assert!(
            !verify_snapshot_content(&conn, snap.id, &canon).unwrap(),
            "a source appearing after an empty snapshot is file-added, not verified"
        );
    }

    #[test]
    fn verify_snapshot_content_detects_drift() {
        use super::verify_snapshot_content;
        let home = tmpdir("idx-verify-home");
        let repo = plain_repo("idx-verify");
        build_full(&home, "r", &repo).unwrap();
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        let canon = repo.canonicalize().unwrap();
        assert!(verify_snapshot_content(&conn, snap.id, &canon).unwrap());
        std::fs::write(repo.join("a.py"), b"def f():\n    return changed()\n").unwrap();
        assert!(!verify_snapshot_content(&conn, snap.id, &canon).unwrap());
    }

    #[test]
    fn snapshot_nonces_are_unique_across_rebuilds() {
        let home = tmpdir("idx-nonce-home");
        let repo = plain_repo("idx-nonce");
        build_full(&home, "r", &repo).unwrap();
        let db = store::db_path(&home, "r");
        let first = store::latest_snapshot(&store::open_live(&db).unwrap())
            .unwrap()
            .unwrap()
            .nonce;
        assert!(!first.is_empty());
        std::fs::write(repo.join("b.py"), b"def g():\n    pass\n").unwrap();
        build_full(&home, "r", &repo).unwrap();
        let second = store::latest_snapshot(&store::open_live(&db).unwrap())
            .unwrap()
            .unwrap()
            .nonce;
        assert_ne!(
            first, second,
            "rebuilds must mint fresh revision identities"
        );
    }

    #[test]
    fn fresh_after_clean_build() {
        let home = tmpdir("idx-fresh-home");
        let repo = plain_repo("idx-fresh");
        let summary = build_full(&home, "r", &repo).unwrap();
        assert_eq!(summary.status_at_write, "fresh");
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.definitions, 1);
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "fresh");
        assert!(fresh.reasons.is_empty());
        assert!(fresh.snapshot.is_some());
    }

    #[test]
    fn dirty_worktree_without_head_change_is_stale() {
        let Some(repo) = git_repo("idx-dirty", "main") else {
            return;
        };
        let home = tmpdir("idx-dirty-home");
        build_full(&home, "r", &repo).unwrap();
        assert_eq!(evaluate(&home, "r", &repo).status, "fresh");
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "stale");
        assert!(fresh
            .reasons
            .iter()
            .any(|r| r == "dirty" || r == "content-changed"));
    }

    #[test]
    fn rename_delete_and_new_files_are_stale() {
        let home = tmpdir("idx-rename-home");
        let repo = plain_repo("idx-rename");
        build_full(&home, "r", &repo).unwrap();
        std::fs::rename(repo.join("a.py"), repo.join("b.py")).unwrap();
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "stale");
        assert!(fresh
            .reasons
            .iter()
            .any(|r| r == "file-added" || r == "file-removed"));
        std::fs::remove_file(repo.join("b.py")).unwrap();
        std::fs::write(repo.join("c.py"), b"x = 1\n").unwrap();
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "stale");
        build_full(&home, "r", &repo).unwrap();
        assert_eq!(evaluate(&home, "r", &repo).status, "fresh");
    }

    #[test]
    fn new_commit_and_ignore_change_are_stale() {
        let Some(repo) = git_repo("idx-head", "main") else {
            return;
        };
        let home = tmpdir("idx-head-home");
        build_full(&home, "r", &repo).unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "b.txt"])
            .current_dir(&repo)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-m", "second"])
            .current_dir(&repo)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(st.success());
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "stale");
        assert!(fresh.reasons.iter().any(|r| r == "head-changed"));
        std::fs::write(repo.join(".riltraignore"), "b.txt\n").unwrap();
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "stale");
        assert!(fresh.reasons.iter().any(|r| r == "config-changed"));
    }

    /// A snapshot recorded by parser `version` must read as stale.
    fn assert_parser_version_is_stale(tag: &str, version: u32) {
        let home = tmpdir(&format!("{tag}-home"));
        let repo = plain_repo(tag);
        // Fabricate a snapshot recorded by a different parser build.
        let db = store::db_path(&home, "r");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = store::open_live(&db).unwrap();
        let mut snap = fabricated_snapshot("r", &repo);
        snap.parser_version = version;
        store::insert_snapshot(&conn, &snap).unwrap();
        drop(conn);
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "stale");
        assert!(fresh.reasons.iter().any(|r| r == "parser-changed"));
    }

    #[test]
    fn parser_version_mismatch_is_stale() {
        assert_parser_version_is_stale("idx-parser", super::super::store::PARSER_VERSION + 1);
    }

    #[test]
    fn parser_two_snapshot_is_stale_because_template_terminals_changed() {
        // Parser 2 recorded the template arguments of a C++ call in its
        // terminal span (`PatchMatchController>` for
        // `std::make_shared<mvs::PatchMatchController>`), so an index it
        // wrote must be rebuilt, never read as fresh.
        assert_parser_version_is_stale("idx-parser-two", 2);
    }

    #[test]
    fn changed_during_index_is_recorded_stale() {
        let home = tmpdir("idx-mixed-home");
        let repo = plain_repo("idx-mixed");
        let db = store::db_path(&home, "r");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = store::open_live(&db).unwrap();
        let mut snap = fabricated_snapshot("r", &repo);
        snap.pre_fingerprint = "before".to_string();
        snap.post_fingerprint = "after".to_string();
        snap.status_at_write = "stale".to_string();
        store::insert_snapshot(&conn, &snap).unwrap();
        drop(conn);
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "stale");
        assert!(fresh.reasons.iter().any(|r| r == "changed-during-index"));
    }

    #[test]
    fn missing_index_and_root_are_unknown_never_fresh() {
        let home = tmpdir("idx-unknown-home");
        let repo = plain_repo("idx-unknown");
        let fresh = evaluate(&home, "nope", &repo);
        assert_eq!(fresh.status, "unknown");
        let fresh = evaluate(&home, "r", &repo);
        assert_eq!(fresh.status, "unknown");
        let gone = repo.join("missing");
        let fresh = evaluate(&home, "r", &gone);
        assert_eq!(fresh.status, "unknown");
        assert_ne!(fresh.status, "fresh");
    }

    #[test]
    fn corrupt_staging_leftover_does_not_block_rebuild() {
        let home = tmpdir("idx-staging-home");
        let repo = plain_repo("idx-staging");
        build_full(&home, "r", &repo).unwrap();
        let staging = store::staging_path(&home, "r");
        std::fs::write(&staging, b"corrupt leftover").unwrap();
        let summary = build_full(&home, "r", &repo).unwrap();
        assert_eq!(summary.processed, 1);
        assert!(!staging.exists(), "staging must be consumed by promote");
        assert_eq!(evaluate(&home, "r", &repo).status, "fresh");
    }

    #[test]
    fn fingerprints_cover_git_and_config_not_content() {
        // Pre/post fingerprints guard global inputs (git state, config).
        // File contents are guarded per-file by digests at evaluate time.
        let repo = plain_repo("idx-fp");
        let before = input_fingerprint(&repo);
        std::fs::write(repo.join("a.py"), b"def f():\n    return h()\n").unwrap();
        assert_eq!(
            input_fingerprint(&repo),
            before,
            "content alone is a per-file digest concern"
        );
        let cfg_before = config_fingerprint(&repo);
        std::fs::write(repo.join(".riltraignore"), "x\n").unwrap();
        assert_ne!(config_fingerprint(&repo), cfg_before);
        assert_ne!(input_fingerprint(&repo), before);
    }

    fn fabricated_snapshot(repo: &str, root: &std::path::Path) -> store::Snapshot {
        let canon = root.canonicalize().unwrap().to_string_lossy().to_string();
        store::Snapshot {
            id: 0,
            repo: repo.to_string(),
            root: canon,
            created_unix: 1,
            head: None,
            branch: None,
            dirty: false,
            untracked: 0,
            parser_version: store::PARSER_VERSION,
            grammar_versions: store::grammar_versions(),
            config_fingerprint: config_fingerprint(root),
            pre_fingerprint: "same".to_string(),
            post_fingerprint: "same".to_string(),
            status_at_write: "fresh".to_string(),
            nonce: store::new_nonce(),
        }
    }

    /// The memo resolves each import once per query: repeated calls with one
    /// import_id share a single map entry (the disk walk behind it runs on
    /// the miss path only), a second import gets its own, and every verdict
    /// equals the unmemoized rule. Guards are not weakened: values come from
    /// the same binding + drift checks the loop used before.
    #[test]
    fn import_verdict_memoizes_one_resolution_per_import() {
        use std::collections::BTreeMap;
        let dir = tmpdir("idx-memo");
        std::fs::write(dir.join("a.py"), b"x\n").unwrap();
        let conn = store::open_memory().unwrap();
        let snap = store::insert_snapshot(&conn, &fabricated_snapshot("r", &dir)).unwrap();
        store::insert_file(
            &conn,
            snap,
            &store::FileRow {
                path: "a.py".to_string(),
                language: "python".to_string(),
                bytes: 2,
                digest: store::content_digest(b"x\n"),
                errors: 0,
            },
        )
        .unwrap();
        let live = crate::semantic::LiveState {
            head: "-".to_string(),
            branch: "-".to_string(),
            dirty: false,
            untracked: 0,
            config_fingerprint: config_fingerprint(&dir),
            snapshot_nonce: store::latest_snapshot(&conn).unwrap().unwrap().nonce,
            content_identity: store::content_identity(&conn, snap).unwrap(),
        };
        let mk_import = || store::SemanticImport {
            snapshot_id: snap,
            provider_name: "scip-import".to_string(),
            provider_version: "v".to_string(),
            capability: "definitions,references".to_string(),
            project_root: dir.to_string_lossy().to_string(),
            text_encoding: "UTF8".to_string(),
            artifact_id: "a".to_string(),
            source_revision: crate::semantic::revision_string(&live),
            claimed_source_digest: String::new(),
            content_identity: live.content_identity.clone(),
            external_symbols: "[]".to_string(),
            verification: "Unverified".to_string(),
        };
        let snapshot = store::latest_snapshot(&conn).unwrap().unwrap();
        let imp1 = store::insert_semantic_import(&conn, &mk_import()).unwrap();
        let imp2 = store::insert_semantic_import(&conn, &mk_import()).unwrap();
        let mut memo = BTreeMap::new();
        let first =
            super::import_verdict(&conn, &snapshot, &live, true, &dir, imp1, &mut memo).unwrap();
        assert_eq!(first.0, "Unverified");
        assert_eq!(memo.len(), 1);
        let second =
            super::import_verdict(&conn, &snapshot, &live, true, &dir, imp1, &mut memo).unwrap();
        assert_eq!(first, second, "same import reuses one verdict");
        assert_eq!(memo.len(), 1, "no recomputation for the same import");
        let third =
            super::import_verdict(&conn, &snapshot, &live, true, &dir, imp2, &mut memo).unwrap();
        assert_eq!(third.0, "Unverified");
        assert_eq!(memo.len(), 2, "distinct imports resolve distinctly");
    }

    fn anchor_harness() -> (rusqlite::Connection, i64, std::path::PathBuf) {
        let dir = tmpdir("idx-anchor");
        std::fs::write(dir.join("a.py"), b"def foo():\n    pass\n").unwrap();
        let conn = store::open_memory().unwrap();
        let snap = store::insert_snapshot(&conn, &fabricated_snapshot("r", &dir)).unwrap();
        let fid = store::insert_file(
            &conn,
            snap,
            &store::FileRow {
                path: "a.py".to_string(),
                language: "python".to_string(),
                bytes: 20,
                digest: store::content_digest(b"def foo():\n    pass\n"),
                errors: 0,
            },
        )
        .unwrap();
        store::insert_symbol(
            &conn,
            snap,
            &store::SymbolRow {
                file_id: fid,
                kind: "function".to_string(),
                name: Some("foo".to_string()),
                start_line: 1,
                start_col: 1,
                end_line: 2,
                end_col: 9,
                byte_start: 0,
                byte_end: 20,
                provider: "syntax".to_string(),
                grammar: "python:0.25.0".to_string(),
                language: "python".to_string(),
            },
        )
        .unwrap();
        (conn, snap, dir)
    }

    #[test]
    fn edge_anchor_does_not_materialize_all_snapshot_files() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (conn, snap, dir) = anchor_harness();
        conn.execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<20000)
             INSERT INTO files(snapshot_id,path,language,bytes,digest,errors)
             SELECT ?1,printf('other-%08d.py',x),'python',1,'digest',0 FROM n",
            [snap],
        )
        .unwrap();
        let steps = AtomicUsize::new(0);
        unsafe extern "C" fn limit(ctx: *mut std::ffi::c_void) -> std::ffi::c_int {
            let steps = unsafe { &*(ctx as *const AtomicUsize) };
            i32::from(steps.fetch_add(1, Ordering::Relaxed) >= 1500)
        }
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                conn.handle(),
                100,
                Some(limit),
                &steps as *const AtomicUsize as *mut std::ffi::c_void,
            );
        }
        let result = super::resolve_edge_anchor(&conn, snap, &dir, "a.py", 0, 5, None);
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(conn.handle(), 0, None, std::ptr::null_mut())
        };
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn edge_anchor_reuses_supplied_source_bytes() {
        let (conn, snap, dir) = anchor_harness();
        let source = std::fs::read(dir.join("a.py")).unwrap();
        std::fs::remove_file(dir.join("a.py")).unwrap();
        let anchor = super::resolve_edge_anchor_with_source(
            &conn,
            snap,
            &dir,
            "a.py",
            0,
            5,
            None,
            Some(&source),
        )
        .unwrap();
        assert_eq!(anchor.display.as_deref(), Some("foo"));
    }

    #[test]
    fn rust_crate_src_root_is_the_last_src_component() {
        assert_eq!(
            rust_crate_src_root("crates/cli/src/knowledge_store.rs"),
            Some("crates/cli/src")
        );
        assert_eq!(rust_crate_src_root("src/main.rs"), Some("src"));
        // A path with no crate source root is not guessed from the repo root.
        assert_eq!(rust_crate_src_root("tests/integration.rs"), None);
    }

    #[test]
    fn rust_crate_use_matches_the_importers_crate_source_root() {
        let import = "use crate::knowledge::{NewRecord, RecordVersion};";
        assert!(import_mentions_path(
            "crates/cli/src/knowledge_store.rs",
            "rust",
            import,
            "crates/cli/src/knowledge.rs"
        ));
        assert!(!import_mentions_path(
            "crates/cli/src/knowledge_store.rs",
            "rust",
            import,
            "src/knowledge.rs"
        ));
        // The pre-existing root-crate layout keeps its meaning.
        assert!(import_mentions_path(
            "src/main.rs",
            "rust",
            "use crate::knowledge;",
            "src/knowledge.rs"
        ));
        // A different crate with the same module name is not a candidate.
        assert!(!import_mentions_path(
            "crates/a/src/main.rs",
            "rust",
            "use crate::helper;",
            "crates/b/src/helper.rs"
        ));
        assert!(import_mentions_path(
            "crates/a/src/main.rs",
            "rust",
            "use crate::helper;",
            "crates/a/src/helper.rs"
        ));
    }

    #[test]
    fn rust_grouped_use_never_becomes_a_repo_root_candidate() {
        // Nested list after the module is recognised.
        assert!(import_mentions_path(
            "crates/a/src/grouped.rs",
            "rust",
            "use crate::helper::{self};",
            "crates/a/src/helper.rs"
        ));
        // The unsupported top-level list stays unsupported, not mis-resolved.
        assert!(!import_mentions_path(
            "crates/a/src/grouped.rs",
            "rust",
            "use crate::{helper};",
            "crates/a/src/helper.rs"
        ));
        // An external crate path is not a local file.
        assert!(!import_mentions_path(
            "crates/a/tests/integration.rs",
            "rust",
            "use a::helper;",
            "crates/a/src/helper.rs"
        ));
    }

    #[test]
    fn rust_callers_include_same_crate_calls_but_keep_same_name_as_candidates() {
        let home = tmpdir("rust-callers-home");
        let repo = tmpdir("rust-callers-repo");
        let cli_src = repo.join("crates/cli/src");
        let other_src = repo.join("crates/other/src");
        std::fs::create_dir_all(&cli_src).unwrap();
        std::fs::create_dir_all(&other_src).unwrap();
        std::fs::write(cli_src.join("semantic.rs"), b"pub fn confined_read() {}\n").unwrap();
        std::fs::write(
            cli_src.join("index.rs"),
            b"fn caller() { crate::semantic::confined_read(); }\n",
        )
        .unwrap();
        std::fs::write(
            cli_src.join("sibling.rs"),
            b"fn confined_read() {}\nfn same_name_caller() { confined_read(); }\n",
        )
        .unwrap();
        std::fs::write(
            other_src.join("other.rs"),
            b"fn caller() { confined_read(); }\n",
        )
        .unwrap();
        build_full(&home, "r", &repo).unwrap();

        let result = super::edge_query(
            &home,
            "r",
            &repo,
            &super::EdgeRequest {
                anchor: super::SpanQuery {
                    rel: "crates/cli/src/semantic.rs",
                    line1: 1,
                    col1: 12,
                    refs: false,
                },
                symbol: None,
                direction: super::EdgeDir::Callers,
                limit: 20,
                offset: 0,
            },
        )
        .unwrap();

        let paths: Vec<_> = result
            .edges
            .iter()
            .map(|hit| hit.edge.caller_path.as_str())
            .collect();
        assert_eq!(
            paths,
            ["crates/cli/src/index.rs", "crates/cli/src/sibling.rs"]
        );
        assert!(result.edges.iter().all(|hit| {
            hit.edge.edge_kind == "POTENTIAL_CALL"
                && hit.edge.precision == "Heuristic"
                && hit.edge.callee.is_none()
        }));
    }

    #[test]
    fn python_import_alias_callers_are_candidates_only_for_the_imported_target() {
        let home = tmpdir("python-alias-callers-home");
        let repo = tmpdir("python-alias-callers-repo");
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        std::fs::create_dir_all(repo.join("other")).unwrap();
        std::fs::write(repo.join("pkg/mod.py"), b"def target():\n    pass\n").unwrap();
        std::fs::write(
            repo.join("main.py"),
            b"from pkg.mod import target as alias\ndef caller():\n    alias()\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("unrelated_module.py"),
            b"from other.mod import target as alias\ndef caller():\n    alias()\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("unrelated_symbol.py"),
            b"from pkg.mod import other as alias\ndef caller():\n    alias()\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("module_alias.py"),
            b"import pkg.mod as alias\ndef caller():\n    alias()\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("shadowed.py"),
            b"from pkg.mod import target as alias\ndef caller(alias):\n    alias()\n",
        )
        .unwrap();
        build_full(&home, "r", &repo).unwrap();
        let conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let snapshot = store::latest_snapshot(&conn).unwrap().unwrap();
        let live = super::live_state_for(&conn, &snapshot).unwrap();
        let (file_id, digest) =
            store::file_id_map(&conn, snapshot.id).unwrap()["pkg/mod.py"].clone();
        let import_id = store::insert_semantic_import(
            &conn,
            &store::SemanticImport {
                snapshot_id: snapshot.id,
                provider_name: "scip-import".to_string(),
                provider_version: "v".to_string(),
                capability: "definitions,references".to_string(),
                project_root: repo.to_string_lossy().into_owned(),
                text_encoding: "UTF8".to_string(),
                artifact_id: "python-alias-target".to_string(),
                source_revision: crate::semantic::revision_string(&live),
                claimed_source_digest: String::new(),
                content_identity: live.content_identity.clone(),
                external_symbols: "[]".to_string(),
                verification: "Unverified".to_string(),
            },
        )
        .unwrap();
        store::insert_semantic_facts(
            &conn,
            &[store::SemanticFact {
                snapshot_id: snapshot.id,
                import_id,
                file_id: Some(file_id),
                rel_path: "pkg/mod.py".to_string(),
                file_digest: digest,
                symbol_uri: Some("scip-test pkg 1 pkg.mod/target().".to_string()),
                display_name: None,
                roles: 1,
                start_line: 0,
                start_col: 4,
                end_line: 0,
                end_col: 10,
                byte_start: 4,
                byte_end: 10,
                precision: "Exact".to_string(),
                relationships: "[]".to_string(),
                external: false,
            }],
        )
        .unwrap();
        drop(conn);

        let result = super::edge_query(
            &home,
            "r",
            &repo,
            &super::EdgeRequest {
                anchor: super::SpanQuery {
                    rel: "pkg/mod.py",
                    line1: 1,
                    col1: 6,
                    refs: false,
                },
                symbol: None,
                direction: super::EdgeDir::Callers,
                limit: 20,
                offset: 0,
            },
        )
        .unwrap();

        assert_eq!(result.anchor.kind.as_str(), "semantic");
        assert_eq!(result.anchor.display, None);
        assert_eq!(result.anchor.candidate_display.as_deref(), Some("target"));
        assert_eq!(
            result
                .edges
                .iter()
                .map(|hit| hit.edge.caller_path.as_str())
                .collect::<Vec<_>>(),
            ["main.py", "shadowed.py"]
        );
        for hit in &result.edges {
            assert_eq!(hit.edge.edge_kind, "POTENTIAL_CALL");
            assert_eq!(hit.edge.callee_name, "alias");
            assert_eq!(hit.edge.precision, "Heuristic");
            assert_eq!(hit.verification, "Unverified");
            assert_eq!(hit.edge.unresolved.as_deref(), Some("alias"));
            assert_eq!(hit.edge.callee, None);
        }
    }

    #[test]
    fn cpp_include_literal_reads_only_the_quoted_form() {
        assert_eq!(
            cpp_include_literal("#include \"colmap/mvs/patch_match.h\"").as_deref(),
            Some("colmap/mvs/patch_match.h")
        );
        // A Windows-authored include separates the path with backslashes.
        assert_eq!(
            cpp_include_literal("#include \"colmap\\mvs\\patch_match.h\"").as_deref(),
            Some("colmap/mvs/patch_match.h")
        );
        // An angle-bracket include names a root or system header; a repository
        // path suffix can never establish which one.
        assert_eq!(cpp_include_literal("#include <numeric>"), None);
        assert_eq!(cpp_include_literal("#include \"\""), None);
    }

    #[test]
    fn suffix_anchors_excludes_the_bare_filename() {
        assert_eq!(
            suffix_anchors("colmap/src/colmap/mvs/patch_match.h"),
            vec![
                "colmap/src/colmap/mvs/patch_match.h",
                "src/colmap/mvs/patch_match.h",
                "colmap/mvs/patch_match.h",
                "mvs/patch_match.h",
            ]
        );
        // A bare filename is the one form a system header can shadow.
        assert!(suffix_anchors("patch_match.h").is_empty());
    }

    #[test]
    fn unique_suffix_match_is_unique_or_nothing() {
        let paths = ["a/b/x.h".to_string(), "c/x.h".to_string()];
        assert_eq!(unique_suffix_match("b/x.h", paths.iter()), Some("a/b/x.h"));
        // Two files share the suffix: ambiguity is never resolved arbitrarily.
        assert_eq!(unique_suffix_match("x.h", paths.iter()), None);
        assert_eq!(unique_suffix_match("z.h", paths.iter()), None);
    }

    /// Semantic anchor: narrowest enclosing occurrence wins, import-scoped.
    #[test]
    fn edge_anchor_semantic_picks_narrowest() {
        let (conn, snap, dir) = anchor_harness();
        let imp = store::insert_semantic_import(
            &conn,
            &store::SemanticImport {
                snapshot_id: snap,
                provider_name: "scip-import".to_string(),
                provider_version: "v".to_string(),
                capability: "definitions,references".to_string(),
                project_root: "C:\\r".to_string(),
                text_encoding: "UTF8".to_string(),
                artifact_id: "a1b2".to_string(),
                source_revision: "r".to_string(),
                claimed_source_digest: String::new(),
                content_identity: "ci".to_string(),
                external_symbols: "[]".to_string(),
                verification: "Unverified".to_string(),
            },
        )
        .unwrap();
        let fact = |uri: &str, roles: u32, span: (u32, u32, u32, u32), bytes: (usize, usize)| {
            store::SemanticFact {
                snapshot_id: snap,
                import_id: imp,
                file_id: None,
                rel_path: "a.py".to_string(),
                file_digest: store::content_digest(&std::fs::read(dir.join("a.py")).unwrap()),
                symbol_uri: Some(uri.to_string()),
                display_name: Some("foo".to_string()),
                roles,
                start_line: span.0,
                start_col: span.1,
                end_line: span.2,
                end_col: span.3,
                byte_start: bytes.0,
                byte_end: bytes.1,
                precision: "Heuristic".to_string(),
                relationships: "[]".to_string(),
                external: false,
            }
        };
        store::insert_semantic_facts(
            &conn,
            &[
                fact("u-outer", 0x1, (0, 0, 1, 8), (0, 21)),
                fact("u-inner", 0x0, (0, 4, 0, 7), (4, 7)),
            ],
        )
        .unwrap();
        let anchor = super::resolve_edge_anchor(&conn, snap, &dir, "a.py", 0, 5, None).unwrap();
        assert_eq!(anchor.symbol_uri.as_deref(), Some("u-inner"));
        assert_eq!(anchor.import_id, Some(imp));
        assert!(anchor.uncertainty.is_none());
    }

    /// Two distinct identities at the same narrowest span: explicit
    /// ambiguity, never an arbitrary pick.
    #[test]
    fn edge_anchor_ambiguous_distinct_uris() {
        let (conn, snap, dir) = anchor_harness();
        let imp = store::insert_semantic_import(
            &conn,
            &store::SemanticImport {
                snapshot_id: snap,
                provider_name: "scip-import".to_string(),
                provider_version: "v".to_string(),
                capability: "definitions,references".to_string(),
                project_root: "C:\\r".to_string(),
                text_encoding: "UTF8".to_string(),
                artifact_id: "a1b2".to_string(),
                source_revision: "r".to_string(),
                claimed_source_digest: String::new(),
                content_identity: "ci".to_string(),
                external_symbols: "[]".to_string(),
                verification: "Unverified".to_string(),
            },
        )
        .unwrap();
        let fact = |uri: &str| store::SemanticFact {
            snapshot_id: snap,
            import_id: imp,
            file_id: None,
            rel_path: "a.py".to_string(),
            file_digest: store::content_digest(&std::fs::read(dir.join("a.py")).unwrap()),
            symbol_uri: Some(uri.to_string()),
            display_name: Some("x".to_string()),
            roles: 0x0,
            start_line: 0,
            start_col: 4,
            end_line: 0,
            end_col: 7,
            byte_start: 4,
            byte_end: 7,
            precision: "Heuristic".to_string(),
            relationships: "[]".to_string(),
            external: false,
        };
        store::insert_semantic_facts(&conn, &[fact("u-a"), fact("u-b")]).unwrap();
        let err = super::resolve_edge_anchor(&conn, snap, &dir, "a.py", 0, 5, None).unwrap_err();
        assert!(err.contains("ambiguous"), "unexpected: {err}");
        assert!(
            err.contains("u-a") && err.contains("u-b"),
            "candidates listed: {err}"
        );
    }

    /// No semantic facts: nearest enclosing syntax definition with explicit
    /// uncertainty, never a fabricated identity.
    #[test]
    fn edge_anchor_syntax_fallback_with_uncertainty() {
        let (conn, snap, dir) = anchor_harness();
        let anchor = super::resolve_edge_anchor(&conn, snap, &dir, "a.py", 0, 5, None).unwrap();
        assert_eq!(anchor.kind.as_str(), "syntax-fallback");
        assert!(anchor.symbol_uri.is_none());
        assert!(anchor.uncertainty.is_some());
        let endpoint = super::anchor_endpoint(&anchor).unwrap();
        assert!(matches!(endpoint, store::EdgeEndpoint::SyntaxDef { .. }));
    }

    /// `--symbol` contradicting the position identity fails loudly.
    #[test]
    fn edge_anchor_symbol_mismatch_errors() {
        let (conn, snap, dir) = anchor_harness();
        let err = super::resolve_edge_anchor(&conn, snap, &dir, "a.py", 0, 5, Some("u-else"))
            .unwrap_err();
        assert!(
            err.contains("mismatch") || err.contains("unresolved"),
            "unexpected: {err}"
        );
    }

    /// One read transaction pins the snapshot a query answers from: rows
    /// written by a later promotion stay invisible inside it (WAL isolation
    /// the edge queries rely on for snapshot consistency).
    #[test]
    fn edge_query_sees_single_snapshot_during_promotion() {
        let home = tmpdir("idx-edgepin-home");
        let repo = tmpdir("idx-edgepin");
        std::fs::write(repo.join("a.py"), b"def f():\n    pass\n").unwrap();
        super::build_full(&home, "r", &repo).unwrap();
        let db = store::db_path(&home, "r");
        let mut conn = store::open_live(&db).unwrap();
        let tx = conn.transaction().unwrap();
        // Nonce (never rowid: promotion copies staging rows wholesale, so
        // ids restart) pins the revision this transaction answers from.
        let pinned = store::latest_snapshot(&tx).unwrap().unwrap().nonce.clone();
        std::fs::write(repo.join("a.py"), b"def f():\n    pass\n    pass\n").unwrap();
        super::build(&home, "r", &repo, false).unwrap();
        let fresh = store::open_live(&db).unwrap();
        let moved = store::latest_snapshot(&fresh).unwrap().unwrap().nonce;
        assert_ne!(moved, pinned, "rebuild must advance the revision");
        let still = store::latest_snapshot(&tx).unwrap().unwrap().nonce;
        assert_eq!(still, pinned, "read txn keeps one consistent snapshot");
    }

    #[test]
    fn task4_actual_query_pins_anchor_edges_and_evidence_across_promotion() {
        let home = tmpdir("edge-query-pin-home");
        let repo = tmpdir("edge-query-pin-repo");
        std::fs::write(
            repo.join("a.py"),
            b"def target():\n    pass\n\ndef caller():\n    target()\n",
        )
        .unwrap();
        super::build_full(&home, "r", &repo).unwrap();
        let conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let old = store::latest_snapshot(&conn).unwrap().unwrap();
        drop(conn);
        let request = super::EdgeRequest {
            anchor: super::SpanQuery {
                rel: "a.py",
                line1: 1,
                col1: 6,
                refs: false,
            },
            symbol: None,
            direction: super::EdgeDir::Callers,
            limit: 50,
            offset: 0,
        };
        let pinned = super::edge_query_inner(&home, "r", &repo, &request, || {
            let writer_home = home.clone();
            let writer_repo = repo.clone();
            std::thread::spawn(move || {
                std::fs::write(
                    writer_repo.join("b.py"),
                    b"from a import target\n\ndef second():\n    target()\n",
                )
                .unwrap();
                super::build(&writer_home, "r", &writer_repo, false).unwrap();
            })
            .join()
            .unwrap();
        })
        .unwrap();
        let next = super::edge_query(&home, "r", &repo, &request).unwrap();
        assert_eq!(pinned.snapshot.nonce, old.nonce);
        assert_ne!(next.snapshot.nonce, old.nonce);
        assert_eq!(pinned.completeness.total, 1);
        assert_eq!(next.completeness.total, 2);
        assert_eq!(pinned.anchor.path, "a.py");
        assert_eq!(pinned.edges.len(), 1);
        assert_eq!(pinned.edges[0].edge.caller_path, "a.py");
        assert_eq!(pinned.edges[0].edge.snapshot_id, old.id);
        assert_eq!(
            pinned.edges[0].evidence.digest,
            store::content_digest(&std::fs::read(repo.join("a.py")).unwrap())
        );
        assert_eq!(
            pinned.verification, "Stale",
            "new file invalidates the OLD source snapshot, never upgrades it"
        );
        assert_eq!(next.freshness, "fresh");
    }

    #[test]
    fn task5_trace_pins_paths_and_digests_across_concurrent_promotion() {
        let home = tmpdir("trace-pin-home");
        let repo = tmpdir("trace-pin-repo");
        let original = b"def f():\n    mystery()\n";
        std::fs::write(repo.join("a.py"), original).unwrap();
        super::build_full(&home, "r", &repo).unwrap();
        let conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let snapshot = store::latest_snapshot(&conn).unwrap().unwrap();
        let live = super::live_state_for(&conn, &snapshot).unwrap();
        let (file_id, digest) = store::file_id_map(&conn, snapshot.id).unwrap()["a.py"].clone();
        let import_id = store::insert_semantic_import(
            &conn,
            &store::SemanticImport {
                snapshot_id: snapshot.id,
                provider_name: "scip-test".to_string(),
                provider_version: "1".to_string(),
                capability: "definitions,references".to_string(),
                project_root: repo.to_string_lossy().into_owned(),
                text_encoding: "UTF8".to_string(),
                artifact_id: "trace-pin-artifact".to_string(),
                source_revision: crate::semantic::revision_string(&live),
                claimed_source_digest: String::new(),
                content_identity: live.content_identity.clone(),
                external_symbols: "[]".to_string(),
                verification: "Unverified".to_string(),
            },
        )
        .unwrap();
        store::insert_semantic_facts(
            &conn,
            &[store::SemanticFact {
                snapshot_id: snapshot.id,
                import_id,
                file_id: Some(file_id),
                rel_path: "a.py".to_string(),
                file_digest: digest,
                symbol_uri: Some("scip-test pkg 1 a.py/f().".to_string()),
                display_name: Some("f".to_string()),
                roles: 1,
                start_line: 0,
                start_col: 4,
                end_line: 0,
                end_col: 5,
                byte_start: 4,
                byte_end: 5,
                precision: "Exact".to_string(),
                relationships: "[]".to_string(),
                external: false,
            }],
        )
        .unwrap();
        crate::edges::upsert_semantic_edges(&conn, snapshot.id, import_id).unwrap();
        drop(conn);
        let request = super::TraceRequest {
            source: super::SpanQuery {
                rel: "a.py",
                line1: 1,
                col1: 5,
                refs: false,
            },
            symbol: None,
            destination: None,
            depth: 3,
            limit: 50,
            offset: 0,
            summary: false,
            include_tests: false,
        };
        let changed = b"def f():\n    different()\n";
        let pinned = super::trace_query_inner(&home, "r", &repo, &request, || {
            let writer_home = home.clone();
            let writer_repo = repo.clone();
            std::thread::spawn(move || {
                // A slow pin hook (a rebuild on a loaded CI runner) must not
                // spend the traversal's time budget; the sleep alone exceeds it.
                std::thread::sleep(std::time::Duration::from_millis(
                    crate::edges::TRACE_TIME_MS + 200,
                ));
                std::fs::write(writer_repo.join("a.py"), changed).unwrap();
                super::build(&writer_home, "r", &writer_repo, false).unwrap();
            })
            .join()
            .unwrap();
        })
        .unwrap();
        let next = super::trace_query(&home, "r", &repo, &request).unwrap();
        assert_ne!(pinned.snapshot.nonce, next.snapshot.nonce);
        assert_eq!(pinned.anchor.path, "a.py");
        assert_eq!(pinned.anchor.kind.as_str(), "semantic");
        assert_eq!(next.anchor.kind.as_str(), "syntax-fallback");
        let old_hit = &pinned.paths[0].edges[0];
        let new_hit = &next.paths[0].edges[0];
        assert_eq!(old_hit.edge.callee_name, "mystery");
        assert_eq!(new_hit.edge.callee_name, "different");
        assert_eq!(old_hit.edge.snapshot_id, pinned.snapshot.id);
        assert_eq!(old_hit.evidence.digest, store::content_digest(original));
        assert_eq!(new_hit.evidence.digest, store::content_digest(changed));
        assert_eq!(pinned.verification, "Stale");
        assert_eq!(old_hit.verification, "Stale");
        assert_eq!(next.freshness, "fresh");
        assert!(old_hit.edge.callee.is_none());
    }

    #[test]
    fn task6_impact_pins_coverage_and_verdict_during_promotion() {
        let home = tmpdir("impact-pin-home");
        let repo = tmpdir("impact-pin-repo");
        std::fs::write(repo.join("a.py"), b"def f0():\n    missing()\n").unwrap();
        super::build_full(&home, "r", &repo).unwrap();
        let request = super::TraceRequest {
            source: super::SpanQuery {
                rel: "a.py",
                line1: 1,
                col1: 6,
                refs: false,
            },
            symbol: None,
            destination: None,
            depth: 3,
            limit: 50,
            offset: 0,
            summary: false,
            include_tests: false,
        };
        let pinned = super::graph_query_inner(
            &home,
            "r",
            &repo,
            &request,
            || {
                let writer_home = home.clone();
                let writer_repo = repo.clone();
                std::thread::spawn(move || {
                    std::fs::write(writer_repo.join("b.py"), b"def second():\n    other()\n")
                        .unwrap();
                    super::build(&writer_home, "r", &writer_repo, false).unwrap();
                })
                .join()
                .unwrap();
            },
            true,
            false,
        )
        .unwrap();
        let next = super::impact_query(&home, "r", &repo, &request).unwrap();
        assert_ne!(pinned.snapshot.nonce, next.snapshot.nonce);
        assert_eq!(pinned.completeness.denominator["unresolved_target"], 1);
        assert_eq!(next.completeness.denominator["unresolved_target"], 2);
        assert_eq!(pinned.impact_verdict, Some("UNKNOWN"));
        assert_eq!(next.impact_verdict, Some("UNKNOWN"));
        assert_eq!(pinned.verification, "Stale");
        assert_eq!(next.freshness, "fresh");
        assert!(pinned.paths.is_empty() && next.paths.is_empty());
    }
}

#[cfg(test)]
mod test_path_classifier {
    use super::is_test_path;

    #[test]
    fn listed_test_paths_are_classified_and_near_misses_are_not() {
        for path in [
            "tests/a.py",
            "src/test/x.rs",
            "a/__tests__/b.ts",
            "pkg/spec/c.rb",
            "test_lib.py",
            "pkg/test_x.py",
            "pkg/a_test.go",
            "web/x.test.ts",
            "web/x.spec.ts",
            "src/FooTest.java",
        ] {
            assert!(is_test_path(path), "{path}");
        }
        for path in [
            "contest.py",
            "latest/foo.py",
            "src/spectrum/a.py",
            "test.py",
            "src/Foo_test.java",
            "x.test.js",
            "web/x.tsx",
            "pkg/a_spec.rb",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }
}

#[cfg(test)]
mod summary_rules {
    use super::{path_row, summarize, PathRow};
    use crate::edges::TracePath;
    use crate::store::EdgeRow;

    fn edge(caller_path: &str) -> EdgeRow {
        EdgeRow {
            snapshot_id: 1,
            file_id: 1,
            caller: None,
            caller_path: caller_path.to_string(),
            caller_span_bytes: (0, 1),
            callee: None,
            callee_name: "f".to_string(),
            edge_kind: "POTENTIAL_CALL".to_string(),
            precision: "Heuristic".to_string(),
            provider: "syntax".to_string(),
            evidence: "{}".to_string(),
            unresolved: None,
        }
    }

    fn row(files: &[&str], basis: &'static str) -> PathRow {
        path_row(&TracePath {
            edges: files.iter().map(|f| edge(f)).collect(),
            cycle: false,
            truncated: false,
            unresolved: Vec::new(),
            basis,
        })
    }

    /// Two proven-or-unproven paths at distance 1 (one proven, two unproven, so a
    /// swapped bucket changes a count), a candidate at distances 2 and 3 whose first
    /// edge is in another file (so first-edge and last-edge disagree), and one test path.
    fn fixture() -> Vec<PathRow> {
        vec![
            row(&["a.py"], "proven"),
            row(&["b.py"], "unproven"),
            row(&["e.py"], "unproven"),
            row(&["x.py", "c.py"], "candidate-name"),
            row(&["x.py", "y.py", "d.py"], "candidate-name"),
            row(&["tests/t.py"], "candidate-name"),
        ]
    }

    #[test]
    fn test_paths_are_excluded_from_counts_unless_included() {
        let s = summarize(&fixture(), false, true, "UNKNOWN", &[]);
        assert_eq!(s.total, 5);
        assert_eq!(
            s.by_depth,
            vec![(1, 3, 1, 2, 0), (2, 1, 0, 0, 1), (3, 1, 0, 0, 1)]
        );
        assert_eq!(s.distinct_files, 5);
        assert_eq!(s.tests, 1, "tests.count is reported either way");
    }

    #[test]
    fn include_tests_counts_every_path() {
        let s = summarize(&fixture(), true, true, "UNKNOWN", &[]);
        assert_eq!(s.total, 6);
        assert_eq!(
            s.by_depth,
            vec![(1, 4, 1, 2, 1), (2, 1, 0, 0, 1), (3, 1, 0, 0, 1)]
        );
        assert_eq!(s.distinct_files, 6);
        assert_eq!(s.tests, 1);
    }

    #[test]
    fn dependent_file_is_the_last_edge_and_distance_its_edge_count() {
        let r = row(&["x.py", "c.py"], "candidate-name");
        assert_eq!(r.file, "c.py");
        assert_eq!(r.distance, 2);
        assert!(!r.test);
    }
}

#[cfg(test)]
mod name_matching {
    use super::*;

    /// Inserts one file and one function definition named `name` in it; the
    /// counting query reads only these columns.
    fn define(conn: &rusqlite::Connection, path: &str, name: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO files (snapshot_id, path, language, bytes, digest, errors) VALUES (1, ?1, 'python', 1, 'd', 0)",
            rusqlite::params![path],
        )
        .unwrap();
        let file_id: i64 = conn
            .query_row(
                "SELECT id FROM files WHERE snapshot_id = 1 AND path = ?1",
                rusqlite::params![path],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO symbols (snapshot_id, file_id, kind, name, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language) VALUES (1, ?1, 'function', ?2, 1, 1, 1, 1, 0, 1, 'syntax', 'g', 'python')",
            rusqlite::params![file_id, name],
        )
        .unwrap();
    }

    #[test]
    fn names_match_exactly_with_sql_metacharacters_and_quotes() {
        let conn = store::open_memory().unwrap();
        for (path, name) in [
            ("a.py", "a_b"),
            ("b.py", "axb"),
            ("c.py", "a%b"),
            ("d.py", "a'b"),
        ] {
            define(&conn, path, name);
        }
        assert_eq!(definition_paths(&conn, 1, "a_b").unwrap(), vec!["a.py"]);
        assert_eq!(definition_paths(&conn, 1, "a%b").unwrap(), vec!["c.py"]);
        assert_eq!(definition_paths(&conn, 1, "a'b").unwrap(), vec!["d.py"]);
        assert!(definition_paths(&conn, 1, "a").unwrap().is_empty());
    }

    #[test]
    fn a_qualified_name_matches_its_terminal_name_only() {
        let conn = store::open_memory().unwrap();
        define(&conn, "w.cpp", "Widget::run");
        define(&conn, "v.cpp", "Widget::running");
        assert_eq!(definition_paths(&conn, 1, "run").unwrap(), vec!["w.cpp"]);
        assert_eq!(
            definition_paths(&conn, 1, "running").unwrap(),
            vec!["v.cpp"]
        );
    }
}
