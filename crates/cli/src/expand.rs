// SPDX-License-Identifier: Apache-2.0
//! Query expansion (PR 9c): one hop along proven edges from the lexical
//! results, as plain data. `expand` owns the status precedence, the seed
//! mapping, the shared edge budget, the destination mapping and the caps. The
//! JSON formatter is in `response.rs`; `lexical::ready_document` runs it on the
//! query's own read transaction.

use crate::edges;
use crate::lexical::Hit;
use crate::store::{self, EdgeEndpoint, EdgeRow, Snapshot, SymbolRow};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

/// The caps of one expansion. `DEFAULT` holds the contract's numbers; tests
/// inject smaller ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub seeds: usize,
    pub per_seed: usize,
    pub total: usize,
    pub edge_read: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Limits = Limits {
        seeds: 5,
        per_seed: 5,
        total: 25,
        edge_read: 200,
    };
}

/// The five statuses, in the precedence order of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    NoneImported,
    NoSeed,
    Stale,
    NoEligibleEdge,
    Available,
}

impl Status {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Status::NoneImported => "none-imported",
            Status::NoSeed => "no-seed",
            Status::Stale => "stale",
            Status::NoEligibleEdge => "no-eligible-edge",
            Status::Available => "available",
        }
    }
}

/// Why a considered seed was not mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    NotCallable,
    UnknownSymbol,
}

impl Skip {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Skip::NotCallable => "not-callable",
            Skip::UnknownSymbol => "unknown-symbol",
        }
    }
}

/// Which way an added row is to its seed: `Callee` means the seed calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Callee,
    Caller,
}

impl Direction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Direction::Callee => "callee",
            Direction::Caller => "caller",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expansion {
    pub status: Status,
    /// Sorted, distinct named reasons.
    pub reasons: Vec<&'static str>,
    pub limits: Limits,
    pub seeds: SeedsReport,
    pub added: usize,
    pub omitted: usize,
    pub truncated: bool,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeedsReport {
    pub considered: usize,
    pub mapped: usize,
    pub skipped: usize,
    pub items: Vec<SeedItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeedItem {
    pub path: String,
    pub line: u32,
    pub name: Option<String>,
    pub kind: String,
    pub skipped: Option<Skip>,
    pub scip: bool,
    pub eligible: usize,
    pub added: usize,
    pub omitted: usize,
    pub unmapped: usize,
    pub read_bounded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub path: String,
    pub line: u32,
    pub name: Option<String>,
    pub kind: String,
    pub via: Via,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Via {
    pub seed: SeedRef,
    pub edge: &'static str,
    pub direction: Direction,
    pub precision: &'static str,
    pub evidence_label: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeedRef {
    pub path: String,
    pub line: u32,
    pub name: Option<String>,
}

const REASON_SOURCE: &str = "source-not-fresh";
const REASON_GRAPH: &str = "graph-proof-incompatible";
const REASON_IMPORT: &str = "import-stale";
const REASON_EMPTY: &str = "empty-result-not-coverage-proof";

/// Dedup identity of a definition: (path, kind, byte_start).
type Key = (String, String, usize);

/// One hop along proven edges from the first `limits.seeds` results, in the
/// contract's precedence (see the module doc). Every read error is returned as
/// `Err`; a status is never a failed read.
pub(crate) fn expand(
    tx: &Connection,
    snapshot: &Snapshot,
    source_fresh: bool,
    hits: &[Hit],
    limits: &Limits,
    verdict: &mut dyn FnMut(i64) -> Result<bool, String>,
) -> Result<Expansion, String> {
    let considered = hits.len().min(limits.seeds);
    let taken = &hits[..considered];
    let valid = valid_imports(tx, snapshot.id)?;
    if valid.is_empty() {
        return Ok(finish(
            Status::NoneImported,
            Vec::new(),
            limits,
            seeds_report(considered, Vec::new()),
            Vec::new(),
        ));
    }
    if taken.is_empty() {
        return Ok(finish(
            Status::NoSeed,
            Vec::new(),
            limits,
            seeds_report(considered, Vec::new()),
            Vec::new(),
        ));
    }
    if !source_fresh {
        return Ok(finish(
            Status::Stale,
            vec![REASON_SOURCE],
            limits,
            seeds_report(considered, Vec::new()),
            Vec::new(),
        ));
    }
    if !store::graph_proof_current(tx, snapshot.id)? {
        return Ok(finish(
            Status::Stale,
            vec![REASON_GRAPH],
            limits,
            seeds_report(considered, Vec::new()),
            Vec::new(),
        ));
    }

    // Step 5: map every considered seed, in returned order.
    let mut items: Vec<SeedItem> = Vec::with_capacity(considered);
    let mut mapped: Vec<(usize, Mapped)> = Vec::new();
    for (index, hit) in taken.iter().enumerate() {
        match map_seed(tx, snapshot.id, &valid, hit)? {
            Mapping::Skipped(skip) => items.push(SeedItem {
                skipped: Some(skip),
                ..seed_item(hit)
            }),
            Mapping::Mapped(found) => {
                items.push(SeedItem {
                    scip: !found.scip.is_empty(),
                    ..seed_item(hit)
                });
                mapped.push((index, *found));
            }
        }
    }
    if mapped.is_empty() {
        return Ok(finish(
            Status::NoSeed,
            Vec::new(),
            limits,
            seeds_report(considered, items),
            Vec::new(),
        ));
    }

    // Step 6: the imports under the seeds' SCIP identities, before any edge read.
    let mut verdicts = Verdicts {
        ask: verdict,
        memo: BTreeMap::new(),
    };
    let scip_imports: BTreeSet<i64> = mapped
        .iter()
        .flat_map(|(_, found)| found.scip.iter())
        .filter_map(scip_import)
        .collect();
    for import_id in scip_imports {
        if verdicts.stale(import_id)? {
            return Ok(stale_with(REASON_IMPORT, limits, considered, items));
        }
    }

    // Step 7: read each identity under its direction's shared budget, then
    // check the imports that the followed edges name.
    let mut usable: BTreeMap<i64, bool> = BTreeMap::new();
    let mut followed_per_seed: Vec<Vec<Followed>> = Vec::with_capacity(mapped.len());
    for (index, found) in &mapped {
        let mut outbound = vec![found.syntax.clone()];
        outbound.extend(found.scip.iter().cloned());
        let (out_edges, out_unknown, out_bounded) =
            read_direction(tx, snapshot.id, &outbound, false, limits.edge_read)?;
        let (in_edges, in_unknown, in_bounded) =
            read_direction(tx, snapshot.id, &found.scip, true, limits.edge_read)?;
        let mut followed = Vec::new();
        let mut unmapped = out_unknown + in_unknown;
        for (inbound, read) in [(false, out_edges), (true, in_edges)] {
            for edge in read {
                match edge_imports(tx, snapshot.id, &mut usable, &edge)? {
                    Some(imports) => followed.push(Followed {
                        edge,
                        inbound,
                        imports,
                    }),
                    None => unmapped += 1,
                }
            }
        }
        let item = &mut items[*index];
        item.unmapped = unmapped;
        item.read_bounded = out_bounded || in_bounded;
        followed_per_seed.push(followed);
    }
    for followed in &followed_per_seed {
        for read in followed {
            for import_id in &read.imports {
                if verdicts.stale(*import_id)? {
                    return Ok(stale_with(REASON_IMPORT, limits, considered, items));
                }
            }
        }
    }

    // Step 8: map the other endpoints, dedup, cap.
    let mut excluded: BTreeSet<Key> = hits
        .iter()
        .map(|hit| (hit.path.clone(), hit.kind.clone(), hit.byte_start))
        .collect();
    let mut rows: Vec<Row> = Vec::new();
    for ((index, found), followed) in mapped.iter().zip(followed_per_seed) {
        let hit = &taken[*index];
        let seed_key: Key = (
            hit.path.clone(),
            found.owner.kind.clone(),
            found.owner.byte_start,
        );
        let mut candidates: BTreeMap<Key, Found> = BTreeMap::new();
        let mut unmapped = 0;
        for read in followed {
            let other = if read.inbound {
                read.edge.caller.as_ref()
            } else {
                read.edge.callee.as_ref()
            };
            let Some(other) = other else {
                unmapped += 1;
                continue;
            };
            let Some(target) = destination(tx, snapshot.id, other)? else {
                unmapped += 1;
                continue;
            };
            let edge = edge_name(&read.edge.edge_kind)?;
            let direction = if read.inbound {
                Direction::Caller
            } else {
                Direction::Callee
            };
            let rank = (edge_rank(edge), direction_rank(direction));
            let key: Key = (
                target.path.clone(),
                target.row.kind.clone(),
                target.row.byte_start,
            );
            let row = Row {
                path: target.path,
                line: target.row.start_line,
                name: target.row.name,
                kind: target.row.kind,
                via: Via {
                    seed: SeedRef {
                        path: hit.path.clone(),
                        line: hit.line,
                        name: hit.name.clone(),
                    },
                    edge,
                    direction,
                    precision: "Exact",
                    evidence_label: "Unverified",
                },
            };
            // Keep the smallest (edge, direction) rank; on a tie the first read
            // (store order) stays.
            let keep = candidates.get(&key).is_none_or(|known| rank < known.rank);
            if keep {
                candidates.insert(key.clone(), Found { key, rank, row });
            }
        }
        let mut fresh: Vec<Found> = candidates
            .into_values()
            .filter(|found| !excluded.contains(&found.key) && found.key != seed_key)
            .collect();
        fresh.sort_by(|a, b| {
            (a.rank, &a.key.0, a.key.2, &a.key.1).cmp(&(b.rank, &b.key.0, b.key.2, &b.key.1))
        });
        let eligible = fresh.len();
        let room = limits.per_seed.min(limits.total.saturating_sub(rows.len()));
        let added = eligible.min(room);
        for found in fresh.into_iter().take(added) {
            excluded.insert(found.key.clone());
            rows.push(found.row);
        }
        let item = &mut items[*index];
        item.eligible = eligible;
        item.added = added;
        item.omitted = eligible - added;
        item.unmapped += unmapped;
    }

    let (status, reasons) = if rows.is_empty() {
        (Status::NoEligibleEdge, vec![REASON_EMPTY])
    } else {
        (Status::Available, Vec::new())
    };
    Ok(finish(
        status,
        reasons,
        limits,
        seeds_report(considered, items),
        rows,
    ))
}

/// The expansion of a document with no index: none imported, no seeds, the
/// default limits. Used by the `no-index` document under `--expand`.
pub(crate) fn none_imported() -> Expansion {
    finish(
        Status::NoneImported,
        Vec::new(),
        &Limits::DEFAULT,
        seeds_report(0, Vec::new()),
        Vec::new(),
    )
}

/// A considered seed after step 5: its own definition and its identities.
struct Mapped {
    owner: SymbolRow,
    syntax: EdgeEndpoint,
    /// One SCIP identity per valid import in which the owner maps; imports are
    /// visited in id order, so the list is in identity order.
    scip: Vec<EdgeEndpoint>,
}

enum Mapping {
    Skipped(Skip),
    Mapped(Box<Mapped>),
}

/// The definition an edge's other endpoint resolves to.
struct Destination {
    path: String,
    row: SymbolRow,
}

/// A proven edge that passed its provenance check.
struct Followed {
    edge: EdgeRow,
    inbound: bool,
    imports: BTreeSet<i64>,
}

/// A candidate row: its dedup key and its `via` rank (edge, direction).
struct Found {
    key: Key,
    rank: (u8, u8),
    row: Row,
}

/// Each import's staleness verdict is asked at most once per expansion.
struct Verdicts<'a> {
    ask: &'a mut dyn FnMut(i64) -> Result<bool, String>,
    memo: BTreeMap<i64, bool>,
}

impl Verdicts<'_> {
    fn stale(&mut self, import_id: i64) -> Result<bool, String> {
        if let Some(known) = self.memo.get(&import_id) {
            return Ok(*known);
        }
        let stale = (*self.ask)(import_id)?;
        self.memo.insert(import_id, stale);
        Ok(stale)
    }
}

fn seed_item(hit: &Hit) -> SeedItem {
    SeedItem {
        path: hit.path.clone(),
        line: hit.line,
        name: hit.name.clone(),
        kind: hit.kind.clone(),
        skipped: None,
        scip: false,
        eligible: 0,
        added: 0,
        omitted: 0,
        unmapped: 0,
        read_bounded: false,
    }
}

/// Counts come from the items, so a report whose mapping never ran is all zero.
fn seeds_report(considered: usize, items: Vec<SeedItem>) -> SeedsReport {
    SeedsReport {
        considered,
        mapped: items.iter().filter(|item| item.skipped.is_none()).count(),
        skipped: items.iter().filter(|item| item.skipped.is_some()).count(),
        items,
    }
}

fn finish(
    status: Status,
    reasons: Vec<&'static str>,
    limits: &Limits,
    seeds: SeedsReport,
    rows: Vec<Row>,
) -> Expansion {
    let mut reasons = reasons;
    reasons.sort_unstable();
    reasons.dedup();
    let omitted = seeds.items.iter().map(|item| item.omitted).sum();
    let truncated = omitted > 0 || seeds.items.iter().any(|item| item.read_bounded);
    Expansion {
        status,
        reasons,
        limits: *limits,
        seeds,
        added: rows.len(),
        omitted,
        truncated,
        rows,
    }
}

fn stale_with(
    reason: &'static str,
    limits: &Limits,
    considered: usize,
    items: Vec<SeedItem>,
) -> Expansion {
    finish(
        Status::Stale,
        vec![reason],
        limits,
        seeds_report(considered, items),
        Vec::new(),
    )
}

/// Imports of the snapshot that pass `import_provenance_ok`, in id order.
fn valid_imports(tx: &Connection, snapshot_id: i64) -> Result<Vec<i64>, String> {
    Ok(store::semantic_imports_for(tx, snapshot_id)?
        .into_iter()
        .filter(|(_, import)| edges::import_provenance_ok(import))
        .map(|(id, _)| id)
        .collect())
}

fn scip_import(endpoint: &EdgeEndpoint) -> Option<i64> {
    match endpoint {
        EdgeEndpoint::Scip { import_id, .. } => Some(*import_id),
        EdgeEndpoint::SyntaxDef { .. } => None,
    }
}

/// Step 5 for one seed: a callable definition row matched by path, kind, name
/// and span; then its SCIP identities per valid import. Never a name match.
fn map_seed(
    tx: &Connection,
    snapshot_id: i64,
    valid: &[i64],
    hit: &Hit,
) -> Result<Mapping, String> {
    if hit.kind != "function" && hit.kind != "method" {
        return Ok(Mapping::Skipped(Skip::NotCallable));
    }
    let symbols = store::symbols_at(tx, snapshot_id, &hit.path)?;
    let matches: Vec<&SymbolRow> = symbols
        .iter()
        .filter(|s| s.kind == hit.kind && s.name == hit.name && s.byte_start == hit.byte_start)
        .collect();
    let [owner] = matches.as_slice() else {
        return Ok(Mapping::Skipped(Skip::UnknownSymbol));
    };
    let owner: SymbolRow = (*owner).clone();
    let Some((file_id, digest)) = store::file_identity(tx, snapshot_id, &hit.path)? else {
        return Ok(Mapping::Skipped(Skip::UnknownSymbol));
    };
    if owner.file_id != file_id {
        return Ok(Mapping::Skipped(Skip::UnknownSymbol));
    }
    let mut scip = Vec::new();
    for &import_id in valid {
        if let Some(endpoint) =
            edges::owner_scip_checked(tx, snapshot_id, import_id, &owner, &hit.path, &digest)?
        {
            scip.push(endpoint);
        }
    }
    let syntax = EdgeEndpoint::SyntaxDef {
        path: hit.path.clone(),
        byte_start: owner.byte_start,
        byte_end: owner.byte_end,
    };
    Ok(Mapping::Mapped(Box::new(Mapped {
        owner,
        syntax,
        scip,
    })))
}

/// The imports one edge names, or `None` when its evidence is invalid: an
/// `import` value that is not a positive integer, or no import at all. An
/// unparseable evidence document is a stored-data error.
fn named_imports(edge: &EdgeRow) -> Result<Option<BTreeSet<i64>>, String> {
    let evidence: serde_json::Value = serde_json::from_str(&edge.evidence)
        .map_err(|e| format!("invalid stored edge evidence: {e}"))?;
    let mut named = BTreeSet::new();
    for endpoint in [&edge.caller, &edge.callee].into_iter().flatten() {
        if let Some(import_id) = scip_import(endpoint) {
            named.insert(import_id);
        }
    }
    if let Some(value) = evidence.get("import") {
        let Some(import_id) = value.as_i64().filter(|id| *id > 0) else {
            return Ok(None);
        };
        named.insert(import_id);
    }
    Ok((!named.is_empty()).then_some(named))
}

/// Whether an import exists on this snapshot and passes provenance. A missing
/// import is `false`, never an error: the edge is then counted as unmapped.
fn import_usable(
    tx: &Connection,
    snapshot_id: i64,
    usable: &mut BTreeMap<i64, bool>,
    import_id: i64,
) -> Result<bool, String> {
    if let Some(known) = usable.get(&import_id) {
        return Ok(*known);
    }
    let ok = match store::semantic_import_by_id(tx, import_id)? {
        Some((owner, import)) => owner == snapshot_id && edges::import_provenance_ok(&import),
        None => false,
    };
    usable.insert(import_id, ok);
    Ok(ok)
}

/// `Some(imports)` when the edge's provenance holds (all named imports usable),
/// `None` when it does not and the edge is not followed.
fn edge_imports(
    tx: &Connection,
    snapshot_id: i64,
    usable: &mut BTreeMap<i64, bool>,
    edge: &EdgeRow,
) -> Result<Option<BTreeSet<i64>>, String> {
    let Some(named) = named_imports(edge)? else {
        return Ok(None);
    };
    let mut all = true;
    for import_id in &named {
        if !import_usable(tx, snapshot_id, usable, *import_id)? {
            all = false;
        }
    }
    Ok(all.then_some(named))
}

/// One direction of one seed under one shared budget (R4). Identities are read
/// in order, each asking for `remaining + 1` rows: the extra row is the
/// sentinel that marks the read bounded and stops further reads.
fn read_direction(
    tx: &Connection,
    snapshot_id: i64,
    identities: &[EdgeEndpoint],
    inbound: bool,
    budget: usize,
) -> Result<(Vec<EdgeRow>, usize, bool), String> {
    let mut kept: Vec<EdgeRow> = Vec::new();
    let mut unknown = 0;
    for identity in identities {
        let remaining = budget - kept.len();
        let (mut page, others) =
            store::proven_edge_page(tx, snapshot_id, identity, inbound, (remaining + 1) as i64)?;
        unknown += others;
        if page.len() > remaining {
            page.truncate(remaining);
            kept.extend(page);
            return Ok((kept, unknown, true));
        }
        kept.extend(page);
    }
    Ok((kept, unknown, false))
}

/// The one callable definition an endpoint names: a syntax identity as is, or
/// the single syntax alias of a SCIP endpoint. Zero or several is `None`.
fn destination(
    tx: &Connection,
    snapshot_id: i64,
    endpoint: &EdgeEndpoint,
) -> Result<Option<Destination>, String> {
    let (path, byte_start, byte_end) = match endpoint {
        EdgeEndpoint::SyntaxDef {
            path,
            byte_start,
            byte_end,
        } => (path.clone(), *byte_start, *byte_end),
        EdgeEndpoint::Scip { .. } => {
            let bridges = edges::owner_bridges_strict(tx, snapshot_id, endpoint)?;
            let [bridge] = bridges.as_slice() else {
                return Ok(None);
            };
            match &bridge.syntax {
                EdgeEndpoint::SyntaxDef {
                    path,
                    byte_start,
                    byte_end,
                } => (path.clone(), *byte_start, *byte_end),
                EdgeEndpoint::Scip { .. } => return Ok(None),
            }
        }
    };
    let mut rows: Vec<SymbolRow> = store::symbols_at(tx, snapshot_id, &path)?
        .into_iter()
        .filter(|s| {
            (s.kind == "function" || s.kind == "method")
                && s.byte_start == byte_start
                && s.byte_end == byte_end
        })
        .collect();
    if rows.len() != 1 {
        return Ok(None);
    }
    Ok(Some(Destination {
        path,
        row: rows.remove(0),
    }))
}

fn edge_name(kind: &str) -> Result<&'static str, String> {
    match kind {
        "CALLS" => Ok("CALLS"),
        "REFERENCES" => Ok("REFERENCES"),
        other => Err(format!("unexpected proven edge kind: {other}")),
    }
}

/// `CALLS` ranks before `REFERENCES` in the `via` choice.
fn edge_rank(name: &str) -> u8 {
    if name == "CALLS" {
        0
    } else {
        1
    }
}

/// The seed calls the row (`callee`) ranks before the row calls the seed.
fn direction_rank(direction: Direction) -> u8 {
    match direction {
        Direction::Callee => 0,
        Direction::Caller => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FileRow, SemanticFact, SemanticImport};

    /// One snapshot with a current graph proof, on a fabricated memory database.
    struct Fx {
        conn: Connection,
        snap: Snapshot,
    }

    fn fx() -> Fx {
        let conn = store::open_memory().unwrap();
        store::insert_snapshot(
            &conn,
            &Snapshot {
                id: 0,
                repo: "r".to_string(),
                root: "repo-root".to_string(),
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
        .unwrap();
        let snap = store::latest_snapshot(&conn).unwrap().unwrap();
        store::mark_graph_proof(&conn, snap.id).unwrap();
        Fx { conn, snap }
    }

    fn import(fx: &Fx, provider: &str) -> i64 {
        let artifact = store::new_nonce();
        crate::identity::record(&fx.conn, &artifact, &Default::default()).unwrap();
        store::insert_semantic_import(
            &fx.conn,
            &SemanticImport {
                snapshot_id: fx.snap.id,
                provider_name: provider.to_string(),
                provider_version: "v".to_string(),
                capability: "definitions,references".to_string(),
                project_root: "repo-root".to_string(),
                text_encoding: "UTF8".to_string(),
                artifact_id: artifact,
                source_revision: "r".to_string(),
                claimed_source_digest: String::new(),
                content_identity: "ci".to_string(),
                external_symbols: "[]".to_string(),
                verification: "Unverified".to_string(),
            },
        )
        .unwrap()
    }

    /// An import that passes `import_provenance_ok`.
    fn valid(fx: &Fx) -> i64 {
        import(fx, "scip-import")
    }

    /// An import that fails `import_provenance_ok` (ephemeral LSP provider).
    fn invalid(fx: &Fx) -> i64 {
        import(fx, "lsp-subprocess")
    }

    struct File {
        id: i64,
        path: String,
        digest: String,
    }

    fn file(fx: &Fx, path: &str, src: &[u8]) -> File {
        let digest = store::content_digest(src);
        let id = store::insert_file(
            &fx.conn,
            fx.snap.id,
            &FileRow {
                path: path.to_string(),
                language: "python".to_string(),
                bytes: src.len() as u64,
                digest: digest.clone(),
                errors: 0,
            },
        )
        .unwrap();
        File {
            id,
            path: path.to_string(),
            digest,
        }
    }

    /// The one source file of most fixtures.
    fn lib(fx: &Fx) -> File {
        file(fx, "lib.py", b"lib source\n")
    }

    fn uri(name: &str) -> String {
        format!("scip-test pkg 1.0 {name}().")
    }

    fn symbol(
        fx: &Fx,
        f: &File,
        kind: &str,
        name: &str,
        line: u32,
        span: (usize, usize),
    ) -> SymbolRow {
        let row = SymbolRow {
            file_id: f.id,
            kind: kind.to_string(),
            name: Some(name.to_string()),
            start_line: line,
            start_col: 1,
            end_line: line,
            end_col: 1,
            byte_start: span.0,
            byte_end: span.1,
            provider: "syntax".to_string(),
            grammar: "test".to_string(),
            language: "python".to_string(),
        };
        store::insert_symbol(&fx.conn, fx.snap.id, &row).unwrap();
        row
    }

    /// The definition fact that lets `owner_scip_checked` map `symbol` to `uri`
    /// inside `imp`: one roles-1 occurrence at the start of the owner's span.
    fn definition(fx: &Fx, f: &File, name: &str, line: u32, at: usize, imp: i64, symbol: &str) {
        store::insert_semantic_facts(
            &fx.conn,
            &[SemanticFact {
                snapshot_id: fx.snap.id,
                import_id: imp,
                file_id: None,
                rel_path: f.path.clone(),
                file_digest: f.digest.clone(),
                symbol_uri: Some(symbol.to_string()),
                display_name: Some(name.to_string()),
                roles: 1,
                start_line: line,
                start_col: 1,
                end_line: line,
                end_col: 1,
                byte_start: at,
                byte_end: at + 1,
                precision: "Heuristic".to_string(),
                relationships: "[]".to_string(),
                external: false,
            }],
        )
        .unwrap();
    }

    /// A function at `span`; with `imp`, also its definition in that import.
    fn func(
        fx: &Fx,
        f: &File,
        name: &str,
        line: u32,
        span: (usize, usize),
        imp: Option<i64>,
    ) -> SymbolRow {
        let row = symbol(fx, f, "function", name, line, span);
        if let Some(imp) = imp {
            definition(fx, f, name, line, span.0, imp, &uri(name));
        }
        row
    }

    fn scip(imp: i64, name: &str) -> EdgeEndpoint {
        EdgeEndpoint::Scip {
            import_id: imp,
            symbol_uri: uri(name),
            doc: None,
        }
    }

    fn syn(f: &File, row: &SymbolRow) -> EdgeEndpoint {
        EdgeEndpoint::SyntaxDef {
            path: f.path.clone(),
            byte_start: row.byte_start,
            byte_end: row.byte_end,
        }
    }

    /// A proven, exact edge with no import evidence beyond its endpoints.
    fn edge(
        fx: &Fx,
        f: &File,
        caller: Option<EdgeEndpoint>,
        callee: Option<EdgeEndpoint>,
        kind: &str,
    ) -> EdgeRow {
        EdgeRow {
            snapshot_id: fx.snap.id,
            file_id: f.id,
            caller,
            caller_path: f.path.clone(),
            caller_span_bytes: (0, 0),
            callee,
            callee_name: "target".to_string(),
            edge_kind: kind.to_string(),
            precision: "Exact".to_string(),
            provider: "scip".to_string(),
            evidence: "{}".to_string(),
            unresolved: None,
        }
    }

    fn put(fx: &Fx, edge: &EdgeRow) {
        store::insert_edge(&fx.conn, edge).unwrap();
    }

    fn hit(f: &File, name: &str, kind: &str, line: u32, byte_start: usize) -> Hit {
        Hit {
            path: f.path.clone(),
            line,
            name: Some(name.to_string()),
            kind: kind.to_string(),
            why: Vec::new(),
            byte_start,
        }
    }

    /// The seed `seed` at line 1, byte 0: the hit for `func(.., "seed", 1, (0, 40), ..)`.
    fn seed_hit(f: &File) -> Hit {
        hit(f, "seed", "function", 1, 0)
    }

    fn run(
        fx: &Fx,
        fresh: bool,
        hits: &[Hit],
        limits: &Limits,
        stale: &[i64],
    ) -> Result<Expansion, String> {
        let mut verdict = |id: i64| -> Result<bool, String> { Ok(stale.contains(&id)) };
        expand(&fx.conn, &fx.snap, fresh, hits, limits, &mut verdict)
    }

    fn names(out: &Expansion) -> Vec<String> {
        out.rows
            .iter()
            .map(|row| row.name.clone().unwrap())
            .collect()
    }

    fn lim(seeds: usize, per_seed: usize, total: usize, edge_read: usize) -> Limits {
        Limits {
            seeds,
            per_seed,
            total,
            edge_read,
        }
    }

    /// `seed` (line 1) and `callee` (line 10) in one import, with the proven
    /// CALLS edge between them. Every test of the common path starts here.
    fn seed_calls_callee(fx: &Fx, f: &File, imp: i64) {
        func(fx, f, "seed", 1, (0, 40), Some(imp));
        func(fx, f, "callee", 10, (100, 140), Some(imp));
        put(
            fx,
            &edge(
                fx,
                f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "callee")),
                "CALLS",
            ),
        );
    }

    // ---- status precedence -------------------------------------------------

    #[test]
    fn none_imported_wins_over_no_seed_and_stale() {
        let fx = fx();
        let f = lib(&fx);
        // No import at all: the first rule wins although a seed exists, the
        // source is not fresh and the graph proof is missing.
        fx.conn
            .execute(
                "UPDATE meta SET value = 'mismatch' WHERE key = 'graph_proof'",
                [],
            )
            .unwrap();
        let out = run(&fx, false, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::NoneImported);
        assert!(out.reasons.is_empty());
        assert_eq!(
            out.seeds.considered, 1,
            "seeds taken, even when none mapped"
        );
        assert!(out.seeds.items.is_empty());
        assert!(out.rows.is_empty());
    }

    #[test]
    fn none_imported_when_every_import_fails_provenance() {
        let fx = fx();
        let f = lib(&fx);
        let bad = invalid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(bad));
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::NoneImported);
        assert!(out.seeds.items.is_empty());
    }

    #[test]
    fn no_seed_wins_over_stale() {
        let fx = fx();
        valid(&fx);
        let out = run(&fx, false, &[], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::NoSeed);
        assert_eq!(out.seeds.considered, 0);
        assert!(out.reasons.is_empty());
    }

    #[test]
    fn source_not_fresh_wins_over_graph_proof_and_import_checks() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        fx.conn
            .execute(
                "UPDATE meta SET value = 'mismatch' WHERE key = 'graph_proof'",
                [],
            )
            .unwrap();
        // Neither a mapping nor an import check may run on a stale source.
        let mut never = |_: i64| -> Result<bool, String> {
            panic!("no import is checked on a source that is not fresh")
        };
        let out = expand(
            &fx.conn,
            &fx.snap,
            false,
            &[seed_hit(&f)],
            &Limits::DEFAULT,
            &mut never,
        )
        .unwrap();
        assert_eq!(out.status, Status::Stale);
        assert_eq!(out.reasons, vec!["source-not-fresh"]);
        assert_eq!(out.seeds.considered, 1);
        assert!(out.seeds.items.is_empty(), "no mapping ran");
        assert!(out.rows.is_empty());
    }

    #[test]
    fn graph_proof_mismatch_wins_over_import_checks() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        fx.conn
            .execute(
                "UPDATE meta SET value = 'mismatch' WHERE key = 'graph_proof'",
                [],
            )
            .unwrap();
        let mut never = |_: i64| -> Result<bool, String> {
            panic!("no import is checked on an incompatible graph proof")
        };
        let out = expand(
            &fx.conn,
            &fx.snap,
            true,
            &[seed_hit(&f)],
            &Limits::DEFAULT,
            &mut never,
        )
        .unwrap();
        assert_eq!(out.status, Status::Stale);
        assert_eq!(out.reasons, vec!["graph-proof-incompatible"]);
        assert!(out.seeds.items.is_empty());
    }

    #[test]
    fn all_skipped_is_no_seed_before_any_import_check() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        // A class is never callable, even with a definition fact that would map.
        symbol(&fx, &f, "class", "Widget", 1, (0, 40));
        definition(&fx, &f, "Widget", 1, 0, imp, &uri("Widget"));
        let mut never = |_: i64| -> Result<bool, String> {
            panic!("a seed that is not mapped is never checked for staleness")
        };
        let out = expand(
            &fx.conn,
            &fx.snap,
            true,
            &[hit(&f, "Widget", "class", 1, 0)],
            &Limits::DEFAULT,
            &mut never,
        )
        .unwrap();
        assert_eq!(out.status, Status::NoSeed);
        assert_eq!(out.seeds.considered, 1);
        assert_eq!(out.seeds.mapped, 0);
        assert_eq!(out.seeds.skipped, 1);
        assert_eq!(out.seeds.items[0].skipped, Some(Skip::NotCallable));
        assert!(!out.seeds.items[0].scip);
    }

    #[test]
    fn no_eligible_edge_when_a_mapped_seed_has_no_relation() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::NoEligibleEdge);
        assert_eq!(out.reasons, vec!["empty-result-not-coverage-proof"]);
        assert_eq!(out.seeds.items.len(), 1);
        assert!(out.seeds.items[0].scip);
        assert_eq!(out.seeds.items[0].eligible, 0);
        assert!(out.rows.is_empty());
        assert_eq!(out.limits, Limits::DEFAULT);
    }

    #[test]
    fn available_adds_the_callee_with_its_via() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        seed_calls_callee(&fx, &f, imp);
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::Available);
        assert!(out.reasons.is_empty());
        assert_eq!(out.rows.len(), 1);
        let row = &out.rows[0];
        assert_eq!(row.path, "lib.py");
        assert_eq!(row.line, 10);
        assert_eq!(row.name.as_deref(), Some("callee"));
        assert_eq!(row.kind, "function");
        assert_eq!(row.via.seed.path, "lib.py");
        assert_eq!(row.via.seed.line, 1);
        assert_eq!(row.via.seed.name.as_deref(), Some("seed"));
        assert_eq!(row.via.edge, "CALLS");
        assert_eq!(row.via.direction, Direction::Callee);
        assert_eq!(row.via.precision, "Exact");
        assert_eq!(row.via.evidence_label, "Unverified");
        assert_eq!(out.added, 1);
        assert_eq!(out.omitted, 0);
        assert!(!out.truncated);
        let item = &out.seeds.items[0];
        assert!(item.scip);
        assert_eq!((item.eligible, item.added, item.omitted), (1, 1, 0));
        assert_eq!(item.unmapped, 0);
        assert!(!item.read_bounded);
        assert_eq!(item.skipped, None);
    }

    #[test]
    fn the_seed_import_is_asked_once_for_step_six_and_step_seven() {
        // The seed's SCIP import and the edge's import are the same import:
        // the verdict is asked once per call (memo).
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        seed_calls_callee(&fx, &f, imp);
        let mut asked = Vec::new();
        let mut verdict = |id: i64| -> Result<bool, String> {
            asked.push(id);
            Ok(false)
        };
        let out = expand(
            &fx.conn,
            &fx.snap,
            true,
            &[seed_hit(&f)],
            &Limits::DEFAULT,
            &mut verdict,
        )
        .unwrap();
        assert_eq!(out.status, Status::Available);
        assert_eq!(asked, vec![imp]);
    }

    // ---- step 6 before edges (S1-S3) --------------------------------------

    #[test]
    fn s1_a_seed_with_only_a_scip_identity_is_stale_when_its_import_changed() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        let hits = [seed_hit(&f)];
        let unchanged = run(&fx, true, &hits, &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(unchanged.status, Status::NoEligibleEdge);
        let changed = run(&fx, true, &hits, &Limits::DEFAULT, &[imp]).unwrap();
        assert_eq!(changed.status, Status::Stale);
        assert_eq!(changed.reasons, vec!["import-stale"]);
        assert!(changed.rows.is_empty());
        assert_eq!(changed.seeds.items.len(), 1, "mapping ran before the check");
    }

    #[test]
    fn s2_an_unmapped_references_only_is_stale_when_its_import_changed() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        // A reference site: no caller endpoint, so the relation is only counted.
        put(
            &fx,
            &edge(&fx, &f, None, Some(scip(imp, "seed")), "REFERENCES"),
        );
        let hits = [seed_hit(&f)];
        let unchanged = run(&fx, true, &hits, &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(unchanged.status, Status::NoEligibleEdge);
        assert_eq!(unchanged.seeds.items[0].unmapped, 1);
        assert_eq!(unchanged.seeds.items[0].eligible, 0);
        let changed = run(&fx, true, &hits, &Limits::DEFAULT, &[imp]).unwrap();
        assert_eq!(changed.status, Status::Stale);
        assert_eq!(changed.reasons, vec!["import-stale"]);
    }

    #[test]
    fn s3_an_import_under_no_seed_identity_and_no_followed_edge_is_never_asked() {
        let fx = fx();
        let f = lib(&fx);
        let other = file(&fx, "other.py", b"other source\n");
        let imp = valid(&fx);
        let unrelated = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        definition(&fx, &other, "unrelated", 1, 0, unrelated, &uri("unrelated"));
        let mut asked = Vec::new();
        // The unrelated import would read as stale if it were asked.
        let mut verdict = |id: i64| -> Result<bool, String> {
            asked.push(id);
            Ok(id == unrelated)
        };
        let out = expand(
            &fx.conn,
            &fx.snap,
            true,
            &[seed_hit(&f)],
            &Limits::DEFAULT,
            &mut verdict,
        )
        .unwrap();
        assert_eq!(asked, vec![imp]);
        assert_eq!(out.status, Status::NoEligibleEdge);
    }

    // ---- caps --------------------------------------------------------------

    #[test]
    fn per_seed_cap_omits_the_rest_and_says_so() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        func(&fx, &f, "aaa", 10, (100, 140), Some(imp));
        func(&fx, &f, "bbb", 20, (200, 240), Some(imp));
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "aaa")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "bbb")),
                "CALLS",
            ),
        );
        let out = run(&fx, true, &[seed_hit(&f)], &lim(5, 1, 25, 200), &[]).unwrap();
        assert_eq!(names(&out), vec!["aaa"]);
        let item = &out.seeds.items[0];
        assert_eq!((item.eligible, item.added, item.omitted), (2, 1, 1));
        assert_eq!(out.omitted, 1);
        assert_eq!(out.added, 1);
        assert!(out.truncated);
    }

    #[test]
    fn total_cap_is_shared_across_seeds() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        func(&fx, &f, "seed2", 5, (50, 90), Some(imp));
        func(&fx, &f, "aaa", 10, (100, 140), Some(imp));
        func(&fx, &f, "bbb", 20, (200, 240), Some(imp));
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "aaa")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed2")),
                Some(scip(imp, "bbb")),
                "CALLS",
            ),
        );
        let hits = [seed_hit(&f), hit(&f, "seed2", "function", 5, 50)];
        let out = run(&fx, true, &hits, &lim(5, 5, 1, 200), &[]).unwrap();
        assert_eq!(names(&out), vec!["aaa"]);
        assert_eq!(out.seeds.items[0].added, 1);
        assert_eq!(out.seeds.items[1].eligible, 1);
        assert_eq!(out.seeds.items[1].added, 0);
        assert_eq!(out.seeds.items[1].omitted, 1);
        assert_eq!(out.omitted, 1);
        assert!(out.truncated);
    }

    #[test]
    fn read_bounded_marks_a_read_that_stopped_at_the_budget() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        func(&fx, &f, "aaa", 10, (100, 140), Some(imp));
        func(&fx, &f, "bbb", 20, (200, 240), Some(imp));
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "aaa")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "bbb")),
                "CALLS",
            ),
        );
        let out = run(&fx, true, &[seed_hit(&f)], &lim(5, 5, 25, 1), &[]).unwrap();
        assert_eq!(names(&out), vec!["aaa"]);
        let item = &out.seeds.items[0];
        assert!(item.read_bounded);
        assert_eq!((item.eligible, item.added, item.omitted), (1, 1, 0));
        assert!(out.truncated, "a bounded read is a truncation");
    }

    // ---- shared edge budget (B1-B3) ----------------------------------------

    /// `seed` has a syntax identity and two SCIP identities (`one`, `two`, both
    /// valid). Callees `aaa`..`fff` live in `one`.
    fn budget_fixture() -> (Fx, File, i64, i64) {
        let fx = fx();
        let f = lib(&fx);
        let one = valid(&fx);
        let two = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(one));
        definition(&fx, &f, "seed", 1, 0, two, &uri("seed"));
        for (i, name) in ["aaa", "bbb", "ccc", "ddd", "eee", "fff"]
            .iter()
            .enumerate()
        {
            let at = 100 * (i + 1);
            func(&fx, &f, name, (i + 2) as u32, (at, at + 40), Some(one));
        }
        (fx, f, one, two)
    }

    /// The fixture's six outbound edges: two from the syntax identity, two from
    /// `one`'s SCIP identity, two from `two`'s.
    fn budget_edges(fx: &Fx, f: &File, one: i64, two: i64) -> Vec<EdgeRow> {
        let syntax = EdgeEndpoint::SyntaxDef {
            path: "lib.py".to_string(),
            byte_start: 0,
            byte_end: 40,
        };
        vec![
            edge(fx, f, Some(syntax.clone()), Some(scip(one, "aaa")), "CALLS"),
            edge(fx, f, Some(syntax), Some(scip(one, "bbb")), "CALLS"),
            edge(
                fx,
                f,
                Some(scip(one, "seed")),
                Some(scip(one, "ccc")),
                "CALLS",
            ),
            edge(
                fx,
                f,
                Some(scip(one, "seed")),
                Some(scip(one, "ddd")),
                "CALLS",
            ),
            edge(
                fx,
                f,
                Some(scip(two, "seed")),
                Some(scip(one, "eee")),
                "CALLS",
            ),
            edge(
                fx,
                f,
                Some(scip(two, "seed")),
                Some(scip(one, "fff")),
                "CALLS",
            ),
        ]
    }

    fn put_in_order(fx: &Fx, edges: &[EdgeRow], reverse: bool) {
        let mut order: Vec<&EdgeRow> = edges.iter().collect();
        if reverse {
            order.reverse();
        }
        for edge in order {
            put(fx, edge);
        }
    }

    fn budget(edge_read: usize) -> Limits {
        lim(5, 5, 25, edge_read)
    }

    #[test]
    fn b1_edge_read_budget_is_shared_across_identities() {
        let (fx, f, one, two) = budget_fixture();
        let edges = budget_edges(&fx, &f, one, two);
        put_in_order(&fx, &edges, false);
        let out = run(&fx, true, &[seed_hit(&f)], &budget(3), &[]).unwrap();
        // Syntax identity: aaa, bbb. Then `one`: the first of ccc, ddd. The rest is cut.
        assert_eq!(names(&out), vec!["aaa", "bbb", "ccc"]);
        let item = &out.seeds.items[0];
        assert!(item.read_bounded);
        assert_eq!((item.eligible, item.added, item.omitted), (3, 3, 0));
        assert!(out.truncated);
    }

    #[test]
    fn b2_an_exactly_used_budget_is_bounded_only_when_a_later_identity_has_an_edge() {
        let (fx, f, one, two) = budget_fixture();
        let edges = budget_edges(&fx, &f, one, two);
        // Three edges, exactly the budget: the syntax identity and `one`.
        put_in_order(&fx, &edges[..3], false);
        let exact = run(&fx, true, &[seed_hit(&f)], &budget(3), &[]).unwrap();
        assert_eq!(names(&exact), vec!["aaa", "bbb", "ccc"]);
        assert!(!exact.seeds.items[0].read_bounded, "nothing is left over");
        // One more edge, under the later identity `two`: the sentinel fires.
        put(&fx, &edges[4]);
        let later = run(&fx, true, &[seed_hit(&f)], &budget(3), &[]).unwrap();
        assert_eq!(names(&later), vec!["aaa", "bbb", "ccc"]);
        assert!(later.seeds.items[0].read_bounded);
    }

    #[test]
    fn b3_identity_order_is_deterministic_under_shuffled_insertion() {
        let forward = {
            let (fx, f, one, two) = budget_fixture();
            let edges = budget_edges(&fx, &f, one, two);
            put_in_order(&fx, &edges, false);
            run(&fx, true, &[seed_hit(&f)], &budget(3), &[]).unwrap()
        };
        let reversed = {
            let (fx, f, one, two) = budget_fixture();
            let edges = budget_edges(&fx, &f, one, two);
            put_in_order(&fx, &edges, true);
            run(&fx, true, &[seed_hit(&f)], &budget(3), &[]).unwrap()
        };
        assert_eq!(forward, reversed);
        let (fx, f, one, two) = budget_fixture();
        let edges = budget_edges(&fx, &f, one, two);
        put_in_order(&fx, &edges, false);
        let again = run(&fx, true, &[seed_hit(&f)], &budget(3), &[]).unwrap();
        assert_eq!(forward, again, "repeated runs are identical");
    }

    // ---- via choice and ordering -------------------------------------------

    #[test]
    fn via_prefers_calls_over_references_and_callee_over_caller() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        func(&fx, &f, "xxx", 10, (100, 140), Some(imp));
        func(&fx, &f, "yyy", 20, (200, 240), Some(imp));
        func(&fx, &f, "zzz", 30, (300, 340), Some(imp));
        // xxx: seed calls it (CALLS, callee) and it calls the seed (CALLS, caller).
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "xxx")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "xxx")),
                Some(scip(imp, "seed")),
                "CALLS",
            ),
        );
        // yyy: only a REFERENCES relation from the seed.
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "yyy")),
                "REFERENCES",
            ),
        );
        // zzz: only a CALLS relation into the seed.
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "zzz")),
                Some(scip(imp, "seed")),
                "CALLS",
            ),
        );
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(names(&out), vec!["xxx", "zzz", "yyy"]);
        assert_eq!(out.rows[0].via.edge, "CALLS");
        assert_eq!(out.rows[0].via.direction, Direction::Callee);
        assert_eq!(out.rows[1].via.edge, "CALLS");
        assert_eq!(out.rows[1].via.direction, Direction::Caller);
        assert_eq!(out.rows[2].via.edge, "REFERENCES");
        assert_eq!(out.rows[2].via.direction, Direction::Callee);
    }

    #[test]
    fn rows_are_ordered_by_path_then_span_within_one_via() {
        let fx = fx();
        let f = lib(&fx);
        let a = file(&fx, "a.py", b"a source\n");
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        func(&fx, &a, "pee", 5, (0, 40), Some(imp));
        func(&fx, &f, "kew", 10, (100, 140), Some(imp));
        // The lib.py callee is inserted first; the order is still by path.
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "kew")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "pee")),
                "CALLS",
            ),
        );
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(names(&out), vec!["pee", "kew"]);
        assert_eq!(out.rows[0].path, "a.py");
        assert_eq!(out.rows[1].path, "lib.py");
    }

    // ---- dedup -------------------------------------------------------------

    #[test]
    fn dedup_excludes_results_earlier_seeds_and_the_seed_itself() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "s1", 1, (0, 40), Some(imp));
        func(&fx, &f, "s2", 2, (50, 90), Some(imp));
        func(&fx, &f, "xx", 10, (100, 140), Some(imp));
        func(&fx, &f, "rr", 20, (150, 190), Some(imp));
        func(&fx, &f, "yy", 30, (200, 240), Some(imp));
        let s1_scip = scip(imp, "s1");
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(s1_scip.clone()),
                Some(scip(imp, "xx")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(s1_scip.clone()),
                Some(scip(imp, "rr")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(s1_scip.clone()),
                Some(s1_scip.clone()),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "s2")),
                Some(scip(imp, "xx")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "s2")),
                Some(scip(imp, "yy")),
                "CALLS",
            ),
        );
        // `rr` is a result but not a seed (seeds = 2), so it is only deduplicated.
        let hits = [
            hit(&f, "s1", "function", 1, 0),
            hit(&f, "s2", "function", 2, 50),
            hit(&f, "rr", "function", 20, 150),
        ];
        let limits = Limits {
            seeds: 2,
            ..Limits::DEFAULT
        };
        let out = run(&fx, true, &hits, &limits, &[]).unwrap();
        assert_eq!(names(&out), vec!["xx", "yy"]);
        assert_eq!(
            out.seeds.items[0].eligible, 1,
            "xx only: rr is a result, s1 is itself"
        );
        assert_eq!(
            out.seeds.items[1].eligible, 1,
            "yy only: xx was added by s1"
        );
    }

    // ---- skipped seeds -----------------------------------------------------

    #[test]
    fn skipped_seeds_say_why_and_a_mapped_seed_still_expands() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        symbol(&fx, &f, "class", "Widget", 1, (0, 40));
        func(&fx, &f, "seed", 5, (50, 90), Some(imp));
        func(&fx, &f, "callee", 10, (100, 140), Some(imp));
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "callee")),
                "CALLS",
            ),
        );
        let hits = [
            hit(&f, "Widget", "class", 1, 0),
            hit(&f, "ghost", "function", 2, 999),
            hit(&f, "seed", "function", 5, 50),
        ];
        let out = run(&fx, true, &hits, &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::Available);
        assert_eq!(out.seeds.considered, 3);
        assert_eq!(out.seeds.mapped, 1);
        assert_eq!(out.seeds.skipped, 2);
        assert_eq!(out.seeds.items[0].skipped, Some(Skip::NotCallable));
        assert_eq!(out.seeds.items[1].skipped, Some(Skip::UnknownSymbol));
        assert_eq!(out.seeds.items[2].skipped, None);
        assert_eq!(names(&out), vec!["callee"]);
    }

    #[test]
    fn all_seeds_unknown_is_no_seed_with_the_reason() {
        let fx = fx();
        let f = lib(&fx);
        valid(&fx);
        let out = run(
            &fx,
            true,
            &[hit(&f, "ghost", "function", 2, 999)],
            &Limits::DEFAULT,
            &[],
        )
        .unwrap();
        assert_eq!(out.status, Status::NoSeed);
        assert_eq!(out.seeds.items[0].skipped, Some(Skip::UnknownSymbol));
        assert_eq!(out.seeds.skipped, 1);
        assert_eq!(out.seeds.mapped, 0);
    }

    // ---- identity (I1-I4) ----------------------------------------------------

    #[test]
    fn i1_a_proven_call_from_a_seed_without_a_scip_identity_is_followed() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        let seed = func(&fx, &f, "seed", 1, (0, 40), None);
        func(&fx, &f, "xx", 10, (100, 140), Some(imp));
        let caller = syn(&f, &seed);
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(caller.clone()),
                Some(scip(imp, "xx")),
                "CALLS",
            ),
        );
        // The stored caller is the syntax identity, so the seed has no SCIP one.
        let (page, _) = store::proven_edge_page(&fx.conn, fx.snap.id, &caller, false, 10).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].caller, Some(caller));
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::Available);
        assert_eq!(names(&out), vec!["xx"]);
        assert!(!out.seeds.items[0].scip);
        assert_eq!(out.rows[0].via.direction, Direction::Callee);
    }

    #[test]
    fn i2_no_inbound_read_for_a_seed_without_a_scip_identity() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        let seed = func(&fx, &f, "seed", 1, (0, 40), None);
        func(&fx, &f, "cc", 10, (100, 140), Some(imp));
        let seed_syntax = syn(&f, &seed);
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "cc")),
                Some(seed_syntax.clone()),
                "CALLS",
            ),
        );
        // Control: the edge is stored and readable by its syntax endpoint.
        let (page, _) =
            store::proven_edge_page(&fx.conn, fx.snap.id, &seed_syntax, true, 10).unwrap();
        assert_eq!(page.len(), 1);
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::NoEligibleEdge);
        assert!(
            out.rows.is_empty(),
            "callers of a seed without SCIP identity are not read"
        );
        assert!(!out.seeds.items[0].scip);
    }

    #[test]
    fn i3_an_edge_found_through_a_syntax_identity_checks_its_import() {
        let fx = fx();
        let f = lib(&fx);
        let seed = func(&fx, &f, "seed", 1, (0, 40), None);
        let other_import = valid(&fx);
        func(&fx, &f, "yy", 10, (100, 140), Some(other_import));
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(syn(&f, &seed)),
                Some(scip(other_import, "yy")),
                "CALLS",
            ),
        );
        let hits = [seed_hit(&f)];
        let fine = run(&fx, true, &hits, &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(names(&fine), vec!["yy"], "control: fresh import, row added");
        let stale = run(&fx, true, &hits, &Limits::DEFAULT, &[other_import]).unwrap();
        assert_eq!(stale.status, Status::Stale);
        assert_eq!(stale.reasons, vec!["import-stale"]);
        assert!(stale.rows.is_empty());
    }

    #[test]
    fn i3_an_edge_naming_a_missing_or_invalid_import_is_never_followed() {
        let fx = fx();
        let f = lib(&fx);
        let seed = func(&fx, &f, "seed", 1, (0, 40), None);
        func(&fx, &f, "target", 10, (100, 140), None);
        // A valid import exists on the snapshot; the edges below name others.
        valid(&fx);
        let bad = invalid(&fx);
        let seed_syntax = syn(&f, &seed);
        let target = EdgeEndpoint::SyntaxDef {
            path: "lib.py".to_string(),
            byte_start: 100,
            byte_end: 140,
        };
        // Missing import id, invalid provider, and a non-integer import id.
        let missing = EdgeRow {
            evidence: "{\"import\": 9999}".to_string(),
            ..edge(
                &fx,
                &f,
                Some(seed_syntax.clone()),
                Some(target.clone()),
                "CALLS",
            )
        };
        let invalid_edge = edge(
            &fx,
            &f,
            Some(seed_syntax.clone()),
            Some(scip(bad, "bad")),
            "CALLS",
        );
        let not_integer = EdgeRow {
            evidence: "{\"import\": \"oops\"}".to_string(),
            ..edge(&fx, &f, Some(seed_syntax), Some(target), "CALLS")
        };
        put(&fx, &missing);
        put(&fx, &invalid_edge);
        put(&fx, &not_integer);
        let mut never = |_: i64| -> Result<bool, String> {
            panic!("an edge that fails provenance names no import to check")
        };
        let out = expand(
            &fx.conn,
            &fx.snap,
            true,
            &[seed_hit(&f)],
            &Limits::DEFAULT,
            &mut never,
        )
        .unwrap();
        assert_eq!(out.status, Status::NoEligibleEdge);
        assert!(out.rows.is_empty());
        assert_eq!(out.seeds.items[0].unmapped, 3);
        assert_eq!(out.seeds.items[0].eligible, 0);
    }

    #[test]
    fn i4_a_same_named_function_with_another_span_is_never_followed() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "run", 1, (0, 40), Some(imp));
        // Another `run` at another span, with its own proven edge.
        let other = symbol(&fx, &f, "function", "run", 5, (50, 90));
        func(&fx, &f, "zed", 20, (200, 240), Some(imp));
        func(&fx, &f, "wee", 30, (300, 340), Some(imp));
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(syn(&f, &other)),
                Some(scip(imp, "zed")),
                "CALLS",
            ),
        );
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "run")),
                Some(scip(imp, "wee")),
                "CALLS",
            ),
        );
        let out = run(
            &fx,
            true,
            &[hit(&f, "run", "function", 1, 0)],
            &Limits::DEFAULT,
            &[],
        )
        .unwrap();
        assert_eq!(names(&out), vec!["wee"]);
    }

    // ---- REFERENCES (F2) ----------------------------------------------------

    #[test]
    fn references_add_no_row_and_are_counted_as_unmapped() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        put(
            &fx,
            &edge(&fx, &f, None, Some(scip(imp, "seed")), "REFERENCES"),
        );
        put(
            &fx,
            &edge(&fx, &f, Some(scip(imp, "seed")), None, "REFERENCES"),
        );
        let out = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::NoEligibleEdge);
        assert!(out.rows.is_empty());
        assert_eq!(out.seeds.items[0].unmapped, 2);
        assert_eq!(out.seeds.items[0].eligible, 0);
    }

    // ---- error paths (E1-E4, E7b) -------------------------------------------

    #[test]
    fn e1_a_failed_graph_proof_read_is_an_error_and_a_mismatch_is_stale() {
        let fx_read = fx();
        let f = lib(&fx_read);
        let imp = valid(&fx_read);
        seed_calls_callee(&fx_read, &f, imp);
        fx_read.conn.execute_batch("DROP TABLE meta").unwrap();
        let err = run(&fx_read, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap_err();
        assert!(err.contains("meta"), "{err}");

        let fx_mismatch = fx();
        let g = lib(&fx_mismatch);
        let imp2 = valid(&fx_mismatch);
        seed_calls_callee(&fx_mismatch, &g, imp2);
        fx_mismatch
            .conn
            .execute(
                "UPDATE meta SET value = 'mismatch' WHERE key = 'graph_proof'",
                [],
            )
            .unwrap();
        let out = run(&fx_mismatch, true, &[seed_hit(&g)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(out.status, Status::Stale);
        assert_eq!(out.reasons, vec!["graph-proof-incompatible"]);
    }

    #[test]
    fn e2_a_failed_edge_or_import_read_is_an_error_never_a_status() {
        let fx_edges = fx();
        let f = lib(&fx_edges);
        let imp = valid(&fx_edges);
        seed_calls_callee(&fx_edges, &f, imp);
        fx_edges
            .conn
            .execute_batch("DROP TABLE code_edges")
            .unwrap();
        let err = run(&fx_edges, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap_err();
        assert!(err.contains("code_edges"), "{err}");

        let fx_imports = fx();
        let g = lib(&fx_imports);
        let imp2 = valid(&fx_imports);
        seed_calls_callee(&fx_imports, &g, imp2);
        fx_imports
            .conn
            .execute_batch("DROP TABLE semantic_imports")
            .unwrap();
        let err = run(&fx_imports, true, &[seed_hit(&g)], &Limits::DEFAULT, &[]).unwrap_err();
        assert!(err.contains("semantic_imports"), "{err}");
    }

    #[test]
    fn e3_a_failed_seed_identity_read_is_an_error_never_scip_false() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        // Control: the seed has its SCIP identity.
        let control = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert!(control.seeds.items[0].scip);
        // Only `semantic_owner_uris` names this index (INDEXED BY).
        fx.conn
            .execute_batch("DROP INDEX idx_semfacts_span")
            .unwrap();
        let err = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap_err();
        assert!(err.contains("idx_semfacts_span"), "{err}");
    }

    #[test]
    fn e4_a_failed_import_verdict_is_an_error() {
        let fx = fx();
        let f = lib(&fx);
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        let mut broken = |_: i64| -> Result<bool, String> { Err("verdict-broken".to_string()) };
        let err = expand(
            &fx.conn,
            &fx.snap,
            true,
            &[seed_hit(&f)],
            &Limits::DEFAULT,
            &mut broken,
        )
        .unwrap_err();
        assert_eq!(err, "verdict-broken");
    }

    #[test]
    fn e7b_a_failed_destination_mapping_is_an_error_not_unmapped() {
        let fx = fx();
        let f = lib(&fx);
        let other = file(&fx, "other.py", b"other source\n");
        let imp = valid(&fx);
        func(&fx, &f, "seed", 1, (0, 40), Some(imp));
        func(&fx, &other, "xx", 5, (100, 140), Some(imp));
        put(
            &fx,
            &edge(
                &fx,
                &f,
                Some(scip(imp, "seed")),
                Some(scip(imp, "xx")),
                "CALLS",
            ),
        );
        // Control first: the intact fixture maps the callee.
        let control = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap();
        assert_eq!(names(&control), vec!["xx"]);
        // Fault: the callee's definition fact cannot be decoded. The seed's own
        // mapping reads only its file, so it still succeeds.
        fx.conn
            .execute(
                "UPDATE semantic_facts SET byte_start = x'00' WHERE symbol_uri = ?1 AND (roles & 1) != 0",
                [uri("xx")],
            )
            .unwrap();
        let err = run(&fx, true, &[seed_hit(&f)], &Limits::DEFAULT, &[]).unwrap_err();
        assert!(err.contains("byte_start"), "{err}");
    }
}
