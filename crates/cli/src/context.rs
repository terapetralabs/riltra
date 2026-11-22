// SPDX-License-Identifier: Apache-2.0
//! Bounded graph context over one pinned code-index snapshot.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use petgraph::algo::dijkstra;
use petgraph::graph::{Graph, NodeIndex};
use petgraph::visit::Reversed;
use petgraph::Directed;

use crate::{
    docs, edges, index, knowledge, knowledge_binding, knowledge_search, knowledge_store, registry,
    response, scan, semantic, store,
};

pub const DEFAULT_DEPTH: usize = 3;
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;
pub const DEFAULT_MAX_BYTES: usize = 32_768;
pub const MIN_MAX_BYTES: usize = 8_192;
pub const MAX_MAX_BYTES: usize = 131_072;

pub struct ContextRequest<'a> {
    pub path: &'a str,
    pub line: u32,
    pub col: u32,
    pub symbol: Option<&'a str>,
    pub query: Option<&'a str>,
    pub semantic: bool,
    /// The anchor came from a name selector, not a position. Only then may a
    /// binding on the proven SCIP name occurrence be listed beside the fallback.
    pub by_name: bool,
    pub depth: usize,
    pub limit: usize,
    pub max_bytes: usize,
}

#[derive(Debug)]
pub(crate) enum PinnedContextError {
    Path(String),
    Store(String),
}

impl PinnedContextError {
    fn into_message(self) -> String {
        match self {
            Self::Path(message) | Self::Store(message) => message,
        }
    }
}

pub struct ContextNode {
    pub endpoint: store::EdgeEndpoint,
    pub distance: Option<usize>,
}

pub struct ContextResult {
    pub graph: Vec<ContextNode>,
    pub trace: index::TraceQueryOut,
    pub knowledge: ContextKnowledge,
    pub facts: index::FactSections,
    pub limit: usize,
    pub omitted_nodes: usize,
    pub max_bytes: usize,
}

pub struct ContextKnowledge {
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub generation: Option<u64>,
    pub schema_version: Option<u32>,
    pub bindings: knowledge_binding::AtPage,
    pub query: ContextRetrieval,
    pub semantic: ContextRetrieval,
}

pub struct ContextRetrieval {
    pub requested: bool,
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub generation: Option<u64>,
    pub hits: Vec<ContextHit>,
    pub total: Option<usize>,
    pub total_exact: bool,
    pub truncated: Option<bool>,
    pub candidates_returned: Option<usize>,
}

pub struct ContextHit {
    pub record: knowledge::RecordVersion,
    pub embedding_truncated: Option<bool>,
}

struct ContextEdge {
    caller: Option<store::EdgeEndpoint>,
    callee: Option<store::EdgeEndpoint>,
    proven: bool,
}

pub fn validate_request(
    root: &Path,
    request: &ContextRequest<'_>,
) -> Result<(PathBuf, String), String> {
    validate_options(
        request.depth,
        request.limit,
        request.max_bytes,
        request.query,
        request.semantic,
    )?;
    let trace = trace_request(request);
    let (canon, rel, _) = index::validated_trace_anchor(root, &trace)?;
    Ok((canon, rel))
}

/// The bounds, query and semantic checks, which need no anchor. A by-name
/// lookup runs them before it can answer, so a name gets the position's refusals.
pub fn validate_options(
    depth: usize,
    limit: usize,
    max_bytes: usize,
    query: Option<&str>,
    semantic: bool,
) -> Result<(), String> {
    if !(1..=edges::TRACE_MAX_DEPTH).contains(&depth)
        || !(1..=MAX_LIMIT).contains(&limit)
        || !(MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(&max_bytes)
    {
        return Err("invalid context bounds".to_string());
    }
    if let Some(query) = query {
        if query.trim().is_empty() {
            return Err("context query must not be empty".to_string());
        }
        if query.len() > crate::knowledge::MAX_QUERY_TEXT_BYTES {
            return Err(format!(
                "context query is {} bytes, at most {} allowed",
                query.len(),
                crate::knowledge::MAX_QUERY_TEXT_BYTES
            ));
        }
    }
    if semantic && query.is_none() {
        return Err("context semantic retrieval requires --query".to_string());
    }
    Ok(())
}

pub fn query_conn(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &ContextRequest<'_>,
) -> Result<ContextResult, String> {
    query_conn_inner(conn, home, entry, repo, root, request, || {}, || {})
}

/// Batch callers keep this transaction alive through the code-anchor lookup
/// used for knowledge bindings; the knowledge database remains independent.
#[cfg(test)]
pub(crate) fn query_pinned(
    tx: &rusqlite::Transaction<'_>,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &ContextRequest<'_>,
) -> Result<ContextResult, PinnedContextError> {
    query_pinned_with_hook(tx, home, entry, repo, root, request, |_| {})
}

pub(crate) fn query_pinned_with_hook(
    tx: &rusqlite::Transaction<'_>,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &ContextRequest<'_>,
    between_resolutions: impl FnOnce(&rusqlite::Transaction<'_>),
) -> Result<ContextResult, PinnedContextError> {
    let (canon, rel) = validate_request(root, request).map_err(PinnedContextError::Path)?;
    let (trace, facts, name_occurrence) =
        read_code_tx(tx, repo, &canon, &rel, request, || {}, between_resolutions)?;
    let mut graph = context_nodes(&trace);
    let omitted_nodes = graph.len().saturating_sub(request.limit);
    graph.truncate(request.limit);
    let knowledge = knowledge_context(
        tx,
        home,
        entry,
        root,
        &trace,
        name_occurrence.as_ref(),
        request,
        || {},
    );
    Ok(ContextResult {
        graph,
        trace,
        knowledge,
        facts,
        limit: request.limit,
        omitted_nodes,
        max_bytes: request.max_bytes,
    })
}

fn read_code_tx(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    canon: &Path,
    rel: &str,
    request: &ContextRequest<'_>,
    pinned: impl FnOnce(),
    between_resolutions: impl FnOnce(&rusqlite::Transaction<'_>),
) -> Result<
    (
        index::TraceQueryOut,
        index::FactSections,
        Option<index::EdgeAnchor>,
    ),
    PinnedContextError,
> {
    let trace = index::graph_query_tx_classified(
        tx,
        repo,
        canon,
        rel,
        None,
        &trace_request(request),
        Instant::now(),
        pinned,
        true,
        false,
    )
    .map_err(|error| match error {
        index::GraphQueryError::Anchor(message) => PinnedContextError::Path(message),
        index::GraphQueryError::Store(message) => PinnedContextError::Store(message),
    })?;
    between_resolutions(tx);
    let facts = fact_sections(tx, repo, canon, rel, request, &trace)?;
    let name_occurrence = if request.by_name {
        proven_name_occurrence(tx, canon, rel, &trace)?
    } else {
        None
    };
    Ok((trace, facts, name_occurrence))
}

/// For a name selector whose anchor is the syntax fallback over a definition,
/// the definition's own SCIP name occurrence, when the same ownership proof that
/// `docs scan` and `docs ingest` use establishes it. Never a name match: the
/// occurrence is returned only when `prove_anchor` proves a semantic anchor on a
/// span other than the definition. An unproven definition or a refused anchor
/// gives `None`; a storage failure is an error. `canon` is the canonical root.
fn proven_name_occurrence(
    tx: &rusqlite::Transaction<'_>,
    canon: &Path,
    rel: &str,
    trace: &index::TraceQueryOut,
) -> Result<Option<index::EdgeAnchor>, PinnedContextError> {
    if trace.anchor.kind != index::AnchorKind::SyntaxFallback {
        return Ok(None);
    }
    let Some((start, end)) = trace.anchor.span_bytes else {
        return Ok(None);
    };
    let snapshot_id = trace.snapshot.id;
    let rows: Vec<store::SymbolRow> = store::symbols_at(tx, snapshot_id, rel)
        .map_err(PinnedContextError::Store)?
        .into_iter()
        .filter(|row| row.byte_start == start && row.byte_end == end)
        .collect();
    let [definition] = rows.as_slice() else {
        return Ok(None);
    };
    let Ok(bytes) = semantic::confined_read(canon, rel, scan::MAX_FILE_BYTES) else {
        return Ok(None);
    };
    // The proof reads these bytes, so they must be the bytes the snapshot indexed.
    let indexed = store::file_identity(tx, snapshot_id, rel).map_err(PinnedContextError::Store)?;
    if indexed.is_none_or(|(_, digest)| digest != store::content_digest(&bytes)) {
        return Ok(None);
    }
    match docs::prove_anchor(tx, snapshot_id, canon, rel, definition, &bytes) {
        Ok(docs::Proof::Proven(edge))
            if edge.kind == index::AnchorKind::Semantic
                && edge.span_bytes.is_some_and(|span| span != (start, end)) =>
        {
            Ok(Some(edge))
        }
        Ok(_) | Err(index::GraphQueryError::Anchor(_)) => Ok(None),
        Err(index::GraphQueryError::Store(message)) => Err(PinnedContextError::Store(message)),
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn query_conn_with_hooks(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &ContextRequest<'_>,
    pinned: impl FnOnce(),
    before_bindings: impl FnOnce(),
) -> Result<ContextResult, String> {
    query_conn_inner(
        conn,
        home,
        entry,
        repo,
        root,
        request,
        pinned,
        before_bindings,
    )
}

#[allow(clippy::too_many_arguments)]
fn query_conn_inner(
    conn: &mut rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    repo: &str,
    root: &Path,
    request: &ContextRequest<'_>,
    pinned: impl FnOnce(),
    before_bindings: impl FnOnce(),
) -> Result<ContextResult, String> {
    let (canon, rel) = validate_request(root, request)?;
    // One read transaction: the graph and every fact section see the same
    // pinned snapshot. The knowledge store is a separate database, read after.
    let (trace, facts, name_occurrence) = {
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        read_code_tx(&tx, repo, &canon, &rel, request, pinned, |_| {})
            .map_err(PinnedContextError::into_message)?
    };
    let mut graph = context_nodes(&trace);
    let omitted_nodes = graph.len().saturating_sub(request.limit);
    graph.truncate(request.limit);
    let knowledge = knowledge_context(
        conn,
        home,
        entry,
        root,
        &trace,
        name_occurrence.as_ref(),
        request,
        before_bindings,
    );
    Ok(ContextResult {
        graph,
        trace,
        knowledge,
        facts,
        limit: request.limit,
        omitted_nodes,
        max_bytes: request.max_bytes,
    })
}

/// The four fact sections, read on the caller's transaction. Only identity
/// problems make a fact section unavailable; every other failure is an error.
fn fact_sections(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    canon: &Path,
    rel: &str,
    request: &ContextRequest<'_>,
    trace: &index::TraceQueryOut,
) -> Result<index::FactSections, PinnedContextError> {
    let snapshot_id = trace.snapshot.id;
    let identity = index::fact_identity(tx, snapshot_id, canon, &trace.anchor)
        .map_err(PinnedContextError::Store)?;
    let imports = index::import_facts(tx, snapshot_id, rel, request.limit)
        .map_err(PinnedContextError::Store)?;
    let callees = index::edge_query_tx_classified(
        tx,
        repo,
        canon,
        rel,
        &index::EdgeRequest {
            anchor: index::SpanQuery {
                rel,
                line1: request.line,
                col1: request.col,
                refs: false,
            },
            symbol: request.symbol,
            direction: index::EdgeDir::Callees,
            limit: request.limit,
            offset: 0,
        },
        || {},
        Some((&trace.freshness, &trace.freshness_reasons)),
    )
    .map_err(|error| match error {
        index::GraphQueryError::Anchor(message) => PinnedContextError::Path(message),
        index::GraphQueryError::Store(message) => PinnedContextError::Store(message),
    })?;
    let (containment, heritage) = match &identity {
        index::FactIdentity::Span {
            file_id,
            byte_start,
            byte_end,
            name,
            is_type,
        } => {
            let span = (*byte_start, *byte_end);
            let containment = index::containment_facts(
                tx,
                snapshot_id,
                canon,
                rel,
                *file_id,
                span,
                request.limit,
            )
            .map_err(PinnedContextError::Store)?;
            let heritage = index::heritage_facts(
                tx,
                snapshot_id,
                *file_id,
                span,
                name.as_deref(),
                *is_type,
                request.limit,
            )
            .map_err(PinnedContextError::Store)?;
            (Some(containment), Some(heritage))
        }
        index::FactIdentity::Unavailable { .. } => (None, None),
    };
    Ok(index::FactSections {
        identity,
        containment,
        heritage,
        imports,
        callees,
    })
}

fn retrieval(requested: bool, status: &'static str) -> ContextRetrieval {
    ContextRetrieval {
        requested,
        status,
        reason: None,
        generation: None,
        hits: Vec::new(),
        total: None,
        total_exact: false,
        truncated: None,
        candidates_returned: None,
    }
}

fn unavailable_page(
    path: &str,
    line: u32,
    col: u32,
    generation: u64,
    reason: &'static str,
) -> knowledge_binding::AtPage {
    let mut page = knowledge_binding::empty_at(path, line, col, generation, 0);
    page.status = "unavailable";
    page.unresolved_reason = Some(reason.to_string());
    page
}

fn map_lexical(outcome: knowledge_search::SearchOutcome) -> ContextRetrieval {
    match outcome {
        knowledge_search::SearchOutcome::Lexical(page) => ContextRetrieval {
            requested: true,
            status: "lexical",
            reason: None,
            generation: Some(page.generation),
            hits: page
                .hits
                .into_iter()
                .map(|record| ContextHit {
                    record,
                    embedding_truncated: None,
                })
                .collect(),
            total: Some(page.total),
            total_exact: true,
            truncated: Some(page.truncated),
            candidates_returned: None,
        },
        knowledge_search::SearchOutcome::Fallback { page, reason } => ContextRetrieval {
            requested: true,
            status: "fallback",
            reason: Some(reason),
            generation: Some(page.generation),
            hits: page
                .hits
                .into_iter()
                .map(|record| ContextHit {
                    record,
                    embedding_truncated: None,
                })
                .collect(),
            total: Some(page.total),
            total_exact: true,
            truncated: Some(page.truncated),
            candidates_returned: None,
        },
        #[cfg(feature = "semantic-knowledge")]
        knowledge_search::SearchOutcome::Semantic { .. } => {
            unreachable!("lexical query returned semantic results")
        }
    }
}

fn map_semantic(outcome: knowledge_search::SearchOutcome) -> ContextRetrieval {
    match outcome {
        #[cfg(feature = "semantic-knowledge")]
        knowledge_search::SearchOutcome::Semantic {
            hits,
            generation,
            candidates_returned,
        } => ContextRetrieval {
            requested: true,
            status: "semantic",
            reason: None,
            generation: Some(generation),
            hits: hits
                .into_iter()
                .map(|hit| ContextHit {
                    record: hit.record,
                    embedding_truncated: Some(hit.embedding_truncated),
                })
                .collect(),
            total: None,
            total_exact: false,
            truncated: None,
            candidates_returned: Some(candidates_returned),
        },
        knowledge_search::SearchOutcome::Fallback { page, reason } => ContextRetrieval {
            requested: true,
            status: "fallback",
            reason: Some(reason),
            generation: Some(page.generation),
            hits: Vec::new(),
            total: None,
            total_exact: false,
            truncated: None,
            candidates_returned: None,
        },
        knowledge_search::SearchOutcome::Lexical(page) => ContextRetrieval {
            requested: true,
            status: "fallback",
            reason: Some("semantic-fallback"),
            generation: Some(page.generation),
            hits: Vec::new(),
            total: None,
            total_exact: false,
            truncated: None,
            candidates_returned: None,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn knowledge_context(
    code_conn: &rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    root: &Path,
    trace: &index::TraceQueryOut,
    name_occurrence: Option<&index::EdgeAnchor>,
    request: &ContextRequest<'_>,
    before_bindings: impl FnOnce(),
) -> ContextKnowledge {
    let mut query = retrieval(
        request.query.is_some(),
        if request.query.is_some() {
            "unavailable"
        } else {
            "not_requested"
        },
    );
    let mut semantic = retrieval(
        request.semantic,
        if request.semantic {
            "unavailable"
        } else {
            "not_requested"
        },
    );
    let db = knowledge_store::knowledge_db_path(home, &entry.name);
    match crate::knowledge_store_absent(&db) {
        Ok(true) => {
            query.status = if query.requested {
                "unavailable"
            } else {
                "not_requested"
            };
            query.reason = query.requested.then_some("knowledge-store-missing");
            semantic.status = if semantic.requested {
                "unavailable"
            } else {
                "not_requested"
            };
            semantic.reason = semantic.requested.then_some("knowledge-store-missing");
            return ContextKnowledge {
                status: "unavailable",
                reason: Some("knowledge-store-missing"),
                generation: None,
                schema_version: None,
                bindings: unavailable_page(
                    request.path,
                    request.line,
                    request.col,
                    0,
                    "knowledge-store-missing",
                ),
                query,
                semantic,
            };
        }
        Err(_) => {
            query.status = if query.requested {
                "unavailable"
            } else {
                "not_requested"
            };
            query.reason = query.requested.then_some("knowledge-store-unavailable");
            semantic.status = if semantic.requested {
                "unavailable"
            } else {
                "not_requested"
            };
            semantic.reason = semantic.requested.then_some("knowledge-store-unavailable");
            return ContextKnowledge {
                status: "unavailable",
                reason: Some("knowledge-store-unavailable"),
                generation: None,
                schema_version: None,
                bindings: unavailable_page(
                    request.path,
                    request.line,
                    request.col,
                    0,
                    "knowledge-store-unavailable",
                ),
                query,
                semantic,
            };
        }
        Ok(false) => {}
    }
    let mut knowledge_conn = match knowledge_store::open_knowledge_at(home, entry, false) {
        Ok(conn) => conn,
        Err(_) => {
            query.status = if query.requested {
                "unavailable"
            } else {
                "not_requested"
            };
            query.reason = query.requested.then_some("knowledge-store-unavailable");
            semantic.status = if semantic.requested {
                "unavailable"
            } else {
                "not_requested"
            };
            semantic.reason = semantic.requested.then_some("knowledge-store-unavailable");
            return ContextKnowledge {
                status: "unavailable",
                reason: Some("knowledge-store-unavailable"),
                generation: None,
                schema_version: None,
                bindings: unavailable_page(
                    request.path,
                    request.line,
                    request.col,
                    0,
                    "knowledge-store-unavailable",
                ),
                query,
                semantic,
            };
        }
    };
    let anchor = match knowledge_binding::anchor_from_trace(code_conn, entry, root, trace) {
        Ok(anchor) => anchor,
        Err(_) => {
            query.reason = query.requested.then_some("code-anchor-unavailable");
            semantic.reason = semantic.requested.then_some("code-anchor-unavailable");
            return ContextKnowledge {
                status: "unavailable",
                reason: Some("code-anchor-unavailable"),
                generation: None,
                schema_version: None,
                bindings: unavailable_page(
                    request.path,
                    request.line,
                    request.col,
                    0,
                    "code-anchor-unavailable",
                ),
                query,
                semantic,
            };
        }
    };
    let alternate = name_occurrence.and_then(|edge| {
        let (byte_start, byte_end) = edge.span_bytes?;
        Some(knowledge_binding::CodeAnchor {
            byte_start,
            byte_end,
            cursor_line: edge.line1,
            cursor_col: edge.col1,
            symbol_uri: edge.symbol_uri.clone(),
            display_name: edge.display.clone(),
            anchor_kind: edge.kind.as_str().to_string(),
            precision: edge.precision.clone(),
            ..anchor.clone()
        })
    });
    let model_dir = std::env::var_os("RILTRA_MODEL_DIR").map(std::path::PathBuf::from);
    let mut expected_generation = None;
    let mut status = "available";
    let mut reason = None;
    if let Some(text) = request.query {
        match knowledge_search::search(
            &mut knowledge_conn,
            home,
            entry,
            text,
            None,
            None,
            false,
            false,
            false,
            request.limit,
            0,
            None,
            false,
            None,
        ) {
            Ok(outcome) => {
                query = map_lexical(outcome);
                expected_generation = query.generation;
            }
            Err(_) => {
                status = "unavailable";
                reason = Some("knowledge-query-unavailable");
                query.status = "unavailable";
                query.reason = reason;
            }
        }
        if request.semantic && expected_generation.is_some() {
            match knowledge_search::search(
                &mut knowledge_conn,
                home,
                entry,
                text,
                None,
                None,
                false,
                false,
                false,
                request.limit,
                0,
                expected_generation,
                true,
                model_dir.as_deref(),
            ) {
                Ok(outcome) => semantic = map_semantic(outcome),
                Err(_) => {
                    semantic.status = "unavailable";
                    semantic.reason = Some("knowledge-changed-or-unavailable");
                }
            }
        } else if request.semantic {
            semantic.status = "unavailable";
            semantic.reason = Some("knowledge-query-unavailable");
        }
    }
    before_bindings();
    let (bindings, binding_generation, binding_schema_version) = match knowledge_binding::at_pinned(
        &knowledge_conn,
        entry,
        &anchor,
        alternate.as_ref(),
        request.limit,
        expected_generation,
    ) {
        Ok(page) => {
            let generation = Some(page.generation);
            let schema_version = Some(page.schema_version);
            (page, generation, schema_version)
        }
        Err(_) => {
            status = "unavailable";
            reason = Some("knowledge-changed-or-unavailable");
            (
                unavailable_page(
                    &anchor.path,
                    anchor.cursor_line,
                    anchor.cursor_col,
                    expected_generation.unwrap_or(0),
                    "knowledge-changed-or-unavailable",
                ),
                None,
                None,
            )
        }
    };
    ContextKnowledge {
        status,
        reason,
        generation: binding_generation,
        schema_version: binding_schema_version,
        bindings,
        query,
        semantic,
    }
}

fn trace_request<'a>(request: &ContextRequest<'a>) -> index::TraceRequest<'a> {
    index::TraceRequest {
        source: index::SpanQuery {
            rel: request.path,
            line1: request.line,
            col1: request.col,
            refs: false,
        },
        symbol: request.symbol,
        destination: None,
        summary: false,
        include_tests: false,
        depth: request.depth,
        // Keep the original impact coverage independent from the output node
        // limit; the existing traversal still enforces its 1000-path budget.
        limit: edges::TRACE_MAX_PATHS,
        offset: 0,
    }
}

fn endpoint_json(endpoint: &store::EdgeEndpoint) -> String {
    response::edge_endpoint_json(&Some(endpoint.clone()))
}

fn context_nodes(trace: &index::TraceQueryOut) -> Vec<ContextNode> {
    let edges = trace
        .paths
        .iter()
        .flat_map(|path| &path.edges)
        .map(|hit| ContextEdge {
            caller: hit.edge.caller.clone(),
            callee: hit.edge.callee.clone(),
            proven: matches!(hit.edge.edge_kind.as_str(), "CALLS" | "REFERENCES")
                && hit.edge.precision == "Exact"
                && hit.edge.unresolved.is_none(),
        });
    graph_nodes(index::anchor_endpoint(&trace.anchor), &trace.bridges, edges)
}

fn graph_nodes(
    anchor: Option<store::EdgeEndpoint>,
    bridges: &[edges::OwnerBridge],
    edges: impl IntoIterator<Item = ContextEdge>,
) -> Vec<ContextNode> {
    let mut bridge_map = BTreeMap::new();
    let mut ambiguous_bridges = std::collections::BTreeSet::new();
    for bridge in bridges {
        let key = endpoint_json(&bridge.syntax);
        if bridge_map
            .get(&key)
            .is_some_and(|semantic| semantic != &bridge.semantic)
        {
            bridge_map.remove(&key);
            ambiguous_bridges.insert(key);
        } else if !ambiguous_bridges.contains(&key) {
            bridge_map.insert(key, bridge.semantic.clone());
        }
    }
    let canonical = |endpoint: &store::EdgeEndpoint| {
        bridge_map
            .get(&endpoint_json(endpoint))
            .cloned()
            .unwrap_or_else(|| endpoint.clone())
    };
    let mut endpoints = BTreeMap::<String, store::EdgeEndpoint>::new();
    let mut proven_edges = Vec::new();
    for edge in edges {
        let caller = edge.caller.as_ref().map(canonical);
        let callee = edge.callee.as_ref().map(canonical);
        if let Some(endpoint) = &caller {
            endpoints.insert(endpoint_json(endpoint), endpoint.clone());
        }
        if let Some(endpoint) = &callee {
            endpoints.insert(endpoint_json(endpoint), endpoint.clone());
        }
        if edge.proven {
            if let (Some(caller), Some(callee)) = (caller, callee) {
                proven_edges.push((endpoint_json(&caller), endpoint_json(&callee)));
            }
        }
    }
    let anchor = anchor.map(|endpoint| canonical(&endpoint));
    let anchor_key = anchor.as_ref().map(endpoint_json);
    if let (Some(endpoint), Some(key)) = (anchor, anchor_key.as_ref()) {
        endpoints.insert(key.clone(), endpoint);
    }

    let mut graph = Graph::<(), (), Directed>::new();
    let mut indices = BTreeMap::<String, NodeIndex>::new();
    for key in endpoints.keys() {
        indices.insert(key.clone(), graph.add_node(()));
    }
    for (caller, callee) in proven_edges {
        if let (Some(from), Some(to)) = (indices.get(&caller), indices.get(&callee)) {
            graph.add_edge(*from, *to, ());
        }
    }
    let distances = anchor_key
        .and_then(|key| indices.get(&key).copied())
        .map(|anchor| dijkstra(&Reversed(&graph), anchor, None, |_| 1usize))
        .unwrap_or_default();

    let mut nodes: Vec<_> = endpoints
        .into_iter()
        .map(|(key, endpoint)| ContextNode {
            distance: indices
                .get(&key)
                .and_then(|index| distances.get(index).copied()),
            endpoint,
        })
        .collect();
    nodes.sort_by(|a, b| {
        a.distance
            .unwrap_or(usize::MAX)
            .cmp(&b.distance.unwrap_or(usize::MAX))
            .then_with(|| endpoint_json(&a.endpoint).cmp(&endpoint_json(&b.endpoint)))
    });
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        index, knowledge::NewRecord, knowledge_binding::BindingInput, knowledge_store, store,
        testutil::tmpdir,
    };

    struct Fixture {
        home: PathBuf,
        root: PathBuf,
        entry: registry::RepoEntry,
        knowledge_generation: u64,
        code_nonce: String,
    }

    fn fixture(tag: &str) -> Fixture {
        let home = tmpdir(&format!("context-{tag}-home"));
        let root = tmpdir(&format!("context-{tag}-repo"))
            .canonicalize()
            .unwrap();
        std::fs::write(root.join("a.py"), b"def anchor():\n    old_call()\n").unwrap();
        index::build_full(&home, "r", &root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };

        let mut conn = knowledge_store::open_knowledge_at(&home, &entry, true).unwrap();
        knowledge_store::create_record(
            &mut conn,
            &NewRecord {
                id: "bound".to_string(),
                kind: "decision".to_string(),
                title: "Local boundary".to_string(),
                body: "local database".to_string(),
                status: "active".to_string(),
                author: "test".to_string(),
            },
        )
        .unwrap();
        drop(conn);
        let (_, knowledge_generation) = knowledge_binding::create_for_test(
            &home,
            &entry,
            "a.py",
            1,
            5,
            &BindingInput {
                record_id: "bound".to_string(),
                record_revision: 1,
                relation: "constrains".to_string(),
                author: "test".to_string(),
                rationale: "Pinned context test binding".to_string(),
                provenance: "unit-test".to_string(),
            },
        )
        .unwrap();
        let conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let code_nonce = store::latest_snapshot(&conn).unwrap().unwrap().nonce;
        Fixture {
            home,
            root,
            entry,
            knowledge_generation,
            code_nonce,
        }
    }

    fn request() -> ContextRequest<'static> {
        ContextRequest {
            path: "a.py",
            line: 1,
            col: 5,
            symbol: None,
            query: Some("local database"),
            semantic: false,
            by_name: false,
            depth: 3,
            limit: 20,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    #[test]
    fn packet_pinned_context_keeps_code_identity_for_binding_lookup() {
        let fixture = fixture("packet-code-pin");
        let mut code = store::open_live(&store::db_path(&fixture.home, "r")).unwrap();
        let tx = code.transaction().unwrap();
        let snapshot = store::latest_snapshot(&tx).unwrap().unwrap();
        let indexed_digest = store::file_identity(&tx, snapshot.id, "a.py")
            .unwrap()
            .unwrap()
            .1;
        let mut req = request();
        req.query = None;
        req.semantic = false;
        let first =
            query_pinned(&tx, &fixture.home, &fixture.entry, "r", &fixture.root, &req).unwrap();
        std::fs::write(
            fixture.root.join("a.py"),
            b"def anchor():\n    changed_call()\n",
        )
        .unwrap();
        index::build_full(&fixture.home, "r", &fixture.root).unwrap();
        let pinned_anchor =
            knowledge_binding::anchor_from_trace(&tx, &fixture.entry, &fixture.root, &first.trace)
                .unwrap();
        assert_eq!(pinned_anchor.source_digest, indexed_digest);
        assert_eq!(pinned_anchor.snapshot_nonce, snapshot.nonce);
        // A stale syntax anchor is refused by the existing context contract.
        // Restore its bytes before testing another query on the old snapshot.
        std::fs::write(
            fixture.root.join("a.py"),
            b"def anchor():\n    old_call()\n",
        )
        .unwrap();
        let second =
            query_pinned(&tx, &fixture.home, &fixture.entry, "r", &fixture.root, &req).unwrap();
        for result in [first, second] {
            assert_eq!(result.trace.snapshot.nonce, snapshot.nonce);
            let anchor = knowledge_binding::anchor_from_trace(
                &tx,
                &fixture.entry,
                &fixture.root,
                &result.trace,
            )
            .unwrap();
            assert_eq!(anchor.source_digest, indexed_digest);
            assert_eq!(anchor.snapshot_nonce, snapshot.nonce);
            assert_eq!(result.knowledge.bindings.bindings.len(), 1);
            assert_eq!(
                result.knowledge.bindings.bindings[0].snapshot_nonce,
                snapshot.nonce
            );
            assert!(!result.knowledge.query.requested);
            assert!(!result.knowledge.semantic.requested);
            assert_eq!(result.knowledge.query.status, "not_requested");
        }
        let live = store::open_live(&store::db_path(&fixture.home, "r")).unwrap();
        let latest = store::latest_snapshot(&live).unwrap().unwrap();
        assert_ne!(latest.nonce, snapshot.nonce);
        assert_ne!(
            store::file_identity(&live, latest.id, "a.py")
                .unwrap()
                .unwrap()
                .1,
            indexed_digest
        );
    }

    #[test]
    fn context_reports_unavailable_binding_after_knowledge_changes_between_reads() {
        let fixture = fixture("knowledge-mutation");
        let expected_generation = fixture.knowledge_generation;
        let (start_write, write_started) = std::sync::mpsc::sync_channel(0);
        let (write_done, generation_written) = std::sync::mpsc::sync_channel(0);
        let writer_home = fixture.home.clone();
        let writer_entry = fixture.entry.clone();
        let writer = std::thread::spawn(move || {
            write_started.recv().unwrap();
            let mut conn =
                knowledge_store::open_knowledge_at(&writer_home, &writer_entry, true).unwrap();
            let (_, generation) = knowledge_store::create_record(
                &mut conn,
                &NewRecord {
                    id: "new-match".to_string(),
                    kind: "decision".to_string(),
                    title: "Local database update".to_string(),
                    body: "local database".to_string(),
                    status: "active".to_string(),
                    author: "writer".to_string(),
                },
            )
            .unwrap();
            write_done.send(generation).unwrap();
        });

        let mut code = store::open_live(&store::db_path(&fixture.home, "r")).unwrap();
        let result = query_conn_with_hooks(
            &mut code,
            &fixture.home,
            &fixture.entry,
            "r",
            &fixture.root,
            &request(),
            || {},
            || {
                start_write.send(()).unwrap();
                assert_eq!(generation_written.recv().unwrap(), expected_generation + 1);
            },
        )
        .unwrap();
        writer.join().unwrap();

        assert_eq!(result.trace.snapshot.nonce, fixture.code_nonce);
        assert_eq!(result.knowledge.query.status, "lexical");
        assert_eq!(result.knowledge.query.generation, Some(expected_generation));
        assert_eq!(result.knowledge.query.hits.len(), 1);
        assert_eq!(result.knowledge.query.hits[0].record.id, "bound");
        assert_eq!(result.knowledge.status, "unavailable");
        assert_eq!(
            result.knowledge.reason,
            Some("knowledge-changed-or-unavailable")
        );
        assert_eq!(result.knowledge.generation, None);
        assert_eq!(result.knowledge.bindings.status, "unavailable");
        assert_eq!(
            result.knowledge.bindings.unresolved_reason.as_deref(),
            Some("knowledge-changed-or-unavailable")
        );
        assert!(result.knowledge.bindings.bindings.is_empty());
    }

    #[test]
    fn context_keeps_pinned_code_binding_during_index_promotion() {
        let fixture = fixture("index-promotion");
        let mut code = store::open_live(&store::db_path(&fixture.home, "r")).unwrap();
        let result = query_conn_with_hooks(
            &mut code,
            &fixture.home,
            &fixture.entry,
            "r",
            &fixture.root,
            &request(),
            || {
                index::build_full(&fixture.home, "r", &fixture.root).unwrap();
            },
            || {},
        )
        .unwrap();

        let live = store::open_live(&store::db_path(&fixture.home, "r")).unwrap();
        let promoted_nonce = store::latest_snapshot(&live).unwrap().unwrap().nonce;
        assert_ne!(fixture.code_nonce, promoted_nonce);
        assert_eq!(result.trace.snapshot.nonce, fixture.code_nonce);
        assert_eq!(result.trace.freshness, "fresh");
        assert_eq!(result.knowledge.status, "available");
        assert_eq!(
            result.knowledge.generation,
            Some(fixture.knowledge_generation)
        );
        assert_eq!(
            result.knowledge.query.generation,
            Some(fixture.knowledge_generation)
        );
        assert_eq!(
            result.knowledge.bindings.generation,
            fixture.knowledge_generation
        );
        assert_eq!(result.knowledge.bindings.status, "linked");
        assert_eq!(result.knowledge.bindings.bindings.len(), 1);
        let binding = &result.knowledge.bindings.bindings[0];
        assert_eq!(binding.record_id, "bound");
        assert_eq!(binding.snapshot_nonce, fixture.code_nonce);
        assert_eq!(binding.freshness, "fresh");
    }

    #[test]
    fn distances_follow_caller_to_anchor_direction_and_ignore_candidate_shortcuts() {
        let endpoint = |path: &str| store::EdgeEndpoint::SyntaxDef {
            path: path.to_string(),
            byte_start: 0,
            byte_end: 1,
        };
        let anchor = endpoint("anchor.py");
        let middle = endpoint("middle.py");
        let caller = endpoint("caller.py");
        let candidate = endpoint("candidate.py");
        let nodes = graph_nodes(
            Some(anchor.clone()),
            &[],
            [
                ContextEdge {
                    caller: Some(caller.clone()),
                    callee: Some(middle.clone()),
                    proven: true,
                },
                ContextEdge {
                    caller: Some(middle.clone()),
                    callee: Some(anchor.clone()),
                    proven: true,
                },
                ContextEdge {
                    caller: Some(candidate.clone()),
                    callee: Some(anchor.clone()),
                    proven: false,
                },
            ],
        );
        let distance = |target: &store::EdgeEndpoint| {
            nodes
                .iter()
                .find(|node| node.endpoint == *target)
                .and_then(|node| node.distance)
        };
        assert_eq!(distance(&anchor), Some(0));
        assert_eq!(distance(&middle), Some(1));
        assert_eq!(distance(&caller), Some(2));
        assert_eq!(distance(&candidate), None);
    }

    #[test]
    fn diamond_cycle_duplicate_edges_bridges_and_scoped_identities_stay_distinct() {
        let syntax = |path: &str| store::EdgeEndpoint::SyntaxDef {
            path: path.to_string(),
            byte_start: 0,
            byte_end: 1,
        };
        let anchor = store::EdgeEndpoint::Scip {
            import_id: 7,
            symbol_uri: "same-name".to_string(),
            doc: None,
        };
        let other_scope = store::EdgeEndpoint::Scip {
            import_id: 8,
            symbol_uri: "same-name".to_string(),
            doc: None,
        };
        let anchor_syntax = syntax("anchor.py");
        let left = syntax("left.py");
        let right = syntax("right.py");
        let root = syntax("root.py");
        let nodes = graph_nodes(
            Some(anchor.clone()),
            &[edges::OwnerBridge {
                semantic: anchor.clone(),
                syntax: anchor_syntax.clone(),
                definition_span: (0, 1),
                digest: "d".to_string(),
            }],
            [
                (left.clone(), anchor.clone(), true),
                (right.clone(), anchor_syntax, true),
                (root.clone(), left.clone(), true),
                (root.clone(), right.clone(), true),
                (left.clone(), root.clone(), true),
                (root.clone(), left.clone(), true),
                (other_scope.clone(), anchor.clone(), false),
            ]
            .map(|(caller, callee, proven)| ContextEdge {
                caller: Some(caller),
                callee: Some(callee),
                proven,
            }),
        );
        let distance = |target: &store::EdgeEndpoint| {
            nodes
                .iter()
                .find(|node| node.endpoint == *target)
                .and_then(|node| node.distance)
        };
        assert_eq!(
            nodes.len(),
            5,
            "bridged syntax and semantic anchors deduplicate"
        );
        assert_eq!(distance(&anchor), Some(0));
        assert_eq!(distance(&left), Some(1));
        assert_eq!(distance(&right), Some(1));
        assert_eq!(distance(&root), Some(2));
        assert_eq!(distance(&other_scope), None);
    }

    #[test]
    fn fact_sections_keep_the_pinned_snapshot_during_promotion() {
        let home = tmpdir("context-facts-pin-home");
        let root = tmpdir("context-facts-pin-repo").canonicalize().unwrap();
        std::fs::write(
            root.join("a.py"),
            b"class Box :\n    def old_member(self):\n        return 1\n",
        )
        .unwrap();
        index::build_full(&home, "r", &root).unwrap();
        let entry = registry::RepoEntry {
            name: "r".to_string(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut code = store::open_live(&store::db_path(&home, "r")).unwrap();
        let pinned_nonce = store::latest_snapshot(&code).unwrap().unwrap().nonce;
        let request = ContextRequest {
            path: "a.py",
            line: 1,
            col: 7,
            symbol: None,
            query: None,
            semantic: false,
            by_name: false,
            depth: 3,
            limit: 20,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        // The promoted snapshot gains a subclass of Box in a new file. The
        // anchor's own bytes do not change, so the pinned anchor still resolves.
        let result = query_conn_with_hooks(
            &mut code,
            &home,
            &entry,
            "r",
            &root,
            &request,
            || {
                std::fs::write(root.join("b.py"), b"class Child(Box):\n    pass\n").unwrap();
                index::build_full(&home, "r", &root).unwrap();
            },
            || {},
        )
        .unwrap();

        let live = store::open_live(&store::db_path(&home, "r")).unwrap();
        assert_ne!(
            store::latest_snapshot(&live).unwrap().unwrap().nonce,
            pinned_nonce
        );
        assert_eq!(result.trace.snapshot.nonce, pinned_nonce);
        let heritage = result.facts.heritage.as_ref().unwrap();
        assert_eq!(heritage.subtypes.total, Some(0));
        assert!(heritage.subtypes.rows.is_empty());
        let members = &result.facts.containment.as_ref().unwrap().members;
        assert_eq!(members.total, Some(1));
        assert_eq!(members.rows[0].name.as_deref(), Some("old_member"));
        assert!(result.facts.imports.imported_by.rows.is_empty());
    }
}
