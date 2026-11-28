// SPDX-License-Identifier: Apache-2.0
//! Read-only context packets for explicitly selected code anchors.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::{context, index, registry, store};

pub const MAX_ANCHORS: usize = 5;
pub const MAX_ANCHOR_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnchorInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, rename = "in", skip_serializing_if = "Option::is_none")]
    pub in_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

pub struct PacketRequest {
    pub anchors: Vec<AnchorInput>,
    pub depth: usize,
    pub limit: usize,
    pub max_bytes: usize,
}

pub fn parse_anchors(text: &str) -> Result<Vec<AnchorInput>, String> {
    if text.len() > MAX_ANCHOR_BYTES {
        return Err("anchors JSON exceeds 16384 bytes".to_string());
    }
    let anchors: Vec<AnchorInput> =
        serde_json::from_str(text).map_err(|e| format!("invalid anchors JSON: {e}"))?;
    validate_anchors(&anchors)?;
    Ok(anchors)
}

pub fn validate_request(request: &PacketRequest) -> Result<(), String> {
    validate_anchors(&request.anchors)?;
    context::validate_options(request.depth, request.limit, request.max_bytes, None, false)
}

/// Transport callers also run this before returning a no-index document.
pub fn validate_paths(root: &Path, request: &PacketRequest) -> Result<(), String> {
    for selector in &request.anchors {
        for path in [&selector.path, &selector.in_path].into_iter().flatten() {
            let rel = path.replace('\\', "/");
            if rel.starts_with('/') || rel.contains(['\0', ':']) {
                return Err("document path not relative/portable".to_string());
            }
            crate::semantic::confined_path(root, &rel)?;
        }
    }
    Ok(())
}

fn validate_anchors(anchors: &[AnchorInput]) -> Result<(), String> {
    if anchors.is_empty() || anchors.len() > MAX_ANCHORS {
        return Err("anchors must contain 1-5 selectors".to_string());
    }
    for anchor in anchors {
        if [
            &anchor.path,
            &anchor.symbol,
            &anchor.target,
            &anchor.in_path,
            &anchor.kind,
        ]
        .into_iter()
        .flatten()
        .any(|value| value.trim().is_empty())
        {
            return Err("anchor selector contains an empty value".to_string());
        }
        let valid = if anchor.target.is_some() {
            anchor.path.is_none()
                && anchor.line.is_none()
                && anchor.col.is_none()
                && anchor.symbol.is_none()
        } else {
            anchor.path.is_some()
                && anchor.line.is_some_and(|n| n > 0)
                && anchor.col.is_some_and(|n| n > 0)
                && anchor.in_path.is_none()
                && anchor.kind.is_none()
        };
        if !valid {
            return Err(
                "anchor requires either path/line/col or target with optional in/kind".to_string(),
            );
        }
    }
    if serde_json::to_vec(anchors)
        .map_err(|e| e.to_string())?
        .len()
        > MAX_ANCHOR_BYTES
    {
        return Err("normalized anchors exceed 16384 bytes".to_string());
    }
    Ok(())
}

pub struct ResolvedAnchor {
    pub path: String,
    pub line: u32,
    pub col: u32,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

pub struct AnchorOutcome {
    pub selector: AnchorInput,
    pub status: &'static str,
    pub resolved: Option<ResolvedAnchor>,
    pub candidates: Vec<index::TargetCandidate>,
    pub total: usize,
    pub total_exact: bool,
    pub candidates_truncated: bool,
    pub context: Option<context::ContextResult>,
    pub context_unavailable: Option<&'static str>,
    pub source_range: Option<ByteRange>,
    pub snippet: Option<usize>,
    pub source_reason: Option<&'static str>,
}

#[derive(Clone)]
pub struct SourceSnippet {
    pub path: String,
    pub digest: String,
    pub requested: ByteRange,
    pub returned: Option<ByteRange>,
    pub text: Option<String>,
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub anchor_indices: Vec<usize>,
}

pub struct PacketResult {
    pub repo: String,
    pub snapshot: Option<store::Snapshot>,
    pub revision: Option<String>,
    pub anchors: Vec<AnchorOutcome>,
    pub snippets: Vec<SourceSnippet>,
    pub max_bytes: usize,
    pub depth: usize,
    pub limit: usize,
}

fn empty_outcome(selector: &AnchorInput) -> AnchorOutcome {
    AnchorOutcome {
        selector: selector.clone(),
        status: "no-index",
        resolved: None,
        candidates: Vec::new(),
        total: 0,
        total_exact: false,
        candidates_truncated: false,
        context: None,
        context_unavailable: None,
        source_range: None,
        snippet: None,
        source_reason: None,
    }
}

pub fn no_index(repo: &str, request: &PacketRequest) -> PacketResult {
    PacketResult {
        repo: repo.to_string(),
        snapshot: None,
        revision: None,
        anchors: request.anchors.iter().map(empty_outcome).collect(),
        snippets: Vec::new(),
        max_bytes: request.max_bytes,
        depth: request.depth,
        limit: request.limit,
    }
}

struct SourceRead {
    digest: String,
    bytes: Option<Vec<u8>>,
    reason: Option<&'static str>,
}

fn read_source(root: &Path, path: &str, digest: String) -> Result<SourceRead, PacketQueryError> {
    read_source_using(root, path, digest, || {
        crate::semantic::confined_read_classified(root, path, crate::scan::MAX_FILE_BYTES)
    })
}

fn read_source_using(
    root: &Path,
    path: &str,
    digest: String,
    read: impl FnOnce() -> Result<Vec<u8>, crate::semantic::ConfinedReadError>,
) -> Result<SourceRead, PacketQueryError> {
    crate::semantic::confined_path(root, path).map_err(PacketQueryError::Path)?;
    let (bytes, reason) = match read() {
        Ok(bytes) => {
            // The content check is not a substitute for path confinement.
            crate::semantic::confined_path(root, path).map_err(PacketQueryError::Path)?;
            if store::content_digest(&bytes) != digest {
                (None, Some("source-changed"))
            } else if std::str::from_utf8(&bytes).is_err() {
                (None, Some("source-non-utf8"))
            } else {
                (Some(bytes), None)
            }
        }
        Err(crate::semantic::ConfinedReadError::Path(message)) => {
            return Err(PacketQueryError::Path(message))
        }
        Err(
            crate::semantic::ConfinedReadError::NotFound(_)
            | crate::semantic::ConfinedReadError::Io(_),
        ) => {
            crate::semantic::confined_path(root, path).map_err(PacketQueryError::Path)?;
            (None, Some("source-unavailable"))
        }
    };
    Ok(SourceRead {
        digest,
        bytes,
        reason,
    })
}

fn indexed_start_span(
    tx: &rusqlite::Transaction<'_>,
    snapshot: i64,
    path: &str,
    line: u32,
    col: u32,
) -> Result<index::SpanOutcome, String> {
    let spans: BTreeSet<_> = store::symbols_at(tx, snapshot, path)?
        .into_iter()
        .filter(|hit| hit.start_line == line && hit.start_col == col)
        .map(|hit| (hit.byte_start, hit.byte_end))
        .collect();
    Ok(if spans.len() == 1 {
        let &(start, end) = spans.first().expect("one span");
        index::SpanOutcome::Span(start, end)
    } else if spans.is_empty() {
        index::SpanOutcome::Missing
    } else {
        index::SpanOutcome::Ambiguous(spans.len())
    })
}

#[derive(Debug)]
pub(crate) enum PacketQueryError {
    Path(String),
    Store(String),
}

impl From<String> for PacketQueryError {
    fn from(message: String) -> Self {
        Self::Store(message)
    }
}

impl From<context::PinnedContextError> for PacketQueryError {
    fn from(error: context::PinnedContextError) -> Self {
        match error {
            context::PinnedContextError::Path(message) => Self::Path(message),
            context::PinnedContextError::Store(message) => Self::Store(message),
        }
    }
}

impl PacketQueryError {
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Path(message) | Self::Store(message) => message,
        }
    }
}

pub fn query_conn(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &PacketRequest,
) -> Result<PacketResult, String> {
    query_conn_classified(conn, home, entry, repo, root, request)
        .map_err(PacketQueryError::into_message)
}

pub(crate) fn query_conn_classified(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &PacketRequest,
) -> Result<PacketResult, PacketQueryError> {
    query_conn_inner(conn, home, entry, repo, root, request, |_| {}, |_, _| {})
}

#[cfg(test)]
pub(crate) fn query_conn_classified_with_hook(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &PacketRequest,
    after_anchor: impl FnMut(usize),
) -> Result<PacketResult, PacketQueryError> {
    query_conn_inner(
        conn,
        home,
        entry,
        repo,
        root,
        request,
        after_anchor,
        |_, _| {},
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)] // Test hooks model a change between anchor resolutions.
pub(crate) fn query_conn_classified_with_hooks(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &PacketRequest,
    after_anchor: impl FnMut(usize),
    between_resolutions: impl FnMut(usize, &rusqlite::Transaction<'_>),
) -> Result<PacketResult, PacketQueryError> {
    let _parse_once = crate::anchor::ParseOnce::hold();
    query_conn_inner(
        conn,
        home,
        entry,
        repo,
        root,
        request,
        after_anchor,
        between_resolutions,
    )
}

#[allow(clippy::too_many_arguments)]
fn query_conn_inner(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &PacketRequest,
    mut after_anchor: impl FnMut(usize),
    mut between_resolutions: impl FnMut(usize, &rusqlite::Transaction<'_>),
) -> Result<PacketResult, PacketQueryError> {
    validate_request(request)?;
    validate_paths(root, request).map_err(PacketQueryError::Path)?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let snapshot = store::latest_snapshot(&tx)?;
    let mut sources = BTreeMap::<String, SourceRead>::new();
    let revision = snapshot
        .as_ref()
        .map(|s| index::live_state_for(&tx, s).map(|live| crate::semantic::revision_string(&live)))
        .transpose()?;
    let mut result = PacketResult {
        repo: repo.to_string(),
        snapshot,
        revision,
        anchors: Vec::new(),
        snippets: Vec::new(),
        max_bytes: request.max_bytes,
        depth: request.depth,
        limit: request.limit,
    };
    for selector in &request.anchors {
        let mut outcome = empty_outcome(selector);
        if let Some(snapshot) = result.snapshot.as_ref() {
            let resolution = if let Some(target) = selector.target.as_deref() {
                let in_path = selector
                    .in_path
                    .as_deref()
                    .map(|path| path.replace('\\', "/"));
                index::resolve_target_data(
                    &tx,
                    &index::TargetQuery {
                        target,
                        in_path: in_path.as_deref(),
                        kind: selector.kind.as_deref(),
                    },
                )?
            } else {
                index::TargetOutcome::One {
                    path: selector.path.clone().expect("validated position"),
                    line: selector.line.expect("validated line"),
                    col: selector.col.expect("validated column"),
                }
            };
            match resolution {
                index::TargetOutcome::One { path, line, col } => {
                    let path = path.replace('\\', "/");
                    if !sources.contains_key(&path) {
                        if let Some((_, digest)) = store::file_identity(&tx, snapshot.id, &path)? {
                            sources.insert(path.clone(), read_source(root, &path, digest)?);
                        }
                    }
                    let unavailable = sources.get(&path).and_then(|source| source.reason);
                    if let Some(reason) = unavailable {
                        // Do not turn a stale cursor (or a caller-supplied symbol
                        // hint) into a graph identity. A unique stored byte span at
                        // the exact position can still be reported as stale source
                        // metadata; it is not used to construct context facts.
                        let identity = indexed_start_span(&tx, snapshot.id, &path, line, col)?;
                        let range = match identity {
                            index::SpanOutcome::Span(start, end) if selector.symbol.is_none() => {
                                outcome.status = "resolved";
                                outcome.total = 1;
                                outcome.total_exact = true;
                                Some(ByteRange { start, end })
                            }
                            unavailable => {
                                outcome.status = "unavailable";
                                outcome.source_reason = Some(match unavailable {
                                    index::SpanOutcome::Ambiguous(_) => "ambiguous-syntax-identity",
                                    _ => "no-syntax-identity",
                                });
                                None
                            }
                        };
                        outcome.resolved = Some(ResolvedAnchor {
                            path,
                            line,
                            col,
                            symbol: selector.symbol.clone(),
                        });
                        outcome.source_range = range;
                        outcome.source_reason.get_or_insert(reason);
                        outcome.context_unavailable = Some(reason);
                        result.anchors.push(outcome);
                        after_anchor(result.anchors.len() - 1);
                        continue;
                    }
                    let anchor_index = result.anchors.len();
                    let context = context::query_pinned_with_hook(
                        &tx,
                        home,
                        entry,
                        repo,
                        root,
                        &context::ContextRequest {
                            path: &path,
                            line,
                            col,
                            symbol: selector.symbol.as_deref(),
                            query: None,
                            semantic: false,
                            by_name: selector.target.is_some(),
                            depth: request.depth,
                            limit: request.limit,
                            max_bytes: request.max_bytes,
                        },
                        |tx| between_resolutions(anchor_index, tx),
                    )?;
                    match &context.facts.identity {
                        index::FactIdentity::Span {
                            byte_start,
                            byte_end,
                            ..
                        } => {
                            outcome.source_range = Some(ByteRange {
                                start: *byte_start,
                                end: *byte_end,
                            });
                        }
                        index::FactIdentity::Unavailable { reason, .. } => {
                            outcome.source_reason = Some(reason)
                        }
                    }
                    result
                        .revision
                        .get_or_insert_with(|| context.trace.revision.clone());
                    outcome.status = "resolved";
                    outcome.total = 1;
                    outcome.total_exact = true;
                    outcome.resolved = Some(ResolvedAnchor {
                        path,
                        line,
                        col,
                        symbol: selector.symbol.clone(),
                    });
                    outcome.context = Some(context);
                }
                index::TargetOutcome::Unresolved {
                    status,
                    total,
                    total_exact,
                    candidates,
                    truncated,
                } => {
                    outcome.status = status;
                    outcome.total = total;
                    outcome.total_exact = total_exact;
                    outcome.candidates = candidates;
                    outcome.candidates_truncated = truncated;
                }
                index::TargetOutcome::NoIndex => {}
            }
        }
        result.anchors.push(outcome);
        after_anchor(result.anchors.len() - 1);
    }
    let mut inputs = Vec::new();
    for (i, outcome) in result.anchors.iter().enumerate() {
        if let (Some(anchor), Some(range)) = (&outcome.resolved, &outcome.source_range) {
            if let Some(source) = sources.get(&anchor.path) {
                inputs.push(crate::packet_source::SnippetInput {
                    path: &anchor.path,
                    digest: &source.digest,
                    requested: range.clone(),
                    source: source
                        .bytes
                        .as_deref()
                        .ok_or(source.reason.unwrap_or("source-unavailable")),
                    anchor_index: i,
                });
            }
        }
    }
    result.snippets = crate::packet_source::assemble(inputs)?;
    for (snippet_id, snippet) in result.snippets.iter().enumerate() {
        for &anchor_index in &snippet.anchor_indices {
            if snippet.reason == Some("invalid-source-range") {
                result.anchors[anchor_index].source_range = None;
                result.anchors[anchor_index].source_reason = Some("invalid-source-range");
            }
            result.anchors[anchor_index].snippet = Some(snippet_id);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_classifies_removed_root_after_anchor_as_path_error() {
        let home = crate::testutil::tmpdir("packet-removed-root-home");
        let root = crate::testutil::tmpdir("packet-removed-root")
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        std::fs::write(root.join("b.py"), "def beta():\n    return 2\n").unwrap();
        index::build_full(&home, "r", &root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let request = PacketRequest {
            anchors: parse_anchors(r#"[{"target":"alpha"},{"target":"beta"}]"#).unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        let parked = root.with_extension("parked");
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let result = query_conn_classified_with_hook(
            &mut conn,
            &home,
            &entry,
            "r",
            &root,
            &request,
            |anchor| {
                if anchor == 0 {
                    std::fs::rename(&root, &parked).unwrap();
                }
            },
        );
        std::fs::rename(&parked, &root).unwrap();
        assert!(
            matches!(result, Err(PacketQueryError::Path(_))),
            "removing the root after the first distinct-path anchor must remain a path refusal"
        );
    }

    #[test]
    fn packet_preserves_detected_confinement_failure_after_path_restoration() {
        let root = crate::testutil::tmpdir("packet-restored-path")
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), b"inside").unwrap();
        let result = read_source_using(&root, "a.py", store::content_digest(b"inside"), || {
            Err(crate::semantic::ConfinedReadError::Path(
                "detected before restoration".to_string(),
            ))
        });
        assert!(
            matches!(result, Err(PacketQueryError::Path(_))),
            "a typed path refusal cannot become unavailable even though the path is safe now"
        );
    }

    #[test]
    fn packet_does_not_classify_read_errors_by_message() {
        let root = crate::testutil::tmpdir("packet-io-message")
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), b"hello").unwrap();
        for message in [
            "not found",
            "too large",
            "document path escapes repository",
            "different diagnostic",
        ] {
            let source = read_source_using(&root, "a.py", "index-digest".to_string(), || {
                Err(crate::semantic::ConfinedReadError::Io(message.to_string()))
            })
            .unwrap();
            assert_eq!(source.reason, Some("source-unavailable"));
            assert!(source.bytes.is_none());
        }
        let read = std::cell::Cell::new(false);
        assert!(
            read_source_using(&root, "../a.py", "index-digest".to_string(), || {
                read.set(true);
                Ok(b"must not be read".to_vec())
            })
            .is_err()
        );
        assert!(!read.get());
    }

    #[test]
    fn packet_refuses_link_escape_between_preflight_and_read() {
        let root = crate::testutil::tmpdir("packet-link-root")
            .canonicalize()
            .unwrap();
        let outside = crate::testutil::tmpdir("packet-link-outside");
        std::fs::write(root.join("a.py"), b"inside").unwrap();
        std::fs::write(outside.join("secret.py"), b"outside").unwrap();
        let probe = root.join("probe");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.join("secret.py"), &probe).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(outside.join("secret.py"), &probe).is_ok();
        #[cfg(not(any(unix, windows)))]
        let made = false;
        if !made {
            eprintln!("packet link-race test not exercised: symlink capability unavailable");
            return;
        }
        let result = read_source_using(&root, "a.py", store::content_digest(b"inside"), || {
            std::fs::remove_file(root.join("a.py")).unwrap();
            std::fs::rename(&probe, root.join("a.py")).unwrap();
            crate::semantic::confined_read_classified(&root, "a.py", crate::scan::MAX_FILE_BYTES)
        });
        assert!(
            matches!(result, Err(PacketQueryError::Path(_))),
            "escape must refuse the packet, not become source-unavailable"
        );
    }

    #[cfg(windows)]
    #[test]
    fn packet_refuses_junction_escape_between_preflight_and_read() {
        let root = crate::testutil::tmpdir("packet-junction-root")
            .canonicalize()
            .unwrap();
        let outside = crate::testutil::tmpdir("packet-junction-outside")
            .canonicalize()
            .unwrap();
        let linked = root.join("linked");
        std::fs::create_dir(&linked).unwrap();
        std::fs::write(linked.join("a.py"), b"inside").unwrap();
        std::fs::write(outside.join("a.py"), b"outside").unwrap();
        let result = read_source_using(
            &root,
            "linked/a.py",
            store::content_digest(b"inside"),
            || {
                std::fs::remove_file(linked.join("a.py")).unwrap();
                std::fs::remove_dir(&linked).unwrap();
                let made = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(&linked)
                    .arg(&outside)
                    .output()
                    .expect("mklink");
                assert!(
                    made.status.success(),
                    "{}",
                    String::from_utf8_lossy(&made.stderr)
                );
                let refused = crate::semantic::confined_read_classified(
                    &root,
                    "linked/a.py",
                    crate::scan::MAX_FILE_BYTES,
                );
                // Restore the safe path before the outer reader can recheck it.
                // The previously detected refusal must still survive.
                std::fs::remove_dir(&linked).unwrap();
                std::fs::create_dir(&linked).unwrap();
                std::fs::write(linked.join("a.py"), b"inside").unwrap();
                refused
            },
        );
        assert!(
            matches!(result, Err(PacketQueryError::Path(_))),
            "junction escape must refuse the entire packet"
        );
    }

    #[test]
    fn packet_unavailable_source_without_syntax_start_is_an_outcome() {
        let home = crate::testutil::tmpdir("packet-no-start-home");
        let root = crate::testutil::tmpdir("packet-no-start-root")
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        index::build_full(&home, "r", &root).unwrap();
        std::fs::write(root.join("a.py"), "changed\n").unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let request = PacketRequest {
            anchors: parse_anchors(r#"[{"path":"a.py","line":2,"col":1}]"#).unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        let result = query_conn(&mut conn, &home, &entry, "r", &root, &request).unwrap();
        assert_eq!(result.anchors[0].status, "unavailable");
        assert_eq!(result.anchors[0].source_reason, Some("no-syntax-identity"));
        assert!(result.anchors[0].source_range.is_none());
        assert!(result.anchors[0].context.is_none());
        assert!(result.snippets.is_empty());
    }

    #[test]
    fn packet_unavailable_source_reports_ambiguous_indexed_spans() {
        let home = crate::testutil::tmpdir("packet-ambiguous-start-home");
        let root = crate::testutil::tmpdir("packet-ambiguous-start-root")
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        index::build_full(&home, "r", &root).unwrap();
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        conn.execute(
            "INSERT INTO symbols (snapshot_id,file_id,kind,name,start_line,start_col,end_line,end_col,byte_start,byte_end,provider,grammar,language) SELECT snapshot_id,file_id,kind,'decoy',start_line,start_col,end_line,end_col,byte_start,byte_end-1,provider,grammar,language FROM symbols WHERE name='alpha'", []
        ).unwrap();
        std::fs::write(root.join("a.py"), "changed\n").unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let request = PacketRequest {
            anchors: parse_anchors(r#"[{"path":"a.py","line":1,"col":1}]"#).unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        let result = query_conn(&mut conn, &home, &entry, "r", &root, &request).unwrap();
        assert_eq!(result.anchors[0].status, "unavailable");
        assert_eq!(
            result.anchors[0].source_reason,
            Some("ambiguous-syntax-identity")
        );
        assert!(result.anchors[0].source_range.is_none());
        assert!(result.snippets.is_empty());
    }

    #[test]
    fn packet_withholds_changed_or_unavailable_source() {
        for mode in ["changed", "missing", "too-large"] {
            let home = crate::testutil::tmpdir(&format!("packet-{mode}-home"));
            let root = crate::testutil::tmpdir(&format!("packet-{mode}-root"))
                .canonicalize()
                .unwrap();
            std::fs::write(root.join("a.py"), "def alpha():\n    return 1\n").unwrap();
            index::build_full(&home, "r", &root).unwrap();
            let entry = registry::RepoEntry {
                name: "r".to_string(),
                root: root.to_string_lossy().into_owned(),
                added_unix: 0,
            };
            match mode {
                "changed" => {
                    std::fs::write(root.join("a.py"), "def alpha():\n    return 2\n").unwrap()
                }
                "missing" => std::fs::remove_file(root.join("a.py")).unwrap(),
                _ => std::fs::write(
                    root.join("a.py"),
                    vec![b'a'; crate::scan::MAX_FILE_BYTES as usize + 1],
                )
                .unwrap(),
            }
            let mut conn = match store::open_read(&store::db_path(&home, "r")) {
                store::IndexRead::Ready(conn) => conn,
                _ => panic!("index missing"),
            };
            let request = PacketRequest {
                anchors: parse_anchors(r#"[{"target":"alpha"}]"#).unwrap(),
                depth: 3,
                limit: 20,
                max_bytes: 32768,
            };
            let result = query_conn(&mut conn, &home, &entry, "r", &root, &request).unwrap();
            assert_eq!(result.anchors[0].status, "resolved", "{mode}");
            assert!(result.anchors[0].context.is_none());
            assert!(result.anchors[0].source_range.is_some(), "{mode}");
            assert_eq!(result.snippets.len(), 1);
            assert_eq!(result.snippets[0].status, "unavailable");
            assert!(result.snippets[0].text.is_none());
            assert!(result.snippets[0].returned.is_none());
            assert_eq!(
                result.snippets[0].reason,
                Some(if mode == "changed" {
                    "source-changed"
                } else {
                    "source-unavailable"
                })
            );
            if mode == "changed" {
                assert_eq!(result.anchors[0].source_reason, Some("source-changed"));
                assert_eq!(
                    result.anchors[0].context_unavailable,
                    Some("source-changed")
                );
            }
        }
    }

    #[test]
    fn packet_does_not_guess_stale_identity_from_symbol_hint() {
        let home = crate::testutil::tmpdir("packet-stale-symbol-home");
        let root = crate::testutil::tmpdir("packet-stale-symbol-root")
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        index::build_full(&home, "r", &root).unwrap();
        std::fs::write(root.join("a.py"), "def alpha():\n    return 2\n").unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let request = PacketRequest {
            anchors: parse_anchors(
                r#"[{"path":"a.py","line":1,"col":1},{"path":"a.py","line":1,"col":1,"symbol":"alpha"}]"#,
            )
            .unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        let result = query_conn(&mut conn, &home, &entry, "r", &root, &request).unwrap();
        assert_eq!(result.anchors[0].status, "resolved");
        assert!(result.anchors[0].source_range.is_some());
        assert_eq!(result.anchors[0].source_reason, Some("source-changed"));
        assert_eq!(result.anchors[1].status, "unavailable");
        assert!(result.anchors[1].source_range.is_none());
        assert_eq!(result.anchors[1].source_reason, Some("no-syntax-identity"));
        let doc: serde_json::Value =
            serde_json::from_str(&crate::response::packet_query_json(&result).unwrap()).unwrap();
        assert_eq!(doc["anchors"][0]["context_evidence"]["freshness"], "stale");
        assert_eq!(
            doc["anchors"][0]["follow_up"]["source"]["reason"],
            "source-changed"
        );
        assert!(doc["anchors"][1]["follow_up"]["source"].is_null());
    }

    #[test]
    fn packet_keeps_other_anchors_when_one_indexed_range_is_invalid() {
        let home = crate::testutil::tmpdir("packet-invalid-range-home");
        let root = crate::testutil::tmpdir("packet-invalid-range-root")
            .canonicalize()
            .unwrap();
        std::fs::write(
            root.join("a.py"),
            "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
        )
        .unwrap();
        index::build_full(&home, "r", &root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        conn.execute(
            "UPDATE symbols SET byte_end = 99999 WHERE name = 'alpha'",
            [],
        )
        .unwrap();
        let request = PacketRequest {
            anchors: parse_anchors(r#"[{"target":"alpha"},{"target":"beta"}]"#).unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        let result = query_conn(&mut conn, &home, &entry, "r", &root, &request).unwrap();
        assert_eq!(result.anchors.len(), 2);
        assert_eq!(result.anchors[0].status, "resolved");
        assert_eq!(result.anchors[0].source_range, None);
        assert_eq!(
            result.anchors[0].source_reason,
            Some("invalid-source-range")
        );
        assert_eq!(result.anchors[1].status, "resolved");
        assert_eq!(result.anchors[1].source_reason, None);
        assert_eq!(result.snippets.len(), 2);
        let alpha = result
            .snippets
            .iter()
            .find(|snippet| snippet.anchor_indices == [0])
            .unwrap();
        assert_eq!(alpha.status, "unavailable");
        assert_eq!(alpha.reason, Some("invalid-source-range"));
        assert_eq!(alpha.text, None);
        let beta = result
            .snippets
            .iter()
            .find(|snippet| snippet.anchor_indices == [1])
            .unwrap();
        assert_eq!(beta.status, "available");
        assert!(beta.text.is_some());
        let doc: serde_json::Value =
            serde_json::from_str(&crate::response::packet_query_json(&result).unwrap()).unwrap();
        assert!(doc["anchors"][0]["follow_up"]["source"].is_null());
        assert!(doc["anchors"][1]["follow_up"]["source"].is_object());
    }

    #[test]
    fn packet_no_snapshot_still_refuses_unsafe_paths() {
        let home = crate::testutil::tmpdir("packet-no-snapshot-home");
        let root = crate::testutil::tmpdir("packet-no-snapshot-root")
            .canonicalize()
            .unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        std::fs::create_dir_all(home.join("index")).unwrap();
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        for text in [
            r#"[{"path":"../escape.py","line":1,"col":1}]"#,
            r#"[{"target":"alpha","in":"../escape.py"}]"#,
            r#"[{"target":"alpha","in":"C:/escape.py"}]"#,
        ] {
            let request = PacketRequest {
                anchors: parse_anchors(text).unwrap(),
                depth: 3,
                limit: 20,
                max_bytes: 32768,
            };
            assert!(query_conn(&mut conn, &home, &entry, "r", &root, &request).is_err());
        }
        let absent = PacketRequest {
            anchors: parse_anchors(r#"[{"path":"missing/leaf.py","line":1,"col":1}]"#).unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        validate_paths(&root, &absent).unwrap();
        let result = query_conn(&mut conn, &home, &entry, "r", &root, &absent).unwrap();
        assert!(result.snapshot.is_none());
        assert_eq!(result.anchors[0].status, "no-index");
    }

    #[test]
    fn packet_pins_every_anchor_during_reindex_and_labels_knowledge_generations() {
        let home = crate::testutil::tmpdir("packet-pin-home");
        let root = crate::testutil::tmpdir("packet-pin-root")
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        index::build_full(&home, "r", &root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut knowledge = crate::knowledge_store::open_knowledge_at(&home, &entry, true).unwrap();
        let mut code = store::open_live(&store::db_path(&home, "r")).unwrap();
        let nonce = store::latest_snapshot(&code).unwrap().unwrap().nonce;
        let request = PacketRequest {
            anchors: parse_anchors(r#"[{"target":"alpha"},{"target":"alpha"}]"#).unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        let result = query_conn_inner(
            &mut code,
            &home,
            &entry,
            "r",
            &root,
            &request,
            |i| {
                if i == 0 {
                    std::fs::write(root.join("b.py"), "def newer():\n    pass\n").unwrap();
                    index::build_full(&home, "r", &root).unwrap();
                    crate::knowledge_store::create_record(
                        &mut knowledge,
                        &crate::knowledge::NewRecord {
                            id: "new-note".to_string(),
                            kind: "decision".to_string(),
                            title: "New note".to_string(),
                            body: "recorded later".to_string(),
                            status: "active".to_string(),
                            author: "test".to_string(),
                        },
                    )
                    .unwrap();
                }
            },
            |_, _| {},
        )
        .unwrap();
        let first = result.anchors[0].context.as_ref().unwrap();
        let second = result.anchors[1].context.as_ref().unwrap();
        assert_eq!(first.trace.snapshot.nonce, nonce);
        assert_eq!(second.trace.snapshot.nonce, nonce);
        assert_ne!(first.knowledge.generation, second.knowledge.generation);
        let live = store::open_live(&store::db_path(&home, "r")).unwrap();
        assert_ne!(store::latest_snapshot(&live).unwrap().unwrap().nonce, nonce);
    }

    #[test]
    fn packet_query_collects_mixed_outcomes_on_one_snapshot() {
        let home = crate::testutil::tmpdir("packet-core-home");
        let root = crate::testutil::tmpdir("packet-core-root")
            .canonicalize()
            .unwrap();
        std::fs::write(
            root.join("a.py"),
            "def alpha():\n    return 1\ndef shared():\n    pass\n",
        )
        .unwrap();
        std::fs::write(root.join("b.py"), "def shared():\n    pass\n").unwrap();
        index::build_full(&home, "r", &root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut conn = match store::open_read(&store::db_path(&home, "r")) {
            store::IndexRead::Ready(conn) => conn,
            _ => panic!("fresh index not readable"),
        };
        let request = PacketRequest {
            anchors: parse_anchors(
                r#"[{"target":"alpha"},{"target":"shared"},{"target":"missing"}]"#,
            )
            .unwrap(),
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        let result = query_conn(&mut conn, &home, &entry, "r", &root, &request).unwrap();
        assert_eq!(
            result.anchors.iter().map(|a| a.status).collect::<Vec<_>>(),
            ["resolved", "ambiguous", "not-found"]
        );
        assert_eq!(result.anchors[1].total, 2);
        assert_eq!(result.anchors[1].candidates.len(), 2);
        let context = result.anchors[0].context.as_ref().unwrap();
        assert_eq!(
            context.trace.snapshot.nonce,
            result.snapshot.as_ref().unwrap().nonce
        );
        assert!(!context.knowledge.query.requested);
        assert!(!context.knowledge.semantic.requested);
    }

    #[test]
    fn packet_selector_validation_and_normalized_limits() {
        let valid = parse_anchors(r#"[{"target":"alpha","in":"a.py"}]"#).unwrap();
        let request = PacketRequest {
            anchors: valid,
            depth: 3,
            limit: 20,
            max_bytes: 32768,
        };
        validate_request(&request).unwrap();
        for value in [
            r#"[]"#,
            r#"[{"target":"alpha","path":"a.py","line":1,"col":1}]"#,
            r#"[{"path":"a.py","line":1,"col":0}]"#,
            r#"[{"target":"alpha","semantic":true}]"#,
            r#"[{"target":" "}]"#,
        ] {
            assert!(parse_anchors(value).is_err(), "{value}");
        }
        let duplicate = parse_anchors(r#"[{"target":"alpha"},{"target":"alpha"}]"#).unwrap();
        assert_eq!(duplicate.len(), 2);
        let nulls = parse_anchors(r#"[{"target":"alpha","path":null}]"#).unwrap();
        assert_eq!(
            serde_json::to_string(&nulls).unwrap(),
            r#"[{"target":"alpha"}]"#
        );
        let mut invalid = request;
        invalid.anchors.resize(5, invalid.anchors[0].clone());
        validate_request(&invalid).unwrap();
        invalid.anchors.resize(6, invalid.anchors[0].clone());
        assert!(validate_request(&invalid).is_err());
    }
}
