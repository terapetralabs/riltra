// SPDX-License-Identifier: Apache-2.0
//! Area map of one indexed snapshot (stage 5, task 1): directory areas, entry
//! points, hubs, proven links between areas and name-match candidates, all read
//! from one database revision. Flows arrive in a later task; `flows` is `[]`.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::edges;
use crate::overview::{self, ActionKind, NextAction, SnapshotState};
use crate::store::{self, EdgeEndpoint, EdgeRow, SymbolRow};
use crate::{index, paths, provider_setup};

const MAX_AREAS: usize = 50;
const ENTRY_POINTS_PER_AREA: usize = 3;
const HUBS_PER_AREA: usize = 3;
const MAX_LINKS: usize = 100;
const MAX_CANDIDATE_LINKS: usize = 100;
const MAX_FLOWS: usize = 3;
const FLOW_DEPTH: usize = 3;
const FLOW_PATHS: usize = 1000;
const FLOW_TIME_MS: u64 = 2000;
const FLOW_AREA_PATHS: usize = 10;
const FLOW_DEFINITIONS: usize = 20;
const MAX_IMPORTS: usize = 20;
const MAX_REPORT_BYTES: usize = 262_144;

const MAIN_BASIS: &str = "main-definition";
const CONVENTIONAL_BASIS: &str = "conventional-path";
const CANDIDATE_BASIS: &str = "candidate-name";
const GROUPING_NOTE: &str =
    "areas are navigation hints from directory structure, not architecture boundaries";
const NOTE: &str =
    "static paths are not proof of execution; candidate links are name matches, not relations";

/// Edge-row read cap for one report (D0). `DEFAULT` is what every caller
/// except the unit tests uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub edges_read: i64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits { edges_read: 50_000 };
}

#[derive(Debug, Clone, Serialize)]
pub struct AreaMap {
    pub repo: String,
    pub snapshot: SnapshotReport,
    pub graph_proof: Option<&'static str>,
    pub grouping: Grouping,
    pub edges_used: EdgesUsed,
    pub bounds: Bounds,
    pub import_verification: ImportVerification,
    pub imports: Vec<ImportEntry>,
    pub areas: Vec<AreaEntry>,
    pub links: Vec<Link>,
    pub candidate_links: Vec<CandidateLink>,
    /// Flow entries come from the flow task; this task always leaves them empty.
    pub flows: Vec<Value>,
    pub completeness: Completeness,
    pub next_actions: Vec<NextAction>,
    pub omissions: Vec<Omission>,
    pub note: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotReport {
    pub status: SnapshotState,
    pub reasons: Vec<String>,
    pub id: Option<i64>,
    pub nonce: Option<String>,
    pub generation: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Grouping {
    pub basis: &'static str,
    pub depth: usize,
    pub note: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgesUsed {
    pub proven: Vec<&'static str>,
    pub candidate: Vec<&'static str>,
    pub counted_only: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bounds {
    pub areas: usize,
    pub entry_points_per_area: usize,
    pub hubs_per_area: usize,
    pub links: usize,
    pub candidate_links: usize,
    pub flows: usize,
    pub flow_depth: usize,
    pub flow_paths: usize,
    pub flow_time_ms: u64,
    pub flow_area_paths: usize,
    pub flow_definitions: usize,
    pub imports: usize,
    pub edges_read: i64,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportVerification {
    pub total: usize,
    pub by_verification: Vec<VerificationCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationCount {
    pub verification: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportEntry {
    pub import: i64,
    pub provider: String,
    pub provider_version: String,
    pub verification: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AreaEntry {
    pub area: String,
    pub files: usize,
    pub languages: Vec<String>,
    pub definitions: usize,
    pub entry_points: Vec<EntryPoint>,
    pub hubs: Vec<Hub>,
    pub proven_in: Option<usize>,
    pub proven_out: Option<usize>,
    pub proven_internal: Option<usize>,
    pub candidate_internal: Option<usize>,
    pub unresolved_calls: Option<usize>,
    pub imports: Option<usize>,
}

impl AreaEntry {
    fn new(area: &str) -> AreaEntry {
        AreaEntry {
            area: area.to_string(),
            files: 0,
            languages: Vec::new(),
            definitions: 0,
            entry_points: Vec::new(),
            hubs: Vec::new(),
            proven_in: None,
            proven_out: None,
            proven_internal: None,
            candidate_internal: None,
            unresolved_calls: None,
            imports: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryPoint {
    pub path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub name: Option<String>,
    pub basis: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hub {
    pub path: String,
    pub line: u32,
    pub col: u32,
    pub name: Option<String>,
    pub kind: String,
    pub inbound_proven: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateLink {
    pub from: String,
    pub to: String,
    pub name_matches: usize,
    pub basis: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Completeness {
    pub edges_read: Option<usize>,
    pub edges_total: Option<usize>,
    pub edges_complete: Option<bool>,
    pub proven_read: Option<usize>,
    pub proven_total: Option<usize>,
    pub proven_complete: Option<bool>,
    pub unattributed_endpoints: Option<usize>,
    pub ambiguous_candidate_names: Option<usize>,
}

impl Completeness {
    /// Every value unknown: the report did not read the graph.
    fn unmeasured() -> Completeness {
        Completeness {
            edges_read: None,
            edges_total: None,
            edges_complete: None,
            proven_read: None,
            proven_total: None,
            proven_complete: None,
            unattributed_endpoints: None,
            ambiguous_candidate_names: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Omission {
    pub section: &'static str,
    pub reason: &'static str,
    pub omitted_count: Option<usize>,
}

/// Directory area of one file (D1): the first `depth` components of its
/// parent directory, joined by `/`; `.` for a file at the root.
pub fn area_of(path: &str, depth: usize) -> String {
    let normalized = path.replace('\\', "/");
    let parent = normalized.rsplit_once('/').map_or("", |(parent, _)| parent);
    let kept: Vec<&str> = parent
        .split('/')
        .filter(|component| !component.is_empty())
        .take(depth)
        .collect();
    if kept.is_empty() {
        ".".to_string()
    } else {
        kept.join("/")
    }
}

/// The proven rule (D3), the same predicate `store::map_edges` orders first.
pub fn is_proven(edge: &EdgeRow) -> bool {
    matches!(edge.edge_kind.as_str(), "CALLS" | "REFERENCES")
        && edge.precision == "Exact"
        && edge.unresolved.is_none()
        && edge.callee.is_some()
}

/// Area map for `repo` at `depth`, read from `home` with the default caps.
pub fn collect(home: &Path, repo: &str, root: &Path, depth: usize) -> Result<AreaMap, String> {
    collect_with(home, repo, root, depth, Limits::DEFAULT, || {})
}

/// `collect` with an edge-read cap and a seam (`after_pin`) that runs right
/// after the database revision is pinned and before any other read (D0).
pub fn collect_with(
    home: &Path,
    repo: &str,
    root: &Path,
    depth: usize,
    limits: Limits,
    after_pin: impl FnOnce(),
) -> Result<AreaMap, String> {
    if !(1..=4).contains(&depth) {
        return Err("area depth must be between 1 and 4".to_string());
    }
    if !paths::validate_name(repo) {
        return Err("invalid repository name".to_string());
    }
    let db = store::db_path(home, repo);
    match store::open_read(&db) {
        store::IndexRead::Missing => Ok(no_index(repo, depth, limits)),
        store::IndexRead::Outdated(v) => Ok(unsupported(
            repo,
            depth,
            limits,
            v,
            overview::outdated_schema_action(repo, v),
        )),
        store::IndexRead::Ahead(v) => Ok(unsupported(
            repo,
            depth,
            limits,
            v,
            overview::ahead_schema_action(v),
        )),
        store::IndexRead::Error(e) => Err(format!("cannot read index: {e}")),
        store::IndexRead::Ready(mut conn) => {
            let tx = conn
                .transaction()
                .map_err(|e| format!("cannot begin index read: {e}"))?;
            let base = store::live_base(&tx)?;
            after_pin();
            let canon: PathBuf = index::canonical_repo_root(root)?;
            let state = index::semantic_status_tx(&tx, repo, &canon)?;
            let report = match state.snapshot.clone() {
                None => without_snapshot(repo, depth, limits, &base, &state),
                Some(snapshot) => with_snapshot(&tx, repo, depth, limits, &base, state, snapshot)?,
            };
            tx.commit()
                .map_err(|e| format!("cannot end index read: {e}"))?;
            Ok(report)
        }
    }
}

pub fn render(map: &AreaMap) -> Result<String, String> {
    serde_json::to_string(map).map_err(|e| format!("cannot encode area map: {e}"))
}

/// Keys of a definition or a callee span: (path, byte start, byte end).
type SpanKey = (String, usize, usize);

/// Result of one graph read, taken only when the graph proof is current.
#[derive(Default)]
struct Graph {
    edges_read: usize,
    edges_total: usize,
    proven_total: usize,
    proven_read: usize,
    unattributed: usize,
    ambiguous: usize,
    links: BTreeMap<(String, String, String), usize>,
    candidates: BTreeMap<(String, String), usize>,
    inbound: BTreeMap<SpanKey, usize>,
}

/// Definitions, areas and name index built from the snapshot's own rows.
struct Base {
    areas: BTreeMap<String, AreaEntry>,
    entry_points: BTreeMap<String, Vec<EntryPoint>>,
    definitions: BTreeMap<SpanKey, Hub>,
    name_areas: BTreeMap<String, BTreeSet<String>>,
    paths_by_id: BTreeMap<i64, String>,
}

/// A callee's file (and span, when one symbol is known).
#[derive(Debug, Clone, PartialEq)]
struct Target {
    path: String,
    span: Option<(usize, usize)>,
}

/// Attributes an edge endpoint to a file (D2), caching the SCIP lookups.
struct Attribution<'a> {
    conn: &'a rusqlite::Connection,
    sid: i64,
    cache: BTreeMap<(i64, String, Option<String>), Option<Target>>,
}

impl Attribution<'_> {
    fn attribute(&mut self, endpoint: &EdgeEndpoint) -> Result<Option<Target>, String> {
        let (import_id, symbol_uri, doc) = match endpoint {
            EdgeEndpoint::SyntaxDef {
                path,
                byte_start,
                byte_end,
            } => {
                return Ok(Some(Target {
                    path: path.clone(),
                    span: Some((*byte_start, *byte_end)),
                }))
            }
            EdgeEndpoint::Scip {
                import_id,
                symbol_uri,
                doc,
            } => (*import_id, symbol_uri.clone(), doc.clone()),
        };
        let key = (import_id, symbol_uri, doc);
        if let Some(hit) = self.cache.get(&key) {
            return Ok(hit.clone());
        }
        let bridges = edges::owner_bridges(self.conn, self.sid, endpoint)?;
        let mut spans: BTreeMap<String, BTreeSet<(usize, usize)>> = BTreeMap::new();
        for bridge in &bridges {
            if let EdgeEndpoint::SyntaxDef {
                path,
                byte_start,
                byte_end,
            } = &bridge.syntax
            {
                spans
                    .entry(path.clone())
                    .or_default()
                    .insert((*byte_start, *byte_end));
            }
        }
        let bridged = if spans.len() == 1 {
            spans.into_iter().next()
        } else {
            None
        };
        let target = match (bridged, &key.2) {
            (Some((path, found)), _) => Some(Target {
                path,
                span: if found.len() == 1 {
                    found.into_iter().next()
                } else {
                    None
                },
            }),
            (None, Some(doc)) => Some(Target {
                path: doc.clone(),
                span: None,
            }),
            (None, None) => None,
        };
        self.cache.insert(key, target.clone());
        Ok(target)
    }
}

/// Counts one cross-area or internal proven edge onto an area counter, when
/// that area exists and the counter is measured.
fn bump(
    areas: &mut BTreeMap<String, AreaEntry>,
    area: &str,
    field: fn(&mut AreaEntry) -> &mut Option<usize>,
) {
    if let Some(entry) = areas.get_mut(area) {
        if let Some(count) = field(entry).as_mut() {
            *count += 1;
        }
    }
}

/// Omission entries keyed by (section, reason). A sum with an unknown count is unknown.
#[derive(Default)]
struct Omissions(BTreeMap<(&'static str, &'static str), Option<usize>>);

impl Omissions {
    fn add(&mut self, section: &'static str, reason: &'static str, count: Option<usize>) {
        let slot = self.0.entry((section, reason)).or_insert(Some(0));
        *slot = match (*slot, count) {
            (Some(total), Some(more)) => Some(total + more),
            _ => None,
        };
    }

    fn add_count(&mut self, section: &'static str, reason: &'static str, count: usize) {
        if count > 0 {
            self.add(section, reason, Some(count));
        }
    }

    fn into_vec(self) -> Vec<Omission> {
        self.0
            .into_iter()
            .map(|((section, reason), omitted_count)| Omission {
                section,
                reason,
                omitted_count,
            })
            .collect()
    }
}

fn quoted_repo(repo: &str) -> String {
    provider_setup::quote(provider_setup::Shell::current(), repo)
}

fn optional_text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn grouping(depth: usize) -> Grouping {
    Grouping {
        basis: "directory",
        depth,
        note: GROUPING_NOTE,
    }
}

fn edges_used() -> EdgesUsed {
    EdgesUsed {
        proven: vec!["CALLS", "REFERENCES"],
        candidate: vec!["POTENTIAL_CALL"],
        counted_only: vec!["IMPORTS"],
    }
}

fn bounds(limits: Limits) -> Bounds {
    Bounds {
        areas: MAX_AREAS,
        entry_points_per_area: ENTRY_POINTS_PER_AREA,
        hubs_per_area: HUBS_PER_AREA,
        links: MAX_LINKS,
        candidate_links: MAX_CANDIDATE_LINKS,
        flows: MAX_FLOWS,
        flow_depth: FLOW_DEPTH,
        flow_paths: FLOW_PATHS,
        flow_time_ms: FLOW_TIME_MS,
        flow_area_paths: FLOW_AREA_PATHS,
        flow_definitions: FLOW_DEFINITIONS,
        imports: MAX_IMPORTS,
        edges_read: limits.edges_read,
        max_bytes: MAX_REPORT_BYTES,
    }
}

/// A report whose snapshot-derived sections are all empty.
fn shell(
    repo: &str,
    depth: usize,
    limits: Limits,
    snapshot: SnapshotReport,
    next_actions: Vec<NextAction>,
) -> AreaMap {
    AreaMap {
        repo: repo.to_string(),
        snapshot,
        graph_proof: None,
        grouping: grouping(depth),
        edges_used: edges_used(),
        bounds: bounds(limits),
        import_verification: ImportVerification {
            total: 0,
            by_verification: Vec::new(),
        },
        imports: Vec::new(),
        areas: Vec::new(),
        links: Vec::new(),
        candidate_links: Vec::new(),
        flows: Vec::new(),
        completeness: Completeness::unmeasured(),
        next_actions,
        omissions: Vec::new(),
        note: NOTE,
    }
}

fn build_index_action() -> NextAction {
    NextAction {
        kind: ActionKind::BuildIndex,
        reason: "create an index to see the area map".to_string(),
    }
}

fn no_index(repo: &str, depth: usize, limits: Limits) -> AreaMap {
    shell(
        repo,
        depth,
        limits,
        SnapshotReport {
            status: SnapshotState::NoIndex,
            reasons: vec!["no-index".to_string()],
            id: None,
            nonce: None,
            generation: None,
        },
        vec![build_index_action()],
    )
}

fn unsupported(
    repo: &str,
    depth: usize,
    limits: Limits,
    version: u32,
    action: NextAction,
) -> AreaMap {
    shell(
        repo,
        depth,
        limits,
        SnapshotReport {
            status: SnapshotState::UnsupportedSchema,
            reasons: vec![format!("unsupported-schema:{version}")],
            id: None,
            nonce: None,
            generation: None,
        },
        vec![action],
    )
}

fn without_snapshot(
    repo: &str,
    depth: usize,
    limits: Limits,
    base: &store::PromoteBase,
    state: &index::SemanticState,
) -> AreaMap {
    let mut report = shell(
        repo,
        depth,
        limits,
        SnapshotReport {
            status: SnapshotState::Unknown,
            reasons: vec!["no-snapshot".to_string()],
            id: None,
            nonce: optional_text(&base.nonce),
            generation: Some(base.generation),
        },
        vec![build_index_action()],
    );
    report.import_verification = import_verification(&state.rows);
    report
}

fn import_verification(rows: &[index::SemanticStatusRow]) -> ImportVerification {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in rows {
        *counts.entry(row.verification.as_str()).or_insert(0) += 1;
    }
    ImportVerification {
        total: rows.len(),
        by_verification: counts
            .into_iter()
            .map(|(verification, count)| VerificationCount {
                verification: verification.to_string(),
                count,
            })
            .collect(),
    }
}

fn import_entries(
    rows: &[index::SemanticStatusRow],
    omissions: &mut Omissions,
) -> Vec<ImportEntry> {
    let mut sorted: Vec<&index::SemanticStatusRow> = rows.iter().collect();
    sorted.sort_by_key(|row| row.import_id);
    omissions.add_count(
        "imports",
        "item_limit",
        sorted.len().saturating_sub(MAX_IMPORTS),
    );
    sorted
        .into_iter()
        .take(MAX_IMPORTS)
        .map(|row| ImportEntry {
            import: row.import_id,
            provider: row.provider_name.clone(),
            provider_version: row.provider_version.clone(),
            verification: row.verification.clone(),
        })
        .collect()
}

/// Files, languages, definitions, entry points and the name index: everything
/// that does not come from `code_edges`.
fn build_base(
    depth: usize,
    files: &[store::FileRow],
    file_ids: &BTreeMap<String, (i64, String)>,
    symbols: &[SymbolRow],
) -> Base {
    let paths_by_id: BTreeMap<i64, String> = file_ids
        .iter()
        .map(|(path, (id, _))| (*id, path.clone()))
        .collect();
    let mut by_file: BTreeMap<i64, Vec<&SymbolRow>> = BTreeMap::new();
    for symbol in symbols {
        by_file.entry(symbol.file_id).or_default().push(symbol);
    }
    for list in by_file.values_mut() {
        list.sort_by(|a, b| {
            (a.byte_start, &a.kind, a.byte_end).cmp(&(b.byte_start, &b.kind, b.byte_end))
        });
    }

    let mut areas: BTreeMap<String, AreaEntry> = BTreeMap::new();
    let mut languages: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut entry_points: BTreeMap<String, Vec<EntryPoint>> = BTreeMap::new();
    for file in files {
        let area = area_of(&file.path, depth);
        languages
            .entry(area.clone())
            .or_default()
            .insert(file.language.clone());
        let entry = areas
            .entry(area.clone())
            .or_insert_with(|| AreaEntry::new(&area));
        entry.files += 1;
        let Some((file_id, _)) = file_ids.get(&file.path) else {
            continue;
        };
        let here: &[&SymbolRow] = match by_file.get(file_id) {
            Some(list) => list.as_slice(),
            None => &[],
        };
        entry.definitions += here.len();
        let has_main = here
            .iter()
            .any(|symbol| symbol.name.as_deref() == Some("main"));
        for symbol in here
            .iter()
            .filter(|symbol| symbol.name.as_deref() == Some("main"))
        {
            entry_points
                .entry(area.clone())
                .or_default()
                .push(EntryPoint {
                    path: file.path.clone(),
                    line: Some(symbol.start_line),
                    col: Some(symbol.start_col),
                    name: Some("main".to_string()),
                    basis: MAIN_BASIS,
                });
        }
        if !has_main && overview::conventional_entry(&file.path) {
            entry_points
                .entry(area.clone())
                .or_default()
                .push(EntryPoint {
                    path: file.path.clone(),
                    line: None,
                    col: None,
                    name: None,
                    basis: CONVENTIONAL_BASIS,
                });
        }
    }
    for (area, names) in languages {
        if let Some(entry) = areas.get_mut(&area) {
            entry.languages = names.into_iter().collect();
        }
    }

    let mut ordered: Vec<&SymbolRow> = symbols.iter().collect();
    ordered.sort_by(|a, b| {
        (
            a.file_id,
            a.byte_start,
            a.byte_end,
            &a.kind,
            a.start_line,
            a.start_col,
        )
            .cmp(&(
                b.file_id,
                b.byte_start,
                b.byte_end,
                &b.kind,
                b.start_line,
                b.start_col,
            ))
    });
    let mut definitions: BTreeMap<SpanKey, Hub> = BTreeMap::new();
    let mut name_areas: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for symbol in ordered {
        let Some(path) = paths_by_id.get(&symbol.file_id) else {
            continue;
        };
        if let Some(name) = symbol.name.as_deref() {
            if matches!(symbol.kind.as_str(), "function" | "method") {
                name_areas
                    .entry(name.to_string())
                    .or_default()
                    .insert(area_of(path, depth));
            }
        }
        definitions
            .entry((path.clone(), symbol.byte_start, symbol.byte_end))
            .or_insert_with(|| Hub {
                path: path.clone(),
                line: symbol.start_line,
                col: symbol.start_col,
                name: symbol.name.clone(),
                kind: symbol.kind.clone(),
                inbound_proven: 0,
            });
    }

    Base {
        areas,
        entry_points,
        definitions,
        name_areas,
        paths_by_id,
    }
}

/// Reads the snapshot's edges, imports and proven callees, all on the pinned
/// transaction. Called only when the graph proof is current.
fn read_graph(
    conn: &rusqlite::Connection,
    sid: i64,
    depth: usize,
    limits: Limits,
    base: &mut Base,
) -> Result<Graph, String> {
    let totals = store::snapshot_edge_counts(conn, sid)?;
    let mut graph = Graph {
        edges_total: totals.total,
        proven_total: store::proven_edge_count(conn, sid)?,
        ..Graph::default()
    };
    let rows = store::map_edges(conn, sid, limits.edges_read)?;
    graph.edges_read = rows.len();
    let mut attribution = Attribution {
        conn,
        sid,
        cache: BTreeMap::new(),
    };
    for row in &rows {
        let from = area_of(&row.caller_path, depth);
        if is_proven(row) {
            graph.proven_read += 1;
            let target = match &row.callee {
                Some(endpoint) => attribution.attribute(endpoint)?,
                None => None,
            };
            let Some(target) = target else {
                graph.unattributed += 1;
                continue;
            };
            let to = area_of(&target.path, depth);
            if to == from {
                bump(&mut base.areas, &from, |a| &mut a.proven_internal);
            } else {
                bump(&mut base.areas, &from, |a| &mut a.proven_out);
                bump(&mut base.areas, &to, |a| &mut a.proven_in);
                *graph
                    .links
                    .entry((from, to, row.edge_kind.clone()))
                    .or_insert(0) += 1;
            }
            if let Some((start, end)) = target.span {
                *graph
                    .inbound
                    .entry((target.path.clone(), start, end))
                    .or_insert(0) += 1;
            }
        } else if row.edge_kind == "POTENTIAL_CALL"
            && row.callee.is_none()
            && !row.callee_name.is_empty()
        {
            match base.name_areas.get(&row.callee_name) {
                None => bump(&mut base.areas, &from, |a| &mut a.unresolved_calls),
                Some(found) if found.len() > 1 => graph.ambiguous += 1,
                Some(found) => {
                    if let Some(to) = found.iter().next() {
                        if *to == from {
                            bump(&mut base.areas, &from, |a| &mut a.candidate_internal);
                        } else {
                            *graph.candidates.entry((from, to.clone())).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    }
    for import in store::imports_for(conn, sid)? {
        if let Some(path) = base.paths_by_id.get(&import.file_id) {
            bump(&mut base.areas, &area_of(path, depth), |a| &mut a.imports);
        }
    }
    Ok(graph)
}

/// Hub lists per area (D4), before the per-area cap.
fn hub_lists(
    definitions: &BTreeMap<SpanKey, Hub>,
    inbound: &BTreeMap<SpanKey, usize>,
    depth: usize,
) -> BTreeMap<String, Vec<Hub>> {
    let mut lists: BTreeMap<String, Vec<Hub>> = BTreeMap::new();
    for (key, count) in inbound {
        if let Some(definition) = definitions.get(key) {
            let mut hub = definition.clone();
            hub.inbound_proven = *count;
            lists
                .entry(area_of(&hub.path, depth))
                .or_default()
                .push(hub);
        }
    }
    lists
}

/// Area list after the D7 cap, sorted by name, with entry points and hubs capped per area.
fn finish_areas(
    areas: BTreeMap<String, AreaEntry>,
    mut entry_points: BTreeMap<String, Vec<EntryPoint>>,
    mut hubs: BTreeMap<String, Vec<Hub>>,
    omissions: &mut Omissions,
) -> Vec<AreaEntry> {
    let mut all: Vec<AreaEntry> = areas.into_values().collect();
    all.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.area.cmp(&b.area)));
    omissions.add_count("areas", "item_limit", all.len().saturating_sub(MAX_AREAS));
    all.truncate(MAX_AREAS);
    all.sort_by(|a, b| a.area.cmp(&b.area));
    for entry in &mut all {
        let mut points = entry_points.remove(&entry.area).unwrap_or_default();
        points.sort_by(|a, b| {
            (basis_rank(a.basis), &a.path, a.line).cmp(&(basis_rank(b.basis), &b.path, b.line))
        });
        omissions.add_count(
            "entry_points",
            "item_limit",
            points.len().saturating_sub(ENTRY_POINTS_PER_AREA),
        );
        points.truncate(ENTRY_POINTS_PER_AREA);
        entry.entry_points = points;

        let mut ranked = hubs.remove(&entry.area).unwrap_or_default();
        ranked.sort_by(|a, b| {
            (Reverse(a.inbound_proven), &a.path, a.line, a.col).cmp(&(
                Reverse(b.inbound_proven),
                &b.path,
                b.line,
                b.col,
            ))
        });
        omissions.add_count(
            "hubs",
            "item_limit",
            ranked.len().saturating_sub(HUBS_PER_AREA),
        );
        ranked.truncate(HUBS_PER_AREA);
        entry.hubs = ranked;
    }
    all
}

/// The main-definition basis sorts first.
fn basis_rank(basis: &str) -> u8 {
    u8::from(basis != MAIN_BASIS)
}

fn finish_links(
    counts: BTreeMap<(String, String, String), usize>,
    omissions: &mut Omissions,
) -> Vec<Link> {
    let mut all: Vec<((String, String, String), usize)> = counts.into_iter().collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    omissions.add_count("links", "item_limit", all.len().saturating_sub(MAX_LINKS));
    all.truncate(MAX_LINKS);
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all.into_iter()
        .map(|((from, to, kind), count)| Link {
            from,
            to,
            kind,
            count,
        })
        .collect()
}

fn finish_candidates(
    counts: BTreeMap<(String, String), usize>,
    omissions: &mut Omissions,
) -> Vec<CandidateLink> {
    let mut all: Vec<((String, String), usize)> = counts.into_iter().collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    omissions.add_count(
        "candidate_links",
        "item_limit",
        all.len().saturating_sub(MAX_CANDIDATE_LINKS),
    );
    all.truncate(MAX_CANDIDATE_LINKS);
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all.into_iter()
        .map(|((from, to), name_matches)| CandidateLink {
            from,
            to,
            name_matches,
            basis: CANDIDATE_BASIS,
        })
        .collect()
}

/// Every snapshot-derived section, from one pinned revision.
fn with_snapshot(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    depth: usize,
    limits: Limits,
    revision: &store::PromoteBase,
    state: index::SemanticState,
    snapshot: store::Snapshot,
) -> Result<AreaMap, String> {
    let sid = snapshot.id;
    let files = store::files_of(tx, sid)?;
    let file_ids = store::file_id_map(tx, sid)?;
    let symbols = store::symbols_for(tx, sid)?;
    let current = store::graph_proof_current(tx, sid)?;
    let mut base = build_base(depth, &files, &file_ids, &symbols);

    let mut omissions = Omissions::default();
    let graph = if current {
        for entry in base.areas.values_mut() {
            entry.proven_in = Some(0);
            entry.proven_out = Some(0);
            entry.proven_internal = Some(0);
            entry.candidate_internal = Some(0);
            entry.unresolved_calls = Some(0);
            entry.imports = Some(0);
        }
        Some(read_graph(tx, sid, depth, limits, &mut base)?)
    } else {
        omissions.add("graph", "graph_proof_incompatible", None);
        None
    };

    let hubs = graph
        .as_ref()
        .map(|g| hub_lists(&base.definitions, &g.inbound, depth))
        .unwrap_or_default();
    let (links, candidate_links, completeness) = match &graph {
        Some(g) => {
            omissions.add_count(
                "edges",
                "item_limit",
                g.edges_total.saturating_sub(g.edges_read),
            );
            let links = finish_links(g.links.clone(), &mut omissions);
            let candidates = finish_candidates(g.candidates.clone(), &mut omissions);
            let completeness = Completeness {
                edges_read: Some(g.edges_read),
                edges_total: Some(g.edges_total),
                edges_complete: Some(g.edges_read == g.edges_total),
                proven_read: Some(g.proven_read),
                proven_total: Some(g.proven_total),
                proven_complete: Some(g.proven_read == g.proven_total),
                unattributed_endpoints: Some(g.unattributed),
                ambiguous_candidate_names: Some(g.ambiguous),
            };
            (links, candidates, completeness)
        }
        None => (Vec::new(), Vec::new(), Completeness::unmeasured()),
    };
    let areas = finish_areas(base.areas, base.entry_points, hubs, &mut omissions);
    let imports = import_entries(&state.rows, &mut omissions);

    let status = match state.freshness.as_deref() {
        Some("fresh") => SnapshotState::Fresh,
        Some("stale") => SnapshotState::Stale,
        _ => SnapshotState::Unknown,
    };
    let mut reasons = state.reasons.clone();
    reasons.sort();
    reasons.dedup();
    let stale_imports = state
        .rows
        .iter()
        .filter(|row| row.verification == "Stale")
        .count();

    // D9, in rule order. Rules 4, 4a and 5 need a current graph.
    let mut next_actions = Vec::new();
    if !current {
        next_actions.push(NextAction {
            kind: ActionKind::RefreshIndex,
            reason: format!(
                "stored relationships do not pass the current graph-proof check; validate them with riltra semantic import or riltra index --full --repo {} before relying on links or flows",
                quoted_repo(repo)
            ),
        });
    }
    if status != SnapshotState::Fresh {
        next_actions.push(NextAction {
            kind: ActionKind::ReviewFreshness,
            reason: "the indexed snapshot is not confirmed fresh; areas and links may not match the source".to_string(),
        });
    }
    if stale_imports > 0 {
        next_actions.push(NextAction {
            kind: ActionKind::ReviewFreshness,
            reason: "one or more semantic imports are Stale; relations from them may not match the source".to_string(),
        });
    }
    if let Some(g) = &graph {
        let has_language = files.iter().any(|file| !file.language.is_empty());
        if has_language && g.proven_total == 0 {
            next_actions.push(NextAction {
                kind: ActionKind::SetupSemanticProvider,
                reason: format!(
                    "no proven relations in this snapshot; riltra semantic setup --repo {} shows provider options",
                    quoted_repo(repo)
                ),
            });
        }
        if g.proven_read < g.proven_total {
            next_actions.push(NextAction {
                kind: ActionKind::ReviewFreshness,
                reason: format!(
                    "the snapshot holds more proven relations than one report reads ({} of {}); links and hubs are partial",
                    g.proven_read, g.proven_total
                ),
            });
        }
    }

    Ok(AreaMap {
        repo: repo.to_string(),
        snapshot: SnapshotReport {
            status,
            reasons,
            id: Some(sid),
            nonce: optional_text(&revision.nonce),
            generation: Some(revision.generation),
        },
        graph_proof: Some(if current { "current" } else { "incompatible" }),
        grouping: grouping(depth),
        edges_used: edges_used(),
        bounds: bounds(limits),
        import_verification: import_verification(&state.rows),
        imports,
        areas,
        links,
        candidate_links,
        flows: Vec::new(),
        completeness,
        next_actions,
        omissions: omissions.into_vec(),
        note: NOTE,
    })
}

#[cfg(test)]
mod tests {
    use super::{area_of, collect, collect_with, is_proven, render, Limits};
    use crate::edges::tests as edge_fixtures;
    use crate::store::tests as store_fixtures;
    use crate::store::{self, EdgeEndpoint, EdgeRow, FileRow};
    use crate::testutil::tmpdir;
    use serde_json::{json, Value};
    use std::path::PathBuf;

    struct Fixture {
        home: PathBuf,
        conn: rusqlite::Connection,
        sid: i64,
    }

    /// A repository database under a fresh home, holding one snapshot.
    fn fixture(tag: &str) -> Fixture {
        let home = tmpdir(tag);
        let db = store::db_path(&home, "r");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = store::open_live(&db).unwrap();
        let sid = edge_fixtures::mk_snap(&conn);
        Fixture { home, conn, sid }
    }

    fn db_of(f: &Fixture) -> PathBuf {
        store::db_path(&f.home, "r")
    }

    fn file(f: &Fixture, path: &str, language: &str) -> i64 {
        store::insert_file(
            &f.conn,
            f.sid,
            &FileRow {
                path: path.to_string(),
                language: language.to_string(),
                bytes: 1,
                digest: "d".to_string(),
                errors: 0,
            },
        )
        .unwrap()
    }

    /// A definition whose span starts at `byte_start` and whose line is unique
    /// per span, so hub order never depends on a tie.
    fn def(f: &Fixture, file_id: i64, name: &str, kind: &str, byte_start: usize) {
        let mut symbol = store_fixtures::sample_symbol(file_id, name);
        symbol.kind = kind.to_string();
        symbol.byte_start = byte_start;
        symbol.byte_end = byte_start + 5;
        symbol.start_line = byte_start as u32 + 1;
        store::insert_symbol(&f.conn, f.sid, &symbol).unwrap();
    }

    fn edge(
        f: &Fixture,
        caller_path: &str,
        callee: EdgeEndpoint,
        kind: &str,
        precision: &str,
    ) -> EdgeRow {
        let mut row = store_fixtures::stored_edge(
            Some(store_fixtures::syntax_endpoint(caller_path, 0)),
            Some(callee),
            kind,
            precision,
            None,
        );
        row.snapshot_id = f.sid;
        row.caller_path = caller_path.to_string();
        row
    }

    /// Proven `CALLS` from a file into the syntax definition at `byte` of `callee_path`.
    fn proven(f: &Fixture, caller_path: &str, callee_path: &str, byte: usize) -> EdgeRow {
        edge(
            f,
            caller_path,
            store_fixtures::syntax_endpoint(callee_path, byte),
            "CALLS",
            "Exact",
        )
    }

    /// Syntax-only call by name: no callee identity, never proven.
    fn candidate(f: &Fixture, caller_path: &str, name: &str) -> EdgeRow {
        let mut row = store_fixtures::stored_edge(
            Some(store_fixtures::syntax_endpoint(caller_path, 0)),
            None,
            "POTENTIAL_CALL",
            "Heuristic",
            None,
        );
        row.snapshot_id = f.sid;
        row.caller_path = caller_path.to_string();
        row.callee_name = name.to_string();
        row
    }

    fn insert(f: &Fixture, rows: &[EdgeRow]) {
        for row in rows {
            store::insert_edge(&f.conn, row).unwrap();
        }
    }

    fn certify(f: &Fixture) {
        store::mark_graph_proof(&f.conn, f.sid).unwrap();
    }

    fn report_with(f: &Fixture, depth: usize, limits: Limits) -> Value {
        let map = collect_with(&f.home, "r", &f.home, depth, limits, || {}).unwrap();
        serde_json::from_str(&render(&map).unwrap()).unwrap()
    }

    fn report(f: &Fixture, depth: usize) -> Value {
        serde_json::from_str(&render(&collect(&f.home, "r", &f.home, depth).unwrap()).unwrap())
            .unwrap()
    }

    fn has_action(v: &Value, kind: &str) -> bool {
        v["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == kind)
    }

    /// Every reason of one action kind, in report order.
    fn reasons_of(v: &Value, kind: &str) -> Vec<String> {
        v["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|action| action["kind"] == kind)
            .map(|action| action["reason"].as_str().unwrap().to_string())
            .collect()
    }

    /// The `omitted_count` of one omission entry, if the report has it.
    fn omission(v: &Value, section: &str, reason: &str) -> Option<Value> {
        v["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["section"] == section && entry["reason"] == reason)
            .map(|entry| entry["omitted_count"].clone())
    }

    fn area<'a>(v: &'a Value, name: &str) -> &'a Value {
        v["areas"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["area"] == name)
            .unwrap_or_else(|| panic!("no area {name}"))
    }

    /// Cross-area proven fixture with a name candidate in a third area; the
    /// `CALLS` row is omitted in the control, which must not be proven.
    fn read_cap_fixture(tag: &str, with_call: bool) -> Fixture {
        let f = fixture(tag);
        file(&f, "a/first.py", "python");
        file(&f, "a/target.py", "python");
        file(&f, "z/last.py", "python");
        let mut rows = vec![
            candidate(&f, "a/first.py", "x"),
            candidate(&f, "a/first.py", "y"),
            candidate(&f, "a/first.py", "w"),
        ];
        if with_call {
            rows.push(proven(&f, "z/last.py", "a/target.py", 0));
        }
        insert(&f, &rows);
        certify(&f);
        f
    }

    /// Graph with a cross-area proven call, a name match and a `main` entry
    /// point: every graph-derived section has something to withhold.
    fn graph_fixture(tag: &str) -> Fixture {
        let f = fixture(tag);
        file(&f, "crates/a/x.py", "python");
        let y = file(&f, "crates/b/y.py", "python");
        let main = file(&f, "crates/a/main.py", "python");
        def(&f, y, "helper", "function", 0);
        def(&f, main, "main", "function", 0);
        insert(
            &f,
            &[
                proven(&f, "crates/a/x.py", "crates/b/y.py", 0),
                candidate(&f, "crates/a/x.py", "helper"),
            ],
        );
        certify(&f);
        f
    }

    #[test]
    fn area_of_fixed_expectations() {
        assert_eq!(area_of("crates/cli/src/main.rs", 2), "crates/cli");
        assert_eq!(area_of("crates/cli/src/main.rs", 1), "crates");
        assert_eq!(area_of("src/a.rs", 2), "src");
        assert_eq!(area_of("main.py", 2), ".");
        assert_eq!(area_of(r"a\b\c.rs", 1), "a");
    }

    #[test]
    fn map_reads_proven_edge_after_read_cap() {
        let f = read_cap_fixture("map-read-cap", true);
        let v = report_with(&f, 2, Limits { edges_read: 2 });
        assert_eq!(
            v["links"],
            json!([{"from":"z","to":"a","kind":"CALLS","count":1}])
        );
        assert_eq!(v["completeness"]["proven_read"], 1);
        assert_eq!(v["completeness"]["proven_total"], 1);
        assert_eq!(v["completeness"]["proven_complete"], true);
        assert_eq!(v["completeness"]["edges_complete"], false);
        assert_eq!(omission(&v, "edges", "item_limit"), Some(json!(2)));
        assert!(!has_action(&v, "setup_semantic_provider"));

        // Control: without the proven row the same read cap gives the setup action.
        let control = read_cap_fixture("map-read-cap-control", false);
        let w = report_with(&control, 2, Limits { edges_read: 2 });
        assert_eq!(w["completeness"]["proven_total"], 0);
        assert!(has_action(&w, "setup_semantic_provider"));
    }

    #[test]
    fn map_partial_proven_read_is_labelled() {
        let f = fixture("map-partial");
        file(&f, "a/target.py", "python");
        file(&f, "z/one.py", "python");
        file(&f, "z/two.py", "python");
        file(&f, "z/three.py", "python");
        insert(
            &f,
            &[
                proven(&f, "z/one.py", "a/target.py", 0),
                proven(&f, "z/two.py", "a/target.py", 10),
                proven(&f, "z/three.py", "a/target.py", 20),
            ],
        );
        certify(&f);
        let v = report_with(&f, 2, Limits { edges_read: 2 });
        assert_eq!(v["completeness"]["proven_read"], 2);
        assert_eq!(v["completeness"]["proven_total"], 3);
        assert_eq!(v["completeness"]["proven_complete"], false);
        assert!(reasons_of(&v, "review_freshness").iter().any(|reason| reason
            == "the snapshot holds more proven relations than one report reads (2 of 3); links and hubs are partial"));
        assert!(!has_action(&v, "setup_semantic_provider"));
    }

    #[test]
    fn map_groups_files_and_languages() {
        let f = fixture("map-groups");
        file(&f, "crates/a/src/x.rs", "rust");
        file(&f, "crates/a/src/y.py", "python");
        file(&f, "crates/b/z.rs", "rust");
        file(&f, "main.py", "python");
        certify(&f);
        let v = report(&f, 2);
        let names: Vec<&str> = v["areas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["area"].as_str().unwrap())
            .collect();
        assert_eq!(names, [".", "crates/a", "crates/b"]);
        let files: Vec<&Value> = v["areas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| &entry["files"])
            .collect();
        assert_eq!(files, [&json!(1), &json!(2), &json!(1)]);
        assert_eq!(area(&v, "crates/a")["languages"], json!(["python", "rust"]));
    }

    #[test]
    fn map_links_cross_area_proven_only() {
        let f = fixture("map-links");
        file(&f, "crates/a/src/one.rs", "rust");
        file(&f, "crates/a/src/two.rs", "rust");
        file(&f, "crates/b/lib.rs", "rust");
        insert(
            &f,
            &[
                proven(&f, "crates/a/src/one.rs", "crates/b/lib.rs", 0),
                proven(&f, "crates/a/src/one.rs", "crates/a/src/two.rs", 0),
                edge(
                    &f,
                    "crates/a/src/one.rs",
                    store_fixtures::syntax_endpoint("crates/b/lib.rs", 10),
                    "CALLS",
                    "Heuristic",
                ),
            ],
        );
        certify(&f);
        let v = report(&f, 2);
        assert_eq!(
            v["links"],
            json!([{"from":"crates/a","to":"crates/b","kind":"CALLS","count":1}])
        );
        assert_eq!(area(&v, "crates/a")["proven_internal"], 1);
        assert_eq!(area(&v, "crates/a")["proven_out"], 1);
        assert_eq!(area(&v, "crates/b")["proven_in"], 1);
    }

    #[test]
    fn map_scip_callee_attributed_by_bridge() {
        let caller_src: &[u8] = b"def caller():\n    return target()\n";
        let lib_src: &[u8] = b"def target():\n    return 1\n";
        let caller_digest = store::content_digest(caller_src);
        let lib_digest = store::content_digest(lib_src);
        let f = fixture("map-bridge");
        edge_fixtures::ingest_py(&f.conn, f.sid, "crates/a/main.py", caller_src);
        edge_fixtures::ingest_py(&f.conn, f.sid, "crates/b/lib.py", lib_src);
        let import = edge_fixtures::mk_import_full(
            &f.conn,
            f.sid,
            "scip-import",
            "definitions,references",
            "UTF8",
            "9f2c3a4b5d6e7f80",
        );
        let target = "scip-test test test-pkg 1.0.0 lib.py/target().";
        let caller = "scip-test test test-pkg 1.0.0 main.py/caller().";
        store::insert_semantic_facts(
            &f.conn,
            &[
                edge_fixtures::mk_fact(
                    f.sid,
                    import,
                    "crates/b/lib.py",
                    &lib_digest,
                    Some(target),
                    "target",
                    0x1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                edge_fixtures::mk_fact(
                    f.sid,
                    import,
                    "crates/a/main.py",
                    &caller_digest,
                    Some(caller),
                    "caller",
                    0x1,
                    (1, 5, 1, 11),
                    (4, 10),
                    false,
                ),
                edge_fixtures::mk_fact(
                    f.sid,
                    import,
                    "crates/a/main.py",
                    &caller_digest,
                    Some(target),
                    "target",
                    0x0,
                    (2, 12, 2, 18),
                    (25, 31),
                    false,
                ),
            ],
        )
        .unwrap();
        let stats = crate::edges::upsert_semantic_edges(&f.conn, f.sid, import).unwrap();
        assert_eq!(stats.calls, 1);
        certify(&f);
        let v = report(&f, 2);
        let cross = json!({"from":"crates/a","to":"crates/b","kind":"CALLS","count":1});
        assert!(
            v["links"].as_array().unwrap().contains(&cross),
            "bridged callee links to crates/b: {}",
            v["links"]
        );
        assert_eq!(v["completeness"]["unattributed_endpoints"], 0);

        // Control: the same callee with no bridge and no doc is unattributed.
        let g = fixture("map-unbridged");
        insert(
            &g,
            &[edge(
                &g,
                "crates/a/main.py",
                store_fixtures::scip_endpoint("unbridged"),
                "CALLS",
                "Exact",
            )],
        );
        certify(&g);
        let w = report(&g, 2);
        assert_eq!(w["completeness"]["unattributed_endpoints"], 1);
        assert_eq!(w["links"], json!([]));
    }

    #[test]
    fn candidate_links_name_match() {
        let f = fixture("map-candidate");
        file(&f, "crates/a/x.py", "python");
        let b = file(&f, "crates/b/y.py", "python");
        def(&f, b, "helper", "function", 0);
        insert(&f, &[candidate(&f, "crates/a/x.py", "helper")]);
        certify(&f);
        let v = report(&f, 2);
        assert_eq!(
            v["candidate_links"],
            json!([{"from":"crates/a","to":"crates/b","name_matches":1,"basis":"candidate-name"}])
        );
        assert_eq!(v["links"], json!([]));
    }

    #[test]
    fn candidate_ambiguous_name_is_not_linked() {
        let f = fixture("map-ambiguous");
        file(&f, "crates/a/x.py", "python");
        let b = file(&f, "crates/b/y.py", "python");
        let c = file(&f, "crates/c/z.py", "python");
        def(&f, b, "helper", "function", 0);
        def(&f, c, "helper", "function", 0);
        insert(&f, &[candidate(&f, "crates/a/x.py", "helper")]);
        certify(&f);
        let v = report(&f, 2);
        assert_eq!(v["candidate_links"], json!([]));
        assert_eq!(v["completeness"]["ambiguous_candidate_names"], 1);
    }

    #[test]
    fn map_mutual_calls_and_hub_are_bounded() {
        let f = fixture("map-hub");
        let many = file(&f, "crates/a/src/many.py", "python");
        let lib = file(&f, "crates/b/src/lib.py", "python");
        for i in 0..200 {
            def(&f, many, &format!("f{i}"), "function", i * 10);
        }
        def(&f, lib, "hub", "function", 0);
        for (i, name) in ["g0", "g1", "g2", "g3"].iter().enumerate() {
            def(&f, lib, name, "function", 10 * (i + 1));
        }
        let mut rows = Vec::new();
        for _ in 0..200 {
            rows.push(proven(&f, "crates/a/src/many.py", "crates/b/src/lib.py", 0));
        }
        for g in 0..4 {
            rows.push(proven(
                &f,
                "crates/a/src/many.py",
                "crates/b/src/lib.py",
                10 * (g + 1),
            ));
        }
        rows.push(proven(&f, "crates/b/src/lib.py", "crates/a/src/many.py", 0));
        insert(&f, &rows);
        certify(&f);
        let v = report(&f, 2);
        let links = v["links"].as_array().unwrap();
        assert!(
            links.contains(&json!({"from":"crates/a","to":"crates/b","kind":"CALLS","count":204}))
        );
        assert!(
            links.contains(&json!({"from":"crates/b","to":"crates/a","kind":"CALLS","count":1}))
        );
        let hubs = area(&v, "crates/b")["hubs"].as_array().unwrap().clone();
        assert_eq!(hubs.len(), 3);
        assert_eq!(
            hubs[0],
            json!({"path":"crates/b/src/lib.py","line":1,"col":1,"name":"hub","kind":"function","inbound_proven":200})
        );
        assert_eq!(omission(&v, "hubs", "item_limit"), Some(json!(2)));
        let first = render(&collect(&f.home, "r", &f.home, 2).unwrap()).unwrap();
        let second = render(&collect(&f.home, "r", &f.home, 2).unwrap()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn map_entry_points_main_and_conventional() {
        let f = fixture("map-entry");
        let main = file(&f, "src/main.rs", "rust");
        def(&f, main, "main", "function", 0);
        file(&f, "src/index.js", "javascript");
        certify(&f);
        let v = report(&f, 2);
        assert_eq!(
            area(&v, "src")["entry_points"],
            json!([
                {"path":"src/main.rs","line":1,"col":1,"name":"main","basis":"main-definition"},
                {"path":"src/index.js","line":null,"col":null,"name":null,"basis":"conventional-path"}
            ])
        );
    }

    #[test]
    fn map_caps_areas_at_50() {
        let f = fixture("map-caps");
        for i in 0..60 {
            file(&f, &format!("pkg{i:02}/f.py"), "python");
        }
        // The largest name has the most files, so it is kept despite its name.
        file(&f, "pkg59/extra.py", "python");
        certify(&f);
        let v = report(&f, 1);
        let names: Vec<String> = v["areas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["area"].as_str().unwrap().to_string())
            .collect();
        let mut expected: Vec<String> = (0..49).map(|i| format!("pkg{i:02}")).collect();
        expected.push("pkg59".to_string());
        assert_eq!(names, expected);
        assert_eq!(omission(&v, "areas", "item_limit"), Some(json!(10)));
    }

    #[test]
    fn map_no_index_is_empty_with_action() {
        let home = tmpdir("map-no-index");
        let v: Value =
            serde_json::from_str(&render(&collect(&home, "r", &home, 2).unwrap()).unwrap())
                .unwrap();
        assert_eq!(v["snapshot"]["status"], "no-index");
        assert_eq!(v["areas"], json!([]));
        assert_eq!(
            v["next_actions"],
            json!([{"kind":"build_index","reason":"create an index to see the area map"}])
        );
        assert!(!store::db_path(&home, "r").exists());
    }

    #[test]
    fn map_ignores_import_committed_after_pin() {
        let f = fixture("map-pin-import");
        file(&f, "z/one.py", "python");
        file(&f, "z/two.py", "python");
        file(&f, "a/target.py", "python");
        insert(&f, &[proven(&f, "z/one.py", "a/target.py", 0)]);
        certify(&f);
        let before = render(&collect(&f.home, "r", &f.home, 2).unwrap()).unwrap();
        let second_edge = proven(&f, "z/two.py", "a/target.py", 10);
        let pinned = collect_with(&f.home, "r", &f.home, 2, Limits::DEFAULT, || {
            let mut writer = store::open_live(&db_of(&f)).unwrap();
            let tx = writer.transaction().unwrap();
            store::insert_edge(&tx, &second_edge).unwrap();
            store::meta_set(&tx, "semantic_generation", "1").unwrap();
            store::mark_graph_proof(&tx, f.sid).unwrap();
            tx.commit().unwrap();
        })
        .unwrap();
        assert_eq!(render(&pinned).unwrap(), before);

        // Control: a read after the commit sees the second edge and the new generation.
        let after = report(&f, 2);
        assert_eq!(after["snapshot"]["generation"], 1);
        assert_eq!(after["completeness"]["proven_total"], 2);
    }

    #[test]
    fn map_ignores_rebuild_committed_after_pin() {
        let f = fixture("map-pin-rebuild");
        file(&f, "z/one.py", "python");
        file(&f, "a/target.py", "python");
        insert(&f, &[proven(&f, "z/one.py", "a/target.py", 0)]);
        certify(&f);
        let before = render(&collect(&f.home, "r", &f.home, 2).unwrap()).unwrap();
        let pinned = collect_with(&f.home, "r", &f.home, 2, Limits::DEFAULT, || {
            let mut writer = store::open_live(&db_of(&f)).unwrap();
            let tx = writer.transaction().unwrap();
            tx.execute("DELETE FROM code_edges WHERE snapshot_id = ?1", [f.sid])
                .unwrap();
            tx.execute(
                "UPDATE snapshots SET nonce = 'rebuilt-nonce' WHERE id = ?1",
                [f.sid],
            )
            .unwrap();
            store::insert_file(
                &tx,
                f.sid,
                &FileRow {
                    path: "q/new.py".to_string(),
                    language: "python".to_string(),
                    bytes: 1,
                    digest: "d".to_string(),
                    errors: 0,
                },
            )
            .unwrap();
            store::insert_edge(&tx, &proven(&f, "q/new.py", "a/target.py", 0)).unwrap();
            store::mark_graph_proof(&tx, f.sid).unwrap();
            tx.commit().unwrap();
        })
        .unwrap();
        assert_eq!(render(&pinned).unwrap(), before);

        let after = report(&f, 2);
        assert_eq!(after["snapshot"]["nonce"], "rebuilt-nonce");
        assert_eq!(after["graph_proof"], "current");
        assert_eq!(
            after["links"],
            json!([{"from":"q","to":"a","kind":"CALLS","count":1}])
        );
    }

    #[test]
    fn map_withholds_graph_without_proof_token() {
        let f = graph_fixture("map-withheld");
        let control = report(&f, 2);
        assert_eq!(control["graph_proof"], "current");
        assert_eq!(control["links"].as_array().unwrap().len(), 1);
        assert_eq!(control["candidate_links"].as_array().unwrap().len(), 1);

        store::open_live(&db_of(&f))
            .unwrap()
            .execute("DELETE FROM meta WHERE key = 'graph_proof'", [])
            .unwrap();
        let v = report(&f, 2);
        assert_eq!(v["graph_proof"], "incompatible");
        assert_eq!(v["links"], json!([]));
        assert_eq!(v["candidate_links"], json!([]));
        assert_eq!(v["flows"], json!([]));
        for entry in v["areas"].as_array().unwrap() {
            assert_eq!(entry["hubs"], json!([]));
            for key in [
                "proven_in",
                "proven_out",
                "proven_internal",
                "candidate_internal",
                "unresolved_calls",
                "imports",
            ] {
                assert!(entry[key].is_null(), "{key} is withheld");
            }
        }
        for (key, value) in v["completeness"].as_object().unwrap() {
            assert!(value.is_null(), "completeness.{key} is withheld");
        }
        assert_eq!(
            v["omissions"],
            json!([{"section":"graph","reason":"graph_proof_incompatible","omitted_count":null}])
        );
        // Everything that does not come from code_edges is still present.
        assert_eq!(area(&v, "crates/a")["files"], 2);
        assert_eq!(area(&v, "crates/b")["definitions"], 1);
        assert_eq!(
            area(&v, "crates/a")["entry_points"][0]["basis"],
            "main-definition"
        );
        assert!(has_action(&v, "refresh_index"));
        assert!(!has_action(&v, "setup_semantic_provider"));
        assert!(!reasons_of(&v, "review_freshness")
            .iter()
            .any(|reason| reason.contains("more proven relations")));
        assert_eq!(store::meta_get(&f.conn, "graph_proof").unwrap(), None);
    }

    #[test]
    fn map_withholds_graph_with_stale_proof_token() {
        let f = graph_fixture("map-stale-token");
        let control = report(&f, 2);
        let nonce = store::latest_snapshot(&f.conn).unwrap().unwrap().nonce;
        store::meta_set(
            &f.conn,
            "graph_proof",
            &format!("m6-task7-source-proof-1:{nonce}:0"),
        )
        .unwrap();
        let v = report(&f, 2);
        assert_eq!(v["graph_proof"], "incompatible");
        assert_eq!(v["links"], json!([]));

        store::mark_graph_proof(&store::open_live(&db_of(&f)).unwrap(), f.sid).unwrap();
        let w = report(&f, 2);
        assert_eq!(w["graph_proof"], "current");
        assert_eq!(w["links"], control["links"]);
    }

    #[test]
    fn map_counts_verification_before_import_cap() {
        let f = fixture("map-imports");
        let root = f.home.canonicalize().unwrap();
        f.conn
            .execute(
                "UPDATE snapshots SET root = ?1, config_fingerprint = ?2 WHERE id = ?3",
                rusqlite::params![
                    root.to_string_lossy(),
                    crate::index::config_fingerprint(&root),
                    f.sid
                ],
            )
            .unwrap();
        let snapshot = store::latest_snapshot(&f.conn).unwrap().unwrap();
        let live = crate::index::live_state_for(&f.conn, &snapshot).unwrap();
        for i in 1..=25 {
            let content_identity = if i == 25 {
                "moved-identity".to_string()
            } else {
                live.content_identity.clone()
            };
            store::insert_semantic_import(
                &f.conn,
                &store::SemanticImport {
                    content_identity,
                    source_revision: crate::semantic::revision_string(&live),
                    ..store_fixtures::sample_semantic_import(f.sid)
                },
            )
            .unwrap();
        }
        let v = report(&f, 2);
        assert_eq!(v["imports"].as_array().unwrap().len(), 20);
        assert_eq!(v["import_verification"]["total"], 25);
        assert_eq!(
            v["import_verification"]["by_verification"],
            json!([
                {"verification":"Stale","count":1},
                {"verification":"Unverified","count":24}
            ])
        );
        assert_eq!(omission(&v, "imports", "item_limit"), Some(json!(5)));
        assert!(reasons_of(&v, "review_freshness").iter().any(|reason| reason
            == "one or more semantic imports are Stale; relations from them may not match the source"));
    }

    #[test]
    fn is_proven_follows_the_stored_predicate() {
        let f = fixture("map-is-proven");
        let call = proven(&f, "a.py", "b.py", 0);
        assert!(is_proven(&call));
        let mut heuristic = call.clone();
        heuristic.precision = "Heuristic".to_string();
        assert!(!is_proven(&heuristic));
        let mut unresolved = call.clone();
        unresolved.unresolved = Some("dynamic".to_string());
        assert!(!is_proven(&unresolved));
        assert!(!is_proven(&candidate(&f, "a.py", "x")));
    }
}
