// SPDX-License-Identifier: Apache-2.0
//! `checks`: test candidates and coverage limits for one git change (stage 6).
//! One read of the index, one transaction, no test run. Every output is a
//! suggestion from git changes, stored relations and file names.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::changes::{self, DefOut, Failure, FileOut, Request, Scope, Structure};
use crate::provider_setup::{self, Shell};
use crate::{edges, git, index, map, overview, semantic, store};

const SCHEMA_VERSION: u32 = 1;
const MAX_LISTED_DEFINITIONS: usize = 200;
const MAX_DEFINITIONS: usize = 20;
const MAX_AREA_CHANGED: usize = 20;
const MAX_AREA_DEPENDENTS: usize = 10;
const MAX_DEPENDENT_REASONS: usize = 3;
const DEPTH: usize = 3;
const TOTAL_TRAVERSAL_MS: u64 = 10_000;
const MAX_CANDIDATES: usize = 50;
const MAX_REASONS: usize = 5;
const MAX_AREAS: usize = 30;
const MAX_LIMIT_PATHS: usize = 20;
const MAX_INSTRUCTION_FILES: usize = 20;
const MAX_QUOTED_LINES: usize = 20;
const MAX_REPORT_BYTES: usize = 262_144;
/// D5: the instruction files read at the root and in each ancestor directory of a changed file.
const INSTRUCTION_FILES: [&str; 6] = [
    "README.md",
    "AGENTS.md",
    "CLAUDE.md",
    "CONTRIBUTING.md",
    "BUILDING.md",
    "TESTING.md",
];
const MAX_INSTRUCTION_BYTES: u64 = 262_144;
const MAX_INSTRUCTION_TEXT_BYTES: usize = 200;
const DISCLAIMER: &str = "Suggestions from git changes, stored relations and file names; riltra ran no test, and an empty or short list does not mean no test is needed. The project's required checks still apply.";

/// What the index side of one report could read (D0, D6 group 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexState {
    Missing,
    Outdated,
    Ahead,
    Ready,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub repo: String,
    pub scope: &'static str,
    pub base: Option<String>,
    pub head: String,
    pub snapshot: Option<SnapshotRef>,
    pub index_freshness: IndexFreshness,
    pub graph_proof: &'static str,
    pub changed: Changed,
    pub review_areas: Vec<ReviewArea>,
    pub test_candidates: Vec<TestCandidate>,
    pub instructions: Vec<Instruction>,
    pub coverage_limits: Vec<CoverageLimit>,
    pub omissions: Vec<Omission>,
    pub next_actions: Vec<NextAction>,
    pub bounds: Bounds,
    pub disclaimer: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SnapshotRef {
    pub nonce: String,
    pub generation: i64,
}

#[derive(Debug, Serialize)]
pub struct IndexFreshness {
    pub status: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Changed {
    pub total_files: usize,
    pub listed_files: usize,
    pub total_definitions: usize,
    pub listed_definitions: usize,
    pub traversed_definitions: usize,
    pub definitions: Vec<DefinitionRow>,
}

#[derive(Debug, Serialize)]
pub struct DefinitionRow {
    pub path: String,
    pub name: String,
    pub kind: &'static str,
    pub change: &'static str,
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ReviewArea {
    pub area: String,
    pub changed_files: Vec<String>,
    pub dependent_files: Option<Vec<DependentFile>>,
    pub dependent_count: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct DependentFile {
    pub path: String,
    pub reasons: Vec<Reason>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reason {
    pub source: Source,
    pub basis: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub source_type: &'static str,
    pub path: String,
    pub name: Option<String>,
    pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestCandidate {
    pub path: String,
    pub area: String,
    pub state: &'static str,
    pub basis: &'static str,
    pub reasons: Vec<Reason>,
}

#[derive(Debug, Serialize)]
pub struct Instruction {
    pub path: String,
    pub line: usize,
    pub text: String,
    pub state: &'static str,
    pub basis: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CoverageLimit {
    pub limit: &'static str,
    pub count: usize,
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Omission {
    pub section: &'static str,
    pub reason: &'static str,
    pub omitted_count: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct NextAction {
    pub reason: String,
    pub command: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Bounds {
    pub changed_files: usize,
    pub listed_definitions: usize,
    pub traversed_definitions: usize,
    pub depth: usize,
    pub traversal_total_ms: u64,
    pub traversal_ms: u64,
    pub test_candidates: usize,
    pub candidate_reasons: usize,
    pub instruction_files: usize,
    pub quoted_lines: usize,
    pub review_areas: usize,
    pub area_changed_files: usize,
    pub area_dependent_files: usize,
    pub dependent_reasons: usize,
    pub limit_paths: usize,
    pub json_bytes: usize,
}

const CONFIG_EXTENSIONS: [&str; 10] = [
    "toml",
    "json",
    "yaml",
    "yml",
    "ini",
    "cfg",
    "lock",
    "xml",
    "gradle",
    "properties",
];

/// Index facts of a readable snapshot, all read on the report's one transaction (D0).
struct Present {
    snapshot: store::Snapshot,
    revision: store::PromoteBase,
    freshness: String,
    freshness_reasons: Vec<String>,
    graph_current: bool,
    has_import: bool,
    files: Vec<store::FileRow>,
}

/// The index side as the report reads it: present, or absent with the limit that says why (D6 item 1).
enum View {
    Absent {
        status: &'static str,
        reason: Option<&'static str>,
        limit: &'static str,
    },
    Present(Box<Present>),
}

/// One `changed.definitions` row (D1), with its snapshot target while it is still to traverse.
struct Row {
    path: String,
    name: String,
    kind: &'static str,
    change: &'static str,
    status: &'static str,
    target: Option<Target>,
}

/// A definition the index holds exactly once: its snapshot path, name, kind and byte span.
struct Target {
    path: String,
    name: String,
    kind: &'static str,
    span: (usize, usize),
}

/// What one snapshot-definition traversal (D2) produced.
enum Walk {
    Analysed {
        /// Dependent file, basis (D10 name) and distance, one per traversal path.
        paths: Vec<(String, &'static str, usize)>,
        partial: bool,
    },
    AnchorUnavailable,
    TimeBudget,
}

#[derive(Default)]
struct AreaBuild {
    changed: Vec<String>,
    dependents: Vec<(String, Vec<Reason>)>,
}

#[derive(Default)]
struct Omissions(BTreeMap<(&'static str, &'static str), Option<usize>>);

impl Omissions {
    fn add_count(&mut self, section: &'static str, reason: &'static str, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(total) = self.0.entry((section, reason)).or_insert(Some(0)) {
            *total += count;
        }
    }

    /// Merges rows from another part of the report; a `null` count stays `null`.
    fn absorb(&mut self, rows: Vec<Omission>) {
        for row in rows {
            match row.omitted_count {
                Some(count) => self.add_count(row.section, row.reason, count),
                None => {
                    self.0.insert((row.section, row.reason), None);
                }
            }
        }
    }

    /// The rows in their fixed order (section, then reason).
    fn rows(&self) -> Vec<Omission> {
        self.0
            .iter()
            .map(|((section, reason), omitted_count)| Omission {
                section,
                reason,
                omitted_count: *omitted_count,
            })
            .collect()
    }
}

/// Rank of a D10 basis: the strongest reason of a candidate or dependent gives its basis.
/// `explicit-instruction` never reaches a reason (instructions are not ranked), so it has no
/// rank here; a basis without a rank is an internal fault, not a guess.
fn basis_rank(basis: &str) -> u8 {
    match basis {
        "proven-edge" => 0,
        "unproven-edge" => 1,
        "name-candidate" => 2,
        "path-convention" => 3,
        other => {
            unreachable!("reason basis '{other}' has no rank: reasons carry only ranked bases")
        }
    }
}

fn best_rank(reasons: &[Reason]) -> u8 {
    reasons
        .iter()
        .map(|reason| basis_rank(reason.basis))
        .min()
        .unwrap_or(u8::MAX)
}

/// D10 reason order: basis rank, source path, source name (null first), kind, detail.
fn reason_key(reason: &Reason) -> (u8, &str, Option<&str>, Option<&str>, &str) {
    (
        basis_rank(reason.basis),
        reason.source.path.as_str(),
        reason.source.name.as_deref(),
        reason.source.kind.as_deref(),
        reason.detail.as_str(),
    )
}

fn normalise(mut reasons: Vec<Reason>) -> Vec<Reason> {
    reasons.sort_by(|a, b| reason_key(a).cmp(&reason_key(b)));
    reasons.dedup();
    reasons
}

/// Maps a traversal basis to its D10 name. An unknown basis is an internal fault, never a guess.
fn basis_of(raw: &str) -> Result<&'static str, String> {
    match raw {
        "proven" => Ok("proven-edge"),
        "unproven" => Ok("unproven-edge"),
        "candidate-name" => Ok("name-candidate"),
        other => Err(format!("unknown traversal basis '{other}'")),
    }
}

/// The file name of a `/`-separated path, without its last extension.
fn base_name(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(base, _)| base)
}

/// The D4 names a test file may carry for a changed file `dir/stem.ext`.
fn same_stem_names(path: &str) -> Vec<String> {
    let stem = base_name(path);
    vec![
        stem.to_string(),
        format!("test_{stem}"),
        format!("{stem}_test"),
        format!("{stem}.test"),
        format!("{stem}.spec"),
        format!("{stem}Test"),
    ]
}

/// D6 item 6: a manifest name, a configuration extension, or anything under `.github/`.
fn is_config(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    overview::is_manifest(path)
        || extension.is_some_and(|ext| CONFIG_EXTENSIONS.contains(&ext))
        || path.starts_with(".github/")
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Unstaged => "unstaged",
        Scope::Staged => "staged",
        Scope::All => "all",
        Scope::Compare => "compare",
    }
}

fn flag(name: &'static str) -> CoverageLimit {
    CoverageLimit {
        limit: name,
        count: 0,
        paths: Vec::new(),
    }
}

/// A limit over paths: `count` is every path, `paths` the first 20 in sort order (D6).
fn paths_limit(
    omissions: &mut Omissions,
    name: &'static str,
    paths: BTreeSet<String>,
) -> CoverageLimit {
    let count = paths.len();
    omissions.add_count(
        "limit_paths",
        "item_limit",
        count.saturating_sub(MAX_LIMIT_PATHS),
    );
    CoverageLimit {
        limit: name,
        count,
        paths: paths.into_iter().take(MAX_LIMIT_PATHS).collect(),
    }
}

/// Reads the index side of one report on the given transaction (D0). Every storage
/// failure returns `Err`; a freshness check that cannot read its inputs is a reason, not a failure.
fn read_view(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    canon: &Path,
    fingerprint: &dyn Fn(&Path) -> git::GitInfo,
) -> Result<View, String> {
    let Some(snapshot) = store::latest_snapshot(tx)? else {
        return Ok(View::Absent {
            status: "unknown",
            reason: Some("no-snapshot"),
            limit: "no-snapshot",
        });
    };
    let graph_current = store::graph_proof_current(tx, snapshot.id)?;
    let (freshness, mut freshness_reasons) =
        index::evaluate_snapshot_with(tx, repo, canon, &snapshot, fingerprint);
    freshness_reasons.sort();
    freshness_reasons.dedup();
    let has_import = !store::semantic_imports_for(tx, snapshot.id)?.is_empty();
    let revision = store::live_base(tx)?;
    let files = store::files_of(tx, snapshot.id)?;
    Ok(View::Present(Box::new(Present {
        snapshot,
        revision,
        freshness,
        freshness_reasons,
        graph_current,
        has_import,
        files,
    })))
}

/// D1 for one definition row: its status, and the snapshot target when the index holds it once.
fn resolve_row(
    file: &FileOut,
    def: &DefOut,
    snapshot_tx: Option<(&rusqlite::Transaction<'_>, i64)>,
) -> Result<Row, String> {
    let mut row = Row {
        path: file.path.clone(),
        name: def.name.clone(),
        kind: def.kind,
        change: def.change,
        status: "added",
        target: None,
    };
    let Some(key) = &def.lookup else {
        return Ok(row);
    };
    if key.2 == "field" {
        row.status = "data-member";
        return Ok(row);
    }
    let Some((tx, sid)) = snapshot_tx else {
        row.status = "index-unavailable";
        return Ok(row);
    };
    let spans = changes::symbol_rows(tx, sid, key).map_err(|e| e.to_string())?;
    match spans.as_slice() {
        [] => row.status = "not-in-index",
        [(start, end)] => {
            row.status = "pending";
            row.target = Some(Target {
                path: key.0.clone(),
                name: key.1.clone(),
                kind: key.2,
                span: (*start, *end),
            });
        }
        _ => row.status = "ambiguous-in-index",
    }
    Ok(row)
}

/// One snapshot-definition traversal (D2). The anchor comes from the snapshot row, so
/// the live source is never read for it.
fn walk(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    canon: &Path,
    snapshot: &store::Snapshot,
    target: &Target,
    started: Instant,
) -> Result<Walk, String> {
    let Ok(rel) = semantic::validate_snapshot_rel(&target.path) else {
        return Ok(Walk::AnchorUnavailable);
    };
    if store::file_identity(tx, snapshot.id, &rel)?.is_none() {
        return Ok(Walk::AnchorUnavailable);
    }
    let anchor = index::snapshot_def_anchor(
        &rel,
        &target.name,
        target.kind,
        target.span.0,
        target.span.1,
    );
    let request = index::TraceRequest {
        source: index::SpanQuery {
            rel: &rel,
            line1: 0,
            col1: 0,
            refs: false,
        },
        symbol: None,
        destination: None,
        depth: DEPTH,
        limit: index::MAX_SEARCH_LIMIT,
        offset: 0,
        summary: false,
        include_tests: false,
    };
    match index::graph_query_from_anchor_tx(
        tx,
        repo,
        canon,
        snapshot.clone(),
        anchor,
        &request,
        started,
        true,
        true,
    ) {
        Ok(out) => {
            let partial =
                out.completeness.status == "partial" || out.impact_verdict == Some("UNKNOWN");
            let mut paths = Vec::new();
            for path in &out.paths {
                let hops: Vec<store::EdgeRow> =
                    path.edges.iter().map(|hit| hit.edge.clone()).collect();
                if hops.is_empty() {
                    continue;
                }
                paths.push((
                    index::dependent_file(&hops),
                    basis_of(path.basis)?,
                    hops.len(),
                ));
            }
            Ok(Walk::Analysed { paths, partial })
        }
        Err(index::GraphQueryError::Anchor(_)) => Ok(Walk::AnchorUnavailable),
        Err(index::GraphQueryError::Store(message)) if message == "time-budget" => {
            Ok(Walk::TimeBudget)
        }
        Err(index::GraphQueryError::Store(message)) => Err(message),
    }
}

/// The test-candidate and coverage-limit report of one git change (D0-D10), built on
/// one index read. `tx` is `None` only when no index was opened; `read` says why.
pub(crate) fn assemble(
    tx: Option<&rusqlite::Transaction<'_>>,
    repo: &str,
    structure: &Structure,
    req: &Request,
    read: IndexState,
    fingerprint: &dyn Fn(&Path) -> git::GitInfo,
) -> Result<Report, String> {
    let view = match (read, tx) {
        (IndexState::Missing, _) => View::Absent {
            status: "no-index",
            reason: None,
            limit: "no-index",
        },
        (IndexState::Outdated, _) => View::Absent {
            status: "unknown",
            reason: Some("schema-outdated"),
            limit: "schema-outdated",
        },
        (IndexState::Ahead, _) => View::Absent {
            status: "unknown",
            reason: Some("schema-ahead"),
            limit: "schema-ahead",
        },
        (IndexState::Ready, Some(tx)) => read_view(tx, repo, &structure.canon, fingerprint)?,
        (IndexState::Ready, None) => {
            return Err("internal: a ready index needs its read transaction".to_string())
        }
    };
    let present: Option<&Present> = match &view {
        View::Present(p) => Some(p.as_ref()),
        View::Absent { .. } => None,
    };
    let snapshot_tx = match (tx, present) {
        (Some(tx), Some(p)) => Some((tx, p.snapshot.id)),
        _ => None,
    };
    let graph = match (tx, present) {
        (Some(tx), Some(p)) if p.graph_current => Some((tx, p)),
        _ => None,
    };
    let graph_ran = graph.is_some();
    let mut omissions = Omissions::default();

    // D1: every definition of an established file, in changes order.
    let mut rows: Vec<Row> = Vec::new();
    for file in structure.files.iter().filter(|f| f.established) {
        for def in &file.defs {
            rows.push(resolve_row(file, def, snapshot_tx)?);
        }
    }

    // D2 and D3: traversals from the first 20 resolvable definitions, under the total budget.
    let mut test_reasons: BTreeMap<String, Vec<Reason>> = BTreeMap::new();
    let mut dependent_reasons: BTreeMap<String, Vec<Reason>> = BTreeMap::new();
    let mut partial_paths: BTreeSet<String> = BTreeSet::new();
    let total_started = Instant::now();
    let mut resolvable = 0usize;
    let mut traversed = 0usize;
    let mut traversal_limit = 0usize;
    let mut time_budget = 0usize;
    for row in rows.iter_mut() {
        let Some(target) = row.target.take() else {
            continue;
        };
        let Some((tx, p)) = graph else {
            row.status = "not-traversed";
            continue;
        };
        resolvable += 1;
        if resolvable > MAX_DEFINITIONS {
            row.status = "not-traversed";
            traversal_limit += 1;
            continue;
        }
        if total_started.elapsed().as_millis() >= u128::from(TOTAL_TRAVERSAL_MS) {
            row.status = "time-budget";
            time_budget += 1;
            continue;
        }
        traversed += 1;
        match walk(
            tx,
            repo,
            &structure.canon,
            &p.snapshot,
            &target,
            Instant::now(),
        )? {
            Walk::Analysed { paths, partial } => {
                row.status = "analysed";
                if partial {
                    partial_paths.insert(row.path.clone());
                }
                for (dependent, basis, distance) in paths {
                    let reason = Reason {
                        source: Source {
                            source_type: "definition",
                            path: row.path.clone(),
                            name: Some(row.name.clone()),
                            kind: Some(row.kind.to_string()),
                        },
                        basis,
                        detail: format!("distance-{distance}"),
                    };
                    if index::is_test_path(&dependent) {
                        test_reasons.entry(dependent).or_default().push(reason);
                    } else {
                        dependent_reasons.entry(dependent).or_default().push(reason);
                    }
                }
            }
            Walk::AnchorUnavailable => row.status = "anchor-unavailable",
            Walk::TimeBudget => row.status = "time-budget",
        }
    }

    // D4: the file-name rule runs on every changed, parsed file, with or without a definition change.
    if let Some(p) = present {
        for file in structure
            .files
            .iter()
            .filter(|f| f.established && f.language.is_some())
        {
            let names = same_stem_names(&file.path);
            for stored in &p.files {
                if index::is_test_path(&stored.path)
                    && names.iter().any(|n| n == base_name(&stored.path))
                {
                    test_reasons
                        .entry(stored.path.clone())
                        .or_default()
                        .push(Reason {
                            source: Source {
                                source_type: "file",
                                path: file.path.clone(),
                                name: None,
                                kind: None,
                            },
                            basis: "path-convention",
                            detail: "same-stem".to_string(),
                        });
                }
            }
        }
    }

    // Test candidates: one per test path, strongest basis first, capped (D7).
    let mut candidates: Vec<TestCandidate> = test_reasons
        .into_iter()
        .map(|(path, reasons)| {
            let reasons = normalise(reasons);
            TestCandidate {
                area: map::area_of(&path, 2),
                state: "suggested",
                basis: reasons[0].basis,
                reasons,
                path,
            }
        })
        .collect();
    candidates.sort_by(|a, b| (basis_rank(a.basis), &a.path).cmp(&(basis_rank(b.basis), &b.path)));
    omissions.add_count(
        "test_candidates",
        "item_limit",
        candidates.len().saturating_sub(MAX_CANDIDATES),
    );
    candidates.truncate(MAX_CANDIDATES);
    for candidate in candidates.iter_mut() {
        let dropped = candidate.reasons.len().saturating_sub(MAX_REASONS);
        candidate.reasons.truncate(MAX_REASONS);
        omissions.add_count("candidate_reasons", "item_limit", dropped);
    }

    // D8: review areas, changed files and (only when the graph ran) non-test dependents.
    let mut areas: BTreeMap<String, AreaBuild> = BTreeMap::new();
    for file in &structure.files {
        areas
            .entry(map::area_of(&file.path, 2))
            .or_default()
            .changed
            .push(file.path.clone());
    }
    for (dependent, reasons) in dependent_reasons {
        areas
            .entry(map::area_of(&dependent, 2))
            .or_default()
            .dependents
            .push((dependent, normalise(reasons)));
    }
    let mut areas: Vec<(String, AreaBuild)> = areas.into_iter().collect();
    areas.sort_by_key(|(_, build)| build.changed.is_empty());
    omissions.add_count(
        "review_areas",
        "item_limit",
        areas.len().saturating_sub(MAX_AREAS),
    );
    areas.truncate(MAX_AREAS);
    let mut review_areas = Vec::new();
    for (area, build) in areas {
        let mut changed = build.changed;
        changed.sort();
        changed.dedup();
        omissions.add_count(
            "area_changed_files",
            "item_limit",
            changed.len().saturating_sub(MAX_AREA_CHANGED),
        );
        changed.truncate(MAX_AREA_CHANGED);
        let mut dependents = build.dependents;
        dependents.sort_by(|a, b| (best_rank(&a.1), &a.0).cmp(&(best_rank(&b.1), &b.0)));
        let count = dependents.len();
        omissions.add_count(
            "area_dependent_files",
            "item_limit",
            count.saturating_sub(MAX_AREA_DEPENDENTS),
        );
        let mut files = Vec::new();
        for (path, mut reasons) in dependents.into_iter().take(MAX_AREA_DEPENDENTS) {
            let dropped = reasons.len().saturating_sub(MAX_DEPENDENT_REASONS);
            reasons.truncate(MAX_DEPENDENT_REASONS);
            omissions.add_count("dependent_reasons", "item_limit", dropped);
            files.push(DependentFile { path, reasons });
        }
        review_areas.push(ReviewArea {
            area,
            changed_files: changed,
            dependent_files: graph_ran.then_some(files),
            dependent_count: graph_ran.then_some(count),
        });
    }

    // Changed definitions: the listing cap is counted apart from the traversal cap.
    let total_definitions = rows.len();
    let listed = total_definitions.min(MAX_LISTED_DEFINITIONS);
    omissions.add_count("definitions", "item_limit", total_definitions - listed);
    omissions.add_count("definitions", "traversal_limit", traversal_limit);
    omissions.add_count("definitions", "time_budget", time_budget);
    let definitions: Vec<DefinitionRow> = rows
        .iter()
        .take(listed)
        .map(|row| DefinitionRow {
            path: row.path.clone(),
            name: row.name.clone(),
            kind: row.kind,
            change: row.change,
            status: row.status,
        })
        .collect();

    // D6: coverage limits in the fixed order.
    let mut limits: Vec<CoverageLimit> = Vec::new();
    if let View::Absent { limit, .. } = &view {
        limits.push(flag(limit));
    }
    if let Some(p) = present {
        if p.freshness == "stale" {
            let reasons: BTreeSet<String> = p.freshness_reasons.iter().cloned().collect();
            limits.push(paths_limit(&mut omissions, "index-stale", reasons));
        }
        if !p.graph_current {
            limits.push(flag("graph-proof-incompatible"));
        }
        if !p.has_import {
            limits.push(flag("no-semantic-import"));
        }
    }
    if !partial_paths.is_empty() {
        limits.push(paths_limit(&mut omissions, "partial-graph", partial_paths));
    }
    let config: BTreeSet<String> = structure
        .files
        .iter()
        .filter(|f| is_config(&f.path))
        .map(|f| f.path.clone())
        .collect();
    if !config.is_empty() {
        limits.push(paths_limit(&mut omissions, "configuration-changed", config));
    }
    let refused: BTreeSet<String> = structure
        .files
        .iter()
        .filter(|f| f.reason.is_some())
        .map(|f| f.path.clone())
        .collect();
    if !refused.is_empty() {
        limits.push(paths_limit(&mut omissions, "file-not-analysed", refused));
    }
    let hunked: BTreeSet<String> = structure
        .files
        .iter()
        .filter(|f| !f.outside.is_empty())
        .map(|f| f.path.clone())
        .collect();
    if !hunked.is_empty() {
        limits.push(paths_limit(
            &mut omissions,
            "hunks-outside-definitions",
            hunked,
        ));
    }
    let added: BTreeSet<String> = rows
        .iter()
        .filter(|r| r.status == "added")
        .map(|r| r.path.clone())
        .collect();
    if !added.is_empty() {
        limits.push(paths_limit(&mut omissions, "added-definitions", added));
    }
    let not_analysed: BTreeSet<String> = rows
        .iter()
        .filter(|r| !matches!(r.status, "analysed" | "added"))
        .map(|r| r.path.clone())
        .collect();
    if !not_analysed.is_empty() {
        limits.push(paths_limit(
            &mut omissions,
            "definitions-not-analysed",
            not_analysed,
        ));
    }
    let budget: BTreeSet<String> = rows
        .iter()
        .filter(|r| r.status == "time-budget")
        .map(|r| r.path.clone())
        .collect();
    if !budget.is_empty() {
        limits.push(paths_limit(&mut omissions, "traversal-time-budget", budget));
    }
    if structure.total_files > structure.files.len() {
        limits.push(CoverageLimit {
            limit: "changes-truncated",
            count: structure.total_files - structure.files.len(),
            paths: Vec::new(),
        });
    }
    if candidates.is_empty() {
        limits.push(flag("no-test-candidates"));
    }

    // D9: next actions, in rule order, each at most once.
    let mut next_actions = Vec::new();
    let quoted = provider_setup::quote(Shell::current(), repo);
    let index_missing = matches!(&view, View::Absent { limit, .. } if *limit != "schema-ahead");
    let stale = present.is_some_and(|p| p.freshness == "stale");
    if index_missing || stale {
        next_actions.push(NextAction {
            reason: "the index is missing or stale, so stored facts may not match the worktree"
                .to_string(),
            command: Some(format!("riltra index --repo {quoted}")),
        });
    }
    if let Some(p) = present {
        if !p.graph_current {
            next_actions.push(NextAction {
                reason: "stored relations fail the graph-proof check, so no traversal ran; validate them before relying on dependents".to_string(),
                command: Some(format!("riltra index --full --repo {quoted}")),
            });
        }
        if !p.has_import {
            next_actions.push(NextAction {
                reason: "the snapshot has no semantic import, so no relation can be proven"
                    .to_string(),
                command: Some(format!("riltra semantic setup --repo {quoted}")),
            });
        }
    }
    next_actions.push(NextAction {
        reason: "run the checks your project requires; riltra does not run tests".to_string(),
        command: None,
    });

    let (snapshot, index_freshness, graph_proof) = match &view {
        View::Absent { status, reason, .. } => (
            None,
            IndexFreshness {
                status: status.to_string(),
                reasons: reason.iter().map(|r| r.to_string()).collect(),
            },
            "unavailable",
        ),
        View::Present(p) => (
            Some(SnapshotRef {
                nonce: p.revision.nonce.clone(),
                generation: p.revision.generation,
            }),
            IndexFreshness {
                status: p.freshness.clone(),
                reasons: p.freshness_reasons.clone(),
            },
            if p.graph_current {
                "current"
            } else {
                "incompatible"
            },
        ),
    };

    Ok(Report {
        schema_version: SCHEMA_VERSION,
        repo: repo.to_string(),
        scope: scope_name(req.scope),
        base: req.base.clone(),
        head: structure.head.clone(),
        snapshot,
        index_freshness,
        graph_proof,
        changed: Changed {
            total_files: structure.total_files,
            listed_files: structure.files.len(),
            total_definitions,
            listed_definitions: listed,
            traversed_definitions: traversed,
            definitions,
        },
        review_areas,
        test_candidates: candidates,
        instructions: Vec::new(),
        coverage_limits: limits,
        omissions: omissions.rows(),
        next_actions,
        bounds: Bounds {
            changed_files: req.limit,
            listed_definitions: MAX_LISTED_DEFINITIONS,
            traversed_definitions: MAX_DEFINITIONS,
            depth: DEPTH,
            traversal_total_ms: TOTAL_TRAVERSAL_MS,
            traversal_ms: edges::TRACE_TIME_MS,
            test_candidates: MAX_CANDIDATES,
            candidate_reasons: MAX_REASONS,
            instruction_files: MAX_INSTRUCTION_FILES,
            quoted_lines: MAX_QUOTED_LINES,
            review_areas: MAX_AREAS,
            area_changed_files: MAX_AREA_CHANGED,
            area_dependent_files: MAX_AREA_DEPENDENTS,
            dependent_reasons: MAX_DEPENDENT_REASONS,
            limit_paths: MAX_LIMIT_PATHS,
            json_bytes: MAX_REPORT_BYTES,
        },
        disclaimer: DISCLAIMER,
    })
}

/// Serialized length of one value in bytes. The D7 trim keeps a running total from these lengths.
fn json_len<T: Serialize + ?Sized>(value: &T) -> Result<usize, String> {
    serde_json::to_string(value)
        .map(|text| text.len())
        .map_err(|e| format!("checks report cannot be serialized: {e}"))
}

/// D5: the quoted lines of the instruction files that apply to the changed paths. Files are the
/// root and every ancestor directory of a changed file, sorted by depth then path, and the
/// files are read through the confined reader. A file that exists but cannot be read is skipped
/// and counted (`instructions`/`unreadable`); a missing one is not an item. Quoted lines are
/// data: nothing here runs them.
pub(crate) fn instruction_lines(
    root: &Path,
    changed: &[String],
) -> (Vec<Instruction>, Vec<Omission>) {
    let mut candidates: BTreeSet<(usize, String)> = BTreeSet::new();
    for path in changed {
        let parts: Vec<&str> = path.split('/').collect();
        // `parts[..depth]` is a directory of the changed file, the root at depth 0.
        for depth in 0..parts.len() {
            let dir = parts[..depth].join("/");
            for name in INSTRUCTION_FILES {
                let rel = if dir.is_empty() {
                    name.to_string()
                } else {
                    format!("{dir}/{name}")
                };
                candidates.insert((depth, rel));
            }
        }
    }
    let mut omissions = Omissions::default();
    let mut quoted: Vec<Instruction> = Vec::new();
    let mut present = 0usize;
    for (_, rel) in candidates {
        if present == MAX_INSTRUCTION_FILES {
            // Past the file cap a file is counted, never read: a stat alone decides the count.
            if std::fs::symlink_metadata(root.join(&rel)).is_ok() {
                omissions.add_count("instructions", "item_limit", 1);
            }
            continue;
        }
        let bytes = match semantic::confined_read_classified(root, &rel, MAX_INSTRUCTION_BYTES) {
            Ok(bytes) => bytes,
            Err(semantic::ConfinedReadError::NotFound(_)) => continue,
            Err(_) => {
                present += 1;
                omissions.add_count("instructions", "unreadable", 1);
                continue;
            }
        };
        present += 1;
        let Ok(text) = String::from_utf8(bytes) else {
            omissions.add_count("instructions", "unreadable", 1);
            continue;
        };
        for (line, body) in quoted_lines(&text) {
            if quoted.len() == MAX_QUOTED_LINES {
                omissions.add_count("instructions", "item_limit", 1);
                continue;
            }
            quoted.push(Instruction {
                path: rel.clone(),
                line,
                text: body,
                state: "found",
                basis: "explicit-instruction",
            });
        }
    }
    (quoted, omissions.rows())
}

/// The lines inside a fenced code block (opened and closed by a line starting with three
/// backticks or three tildes) that contain `test` as a whole word, as (1-based line, text).
/// The fence lines themselves are never quoted.
fn quoted_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        match fence {
            Some(marker) => {
                if trimmed.starts_with(marker) {
                    fence = None;
                } else if has_word_test(line) {
                    out.push((index + 1, cut_text(line.trim())));
                }
            }
            None => {
                if trimmed.starts_with("```") {
                    fence = Some("```");
                } else if trimmed.starts_with("~~~") {
                    fence = Some("~~~");
                }
            }
        }
    }
    out
}

/// Whether `test` occurs as a whole word, case-insensitively, with ASCII word boundaries.
fn has_word_test(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    lower.match_indices("test").any(|(at, _)| {
        let before = at == 0 || !is_word(bytes[at - 1]);
        let after = at + 4 >= bytes.len() || !is_word(bytes[at + 4]);
        before && after
    })
}

/// At most 200 bytes of the line, cut on a character boundary.
fn cut_text(line: &str) -> String {
    let mut end = line.len().min(MAX_INSTRUCTION_TEXT_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_string()
}

/// Removes the last item of a list. Returns the bytes the removal takes off the list's serialized
/// length: the item's bytes, plus its comma when other items remain (`[a,b]` to `[a]` takes `,b`).
/// `None` when the list is empty.
fn pop_item<T: Serialize>(list: &mut Vec<T>) -> Result<Option<isize>, String> {
    let Some(last) = list.last() else {
        return Ok(None);
    };
    let item = json_len(last)?;
    let comma = usize::from(list.len() > 1);
    list.pop();
    Ok(Some((item + comma) as isize))
}

/// `pop_item`, but only while more than one item remains (D7 keeps the first item of a list).
fn pop_beyond_first<T: Serialize>(list: &mut Vec<T>) -> Result<Option<isize>, String> {
    if list.len() > 1 {
        pop_item(list)
    } else {
        Ok(None)
    }
}

/// One step of the D7 order: removes one item from the first source that still has one. Returns
/// the section it came from and the bytes the step takes off the report's serialized length
/// (a positive number; a `listed_definitions` digit that disappears counts too).
fn trim_one(report: &mut Report) -> Result<Option<(&'static str, isize)>, String> {
    // 1. quoted instructions, last first
    if let Some(bytes) = pop_item(&mut report.instructions)? {
        return Ok(Some(("instructions", bytes)));
    }
    // 2. candidate reasons beyond the first, last candidate first
    for candidate in report.test_candidates.iter_mut().rev() {
        if let Some(bytes) = pop_beyond_first(&mut candidate.reasons)? {
            return Ok(Some(("candidate_reasons", bytes)));
        }
    }
    // 3. dependent reasons beyond the first, then dependent files (last area first)
    for area in report.review_areas.iter_mut().rev() {
        if let Some(files) = area.dependent_files.as_mut() {
            for file in files.iter_mut().rev() {
                if let Some(bytes) = pop_beyond_first(&mut file.reasons)? {
                    return Ok(Some(("dependent_reasons", bytes)));
                }
            }
        }
    }
    for area in report.review_areas.iter_mut().rev() {
        if let Some(files) = area.dependent_files.as_mut() {
            if let Some(bytes) = pop_item(files)? {
                return Ok(Some(("area_dependent_files", bytes)));
            }
        }
    }
    // 4. test candidates, last first
    if let Some(bytes) = pop_item(&mut report.test_candidates)? {
        return Ok(Some(("test_candidates", bytes)));
    }
    // 5. changed definitions, last first; listed_definitions follows the list
    let digits_before = json_len(&report.changed.listed_definitions)? as isize;
    if let Some(bytes) = pop_item(&mut report.changed.definitions)? {
        report.changed.listed_definitions = report.changed.definitions.len();
        let digits_after = json_len(&report.changed.listed_definitions)? as isize;
        return Ok(Some((
            "definitions",
            bytes - (digits_after - digits_before),
        )));
    }
    // 6. changed files per area beyond the first (last area first), then review areas (last first)
    for area in report.review_areas.iter_mut().rev() {
        if let Some(bytes) = pop_beyond_first(&mut area.changed_files)? {
            return Ok(Some(("area_changed_files", bytes)));
        }
    }
    if let Some(bytes) = pop_item(&mut report.review_areas)? {
        return Ok(Some(("review_areas", bytes)));
    }
    // 7. coverage-limit paths, last limit first
    for limit in report.coverage_limits.iter_mut().rev() {
        if let Some(bytes) = pop_item(&mut limit.paths)? {
            return Ok(Some(("limit_paths", bytes)));
        }
    }
    Ok(None)
}

/// Fits the report to `max_bytes` (D7). Removes one item at a time in the D7 order and records
/// each removal as `json_budget` under its section. The running length is updated by the exact
/// bytes each removal takes, so no step re-serializes the whole report; the final length is
/// then measured exactly, and a report still over budget is an error, never returned.
fn fit_with(mut report: Report, max_bytes: usize) -> Result<Report, String> {
    let mut omissions = Omissions::default();
    omissions.absorb(std::mem::take(&mut report.omissions));
    report.omissions = omissions.rows();
    let mut size = json_len(&report)? as isize;
    while size > max_bytes as isize {
        let Some((section, bytes)) = trim_one(&mut report)? else {
            return Err("checks report exceeds byte budget".to_string());
        };
        let before = json_len(&report.omissions)? as isize;
        omissions.add_count(section, "json_budget", 1);
        report.omissions = omissions.rows();
        let after = json_len(&report.omissions)? as isize;
        size += (after - before) - bytes;
    }
    if json_len(&report)? > max_bytes {
        return Err("checks report exceeds byte budget".to_string());
    }
    Ok(report)
}

/// Fits the report to the 262,144-byte budget (D7).
fn fit(report: Report) -> Result<Report, String> {
    fit_with(report, MAX_REPORT_BYTES)
}

/// The `checks` document for one registered repository (D0-D10): git structure first, then one
/// read of the index in one transaction, then the quoted instructions (D5) and the byte budget
/// (D7). A missing index is reported, never created. Every failure is exit 3, except what
/// `changes::structure` refuses with its own code.
pub fn document(home: &Path, repo: &str, root: &Path, req: &Request) -> Result<String, Failure> {
    let (structure, diff_git) = changes::structure(root, req)?;
    let fingerprint = |path: &Path| diff_git.fingerprint(path);
    let outcome = match store::open_read(&store::db_path(home, repo)) {
        store::IndexRead::Missing => assemble(
            None,
            repo,
            &structure,
            req,
            IndexState::Missing,
            &fingerprint,
        ),
        store::IndexRead::Outdated(_) => assemble(
            None,
            repo,
            &structure,
            req,
            IndexState::Outdated,
            &fingerprint,
        ),
        store::IndexRead::Ahead(_) => {
            assemble(None, repo, &structure, req, IndexState::Ahead, &fingerprint)
        }
        store::IndexRead::Error(message) => Err(format!("cannot read index: {message}")),
        store::IndexRead::Ready(mut conn) => {
            let tx = conn.transaction().map_err(|e| e.to_string());
            match tx {
                Err(message) => Err(message),
                Ok(tx) => {
                    let report = assemble(
                        Some(&tx),
                        repo,
                        &structure,
                        req,
                        IndexState::Ready,
                        &fingerprint,
                    );
                    match tx.commit() {
                        Ok(()) => report,
                        Err(e) => Err(e.to_string()),
                    }
                }
            }
        }
    };
    let mut report = outcome.map_err(|message| Failure { code: 3, message })?;
    let changed: Vec<String> = structure.files.iter().map(|f| f.path.clone()).collect();
    let (instructions, unread) = instruction_lines(&structure.canon, &changed);
    report.instructions = instructions;
    let mut omissions = Omissions::default();
    omissions.absorb(std::mem::take(&mut report.omissions));
    omissions.absorb(unread);
    report.omissions = omissions.rows();
    let report = fit(report).map_err(|message| Failure { code: 3, message })?;
    serde_json::to_string(&report).map_err(|e| Failure {
        code: 3,
        message: format!("checks report cannot be serialized: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{DefOut, FileOut, Hunk, Range};
    use crate::edges::tests as edge_fixtures;
    use crate::store::tests as store_fixtures;
    use crate::store::{EdgeEndpoint, EdgeRow, FileRow};
    use crate::testutil::tmpdir;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture {
        home: PathBuf,
        root: PathBuf,
        conn: rusqlite::Connection,
        sid: i64,
    }

    fn snapshot_at(conn: &rusqlite::Connection, root: &Path) -> i64 {
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

    /// A repository database and a worktree root under one fresh home, holding one snapshot.
    fn fixture(tag: &str) -> Fixture {
        let home = tmpdir(tag);
        let root = home.join("worktree");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let db = store::db_path(&home, "r");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = store::open_live(&db).unwrap();
        let sid = snapshot_at(&conn, &root);
        Fixture {
            home,
            root,
            conn,
            sid,
        }
    }

    /// Clean git state matching the snapshot row, so only content decides freshness.
    fn clean(_root: &Path) -> git::GitInfo {
        git::GitInfo {
            present: true,
            valid: true,
            head: None,
            branch: None,
            dirty: false,
            untracked: 0,
            toplevel: None,
            worktree: true,
        }
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

    /// A definition whose span starts at `byte_start`, five bytes long.
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
        row.file_id = file_id_of(f, caller_path);
        row.caller_path = caller_path.to_string();
        row.caller_span_bytes = (0, 5);
        row.evidence = json!({"path": caller_path, "span": [0, 5]}).to_string();
        row
    }

    /// The `files` id of a path, which an edge row names by id.
    fn file_id_of(f: &Fixture, path: &str) -> i64 {
        f.conn
            .query_row(
                "SELECT id FROM files WHERE snapshot_id = ?1 AND path = ?2",
                rusqlite::params![f.sid, path],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A file holding one caller definition at bytes 0..5, the span every test edge names.
    fn caller_file(f: &Fixture, path: &str) -> i64 {
        let id = file(f, path, "rust");
        def(f, id, "caller", "function", 0);
        id
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
    fn name_call(f: &Fixture, caller_path: &str, name: &str) -> EdgeRow {
        let mut row = store_fixtures::stored_edge(
            Some(store_fixtures::syntax_endpoint(caller_path, 0)),
            None,
            "POTENTIAL_CALL",
            "Heuristic",
            None,
        );
        row.snapshot_id = f.sid;
        row.file_id = file_id_of(f, caller_path);
        row.caller_path = caller_path.to_string();
        row.caller_span_bytes = (0, 5);
        row.evidence = json!({"path": caller_path, "span": [0, 5]}).to_string();
        row.provider = "syntax".to_string();
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

    fn write_live(f: &Fixture, rel: &str, text: &str) {
        let path = f.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Two files of one SCIP import: `crates/b/lib.py::target` and its caller in `tests/test_main.py`.
    fn scip_fixture(tag: &str) -> Fixture {
        let caller_src: &[u8] = b"def caller():\n    return target()\n";
        let lib_src: &[u8] = b"def target():\n    return 1\n";
        let caller_digest = store::content_digest(caller_src);
        let lib_digest = store::content_digest(lib_src);
        let f = fixture(tag);
        edge_fixtures::ingest_py(&f.conn, f.sid, "tests/test_main.py", caller_src);
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
                    "tests/test_main.py",
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
                    "tests/test_main.py",
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
        f
    }

    fn def_row(path: &str, name: &str, kind: &'static str, change: &'static str) -> DefOut {
        DefOut {
            name: name.to_string(),
            kind,
            change,
            header_touched: true,
            old_span: None,
            new_span: None,
            lookup: (change != "added").then(|| (path.to_string(), name.to_string(), kind)),
            sort_line: 1,
        }
    }

    fn modified(path: &str, language: Option<&'static str>, defs: Vec<DefOut>) -> FileOut {
        FileOut {
            path: path.to_string(),
            old_path: None,
            status: "modified",
            change: "modified",
            reason: None,
            language,
            established: language.is_some(),
            defs,
            outside: Vec::new(),
            hunks: 1,
        }
    }

    fn refused(path: &str, status: &'static str, reason: &'static str) -> FileOut {
        FileOut {
            path: path.to_string(),
            old_path: None,
            status,
            change: "modified",
            reason: Some(reason),
            language: None,
            established: false,
            defs: Vec::new(),
            outside: Vec::new(),
            hunks: 1,
        }
    }

    /// A deleted file whose one definition was removed with it.
    fn removed_file(path: &str, name: &str) -> FileOut {
        FileOut {
            path: path.to_string(),
            old_path: None,
            status: "deleted",
            change: "deleted",
            reason: None,
            language: Some("rust"),
            established: true,
            defs: vec![DefOut {
                name: name.to_string(),
                kind: "function",
                change: "removed",
                header_touched: true,
                old_span: None,
                new_span: None,
                lookup: Some((path.to_string(), name.to_string(), "function")),
                sort_line: 1,
            }],
            outside: Vec::new(),
            hunks: 1,
        }
    }

    fn hunk(start: u32) -> Hunk {
        Hunk {
            old: Range { start, count: 1 },
            new: Range { start, count: 1 },
        }
    }

    fn structure_for(canon: &Path, files: Vec<FileOut>) -> Structure {
        let total = files.len();
        Structure {
            head: "0123456789abcdef0123456789abcdef01234567".to_string(),
            files,
            total_files: total,
            truncated: false,
            canon: canon.to_path_buf(),
        }
    }

    fn request() -> Request {
        changes::request("unstaged", None, changes::DEFAULT_LIMIT).unwrap()
    }

    /// The Ready path: one transaction on the fixture database, committed afterwards.
    fn run(f: &mut Fixture, structure: &Structure) -> Result<Report, String> {
        let tx = f.conn.transaction().unwrap();
        let out = assemble(
            Some(&tx),
            "r",
            structure,
            &request(),
            IndexState::Ready,
            &clean,
        );
        tx.commit().unwrap();
        out
    }

    fn find<'a>(r: &'a Report, path: &str) -> Option<&'a TestCandidate> {
        r.test_candidates.iter().find(|c| c.path == path)
    }

    fn limit<'a>(r: &'a Report, name: &str) -> Option<&'a CoverageLimit> {
        r.coverage_limits.iter().find(|l| l.limit == name)
    }

    fn limit_names(r: &Report) -> Vec<&'static str> {
        r.coverage_limits.iter().map(|l| l.limit).collect()
    }

    fn omission(r: &Report, section: &str, reason: &str) -> Option<Option<usize>> {
        r.omissions
            .iter()
            .find(|o| o.section == section && o.reason == reason)
            .map(|o| o.omitted_count)
    }

    fn area<'a>(r: &'a Report, name: &str) -> Option<&'a ReviewArea> {
        r.review_areas.iter().find(|a| a.area == name)
    }

    fn dir_names(home: &Path) -> BTreeSet<String> {
        std::fs::read_dir(home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn proven_test_caller_is_proven_edge_candidate() {
        let mut f = fixture("checks-proven");
        let lib = file(&f, "src/lib.rs", "rust");
        caller_file(&f, "src/tests/lib.rs");
        def(&f, lib, "target", "function", 0);
        insert(&f, &[proven(&f, "src/tests/lib.rs", "src/lib.rs", 0)]);
        certify(&f);
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/lib.rs",
                Some("rust"),
                vec![def_row("src/lib.rs", "target", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        assert_eq!(r.graph_proof, "current");
        assert_eq!(r.test_candidates.len(), 1, "{:?}", r.test_candidates);
        let c = &r.test_candidates[0];
        assert_eq!(c.path, "src/tests/lib.rs");
        assert_eq!(c.basis, "proven-edge");
        assert_eq!(c.state, "suggested");
        assert!(c
            .reasons
            .iter()
            .any(|x| x.basis == "proven-edge" && x.detail == "distance-1"));
    }

    #[test]
    fn name_match_is_name_candidate_never_proven() {
        let mut f = fixture("checks-name");
        let lib = file(&f, "src/lib.rs", "rust");
        caller_file(&f, "src/tests/lib.rs");
        def(&f, lib, "target", "function", 0);
        insert(&f, &[name_call(&f, "src/tests/lib.rs", "target")]);
        certify(&f);
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/lib.rs",
                Some("rust"),
                vec![def_row("src/lib.rs", "target", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        let c = find(&r, "src/tests/lib.rs").expect("name candidate");
        assert_eq!(c.basis, "name-candidate");
        assert!(c.reasons.iter().all(|x| x.basis != "proven-edge"));
        // Control: the same caller with a proven edge is proven.
        insert(&f, &[proven(&f, "src/tests/lib.rs", "src/lib.rs", 0)]);
        certify(&f);
        let r = run(&mut f, &s).unwrap();
        assert_eq!(find(&r, "src/tests/lib.rs").unwrap().basis, "proven-edge");
    }

    #[test]
    fn same_stem_test_file_is_path_convention() {
        let mut f = fixture("checks-stem");
        file(&f, "crates/cli/tests/map.rs", "rust");
        file(&f, "crates/cli/tests/maps.rs", "rust");
        file(&f, "crates/cli/src/map_test_data.rs", "rust");
        file(&f, "crates/cli/src/map.rs", "rust");
        certify(&f);
        let s = structure_for(
            &f.root,
            vec![modified("crates/cli/src/map.rs", Some("rust"), Vec::new())],
        );
        let r = run(&mut f, &s).unwrap();
        let paths: Vec<&str> = r.test_candidates.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["crates/cli/tests/map.rs"]);
        let c = &r.test_candidates[0];
        assert_eq!(c.basis, "path-convention");
        assert_eq!(c.reasons[0].detail, "same-stem");
        assert_eq!(
            serde_json::to_value(&c.reasons[0].source).unwrap(),
            json!({"type":"file","path":"crates/cli/src/map.rs","name":null,"kind":null})
        );
    }

    #[test]
    fn module_level_change_gets_file_sourced_candidate() {
        let mut f = fixture("checks-module");
        file(&f, "tests/test_calc.py", "python");
        certify(&f);
        let mut calc = modified("pkg/calc.py", Some("python"), Vec::new());
        calc.outside.push(hunk(3));
        let s = structure_for(&f.root, vec![calc]);
        let r = run(&mut f, &s).unwrap();
        assert_eq!(r.changed.total_definitions, 0);
        assert_eq!(r.test_candidates.len(), 1);
        let c = &r.test_candidates[0];
        assert_eq!(c.path, "tests/test_calc.py");
        assert_eq!(c.basis, "path-convention");
        assert!(c
            .reasons
            .iter()
            .all(|x| x.source.source_type == "file" && x.source.path == "pkg/calc.py"));
    }

    #[test]
    fn dependents_keep_source_basis_and_distance() {
        let mut f = fixture("checks-deps");
        let a = file(&f, "src/a.rs", "rust");
        caller_file(&f, "src/b.rs");
        caller_file(&f, "src/c.rs");
        def(&f, a, "f", "function", 0);
        insert(
            &f,
            &[
                proven(&f, "src/b.rs", "src/a.rs", 0),
                name_call(&f, "src/c.rs", "f"),
            ],
        );
        certify(&f);
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/a.rs",
                Some("rust"),
                vec![def_row("src/a.rs", "f", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        let src = area(&r, "src").expect("area src");
        assert_eq!(src.changed_files, ["src/a.rs"]);
        assert_eq!(src.dependent_count, Some(2));
        let deps = src.dependent_files.as_ref().expect("dependents listed");
        let paths: Vec<&str> = deps.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["src/b.rs", "src/c.rs"]);
        let b = &deps[0].reasons[0];
        assert_eq!((b.basis, b.detail.as_str()), ("proven-edge", "distance-1"));
        assert_eq!(b.source.source_type, "definition");
        assert_eq!(b.source.path, "src/a.rs");
        assert_eq!(b.source.name.as_deref(), Some("f"));
        assert_eq!(b.source.kind.as_deref(), Some("function"));
        let c = &deps[1].reasons[0];
        assert_eq!(
            (c.basis, c.detail.as_str()),
            ("name-candidate", "distance-1")
        );
    }

    #[test]
    fn checks_traverses_snapshot_definition_after_edit() {
        let mut f = fixture("checks-edit");
        write_live(&f, "src/lib.rs", "fn target() {}\n");
        let lib = file(&f, "src/lib.rs", "rust");
        caller_file(&f, "src/tests/lib.rs");
        def(&f, lib, "target", "function", 0);
        insert(&f, &[name_call(&f, "src/tests/lib.rs", "target")]);
        certify(&f);
        // Edited after indexing: the definition keeps its place, its body changes.
        write_live(&f, "src/lib.rs", "fn target() { let _x = 1; }\n");
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/lib.rs",
                Some("rust"),
                vec![def_row("src/lib.rs", "target", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        assert_eq!(r.changed.definitions[0].status, "analysed");
        let c = find(&r, "src/tests/lib.rs").expect("candidate kept");
        assert_eq!(c.basis, "name-candidate");
        assert!(c.reasons.iter().any(|x| x.basis == "name-candidate"
            && x.source.source_type == "definition"
            && x.source.name.as_deref() == Some("target")));
        assert_eq!(r.index_freshness.status, "stale");
        let partial = limit(&r, "partial-graph").expect("partial graph");
        assert!(partial.paths.contains(&"src/lib.rs".to_string()));
        // Control: a position query on the same edited source is refused.
        let request = index::TraceRequest {
            source: index::SpanQuery {
                rel: "src/lib.rs",
                line1: 1,
                col1: 4,
                refs: false,
            },
            symbol: None,
            destination: None,
            depth: 3,
            limit: index::MAX_SEARCH_LIMIT,
            offset: 0,
            summary: false,
            include_tests: false,
        };
        let err = match index::impact_query(&f.home, "r", &f.root, &request) {
            Err(message) => message,
            Ok(_) => panic!("a position query on an edited source must be refused"),
        };
        assert!(err.contains("source-changed"), "{err}");
    }

    #[test]
    fn removed_file_definition_is_traversed() {
        let mut f = fixture("checks-removed");
        let lib = file(&f, "src/lib.rs", "rust");
        caller_file(&f, "src/tests/lib.rs");
        def(&f, lib, "target", "function", 0);
        insert(&f, &[name_call(&f, "src/tests/lib.rs", "target")]);
        certify(&f);
        // The live root has no src/lib.rs: it was deleted after indexing.
        let s = structure_for(&f.root, vec![removed_file("src/lib.rs", "target")]);
        let r = run(&mut f, &s).unwrap();
        assert_eq!(r.changed.definitions[0].status, "analysed");
        let c = find(&r, "src/tests/lib.rs").expect("candidate");
        assert!(c
            .reasons
            .iter()
            .any(|x| x.basis == "name-candidate" && x.source.source_type == "definition"));
        // Control: a file with no snapshot row gives no candidate. The brief expects
        // anchor-unavailable here; symbol_rows joins files, so the row is not-in-index.
        let s2 = structure_for(&f.root, vec![removed_file("src/other.rs", "target")]);
        let r2 = run(&mut f, &s2).unwrap();
        assert_eq!(r2.changed.definitions[0].status, "not-in-index");
        assert!(find(&r2, "src/tests/lib.rs").is_none());
    }

    #[test]
    fn checks_traverses_scip_owner_after_edit() {
        let mut f = scip_fixture("checks-scip-edit");
        write_live(&f, "crates/b/lib.py", "def target():\n    return 2\n");
        let s = structure_for(
            &f.root,
            vec![modified(
                "crates/b/lib.py",
                Some("python"),
                vec![def_row("crates/b/lib.py", "target", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        assert_eq!(r.changed.definitions[0].status, "analysed");
        let c = find(&r, "tests/test_main.py").expect("the SCIP owner's caller is reached");
        assert_eq!(c.basis, "proven-edge");
    }

    #[test]
    fn strongest_basis_wins_and_all_reasons_stay() {
        let mut f = fixture("checks-strongest");
        let lib = file(&f, "src/lib.rs", "rust");
        caller_file(&f, "src/tests/lib.rs");
        def(&f, lib, "target", "function", 0);
        insert(&f, &[proven(&f, "src/tests/lib.rs", "src/lib.rs", 0)]);
        certify(&f);
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/lib.rs",
                Some("rust"),
                vec![def_row("src/lib.rs", "target", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        let c = find(&r, "src/tests/lib.rs").unwrap();
        assert_eq!(c.basis, "proven-edge");
        let bases: Vec<&str> = c.reasons.iter().map(|x| x.basis).collect();
        assert_eq!(bases, ["proven-edge", "path-convention"]);
    }

    #[test]
    fn graph_proof_missing_withholds_edges() {
        let mut f = fixture("checks-no-proof");
        let lib = file(&f, "src/lib.rs", "rust");
        caller_file(&f, "src/tests/lib.rs");
        def(&f, lib, "target", "function", 0);
        insert(&f, &[proven(&f, "src/tests/lib.rs", "src/lib.rs", 0)]);
        // No certify: the stored relations are not proven current.
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/lib.rs",
                Some("rust"),
                vec![def_row("src/lib.rs", "target", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        assert_eq!(r.graph_proof, "incompatible");
        let c = find(&r, "src/tests/lib.rs").expect("the file-name rule still runs");
        assert_eq!(c.basis, "path-convention");
        assert!(c.reasons.iter().all(|x| x.basis == "path-convention"));
        assert_eq!(r.changed.traversed_definitions, 0);
        assert!(r
            .review_areas
            .iter()
            .all(|a| a.dependent_files.is_none() && a.dependent_count.is_none()));
        assert!(limit(&r, "graph-proof-incompatible").is_some());
        assert!(r
            .next_actions
            .iter()
            .any(|a| a.command.as_deref() == Some("riltra index --full --repo r")));
    }

    #[test]
    fn no_index_reports_limits_and_creates_nothing() {
        let home = tmpdir("checks-no-index");
        let before = dir_names(&home);
        let s = structure_for(
            &home,
            vec![modified("src/lib.rs", Some("rust"), Vec::new())],
        );
        let r = assemble(None, "r", &s, &request(), IndexState::Missing, &clean).unwrap();
        assert!(r.snapshot.is_none());
        assert_eq!(r.index_freshness.status, "no-index");
        assert_eq!(limit_names(&r), ["no-index", "no-test-candidates"]);
        assert!(matches!(
            store::open_read(&store::db_path(&home, "r")),
            store::IndexRead::Missing
        ));
        assert_eq!(dir_names(&home), before);
    }

    #[test]
    fn config_and_unanalysed_files_are_limits() {
        let home = tmpdir("checks-config");
        let mut hunked = modified("src/c.py", Some("python"), Vec::new());
        hunked.outside.push(hunk(2));
        let s = structure_for(
            &home,
            vec![
                refused("Cargo.toml", "unsupported", "language-unsupported"),
                refused(
                    ".github/workflows/ci.yml",
                    "unsupported",
                    "language-unsupported",
                ),
                refused("assets/logo.bin", "binary", "binary"),
                hunked,
                modified("src/a.rs", Some("rust"), Vec::new()),
            ],
        );
        let r = assemble(None, "r", &s, &request(), IndexState::Missing, &clean).unwrap();
        assert_eq!(
            limit_names(&r),
            [
                "no-index",
                "configuration-changed",
                "file-not-analysed",
                "hunks-outside-definitions",
                "no-test-candidates"
            ]
        );
        assert_eq!(
            limit(&r, "configuration-changed").unwrap().paths,
            [".github/workflows/ci.yml", "Cargo.toml"]
        );
        assert_eq!(
            limit(&r, "file-not-analysed").unwrap().paths,
            [".github/workflows/ci.yml", "Cargo.toml", "assets/logo.bin"]
        );
        assert_eq!(
            limit(&r, "hunks-outside-definitions").unwrap().paths,
            ["src/c.py"]
        );
        for l in &r.coverage_limits {
            assert!(!l.paths.iter().any(|p| p == "src/a.rs"));
        }
    }

    #[test]
    fn checks_changed_test_file_is_kept() {
        let mut f = fixture("checks-changed-test");
        caller_file(&f, "src/tests/lib.rs");
        certify(&f);
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/tests/lib.rs",
                Some("rust"),
                vec![def_row("src/tests/lib.rs", "check", "function", "modified")],
            )],
        );
        let r = run(&mut f, &s).unwrap();
        let c = find(&r, "src/tests/lib.rs").expect("a changed test file stays a candidate");
        assert_eq!(c.basis, "path-convention");
        assert_eq!(c.reasons[0].source.source_type, "file");
        assert_eq!(c.reasons[0].source.path, "src/tests/lib.rs");
    }

    #[test]
    fn checks_caps_candidates_and_counts_omissions() {
        let mut f = fixture("checks-caps");
        let lib = file(&f, "src/lib.rs", "rust");
        def(&f, lib, "target", "function", 0);
        let mut rows = Vec::new();
        for i in 0..60 {
            let path = format!("tests/t{i:02}.rs");
            caller_file(&f, &path);
            rows.push(proven(&f, &path, "src/lib.rs", 0));
        }
        // A cycle: the changed definition's own file calls it.
        rows.push(proven(&f, "src/lib.rs", "src/lib.rs", 0));
        insert(&f, &rows);
        certify(&f);
        let s = structure_for(
            &f.root,
            vec![modified(
                "src/lib.rs",
                Some("rust"),
                vec![def_row("src/lib.rs", "target", "function", "modified")],
            )],
        );
        let first = run(&mut f, &s).unwrap();
        assert_eq!(first.test_candidates.len(), 50);
        assert_eq!(
            omission(&first, "test_candidates", "item_limit"),
            Some(Some(10))
        );
        let again = run(&mut f, &s).unwrap();
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&again).unwrap()
        );
    }

    #[test]
    fn traversal_error_other_than_budget_is_store_error() {
        let mut f = scip_fixture("checks-store-error");
        let s = structure_for(
            &f.root,
            vec![modified(
                "crates/b/lib.py",
                Some("python"),
                vec![def_row("crates/b/lib.py", "target", "function", "modified")],
            )],
        );
        // Control: the intact index answers.
        let ok = run(&mut f, &s).unwrap();
        assert_eq!(
            find(&ok, "tests/test_main.py").unwrap().basis,
            "proven-edge"
        );
        // Fault: the stored edges are read only by the D2 traversal (D0 and D1 read
        // snapshots, meta, files, symbols and imports), so a missing table is a
        // storage error inside D2. The SCIP owner index fault has its own test.
        let second = rusqlite::Connection::open(store::db_path(&f.home, "r")).unwrap();
        second.execute_batch("DROP TABLE code_edges").unwrap();
        drop(second);
        let err = run(&mut f, &s).expect_err("a storage error inside D2 must fail the report");
        assert!(err.contains("code_edges"), "{err}");
    }

    #[test]
    fn scip_owner_lookup_error_propagates() {
        let mut f = scip_fixture("checks-owner-error");
        let s = structure_for(
            &f.root,
            vec![modified(
                "crates/b/lib.py",
                Some("python"),
                vec![def_row("crates/b/lib.py", "target", "function", "modified")],
            )],
        );
        // Control: the intact index maps the SCIP owner, so the caller is a proven candidate.
        let ok = run(&mut f, &s).unwrap();
        assert_eq!(
            find(&ok, "tests/test_main.py").unwrap().basis,
            "proven-edge"
        );
        // Fault: the owner query names this index (INDEXED BY). A failed owner read is a
        // storage error inside the traversal, so the report must fail, not omit the owner.
        let second = rusqlite::Connection::open(store::db_path(&f.home, "r")).unwrap();
        second
            .execute_batch("DROP INDEX idx_semfacts_span")
            .unwrap();
        drop(second);
        let err = run(&mut f, &s).expect_err("a failed SCIP owner read must fail the report");
        assert!(err.contains("idx_semfacts_span"), "{err}");
    }

    /// The no-index report shape, built through `assemble` so the tests never write the bounds out.
    fn bare_report() -> Report {
        let structure = structure_for(Path::new("."), Vec::new());
        assemble(
            None,
            "r",
            &structure,
            &request(),
            IndexState::Missing,
            &clean,
        )
        .unwrap()
    }

    fn row_count(rows: &[Omission], section: &str, reason: &str) -> Option<Option<usize>> {
        rows.iter()
            .find(|o| o.section == section && o.reason == reason)
            .map(|o| o.omitted_count)
    }

    fn def_reason(path: &str, name: &str, basis: &'static str) -> Reason {
        Reason {
            source: Source {
                source_type: "definition",
                path: path.to_string(),
                name: Some(name.to_string()),
                kind: Some("function".to_string()),
            },
            basis,
            detail: "distance-1".to_string(),
        }
    }

    /// One quoted instruction and one candidate with two reasons: D7 steps 1, 2 and 4 each have an item.
    fn trimmable_report() -> Report {
        let mut report = bare_report();
        report.instructions.push(Instruction {
            path: "AGENTS.md".to_string(),
            line: 3,
            text: "cargo test --locked".to_string(),
            state: "found",
            basis: "explicit-instruction",
        });
        report.test_candidates.push(TestCandidate {
            path: "tests/a.rs".to_string(),
            area: "tests".to_string(),
            state: "suggested",
            basis: "proven-edge",
            reasons: vec![
                def_reason("src/a.rs", "f", "proven-edge"),
                def_reason("src/a.rs", "g", "proven-edge"),
            ],
        });
        report
    }

    /// A string of about `bytes` bytes that starts with `tag`, so each path is distinct.
    fn long(tag: &str, bytes: usize) -> String {
        format!("{tag}{}", "x".repeat(bytes))
    }

    #[test]
    fn instructions_quote_fenced_test_lines_only() {
        let home = tmpdir("checks-instructions");
        let root = home.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("CONTRIBUTING.md"),
            "Intro line.\nrun cargo test outside a fence\n```sh\ncargo test --locked\ncargo build\necho contest\n```\nafter the fence, test is not quoted\n",
        )
        .unwrap();
        let changed = ["src/lib.rs".to_string()];
        let (items, omitted) = instruction_lines(&root, &changed);
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.path, "CONTRIBUTING.md");
        assert_eq!(item.line, 4);
        assert_eq!(item.text, "cargo test --locked");
        assert_eq!(item.state, "found");
        assert_eq!(item.basis, "explicit-instruction");
        assert!(omitted.is_empty(), "{omitted:?}");
        // A 300 KiB TESTING.md and a non-UTF-8 src/README.md are skipped and counted; the quote stays.
        std::fs::write(root.join("TESTING.md"), vec![b'a'; 300 * 1024]).unwrap();
        std::fs::write(
            root.join("src/README.md"),
            [0xff, 0xfe, b't', b'e', b's', b't'],
        )
        .unwrap();
        let (items, omitted) = instruction_lines(&root, &changed);
        assert_eq!(items.len(), 1);
        assert_eq!(
            row_count(&omitted, "instructions", "unreadable"),
            Some(Some(2)),
            "{omitted:?}"
        );
    }

    #[test]
    fn json_budget_trims_in_fixed_order() {
        // Step 1: one byte short removes only the quoted instruction (it is the first step).
        let full = json_len(&trimmable_report()).unwrap();
        let first = fit_with(trimmable_report(), full - 1).unwrap();
        assert!(first.instructions.is_empty());
        assert_eq!(first.test_candidates[0].reasons.len(), 2);
        assert_eq!(
            row_count(&first.omissions, "instructions", "json_budget"),
            Some(Some(1))
        );
        assert_eq!(
            row_count(&first.omissions, "candidate_reasons", "json_budget"),
            None
        );
        // Step 2: one more byte short removes a reason beyond the first, not the candidate.
        let budget = json_len(&first).unwrap() - 1;
        let second = fit_with(trimmable_report(), budget).unwrap();
        assert_eq!(second.test_candidates.len(), 1);
        assert_eq!(second.test_candidates[0].reasons.len(), 1);
        assert_eq!(
            row_count(&second.omissions, "candidate_reasons", "json_budget"),
            Some(Some(1))
        );
        // Step 4: one more byte short removes the candidate itself.
        let budget = json_len(&second).unwrap() - 1;
        let third = fit_with(trimmable_report(), budget).unwrap();
        assert!(third.test_candidates.is_empty());
        assert_eq!(
            row_count(&third.omissions, "test_candidates", "json_budget"),
            Some(Some(1))
        );
    }

    #[test]
    fn json_budget_trims_definitions_when_they_alone_exceed() {
        let mut report = bare_report();
        for i in 0..200 {
            report.changed.definitions.push(DefinitionRow {
                path: long(&format!("{i:04}"), 1_996),
                name: long(&format!("{i:04}n"), 1_996),
                kind: "function",
                change: "modified",
                status: "analysed",
            });
        }
        report.changed.total_definitions = 200;
        report.changed.listed_definitions = 200;
        assert!(json_len(&report).unwrap() > MAX_REPORT_BYTES);
        let fitted = fit(report).expect("definitions alone must fit after trimming");
        let kept = fitted.changed.definitions.len();
        let text = serde_json::to_string(&fitted).unwrap();
        assert!(text.len() <= MAX_REPORT_BYTES, "{} bytes", text.len());
        assert!(kept > 0 && kept < 200, "kept {kept}");
        assert_eq!(fitted.changed.listed_definitions, kept);
        assert_eq!(fitted.changed.total_definitions, 200);
        assert_eq!(
            row_count(&fitted.omissions, "definitions", "json_budget"),
            Some(Some(200 - kept))
        );
    }

    #[test]
    fn json_budget_remainder_is_small() {
        let mut report = bare_report();
        report.changed.total_files = 1_000;
        report.changed.listed_files = 1_000;
        report.changed.total_definitions = 200;
        report.changed.listed_definitions = 200;
        for i in 0..200 {
            report.changed.definitions.push(DefinitionRow {
                path: long(&format!("d{i}"), 4_000),
                name: long(&format!("n{i}"), 4_000),
                kind: "function",
                change: "modified",
                status: "analysed",
            });
        }
        for i in 0..20 {
            report.instructions.push(Instruction {
                path: long(&format!("i{i}"), 4_000),
                line: i + 1,
                text: "t".repeat(200),
                state: "found",
                basis: "explicit-instruction",
            });
        }
        for i in 0..50 {
            report.test_candidates.push(TestCandidate {
                path: long(&format!("t{i}"), 4_000),
                area: "tests".to_string(),
                state: "suggested",
                basis: "proven-edge",
                reasons: (0..5)
                    .map(|k| def_reason(&long(&format!("s{k}"), 4_000), "f", "proven-edge"))
                    .collect(),
            });
        }
        for a in 0..30 {
            report.review_areas.push(ReviewArea {
                area: format!("area{a}"),
                changed_files: (0..20).map(|k| long(&format!("c{a}-{k}"), 4_000)).collect(),
                dependent_files: Some(
                    (0..10)
                        .map(|k| DependentFile {
                            path: long(&format!("d{a}-{k}"), 4_000),
                            reasons: (0..3)
                                .map(|r| {
                                    def_reason(
                                        &long(&format!("r{r}"), 4_000),
                                        "f",
                                        "name-candidate",
                                    )
                                })
                                .collect(),
                        })
                        .collect(),
                ),
                dependent_count: Some(10),
            });
        }
        for i in 0..13 {
            report.coverage_limits.push(CoverageLimit {
                limit: "partial-graph",
                count: 20,
                paths: (0..20).map(|k| long(&format!("p{i}-{k}"), 4_000)).collect(),
            });
        }
        assert!(json_len(&report).unwrap() > MAX_REPORT_BYTES);
        let mut fitted = fit(report).unwrap();
        assert!(json_len(&fitted).unwrap() <= MAX_REPORT_BYTES);
        // The remainder: every trimmable list emptied.
        fitted.instructions.clear();
        fitted.test_candidates.clear();
        fitted.review_areas.clear();
        fitted.changed.definitions.clear();
        for limit in fitted.coverage_limits.iter_mut() {
            limit.paths.clear();
        }
        let remainder = json_len(&fitted).unwrap();
        assert!(remainder < 16_384, "remainder is {remainder} bytes");
    }
}
