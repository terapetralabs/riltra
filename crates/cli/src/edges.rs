// SPDX-License-Identifier: Apache-2.0
//! M6 graph edges (Task 3): taxonomy, callable-owner resolution, derivation.
//! Syntax builds derive `IMPORTS` + syntax-only `POTENTIAL_CALL`; the SCIP
//! import transaction upserts semantic-backed edges with source proof;
//! rebuilds re-derive after semantic carry-over. Traversal (Task 5) and
//! queries (Task 4) build on the `EdgeEndpoint` identity defined in
//! `store.rs`: endpoints link only through validated evidence, never names.

use crate::store::{self, EdgeEndpoint, EdgeRow};
use std::collections::{BTreeMap, BTreeSet};

/// Edge taxonomy. `CALLS` is the strict proven subset; everything else
/// carries its own evidence and never upgrades silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Calls,
    PotentialCall,
    References,
    Imports,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Calls => "CALLS",
            EdgeKind::PotentialCall => "POTENTIAL_CALL",
            EdgeKind::References => "REFERENCES",
            EdgeKind::Imports => "IMPORTS",
        }
    }
}

/// Derivation totals. Reported by upsert/rederive; no performance claims.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EdgeStats {
    pub calls: usize,
    pub potential: usize,
    pub references: usize,
    pub imports: usize,
}

/// Structural endpoint equality: the ONLY way two endpoints link. Cross-kind
/// (`Scip` vs `SyntaxDef`) never unifies by itself — the mapping happens in
/// derivation, where both the syntax definition and the semantic definition
/// fact are in hand as validated evidence.
// Task 5 traversal is the production caller; allow until then.
#[allow(dead_code)]
pub fn unify(a: &EdgeEndpoint, b: &EdgeEndpoint) -> bool {
    a == b
}

/// Import provenance gate: `provider_name` alone is NOT the trust boundary
/// (the row carries the real producer tool name by upsert time). A row
/// counts as SCIP-verified only with the full metadata the trusted importer
/// lifecycle writes: a non-ephemeral provider, a non-empty producer
/// artifact digest, a `references` capability, and a concrete text encoding.
/// The trust root is `import_file` — the only writer of such rows —
/// re-validated here defense-in-depth, so a forged row can never resolve
/// `CALLS` no matter what provider it claims.
pub(crate) fn import_provenance_ok(import: &store::SemanticImport) -> bool {
    import.provider_name != "lsp-subprocess"
        && !import.artifact_id.is_empty()
        && import
            .capability
            .split(',')
            .any(|c| c.trim() == "references")
        && (import.text_encoding == "UTF8" || import.text_encoding == "UTF16")
}

/// Narrowest enclosing CALLABLE definition (`function|method`) containing a
/// byte offset in one file. Class/interface/struct bodies and anonymous
/// scopes never own definite caller edges.
fn owner_of(
    symbols: &[store::SymbolRow],
    file_id: i64,
    call_bs: usize,
) -> Option<&store::SymbolRow> {
    symbols
        .iter()
        .filter(|s| {
            s.file_id == file_id
                && (s.kind == "function" || s.kind == "method")
                && s.byte_start <= call_bs
                && call_bs < s.byte_end
        })
        .min_by_key(|s| s.byte_end - s.byte_start)
}

/// Query code never reparses source. Derivation validates missing declaration
/// and scope evidence from confined, digest-matching bytes, once per file in
/// this derivation. Failure is uncertainty, never permission to guess.
fn source_proofs(
    conn: &rusqlite::Connection,
    snapshot: i64,
) -> Result<BTreeMap<i64, crate::syntax::RelationshipProof>, String> {
    let root: String = conn
        .query_row("SELECT root FROM snapshots WHERE id=?1", [snapshot], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    let mut proofs = BTreeMap::new();
    let Ok(root) = std::path::Path::new(&root).canonicalize() else {
        return Ok(proofs);
    };
    for (path, (id, digest)) in store::file_id_map(conn, snapshot)? {
        let crate::syntax::FileKind::Supported(lang) =
            crate::syntax::classify(std::path::Path::new(&path))
        else {
            continue;
        };
        if let Ok(raw) = crate::semantic::confined_read(&root, &path, crate::scan::MAX_FILE_BYTES) {
            if store::content_digest(&raw) == digest {
                proofs.insert(id, crate::syntax::relationship_proof(lang, &raw));
            }
        }
    }
    Ok(proofs)
}

fn call_owner<'a>(
    symbols: &'a [store::SymbolRow],
    call: &store::CallRow,
    proofs: &BTreeMap<i64, crate::syntax::RelationshipProof>,
) -> Option<&'a store::SymbolRow> {
    let bounds = proofs
        .get(&call.file_id)?
        .owners
        .get(&(call.byte_start, call.byte_end))?
        .as_ref()?;
    owner_of(symbols, call.file_id, call.byte_start)
        .filter(|s| (s.byte_start, s.byte_end) == *bounds)
}

/// Called name of a call: the part of the callee text that the stored
/// terminal span covers, empty when the call has no terminal. Reading the
/// stored span keeps graph names in step with the syntax facts instead of
/// splitting the text a second time.
fn terminal_text(call: &store::CallRow) -> &str {
    let (Some(name), Some(callee), Some(terminal)) = (
        call.name.as_deref(),
        call.callee.as_ref(),
        call.tcallee.as_ref(),
    ) else {
        return "";
    };
    let (Some(start), Some(end)) = (
        terminal.byte_start.checked_sub(callee.byte_start),
        terminal.byte_end.checked_sub(callee.byte_start),
    ) else {
        return "";
    };
    name.get(start..end).unwrap_or("")
}

/// A receiver or scope (`obj.`, `ns::`, `ptr->`) precedes the called name
/// when the terminal starts after the callee does.
fn has_receiver(call: &store::CallRow) -> bool {
    matches!((&call.callee, &call.tcallee), (Some(c), Some(t)) if t.byte_start > c.byte_start)
}

fn evidence_json(path: &str, span_bs: usize, span_be: usize, extra: &str) -> String {
    let path = crate::json::escape(path);
    format!("{{\"path\":\"{path}\",\"span\":[{span_bs},{span_be}]{extra}}}")
}

struct RustAliasProof<'a> {
    target: &'a store::SemanticFact,
    target_uri: &'a str,
    binding: (usize, usize),
    local_use: (usize, usize),
}

fn rust_alias_target<'a>(
    call: &store::CallRow,
    local_use: &'a store::SemanticFact,
    path: &str,
    facts: &'a [store::SemanticFact],
    proofs: &BTreeMap<i64, crate::syntax::RelationshipProof>,
    fid_of: &BTreeMap<&str, i64>,
) -> Option<RustAliasProof<'a>> {
    let local_uri = local_use.symbol_uri.as_deref()?;
    if !crate::scip::is_local_symbol(local_uri)
        || local_use.external
        || local_use.rel_path != path
        || call.tcallee.as_ref().is_none_or(|terminal| {
            (terminal.byte_start, terminal.byte_end) != (local_use.byte_start, local_use.byte_end)
        })
    {
        return None;
    }
    let proof = proofs.get(&call.file_id)?;
    let block = proof
        .call_blocks
        .get(&(call.byte_start, call.byte_end))?
        .as_ref()?;
    let aliases: Vec<_> = proof
        .local_function_aliases
        .iter()
        .filter(|alias| {
            alias.block == *block
                && alias.binding.1 < call.byte_start
                && facts
                    .iter()
                    .filter(|fact| {
                        fact.rel_path == path
                            && fact.roles & 1 != 0
                            && (fact.byte_start, fact.byte_end) == alias.binding
                            && fact.symbol_uri.as_deref() == Some(local_uri)
                            && !fact.external
                    })
                    .count()
                    == 1
        })
        .collect();
    if aliases.len() != 1 {
        return None;
    }
    let alias = aliases[0];
    let targets: Vec<_> = facts
        .iter()
        .filter(|fact| {
            fact.rel_path == path
                && fact.roles & 1 == 0
                && (fact.byte_start, fact.byte_end) == alias.target
                && fact.symbol_uri.is_some()
                && !fact.external
        })
        .collect();
    if targets.len() != 1 {
        return None;
    }
    let target = targets[0];
    let target_uri = target.symbol_uri.as_deref()?;
    let target_definitions: Vec<_> = facts
        .iter()
        .filter(|fact| {
            fact.symbol_uri.as_deref() == Some(target_uri)
                && fact.roles & 1 != 0
                && !fact.external
                && (!crate::scip::is_local_symbol(target_uri) || fact.rel_path == target.rel_path)
        })
        .filter(|fact| {
            fid_of
                .get(fact.rel_path.as_str())
                .and_then(|fid| proofs.get(fid))
                .is_some_and(|source| {
                    source
                        .declarations
                        .contains(&(fact.byte_start, fact.byte_end))
                })
        })
        .collect();
    if target_definitions.len() != 1 {
        return None;
    }
    Some(RustAliasProof {
        target,
        target_uri,
        binding: alias.binding,
        local_use: (local_use.byte_start, local_use.byte_end),
    })
}

/// Factory templates whose first template argument is the object they build.
// shortcut: matched by the called name, not by a semantic proof that the
// callee is `std::make_unique` or `std::make_shared`. Upgrade when a semantic
// provider names the callee.
const FACTORY_CALLEES: [&str; 2] = ["make_unique", "make_shared"];

/// The argument of a factory call, as its stored callee text writes it.
#[derive(Debug, PartialEq)]
struct FactoryArgument<'a> {
    /// Argument as written (`mvs::PatchMatchController`), absolute bytes.
    span: (usize, usize),
    /// Terminal name of the argument (`PatchMatchController`), absolute bytes.
    terminal: (usize, usize),
    name: &'a str,
}

/// Argument of a factory call (`std::make_unique<PatchMatchCuda>(args)`),
/// read from the stored callee text. `None` unless the argument is a plain
/// qualified name: arrays, pointers and other shapes are left out, not
/// guessed. `callee_bs` is the callee's absolute byte offset and `terminal`
/// the absolute range of its called name.
fn factory_argument(
    callee: &str,
    callee_bs: usize,
    terminal: (usize, usize),
) -> Option<FactoryArgument<'_>> {
    let t_start = terminal.0.checked_sub(callee_bs)?;
    let t_end = terminal.1.checked_sub(callee_bs)?;
    if !FACTORY_CALLEES.contains(&callee.get(t_start..t_end)?) {
        return None;
    }
    // The template argument list follows the called name: `<args>`.
    let after = callee.get(t_end..)?;
    let lead = after.len() - after.trim_start().len();
    let args = after.get(lead..)?.strip_prefix('<')?.strip_suffix('>')?;
    let arg = args.trim();
    let arg_at = t_end + lead + 1 + (args.len() - args.trim_start().len());
    // The type's name ends before any nested `<`; its last `::` segment is
    // the terminal name.
    let head = arg[..arg.find('<').unwrap_or(arg.len())].trim_end();
    if !is_plain_qualified_name(head) {
        return None;
    }
    let name_at = head.rfind("::").map_or(0, |i| i + 2);
    Some(FactoryArgument {
        span: (callee_bs + arg_at, callee_bs + arg_at + arg.len()),
        terminal: (
            callee_bs + arg_at + name_at,
            callee_bs + arg_at + head.len(),
        ),
        name: &head[name_at..],
    })
}

/// `a::b::C`: identifier segments joined by `::`, and nothing else.
fn is_plain_qualified_name(name: &str) -> bool {
    !name.is_empty()
        && name.split("::").all(|segment| {
            let mut chars = segment.chars();
            chars
                .next()
                .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
                && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        })
}

/// The factory argument of a call fact, when the call is a factory call.
fn factory_argument_of(call: &store::CallRow) -> Option<FactoryArgument<'_>> {
    let callee = call.callee.as_ref()?;
    let terminal = call.tcallee.as_ref()?;
    factory_argument(
        call.name.as_deref()?,
        callee.byte_start,
        (terminal.byte_start, terminal.byte_end),
    )
}

/// Candidate edge for the type a factory call constructs: a `POTENTIAL_CALL`
/// at the argument's own span. Nothing here proves that the constructor runs,
/// so it is never `CALLS`.
fn factory_candidate(
    snapshot_id: i64,
    call: &store::CallRow,
    path: &str,
    arg: &FactoryArgument<'_>,
    caller: Option<EdgeEndpoint>,
) -> EdgeRow {
    let (bs, be) = arg.span;
    let (ts, te) = arg.terminal;
    let reason = if caller.is_none() {
        "owner-unproven"
    } else {
        "factory-template-argument"
    };
    EdgeRow {
        snapshot_id,
        file_id: call.file_id,
        caller,
        caller_path: path.to_string(),
        caller_span_bytes: (bs, be),
        callee: None,
        callee_name: arg.name.to_string(),
        edge_kind: EdgeKind::PotentialCall.as_str().to_string(),
        precision: "Heuristic".to_string(),
        provider: "syntax".to_string(),
        evidence: evidence_json(
            path,
            bs,
            be,
            &format!(",\"callee_span\":[{bs},{be}],\"terminal_span\":[{ts},{te}]"),
        ),
        unresolved: Some(reason.to_string()),
    }
}

/// Syntax-only derivation: `IMPORTS` verbatim + honest `POTENTIAL_CALL`
/// candidates. Never proves `CALLS`. Reads only; the caller stores the rows.
pub fn derive_syntax_edges(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
) -> Result<Vec<EdgeRow>, String> {
    let symbols = store::symbols_for(conn, snapshot_id)?;
    let proofs = source_proofs(conn, snapshot_id)?;
    let calls = store::calls_for(conn, snapshot_id)?;
    let imports = store::imports_for(conn, snapshot_id)?;
    let id_map = store::file_id_map(conn, snapshot_id)?;
    let mut path_of: BTreeMap<i64, &str> = BTreeMap::new();
    for (path, (fid, _)) in &id_map {
        path_of.insert(*fid, path.as_str());
    }
    let mut edges = Vec::new();
    for import in &imports {
        let path = path_of.get(&import.file_id).copied().unwrap_or("?");
        edges.push(EdgeRow {
            snapshot_id,
            file_id: import.file_id,
            caller: None,
            caller_path: path.to_string(),
            caller_span_bytes: (import.byte_start, import.byte_end),
            callee: None,
            callee_name: import.text.clone().unwrap_or_default(),
            edge_kind: EdgeKind::Imports.as_str().to_string(),
            // Syntax-only: the span is exact, the import is not resolved.
            precision: "Heuristic".to_string(),
            provider: "syntax".to_string(),
            evidence: evidence_json(path, import.byte_start, import.byte_end, ""),
            unresolved: None,
        });
    }
    // Import texts per file for the alias rule (text-match only).
    let mut import_texts: BTreeMap<i64, Vec<&str>> = BTreeMap::new();
    for import in &imports {
        if let Some(text) = import.text.as_deref() {
            import_texts.entry(import.file_id).or_default().push(text);
        }
    }
    for call in &calls {
        let path = path_of.get(&call.file_id).copied().unwrap_or("?");
        let owner = call_owner(&symbols, call, &proofs);
        let caller = owner.map(|o| EdgeEndpoint::SyntaxDef {
            path: path.to_string(),
            byte_start: o.byte_start,
            byte_end: o.byte_end,
        });
        let full = call.name.as_deref().unwrap_or("");
        let terminal = Some(terminal_text(call)).filter(|t| !t.is_empty());
        let reason: Option<String> = if call.callee.is_none() {
            Some("no-callee".to_string())
        } else if caller.is_none() {
            Some("owner-unproven".to_string())
        } else if terminal.is_none() {
            Some("no-callee".to_string())
        } else {
            let t = terminal.unwrap_or("");
            let candidates = symbols
                .iter()
                .filter(|s| {
                    s.file_id == call.file_id
                        && (s.kind == "function" || s.kind == "method")
                        && s.name.as_deref() == Some(t)
                })
                .count();
            let aliased = import_texts
                .get(&call.file_id)
                .map(|texts| {
                    texts
                        .iter()
                        .any(|text| text.contains(" as ") && text.contains(t))
                })
                .unwrap_or(false);
            if aliased {
                Some("alias".to_string())
            } else if candidates > 1 {
                Some(format!("ambiguous:{candidates}"))
            } else if has_receiver(call) {
                Some("dynamic".to_string())
            } else {
                Some("no-overlap".to_string())
            }
        };
        if let Some(arg) = factory_argument_of(call) {
            edges.push(factory_candidate(
                snapshot_id,
                call,
                path,
                &arg,
                caller.clone(),
            ));
        }
        edges.push(EdgeRow {
            snapshot_id,
            file_id: call.file_id,
            caller,
            caller_path: path.to_string(),
            caller_span_bytes: (call.byte_start, call.byte_end),
            callee: None,
            callee_name: terminal.unwrap_or(full).to_string(),
            edge_kind: EdgeKind::PotentialCall.as_str().to_string(),
            precision: "Heuristic".to_string(),
            provider: "syntax".to_string(),
            evidence: evidence_json(path, call.byte_start, call.byte_end, ""),
            unresolved: reason,
        });
    }
    Ok(edges)
}

/// Upsert semantic-backed edges for ONE import, inside the caller's
/// transaction: proven `CALLS` where the evidence rule holds, one
/// `POTENTIAL_CALL` per unproven call site, one `REFERENCES` edge per
/// non-call reference occurrence. Stored import rows are SCIP-provenance by
/// construction; the ephemeral `lsp-subprocess` provider (location-only, no
/// stored identity) yields zero edges — no SCIP identity is ever fabricated
/// for it. Rust local-function aliases derive a call only from matching local
/// binding/use identities, a direct initializer reference, and a source-backed
/// target definition; SCIP does not provide that call edge. Syntax-only
/// potentials this import resolves are replaced, never duplicated.
pub fn upsert_semantic_edges(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    import_id: i64,
) -> Result<EdgeStats, String> {
    let import = store::semantic_import_by_id(conn, import_id)?
        .ok_or_else(|| "import vanished".to_string())?;
    if !import_provenance_ok(&import.1) {
        return Ok(EdgeStats::default());
    }
    let facts = store::semantic_facts_of_import(conn, snapshot_id, import_id)?;
    let proofs = source_proofs(conn, snapshot_id)?;
    let symbols = store::symbols_for(conn, snapshot_id)?;
    let calls = store::calls_for(conn, snapshot_id)?;
    let id_map = store::file_id_map(conn, snapshot_id)?;
    let mut fid_of: BTreeMap<&str, i64> = BTreeMap::new();
    let mut path_of: BTreeMap<i64, &str> = BTreeMap::new();
    let mut digest_of: BTreeMap<i64, &str> = BTreeMap::new();
    for (path, (fid, digest)) in &id_map {
        fid_of.insert(path.as_str(), *fid);
        path_of.insert(*fid, path.as_str());
        digest_of.insert(*fid, digest.as_str());
    }
    // A row ID is not a source revision. Contradictory indexed bytes cannot
    // participate in strict reference/definition proof or owner bridging.
    // Unindexed semantic documents retain M5's digest verification lifecycle.
    let facts: Vec<_> = facts
        .into_iter()
        .filter(|fact| match id_map.get(&fact.rel_path) {
            Some((fid, digest)) => {
                fact.file_id.is_none_or(|bound| bound == *fid) && fact.file_digest == *digest
            }
            None => fact.file_id.is_none(),
        })
        .collect();
    // Definitions by URI; locals additionally keyed by document.
    let mut defs: BTreeMap<&str, Vec<&store::SemanticFact>> = BTreeMap::new();
    for fact in &facts {
        if let Some(uri) = fact.symbol_uri.as_deref() {
            if fact.roles & 0x1 != 0 {
                defs.entry(uri).or_default().push(fact);
            }
        }
    }
    let def_for = |uri: &str, rel: &str| -> Option<&store::SemanticFact> {
        defs.get(uri).and_then(|candidates| {
            candidates
                .iter()
                .find(|d| !crate::scip::is_local_symbol(uri) || d.rel_path == rel)
                .copied()
        })
    };
    // Calls by file for overlap joins.
    let mut calls_by_file: BTreeMap<i64, Vec<&store::CallRow>> = BTreeMap::new();
    for call in &calls {
        calls_by_file.entry(call.file_id).or_default().push(call);
    }
    let empty: Vec<&store::CallRow> = Vec::new();
    let mut terminal_occurrence_sites = BTreeSet::new();
    for fact in &facts {
        if fact.roles & 1 != 0 || fact.symbol_uri.is_none() {
            continue;
        }
        if let Some(fid) = fid_of.get(fact.rel_path.as_str()) {
            for call in calls_by_file.get(fid).unwrap_or(&empty) {
                if call
                    .tcallee
                    .as_ref()
                    .is_some_and(|t| t.byte_start < fact.byte_end && fact.byte_start < t.byte_end)
                {
                    terminal_occurrence_sites.insert((
                        call.file_id,
                        call.byte_start,
                        call.byte_end,
                    ));
                }
            }
        }
    }
    let mut stats = EdgeStats::default();
    let mut resolved: BTreeSet<(i64, usize, usize)> = BTreeSet::new();
    let mut terminal_sites = BTreeSet::new();
    let mut out = Vec::new();
    for fact in &facts {
        let uri = match fact.symbol_uri.as_deref() {
            Some(uri) => uri,
            None => continue,
        };
        if fact.roles & 0x1 != 0 {
            continue; // definitions anchor identity; they are not edges.
        }
        let fid = match fid_of.get(fact.rel_path.as_str()).copied() {
            Some(fid) => fid,
            None => continue,
        };
        let file_calls = calls_by_file.get(&fid).unwrap_or(&empty);
        // Terminal-overlapping calls: CALLS candidates. Callee-overlapping
        // but terminal-missing: object-base reference, never a call.
        let mut terminal_hits = Vec::new();
        let mut callee_hits = Vec::new();
        for call in file_calls {
            if let Some(t) = call.tcallee.as_ref() {
                if t.byte_start < fact.byte_end && fact.byte_start < t.byte_end {
                    terminal_hits.push(*call);
                    continue;
                }
            }
            if let Some(c) = call.callee.as_ref() {
                if c.byte_start < fact.byte_end && fact.byte_start < c.byte_end {
                    callee_hits.push(*call);
                }
            }
        }
        callee_hits.retain(|call| {
            !(call
                .tcallee
                .as_ref()
                .is_some_and(|terminal| fact.byte_end <= terminal.byte_start)
                && terminal_occurrence_sites.contains(&(
                    call.file_id,
                    call.byte_start,
                    call.byte_end,
                )))
        });
        let local_doc = if crate::scip::is_local_symbol(uri) {
            Some(fact.rel_path.clone())
        } else {
            None
        };
        let target = EdgeEndpoint::Scip {
            import_id,
            symbol_uri: uri.to_string(),
            doc: local_doc,
        };
        if terminal_hits.len() == 1 {
            let call = terminal_hits[0];
            let path = path_of.get(&call.file_id).copied().unwrap_or("?");
            let owner = call_owner(&symbols, call, &proofs);
            let site = (call.file_id, call.byte_start, call.byte_end);
            if !terminal_sites.insert(site) {
                continue;
            }
            let terminal = call.tcallee.as_ref().expect("terminal overlap");
            let targets: BTreeSet<_> = facts
                .iter()
                .filter(|candidate| {
                    candidate.roles & 1 == 0
                        && candidate.rel_path == path
                        && candidate.byte_start < terminal.byte_end
                        && terminal.byte_start < candidate.byte_end
                })
                .filter_map(|candidate| candidate.symbol_uri.as_deref())
                .collect();
            if targets.len() > 1 {
                out.push(potential_row(
                    (snapshot_id, import_id),
                    call.file_id,
                    path,
                    call,
                    owner.map(|o| EdgeEndpoint::SyntaxDef {
                        path: path.to_string(),
                        byte_start: o.byte_start,
                        byte_end: o.byte_end,
                    }),
                    terminal_text(call),
                    &format!("ambiguous:{}", targets.len()),
                ));
                stats.potential += 1;
                resolved.insert(site);
                continue;
            }
            let external = fact.external;
            let def = def_for(uri, &fact.rel_path);
            // A reference to a variable/alias binding is not proof of the
            // callable that executes. Require a source-bound callable
            // definition at the producer's target span, never a name join.
            let callable_target = def.is_some_and(|d| {
                fid_of
                    .get(d.rel_path.as_str())
                    .and_then(|id| proofs.get(id))
                    .is_some_and(|p| {
                        p.declarations.iter().any(|(a, b)| {
                            *a <= d.byte_start && d.byte_start < d.byte_end && d.byte_end <= *b
                        })
                    })
            });
            if let (Some(o), Some(alias)) = (
                owner,
                rust_alias_target(call, fact, path, &facts, &proofs, &fid_of),
            ) {
                let callee = EdgeEndpoint::Scip {
                    import_id,
                    symbol_uri: alias.target_uri.to_string(),
                    doc: crate::scip::is_local_symbol(alias.target_uri)
                        .then(|| alias.target.rel_path.clone()),
                };
                let file_digest = digest_of.get(&o.file_id).copied().unwrap_or("");
                let caller = owner_scip(conn, snapshot_id, import_id, o, path, file_digest)
                    .unwrap_or(EdgeEndpoint::SyntaxDef {
                        path: path.to_string(),
                        byte_start: o.byte_start,
                        byte_end: o.byte_end,
                    });
                let ref_start = alias.target.byte_start;
                let ref_end = alias.target.byte_end;
                out.push(EdgeRow {
                    snapshot_id,
                    file_id: call.file_id,
                    caller: Some(caller),
                    caller_path: path.to_string(),
                    caller_span_bytes: (call.byte_start, call.byte_end),
                    callee: Some(callee),
                    callee_name: alias
                        .target
                        .display_name
                        .clone()
                        .unwrap_or_else(|| terminal_text(call).to_string()),
                    edge_kind: EdgeKind::Calls.as_str().to_string(),
                    precision: "Exact".to_string(),
                    provider: "scip-import".to_string(),
                    evidence: evidence_json(
                        path,
                        call.byte_start,
                        call.byte_end,
                        &format!(
                            ",\"ref\":[{ref_start},{ref_end}],\"alias\":{{\"binding\":[{},{}],\"use\":[{},{}],\"initializer\":[{ref_start},{ref_end}]}}",
                            alias.binding.0,
                            alias.binding.1,
                            alias.local_use.0,
                            alias.local_use.1,
                        ),
                    ),
                    unresolved: None,
                });
                stats.calls += 1;
                resolved.insert(site);
                continue;
            }
            match (owner, def, external) {
                (Some(o), Some(_), false) if callable_target => {
                    // Proven call: overlapping reference of the resolved
                    // target + proven callable owner. The caller endpoint
                    // unifies to the owner's SCIP identity when the owner
                    // fact is file-bound in this import, else its
                    // snapshot-local syntax identity.
                    let file_digest = digest_of.get(&o.file_id).copied().unwrap_or("");
                    let caller = owner_scip(conn, snapshot_id, import_id, o, path, file_digest)
                        .unwrap_or(EdgeEndpoint::SyntaxDef {
                            path: path.to_string(),
                            byte_start: o.byte_start,
                            byte_end: o.byte_end,
                        });
                    let terminal = terminal_text(call);
                    out.push(EdgeRow {
                        snapshot_id,
                        file_id: call.file_id,
                        caller: Some(caller),
                        caller_path: path.to_string(),
                        caller_span_bytes: (call.byte_start, call.byte_end),
                        callee: Some(target),
                        callee_name: terminal.to_string(),
                        edge_kind: EdgeKind::Calls.as_str().to_string(),
                        precision: "Exact".to_string(),
                        provider: "scip-import".to_string(),
                        evidence: evidence_json(
                            path,
                            call.byte_start,
                            call.byte_end,
                            &format!(",\"ref\":[{},{}]", fact.byte_start, fact.byte_end),
                        ),
                        unresolved: None,
                    });
                    stats.calls += 1;
                    resolved.insert((call.file_id, call.byte_start, call.byte_end));
                }
                _ => {
                    let reason = if external {
                        "external"
                    } else if owner.is_none() {
                        "owner-unproven"
                    } else if def.is_some() && !callable_target {
                        "target-unproven"
                    } else {
                        "unresolved"
                    };
                    let caller = owner.map(|o| EdgeEndpoint::SyntaxDef {
                        path: path.to_string(),
                        byte_start: o.byte_start,
                        byte_end: o.byte_end,
                    });
                    let terminal = terminal_text(call);
                    out.push(potential_row(
                        (snapshot_id, import_id),
                        call.file_id,
                        path,
                        call,
                        caller,
                        terminal,
                        reason,
                    ));
                    stats.potential += 1;
                    resolved.insert((call.file_id, call.byte_start, call.byte_end));
                }
            }
        } else if terminal_hits.len() > 1 {
            let n = terminal_hits.len();
            for call in &terminal_hits {
                let path = path_of.get(&call.file_id).copied().unwrap_or("?");
                let owner = call_owner(&symbols, call, &proofs);
                let caller = owner.map(|o| EdgeEndpoint::SyntaxDef {
                    path: path.to_string(),
                    byte_start: o.byte_start,
                    byte_end: o.byte_end,
                });
                let terminal = terminal_text(call);
                out.push(potential_row(
                    (snapshot_id, import_id),
                    call.file_id,
                    path,
                    call,
                    caller,
                    terminal,
                    &format!("ambiguous:{n}"),
                ));
                stats.potential += 1;
                resolved.insert((call.file_id, call.byte_start, call.byte_end));
            }
        } else if !callee_hits.is_empty() {
            for call in callee_hits {
                let path = path_of.get(&call.file_id).copied().unwrap_or("?");
                let owner = call_owner(&symbols, call, &proofs);
                let caller = owner.map(|o| EdgeEndpoint::SyntaxDef {
                    path: path.to_string(),
                    byte_start: o.byte_start,
                    byte_end: o.byte_end,
                });
                let terminal = terminal_text(call);
                out.push(potential_row(
                    (snapshot_id, import_id),
                    call.file_id,
                    path,
                    call,
                    caller,
                    terminal,
                    "target-mismatch",
                ));
                stats.potential += 1;
                resolved.insert((call.file_id, call.byte_start, call.byte_end));
            }
        } else {
            // Plain reference: no call site involved. Caller stays unknown
            // with the occurrence location as evidence; never expanded.
            // Exact needs a proven callee endpoint: a definition of the same
            // symbol in this import. Otherwise only the span is known.
            let endpoint_proven = def_for(uri, &fact.rel_path).is_some();
            let precision = if endpoint_proven {
                "Exact"
            } else {
                "Heuristic"
            };
            out.push(EdgeRow {
                snapshot_id,
                file_id: fid,
                caller: None,
                caller_path: fact.rel_path.clone(),
                caller_span_bytes: (fact.byte_start, fact.byte_end),
                callee: Some(target),
                callee_name: fact.display_name.clone().unwrap_or_default(),
                edge_kind: EdgeKind::References.as_str().to_string(),
                precision: precision.to_string(),
                provider: "scip-import".to_string(),
                evidence: evidence_json(&fact.rel_path, fact.byte_start, fact.byte_end, ""),
                unresolved: None,
            });
            stats.references += 1;
        }
    }
    // Replace the syntax-only potentials this import resolved: one row per
    // call site per layer, never duplicates.
    for (file_id, bs, be) in &resolved {
        conn.execute(
            "DELETE FROM code_edges WHERE snapshot_id = ?1 AND file_id = ?2 AND caller_sbs = ?3 AND caller_sbe = ?4 AND edge_kind = 'POTENTIAL_CALL' AND provider = 'syntax'",
            rusqlite::params![snapshot_id, file_id, *bs as i64, *be as i64],
        )
        .map_err(|e| e.to_string())?;
    }
    for edge in &out {
        store::insert_edge(conn, edge)?;
    }
    Ok(stats)
}

fn potential_row(
    scope: (i64, i64),
    file_id: i64,
    path: &str,
    call: &store::CallRow,
    caller: Option<EdgeEndpoint>,
    terminal: &str,
    reason: &str,
) -> EdgeRow {
    let (snapshot_id, import_id) = scope;
    EdgeRow {
        snapshot_id,
        file_id,
        caller,
        caller_path: path.to_string(),
        caller_span_bytes: (call.byte_start, call.byte_end),
        callee: None,
        callee_name: terminal.to_string(),
        edge_kind: EdgeKind::PotentialCall.as_str().to_string(),
        precision: "Heuristic".to_string(),
        provider: "scip-import".to_string(),
        evidence: evidence_json(
            path,
            call.byte_start,
            call.byte_end,
            &format!(",\"import\":{import_id}"),
        ),
        unresolved: Some(reason.to_string()),
    }
}

/// Map a proven owner definition to its SCIP identity in this import when the
/// definition fact is bound to the indexed file: same file, contained
/// occurrence, same import, same display name, AND file-identity evidence —
/// the fact's `file_id` when present, plus a digest match against the files
/// row (stale or foreign facts never bridge, even with the right name).
/// Local symbols additionally require the fact document. `None` keeps the
/// snapshot-local syntax identity. This is the single validated
/// SyntaxDef↔Scip mapping point.
fn owner_scip(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    import_id: i64,
    owner: &store::SymbolRow,
    path: &str,
    file_digest: &str,
) -> Option<EdgeEndpoint> {
    let mut uris =
        store::semantic_owner_uris(conn, snapshot_id, import_id, owner, path, file_digest).ok()?;
    if uris.len() != 1 {
        return None;
    }
    let uri = uris.remove(0);
    Some(EdgeEndpoint::Scip {
        import_id,
        doc: if crate::scip::is_local_symbol(&uri) {
            Some(path.to_string())
        } else {
            None
        },
        symbol_uri: uri,
    })
}

#[derive(Debug, Clone)]
pub struct OwnerBridge {
    pub semantic: EdgeEndpoint,
    pub syntax: EdgeEndpoint,
    pub definition_span: (usize, usize),
    pub digest: String,
}

/// Query aliases reuse the exact same validated owner mapping as derivation.
/// They retrieve unresolved syntax rows; they never change edge identity.
pub(crate) fn owner_bridges(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    endpoint: &EdgeEndpoint,
) -> Result<Vec<OwnerBridge>, String> {
    owner_bridges_checked(conn, snapshot_id, endpoint, None, || false)
}

fn owner_bridges_checked(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    endpoint: &EdgeEndpoint,
    limit: Option<usize>,
    expired: impl Fn() -> bool,
) -> Result<Vec<OwnerBridge>, String> {
    let EdgeEndpoint::Scip {
        import_id,
        symbol_uri,
        doc,
    } = endpoint
    else {
        return Ok(Vec::new());
    };
    let valid = store::semantic_import_by_id(conn, *import_id)?
        .is_some_and(|(_, i)| i.snapshot_id == snapshot_id && import_provenance_ok(&i));
    if !valid {
        return Ok(Vec::new());
    }
    let mut aliases = Vec::new();
    let definitions = store::semantic_definitions_for(
        conn,
        snapshot_id,
        *import_id,
        symbol_uri,
        doc.as_deref(),
        limit.map(|n| n as i64 + 1).unwrap_or(i64::MAX),
    )?;
    if limit.is_some_and(|cap| definitions.len() > cap) {
        return Err("mapping-budget".to_string());
    }
    for fact in definitions {
        if expired() {
            return Err("time-budget".to_string());
        }
        if fact.import_id != *import_id
            || fact.roles & 1 == 0
            || doc.as_ref().is_some_and(|p| *p != fact.rel_path)
        {
            continue;
        }
        let Some((fid, digest)) = store::file_identity(conn, snapshot_id, &fact.rel_path)? else {
            continue;
        };
        if fact.file_digest != digest || !fact.file_id.is_none_or(|bound| bound == fid) {
            continue;
        }
        let symbols = store::symbols_at(conn, snapshot_id, &fact.rel_path)?;
        let Some(owner) = owner_of(&symbols, fid, fact.byte_start) else {
            continue;
        };
        if owner_scip(
            conn,
            snapshot_id,
            *import_id,
            owner,
            &fact.rel_path,
            &digest,
        )
        .is_some_and(|mapped| unify(endpoint, &mapped))
        {
            let alias = EdgeEndpoint::SyntaxDef {
                path: fact.rel_path,
                byte_start: owner.byte_start,
                byte_end: owner.byte_end,
            };
            if !aliases
                .iter()
                .any(|previous: &OwnerBridge| unify(&previous.syntax, &alias))
            {
                aliases.push(OwnerBridge {
                    semantic: endpoint.clone(),
                    syntax: alias,
                    definition_span: (fact.byte_start, fact.byte_end),
                    digest: digest.clone(),
                });
            }
        }
    }
    Ok(aliases)
}

pub(crate) fn syntax_aliases(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    endpoint: &EdgeEndpoint,
) -> Result<Vec<EdgeEndpoint>, String> {
    Ok(owner_bridges(conn, snapshot_id, endpoint)?
        .into_iter()
        .map(|bridge| bridge.syntax)
        .collect())
}

pub const TRACE_MAX_DEPTH: usize = 6;
pub const TRACE_MAX_NODES: usize = 500;
pub const TRACE_MAX_EDGES: usize = 3000;
pub const TRACE_MAX_PATHS: usize = 1000;
pub const TRACE_TIME_MS: u64 = 2000;

pub struct TraceBudget {
    pub depth: usize,
    pub nodes: usize,
    pub edges: usize,
    pub paths: usize,
    /// Candidate rows per node (impact only).
    pub candidates: usize,
    pub deadline: std::time::Instant,
}

impl TraceBudget {
    pub fn new(depth: usize, started: std::time::Instant) -> Self {
        Self {
            depth,
            nodes: TRACE_MAX_NODES,
            edges: TRACE_MAX_EDGES,
            paths: TRACE_MAX_PATHS,
            candidates: 50,
            deadline: started + std::time::Duration::from_millis(TRACE_TIME_MS),
        }
    }
    fn expired(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }
}

#[derive(Clone)]
pub struct TracePath {
    pub edges: Vec<EdgeRow>,
    pub cycle: bool,
    pub truncated: bool,
    pub unresolved: Vec<String>,
    /// Impact only: `proven`, `unproven` or `candidate-name`; empty for trace.
    pub basis: &'static str,
}

/// Name-matched callers of one syntax definition, as `callers` selects them.
pub struct CandidateSeed {
    pub name: String,
    pub paths: Vec<String>,
    pub aliases: Vec<(String, String)>,
}

/// What the name seed of one walk node says.
pub enum NameSeed {
    /// Name-matched rows are read for the node.
    Named(CandidateSeed),
    /// The anchor is a C/C++ data member, which takes no call by name: no pass, no
    /// gap. Other data fields reached on the walk get `Missing`.
    NoName,
    /// No name could be derived: the node is a gap (`candidate-pass-skipped`).
    Missing,
}

/// Seed lookup for one walk node, from its syntax definition.
pub type SeedLookup<'a> = dyn Fn(&EdgeEndpoint) -> Result<NameSeed, String> + 'a;

pub struct Traversal {
    pub paths: Vec<TracePath>,
    pub bridges: Vec<OwnerBridge>,
    pub nodes: usize,
    pub edges: usize,
    pub cycle: bool,
    pub truncated: bool,
    pub unresolved: BTreeSet<String>,
    /// Nodes a candidate pass ran for; candidate rows emitted; every pass read all rows.
    pub candidate_nodes: usize,
    pub candidates_returned: usize,
    pub candidates_complete: bool,
}

impl Traversal {
    fn stop(&mut self, reason: &str) {
        self.truncated = true;
        self.unresolved.insert(reason.to_string());
    }
    fn emit(&mut self, path: TracePath, target: bool, reached: bool, budget: &TraceBudget) {
        self.cycle |= path.cycle;
        self.truncated |= path.truncated;
        self.unresolved.extend(path.unresolved.iter().cloned());
        if !target || reached {
            if self.paths.len() < budget.paths {
                self.paths.push(path);
            } else {
                self.stop("path-budget");
            }
        }
    }
    fn remember(&mut self, bridges: Vec<OwnerBridge>) -> Result<(), String> {
        for bridge in bridges {
            if !self.bridges.iter().any(|old| {
                unify(&old.semantic, &bridge.semantic) && unify(&old.syntax, &bridge.syntax)
            }) {
                if self.bridges.len() == TRACE_MAX_NODES {
                    return Err("mapping-budget".to_string());
                }
                self.bridges.push(bridge);
            }
        }
        Ok(())
    }
}

fn trace_bridges(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    endpoint: &EdgeEndpoint,
    budget: &TraceBudget,
) -> Result<Vec<OwnerBridge>, String> {
    if budget.expired() {
        return Err("time-budget".to_string());
    }
    owner_bridges_checked(conn, snapshot_id, endpoint, Some(TRACE_MAX_NODES), || {
        budget.expired()
    })
}

fn endpoint_import(endpoint: &EdgeEndpoint) -> Option<i64> {
    match endpoint {
        EdgeEndpoint::Scip { import_id, .. } => Some(*import_id),
        _ => None,
    }
}

/// No reverse name inference: a syntax node connects only when the existing
/// owner proof yields exactly one import-scoped semantic identity.
fn trace_node(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    endpoint: &EdgeEndpoint,
    scope: Option<i64>,
    budget: &TraceBudget,
) -> Result<(EdgeEndpoint, Vec<OwnerBridge>, bool), String> {
    let EdgeEndpoint::SyntaxDef {
        path,
        byte_start,
        byte_end,
    } = endpoint
    else {
        return Ok((endpoint.clone(), Vec::new(), false));
    };
    let Some((_, digest)) = store::file_identity(conn, snapshot_id, path)? else {
        return Ok((endpoint.clone(), Vec::new(), false));
    };
    let owner = store::symbols_at(conn, snapshot_id, path)?
        .into_iter()
        .find(|s| {
            s.byte_start == *byte_start
                && s.byte_end == *byte_end
                && (s.kind == "function" || s.kind == "method")
        });
    let Some(owner) = owner else {
        return Ok((endpoint.clone(), Vec::new(), false));
    };
    let mut candidates = Vec::new();
    let imports_count = conn
        .query_row(
            "SELECT COUNT(*) FROM semantic_imports WHERE snapshot_id=?1",
            [snapshot_id],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| e.to_string())?;
    if imports_count > TRACE_MAX_NODES as i64 {
        return Err("mapping-budget".to_string());
    }
    for (id, import) in store::semantic_imports_for(conn, snapshot_id)? {
        if budget.expired() {
            return Err("time-budget".to_string());
        }
        if scope.is_some_and(|want| want != id) || !import_provenance_ok(&import) {
            continue;
        }
        if let Some(mapped) = owner_scip(conn, snapshot_id, id, &owner, path, &digest) {
            if !candidates.iter().any(|old| unify(old, &mapped)) {
                candidates.push(mapped);
            }
        }
    }
    if candidates.len() != 1 {
        return Ok((endpoint.clone(), Vec::new(), candidates.len() > 1));
    }
    let mapped = candidates.remove(0);
    let bridges = trace_bridges(conn, snapshot_id, &mapped, budget)?;
    if !bridges.iter().any(|bridge| unify(&bridge.syntax, endpoint)) {
        return Ok((endpoint.clone(), Vec::new(), false));
    }
    Ok((mapped, bridges, false))
}

/// Bounded breadth-first enumeration. Lineage detects cycles per PATH so a
/// diamond's shared node preserves both routes. All joins use scoped keys.
pub fn traverse(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    start: &EdgeEndpoint,
    target: Option<&EdgeEndpoint>,
    budget: &TraceBudget,
) -> Result<Traversal, String> {
    let mut out = Traversal {
        paths: Vec::new(),
        bridges: Vec::new(),
        nodes: 0,
        edges: 0,
        cycle: false,
        truncated: false,
        unresolved: BTreeSet::new(),
        candidate_nodes: 0,
        candidates_returned: 0,
        candidates_complete: true,
    };
    if !(1..=TRACE_MAX_NODES).contains(&budget.nodes)
        || !(1..=TRACE_MAX_EDGES).contains(&budget.edges)
        || !(1..=TRACE_MAX_PATHS).contains(&budget.paths)
        || budget.depth == 0
        || budget.depth > TRACE_MAX_DEPTH
    {
        return Err("invalid trace budget".to_string());
    }
    if budget.expired() {
        out.stop("time-budget");
        return Ok(out);
    }
    struct State {
        node: EdgeEndpoint,
        edges: Vec<EdgeRow>,
        lineage: Vec<EdgeEndpoint>,
    }
    enum Work {
        Node(State),
        Done(TracePath, bool),
    }
    let mut queue = std::collections::VecDeque::new();
    let result = (|| -> Result<(), String> {
        let (start, bridges, ambiguous) = trace_node(conn, snapshot_id, start, None, budget)?;
        out.remember(bridges)?;
        if ambiguous {
            out.unresolved.insert("ambiguous-owner-mapping".to_string());
        }
        let target = if let Some(target) = target {
            let (node, bridges, ambiguous) =
                trace_node(conn, snapshot_id, target, endpoint_import(&start), budget)?;
            out.remember(bridges)?;
            if ambiguous {
                out.unresolved
                    .insert("ambiguous-destination-mapping".to_string());
            }
            Some(node)
        } else {
            None
        };
        let mut known = vec![start.clone()];
        out.nodes = 1;
        queue.push_back(Work::Node(State {
            node: start.clone(),
            edges: Vec::new(),
            lineage: vec![start],
        }));
        'search: while let Some(work) = queue.pop_front() {
            if budget.expired() {
                out.stop("time-budget");
                break;
            }
            let state = match work {
                Work::Node(state) => state,
                Work::Done(path, reached) => {
                    out.emit(path, target.is_some(), reached, budget);
                    if out.paths.len() >= budget.paths && !queue.is_empty() {
                        out.stop("path-budget");
                        break;
                    }
                    continue;
                }
            };
            if target.as_ref().is_some_and(|t| unify(t, &state.node)) {
                out.emit(
                    TracePath {
                        edges: state.edges,
                        cycle: false,
                        truncated: false,
                        unresolved: Vec::new(),
                        basis: "",
                    },
                    true,
                    true,
                    budget,
                );
                continue;
            }
            let bridges = trace_bridges(conn, snapshot_id, &state.node, budget)?;
            let aliases: Vec<_> = bridges.iter().map(|b| b.syntax.clone()).collect();
            out.remember(bridges)?;
            let selection = store::EdgeSelection {
                snapshot_id,
                endpoint: &state.node,
                inbound: false,
                candidate_name: None,
                candidate_paths: &[],
                candidate_aliases: &[],
                syntax_aliases: &aliases,
            };
            if budget.expired() {
                out.stop("time-budget");
                break;
            }
            // Only a one-row existence probe at the depth boundary; no extra hop.
            if state.edges.len() == budget.depth {
                let more = !store::selected_edge_page(conn, &selection, 1, 0)?.is_empty();
                let reasons = if more {
                    vec!["depth-budget".to_string()]
                } else {
                    Vec::new()
                };
                out.emit(
                    TracePath {
                        edges: state.edges,
                        cycle: false,
                        truncated: more,
                        unresolved: reasons,
                        basis: "",
                    },
                    target.is_some(),
                    false,
                    budget,
                );
                continue;
            }
            let mut offset = 0;
            loop {
                if budget.expired() {
                    out.stop("time-budget");
                    break 'search;
                }
                if out.edges == budget.edges {
                    out.stop("edge-budget");
                    break 'search;
                }
                let batch = (budget.edges - out.edges).min(1000);
                let rows =
                    store::selected_edge_page(conn, &selection, batch as i64, offset as i64)?;
                let count = rows.len();
                if count == 0 && offset == 0 {
                    if state.edges.is_empty() {
                        out.unresolved.insert("no-outbound-edges".to_string());
                    } else {
                        out.emit(
                            TracePath {
                                edges: state.edges.clone(),
                                cycle: false,
                                truncated: false,
                                unresolved: Vec::new(),
                                basis: "",
                            },
                            target.is_some(),
                            false,
                            budget,
                        );
                    }
                }
                for edge in rows {
                    if budget.expired() {
                        out.stop("time-budget");
                        break 'search;
                    }
                    out.edges += 1;
                    if out.paths.len() >= budget.paths {
                        out.stop("path-budget");
                        break 'search;
                    }
                    let mut hops = state.edges.clone();
                    let Some(callee) = edge.callee.as_ref().filter(|_| edge.edge_kind == "CALLS")
                    else {
                        let mut reasons = vec!["unresolved-target".to_string()];
                        if let Some(reason) = &edge.unresolved {
                            reasons.push(reason.clone());
                        }
                        hops.push(edge);
                        queue.push_back(Work::Done(
                            TracePath {
                                edges: hops,
                                cycle: false,
                                truncated: false,
                                unresolved: reasons,
                                basis: "",
                            },
                            false,
                        ));
                        continue;
                    };
                    if endpoint_import(&state.node)
                        .zip(endpoint_import(callee))
                        .is_some_and(|(a, b)| a != b)
                    {
                        return Err("trace edge crosses import scope".to_string());
                    }
                    let (next, bridges, ambiguous) = trace_node(
                        conn,
                        snapshot_id,
                        callee,
                        endpoint_import(&state.node),
                        budget,
                    )?;
                    out.remember(bridges)?;
                    if ambiguous {
                        out.unresolved.insert("ambiguous-owner-mapping".to_string());
                    }
                    let cycle = state.lineage.iter().any(|old| unify(old, &next));
                    if !known.iter().any(|old| unify(old, &next)) {
                        if known.len() == budget.nodes {
                            out.stop("node-budget");
                            out.emit(
                                TracePath {
                                    edges: hops,
                                    cycle: false,
                                    truncated: true,
                                    unresolved: vec!["node-budget".to_string()],
                                    basis: "",
                                },
                                target.is_some(),
                                false,
                                budget,
                            );
                            break 'search;
                        }
                        known.push(next.clone());
                        out.nodes = known.len();
                    }
                    hops.push(edge);
                    let reached = target.as_ref().is_some_and(|t| unify(t, &next));
                    if cycle || reached {
                        queue.push_back(Work::Done(
                            TracePath {
                                edges: hops,
                                cycle,
                                truncated: false,
                                unresolved: if cycle {
                                    vec!["cycle".to_string()]
                                } else {
                                    Vec::new()
                                },
                                basis: "",
                            },
                            reached,
                        ));
                    } else {
                        let mut lineage = state.lineage.clone();
                        lineage.push(next.clone());
                        queue.push_back(Work::Node(State {
                            node: next,
                            edges: hops,
                            lineage,
                        }));
                    }
                }
                offset += count;
                if count < batch {
                    break;
                }
            }
            if out.paths.len() >= budget.paths && !queue.is_empty() {
                out.stop("path-budget");
                break;
            }
        }
        Ok(())
    })();
    if let Err(reason) = result {
        if reason == "time-budget" || reason == "mapping-budget" {
            out.stop(&reason);
        } else {
            return Err(reason);
        }
    }
    while let Some(work) = queue.pop_front() {
        if out.paths.len() >= budget.paths {
            break;
        }
        if let Work::Done(path, reached) = work {
            out.emit(path, target.is_some(), reached, budget);
        }
    }
    if target.is_some() && out.paths.is_empty() {
        let reason = if !out.truncated
            && !out.unresolved.contains("unresolved-target")
            && !out.unresolved.contains("ambiguous-owner-mapping")
            && !out.unresolved.contains("ambiguous-destination-mapping")
        {
            "target-unreachable"
        } else {
            "target-not-proven"
        };
        out.unresolved.insert(reason.to_string());
    }
    Ok(out)
}

/// The syntax definition behind a walk node: the node itself, or the syntax of
/// a bridge whose semantic side is the node.
fn syntax_of(node: &EdgeEndpoint, bridges: &[OwnerBridge]) -> Option<EdgeEndpoint> {
    if matches!(node, EdgeEndpoint::SyntaxDef { .. }) {
        return Some(node.clone());
    }
    // shortcut: only the first bridged syntax definition gets candidates, upgrade when one semantic node maps to several syntax definitions
    bridges
        .iter()
        .find(|b| unify(&b.semantic, node))
        .map(|b| b.syntax.clone())
}

/// What one impact edge did to the walk.
enum Visit {
    /// Emitted and not walked further.
    Leaf,
    /// Emitted; the proven caller to walk from, with the path that reaches it.
    Walk(EdgeEndpoint, Vec<EdgeRow>),
    /// A budget ended the walk; the edge was not emitted.
    Stop,
}

/// One inbound edge of an impact walk: reasons, caller mapping, node budget,
/// then emit. A candidate row is never walked, whatever its relation.
#[allow(clippy::too_many_arguments)]
fn impact_edge(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    out: &mut Traversal,
    known: &mut Vec<EdgeEndpoint>,
    node: &EdgeEndpoint,
    lineage: &[EdgeEndpoint],
    prefix: &[EdgeRow],
    edge: EdgeRow,
    budget: &TraceBudget,
    candidate: bool,
) -> Result<Visit, String> {
    if out.paths.len() == budget.paths {
        out.stop("path-budget");
        return Ok(Visit::Stop);
    }
    out.edges += 1;
    let mut reasons = Vec::new();
    if let Some(reason) = &edge.unresolved {
        reasons.push(reason.clone());
    }
    let proven = matches!(edge.edge_kind.as_str(), "CALLS" | "REFERENCES")
        && edge.precision == "Exact"
        && edge.unresolved.is_none();
    if !proven {
        reasons.push("unproven-relation".to_string());
    }
    let mut next = None;
    if let Some(caller) = &edge.caller {
        if endpoint_import(node)
            .zip(endpoint_import(caller))
            .is_some_and(|(a, b)| a != b)
        {
            return Err("impact edge crosses import scope".to_string());
        }
        let (mapped, bridges, ambiguous) =
            trace_node(conn, snapshot_id, caller, endpoint_import(node), budget)?;
        out.remember(bridges)?;
        if ambiguous {
            reasons.push("ambiguous-owner-mapping".to_string());
        }
        if matches!(mapped, EdgeEndpoint::SyntaxDef { .. }) {
            reasons.push("semantic-midpoint-unavailable".to_string());
        } else if proven && !candidate {
            next = Some(mapped);
        }
    } else {
        reasons.push("unresolved-owner".to_string());
    }
    let cycle = next
        .as_ref()
        .is_some_and(|n| lineage.iter().any(|old| unify(old, n)));
    if let Some(n) = &next {
        if !known.iter().any(|old| unify(old, n)) {
            if known.len() == budget.nodes {
                out.stop("node-budget");
                return Ok(Visit::Stop);
            }
            known.push(n.clone());
            out.nodes = known.len();
        }
    }
    let mut hops = prefix.to_vec();
    hops.push(edge);
    let basis = if candidate {
        "candidate-name"
    } else if proven {
        "proven"
    } else {
        "unproven"
    };
    out.emit(
        TracePath {
            edges: hops.clone(),
            cycle,
            truncated: false,
            unresolved: reasons,
            basis,
        },
        false,
        false,
        budget,
    );
    Ok(match next {
        Some(n) if !cycle => Visit::Walk(n, hops),
        _ => Visit::Leaf,
    })
}

/// The name-matched selection `callers` makes for `seed`, read from `endpoint`.
fn candidate_selection<'a>(
    snapshot_id: i64,
    endpoint: &'a EdgeEndpoint,
    seed: &'a CandidateSeed,
) -> store::EdgeSelection<'a> {
    store::EdgeSelection {
        snapshot_id,
        endpoint,
        inbound: true,
        candidate_name: Some(&seed.name),
        candidate_paths: &seed.paths,
        candidate_aliases: &seed.aliases,
        syntax_aliases: &[],
    }
}

/// One candidate pass over `node`'s name-matched inbound rows, read from
/// `endpoint` (the original anchor for the start). Rows the exact pass already
/// emitted are skipped. Returns `true` only when every row was read.
#[allow(clippy::too_many_arguments)]
fn read_candidates(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    out: &mut Traversal,
    known: &mut Vec<EdgeEndpoint>,
    node: &EdgeEndpoint,
    endpoint: &EdgeEndpoint,
    lineage: &[EdgeEndpoint],
    prefix: &[EdgeRow],
    seed: &CandidateSeed,
    exact: &[EdgeRow],
    budget: &TraceBudget,
) -> Result<bool, String> {
    let selection = candidate_selection(snapshot_id, endpoint, seed);
    let mut returned = 0;
    let mut offset = 0;
    loop {
        if budget.expired() {
            return Err("time-budget".to_string());
        }
        if out.edges == budget.edges {
            out.stop("edge-budget");
            return Ok(false);
        }
        let batch = (budget.edges - out.edges).min(1000);
        let rows = store::selected_edge_page(conn, &selection, batch as i64, offset as i64)?;
        let count = rows.len();
        for edge in rows {
            if budget.expired() {
                return Err("time-budget".to_string());
            }
            if exact.contains(&edge) {
                continue;
            }
            if returned == budget.candidates {
                out.stop("candidate-limit");
                return Ok(false);
            }
            if let Visit::Stop = impact_edge(
                conn,
                snapshot_id,
                out,
                known,
                node,
                lineage,
                prefix,
                edge,
                budget,
                true,
            )? {
                return Ok(false);
            }
            returned += 1;
            out.candidates_returned += 1;
        }
        if count < batch {
            return Ok(true);
        }
        offset += count;
    }
}

/// One distinct node of the proven walk, kept in BFS order for the candidate phase.
struct WalkNode {
    node: EdgeEndpoint,
    prefix: Vec<EdgeRow>,
    lineage: Vec<EdgeEndpoint>,
    exact: Vec<EdgeRow>,
}

/// Reverse BFS. Each emitted path proves one observed dependent, with the
/// nearest-to-anchor edge first. References with unknown owners stay terminal.
/// Phase 1 is the proven walk alone, so candidates never change it. Phase 2
/// then lists, for each distinct node the walk reached, its name-matched callers
/// as `candidate-name` paths, with whatever budget phase 1 left. A candidate is
/// never walked further.
pub fn traverse_impact(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
    start: &EdgeEndpoint,
    budget: &TraceBudget,
    seeds: Option<&SeedLookup<'_>>,
) -> Result<Traversal, String> {
    if !(1..=TRACE_MAX_NODES).contains(&budget.nodes)
        || !(1..=TRACE_MAX_EDGES).contains(&budget.edges)
        || !(1..=TRACE_MAX_PATHS).contains(&budget.paths)
        || !(1..=TRACE_MAX_PATHS).contains(&budget.candidates)
        || !(1..=TRACE_MAX_DEPTH).contains(&budget.depth)
    {
        return Err("invalid impact budget".to_string());
    }
    let mut out = Traversal {
        paths: Vec::new(),
        bridges: Vec::new(),
        nodes: 0,
        edges: 0,
        cycle: false,
        truncated: false,
        unresolved: BTreeSet::new(),
        candidate_nodes: 0,
        candidates_returned: 0,
        candidates_complete: true,
    };
    let original = start.clone();
    let mut known: Vec<EdgeEndpoint> = Vec::new();
    let mut records: Vec<WalkNode> = Vec::new();
    let walked = (|| -> Result<(), String> {
        if budget.expired() {
            return Err("time-budget".to_string());
        }
        let (start, bridges, ambiguous) = trace_node(conn, snapshot_id, &original, None, budget)?;
        out.remember(bridges)?;
        if ambiguous {
            out.unresolved.insert("ambiguous-owner-mapping".to_string());
        }
        known.push(start.clone());
        out.nodes = 1;
        let mut queue =
            std::collections::VecDeque::from([(start.clone(), Vec::<EdgeRow>::new(), vec![start])]);
        while let Some((node, prefix, lineage)) = queue.pop_front() {
            if budget.expired() {
                return Err("time-budget".to_string());
            }
            let selection = store::EdgeSelection {
                snapshot_id,
                endpoint: &node,
                inbound: true,
                candidate_name: None,
                candidate_paths: &[],
                candidate_aliases: &[],
                syntax_aliases: &[],
            };
            let fresh = !records.iter().any(|r| unify(&r.node, &node));
            if prefix.len() == budget.depth {
                if fresh {
                    records.push(WalkNode {
                        node: node.clone(),
                        prefix,
                        lineage,
                        exact: Vec::new(),
                    });
                }
                if !store::selected_edge_page(conn, &selection, 1, 0)?.is_empty() {
                    out.stop("depth-budget");
                }
                continue;
            }
            let mut exact = Vec::new();
            let mut offset = 0;
            loop {
                if budget.expired() {
                    return Err("time-budget".to_string());
                }
                if out.edges == budget.edges {
                    out.stop("edge-budget");
                    return Ok(());
                }
                let batch = (budget.edges - out.edges).min(1000);
                let rows =
                    store::selected_edge_page(conn, &selection, batch as i64, offset as i64)?;
                let count = rows.len();
                for edge in rows {
                    if budget.expired() {
                        return Err("time-budget".to_string());
                    }
                    exact.push(edge.clone());
                    match impact_edge(
                        conn,
                        snapshot_id,
                        &mut out,
                        &mut known,
                        &node,
                        &lineage,
                        &prefix,
                        edge,
                        budget,
                        false,
                    )? {
                        Visit::Stop => return Ok(()),
                        Visit::Leaf => {}
                        Visit::Walk(next, hops) => {
                            let mut ancestors = lineage.clone();
                            ancestors.push(next.clone());
                            queue.push_back((next, hops, ancestors));
                        }
                    }
                }
                offset += count;
                if count < batch {
                    break;
                }
            }
            if fresh {
                records.push(WalkNode {
                    node,
                    prefix,
                    lineage,
                    exact,
                });
            }
        }
        Ok(())
    })();
    if let Err(reason) = walked {
        if reason == "time-budget" || reason == "mapping-budget" {
            out.stop(&reason);
        } else {
            return Err(reason);
        }
    }
    if let Some(seeds) = seeds {
        let candidated = (|| -> Result<(), String> {
            for rec in &records {
                if out.edges == budget.edges {
                    out.stop("edge-budget");
                    out.candidates_complete = false;
                    return Ok(());
                }
                if out.paths.len() == budget.paths {
                    out.stop("path-budget");
                    out.candidates_complete = false;
                    return Ok(());
                }
                if budget.expired() {
                    return Err("time-budget".to_string());
                }
                // The start uses the anchor endpoint as given; another Scip node is
                // bridged to its syntax definition on demand, as `trace` bridges.
                let found = if rec.prefix.is_empty() {
                    seeds(&original)?
                } else {
                    let syntax = match syntax_of(&rec.node, &out.bridges) {
                        Some(syntax) => Some(syntax),
                        None if matches!(rec.node, EdgeEndpoint::Scip { .. }) => {
                            let bridged = trace_bridges(conn, snapshot_id, &rec.node, budget)?;
                            syntax_of(&rec.node, &bridged)
                        }
                        None => None,
                    };
                    match syntax {
                        Some(syntax) => seeds(&syntax)?,
                        None => NameSeed::Missing,
                    }
                };
                let end = if rec.prefix.is_empty() {
                    &original
                } else {
                    &rec.node
                };
                match found {
                    NameSeed::NoName => {}
                    NameSeed::Missing => {
                        out.unresolved.insert("candidate-pass-skipped".to_string());
                        out.candidates_complete = false;
                    }
                    NameSeed::Named(seed) if rec.prefix.len() == budget.depth => {
                        // One hop past the depth budget: state it, read nothing more.
                        let probe = candidate_selection(snapshot_id, end, &seed);
                        if !store::selected_edge_page(conn, &probe, 1, 0)?.is_empty() {
                            out.stop("depth-budget");
                            out.candidates_complete = false;
                        }
                    }
                    NameSeed::Named(seed) => {
                        out.candidate_nodes += 1;
                        let done = read_candidates(
                            conn,
                            snapshot_id,
                            &mut out,
                            &mut known,
                            &rec.node,
                            end,
                            &rec.lineage,
                            &rec.prefix,
                            &seed,
                            &rec.exact,
                            budget,
                        )?;
                        out.candidates_complete &= done;
                    }
                }
            }
            Ok(())
        })();
        if let Err(reason) = candidated {
            if reason == "time-budget" || reason == "mapping-budget" {
                out.stop(&reason);
            } else {
                return Err(reason);
            }
        }
    }
    if out.candidates_returned > 0 {
        out.unresolved
            .insert("candidate-name-not-followed".to_string());
    }
    // A stopped walk may have skipped nodes, so their candidates are unread.
    out.candidates_complete &= !out.truncated;
    Ok(out)
}

/// A successful bounded search cannot prove an exhaustive program graph.
/// The positive levels describe observed static evidence, never safety.
pub fn impact_verdict(traversal: &Traversal, uncertain: bool) -> &'static str {
    if uncertain || traversal.truncated || !traversal.unresolved.is_empty() {
        return "UNKNOWN";
    }
    let mut calls = false;
    let mut references = false;
    for path in &traversal.paths {
        for edge in &path.edges {
            if edge.precision != "Exact"
                || edge.caller.is_none()
                || edge.callee.is_none()
                || edge.unresolved.is_some()
            {
                return "UNKNOWN";
            }
            match edge.edge_kind.as_str() {
                "CALLS" => calls = true,
                "REFERENCES" => references = true,
                _ => return "UNKNOWN",
            }
        }
    }
    if calls {
        "HIGH"
    } else if references {
        "MEDIUM"
    } else {
        "UNKNOWN"
    }
}

/// Full re-derivation after semantic carry-over and `import_id` rebinding.
/// Edges are never carried or copied: staging always re-derives from its own
/// facts, so remapped `file_id`/`import_id` values cannot dangle.
pub fn rederive_after_carry(
    conn: &rusqlite::Connection,
    snapshot_id: i64,
) -> Result<EdgeStats, String> {
    conn.execute(
        "DELETE FROM code_edges WHERE snapshot_id = ?1",
        rusqlite::params![snapshot_id],
    )
    .map_err(|e| e.to_string())?;
    let mut totals = EdgeStats::default();
    for edge in derive_syntax_edges(conn, snapshot_id)? {
        store::insert_edge(conn, &edge)?;
        match edge.edge_kind.as_str() {
            "IMPORTS" => totals.imports += 1,
            _ => totals.potential += 1,
        }
    }
    for (id, _) in store::semantic_imports_for(conn, snapshot_id)? {
        let stats = upsert_semantic_edges(conn, snapshot_id, id)?;
        totals.calls += stats.calls;
        totals.potential += stats.potential;
        totals.references += stats.references;
    }
    store::mark_graph_proof(conn, snapshot_id)?;
    Ok(totals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task7_changed_or_missing_source_cannot_prove_callable_owner() {
        let conn = mem_db();
        let snapshot = mk_snap(&conn);
        ingest_py(&conn, snapshot, "a.py", b"def f():\n    missing()\n");
        let initial = derive_syntax_edges(&conn, snapshot).unwrap();
        assert!(initial[0].caller.is_some());
        let root: String = conn
            .query_row("SELECT root FROM snapshots WHERE id=?1", [snapshot], |r| {
                r.get(0)
            })
            .unwrap();
        let path = std::path::Path::new(&root).join("a.py");
        std::fs::write(&path, b"def f():\n    different()\n").unwrap();
        let changed = derive_syntax_edges(&conn, snapshot).unwrap();
        assert!(changed[0].caller.is_none());
        assert_eq!(changed[0].unresolved.as_deref(), Some("owner-unproven"));
        std::fs::remove_file(path).unwrap();
        let missing = derive_syntax_edges(&conn, snapshot).unwrap();
        assert!(missing[0].caller.is_none());
        assert_eq!(missing[0].unresolved.as_deref(), Some("owner-unproven"));
    }

    #[test]
    fn task6_impact_budget_guards_and_verdicts_never_claim_safety() {
        let (conn, snapshot, start) = trace_store(&[(1, 0), (2, 1), (3, 0)]);
        let full = traverse_impact(
            &conn,
            snapshot,
            &start,
            &TraceBudget::new(3, std::time::Instant::now()),
            None,
        )
        .unwrap();
        assert_eq!(full.paths.len(), 3);
        assert_eq!(impact_verdict(&full, false), "HIGH");
        assert_eq!(impact_verdict(&full, true), "UNKNOWN");
        for cutoff in ["nodes", "edges", "paths", "time", "depth"] {
            let mut budget = TraceBudget::new(3, std::time::Instant::now());
            match cutoff {
                "nodes" => budget.nodes = 1,
                "edges" => budget.edges = 1,
                "paths" => budget.paths = 1,
                "time" => budget.deadline = std::time::Instant::now(),
                "depth" => budget.depth = 1,
                _ => unreachable!(),
            }
            let result = traverse_impact(&conn, snapshot, &start, &budget, None).unwrap();
            assert!(result.truncated, "{cutoff}");
            assert!(
                result.nodes <= budget.nodes
                    && result.edges <= budget.edges
                    && result.paths.len() <= budget.paths
            );
            assert_eq!(impact_verdict(&result, false), "UNKNOWN", "{cutoff}");
            assert!(result.unresolved.contains(&format!(
                "{}-budget",
                if cutoff == "nodes" {
                    "node"
                } else if cutoff == "edges" {
                    "edge"
                } else if cutoff == "paths" {
                    "path"
                } else {
                    cutoff
                }
            )));
        }
        let mut invalid = TraceBudget::new(3, std::time::Instant::now());
        invalid.nodes = 501;
        assert!(traverse_impact(&conn, snapshot, &start, &invalid, None).is_err());

        // Policy support for a proven reference owner. The current SCIP
        // derivation does not infer callable ownership for plain references.
        conn.execute("UPDATE code_edges SET edge_kind='REFERENCES'", [])
            .unwrap();
        let references = traverse_impact(
            &conn,
            snapshot,
            &start,
            &TraceBudget::new(3, std::time::Instant::now()),
            None,
        )
        .unwrap();
        assert_eq!(impact_verdict(&references, false), "MEDIUM");
        conn.execute("UPDATE code_edges SET precision='Heuristic'", [])
            .unwrap();
        let heuristic = traverse_impact(
            &conn,
            snapshot,
            &start,
            &TraceBudget::new(3, std::time::Instant::now()),
            None,
        )
        .unwrap();
        assert_eq!(impact_verdict(&heuristic, false), "UNKNOWN");
    }

    fn trace_store(pairs: &[(usize, usize)]) -> (rusqlite::Connection, i64, EdgeEndpoint) {
        let conn = mem_db();
        let snapshot = mk_snap(&conn);
        let import = mk_import_full(
            &conn,
            snapshot,
            "scip-import",
            "definitions,references",
            "UTF8",
            "trace-test",
        );
        let node = |n| EdgeEndpoint::Scip {
            import_id: import,
            symbol_uri: format!("node-{n}"),
            doc: None,
        };
        for (i, (a, b)) in pairs.iter().enumerate() {
            store::insert_edge(
                &conn,
                &EdgeRow {
                    snapshot_id: snapshot,
                    file_id: 1,
                    caller: Some(node(a)),
                    caller_path: "graph.py".to_string(),
                    caller_span_bytes: (i * 2, i * 2 + 1),
                    callee: Some(node(b)),
                    callee_name: format!("n{b}"),
                    edge_kind: "CALLS".to_string(),
                    precision: "Exact".to_string(),
                    provider: "scip-import".to_string(),
                    evidence: "{}".to_string(),
                    unresolved: None,
                },
            )
            .unwrap();
        }
        (conn, snapshot, node(&0))
    }

    #[test]
    fn trace_node_edge_path_and_time_budgets_are_explicit() {
        let (conn, snapshot, start) = trace_store(&[(0, 1), (1, 2), (2, 3)]);
        let mut budget = TraceBudget::new(6, std::time::Instant::now());
        budget.nodes = 2;
        let limited = traverse(&conn, snapshot, &start, None, &budget).unwrap();
        assert_eq!(limited.nodes, 2);
        assert!(limited.truncated);
        assert!(limited.unresolved.contains("node-budget"));
        let mut budget = TraceBudget::new(6, std::time::Instant::now());
        budget.edges = 1;
        let limited = traverse(&conn, snapshot, &start, None, &budget).unwrap();
        assert_eq!(limited.edges, 1);
        assert!(limited.unresolved.contains("edge-budget"));
        let (conn, snapshot, start) = trace_store(&[(0, 1), (0, 2), (0, 3)]);
        let mut budget = TraceBudget::new(6, std::time::Instant::now());
        budget.paths = 1;
        let limited = traverse(&conn, snapshot, &start, None, &budget).unwrap();
        assert_eq!(limited.paths.len(), 1);
        assert!(limited.unresolved.contains("path-budget"));
        let mut budget = TraceBudget::new(6, std::time::Instant::now());
        budget.deadline = std::time::Instant::now();
        let limited = traverse(&conn, snapshot, &start, None, &budget).unwrap();
        assert!(limited.paths.is_empty());
        assert_eq!(limited.nodes, 0);
        assert_eq!(limited.edges, 0);
        assert!(limited.truncated);
        assert!(limited.unresolved.contains("time-budget"));
    }

    #[test]
    fn trace_rejects_internal_budget_above_contract() {
        let (conn, snapshot, start) = trace_store(&[]);
        let mut budget = TraceBudget::new(6, std::time::Instant::now());
        budget.nodes = 501;
        assert!(traverse(&conn, snapshot, &start, None, &budget).is_err());
    }

    #[test]
    fn trace_bfs_orders_leaf_and_cycle_paths_at_same_depth() {
        let (conn, snapshot, start) = trace_store(&[(0, 1), (0, 2), (1, 3), (2, 2)]);
        let result = traverse(
            &conn,
            snapshot,
            &start,
            None,
            &TraceBudget::new(3, std::time::Instant::now()),
        )
        .unwrap();
        assert_eq!(result.paths.len(), 2);
        assert!(
            !result.paths[0].cycle,
            "first branch's leaf precedes second branch cycle"
        );
        assert!(result.paths[1].cycle);
        assert_eq!(result.nodes, 4);
    }

    #[test]
    fn trace_retains_completed_target_when_later_branch_exceeds_budget() {
        let (conn, snapshot, start) = trace_store(&[(0, 1), (0, 2), (0, 3)]);
        let mut budget = TraceBudget::new(3, std::time::Instant::now());
        budget.nodes = 3;
        let target = EdgeEndpoint::Scip {
            import_id: endpoint_import(&start).unwrap(),
            symbol_uri: "node-1".to_string(),
            doc: None,
        };
        let result = traverse(&conn, snapshot, &start, Some(&target), &budget).unwrap();
        assert!(result.truncated);
        assert_eq!(
            result.paths.len(),
            1,
            "already-proven target must survive a later frontier cutoff"
        );
        assert_eq!(result.paths[0].edges.len(), 1);
        assert!(!result.unresolved.contains("target-not-proven"));
    }
    #[test]
    fn impact_candidate_pass_skips_rows_the_exact_pass_emitted() {
        let conn = mem_db();
        let snapshot = mk_snap(&conn);
        let def = |path: &str, bs: usize| EdgeEndpoint::SyntaxDef {
            path: path.to_string(),
            byte_start: bs,
            byte_end: bs + 5,
        };
        let row = |caller: EdgeEndpoint, callee: Option<EdgeEndpoint>, kind: &str| EdgeRow {
            snapshot_id: snapshot,
            file_id: 1,
            caller_path: match &caller {
                EdgeEndpoint::SyntaxDef { path, .. } => path.clone(),
                _ => unreachable!(),
            },
            caller: Some(caller),
            caller_span_bytes: (1, 2),
            callee,
            callee_name: "target".to_string(),
            edge_kind: kind.to_string(),
            precision: if kind == "CALLS" {
                "Exact"
            } else {
                "Heuristic"
            }
            .to_string(),
            provider: "syntax".to_string(),
            evidence: "{}".to_string(),
            unresolved: None,
        };
        let start = def("t.py", 0);
        // Exact: a proven caller that the candidate query also selects.
        let exact = row(def("c.py", 20), Some(start.clone()), "CALLS");
        store::insert_edge(&conn, &exact).unwrap();
        // Name-matched: an unresolved callee whose name is the anchor's.
        store::insert_edge(&conn, &row(def("d.py", 40), None, "POTENTIAL_CALL")).unwrap();
        let seeds = |endpoint: &EdgeEndpoint| -> Result<NameSeed, String> {
            Ok(if endpoint == &start {
                NameSeed::Named(CandidateSeed {
                    name: "target".to_string(),
                    paths: vec!["c.py".to_string(), "d.py".to_string()],
                    aliases: Vec::new(),
                })
            } else {
                NameSeed::Missing
            })
        };
        let result = traverse_impact(
            &conn,
            snapshot,
            &start,
            &TraceBudget::new(3, std::time::Instant::now()),
            Some(&seeds),
        )
        .unwrap();
        let bases: Vec<&str> = result.paths.iter().map(|p| p.basis).collect();
        assert_eq!(bases, ["proven", "candidate-name"]);
        assert_eq!(result.paths[0].edges, vec![exact]);
        assert_eq!(result.candidates_returned, 1);
        assert_eq!(result.candidate_nodes, 1);
        assert!(result.candidates_complete);
        assert!(result.unresolved.contains("candidate-name-not-followed"));
    }

    #[test]
    fn a_node_without_a_name_seed_marks_candidates_incomplete() {
        let (conn, snapshot, start) = trace_store(&[(1, 0)]);
        let seeds = |_: &EdgeEndpoint| -> Result<NameSeed, String> { Ok(NameSeed::Missing) };
        let result = traverse_impact(
            &conn,
            snapshot,
            &start,
            &TraceBudget::new(3, std::time::Instant::now()),
            Some(&seeds),
        )
        .unwrap();
        assert!(!result.candidates_complete, "a skipped pass is a gap");
        assert!(result.unresolved.contains("candidate-pass-skipped"));
    }

    /// `n` name-matched rows in d.py, all for a start seeded with the name "n0".
    fn insert_candidate_rows(conn: &rusqlite::Connection, snapshot: i64, n: usize) {
        for i in 0..n {
            store::insert_edge(
                conn,
                &EdgeRow {
                    snapshot_id: snapshot,
                    file_id: 1,
                    caller: Some(EdgeEndpoint::SyntaxDef {
                        path: "d.py".to_string(),
                        byte_start: 10 + i,
                        byte_end: 20 + i,
                    }),
                    caller_path: "d.py".to_string(),
                    caller_span_bytes: (10 + i, 20 + i),
                    callee: None,
                    callee_name: "n0".to_string(),
                    edge_kind: "POTENTIAL_CALL".to_string(),
                    precision: "Heuristic".to_string(),
                    provider: "syntax".to_string(),
                    evidence: "{}".to_string(),
                    unresolved: None,
                },
            )
            .unwrap();
        }
    }

    /// Seeds the start with the name "n0" (d.py rows); every other node is Missing.
    fn start_named_n0<'a>(
        start: &'a EdgeEndpoint,
    ) -> impl Fn(&EdgeEndpoint) -> Result<NameSeed, String> + 'a {
        move |endpoint: &EdgeEndpoint| {
            Ok(if endpoint == start {
                NameSeed::Named(CandidateSeed {
                    name: "n0".to_string(),
                    paths: vec!["d.py".to_string()],
                    aliases: Vec::new(),
                })
            } else {
                NameSeed::Missing
            })
        }
    }

    /// Lengths of the proven paths, in order.
    fn proven_depths(result: &Traversal) -> Vec<usize> {
        result
            .paths
            .iter()
            .filter(|p| p.basis == "proven")
            .map(|p| p.edges.len())
            .collect()
    }

    #[test]
    fn proven_paths_survive_a_candidate_heavy_start() {
        // Proven: node-1 calls node-0 (depth 1), node-2 calls node-1 (depth 2).
        let (conn, snapshot, start) = trace_store(&[(1, 0), (2, 1)]);
        insert_candidate_rows(&conn, snapshot, 5);
        let seeds = start_named_n0(&start);
        let mut budget = TraceBudget::new(3, std::time::Instant::now());
        budget.paths = 2;
        let result = traverse_impact(&conn, snapshot, &start, &budget, Some(&seeds)).unwrap();
        assert_eq!(
            proven_depths(&result),
            vec![1, 2],
            "{:?}",
            result
                .paths
                .iter()
                .map(|p| (p.basis, p.edges.len()))
                .collect::<Vec<_>>()
        );
        // The path budget ends phase 2 early: stated, and the answer is incomplete.
        assert!(
            result.unresolved.contains("path-budget"),
            "{:?}",
            result.unresolved
        );
        assert!(!result.candidates_complete);
    }

    #[test]
    fn phase_two_ends_on_the_edge_budget_and_proven_paths_stay() {
        // Phase 1 reads two rows; the candidate rows at the start need the rest.
        let (conn, snapshot, start) = trace_store(&[(1, 0), (2, 1)]);
        insert_candidate_rows(&conn, snapshot, 5);
        let seeds = start_named_n0(&start);
        let mut budget = TraceBudget::new(3, std::time::Instant::now());
        budget.edges = 3;
        let result = traverse_impact(&conn, snapshot, &start, &budget, Some(&seeds)).unwrap();
        assert_eq!(proven_depths(&result), vec![1, 2]);
        assert!(
            result.unresolved.contains("edge-budget"),
            "{:?}",
            result.unresolved
        );
        assert!(!result.candidates_complete);
    }

    #[test]
    fn a_data_member_anchor_takes_no_name_pass_and_no_gap() {
        // No proven callers; five name-matched rows that a running pass would list.
        let (conn, snapshot, start) = trace_store(&[]);
        insert_candidate_rows(&conn, snapshot, 5);
        let seeds = |_: &EdgeEndpoint| -> Result<NameSeed, String> { Ok(NameSeed::NoName) };
        let result = traverse_impact(
            &conn,
            snapshot,
            &start,
            &TraceBudget::new(3, std::time::Instant::now()),
            Some(&seeds),
        )
        .unwrap();
        assert_eq!(result.candidate_nodes, 0, "no pass runs");
        assert_eq!(result.candidates_returned, 0);
        assert!(result.paths.is_empty());
        assert!(result.candidates_complete, "no name, so no gap");
        assert!(!result.unresolved.contains("candidate-pass-skipped"));
    }

    #[test]
    fn a_boundary_node_without_a_seed_is_a_gap() {
        // depth 1: node-1 is one hop out, at the boundary, with no syntax definition
        // and no further inbound edge (so no depth-budget is stated).
        let (conn, snapshot, start) = trace_store(&[(1, 0)]);
        let seeds = |endpoint: &EdgeEndpoint| -> Result<NameSeed, String> {
            Ok(if endpoint == &start {
                NameSeed::NoName
            } else {
                NameSeed::Missing
            })
        };
        let result = traverse_impact(
            &conn,
            snapshot,
            &start,
            &TraceBudget::new(1, std::time::Instant::now()),
            Some(&seeds),
        )
        .unwrap();
        assert_eq!(proven_depths(&result), vec![1]);
        assert!(!result.candidates_complete);
        assert!(
            result.unresolved.contains("candidate-pass-skipped"),
            "{:?}",
            result.unresolved
        );
    }

    use crate::testutil::tmpdir;

    fn mem_db() -> rusqlite::Connection {
        store::open_memory().unwrap()
    }

    fn mk_snap(conn: &rusqlite::Connection) -> i64 {
        let root = std::env::temp_dir().join(format!("riltra-edge-source-{}", store::new_nonce()));
        std::fs::create_dir_all(&root).unwrap();
        store::insert_snapshot(
            conn,
            &store::Snapshot {
                id: 0,
                repo: "r".to_string(),
                root: root.to_string_lossy().into_owned(),
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
                nonce: store::new_nonce(),
            },
        )
        .unwrap()
    }

    fn span_cols(s: &Option<crate::syntax::CalleeSpan>) -> Option<store::SpanCols> {
        s.as_ref().map(|v| store::SpanCols {
            start_line: v.start_line,
            start_col: v.start_col,
            end_line: v.end_line,
            end_col: v.end_col,
            byte_start: v.byte_start,
            byte_end: v.byte_end,
        })
    }

    /// Ingest parsed source facts exactly like `index::ingest_file`.
    fn ingest_source(
        conn: &rusqlite::Connection,
        snap: i64,
        path: &str,
        src: &[u8],
        lang: crate::syntax::Language,
    ) -> i64 {
        let root: String = conn
            .query_row("SELECT root FROM snapshots WHERE id=?1", [snap], |r| {
                r.get(0)
            })
            .unwrap();
        let source_path = std::path::Path::new(&root).join(path);
        std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        std::fs::write(source_path, src).unwrap();
        let facts = crate::syntax::parse_source(lang, src);
        let fid = store::insert_file(
            conn,
            snap,
            &store::FileRow {
                path: path.to_string(),
                language: crate::syntax::language_name(lang).to_string(),
                bytes: src.len() as u64,
                digest: store::content_digest(src),
                errors: facts.errors,
            },
        )
        .unwrap();
        for d in &facts.definitions {
            store::insert_symbol(
                conn,
                snap,
                &store::SymbolRow {
                    file_id: fid,
                    kind: d.kind.to_string(),
                    name: d.name.clone(),
                    start_line: d.span.start_line,
                    start_col: d.span.start_col,
                    end_line: d.span.end_line,
                    end_col: d.span.end_col,
                    byte_start: d.byte_start,
                    byte_end: d.byte_end,
                    provider: "syntax".to_string(),
                    grammar: crate::syntax::grammar_version(lang).to_string(),
                    language: crate::syntax::language_name(lang).to_string(),
                },
            )
            .unwrap();
        }
        for import in &facts.imports {
            store::insert_import(
                conn,
                snap,
                &store::ImportRow {
                    file_id: fid,
                    text: import.text.clone(),
                    start_line: import.span.start_line,
                    start_col: import.span.start_col,
                    end_line: import.span.end_line,
                    end_col: import.span.end_col,
                    byte_start: import.byte_start,
                    byte_end: import.byte_end,
                    provider: "syntax".to_string(),
                    grammar: crate::syntax::grammar_version(lang).to_string(),
                    language: crate::syntax::language_name(lang).to_string(),
                },
            )
            .unwrap();
        }
        for call in &facts.calls {
            store::insert_call(
                conn,
                snap,
                &store::CallRow {
                    file_id: fid,
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
                    grammar: crate::syntax::grammar_version(lang).to_string(),
                    language: crate::syntax::language_name(lang).to_string(),
                },
            )
            .unwrap();
        }
        fid
    }

    fn ingest_py(conn: &rusqlite::Connection, snap: i64, path: &str, src: &[u8]) -> i64 {
        ingest_source(conn, snap, path, src, crate::syntax::Language::Python)
    }

    #[allow(clippy::too_many_arguments)]
    fn source_fact(
        snap: i64,
        imp: i64,
        path: &str,
        src: &[u8],
        uri: &str,
        display: &str,
        roles: u32,
        bytes: (usize, usize),
    ) -> store::SemanticFact {
        let position = |offset: usize| {
            let row = src[..offset].iter().filter(|b| **b == b'\n').count();
            let line_start = src[..offset]
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |i| i + 1);
            (row + 1, offset - line_start + 1)
        };
        let (start_line, start_col) = position(bytes.0);
        let (end_line, end_col) = position(bytes.1);
        mk_fact(
            snap,
            imp,
            path,
            &store::content_digest(src),
            Some(uri),
            display,
            roles,
            (
                start_line as u32,
                start_col as u32,
                end_line as u32,
                end_col as u32,
            ),
            bytes,
            false,
        )
    }

    fn wiy_alias_facts(
        snap: i64,
        imp: i64,
        path: &str,
        src: &[u8],
        call_local_uri: &str,
    ) -> Vec<store::SemanticFact> {
        let a = "rust-analyzer cargo precision_fixture 0.1.0 a/target().";
        let b = "rust-analyzer cargo precision_fixture 0.1.0 b/target().";
        vec![
            source_fact(snap, imp, path, src, a, "target", 1, (19, 25)),
            source_fact(snap, imp, path, src, b, "target", 1, (63, 69)),
            source_fact(
                snap,
                imp,
                path,
                src,
                "rust-analyzer cargo precision_fixture 0.1.0 direct().",
                "direct",
                1,
                (95, 101),
            ),
            source_fact(snap, imp, path, src, a, "target", 0, (116, 122)),
            source_fact(
                snap,
                imp,
                path,
                src,
                "rust-analyzer cargo precision_fixture 0.1.0 alias().",
                "alias",
                1,
                (135, 140),
            ),
            source_fact(snap, imp, path, src, "local 0", "f", 1, (156, 157)),
            source_fact(snap, imp, path, src, a, "target", 0, (163, 169)),
            source_fact(snap, imp, path, src, call_local_uri, "f", 0, (171, 172)),
            source_fact(
                snap,
                imp,
                path,
                src,
                "rust-analyzer cargo precision_fixture 0.1.0 reference().",
                "reference",
                1,
                (185, 194),
            ),
            source_fact(snap, imp, path, src, "local 1", "_f", 1, (203, 205)),
            source_fact(snap, imp, path, src, a, "target", 0, (211, 217)),
            source_fact(
                snap,
                imp,
                path,
                src,
                "rust-analyzer cargo precision_fixture 0.1.0 other().",
                "other",
                1,
                (229, 234),
            ),
            source_fact(snap, imp, path, src, b, "target", 0, (249, 255)),
        ]
    }

    fn mk_import(conn: &rusqlite::Connection, snap: i64, provider: &str) -> i64 {
        store::insert_semantic_import(
            conn,
            &store::SemanticImport {
                snapshot_id: snap,
                provider_name: provider.to_string(),
                provider_version: "v".to_string(),
                capability: "definitions,references".to_string(),
                project_root: "C:\\r".to_string(),
                text_encoding: "UTF8".to_string(),
                artifact_id: "a".to_string(),
                source_revision: "r".to_string(),
                claimed_source_digest: String::new(),
                content_identity: "ci".to_string(),
                external_symbols: "[]".to_string(),
                verification: "Unverified".to_string(),
            },
        )
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn mk_fact(
        snap: i64,
        imp: i64,
        path: &str,
        digest: &str,
        uri: Option<&str>,
        display: &str,
        roles: u32,
        span: (u32, u32, u32, u32),
        bytes: (usize, usize),
        external: bool,
    ) -> store::SemanticFact {
        store::SemanticFact {
            snapshot_id: snap,
            import_id: imp,
            file_id: None,
            rel_path: path.to_string(),
            file_digest: digest.to_string(),
            symbol_uri: uri.map(|s| s.to_string()),
            display_name: Some(display.to_string()),
            roles,
            start_line: span.0,
            start_col: span.1,
            end_line: span.2,
            end_col: span.3,
            byte_start: bytes.0,
            byte_end: bytes.1,
            precision: "Heuristic".to_string(),
            relationships: "[]".to_string(),
            external,
        }
    }

    #[test]
    fn edges_derive_syntax_only_calls_are_honest_potentials() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src = b"import os\n\n\ndef caller():\n    target()\n    w.process(1)\n";
        ingest_py(&conn, snap, "m.py", src);
        let edges = derive_syntax_edges(&conn, snap).unwrap();
        assert!(
            edges.iter().all(|e| e.edge_kind != "CALLS"),
            "syntax-only derivation never proves CALLS"
        );
        let pots: Vec<&EdgeRow> = edges
            .iter()
            .filter(|e| e.edge_kind == "POTENTIAL_CALL")
            .collect();
        assert_eq!(pots.len(), 2);
        assert!(pots.iter().any(|e| e.callee_name == "target"));
        let member = pots.iter().find(|e| e.callee_name == "process").unwrap();
        assert_eq!(
            member.unresolved.as_deref(),
            Some("dynamic"),
            "receiver dispatch without proof is dynamic, not resolved"
        );
        assert!(edges.iter().any(|e| e.edge_kind == "IMPORTS"));
        assert!(
            edges
                .iter()
                .all(|e| e.provider != "syntax" || e.precision == "Heuristic"),
            "syntax-only edges never carry Exact; Exact is reserved for SCIP-backed relations"
        );
    }

    #[test]
    fn edges_factory_argument_names_the_constructed_type_only() {
        // (stored callee text, constructed type as written, its terminal name)
        let cases: &[(&str, Option<(&str, &str)>)] = &[
            (
                "std::make_unique<PatchMatchCuda>",
                Some(("PatchMatchCuda", "PatchMatchCuda")),
            ),
            (
                "std::make_shared<mvs::PatchMatchController>",
                Some(("mvs::PatchMatchController", "PatchMatchController")),
            ),
            (
                "std::make_unique< PatchMatchCuda >",
                Some(("PatchMatchCuda", "PatchMatchCuda")),
            ),
            ("std::make_unique <Thing>", Some(("Thing", "Thing"))),
            (
                "std::make_unique<std::vector<std::string>>",
                Some(("std::vector<std::string>", "vector")),
            ),
            ("make_shared<Thing>", Some(("Thing", "Thing"))),
            ("std::make_unique<int[]>", None),
            ("std::make_unique<Thing*>", None),
            ("std::make_unique<decltype(x)>", None),
            ("std::make_unique<>", None),
            ("std::make_unique", None),
            ("make_unique_widget<Thing>", None),
            ("make<Thing>", None),
            ("std::vector<int>", None),
        ];
        for &(callee, expected) in cases {
            // The called name is the identifier run at `make_`, else the first identifier.
            let start = callee.find("make_").unwrap_or(0);
            let end = start
                + callee[start..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(callee.len() - start);
            // Offsets are absolute: place the callee at byte 100 of its file.
            let got = factory_argument(callee, 100, (start + 100, end + 100)).map(|a| {
                (
                    &callee[a.span.0 - 100..a.span.1 - 100],
                    &callee[a.terminal.0 - 100..a.terminal.1 - 100],
                )
            });
            assert_eq!(got, expected, "{callee}");
        }
    }

    #[test]
    fn receiver_references_do_not_duplicate_a_resolved_call() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let main = b"def caller():\n    x.target()\n";
        let lib = b"def target():\n    pass\n";
        ingest_py(&conn, snap, "main.py", main);
        ingest_py(&conn, snap, "lib.py", lib);
        let imp = mk_import(&conn, snap, "scip-import");
        store::insert_semantic_facts(
            &conn,
            &[
                mk_fact(
                    snap,
                    imp,
                    "lib.py",
                    &store::content_digest(lib),
                    Some("target"),
                    "target",
                    1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &store::content_digest(main),
                    Some("receiver"),
                    "x",
                    0,
                    (2, 5, 2, 6),
                    (18, 19),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &store::content_digest(main),
                    Some("target"),
                    "target",
                    0,
                    (2, 7, 2, 13),
                    (20, 26),
                    false,
                ),
            ],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!((stats.calls, stats.potential, stats.references), (1, 0, 1));
    }

    #[test]
    fn edges_derive_scip_overlap_proves_calls_strictly() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        // main.py: 0:"from lib import target\n" 1:"\n" 2:"def caller():\n"
        // 3:"    return target()\n" 4:"    lone()\n" 5:"    w.process(9)\n"
        let main = b"from lib import target\n\ndef caller():\n    return target()\n    lone()\n    w.process(9)\n";
        let lib = b"def target():\n    return 1\n";
        let main_digest = store::content_digest(main);
        let lib_digest = store::content_digest(lib);
        ingest_py(&conn, snap, "main.py", main);
        ingest_py(&conn, snap, "lib.py", lib);
        let imp = mk_import(&conn, snap, "scip-import");
        let t = "scip-test test test-pkg 1.0.0 lib.py/target().";
        let c = "scip-test test test-pkg 1.0.0 main.py/caller().";
        let l = "scip-test test test-pkg 1.0.0 main.py/lone().";
        let p = "scip-test test test-pkg 1.0.0 other.py/process().";
        store::insert_semantic_facts(
            &conn,
            &[
                // def target in lib.py (line 1, cols 5..11 1-based).
                mk_fact(
                    snap,
                    imp,
                    "lib.py",
                    &lib_digest,
                    Some(t),
                    "target",
                    0x1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                // def caller in main.py (line 3, cols 5..11).
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &main_digest,
                    Some(c),
                    "caller",
                    0x1,
                    (3, 5, 3, 11),
                    (28, 34),
                    false,
                ),
                // ref target overlapping the target() terminal (line 4,
                // 1-based cols 12..18).
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &main_digest,
                    Some(t),
                    "target",
                    0x0,
                    (4, 12, 4, 18),
                    (49, 55),
                    false,
                ),
                // ref lone with NO definition anywhere: dangling.
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &main_digest,
                    Some(l),
                    "lone",
                    0x0,
                    (5, 5, 5, 9),
                    (62, 66),
                    false,
                ),
                // ref process overlapping only the RECEIVER w (cols 5..6),
                // never the terminal: object-base reference, not a call.
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &main_digest,
                    Some(p),
                    "process",
                    0x0,
                    (6, 5, 6, 6),
                    (73, 74),
                    false,
                ),
            ],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(stats.calls, 1, "exactly one proven call");
        let calls = store::edges_outbound(
            &conn,
            snap,
            &EdgeEndpoint::Scip {
                import_id: imp,
                symbol_uri: c.to_string(),
                doc: None,
            },
            50,
            0,
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].edge_kind, "CALLS");
        assert_eq!(calls[0].precision, "Exact");
        assert_eq!(
            calls[0].callee,
            Some(EdgeEndpoint::Scip {
                import_id: imp,
                symbol_uri: t.to_string(),
                doc: None
            })
        );
        // Dangling ref: POTENTIAL_CALL unresolved, never CALLS.
        let dangling = store::edges_outbound(
            &conn,
            snap,
            &EdgeEndpoint::Scip {
                import_id: imp,
                symbol_uri: l.to_string(),
                doc: None,
            },
            50,
            0,
        )
        .unwrap();
        assert!(dangling.is_empty(), "dangling refs anchor nothing inbound");
        // Receiver-only overlap: target-mismatch, never CALLS. The target
        // stays unknown (no Scip endpoint fabricated), so assert the stored
        // row directly: one POTENTIAL_CALL naming the terminal.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM code_edges WHERE snapshot_id = ?1 AND edge_kind = 'POTENTIAL_CALL' AND callee_name = 'process' AND unresolved = 'target-mismatch' AND callee_kind = 'unknown'",
                [snap],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn rust_local_function_alias_resolves_to_its_scip_target() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src = include_bytes!(
            "../../../docs/pilots/wiy-caller-precision-2026-10-31/source/src/lib.rs"
        );
        let path = "src/lib.rs";
        ingest_source(&conn, snap, path, src, crate::syntax::Language::Rust);
        let imp = mk_import(&conn, snap, "scip-import");
        store::insert_semantic_facts(&conn, &wiy_alias_facts(snap, imp, path, src, "local 0"))
            .unwrap();

        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!((stats.calls, stats.references), (3, 2));

        let edge_rows = |uri: &str| {
            store::edges_inbound(
                &conn,
                snap,
                &EdgeEndpoint::Scip {
                    import_id: imp,
                    symbol_uri: uri.to_string(),
                    doc: None,
                },
                50,
                0,
            )
            .unwrap()
        };
        let a = "rust-analyzer cargo precision_fixture 0.1.0 a/target().";
        let b = "rust-analyzer cargo precision_fixture 0.1.0 b/target().";
        let a_calls: Vec<_> = edge_rows(a)
            .into_iter()
            .filter(|e| e.edge_kind == "CALLS")
            .collect();
        assert_eq!(a_calls.len(), 2, "direct + alias callers of a::target");
        assert!(a_calls.iter().any(|e| e.caller_span_bytes == (113, 124)));
        let alias_edge = a_calls
            .iter()
            .find(|e| e.caller_span_bytes == (171, 174))
            .expect("alias invocation is a CALLS edge");
        assert_eq!(alias_edge.callee_name, "target");
        assert!(alias_edge.evidence.contains("\"ref\":[163,169]"));
        assert!(alias_edge.evidence.contains(
            "\"alias\":{\"binding\":[156,157],\"use\":[171,172],\"initializer\":[163,169]}"
        ));
        assert_eq!(alias_edge.provider, "scip-import");

        let b_calls: Vec<_> = edge_rows(b)
            .into_iter()
            .filter(|e| e.edge_kind == "CALLS")
            .collect();
        assert_eq!(b_calls.len(), 1, "b::target remains a separate identity");
        assert_eq!(b_calls[0].caller_span_bytes, (246, 257));

        let reference_only: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM code_edges WHERE snapshot_id = ?1 AND edge_kind = 'CALLS' AND caller_sbs = 211 AND caller_sbe = 217",
                [snap],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reference_only, 0, "reference-only never becomes CALLS");
    }

    #[test]
    fn rust_alias_does_not_resolve_by_name_when_scip_local_identity_differs() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src = include_bytes!(
            "../../../docs/pilots/wiy-caller-precision-2026-10-31/source/src/lib.rs"
        );
        let path = "src/lib.rs";
        ingest_source(&conn, snap, path, src, crate::syntax::Language::Rust);
        let imp = mk_import(&conn, snap, "scip-import");
        store::insert_semantic_facts(&conn, &wiy_alias_facts(snap, imp, path, src, "local 9"))
            .unwrap();

        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(
            stats.calls, 2,
            "direct calls remain; alias lacks local identity proof"
        );
        let alias_calls: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM code_edges WHERE snapshot_id = ?1 AND edge_kind = 'CALLS' AND caller_sbs = 171 AND caller_sbe = 174",
                [snap],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alias_calls, 0);
        let unresolved_alias: (String, Option<String>) = conn
            .query_row(
                "SELECT edge_kind, unresolved FROM code_edges WHERE snapshot_id = ?1 AND caller_sbs = 171 AND caller_sbe = 174 AND edge_kind = 'POTENTIAL_CALL'",
                [snap],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(unresolved_alias.0, "POTENTIAL_CALL");
        assert_eq!(unresolved_alias.1.as_deref(), Some("unresolved"));
    }

    #[test]
    fn rust_alias_syntax_proof_excludes_mutable_typed_destructured_and_cast_bindings() {
        let src = b"fn caller() { let simple = a::target; simple(); let mut mutable = a::target; mutable(); let typed: fn() = a::target; typed(); let (tuple,) = (a::target,); tuple(); let cast = a::target as fn(); cast(); let _reference = a::target; }";
        let proof = crate::syntax::relationship_proof(crate::syntax::Language::Rust, src);
        let names: Vec<_> = proof
            .local_function_aliases
            .iter()
            .map(|alias| std::str::from_utf8(&src[alias.binding.0..alias.binding.1]).unwrap())
            .collect();
        assert_eq!(names, ["simple", "_reference"]);
    }

    #[test]
    fn rust_closure_call_keeps_owner_unproven() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src = include_bytes!("../../../docs/pilots/riltra-rust-semantic-feasibility-2026-10-31/diagnosis/closure-fixture/src/lib.rs");
        let path = "src/lib.rs";
        ingest_source(&conn, snap, path, src, crate::syntax::Language::Rust);
        let imp = mk_import(&conn, snap, "scip-import");
        let callee = "rust-analyzer cargo mini 0.1.0 callee().";
        store::insert_semantic_facts(
            &conn,
            &[
                source_fact(snap, imp, path, src, callee, "callee", 1, (7, 13)),
                source_fact(
                    snap,
                    imp,
                    path,
                    src,
                    "rust-analyzer cargo mini 0.1.0 direct_caller().",
                    "direct_caller",
                    1,
                    (61, 74),
                ),
                source_fact(snap, imp, path, src, callee, "callee", 0, (104, 110)),
                source_fact(
                    snap,
                    imp,
                    path,
                    src,
                    "rust-analyzer cargo mini 0.1.0 closure_caller().",
                    "closure_caller",
                    1,
                    (124, 138),
                ),
                source_fact(snap, imp, path, src, callee, "callee", 0, (184, 190)),
            ],
        )
        .unwrap();

        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!((stats.calls, stats.potential), (1, 1));
        let closure: (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT edge_kind, unresolved, caller_uri FROM code_edges WHERE snapshot_id = ?1 AND caller_sbs = 184 AND caller_sbe = 193",
                [snap],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(closure.0, "POTENTIAL_CALL");
        assert_eq!(closure.1.as_deref(), Some("owner-unproven"));
        assert_eq!(closure.2, None);
    }

    #[test]
    fn edges_derive_ambiguous_alias_and_owner_rules() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src =
            b"def dup():\n    return 1\n\ndef dup():\n    return 2\n\ndef user():\n    dup()\n";
        ingest_py(&conn, snap, "a.py", src);
        let alias_src = b"from m import real as alias\n\ndef real():\n    return 1\n\ndef user():\n    alias()\n";
        ingest_py(&conn, snap, "b.py", alias_src);
        let class_src = b"class K:\n    x = helper()\n";
        ingest_py(&conn, snap, "c.py", class_src);
        let edges = derive_syntax_edges(&conn, snap).unwrap();
        let by_callee = |name: &str| {
            edges
                .iter()
                .filter(|e| e.callee_name == name && e.edge_kind == "POTENTIAL_CALL")
                .collect::<Vec<_>>()
        };
        let dup = by_callee("dup");
        assert_eq!(dup.len(), 1);
        assert_eq!(dup[0].unresolved.as_deref(), Some("ambiguous:2"));
        let alias = by_callee("alias");
        assert_eq!(alias.len(), 1);
        assert_eq!(alias[0].unresolved.as_deref(), Some("alias"));
        let helper = by_callee("helper");
        assert_eq!(helper.len(), 1);
        assert!(helper[0].caller.is_none(), "class body owns no caller");
        assert_eq!(helper[0].unresolved.as_deref(), Some("owner-unproven"));
    }

    #[test]
    fn edges_derive_recursion_proves_self_call() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src = b"def r():\n    return r()\n";
        let digest = store::content_digest(src);
        ingest_py(&conn, snap, "r.py", src);
        let imp = mk_import(&conn, snap, "scip-import");
        let r = "scip-test test test-pkg 1.0.0 r.py/r().";
        store::insert_semantic_facts(
            &conn,
            &[
                mk_fact(
                    snap,
                    imp,
                    "r.py",
                    &digest,
                    Some(r),
                    "r",
                    0x1,
                    (1, 5, 1, 6),
                    (4, 5),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "r.py",
                    &digest,
                    Some(r),
                    "r",
                    0x0,
                    (2, 12, 2, 13),
                    (20, 21),
                    false,
                ),
            ],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(stats.calls, 1);
        let key = EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: r.to_string(),
            doc: None,
        };
        let out = store::edges_outbound(&conn, snap, &key, 50, 0).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].caller, Some(key.clone()));
        assert_eq!(out[0].callee, Some(key));
    }

    #[test]
    fn edges_derive_chain_links_only_through_unified_identity() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let a = b"def a():\n    return 1\n";
        let b = b"from a import a\n\ndef b():\n    return a()\n";
        let c = b"from b import b\n\ndef c():\n    return b()\n";
        let (da, db, dc) = (
            store::content_digest(a),
            store::content_digest(b),
            store::content_digest(c),
        );
        ingest_py(&conn, snap, "a.py", a);
        ingest_py(&conn, snap, "b.py", b);
        ingest_py(&conn, snap, "c.py", c);
        let imp = mk_import(&conn, snap, "scip-import");
        let (ua, ub, uc) = (
            "scip-test test test-pkg 1.0.0 a.py/a().",
            "scip-test test test-pkg 1.0.0 b.py/b().",
            "scip-test test test-pkg 1.0.0 c.py/c().",
        );
        store::insert_semantic_facts(
            &conn,
            &[
                mk_fact(
                    snap,
                    imp,
                    "a.py",
                    &da,
                    Some(ua),
                    "a",
                    0x1,
                    (1, 5, 1, 6),
                    (4, 5),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "b.py",
                    &db,
                    Some(ub),
                    "b",
                    0x1,
                    (3, 5, 3, 6),
                    (21, 22),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "b.py",
                    &db,
                    Some(ua),
                    "a",
                    0x0,
                    (4, 12, 4, 13),
                    (37, 38),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "c.py",
                    &dc,
                    Some(uc),
                    "c",
                    0x1,
                    (3, 5, 3, 6),
                    (21, 22),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "c.py",
                    &dc,
                    Some(ub),
                    "b",
                    0x0,
                    (4, 12, 4, 13),
                    (37, 38),
                    false,
                ),
            ],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(stats.calls, 2, "c→b and b→a both proven");
        let kb = EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: ub.to_string(),
            doc: None,
        };
        let inbound = store::edges_inbound(&conn, snap, &kb, 50, 0).unwrap();
        let outbound = store::edges_outbound(&conn, snap, &kb, 50, 0).unwrap();
        assert_eq!(inbound.len(), 1, "c calls b");
        assert_eq!(outbound.len(), 1, "b calls a");
        assert_eq!(
            inbound[0].callee, outbound[0].caller,
            "B is the same proven identity on both sides of the chain"
        );
        // Negative: same URI under a DIFFERENT import never merges.
        let imp2 = mk_import(&conn, snap, "scip-import");
        let kb2 = EdgeEndpoint::Scip {
            import_id: imp2,
            symbol_uri: ub.to_string(),
            doc: None,
        };
        assert!(!unify(&kb, &kb2), "import scope is identity");
        assert!(store::edges_inbound(&conn, snap, &kb2, 50, 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn edges_derive_unify_is_structural_equality() {
        let s = |u: &str| EdgeEndpoint::Scip {
            import_id: 7,
            symbol_uri: u.to_string(),
            doc: None,
        };
        assert!(unify(&s("x"), &s("x")));
        assert!(!unify(&s("x"), &s("y")));
        assert!(!unify(
            &s("x"),
            &EdgeEndpoint::Scip {
                import_id: 8,
                symbol_uri: "x".to_string(),
                doc: None
            }
        ));
        let d = |p: &str| EdgeEndpoint::SyntaxDef {
            path: p.to_string(),
            byte_start: 0,
            byte_end: 9,
        };
        assert!(unify(&d("a.py"), &d("a.py")));
        assert!(!unify(&d("a.py"), &d("b.py")));
        assert!(
            !unify(&s("x"), &d("a.py")),
            "cross-kind never unifies by itself"
        );
    }

    #[test]
    fn edges_derive_non_scip_provider_yields_zero_edges() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        ingest_py(&conn, snap, "m.py", b"def f():\n    pass\n");
        let imp = mk_import(&conn, snap, "lsp-subprocess");
        store::insert_semantic_facts(
            &conn,
            &[mk_fact(
                snap,
                imp,
                "m.py",
                "d",
                Some("u"),
                "f",
                0x1,
                (1, 5, 1, 6),
                (4, 5),
                false,
            )],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(
            stats,
            EdgeStats::default(),
            "LSP rows never become stored graph edges"
        );
    }

    fn mk_import_full(
        conn: &rusqlite::Connection,
        snap: i64,
        provider: &str,
        capability: &str,
        encoding: &str,
        artifact: &str,
    ) -> i64 {
        store::insert_semantic_import(
            conn,
            &store::SemanticImport {
                snapshot_id: snap,
                provider_name: provider.to_string(),
                provider_version: "v".to_string(),
                capability: capability.to_string(),
                project_root: "C:\\r".to_string(),
                text_encoding: encoding.to_string(),
                artifact_id: artifact.to_string(),
                source_revision: "r".to_string(),
                claimed_source_digest: String::new(),
                content_identity: "ci".to_string(),
                external_symbols: "[]".to_string(),
                verification: "Unverified".to_string(),
            },
        )
        .unwrap()
    }

    /// Bridging audit: a SyntaxDef owner maps to a Scip identity ONLY when
    /// the definition fact is bound to the indexed file bytes (digest, or
    /// file_id when present) in the same file and import. Same display name
    /// alone — stale digest, foreign file_id, ghost document — never bridges.
    /// The CALLS edge itself still stands (callee proven); only the caller
    /// endpoint falls back to the snapshot-local syntax identity.
    #[test]
    fn edges_bridge_requires_file_bound_evidence() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let main = b"def caller():\n    return target()\n";
        let lib = b"def target():\n    return 1\n";
        let main_digest = store::content_digest(main);
        ingest_py(&conn, snap, "main.py", main);
        ingest_py(&conn, snap, "lib.py", lib);
        let imp = mk_import_full(
            &conn,
            snap,
            "scip-import",
            "definitions,references",
            "UTF8",
            "9f2c3a4b5d6e7f80",
        );
        let t = "scip-test test test-pkg 1.0.0 lib.py/target().";
        let c = "scip-test test test-pkg 1.0.0 main.py/caller().";
        // Caller def fact with a STALE digest: same name, same path,
        // contained occurrence — but not the indexed bytes. Must not bridge.
        let mut stale_caller = mk_fact(
            snap,
            imp,
            "main.py",
            "stale-digest",
            Some(c),
            "caller",
            0x1,
            (1, 5, 1, 11),
            (4, 10),
            false,
        );
        // Foreign file_id with the right digest: also not this file.
        stale_caller.file_id = Some(9999);
        // Ghost document: same display name, nowhere near the owner.
        let ghost = mk_fact(
            snap,
            imp,
            "ghost.py",
            "g",
            Some(c),
            "caller",
            0x1,
            (1, 5, 1, 11),
            (4, 10),
            false,
        );
        store::insert_semantic_facts(
            &conn,
            &[
                mk_fact(
                    snap,
                    imp,
                    "lib.py",
                    &store::content_digest(lib),
                    Some(t),
                    "target",
                    0x1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &main_digest,
                    Some(t),
                    "target",
                    0x0,
                    (2, 12, 2, 18),
                    (25, 31),
                    false,
                ),
                stale_caller,
                ghost,
            ],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(
            stats.calls, 1,
            "callee proof is independent of caller bridging"
        );
        let key = EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: t.to_string(),
            doc: None,
        };
        let inbound = store::edges_inbound(&conn, snap, &key, 50, 0).unwrap();
        assert_eq!(inbound.len(), 1);
        assert!(
            matches!(inbound[0].caller, Some(EdgeEndpoint::SyntaxDef { .. })),
            "unproven caller stays snapshot-local, never a fabricated Scip: {:?}",
            inbound[0].caller
        );
    }

    #[test]
    fn edges_bridge_matching_file_id_does_not_override_changed_digest() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src = b"def caller():\n    return target()\n";
        ingest_py(&conn, snap, "main.py", src);
        let imp = mk_import_full(
            &conn,
            snap,
            "scip-import",
            "definitions,references",
            "UTF8",
            "artifact",
        );
        let owner = store::symbols_for(&conn, snap).unwrap().remove(0);
        let mut fact = mk_fact(
            snap,
            imp,
            "main.py",
            "old-revision-digest",
            Some("caller-uri"),
            "caller",
            1,
            (1, 5, 1, 11),
            (4, 10),
            false,
        );
        fact.file_id = Some(owner.file_id);
        store::insert_semantic_facts(&conn, &[fact]).unwrap();
        assert!(
            owner_scip(
                &conn,
                snap,
                imp,
                &owner,
                "main.py",
                &store::content_digest(src)
            )
            .is_none(),
            "row ID alone must not override contradictory source identity"
        );
    }

    #[test]
    fn edges_evidence_escapes_source_paths() {
        let encoded = evidence_json("a\"\\\n.py", 4, 9, "");
        let value: serde_json::Value =
            serde_json::from_str(&encoded).expect("path must be JSON escaped");
        assert_eq!(value["path"], "a\"\\\n.py");
    }

    #[test]
    fn edges_bridge_ambiguous_semantic_owners_stays_syntax() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let src = b"def caller():\n    pass\n";
        let fid = ingest_py(&conn, snap, "main.py", src);
        let imp = mk_import_full(
            &conn,
            snap,
            "scip-import",
            "definitions,references",
            "UTF8",
            "artifact",
        );
        let owner = store::symbols_for(&conn, snap).unwrap().remove(0);
        for uri in ["owner-a", "owner-b"] {
            let mut fact = mk_fact(
                snap,
                imp,
                "main.py",
                &store::content_digest(src),
                Some(uri),
                "caller",
                1,
                (1, 5, 1, 11),
                (4, 10),
                false,
            );
            fact.file_id = Some(fid);
            store::insert_semantic_facts(&conn, &[fact]).unwrap();
        }
        assert!(
            owner_scip(
                &conn,
                snap,
                imp,
                &owner,
                "main.py",
                &store::content_digest(src)
            )
            .is_none(),
            "ambiguous semantic owners must not pick the first row"
        );
    }

    #[test]
    fn edges_stale_reference_or_target_digest_never_proves_calls() {
        for stale_reference in [true, false] {
            let conn = mem_db();
            let snap = mk_snap(&conn);
            let main = b"def caller():\n    return target()\n";
            let lib = b"def target():\n    return 1\n";
            let fid = ingest_py(&conn, snap, "main.py", main);
            let target_fid = ingest_py(&conn, snap, "lib.py", lib);
            let imp = mk_import_full(
                &conn,
                snap,
                "scip-import",
                "definitions,references",
                "UTF8",
                "artifact",
            );
            let mut reference = mk_fact(
                snap,
                imp,
                "main.py",
                &store::content_digest(main),
                Some("target-uri"),
                "target",
                0,
                (2, 12, 2, 18),
                (25, 31),
                false,
            );
            reference.file_id = Some(fid);
            let mut target = mk_fact(
                snap,
                imp,
                "lib.py",
                &store::content_digest(lib),
                Some("target-uri"),
                "target",
                1,
                (1, 5, 1, 11),
                (4, 10),
                false,
            );
            target.file_id = Some(target_fid);
            if stale_reference {
                reference.file_digest = "old-reference".to_string();
            } else {
                target.file_digest = "old-target".to_string();
            }
            store::insert_semantic_facts(&conn, &[reference, target]).unwrap();
            assert_eq!(
                upsert_semantic_edges(&conn, snap, imp).unwrap().calls,
                0,
                "same row ID cannot prove source alignment; stale_reference={stale_reference}"
            );
        }
    }

    #[test]
    fn edges_two_semantic_targets_at_one_terminal_are_potential_only() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let main = b"def caller():\n    target()\n";
        let lib = b"def target():\n    pass\n";
        ingest_py(&conn, snap, "main.py", main);
        for path in ["a.py", "b.py"] {
            ingest_py(&conn, snap, path, lib);
        }
        let imp = mk_import_full(
            &conn,
            snap,
            "scip-import",
            "definitions,references",
            "UTF8",
            "artifact",
        );
        for (path, uri) in [("a.py", "target-a"), ("b.py", "target-b")] {
            store::insert_semantic_facts(
                &conn,
                &[
                    mk_fact(
                        snap,
                        imp,
                        path,
                        &store::content_digest(lib),
                        Some(uri),
                        "target",
                        1,
                        (1, 5, 1, 11),
                        (4, 10),
                        false,
                    ),
                    mk_fact(
                        snap,
                        imp,
                        "main.py",
                        &store::content_digest(main),
                        Some(uri),
                        "target",
                        0,
                        (2, 5, 2, 11),
                        (18, 24),
                        false,
                    ),
                ],
            )
            .unwrap();
        }
        let result = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(
            result.calls, 0,
            "multiple semantic candidates cannot prove either CALLS edge"
        );
        assert_eq!(
            result.potential, 1,
            "one unresolved callsite, not duplicated per candidate"
        );
    }

    /// Positive control for the bridge: fully-bound owner fact (right file,
    /// right digest, contained occurrence, same import) maps to Scip.
    #[test]
    fn edges_bridge_binds_proven_owner() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let main = b"def caller():\n    return target()\n";
        let lib = b"def target():\n    return 1\n";
        let main_digest = store::content_digest(main);
        ingest_py(&conn, snap, "main.py", main);
        ingest_py(&conn, snap, "lib.py", lib);
        let imp = mk_import_full(
            &conn,
            snap,
            "scip-import",
            "definitions,references",
            "UTF8",
            "9f2c3a4b5d6e7f80",
        );
        let t = "scip-test test test-pkg 1.0.0 lib.py/target().";
        let c = "scip-test test test-pkg 1.0.0 main.py/caller().";
        store::insert_semantic_facts(
            &conn,
            &[
                mk_fact(
                    snap,
                    imp,
                    "lib.py",
                    &store::content_digest(lib),
                    Some(t),
                    "target",
                    0x1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &main_digest,
                    Some(c),
                    "caller",
                    0x1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &main_digest,
                    Some(t),
                    "target",
                    0x0,
                    (2, 12, 2, 18),
                    (25, 31),
                    false,
                ),
            ],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(stats.calls, 1);
        let key = EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: t.to_string(),
            doc: None,
        };
        let inbound = store::edges_inbound(&conn, snap, &key, 50, 0).unwrap();
        assert_eq!(
            inbound[0].caller,
            Some(EdgeEndpoint::Scip {
                import_id: imp,
                symbol_uri: c.to_string(),
                doc: None
            }),
            "fully-bound owner bridges to its Scip identity"
        );
    }

    /// Provenance audit: `provider_name` alone is not the trust boundary. A
    /// forged row with a SCIP-sounding provider but unverified import
    /// metadata (empty artifact, non-SCIP encoding/capability) must yield
    /// zero resolved CALLS. The trust root is the importer lifecycle — the
    /// only writer of verified rows — re-validated here defense-in-depth.
    #[test]
    fn edges_forged_import_metadata_yields_zero_edges() {
        let conn = mem_db();
        let snap = mk_snap(&conn);
        let main = b"def caller():\n    return target()\n";
        let lib = b"def target():\n    return 1\n";
        ingest_py(&conn, snap, "main.py", main);
        ingest_py(&conn, snap, "lib.py", lib);
        let imp = mk_import_full(&conn, snap, "scip-import", "definitions", "Unspecified", "");
        let t = "scip-test test test-pkg 1.0.0 lib.py/target().";
        store::insert_semantic_facts(
            &conn,
            &[
                mk_fact(
                    snap,
                    imp,
                    "lib.py",
                    &store::content_digest(lib),
                    Some(t),
                    "target",
                    0x1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                mk_fact(
                    snap,
                    imp,
                    "main.py",
                    &store::content_digest(main),
                    Some(t),
                    "target",
                    0x0,
                    (2, 12, 2, 18),
                    (25, 31),
                    false,
                ),
            ],
        )
        .unwrap();
        let stats = upsert_semantic_edges(&conn, snap, imp).unwrap();
        assert_eq!(
            stats,
            EdgeStats::default(),
            "unverified import metadata never resolves CALLS, whatever the provider claims"
        );
        let key = EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: t.to_string(),
            doc: None,
        };
        assert!(store::edges_inbound(&conn, snap, &key, 50, 0)
            .unwrap()
            .is_empty());
    }

    // --- lifecycle tests (real build + real SCIP bytes, no reindex) ---

    /// Minimal proto3 writer for exactly the fields the importer reads.
    mod scip_enc {
        pub fn varint(mut v: u64, out: &mut Vec<u8>) {
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
        }
        pub fn tag(field: u32, wire: u32, out: &mut Vec<u8>) {
            varint(((field << 3) | wire) as u64, out);
        }
        pub fn len_field(field: u32, bytes: &[u8], out: &mut Vec<u8>) {
            tag(field, 2, out);
            varint(bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        pub fn str_field(field: u32, value: &str, out: &mut Vec<u8>) {
            len_field(field, value.as_bytes(), out);
        }
        pub fn varint_field(field: u32, value: u64, out: &mut Vec<u8>) {
            tag(field, 0, out);
            varint(value, out);
        }
        pub fn occurrence(symbol: &str, roles: u64, line: i64, start: i64, end: i64) -> Vec<u8> {
            let mut r = Vec::new();
            varint_field(1, line as u64, &mut r);
            varint_field(2, start as u64, &mut r);
            varint_field(3, end as u64, &mut r);
            let mut o = Vec::new();
            str_field(2, symbol, &mut o);
            varint_field(3, roles, &mut o);
            len_field(8, &r, &mut o);
            o
        }
        pub fn symbol_info(symbol: &str, display: &str) -> Vec<u8> {
            let mut s = Vec::new();
            str_field(1, symbol, &mut s);
            str_field(6, display, &mut s);
            s
        }
        pub fn document(rel: &str, occs: &[Vec<u8>], symbols: &[Vec<u8>]) -> Vec<u8> {
            let mut d = Vec::new();
            str_field(1, rel, &mut d);
            for o in occs {
                len_field(2, o, &mut d);
            }
            for s in symbols {
                len_field(3, s, &mut d);
            }
            varint_field(6, 1, &mut d); // UTF8
            d
        }
        pub fn index_bytes(root: &std::path::Path, docs: &[Vec<u8>]) -> Vec<u8> {
            let mut md = Vec::new();
            let mut tool = Vec::new();
            str_field(1, "scip-test", &mut tool);
            str_field(2, "1.0", &mut tool);
            len_field(2, &tool, &mut md);
            str_field(
                3,
                &format!("file://{}", root.to_string_lossy().replace('\\', "/")),
                &mut md,
            );
            varint_field(4, 1, &mut md);
            let mut out = Vec::new();
            len_field(1, &md, &mut out);
            for d in docs {
                len_field(2, d, &mut out);
            }
            out
        }
    }

    const T_URI: &str = "scip-test test test-pkg 1.0.0 lib.py/target().";
    const C_URI: &str = "scip-test test test-pkg 1.0.0 main.py/caller().";

    fn lifecycle_repo(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let home = tmpdir(&format!("edges-life-home-{tag}"));
        let repo = tmpdir(&format!("edges-life-{tag}"));
        std::fs::write(repo.join("lib.py"), b"def target():\n    return 1\n").unwrap();
        std::fs::write(
            repo.join("main.py"),
            b"from lib import target\n\ndef caller():\n    return target()\n",
        )
        .unwrap();
        (home, repo)
    }

    fn lifecycle_scip(repo: &std::path::Path) -> std::path::PathBuf {
        let docs = vec![
            scip_enc::document(
                "lib.py",
                &[scip_enc::occurrence(T_URI, 0x1, 0, 4, 10)],
                &[scip_enc::symbol_info(T_URI, "target")],
            ),
            scip_enc::document(
                "main.py",
                &[
                    scip_enc::occurrence(C_URI, 0x1, 2, 4, 10),
                    scip_enc::occurrence(T_URI, 0x0, 3, 11, 17),
                ],
                &[
                    scip_enc::symbol_info(C_URI, "caller"),
                    scip_enc::symbol_info(T_URI, "target"),
                ],
            ),
        ];
        let scip = repo.join("index.scip");
        std::fs::write(&scip, scip_enc::index_bytes(repo, &docs)).unwrap();
        scip
    }

    fn live_snapshot_id(home: &std::path::Path) -> i64 {
        let db = store::db_path(home, "r");
        let conn = store::open_live(&db).unwrap();
        store::latest_snapshot(&conn).unwrap().unwrap().id
    }

    fn calls_edges(home: &std::path::Path) -> Vec<EdgeRow> {
        // Read back through the public query surface (no raw SQL).
        let db = store::db_path(home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap().id;
        let imports = store::semantic_imports_for(&conn, snap).unwrap();
        // Newest import: rebuilds carry older imports forward, so the fully
        // bound import is the last one.
        let imp = imports.last().map(|(id, _)| *id).unwrap();
        let key = EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: C_URI.to_string(),
            doc: None,
        };
        store::edges_outbound(&conn, snap, &key, 50, 0).unwrap()
    }

    #[test]
    fn edges_lifecycle_import_upsert_visible_without_reindex() {
        let (home, repo) = lifecycle_repo("noreindex");
        crate::index::build_full(&home, "r", &repo).unwrap();
        let scip = lifecycle_scip(&repo);
        let out = crate::index::semantic_import(&home, "r", &repo, &scip, None).unwrap();
        assert_eq!(out.documents, 2);
        // No rebuild happened: the import transaction itself stored CALLS.
        let edges = calls_edges(&home);
        assert_eq!(edges.len(), 1, "CALLS visible without reindex");
        assert_eq!(edges[0].edge_kind, "CALLS");
        assert_eq!(edges[0].precision, "Exact");
        // Exact is evidence strength, not freshness: the import stays
        // Unverified (never Current) while the edge reads Exact.
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let row = store::semantic_import_by_id(&conn, out.import_id)
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(row.verification, "Unverified");
        assert_ne!(
            row.verification, "Current",
            "Exact edges never imply Current"
        );
    }

    #[test]
    fn edges_lifecycle_import_failure_rolls_back_edges() {
        let (home, repo) = lifecycle_repo("rollback");
        crate::index::build_full(&home, "r", &repo).unwrap();
        let bad = repo.join("bad.scip");
        std::fs::write(&bad, b"not a scip payload").unwrap();
        assert!(crate::index::semantic_import(&home, "r", &repo, &bad, None).is_err());
        let db = store::db_path(&home, "r");
        let conn = store::open_live(&db).unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap().id;
        assert!(store::semantic_imports_for(&conn, snap).unwrap().is_empty());
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM code_edges WHERE provider = 'scip-import'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "failed import leaves no semantic edges behind");
    }

    #[test]
    fn edges_lifecycle_rebuild_rederives_after_target_change() {
        let (home, repo) = lifecycle_repo("targetchange");
        crate::index::build_full(&home, "r", &repo).unwrap();
        let scip = lifecycle_scip(&repo);
        crate::index::semantic_import(&home, "r", &repo, &scip, None).unwrap();
        assert_eq!(calls_edges(&home).len(), 1);
        // Change ONLY the target file; the caller file is untouched.
        std::fs::write(repo.join("lib.py"), b"def target():\n    return 2\n").unwrap();
        let second = crate::index::build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.mode, "incremental");
        // The semantic import must be re-applied to the new snapshot before
        // edges exist there (fresh import binds the new revision).
        let scip2 = lifecycle_scip(&repo);
        crate::index::semantic_import(&home, "r", &repo, &scip2, None).unwrap();
        let edges = calls_edges(&home);
        assert_eq!(edges.len(), 1, "edge re-derived under the new snapshot");
        assert_eq!(edges[0].snapshot_id, live_snapshot_id(&home));
    }

    #[test]
    fn edges_lifecycle_incremental_noop_keeps_edges() {
        let (home, repo) = lifecycle_repo("noop");
        crate::index::build_full(&home, "r", &repo).unwrap();
        let scip = lifecycle_scip(&repo);
        crate::index::semantic_import(&home, "r", &repo, &scip, None).unwrap();
        let second = crate::index::build(&home, "r", &repo, false).unwrap();
        assert_eq!(second.mode, "incremental");
        // Stale semantic facts do not carry (source unchanged → they do),
        // and the re-derivation restores the same CALLS edge.
        let edges = calls_edges(&home);
        assert_eq!(edges.len(), 1, "noop rebuild keeps the proven edge");
    }

    #[test]
    fn edges_lifecycle_promote_race_aborts_with_edges_intact() {
        // Store-level writer race: staging based on an old generation must
        // abort with the committed live edges intact.
        let dir = tmpdir("edges-race");
        let live_path = dir.join("live.db");
        let live = store::open_live(&live_path).unwrap();
        let s1 = mk_snap_on(&live);
        let imp = mk_import(&live, s1, "scip-import");
        store::insert_edge(
            &live,
            &EdgeRow {
                snapshot_id: s1,
                file_id: 1,
                caller: None,
                caller_path: "m.py".to_string(),
                caller_span_bytes: (0, 9),
                callee: Some(EdgeEndpoint::Scip {
                    import_id: imp,
                    symbol_uri: "u".to_string(),
                    doc: None,
                }),
                callee_name: "u".to_string(),
                edge_kind: "REFERENCES".to_string(),
                precision: "Exact".to_string(),
                provider: "scip-import".to_string(),
                evidence: "{}".to_string(),
                unresolved: None,
            },
        )
        .unwrap();
        let base = store::live_base(&live).unwrap();
        let staging_path = dir.join("s.db");
        {
            let staging = store::open_live(&staging_path).unwrap();
            store::insert_snapshot(
                &staging,
                &store::Snapshot {
                    id: 0,
                    repo: "r".to_string(),
                    root: "C:\\r".to_string(),
                    created_unix: 2,
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
                    nonce: store::new_nonce(),
                },
            )
            .unwrap();
        }
        store::meta_set(&live, "semantic_generation", "1").unwrap();
        let err = store::promote_staging(&live, &staging_path, &base).unwrap_err();
        assert!(err.contains("semantic-changed"), "unexpected: {err}");
        let key = EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: "u".to_string(),
            doc: None,
        };
        let kept = store::edges_inbound(&live, s1, &key, 50, 0).unwrap();
        assert_eq!(kept.len(), 1, "live edges survive the aborted race");
    }

    fn mk_snap_on(conn: &rusqlite::Connection) -> i64 {
        mk_snap(conn)
    }
}
