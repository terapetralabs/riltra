// SPDX-License-Identifier: Apache-2.0
//! Bounded, read-only project orientation from one indexed snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

use crate::{context, index, packet_source, semantic, store};

const MAX_ITEMS: usize = 20;
const MAX_DOCUMENT_BYTES: u64 = 65_536;
/// Maximum number of candidate document paths read for one overview.
const MAX_DOCUMENT_READS: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectOverview {
    pub repo: String,
    pub snapshot: SnapshotStatus,
    pub languages: Vec<String>,
    pub source_directories: Vec<String>,
    pub entry_points: Vec<SourceEvidence>,
    pub build_test_sources: Vec<SourceEvidence>,
    pub definitions: Vec<SourceEvidence>,
    pub next_actions: Vec<NextAction>,
    pub omissions: Vec<Omission>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotStatus {
    pub status: SnapshotState,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotState {
    Fresh,
    Stale,
    Unknown,
    NoIndex,
    UnsupportedSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceEvidence {
    pub kind: EvidenceKind,
    pub path: String,
    pub basis: EvidenceBasis,
    pub status: EvidenceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    #[serde(skip)]
    pub start_col: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_end: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    EntryPoint,
    BuildTestInstruction,
    Manifest,
    Definition,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBasis {
    IndexSnapshot,
    LiveDocument,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Available,
    Truncated,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextAction {
    pub kind: ActionKind,
    pub reason: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Task 3 wires the setup actions into CLI/MCP flows.
pub enum ActionKind {
    RegisterProject,
    BuildIndex,
    RefreshIndex,
    ReviewFreshness,
    ReadInstructions,
    ChooseDefinition,
    ConfigureClientManually,
}

#[derive(Debug, Clone, Serialize)]
pub struct Omission {
    pub section: OmissionSection,
    pub reason: OmissionReason,
    /// Unit depends on `reason`; unattempted candidate paths are not known documents.
    pub omitted_count: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OmissionSection {
    SnapshotReasons,
    Languages,
    SourceDirectories,
    EntryPoints,
    BuildTestSources,
    Definitions,
    NextActions,
    EvidenceText,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OmissionReason {
    ItemLimit,
    JsonBudget,
    /// Candidate paths skipped after the 64-read cap; their existence is unknown.
    UnattemptedCandidatePaths,
}

struct PendingSymbol {
    path: String,
    digest: String,
    symbol: store::SymbolRow,
}
struct PendingEntry {
    path: String,
    digest: String,
    symbol: Option<store::SymbolRow>,
}
type IndexedSource = Result<Vec<u8>, &'static str>;

/// Collects against the supplied home and root. Callers resolve the registry entry;
/// this function never opens a database for writing or migrates its schema.
pub fn collect(home: &Path, repo: &str, root: &Path) -> Result<ProjectOverview, String> {
    if !crate::paths::validate_name(repo) {
        return Err("invalid repository name".into());
    }
    let db = store::db_path(home, repo);
    let overview = match store::open_read(&db) {
        store::IndexRead::Missing => with_live_documents(
            empty(repo, SnapshotState::NoIndex, vec!["no-index".into()], None),
            root,
        ),
        store::IndexRead::Outdated(v) | store::IndexRead::Ahead(v) => with_live_documents(
            empty(
                repo,
                SnapshotState::UnsupportedSchema,
                vec![format!("unsupported-schema:{v}")],
                None,
            ),
            root,
        ),
        store::IndexRead::Error(e) => return Err(format!("cannot read index: {e}")),
        store::IndexRead::Ready(conn) => {
            let mut conn = conn;
            let tx = conn
                .transaction()
                .map_err(|e| format!("cannot begin index read: {e}"))?;
            let snapshot =
                store::latest_snapshot(&tx).map_err(|e| format!("cannot read snapshot: {e}"))?;
            let Some(snapshot) = snapshot else {
                let _ = tx.commit();
                return fit(with_live_documents(
                    empty(
                        repo,
                        SnapshotState::Unknown,
                        vec!["no-snapshot".into()],
                        None,
                    ),
                    root,
                ));
            };
            let (status, mut reasons) = index::evaluate_snapshot(&tx, repo, root, &snapshot);
            reasons.sort();
            reasons.dedup();
            let state = match status.as_str() {
                "fresh" => SnapshotState::Fresh,
                "stale" => SnapshotState::Stale,
                _ => SnapshotState::Unknown,
            };
            let files = store::files_of(&tx, snapshot.id)
                .map_err(|e| format!("cannot read indexed files: {e}"))?;
            let mut languages = files.iter().map(|f| f.language.clone()).collect::<Vec<_>>();
            languages.sort();
            languages.dedup();
            let mut dirs = files
                .iter()
                .filter_map(|f| Path::new(&f.path).parent())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>();
            dirs.sort();
            dirs.dedup();
            let mut definition_rows = Vec::new();
            let mut entry_rows = Vec::new();
            let mut definition_count = 0;
            let mut entry_count = 0;
            for file in &files {
                let mut symbols = store::symbols_at(&tx, snapshot.id, &file.path)
                    .map_err(|e| format!("cannot read definitions: {e}"))?;
                symbols.sort_by(|a, b| {
                    (a.byte_start, &a.kind, a.byte_end).cmp(&(b.byte_start, &b.kind, b.byte_end))
                });
                let has_main = symbols
                    .iter()
                    .any(|sym| sym.name.as_deref() == Some("main"));
                for sym in symbols {
                    definition_count += 1;
                    if definition_rows.len() < MAX_ITEMS {
                        definition_rows.push(PendingSymbol {
                            path: file.path.clone(),
                            digest: file.digest.clone(),
                            symbol: sym.clone(),
                        });
                    }
                    if sym.name.as_deref() == Some("main") {
                        entry_count += 1;
                        if entry_rows.len() < MAX_ITEMS {
                            entry_rows.push(PendingEntry {
                                path: file.path.clone(),
                                digest: file.digest.clone(),
                                symbol: Some(sym),
                            });
                        }
                    }
                }
                if !has_main && conventional_entry(&file.path) {
                    entry_count += 1;
                    if entry_rows.len() < MAX_ITEMS {
                        entry_rows.push(PendingEntry {
                            path: file.path.clone(),
                            digest: file.digest.clone(),
                            symbol: None,
                        });
                    }
                }
            }
            let mut source_cache = BTreeMap::new();
            let definitions = definition_rows
                .iter()
                .map(|pending| {
                    let source = cached_indexed_source(
                        &mut source_cache,
                        root,
                        &pending.path,
                        &pending.digest,
                    );
                    indexed_evidence(
                        &pending.path,
                        &pending.symbol,
                        source,
                        EvidenceKind::Definition,
                    )
                })
                .collect::<Vec<_>>();
            let entries = entry_rows
                .iter()
                .map(|pending| match &pending.symbol {
                    Some(symbol) => {
                        let source = cached_indexed_source(
                            &mut source_cache,
                            root,
                            &pending.path,
                            &pending.digest,
                        );
                        indexed_evidence(&pending.path, symbol, source, EvidenceKind::EntryPoint)
                    }
                    None => conventional_entry_evidence(&pending.path),
                })
                .collect::<Vec<_>>();
            let definition_retained = definitions.len();
            let entry_retained = entries.len();
            let mut value = ProjectOverview {
                repo: repo.into(),
                snapshot: SnapshotStatus {
                    status: state,
                    reasons,
                    nonce: Some(snapshot.nonce.clone()),
                },
                languages,
                source_directories: dirs,
                entry_points: entries,
                build_test_sources: Vec::new(),
                definitions,
                next_actions: Vec::new(),
                omissions: Vec::new(),
            };
            let (documents, omitted) = document_evidence(root, &files);
            record_document_evidence(&mut value, documents, omitted);
            if definition_count > definition_retained {
                add_omission(
                    &mut value,
                    OmissionSection::Definitions,
                    OmissionReason::ItemLimit,
                    definition_count - definition_retained,
                );
            }
            if entry_count > entry_retained {
                add_omission(
                    &mut value,
                    OmissionSection::EntryPoints,
                    OmissionReason::ItemLimit,
                    entry_count - entry_retained,
                );
            }
            if value.snapshot.status != SnapshotState::Fresh {
                value.next_actions.push(NextAction {
                    kind: ActionKind::ReviewFreshness,
                    reason: "the indexed snapshot is not confirmed fresh".into(),
                });
            }
            if !value.build_test_sources.is_empty() {
                value.next_actions.push(NextAction {
                    kind: ActionKind::ReadInstructions,
                    reason: "review the project build and test guidance".into(),
                });
            }
            if !value.definitions.is_empty() {
                value.next_actions.push(NextAction {
                    kind: ActionKind::ChooseDefinition,
                    reason: "choose a definition to inspect".into(),
                });
            }
            let _ = tx.commit();
            value
        }
    };
    fit(overview)
}

pub fn render(overview: &ProjectOverview) -> Result<String, String> {
    serde_json::to_string(overview).map_err(|e| format!("cannot encode overview: {e}"))
}

fn empty(
    repo: &str,
    status: SnapshotState,
    reasons: Vec<String>,
    nonce: Option<String>,
) -> ProjectOverview {
    ProjectOverview {
        repo: repo.into(),
        snapshot: SnapshotStatus {
            status,
            reasons,
            nonce,
        },
        languages: Vec::new(),
        source_directories: Vec::new(),
        entry_points: Vec::new(),
        build_test_sources: Vec::new(),
        definitions: Vec::new(),
        next_actions: vec![NextAction {
            kind: ActionKind::BuildIndex,
            reason: "create an index to see project structure".into(),
        }],
        omissions: Vec::new(),
    }
}

fn cached_indexed_source<'a>(
    cache: &'a mut BTreeMap<(String, String), IndexedSource>,
    root: &Path,
    path: &str,
    digest: &str,
) -> &'a IndexedSource {
    cached_source(cache, (path.to_string(), digest.to_string()), || {
        read_indexed_source(root, path, digest)
    })
}

fn cached_source(
    cache: &mut BTreeMap<(String, String), IndexedSource>,
    key: (String, String),
    load: impl FnOnce() -> IndexedSource,
) -> &IndexedSource {
    cache.entry(key).or_insert_with(load)
}

fn read_indexed_source(root: &Path, path: &str, digest: &str) -> IndexedSource {
    match semantic::confined_read_classified(root, path, MAX_DOCUMENT_BYTES) {
        Ok(bytes) if store::content_digest(&bytes) != digest => Err("source-changed"),
        Ok(bytes) if std::str::from_utf8(&bytes).is_err() => Err("source-non-utf8"),
        Ok(bytes) => Ok(bytes),
        Err(semantic::ConfinedReadError::Path(_)) => Err("unsafe-path"),
        Err(semantic::ConfinedReadError::Io(e)) if e.contains("too large") => {
            Err("source-too-large")
        }
        Err(_) => Err("source-unavailable"),
    }
}

fn indexed_evidence(
    path: &str,
    sym: &store::SymbolRow,
    source: &IndexedSource,
    kind: EvidenceKind,
) -> SourceEvidence {
    let (status, reason, text) = match source {
        Err(reason) => (EvidenceStatus::Unavailable, Some((*reason).into()), None),
        Ok(bytes) => {
            let source = std::str::from_utf8(bytes).expect("indexed source was checked as UTF-8");
            if sym.byte_start > sym.byte_end
                || sym.byte_end > bytes.len()
                || !source.is_char_boundary(sym.byte_start)
                || !source.is_char_boundary(sym.byte_end)
            {
                (
                    EvidenceStatus::Unavailable,
                    Some("invalid-source-range".into()),
                    None,
                )
            } else {
                let (text, end) =
                    packet_source::bounded_excerpt(source, sym.byte_start, sym.byte_end);
                let state = if end < sym.byte_end {
                    EvidenceStatus::Truncated
                } else {
                    EvidenceStatus::Available
                };
                (
                    state,
                    (state == EvidenceStatus::Truncated).then(|| "snippet-cap".into()),
                    Some(text),
                )
            }
        }
    };
    SourceEvidence {
        kind,
        path: path.into(),
        basis: EvidenceBasis::IndexSnapshot,
        status,
        reason,
        symbol_name: sym.name.clone(),
        symbol_kind: Some(sym.kind.clone()),
        line_start: Some(sym.start_line),
        start_col: Some(sym.start_col),
        line_end: Some(sym.end_line),
        byte_start: Some(sym.byte_start as u64),
        byte_end: Some(sym.byte_end as u64),
        text,
    }
}

fn conventional_entry(path: &str) -> bool {
    matches!(
        Path::new(path).file_name().and_then(|s| s.to_str()),
        Some(
            "main.rs"
                | "main.py"
                | "main.go"
                | "main.java"
                | "main.c"
                | "main.cpp"
                | "index.js"
                | "index.ts"
        )
    )
}

fn conventional_entry_evidence(path: &str) -> SourceEvidence {
    SourceEvidence {
        kind: EvidenceKind::EntryPoint,
        path: path.to_string(),
        basis: EvidenceBasis::IndexSnapshot,
        status: EvidenceStatus::Unavailable,
        reason: Some("conventional-location-candidate".into()),
        symbol_name: None,
        symbol_kind: None,
        line_start: None,
        start_col: None,
        line_end: None,
        byte_start: None,
        byte_end: None,
        text: None,
    }
}

fn document_evidence(root: &Path, files: &[store::FileRow]) -> (Vec<SourceEvidence>, usize) {
    document_evidence_with(root, files, semantic::confined_read_classified)
}

fn document_evidence_with(
    root: &Path,
    files: &[store::FileRow],
    mut read: impl FnMut(&Path, &str, u64) -> Result<Vec<u8>, semantic::ConfinedReadError>,
) -> (Vec<SourceEvidence>, usize) {
    let mut candidates = BTreeSet::from([
        "README.md".to_string(),
        "AGENTS.md".into(),
        "CLAUDE.md".into(),
        "CONTRIBUTING.md".into(),
        "BUILDING.md".into(),
        "TESTING.md".into(),
        "Cargo.toml".into(),
        "package.json".into(),
        "pyproject.toml".into(),
        "go.mod".into(),
        "pom.xml".into(),
        "build.gradle".into(),
        "build.gradle.kts".into(),
        "CMakeLists.txt".into(),
        "Makefile".into(),
    ]);
    for f in files {
        if document_path_depth(&f.path) <= 2 && (is_manifest(&f.path) || is_instruction(&f.path)) {
            candidates.insert(f.path.clone());
        }
    }
    // The indexed directory list bounds candidate discovery to the root and one known level.
    let dirs = files
        .iter()
        .filter_map(|f| Path::new(&f.path).parent())
        .filter(|p| p.components().count() == 1)
        .map(|p| p.to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();
    for d in dirs {
        for name in [
            "README.md",
            "AGENTS.md",
            "CLAUDE.md",
            "CONTRIBUTING.md",
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "go.mod",
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "CMakeLists.txt",
            "Makefile",
        ] {
            candidates.insert(format!("{d}/{name}"));
        }
    }
    let indexed = files
        .iter()
        .map(|f| f.path.as_str())
        .collect::<BTreeSet<_>>();
    let (mut root_candidates, mut child_candidates): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|path| document_path_depth(path) == 1);
    root_candidates.append(&mut child_candidates);
    // This counts candidates not attempted; their existence on disk is unknown.
    let omitted = root_candidates.len().saturating_sub(MAX_DOCUMENT_READS);
    let documents = root_candidates
        .into_iter()
        .take(MAX_DOCUMENT_READS)
        .filter_map(|path| {
            let is_manifest = is_manifest(&path);
            let bytes = match read(root, &path, MAX_DOCUMENT_BYTES) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let is_indexed = indexed.contains(path.as_str());
                    return failed_document(path, is_manifest, is_indexed, error);
                }
            };
            if path.ends_with("package.json")
                && serde_json::from_slice::<serde_json::Value>(&bytes).is_err()
            {
                return Some(unavailable_document(path, is_manifest, "invalid-json"));
            }
            let text = match std::str::from_utf8(&bytes) {
                Ok(t) => t,
                Err(_) => return Some(unavailable_document(path, is_manifest, "source-non-utf8")),
            };
            let (excerpt, end) = packet_source::bounded_excerpt(text, 0, text.len());
            Some(SourceEvidence {
                kind: if is_manifest {
                    EvidenceKind::Manifest
                } else {
                    EvidenceKind::BuildTestInstruction
                },
                path,
                basis: EvidenceBasis::LiveDocument,
                status: if end < text.len() {
                    EvidenceStatus::Truncated
                } else {
                    EvidenceStatus::Available
                },
                reason: (end < text.len()).then(|| "snippet-cap".into()),
                symbol_name: None,
                symbol_kind: None,
                line_start: None,
                start_col: None,
                line_end: None,
                byte_start: None,
                byte_end: None,
                text: Some(excerpt),
            })
        })
        .collect();
    (documents, omitted)
}

fn document_path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn record_document_evidence(
    value: &mut ProjectOverview,
    documents: Vec<SourceEvidence>,
    omitted: usize,
) {
    value.build_test_sources = documents;
    if omitted > 0 {
        add_omission(
            value,
            OmissionSection::BuildTestSources,
            OmissionReason::UnattemptedCandidatePaths,
            omitted,
        );
    }
}

fn failed_document(
    path: String,
    manifest: bool,
    indexed: bool,
    error: semantic::ConfinedReadError,
) -> Option<SourceEvidence> {
    let reason = match error {
        semantic::ConfinedReadError::Path(_) => "unsafe-path",
        semantic::ConfinedReadError::NotFound(_) if !indexed => return None,
        semantic::ConfinedReadError::NotFound(_) => "source-unavailable",
        semantic::ConfinedReadError::Io(ref e) if e.contains("too large") => "source-too-large",
        semantic::ConfinedReadError::Io(_) => "source-unavailable",
    };
    Some(unavailable_document(path, manifest, reason))
}

fn with_live_documents(mut value: ProjectOverview, root: &Path) -> ProjectOverview {
    let (documents, omitted) = document_evidence(root, &[]);
    record_document_evidence(&mut value, documents, omitted);
    if !value.build_test_sources.is_empty() {
        value.next_actions.push(NextAction {
            kind: ActionKind::ReadInstructions,
            reason: "review the project build and test guidance".into(),
        });
    }
    value
}

fn unavailable_document(path: String, manifest: bool, reason: &str) -> SourceEvidence {
    SourceEvidence {
        kind: if manifest {
            EvidenceKind::Manifest
        } else {
            EvidenceKind::BuildTestInstruction
        },
        path,
        basis: EvidenceBasis::LiveDocument,
        status: EvidenceStatus::Unavailable,
        reason: Some(reason.into()),
        symbol_name: None,
        symbol_kind: None,
        line_start: None,
        start_col: None,
        line_end: None,
        byte_start: None,
        byte_end: None,
        text: None,
    }
}
fn is_manifest(p: &str) -> bool {
    matches!(
        Path::new(p).file_name().and_then(|s| s.to_str()),
        Some(
            "Cargo.toml"
                | "package.json"
                | "pyproject.toml"
                | "setup.py"
                | "go.mod"
                | "pom.xml"
                | "build.gradle"
                | "build.gradle.kts"
                | "CMakeLists.txt"
                | "Makefile"
        )
    )
}
fn is_instruction(p: &str) -> bool {
    matches!(
        Path::new(p).file_name().and_then(|s| s.to_str()),
        Some(
            "README.md"
                | "AGENTS.md"
                | "CLAUDE.md"
                | "CONTRIBUTING.md"
                | "BUILDING.md"
                | "TESTING.md"
        )
    )
}

fn fit(mut value: ProjectOverview) -> Result<ProjectOverview, String> {
    for (section, len) in [
        (
            OmissionSection::SnapshotReasons,
            value.snapshot.reasons.len(),
        ),
        (OmissionSection::Languages, value.languages.len()),
        (
            OmissionSection::SourceDirectories,
            value.source_directories.len(),
        ),
        (OmissionSection::EntryPoints, value.entry_points.len()),
        (
            OmissionSection::BuildTestSources,
            value.build_test_sources.len(),
        ),
        (OmissionSection::Definitions, value.definitions.len()),
        (OmissionSection::NextActions, value.next_actions.len()),
    ] {
        if len > MAX_ITEMS {
            let excess = len - MAX_ITEMS;
            trim_section(&mut value, section);
            add_omission(&mut value, section, OmissionReason::ItemLimit, excess);
        }
    }
    loop {
        let json =
            serde_json::to_vec(&value).map_err(|e| format!("cannot encode overview: {e}"))?;
        if json.len() <= context::DEFAULT_MAX_BYTES {
            return Ok(value);
        }
        if let Some(e) = all_evidence_mut(&mut value)
            .into_iter()
            .rev()
            .find(|e| e.text.is_some())
        {
            e.text = None;
            e.status = EvidenceStatus::Truncated;
            e.reason = Some("json-budget".into());
            add_omission(
                &mut value,
                OmissionSection::EvidenceText,
                OmissionReason::JsonBudget,
                1,
            );
            continue;
        }
        if !value.definitions.is_empty() {
            value.definitions.pop();
            add_omission(
                &mut value,
                OmissionSection::Definitions,
                OmissionReason::JsonBudget,
                1,
            );
            continue;
        }
        if !value.source_directories.is_empty() {
            value.source_directories.pop();
            add_omission(
                &mut value,
                OmissionSection::SourceDirectories,
                OmissionReason::JsonBudget,
                1,
            );
            continue;
        }
        if !value.build_test_sources.is_empty() {
            value.build_test_sources.pop();
            add_omission(
                &mut value,
                OmissionSection::BuildTestSources,
                OmissionReason::JsonBudget,
                1,
            );
            continue;
        }
        return Err("mandatory overview metadata exceeds JSON budget".into());
    }
}
fn all_evidence_mut(v: &mut ProjectOverview) -> Vec<&mut SourceEvidence> {
    v.entry_points
        .iter_mut()
        .chain(v.build_test_sources.iter_mut())
        .chain(v.definitions.iter_mut())
        .collect()
}
fn add_omission(
    v: &mut ProjectOverview,
    section: OmissionSection,
    reason: OmissionReason,
    n: usize,
) {
    if let Some(o) = v
        .omissions
        .iter_mut()
        .find(|o| o.section == section && o.reason == reason)
    {
        o.omitted_count += n;
    } else {
        v.omissions.push(Omission {
            section,
            reason,
            omitted_count: n,
        });
        v.omissions.sort_by_key(|o| (o.section, o.reason));
    }
}
fn trim_section(v: &mut ProjectOverview, s: OmissionSection) {
    match s {
        OmissionSection::SnapshotReasons => v.snapshot.reasons.truncate(MAX_ITEMS),
        OmissionSection::Languages => v.languages.truncate(MAX_ITEMS),
        OmissionSection::SourceDirectories => v.source_directories.truncate(MAX_ITEMS),
        OmissionSection::EntryPoints => v.entry_points.truncate(MAX_ITEMS),
        OmissionSection::BuildTestSources => v.build_test_sources.truncate(MAX_ITEMS),
        OmissionSection::Definitions => v.definitions.truncate(MAX_ITEMS),
        OmissionSection::NextActions => v.next_actions.truncate(MAX_ITEMS),
        OmissionSection::EvidenceText => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;
    use std::fs;
    #[test]
    fn json_budget_removes_excerpts_before_definition_samples_and_keeps_identity() {
        let mut value = empty("demo", SnapshotState::Fresh, vec![], Some("nonce".into()));
        for n in 0..12 {
            value.definitions.push(SourceEvidence {
                kind: EvidenceKind::Definition,
                path: format!("src/{n}.rs"),
                basis: EvidenceBasis::IndexSnapshot,
                status: EvidenceStatus::Available,
                reason: None,
                symbol_name: Some(format!("f{n}")),
                symbol_kind: Some("function".into()),
                line_start: Some(1),
                start_col: Some(1),
                line_end: Some(1),
                byte_start: Some(0),
                byte_end: Some(1),
                text: Some("x".repeat(4096)),
            });
        }
        let got = fit(value).unwrap();
        assert_eq!(got.definitions.len(), 12);
        assert!(got
            .definitions
            .iter()
            .all(|d| d.symbol_name.is_some() && d.byte_start == Some(0)));
        assert!(got.definitions.iter().any(|d| d.text.is_none()));
        assert!(got.definitions.iter().all(|d| d.symbol_name.is_some()));
        assert!(got
            .omissions
            .iter()
            .any(|o| o.section == OmissionSection::EvidenceText
                && o.reason == OmissionReason::JsonBudget));
    }

    #[test]
    fn json_budget_counts_each_definition_removed_including_the_last_one() {
        let definition = |name: String| SourceEvidence {
            kind: EvidenceKind::Definition,
            path: "src/lib.rs".into(),
            basis: EvidenceBasis::IndexSnapshot,
            status: EvidenceStatus::Unavailable,
            reason: Some("source-unavailable".into()),
            symbol_name: Some(name),
            symbol_kind: Some("function".into()),
            line_start: Some(1),
            start_col: Some(1),
            line_end: Some(1),
            byte_start: Some(0),
            byte_end: Some(1),
            text: None,
        };

        let mut many = empty("demo", SnapshotState::Fresh, vec![], Some("nonce".into()));
        many.definitions = (0..20)
            .map(|n| definition(format!("f{n}-{}", "x".repeat(2500))))
            .collect();
        let got = fit(many).unwrap();
        let omitted = got
            .omissions
            .iter()
            .find(|o| {
                o.section == OmissionSection::Definitions && o.reason == OmissionReason::JsonBudget
            })
            .unwrap();
        assert_eq!(omitted.omitted_count, 20 - got.definitions.len());
        assert!(omitted.omitted_count > 0);

        let mut last = empty("demo", SnapshotState::Fresh, vec![], Some("nonce".into()));
        last.definitions
            .push(definition("x".repeat(context::DEFAULT_MAX_BYTES + 1)));
        let got = fit(last).unwrap();
        assert!(got.definitions.is_empty());
        assert_eq!(
            got.omissions
                .iter()
                .find(|o| o.section == OmissionSection::Definitions
                    && o.reason == OmissionReason::JsonBudget)
                .unwrap()
                .omitted_count,
            1
        );
    }

    #[test]
    fn missing_index_and_unsupported_schema_are_read_only() {
        let home = tmpdir("overview-read-only");
        let root = tmpdir("overview-root");
        fs::write(root.join("README.md"), "read me\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        let missing = store::db_path(&home, "demo");
        let got = collect(&home, "demo", &root).unwrap();
        assert_eq!(got.snapshot.status, SnapshotState::NoIndex);
        assert!(got
            .build_test_sources
            .iter()
            .any(|e| e.path == "README.md" && e.status == EvidenceStatus::Available));
        assert!(got
            .build_test_sources
            .iter()
            .any(|e| e.path == "Cargo.toml" && e.kind == EvidenceKind::Manifest));
        assert!(
            !missing.exists(),
            "no-index collection must not create a database"
        );

        fs::create_dir_all(missing.parent().unwrap()).unwrap();
        let db = rusqlite::Connection::open(&missing).unwrap();
        db.execute_batch("PRAGMA user_version = 99;").unwrap();
        drop(db);
        let got = collect(&home, "demo", &root).unwrap();
        assert_eq!(got.snapshot.status, SnapshotState::UnsupportedSchema);
        let db = rusqlite::Connection::open(&missing).unwrap();
        let version: u32 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 99, "unsupported schema must not be migrated");
    }

    #[cfg(unix)]
    #[test]
    fn fifo_document_is_unavailable_without_blocking_collection() {
        use std::process::Command;
        use std::sync::mpsc;
        use std::time::Duration;

        let home = tmpdir("overview-fifo-home");
        let root = tmpdir("overview-fifo-root");
        fs::write(root.join("README.md"), "read me\n").unwrap();
        let regular = collect(&home, "demo", &root).unwrap();
        assert!(regular
            .build_test_sources
            .iter()
            .any(|e| { e.path == "README.md" && e.status == EvidenceStatus::Available }));

        fs::remove_file(root.join("README.md")).unwrap();
        let fifo = root.join("README.md");
        let status = match Command::new("mkfifo").arg(&fifo).status() {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("could not run mkfifo: {error}"),
        };
        assert!(status.success(), "mkfifo failed with {status}");

        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let result = collect(&home, "demo", &root);
            let _ = send.send(result);
        });
        let overview = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("collect blocked while reading FIFO")
            .unwrap();
        assert!(overview
            .build_test_sources
            .iter()
            .any(|e| { e.path == "README.md" && e.status == EvidenceStatus::Unavailable }));
    }

    #[test]
    fn collect_never_uses_semantic_retrieval() {
        let home = tmpdir("overview-no-semantic-retrieval");
        let root = tmpdir("overview-no-semantic-root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        index::build(&home, "demo", &root, false).unwrap();

        let searches_before = crate::knowledge_search::test_search_call_count();
        #[cfg(feature = "semantic-knowledge")]
        let model_loads_before = crate::inference::test_vector_model_load_call_count();
        let got = collect(&home, "demo", &root).unwrap();

        assert!(got
            .definitions
            .iter()
            .any(|definition| definition.symbol_name.as_deref() == Some("main")));
        assert_eq!(
            crate::knowledge_search::test_search_call_count(),
            searches_before,
            "overview must not enter lexical or semantic knowledge search"
        );
        #[cfg(feature = "semantic-knowledge")]
        assert_eq!(
            crate::inference::test_vector_model_load_call_count(),
            model_loads_before,
            "overview must not load a local vector model"
        );
    }

    #[test]
    fn unreadable_live_document_is_reported_but_missing_unindexed_candidate_is_omitted() {
        let root = tmpdir("overview-doc-errors");
        fs::write(root.join("README.md"), "read me\n").unwrap();
        let read = |root: &Path, path: &str, cap| {
            if path == "README.md" {
                Err(semantic::ConfinedReadError::Io(
                    "cannot read 'README.md': permission denied".into(),
                ))
            } else {
                semantic::confined_read_classified(root, path, cap)
            }
        };
        let (docs, _) = document_evidence_with(&root, &[], read);
        let readme = docs.iter().find(|e| e.path == "README.md").unwrap();
        assert_eq!(readme.status, EvidenceStatus::Unavailable);
        assert_eq!(readme.reason.as_deref(), Some("source-unavailable"));
        assert!(!docs.iter().any(|e| e.path == "Cargo.toml"));

        let unreadable_root = tmpdir("overview-doc-unreadable");
        fs::create_dir(unreadable_root.join("README.md")).unwrap();
        let (docs, _) = document_evidence(&unreadable_root, &[]);
        let readme = docs.iter().find(|e| e.path == "README.md").unwrap();
        assert_eq!(readme.status, EvidenceStatus::Unavailable);
        assert_eq!(readme.reason.as_deref(), Some("source-unavailable"));
    }

    #[test]
    fn document_candidate_reads_are_limited_and_only_consider_one_level() {
        let root = tmpdir("overview-document-read-limit");
        let mut files = Vec::new();
        for n in 0..200 {
            files.push(store::FileRow {
                path: format!("nested{n}/child/README.md"),
                language: "markdown".into(),
                bytes: 0,
                digest: String::new(),
                errors: 0,
            });
        }
        for n in 0..70 {
            files.push(store::FileRow {
                path: format!("level{n:02}/source.rs"),
                language: "rust".into(),
                bytes: 0,
                digest: String::new(),
                errors: 0,
            });
        }
        let reads = std::cell::RefCell::new(Vec::new());
        let (docs, omitted) = document_evidence_with(&root, &files, |_, path, _| {
            reads.borrow_mut().push(path.to_string());
            Ok(b"doc".to_vec())
        });

        assert!(reads.borrow().len() <= 64);
        assert!(reads
            .borrow()
            .iter()
            .all(|path| path.matches('/').count() <= 1));
        assert!(omitted > 0);
        assert!(docs.iter().all(|doc| doc.path.matches('/').count() <= 1));

        let mut value = empty("demo", SnapshotState::NoIndex, vec![], None);
        record_document_evidence(&mut value, docs, omitted);
        let omission = value
            .omissions
            .iter()
            .find(|o| o.section == OmissionSection::BuildTestSources)
            .unwrap();
        assert_eq!(omission.reason, OmissionReason::UnattemptedCandidatePaths);
        assert_eq!(omission.omitted_count, omitted);
    }

    #[test]
    fn root_manifest_is_read_before_one_level_candidates() {
        let root = tmpdir("overview-root-manifest-priority");
        fs::write(root.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        let files = (0..70)
            .map(|n| store::FileRow {
                path: format!("a{n:02}/source.rs"),
                language: "rust".into(),
                bytes: 0,
                digest: String::new(),
                errors: 0,
            })
            .collect::<Vec<_>>();
        let reads = std::cell::RefCell::new(Vec::new());
        let (docs, omitted) = document_evidence_with(&root, &files, |root, path, _| {
            reads.borrow_mut().push(path.to_string());
            if path == "package.json" {
                fs::read(root.join(path))
                    .map_err(|error| semantic::ConfinedReadError::Io(error.to_string()))
            } else {
                Err(semantic::ConfinedReadError::NotFound(path.to_string()))
            }
        });

        assert!(reads.borrow().len() <= 64);
        assert!(omitted > 0);
        assert!(reads.borrow().contains(&"package.json".to_string()));
        assert!(docs
            .iter()
            .any(|doc| doc.path == "package.json" && doc.status == EvidenceStatus::Available));
    }

    #[test]
    fn omitted_missing_candidates_are_serialized_as_unattempted_paths() {
        let root = tmpdir("overview-unattempted-candidates");
        let files = (0..70)
            .map(|n| store::FileRow {
                path: format!("a{n:02}/source.rs"),
                language: "rust".into(),
                bytes: 0,
                digest: String::new(),
                errors: 0,
            })
            .collect::<Vec<_>>();
        let reads = std::cell::Cell::new(0);
        let (docs, omitted) = document_evidence_with(&root, &files, |_, path, _| {
            reads.set(reads.get() + 1);
            Err(semantic::ConfinedReadError::NotFound(path.to_string()))
        });
        assert!(docs.is_empty());
        assert!(omitted > 0);
        assert_eq!(reads.get(), MAX_DOCUMENT_READS);

        let mut value = empty("demo", SnapshotState::NoIndex, vec![], None);
        record_document_evidence(&mut value, docs, omitted);
        let json = serde_json::to_value(value).unwrap();
        assert_eq!(
            json["omissions"][0]["reason"],
            "unattempted_candidate_paths"
        );
        assert_eq!(json["omissions"][0]["omitted_count"], omitted);
    }

    #[test]
    fn repeated_symbol_excerpts_load_and_hash_one_source_per_path() {
        let mut cache = BTreeMap::new();
        let reads = std::cell::Cell::new(0);
        for _ in 0..100 {
            let _ = cached_source(&mut cache, ("src/lib.rs".into(), "digest".into()), || {
                reads.set(reads.get() + 1);
                Ok(b"source".to_vec())
            });
        }
        assert_eq!(reads.get(), 1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn item_limit_is_reported_even_when_json_fits() {
        let mut value = empty("demo", SnapshotState::Fresh, vec![], Some("nonce".into()));
        value.languages = (0..21).map(|n| format!("lang{n:02}")).collect();
        let got = fit(value).unwrap();
        assert_eq!(got.languages.len(), 20);
        assert_eq!(
            got.omissions
                .iter()
                .find(|o| o.section == OmissionSection::Languages)
                .unwrap()
                .omitted_count,
            1
        );
        assert!(serde_json::to_vec(&got).unwrap().len() < context::DEFAULT_MAX_BYTES);
    }

    #[test]
    fn indexed_source_failures_keep_truthful_status_and_identity() {
        let root = tmpdir("overview-source-errors");
        fs::create_dir_all(root.join("src")).unwrap();
        let sym = store::SymbolRow {
            file_id: 1,
            kind: "function".into(),
            name: Some("main".into()),
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 2,
            byte_start: 0,
            byte_end: 1,
            provider: "syntax".into(),
            grammar: "test".into(),
            language: "rust".into(),
        };

        let missing_source = read_indexed_source(&root, "src/missing.rs", "digest");
        let missing = indexed_evidence(
            "src/missing.rs",
            &sym,
            &missing_source,
            EvidenceKind::Definition,
        );
        assert_eq!(missing.reason.as_deref(), Some("source-unavailable"));
        assert_eq!(missing.symbol_name.as_deref(), Some("main"));

        let escaped_source = read_indexed_source(&root, "../escape.rs", "digest");
        let escaped = indexed_evidence(
            "../escape.rs",
            &sym,
            &escaped_source,
            EvidenceKind::Definition,
        );
        assert_eq!(escaped.reason.as_deref(), Some("unsafe-path"));

        fs::write(root.join("src/nonutf8.rs"), [0xff, 0xfe]).unwrap();
        let nonutf8_source = read_indexed_source(
            &root,
            "src/nonutf8.rs",
            &store::content_digest(&[0xff, 0xfe]),
        );
        let nonutf8 = indexed_evidence(
            "src/nonutf8.rs",
            &sym,
            &nonutf8_source,
            EvidenceKind::Definition,
        );
        assert_eq!(nonutf8.reason.as_deref(), Some("source-non-utf8"));

        let large = vec![b'x'; MAX_DOCUMENT_BYTES as usize + 1];
        fs::write(root.join("src/large.rs"), &large).unwrap();
        let oversized_source =
            read_indexed_source(&root, "src/large.rs", &store::content_digest(&large));
        let oversized = indexed_evidence(
            "src/large.rs",
            &sym,
            &oversized_source,
            EvidenceKind::Definition,
        );
        assert_eq!(oversized.reason.as_deref(), Some("source-too-large"));

        let outside = tmpdir("overview-source-outside");
        fs::write(outside.join("escape.rs"), "fn main() {}\n").unwrap();
        let link = root.join("src/escape.rs");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(outside.join("escape.rs"), &link).is_ok();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(outside.join("escape.rs"), &link).is_ok();
        if linked {
            let escaped_source = read_indexed_source(&root, "src/escape.rs", "digest");
            let escaped = indexed_evidence(
                "src/escape.rs",
                &sym,
                &escaped_source,
                EvidenceKind::Definition,
            );
            assert_eq!(escaped.reason.as_deref(), Some("unsafe-path"));
        }
    }

    #[test]
    fn changed_snapshot_source_keeps_definition_identity_and_marks_excerpt_unavailable() {
        let home = tmpdir("overview-snapshot-home");
        let root = tmpdir("overview-snapshot-root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        index::build(&home, "demo", &root, false).unwrap();
        let indexed = collect(&home, "demo", &root).unwrap();
        assert_eq!(indexed.snapshot.status, SnapshotState::Stale);
        let main = indexed
            .definitions
            .iter()
            .find(|d| d.symbol_name.as_deref() == Some("main"))
            .unwrap();
        assert_eq!(main.status, EvidenceStatus::Available);
        fs::write(root.join("src/main.rs"), "fn main() { changed(); }\n").unwrap();
        let changed = collect(&home, "demo", &root).unwrap();
        assert_eq!(changed.snapshot.status, SnapshotState::Stale);
        assert!(changed
            .snapshot
            .reasons
            .iter()
            .any(|reason| reason == "content-changed"));
        let main = changed
            .definitions
            .iter()
            .find(|d| d.symbol_name.as_deref() == Some("main"))
            .unwrap();
        assert_eq!(main.status, EvidenceStatus::Unavailable);
        assert_eq!(main.reason.as_deref(), Some("source-changed"));
        assert_eq!(main.byte_start, Some(0));
        assert_eq!(main.byte_end, Some(12));
    }

    #[test]
    fn snippet_constants_match_packet_limits_and_excerpt_is_utf8_safe() {
        let text = "é".repeat(2100);
        let (excerpt, end) = packet_source::bounded_excerpt(&text, 0, text.len());
        assert_eq!(excerpt.len(), packet_source::MAX_SNIPPET_BYTES);
        assert_eq!(end, excerpt.len());
        assert!(text.is_char_boundary(end));
        assert_eq!(packet_source::MAX_SNIPPET_LINES, 40);
    }
}
