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
    edges, index, knowledge, knowledge_binding, knowledge_search, knowledge_store, registry,
    response, store,
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
    pub depth: usize,
    pub limit: usize,
    pub max_bytes: usize,
}

pub struct ContextNode {
    pub endpoint: store::EdgeEndpoint,
    pub distance: Option<usize>,
}

pub struct ContextResult {
    pub graph: Vec<ContextNode>,
    pub trace: index::TraceQueryOut,
    pub knowledge: ContextKnowledge,
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
    let trace_request = trace_request(request);
    let trace = index::graph_query_conn(
        conn,
        repo,
        &canon,
        &rel,
        None,
        &trace_request,
        Instant::now(),
        pinned,
        true,
        false,
    )?;
    let mut graph = context_nodes(&trace);
    let omitted_nodes = graph.len().saturating_sub(request.limit);
    graph.truncate(request.limit);
    let knowledge = knowledge_context(conn, home, entry, root, &trace, request, before_bindings);
    Ok(ContextResult {
        graph,
        trace,
        knowledge,
        limit: request.limit,
        omitted_nodes,
        max_bytes: request.max_bytes,
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

fn knowledge_context(
    code_conn: &rusqlite::Connection,
    home: &Path,
    entry: &registry::RepoEntry,
    root: &Path,
    trace: &index::TraceQueryOut,
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
            depth: 3,
            limit: 20,
            max_bytes: DEFAULT_MAX_BYTES,
        }
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
}
