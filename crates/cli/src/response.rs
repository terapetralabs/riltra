// SPDX-License-Identifier: Apache-2.0
//! Shared deterministic response assembly for the CLI and (M8 CP3+) the MCP
//! server. One code path builds each response shape; callers add the trailing
//! newline. This module is extraction-only: every byte matches the historical
//! inline formatting in `main.rs`. (The semantic query document is the one
//! exception: it carries its own trailing newline, exactly as the CLI's
//! inline version did.)

use std::path::Path;

use crate::{edges, git, ignore, index, json, knowledge, knowledge_store, semantic, store, syntax};

/// One snapshot identity block, shared by search and index-status output.
pub(crate) fn snapshot_json(snapshot: &store::Snapshot) -> String {
    format!(
        "{{\"id\":{},\"created_unix\":{},\"head\":{},\"branch\":{},\"dirty\":{},\"status_at_write\":\"{}\",\"nonce\":\"{}\"}}",
        snapshot.id,
        snapshot.created_unix,
        match &snapshot.head {
            Some(h) => format!("\"{}\"", json::escape(h)),
            None => "null".to_string(),
        },
        match &snapshot.branch {
            Some(b) => format!("\"{}\"", json::escape(b)),
            None => "null".to_string(),
        },
        snapshot.dirty,
        snapshot.status_at_write,
        snapshot.nonce
    )
}

// --- search ----------------------------------------------------------------

/// Parameters for one search response. `reasons` are quoted raw, exactly the
/// bytes the inline call sites always produced; the producers are fixed
/// literals (`no-index`, `root-unreadable`, freshness reasons).
pub(crate) struct SearchResponse<'a> {
    pub repo: &'a str,
    pub root: &'a str,
    pub snapshot: Option<&'a store::Snapshot>,
    pub freshness: &'a str,
    pub reasons: &'a [String],
    pub symbol: &'a str,
    pub limit: usize,
    pub offset: usize,
    pub total: usize,
    pub hits: &'a [store::Hit],
    /// The opt-in occurrence census (`--occurrences`); `None` leaves the
    /// document byte-identical to a search without the flag.
    pub occurrences: Option<&'a store::OccurrenceList>,
}

fn occurrence_item_json(hit: &store::OccurrenceHit) -> String {
    format!(
        "{{\"path\":\"{}\",\"role\":\"{}\",\"confidence\":\"{}\",\"span\":{{\"start_line\":{},\"start_col\":{},\"end_line\":{},\"end_col\":{}}},\"bytes\":[{},{}],\"provenance\":{{\"provider\":\"{}\",\"grammar\":\"{}\",\"language\":\"{}\"}}}}",
        json::escape(&hit.path),
        json::escape(&hit.role),
        syntax::occurrence_confidence(&hit.role),
        hit.start_line,
        hit.start_col,
        hit.end_line,
        hit.end_col,
        hit.byte_start,
        hit.byte_end,
        hit.provider,
        hit.grammar,
        hit.language
    )
}

/// The occurrence section. `scope` and `binding` are literals that say what
/// the list is: a repository-wide census of an exact name, bound to no
/// declaration. They are present even when the list is empty, so an empty
/// list is never read as proof of absence.
fn occurrences_json(list: &store::OccurrenceList, offset: usize) -> String {
    let items: Vec<String> = list.items.iter().map(occurrence_item_json).collect();
    format!(
        "{{\"scope\":\"repository-wide-exact-name\",\"binding\":\"unbound\",\"field_declarations\":{},\"symbol_declarations\":{},\"total\":{},\"truncated\":{},\"items\":[{}]}}",
        list.field_declarations,
        list.symbol_declarations,
        list.total,
        offset + items.len() < list.total,
        items.join(",")
    )
}

fn search_hit_json(hit: &store::Hit) -> String {
    format!(
        "{{\"path\":\"{}\",\"kind\":\"{}\",\"name\":{},\"span\":{{\"start_line\":{},\"start_col\":{},\"end_line\":{},\"end_col\":{}}},\"bytes\":[{},{}],\"provenance\":{{\"provider\":\"{}\",\"grammar\":\"{}\",\"language\":\"{}\"}},\"snapshot\":{}}}",
        json::escape(&hit.path),
        hit.kind,
        match &hit.name {
            Some(n) => format!("\"{}\"", json::escape(n)),
            None => "null".to_string(),
        },
        hit.start_line,
        hit.start_col,
        hit.end_line,
        hit.end_col,
        hit.byte_start,
        hit.byte_end,
        hit.provider,
        hit.grammar,
        hit.language,
        hit.snapshot_id
    )
}

pub(crate) fn search_response_json(response: &SearchResponse<'_>) -> String {
    let results: Vec<String> = response.hits.iter().map(search_hit_json).collect();
    let truncated = response.offset + results.len() < response.total;
    let occurrences = response
        .occurrences
        .map(|list| {
            format!(
                ",\"occurrences\":{}",
                occurrences_json(list, response.offset)
            )
        })
        .unwrap_or_default();
    format!(
        "{{\"repo\":\"{}\",\"root\":\"{}\",\"snapshot\":{},\"freshness\":\"{}\",\"reasons\":[{}],\"query\":{{\"symbol\":\"{}\",\"limit\":{},\"offset\":{}}},\"total\":{},\"results\":[{}],\"truncated\":{}{}}}",
        json::escape(response.repo),
        json::escape(response.root),
        match response.snapshot {
            Some(snapshot) => snapshot_json(snapshot),
            None => "null".to_string(),
        },
        response.freshness,
        response.reasons.iter().map(|r| format!("\"{r}\"")).collect::<Vec<_>>().join(","),
        json::escape(response.symbol),
        response.limit,
        response.offset,
        response.total,
        results.join(","),
        truncated,
        occurrences
    )
}

// --- repository status and index status ------------------------------------

/// One `repo status` document. Shared by the CLI and MCP.
pub(crate) fn repo_status_json(
    name: &str,
    root: &str,
    g: &git::GitInfo,
    ig: &ignore::IgnoreInfo,
) -> String {
    format!(
        "{{\"name\":\"{}\",\"root\":\"{}\",\"git\":{},\"ignore\":{}}}",
        json::escape(name),
        json::escape(root),
        git::git_json(g),
        ignore::ignore_json(ig)
    )
}

/// One `index status` document. The CLI fills `freshness` through its
/// migrating open; MCP fills it through the read-only path (`open_read` +
/// `evaluate_tx`). `reasons` are quoted raw, exactly the historical bytes.
pub(crate) fn index_status_json(
    repo: &str,
    root: &str,
    freshness: &index::Freshness,
    fts: bool,
) -> String {
    let reasons: Vec<String> = freshness
        .reasons
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect();
    format!(
        "{{\"repo\":\"{}\",\"root\":\"{}\",\"freshness\":\"{}\",\"reasons\":[{}],\"fts\":{},\"snapshot\":{}}}",
        json::escape(repo),
        json::escape(root),
        freshness.status,
        reasons.join(","),
        fts,
        match &freshness.snapshot {
            Some(snapshot) => snapshot_json(snapshot),
            None => "null".to_string(),
        }
    )
}

// --- graph edges and paths -------------------------------------------------

fn edge_string(value: &str) -> String {
    format!("\"{}\"", json::escape(value))
}
fn edge_optional(value: Option<&str>) -> String {
    value.map(edge_string).unwrap_or_else(|| "null".to_string())
}
fn edge_number<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|n| n.to_string())
        .unwrap_or_else(|| "null".to_string())
}
fn edge_span(value: Option<(usize, usize)>) -> String {
    value
        .map(|(a, b)| format!("[{a},{b}]"))
        .unwrap_or_else(|| "null".to_string())
}
fn edge_strings(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|s| edge_string(s))
            .collect::<Vec<_>>()
            .join(",")
    )
}

pub(crate) fn edge_endpoint_json(endpoint: &Option<store::EdgeEndpoint>) -> String {
    match endpoint {
        Some(store::EdgeEndpoint::Scip { import_id, symbol_uri, doc }) => format!("{{\"type\":\"scip\",\"import_id\":{import_id},\"symbol_uri\":{},\"doc\":{},\"path\":null,\"byte_start\":null,\"byte_end\":null}}", edge_string(symbol_uri), edge_optional(doc.as_deref())),
        Some(store::EdgeEndpoint::SyntaxDef { path, byte_start, byte_end }) => format!("{{\"type\":\"syntax_def\",\"import_id\":null,\"symbol_uri\":null,\"doc\":null,\"path\":{},\"byte_start\":{byte_start},\"byte_end\":{byte_end}}}", edge_string(path)),
        None => "null".to_string(),
    }
}

pub(crate) fn edge_hit_json(hit: &index::EdgeHitOut) -> String {
    let e = &hit.edge;
    let imports = hit.evidence.imports.iter().map(|(id, i)| format!("{{\"id\":{id},\"provider\":{},\"version\":{},\"artifact_id\":{},\"source_revision\":{},\"content_identity\":{}}}", edge_string(&i.provider_name), edge_string(&i.provider_version), edge_string(&i.artifact_id), edge_string(&i.source_revision), edge_string(&i.content_identity))).collect::<Vec<_>>().join(",");
    // Legacy convenience fields retain only a UNIQUE proven import, never an
    // arbitrary first overlap. Complete provenance is in evidence.imports.
    let unique = if hit.evidence.imports.len() == 1 {
        hit.evidence.imports.first()
    } else {
        None
    };
    format!("{{\"kind\":{},\"precision\":{},\"caller\":{},\"callee\":{},\"caller_path\":{},\"caller_span\":{},\"callee_name\":{},\"provider\":{},\"import_id\":{},\"tool\":{},\"verification\":{},\"unresolved\":{},\"evidence\":{{\"path\":{},\"span\":{},\"callee_span\":{},\"terminal_span\":{},\"reference_span\":{},\"digest\":{},\"provider\":{},\"imports\":[{imports}]}}}}",
        edge_string(&e.edge_kind), edge_string(&e.precision), edge_endpoint_json(&e.caller), edge_endpoint_json(&e.callee), edge_string(&e.caller_path), edge_span(Some(e.caller_span_bytes)), edge_string(&e.callee_name), edge_string(&e.provider), edge_number(unique.map(|(id,_)| id)), edge_optional(unique.map(|(_,i)| i.provider_name.as_str())), edge_string(&hit.verification), edge_optional(e.unresolved.as_deref()),
        edge_string(&e.caller_path), edge_span(Some(e.caller_span_bytes)), edge_span(hit.evidence.callee_span), edge_span(hit.evidence.terminal_span), edge_span(hit.evidence.reference_span), edge_string(&hit.evidence.digest), edge_string(&e.provider))
}

pub(crate) fn edge_query_json(o: &index::EdgeQueryOut) -> String {
    let a = &o.anchor;
    let edges = o
        .edges
        .iter()
        .map(edge_hit_json)
        .collect::<Vec<_>>()
        .join(",");
    let denominator = o
        .completeness
        .denominator
        .iter()
        .map(|(k, v)| format!("{}:{v}", edge_string(k)))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"schema_version\":1,\"repo\":{},\"snapshot\":{{\"id\":{},\"nonce\":{},\"revision\":{}}},\"direction\":{},\"anchor\":{{\"kind\":{},\"path\":{},\"line\":{},\"col\":{},\"symbol_uri\":{},\"import_id\":{},\"precision\":{},\"anchor_uncertainty\":{},\"span\":{},\"endpoint\":{}}},\"freshness\":{{\"status\":{},\"reasons\":{}}},\"verification\":{},\"completeness\":{{\"status\":{},\"covered\":{},\"total\":{},\"denominator\":{{{denominator}}},\"unresolved\":{}}},\"edges\":[{edges}],\"truncated\":{},\"limits\":{{\"limit\":{},\"offset\":{},\"next_offset\":{}}}}}",
        edge_string(&o.repo), o.snapshot.id, edge_string(&o.snapshot.nonce), edge_string(&o.revision), edge_string(o.direction.as_str()), edge_string(a.kind.as_str()), edge_string(&a.path), a.line1, a.col1, edge_optional(a.symbol_uri.as_deref()), edge_number(a.import_id), edge_string(&a.precision), edge_optional(a.uncertainty.as_deref()), edge_span(a.span_bytes), edge_endpoint_json(&index::anchor_endpoint(a)),
        edge_string(&o.freshness), edge_strings(&o.freshness_reasons), edge_string(&o.verification), edge_string(&o.completeness.status), o.completeness.covered, o.completeness.total, edge_strings(&o.completeness.unresolved), o.truncated, o.limit, o.offset, edge_number(o.next_offset))
}

pub(crate) fn trace_anchor_json(anchor: &index::EdgeAnchor) -> String {
    format!("{{\"kind\":{},\"path\":{},\"line\":{},\"col\":{},\"symbol_uri\":{},\"import_id\":{},\"precision\":{},\"anchor_uncertainty\":{},\"span\":{},\"endpoint\":{}}}",edge_string(anchor.kind.as_str()),edge_string(&anchor.path),anchor.line1,anchor.col1,edge_optional(anchor.symbol_uri.as_deref()),edge_number(anchor.import_id),edge_string(&anchor.precision),edge_optional(anchor.uncertainty.as_deref()),edge_span(anchor.span_bytes),edge_endpoint_json(&index::anchor_endpoint(anchor)))
}

pub(crate) fn trace_query_json(o: &index::TraceQueryOut) -> String {
    let direction = if o.impact_verdict.is_some() {
        "impact"
    } else {
        "trace"
    };
    let disclaimer = if o.impact_verdict.is_some() {
        "plausible-impact-not-execution"
    } else {
        "plausible-path-not-execution"
    };
    let verdict = o
        .impact_verdict
        .map(|v| format!("\"verdict\":{},", edge_string(v)))
        .unwrap_or_default();
    let paths = o.paths.iter().map(|p| {
        let edges=p.edges.iter().map(edge_hit_json).collect::<Vec<_>>().join(",");
        let distance = p.edges.len();
        let scope = if distance == 1 { "direct" } else { "transitive" };
        let impact = if o.impact_verdict.is_some() { format!("\"distance\":{distance},\"scope\":\"{scope}\",") } else { String::new() };
        format!("{{{impact}\"edges\":[{edges}],\"cycle\":{},\"truncated\":{},\"verification\":{},\"unresolved\":{}}}",p.cycle,p.truncated,edge_string(&p.verification),edge_strings(&p.unresolved))
    }).collect::<Vec<_>>().join(",");
    let bridges = o
        .bridges
        .iter()
        .map(|b| {
            format!(
                "{{\"semantic\":{},\"syntax\":{},\"definition_span\":{},\"digest\":{}}}",
                edge_endpoint_json(&Some(b.semantic.clone())),
                edge_endpoint_json(&Some(b.syntax.clone())),
                edge_span(Some(b.definition_span)),
                edge_string(&b.digest)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let denominator = o
        .completeness
        .denominator
        .iter()
        .map(|(k, v)| format!("{}:{v}", edge_string(k)))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{verdict}\"schema_version\":1,\"repo\":{},\"snapshot\":{{\"id\":{},\"nonce\":{},\"revision\":{}}},\"direction\":\"{direction}\",\"anchor\":{},\"destination\":{},\"freshness\":{{\"status\":{},\"reasons\":{}}},\"verification\":{},\"completeness\":{{\"status\":{},\"covered\":{},\"total\":{},\"total_exact\":{},\"denominator\":{{{denominator}}},\"unresolved\":{}}},\"paths\":[{paths}],\"bridges\":[{bridges}],\"visits\":{{\"nodes\":{},\"edges\":{}}},\"budgets\":{{\"depth\":{},\"max_depth\":6,\"nodes\":500,\"edges\":3000,\"paths\":1000,\"mappings\":500,\"time_ms\":2000}},\"limits\":{{\"limit\":{},\"offset\":{},\"next_offset\":{}}},\"cycle\":{},\"truncated\":{},\"unresolved\":{},\"disclaimer\":\"{disclaimer}\"}}",
        edge_string(&o.repo),o.snapshot.id,edge_string(&o.snapshot.nonce),edge_string(&o.revision),trace_anchor_json(&o.anchor),o.destination.as_ref().map(trace_anchor_json).unwrap_or_else(||"null".to_string()),edge_string(&o.freshness),edge_strings(&o.freshness_reasons),edge_string(&o.verification),edge_string(&o.completeness.status),o.completeness.covered,o.completeness.total,o.total_exact,edge_strings(&o.completeness.unresolved),o.nodes,o.edge_visits,o.depth,o.limit,o.offset,edge_number(o.next_offset),o.cycle,o.truncated,edge_strings(&o.unresolved))
}

pub(crate) fn context_query_json(result: &crate::context::ContextResult) -> Result<String, String> {
    let trace = &result.trace;
    let mut selected: std::collections::BTreeSet<_> = result
        .graph
        .iter()
        .map(|node| edge_endpoint_json(&Some(node.endpoint.clone())))
        .collect();
    let mut bridge_targets =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    for bridge in &trace.bridges {
        let syntax = edge_endpoint_json(&Some(bridge.syntax.clone()));
        let semantic = edge_endpoint_json(&Some(bridge.semantic.clone()));
        bridge_targets.entry(syntax).or_default().insert(semantic);
    }
    let bridge_targets: std::collections::BTreeMap<_, _> = bridge_targets
        .into_iter()
        .filter_map(|(syntax, semantics)| {
            (semantics.len() == 1).then(|| (syntax, semantics.into_iter().next().unwrap()))
        })
        .collect();
    for (syntax, semantic) in &bridge_targets {
        if selected.contains(semantic) {
            selected.insert(syntax.clone());
        }
    }
    let node_distances: std::collections::BTreeMap<_, _> = result
        .graph
        .iter()
        .map(|node| {
            (
                edge_endpoint_json(&Some(node.endpoint.clone())),
                node.distance,
            )
        })
        .collect();
    let canonical_identity = |endpoint: &store::EdgeEndpoint| {
        let identity = edge_endpoint_json(&Some(endpoint.clone()));
        bridge_targets.get(&identity).cloned().unwrap_or(identity)
    };
    let mut all_edges = std::collections::BTreeSet::new();
    let mut visible_edges =
        std::collections::BTreeMap::<String, (Option<usize>, String, serde_json::Value)>::new();
    for hit in trace.paths.iter().flat_map(|path| &path.edges) {
        let encoded = edge_hit_json(hit);
        let value: serde_json::Value = serde_json::from_str(&encoded)
            .map_err(|e| format!("context edge assembly failed: {e}"))?;
        all_edges.insert(encoded.clone());
        let touches_selected = hit
            .edge
            .caller
            .as_ref()
            .is_some_and(|endpoint| selected.contains(&canonical_identity(endpoint)))
            || hit
                .edge
                .callee
                .as_ref()
                .is_some_and(|endpoint| selected.contains(&canonical_identity(endpoint)));
        if touches_selected {
            let identity = hit
                .edge
                .caller
                .as_ref()
                .or(hit.edge.callee.as_ref())
                .map(canonical_identity)
                .unwrap_or_default();
            let distance = node_distances.get(&identity).copied().flatten();
            visible_edges
                .entry(encoded)
                .or_insert((distance, identity, value));
        }
    }
    let mut visible_edges: Vec<_> = visible_edges
        .into_iter()
        .map(|(encoded, (distance, identity, value))| {
            (distance.unwrap_or(usize::MAX), identity, encoded, value)
        })
        .collect();
    visible_edges.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    let mut edge_values: Vec<_> = visible_edges
        .into_iter()
        .take(result.limit)
        .map(|(_, _, _, value)| value)
        .collect();
    let mut omitted_edges = all_edges.len().saturating_sub(edge_values.len());
    let mut nodes: Vec<_> = result
        .graph
        .iter()
        .map(|node| {
            serde_json::json!({
                "identity": serde_json::from_str::<serde_json::Value>(
                    &edge_endpoint_json(&Some(node.endpoint.clone()))
                ).expect("endpoint serializer emits valid JSON"),
                "distance": node.distance,
            })
        })
        .collect();
    let bridges: Vec<_> = trace
        .bridges
        .iter()
        .map(|bridge| {
            serde_json::json!({
                "semantic": serde_json::from_str::<serde_json::Value>(
                    &edge_endpoint_json(&Some(bridge.semantic.clone()))
                ).expect("endpoint serializer emits valid JSON"),
                "syntax": serde_json::from_str::<serde_json::Value>(
                    &edge_endpoint_json(&Some(bridge.syntax.clone()))
                ).expect("endpoint serializer emits valid JSON"),
                "definition_span": [bridge.definition_span.0, bridge.definition_span.1],
                "digest": bridge.digest,
            })
        })
        .collect();
    let mut omitted_nodes = result.omitted_nodes;
    let context_limit_omitted_edges = omitted_edges;
    let context_limit_omitted_nodes = omitted_nodes;
    let mut byte_omitted_edges = 0usize;
    let mut byte_omitted_nodes = 0usize;
    let root = Path::new(&trace.snapshot.root);
    let mut query_hits = context_hits_json(&result.knowledge.query, root)?;
    let mut semantic_hits = context_hits_json(&result.knowledge.semantic, root)?;
    let mut bindings: serde_json::Value = serde_json::from_str(
        &crate::knowledge_binding::page_json(&result.knowledge.bindings),
    )
    .map_err(|e| format!("context binding assembly failed: {e}"))?;
    for binding in bindings["bindings"].as_array_mut().into_iter().flatten() {
        if let Some(record) = binding
            .get_mut("record")
            .and_then(serde_json::Value::as_object_mut)
        {
            if let Some(body) = record.remove("body") {
                record.insert("snippet".to_string(), body);
                record.insert(
                    "snippet_truncated".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
        }
    }
    let bindings_total_exact = result.knowledge.bindings.status != "unavailable";
    let omitted_bindings_by_limit = bindings_total_exact.then(|| {
        result
            .knowledge
            .bindings
            .total
            .saturating_sub(result.knowledge.bindings.bindings.len())
    });
    if let Some(fields) = bindings.as_object_mut() {
        if result.knowledge.schema_version.is_none() {
            fields.insert("schema_version".to_string(), serde_json::Value::Null);
        }
        fields.insert(
            "total".to_string(),
            if bindings_total_exact {
                serde_json::json!(result.knowledge.bindings.total)
            } else {
                serde_json::Value::Null
            },
        );
        fields.insert(
            "total_exact".to_string(),
            serde_json::Value::Bool(bindings_total_exact),
        );
        fields.insert(
            "truncated".to_string(),
            if bindings_total_exact {
                serde_json::json!(result.knowledge.bindings.truncated)
            } else {
                serde_json::Value::Null
            },
        );
        fields.insert(
            "omitted_by_limit".to_string(),
            serde_json::json!(omitted_bindings_by_limit),
        );
    }
    let mut omitted_query_hits = 0usize;
    let mut omitted_semantic_hits = 0usize;
    let mut omitted_bindings_by_budget = 0usize;
    loop {
        if let Some(fields) = bindings.as_object_mut() {
            fields.insert(
                "truncated".to_string(),
                if bindings_total_exact {
                    serde_json::json!(
                        result.knowledge.bindings.truncated || omitted_bindings_by_budget > 0
                    )
                } else {
                    serde_json::Value::Null
                },
            );
            fields.insert(
                "omitted_by_budget".to_string(),
                serde_json::json!(omitted_bindings_by_budget),
            );
        }
        let snippet_truncated = count_truncated_snippets(&bindings)
            + count_truncated_snippets_in(&query_hits)
            + count_truncated_snippets_in(&semantic_hits);
        let query_limited = result.knowledge.query.truncated == Some(true);
        let truncated = trace.truncated
            || omitted_edges > 0
            || omitted_nodes > 0
            || omitted_query_hits > 0
            || omitted_semantic_hits > 0
            || omitted_bindings_by_budget > 0
            || omitted_bindings_by_limit.is_some_and(|count| count > 0)
            || snippet_truncated > 0
            || query_limited;
        let mut graph_reasons = trace.unresolved.clone();
        if context_limit_omitted_edges > 0 || context_limit_omitted_nodes > 0 {
            graph_reasons.push("context-limit".to_string());
        }
        if byte_omitted_edges > 0 || byte_omitted_nodes > 0 {
            graph_reasons.push("output-byte-budget".to_string());
        }
        graph_reasons.sort();
        graph_reasons.dedup();
        let mut truncation_reasons = graph_reasons.clone();
        if omitted_query_hits > 0 || omitted_semantic_hits > 0 {
            truncation_reasons.push("knowledge-candidates".to_string());
        }
        if omitted_bindings_by_limit.is_some_and(|count| count > 0) {
            truncation_reasons.push("binding-limit".to_string());
        }
        if omitted_bindings_by_budget > 0 {
            truncation_reasons.push("binding-budget".to_string());
        }
        if snippet_truncated > 0 {
            truncation_reasons.push("knowledge-snippets".to_string());
        }
        if query_limited {
            truncation_reasons.push("query-limit".to_string());
        }
        truncation_reasons.sort();
        truncation_reasons.dedup();
        let value = serde_json::json!({
            "schema_version": 1,
            "operation": "context",
            "repo": &trace.repo,
            "snapshot": {
                "id": trace.snapshot.id,
                "nonce": &trace.snapshot.nonce,
                "revision": &trace.revision,
            },
            "freshness": {"status": &trace.freshness, "reasons": &trace.freshness_reasons},
            "verification": &trace.verification,
            "anchor": serde_json::from_str::<serde_json::Value>(&trace_anchor_json(&trace.anchor))
                .expect("anchor serializer emits valid JSON"),
            "distance_scope": "inspected_subgraph",
            "impact": {
                "verdict": trace.impact_verdict,
                "completeness": {
                    "status": &trace.completeness.status,
                    "covered": trace.completeness.covered,
                    "total": trace.completeness.total,
                    "total_exact": trace.total_exact,
                    "denominator": &trace.completeness.denominator,
                    "unresolved": &trace.completeness.unresolved,
                }
            },
            "graph": {
                "nodes": &nodes,
                "edges": &edge_values,
                "bridges": &bridges,
                "visits": {"nodes": trace.nodes, "edges": trace.edge_visits},
                "truncated": trace.truncated || omitted_edges > 0 || omitted_nodes > 0,
                "unresolved": &graph_reasons,
            },
            "knowledge": {
                "status": result.knowledge.status,
                "reason": result.knowledge.reason,
                "snapshot": {
                    "generation": result.knowledge.generation,
                    "schema_version": result.knowledge.schema_version,
                },
                "freshness": {
                    "status": result.knowledge.status,
                    "bindings": result.knowledge.bindings.status,
                },
                "bindings": &bindings,
            },
            "query": {
                "requested": result.knowledge.query.requested,
                "status": result.knowledge.query.status,
                "reason": result.knowledge.query.reason,
                "generation": result.knowledge.query.generation,
                "hits": &query_hits,
                "total": result.knowledge.query.total,
                "total_exact": result.knowledge.query.total_exact,
                "truncated": result.knowledge.query.truncated,
                "omitted_by_budget": omitted_query_hits,
            },
            "semantic": {
                "requested": result.knowledge.semantic.requested,
                "status": result.knowledge.semantic.status,
                "reason": result.knowledge.semantic.reason,
                "generation": result.knowledge.semantic.generation,
                "hits": &semantic_hits,
                "total": result.knowledge.semantic.total,
                "total_exact": result.knowledge.semantic.total_exact,
                "truncated": result.knowledge.semantic.truncated,
                "candidate_limit": 1000,
                "candidates_returned": result.knowledge.semantic.candidates_returned,
                "omitted_by_budget": omitted_semantic_hits,
            },
            "budgets": {
                "depth": trace.depth,
                "max_depth": edges::TRACE_MAX_DEPTH,
                "nodes": edges::TRACE_MAX_NODES,
                "edges": edges::TRACE_MAX_EDGES,
                "paths": edges::TRACE_MAX_PATHS,
                "mappings": edges::TRACE_MAX_NODES,
                "time_ms": edges::TRACE_TIME_MS,
            },
            "limits": {"limit": result.limit, "max_bytes": result.max_bytes},
            "truncation": {
                "truncated": truncated,
                "max_bytes": result.max_bytes,
                "omitted_nodes": omitted_nodes,
                "omitted_edges": omitted_edges,
                "omitted_query_hits": omitted_query_hits,
                "omitted_semantic_hits": omitted_semantic_hits,
                "omitted_bindings_by_limit": omitted_bindings_by_limit,
                "omitted_bindings_by_budget": omitted_bindings_by_budget,
                "snippets_truncated": snippet_truncated,
                "reasons": truncation_reasons,
            },
        });
        #[cfg(feature = "semantic-knowledge")]
        let value = {
            let mut value = value;
            crate::json::sort_value_keys(&mut value);
            value
        };
        let encoded = serde_json::to_string(&value)
            .map_err(|e| format!("context response assembly failed: {e}"))?;
        if encoded.len() <= result.max_bytes {
            return Ok(encoded);
        }
        if shrink_first_snippet_in(&mut semantic_hits)
            || shrink_first_snippet_in(&mut query_hits)
            || shrink_first_snippet(&mut bindings)
        {
            continue;
        } else if !semantic_hits.is_empty() {
            semantic_hits.pop();
            omitted_semantic_hits += 1;
        } else if !query_hits.is_empty() {
            query_hits.pop();
            omitted_query_hits += 1;
        } else if !edge_values.is_empty() {
            edge_values.pop();
            omitted_edges += 1;
            byte_omitted_edges += 1;
        } else if nodes.len() > 1 {
            nodes.pop();
            omitted_nodes += 1;
            byte_omitted_nodes += 1;
        } else if bindings["bindings"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        {
            bindings["bindings"].as_array_mut().unwrap().pop();
            omitted_bindings_by_budget += 1;
        } else {
            return Err("context response metadata exceeds max_bytes".to_string());
        }
    }
}

fn context_hits_json(
    retrieval: &crate::context::ContextRetrieval,
    root: &Path,
) -> Result<Vec<serde_json::Value>, String> {
    retrieval
        .hits
        .iter()
        .map(|hit| {
            let mut value: serde_json::Value =
                serde_json::from_str(&record_json(&hit.record, root))
                    .map_err(|e| format!("context candidate assembly failed: {e}"))?;
            if let Some(record) = value.as_object_mut() {
                let body = record
                    .remove("body")
                    .unwrap_or(serde_json::Value::String(String::new()));
                record.insert("snippet".to_string(), body);
                record.insert(
                    "snippet_truncated".to_string(),
                    serde_json::Value::Bool(false),
                );
                record.insert("relevance".to_string(), serde_json::json!("candidate"));
                if let Some(truncated) = hit.embedding_truncated {
                    record.insert(
                        "embedding_truncated".to_string(),
                        serde_json::json!(truncated),
                    );
                }
            }
            Ok(value)
        })
        .collect()
}

fn shrink_first_snippet(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(values) => values.iter_mut().rev().any(shrink_first_snippet),
        serde_json::Value::Object(fields) => {
            if let Some(serde_json::Value::String(snippet)) = fields.get_mut("snippet") {
                if !snippet.is_empty() {
                    let mut keep = snippet.len() / 2;
                    while !snippet.is_char_boundary(keep) {
                        keep -= 1;
                    }
                    snippet.truncate(keep);
                    fields.insert(
                        "snippet_truncated".to_string(),
                        serde_json::Value::Bool(true),
                    );
                    return true;
                }
            }
            fields.values_mut().any(shrink_first_snippet)
        }
        _ => false,
    }
}

fn shrink_first_snippet_in(values: &mut [serde_json::Value]) -> bool {
    values.iter_mut().any(shrink_first_snippet)
}

fn count_truncated_snippets(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => values.iter().map(count_truncated_snippets).sum(),
        serde_json::Value::Object(fields) => {
            usize::from(fields.get("snippet_truncated") == Some(&serde_json::Value::Bool(true)))
                + fields.values().map(count_truncated_snippets).sum::<usize>()
        }
        _ => 0,
    }
}

fn count_truncated_snippets_in(values: &[serde_json::Value]) -> usize {
    values.iter().map(count_truncated_snippets).sum()
}

// --- knowledge -------------------------------------------------------------

fn opt_str(value: &Option<String>) -> String {
    match value {
        Some(s) => format!("\"{}\"", json::escape(s)),
        None => "null".to_string(),
    }
}

fn opt_i64(value: Option<i64>) -> String {
    match value {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn opt_u64(value: Option<u64>) -> String {
    match value {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn opt_usize(value: Option<usize>) -> String {
    match value {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn measurement_json(m: &knowledge::BenchmarkData) -> String {
    let metrics: Vec<String> = m
        .metrics
        .iter()
        .map(|x| {
            format!(
                "{{\"name\":\"{}\",\"value\":{},\"unit\":\"{}\"}}",
                json::escape(&x.name),
                x.value,
                json::escape(&x.unit)
            )
        })
        .collect();
    format!(
        "{{\"method\":\"{}\",\"environment\":\"{}\",\"sample_count\":{},\"metrics\":[{}]}}",
        json::escape(&m.method),
        json::escape(&m.environment),
        m.sample_count,
        metrics.join(",")
    )
}

fn freshness_json(f: &knowledge::EvidenceFreshness) -> String {
    match f {
        knowledge::EvidenceFreshness::Fresh => "{\"status\":\"fresh\"}".to_string(),
        knowledge::EvidenceFreshness::Stale { observed_digest } => format!(
            "{{\"status\":\"stale\",\"observed_digest\":\"{}\"}}",
            json::escape(observed_digest)
        ),
        knowledge::EvidenceFreshness::Unresolved { reason } => {
            format!(
                "{{\"status\":\"unresolved\",\"reason\":\"{}\"}}",
                json::escape(reason)
            )
        }
    }
}

fn evidence_json(item: &knowledge::CapturedEvidence, root: &Path) -> String {
    let freshness = freshness_json(&knowledge_store::evidence_freshness(item, root));
    format!(
        "{{\"path\":\"{}\",\"byte_start\":{},\"byte_end\":{},\"digest\":\"{}\",\"git_head\":{},\"git_branch\":{},\"freshness\":{}}}",
        json::escape(&item.path),
        opt_i64(item.byte_start),
        opt_i64(item.byte_end),
        json::escape(&item.digest),
        opt_str(&item.git_head),
        opt_str(&item.git_branch),
        freshness
    )
}

/// One revision as JSON. Evidence is sampled against `root`; freshness is
/// reported, never used to promote the claim above `Unverified`.
pub(crate) fn record_json(record: &knowledge::RecordVersion, root: &Path) -> String {
    let evidence: Vec<String> = record
        .evidence
        .iter()
        .map(|item| evidence_json(item, root))
        .collect();
    let measurement = match &record.measurement {
        Some(m) => measurement_json(m),
        None => "null".to_string(),
    };
    // Full-stage lifecycle fields appear only when the revision actually
    // carries them, so Foundation output on v1 stores stays byte-identical.
    let mut lifecycle = String::new();
    if let Some(target) = &record.superseded_by {
        lifecycle.push_str(&format!(
            ",\"superseded_by\":{{\"id\":\"{}\",\"revision\":{}}}",
            json::escape(&target.id),
            target.revision
        ));
    }
    if let Some(reason) = &record.retracted_reason {
        lifecycle.push_str(&format!(
            ",\"retracted_reason\":\"{}\"",
            json::escape(reason)
        ));
    }
    if let Some(author) = &record.retracted_author {
        lifecycle.push_str(&format!(
            ",\"retracted_author\":\"{}\"",
            json::escape(author)
        ));
    }
    if let Some(state) = &record.confirm_state {
        lifecycle.push_str(&format!(",\"confirm_state\":\"{}\"", json::escape(state)));
    }
    format!(
        "{{\"id\":\"{}\",\"revision\":{},\"kind\":\"{}\",\"title\":\"{}\",\"body\":\"{}\",\"status\":\"{}\",\"author\":\"{}\",\"created_unix\":{},\"measurement\":{},\"evidence\":[{}],\"verification\":\"{}\",\"support\":\"{}\"{lifecycle}}}",
        json::escape(&record.id),
        record.revision,
        json::escape(&record.kind),
        json::escape(&record.title),
        json::escape(&record.body),
        json::escape(&record.status),
        json::escape(&record.author),
        record.created_unix,
        measurement,
        evidence.join(","),
        knowledge::VERIFICATION_UNVERIFIED,
        knowledge::evidence_support(record.evidence.len())
    )
}

pub(crate) fn relation_json(r: &knowledge_store::RelationView) -> String {
    // The confirmation state appears only once Full-stage data exists (v2
    // stores); v1 output stays byte-identical.
    let mut confirm = String::new();
    if let Some(state) = &r.confirm_state {
        confirm.push_str(&format!(",\"confirm_state\":\"{}\"", json::escape(state)));
    }
    format!(
        "{{\"relation_id\":\"{}\",\"src_id\":\"{}\",\"src_revision\":{},\"dst_id\":\"{}\",\"dst_revision\":{},\"kind\":\"{}\",\"author\":\"{}\",\"rationale\":\"{}\",\"created_unix\":{},\"retracted\":{},\"retraction_author\":{},\"retraction_reason\":{}{confirm}}}",
        json::escape(&r.relation_id),
        json::escape(&r.src_id),
        r.src_revision,
        json::escape(&r.dst_id),
        r.dst_revision,
        json::escape(&r.kind),
        json::escape(&r.author),
        json::escape(&r.rationale),
        r.created_unix,
        r.retracted,
        opt_str(&r.retraction_author),
        opt_str(&r.retraction_reason)
    )
}

pub(crate) fn pagination_json(
    limit: usize,
    offset: usize,
    total: usize,
    truncated: bool,
    next_offset: Option<usize>,
) -> String {
    format!(
        "{{\"limit\":{},\"offset\":{},\"total\":{},\"truncated\":{},\"next_offset\":{}}}",
        limit,
        offset,
        total,
        truncated,
        opt_usize(next_offset)
    )
}

#[cfg(feature = "semantic-knowledge")]
pub(crate) fn semantic_pagination_json(limit: usize) -> String {
    format!(
        "{{\"limit\":{limit},\"offset\":0,\"total\":null,\"truncated\":null,\"next_offset\":null,\"total_exact\":false}}"
    )
}

/// Knowledge envelope: one shape for every operation, with an embedded
/// newline (callers do not add another).
pub(crate) fn knowledge_envelope(
    repo: &str,
    generation: u64,
    operation: &str,
    result: &str,
    pagination: Option<String>,
) -> String {
    match pagination {
        Some(p) => format!(
            "{{\"schema_version\":1,\"repo\":\"{}\",\"generation\":{},\"operation\":\"{}\",\"result\":{},\"pagination\":{}}}\n",
            json::escape(repo),
            generation,
            operation,
            result,
            p
        ),
        None => format!(
            "{{\"schema_version\":1,\"repo\":\"{}\",\"generation\":{},\"operation\":\"{}\",\"result\":{}}}\n",
            json::escape(repo),
            generation,
            operation,
            result
        ),
    }
}

/// Search result assembly: the echoed query, optional exact evidence path,
/// visibility flag and hits form one object. KJSON-01: until CP5 this closed
/// the query object before `,"hits"`, so every successful search emitted
/// invalid JSON. Omitting `path` keeps the pre-path bytes unchanged.
pub(crate) fn knowledge_search_result_json(
    query: Option<&str>,
    path: Option<&str>,
    kind: Option<&str>,
    include_archived: bool,
    hits: &[knowledge::RecordVersion],
    root: &Path,
) -> String {
    let hits: Vec<String> = hits.iter().map(|r| record_json(r, root)).collect();
    knowledge_search_result_rendered_json(query, path, kind, include_archived, &hits)
}

fn knowledge_search_result_rendered_json(
    query: Option<&str>,
    path: Option<&str>,
    kind: Option<&str>,
    include_archived: bool,
    hits: &[String],
) -> String {
    let path = path
        .map(|p| format!(",\"path\":\"{}\"", json::escape(p)))
        .unwrap_or_default();
    format!(
        "{{\"query\":{}{path},\"kind\":{},\"include_archived\":{},\"hits\":[{}]}}",
        opt_str(&query.map(str::to_string)),
        opt_str(&kind.map(str::to_string)),
        include_archived,
        hits.join(",")
    )
}

#[cfg(feature = "semantic-knowledge")]
fn semantic_record_hit_json(hit: &crate::knowledge_vector::SemanticHit, root: &Path) -> String {
    let mut record = record_json(&hit.record, root);
    record.pop();
    format!(
        "{record},\"embedding_truncated\":{}}}",
        hit.embedding_truncated
    )
}

#[cfg(feature = "semantic-knowledge")]
pub(crate) fn knowledge_semantic_hits_result_json(
    query: Option<&str>,
    path: Option<&str>,
    kind: Option<&str>,
    hits: &[crate::knowledge_vector::SemanticHit],
    root: &Path,
    candidates_returned: usize,
) -> String {
    let hits = hits
        .iter()
        .map(|hit| semantic_record_hit_json(hit, root))
        .collect::<Vec<_>>();
    let mut result = knowledge_search_result_rendered_json(query, path, kind, false, &hits);
    result.pop();
    format!(
        "{result},\"semantic\":{{\"status\":\"semantic\",\"reason\":null,\"candidate_limit\":1000,\"candidates_returned\":{candidates_returned}}}}}"
    )
}

pub(crate) fn knowledge_semantic_search_result_json(
    query: Option<&str>,
    path: Option<&str>,
    kind: Option<&str>,
    hits: &[knowledge::RecordVersion],
    root: &Path,
    reason: Option<&str>,
) -> String {
    let mut result = knowledge_search_result_json(query, path, kind, false, hits, root);
    result.pop();
    format!(
        "{result},\"semantic\":{{\"status\":\"fallback\",\"reason\":{},\"candidate_limit\":1000,\"candidates_returned\":null}}}}",
        opt_str(&reason.map(str::to_string))
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn knowledge_search_output(
    search: crate::knowledge_search::SearchOutcome,
    query: Option<&str>,
    path: Option<&str>,
    kind: Option<&str>,
    include_archived: bool,
    limit: usize,
    offset: usize,
    root: &Path,
) -> (String, u64, String) {
    match search {
        crate::knowledge_search::SearchOutcome::Lexical(page) => (
            knowledge_search_result_json(query, path, kind, include_archived, &page.hits, root),
            page.generation,
            pagination_json(limit, offset, page.total, page.truncated, page.next_offset),
        ),
        #[cfg(feature = "semantic-knowledge")]
        crate::knowledge_search::SearchOutcome::Semantic {
            hits,
            generation,
            candidates_returned,
        } => (
            knowledge_semantic_hits_result_json(
                query,
                path,
                kind,
                &hits,
                root,
                candidates_returned,
            ),
            generation,
            semantic_pagination_json(limit),
        ),
        crate::knowledge_search::SearchOutcome::Fallback { page, reason } => (
            knowledge_semantic_search_result_json(
                query,
                path,
                kind,
                &page.hits,
                root,
                Some(reason),
            ),
            page.generation,
            pagination_json(limit, offset, page.total, page.truncated, page.next_offset),
        ),
    }
}

pub(crate) fn knowledge_related_result_json(
    id: &str,
    revision: Option<u64>,
    direction: &str,
    relations: &[knowledge_store::RelationView],
) -> String {
    let relations: Vec<String> = relations.iter().map(relation_json).collect();
    format!(
        "{{\"id\":\"{}\",\"revision\":{},\"direction\":\"{}\",\"relations\":[{}]}}",
        json::escape(id),
        opt_u64(revision),
        direction,
        relations.join(",")
    )
}

// --- semantic query ----------------------------------------------------------

/// One semantic hit: the shape the CLI and the MCP tool print. An LSP answer
/// carries no symbol identity, so `symbol_uri` and `display_name` are null.
pub(crate) fn semantic_hit_json(hit: &index::SemanticHit) -> String {
    format!(
        "{{\"symbol_uri\":{},\"display_name\":{},\"path\":\"{}\",\"span\":{{\"start_line\":{},\"start_col\":{},\"end_line\":{},\"end_col\":{},\"byte_start\":{},\"byte_end\":{}}},\"roles\":{},\"precision\":\"{}\",\"verification\":\"{}\"}}",
        match &hit.symbol_uri {
            Some(u) => format!("\"{}\"", json::escape(u)),
            None => "null".to_string(),
        },
        match &hit.display_name {
            Some(n) => format!("\"{}\"", json::escape(n)),
            None => "null".to_string(),
        },
        json::escape(&hit.path),
        hit.start_line,
        hit.start_col,
        hit.end_line,
        hit.end_col,
        hit.byte_start,
        hit.byte_end,
        hit.roles,
        hit.precision,
        hit.verification
    )
}

/// The semantic query document: the CLI's `semantic query` answer, byte for
/// byte, so the CLI and the MCP tool print the same document.
///
/// **The trailing newline is part of this format string** — unlike the other
/// formatters in this module, whose callers add it.
pub(crate) fn semantic_query_json(repo: &str, out: &index::SemanticQueryOut) -> String {
    let defs: Vec<String> = out.definitions.iter().map(semantic_hit_json).collect();
    let references: Vec<String> = out.references.iter().map(semantic_hit_json).collect();
    let diags: Vec<String> = out
        .server_diagnostics
        .iter()
        .map(|d| format!("\"{}\"", json::escape(d)))
        .collect();
    format!(
        "{{\"repo\":\"{}\",\"definitions\":[{}],\"references\":[{}],\"provider\":\"{}\",\"provider_version\":\"{}\",\"verification\":\"{}\",\"server_diagnostics\":[{}],\"fallback\":{}}}\n",
        json::escape(repo),
        defs.join(","),
        references.join(","),
        json::escape(&out.provider_name),
        json::escape(&out.provider_version),
        out.verification,
        diags.join(","),
        match &out.fallback {
            Some(reason) => semantic::fallback_report(&out.provider_name, reason),
            None => "null".to_string(),
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The semantic query document must stay byte-identical to the CLI's
    /// historical inline format, trailing newline included. The expected
    /// string is a verbatim copy of that format, so any change to the
    /// formatter shows up here.
    #[test]
    fn the_semantic_query_document_is_byte_identical_to_the_cli_format() {
        let hit = index::SemanticHit {
            symbol_uri: None,
            display_name: None,
            path: "src/main.rs".to_string(),
            start_line: 1,
            start_col: 4,
            end_line: 1,
            end_col: 7,
            byte_start: 10,
            byte_end: 13,
            roles: 1,
            precision: "Heuristic".to_string(),
            verification: "Unverified".to_string(),
        };
        let hit_json = format!(
            "{{\"symbol_uri\":null,\"display_name\":null,\"path\":\"{}\",\"span\":{{\"start_line\":{},\"start_col\":{},\"end_line\":{},\"end_col\":{},\"byte_start\":{},\"byte_end\":{}}},\"roles\":{},\"precision\":\"{}\",\"verification\":\"{}\"}}",
            hit.path,
            hit.start_line,
            hit.start_col,
            hit.end_line,
            hit.end_col,
            hit.byte_start,
            hit.byte_end,
            hit.roles,
            hit.precision,
            hit.verification
        );
        let out = index::SemanticQueryOut {
            definitions: vec![hit],
            references: Vec::new(),
            provider_name: "lsp-subprocess".to_string(),
            provider_version: "clangd version 23.1.4".to_string(),
            verification: "Unverified".to_string(),
            server_diagnostics: vec!["note".to_string()],
            fallback: None,
        };
        let expected = format!(
            "{{\"repo\":\"demo\",\"definitions\":[{}],\"references\":[],\"provider\":\"lsp-subprocess\",\"provider_version\":\"clangd version 23.1.4\",\"verification\":\"Unverified\",\"server_diagnostics\":[\"note\"],\"fallback\":null}}\n",
            hit_json
        );
        assert_eq!(semantic_query_json("demo", &out), expected);

        // The fallback form is the contract's payload, and the document keeps
        // the same shape.
        let failed = index::SemanticQueryOut {
            definitions: Vec::new(),
            references: Vec::new(),
            provider_name: "lsp-subprocess".to_string(),
            provider_version: String::new(),
            verification: "Unverified".to_string(),
            server_diagnostics: Vec::new(),
            fallback: Some("language server not found: 'x'".to_string()),
        };
        let expected_failed = format!(
            "{{\"repo\":\"demo\",\"definitions\":[],\"references\":[],\"provider\":\"lsp-subprocess\",\"provider_version\":\"\",\"verification\":\"Unverified\",\"server_diagnostics\":[],\"fallback\":{}}}\n",
            semantic::fallback_report("lsp-subprocess", "language server not found: 'x'")
        );
        assert_eq!(semantic_query_json("demo", &failed), expected_failed);
    }

    fn snapshot() -> store::Snapshot {
        store::Snapshot {
            id: 7,
            repo: "demo".to_string(),
            root: "C:\\repo".to_string(),
            created_unix: 100,
            head: Some("abc".to_string()),
            branch: None,
            dirty: false,
            untracked: 0,
            parser_version: 1,
            grammar_versions: "g".to_string(),
            config_fingerprint: "c".to_string(),
            pre_fingerprint: "p".to_string(),
            post_fingerprint: "q".to_string(),
            status_at_write: "fresh".to_string(),
            nonce: "n".to_string(),
        }
    }

    fn hit(name: Option<&str>) -> store::Hit {
        store::Hit {
            snapshot_id: 7,
            path: "lib.py".to_string(),
            kind: "function".to_string(),
            name: name.map(str::to_string),
            start_line: 1,
            start_col: 1,
            end_line: 2,
            end_col: 13,
            byte_start: 0,
            byte_end: 26,
            provider: "syntax".to_string(),
            grammar: "python:0.25.0".to_string(),
            language: "python".to_string(),
        }
    }

    fn syntax_anchor() -> index::EdgeAnchor {
        index::EdgeAnchor {
            kind: index::AnchorKind::SyntaxFallback,
            path: "lib.py".to_string(),
            line1: 1,
            col1: 6,
            symbol_uri: None,
            import_id: None,
            doc: None,
            display: None,
            candidate_display: None,
            span_bytes: Some((0, 26)),
            precision: "Heuristic".to_string(),
            uncertainty: Some(
                "syntax-only anchor: no semantic identity at this position".to_string(),
            ),
            data_member: false,
        }
    }

    fn completeness() -> index::CompletenessOut {
        let mut denominator = BTreeMap::new();
        denominator.insert("observed_edges", 1);
        index::CompletenessOut {
            status: "partial".to_string(),
            covered: 1,
            total: 1,
            denominator,
            unresolved: vec!["no-overlap".to_string()],
        }
    }

    #[test]
    fn search_no_index_shape_is_byte_stable() {
        let text = search_response_json(&SearchResponse {
            repo: "demo",
            root: "C:\\repo",
            snapshot: None,
            freshness: "unknown",
            reasons: &["no-index".to_string()],
            symbol: "tar\"get\\x",
            limit: 50,
            offset: 0,
            total: 0,
            hits: &[],
            occurrences: None,
        });
        assert_eq!(
            text,
            r#"{"repo":"demo","root":"C:\\repo","snapshot":null,"freshness":"unknown","reasons":["no-index"],"query":{"symbol":"tar\"get\\x","limit":50,"offset":0},"total":0,"results":[],"truncated":false}"#
        );
    }

    #[test]
    fn search_root_unreadable_shape_is_byte_stable() {
        let snap = snapshot();
        let text = search_response_json(&SearchResponse {
            repo: "demo",
            root: "C:\\repo",
            snapshot: Some(&snap),
            freshness: "unknown",
            reasons: &["root-unreadable".to_string()],
            symbol: "x",
            limit: 1,
            offset: 0,
            total: 0,
            hits: &[],
            occurrences: None,
        });
        assert_eq!(
            text,
            r#"{"repo":"demo","root":"C:\\repo","snapshot":{"id":7,"created_unix":100,"head":"abc","branch":null,"dirty":false,"status_at_write":"fresh","nonce":"n"},"freshness":"unknown","reasons":["root-unreadable"],"query":{"symbol":"x","limit":1,"offset":0},"total":0,"results":[],"truncated":false}"#
        );
    }

    #[test]
    fn search_hit_shape_and_truncation() {
        let snap = snapshot();
        let hits = vec![hit(Some("target"))];
        let text = search_response_json(&SearchResponse {
            repo: "demo",
            root: "C:\\repo",
            snapshot: Some(&snap),
            freshness: "fresh",
            reasons: &[],
            symbol: "target",
            limit: 50,
            offset: 0,
            total: 2,
            hits: &hits,
            occurrences: None,
        });
        assert_eq!(
            text,
            r#"{"repo":"demo","root":"C:\\repo","snapshot":{"id":7,"created_unix":100,"head":"abc","branch":null,"dirty":false,"status_at_write":"fresh","nonce":"n"},"freshness":"fresh","reasons":[],"query":{"symbol":"target","limit":50,"offset":0},"total":2,"results":[{"path":"lib.py","kind":"function","name":"target","span":{"start_line":1,"start_col":1,"end_line":2,"end_col":13},"bytes":[0,26],"provenance":{"provider":"syntax","grammar":"python:0.25.0","language":"python"},"snapshot":7}],"truncated":true}"#
        );
    }

    #[test]
    fn search_null_name_and_offset_truncation() {
        let snap = snapshot();
        let hits = vec![hit(None)];
        let text = search_response_json(&SearchResponse {
            repo: "demo",
            root: "C:\\repo",
            snapshot: Some(&snap),
            freshness: "fresh",
            reasons: &[],
            symbol: "x",
            limit: 1,
            offset: 1,
            total: 2,
            hits: &hits,
            occurrences: None,
        });
        assert_eq!(
            text,
            r#"{"repo":"demo","root":"C:\\repo","snapshot":{"id":7,"created_unix":100,"head":"abc","branch":null,"dirty":false,"status_at_write":"fresh","nonce":"n"},"freshness":"fresh","reasons":[],"query":{"symbol":"x","limit":1,"offset":1},"total":2,"results":[{"path":"lib.py","kind":"function","name":null,"span":{"start_line":1,"start_col":1,"end_line":2,"end_col":13},"bytes":[0,26],"provenance":{"provider":"syntax","grammar":"python:0.25.0","language":"python"},"snapshot":7}],"truncated":false}"#
        );
    }

    #[test]
    fn search_occurrence_section_shape_is_byte_stable() {
        let snap = snapshot();
        let list = store::OccurrenceList {
            field_declarations: 1,
            symbol_declarations: 2,
            total: 3,
            items: vec![store::OccurrenceHit {
                path: "use.cc".to_string(),
                role: "macro-argument".to_string(),
                start_line: 4,
                start_col: 9,
                end_line: 4,
                end_col: 12,
                byte_start: 40,
                byte_end: 43,
                provider: "syntax".to_string(),
                grammar: "cpp:0.23.4".to_string(),
                language: "cpp".to_string(),
            }],
        };
        let text = search_response_json(&SearchResponse {
            repo: "demo",
            root: "C:\\repo",
            snapshot: Some(&snap),
            freshness: "fresh",
            reasons: &[],
            symbol: "b",
            limit: 1,
            offset: 0,
            total: 0,
            hits: &[],
            occurrences: Some(&list),
        });
        assert_eq!(
            text,
            r#"{"repo":"demo","root":"C:\\repo","snapshot":{"id":7,"created_unix":100,"head":"abc","branch":null,"dirty":false,"status_at_write":"fresh","nonce":"n"},"freshness":"fresh","reasons":[],"query":{"symbol":"b","limit":1,"offset":0},"total":0,"results":[],"truncated":false,"occurrences":{"scope":"repository-wide-exact-name","binding":"unbound","field_declarations":1,"symbol_declarations":2,"total":3,"truncated":true,"items":[{"path":"use.cc","role":"macro-argument","confidence":"candidate","span":{"start_line":4,"start_col":9,"end_line":4,"end_col":12},"bytes":[40,43],"provenance":{"provider":"syntax","grammar":"cpp:0.23.4","language":"cpp"}}]}}"#
        );
    }

    #[test]
    fn edge_query_minimal_shape_is_byte_stable() {
        let snap = snapshot();
        let out = index::EdgeQueryOut {
            repo: "demo".to_string(),
            snapshot: snap,
            revision: "r1".to_string(),
            direction: index::EdgeDir::Callers,
            anchor: syntax_anchor(),
            freshness: "fresh".to_string(),
            freshness_reasons: Vec::new(),
            verification: "Unverified".to_string(),
            completeness: completeness(),
            edges: Vec::new(),
            truncated: false,
            limit: 50,
            offset: 0,
            next_offset: None,
        };
        assert_eq!(
            edge_query_json(&out),
            r#"{"schema_version":1,"repo":"demo","snapshot":{"id":7,"nonce":"n","revision":"r1"},"direction":"callers","anchor":{"kind":"syntax-fallback","path":"lib.py","line":1,"col":6,"symbol_uri":null,"import_id":null,"precision":"Heuristic","anchor_uncertainty":"syntax-only anchor: no semantic identity at this position","span":[0,26],"endpoint":{"type":"syntax_def","import_id":null,"symbol_uri":null,"doc":null,"path":"lib.py","byte_start":0,"byte_end":26}},"freshness":{"status":"fresh","reasons":[]},"verification":"Unverified","completeness":{"status":"partial","covered":1,"total":1,"denominator":{"observed_edges":1},"unresolved":["no-overlap"]},"edges":[],"truncated":false,"limits":{"limit":50,"offset":0,"next_offset":null}}"#
        );
    }

    #[test]
    fn edge_hit_with_unique_import_is_byte_stable() {
        let snap = snapshot();
        let import = store::SemanticImport {
            snapshot_id: 1,
            provider_name: "scip-test".to_string(),
            provider_version: "1.0".to_string(),
            capability: "c".to_string(),
            project_root: "p".to_string(),
            text_encoding: "utf-8".to_string(),
            artifact_id: "a".to_string(),
            source_revision: "s".to_string(),
            claimed_source_digest: String::new(),
            content_identity: "c".to_string(),
            external_symbols: String::new(),
            verification: "Unverified".to_string(),
        };
        let out = index::EdgeQueryOut {
            repo: "demo".to_string(),
            snapshot: snap,
            revision: "r1".to_string(),
            direction: index::EdgeDir::Callees,
            anchor: syntax_anchor(),
            freshness: "fresh".to_string(),
            freshness_reasons: vec!["reason".to_string()],
            verification: "Unverified".to_string(),
            completeness: completeness(),
            edges: vec![index::EdgeHitOut {
                edge: store::EdgeRow {
                    snapshot_id: 1,
                    file_id: 2,
                    caller: Some(store::EdgeEndpoint::Scip {
                        import_id: 3,
                        symbol_uri: "sym".to_string(),
                        doc: None,
                    }),
                    caller_path: "main.py".to_string(),
                    caller_span_bytes: (49, 57),
                    callee: None,
                    callee_name: "target".to_string(),
                    edge_kind: "POTENTIAL_CALL".to_string(),
                    precision: "Heuristic".to_string(),
                    provider: "syntax".to_string(),
                    evidence: "{}".to_string(),
                    unresolved: Some("no-overlap".to_string()),
                },
                verification: "Unverified".to_string(),
                evidence: index::EdgeEvidence {
                    digest: "d".to_string(),
                    callee_span: Some((49, 55)),
                    terminal_span: Some((49, 55)),
                    reference_span: None,
                    imports: vec![(3, import)],
                },
            }],
            truncated: true,
            limit: 1,
            offset: 2,
            next_offset: Some(3),
        };
        assert_eq!(
            edge_query_json(&out),
            r#"{"schema_version":1,"repo":"demo","snapshot":{"id":7,"nonce":"n","revision":"r1"},"direction":"callees","anchor":{"kind":"syntax-fallback","path":"lib.py","line":1,"col":6,"symbol_uri":null,"import_id":null,"precision":"Heuristic","anchor_uncertainty":"syntax-only anchor: no semantic identity at this position","span":[0,26],"endpoint":{"type":"syntax_def","import_id":null,"symbol_uri":null,"doc":null,"path":"lib.py","byte_start":0,"byte_end":26}},"freshness":{"status":"fresh","reasons":["reason"]},"verification":"Unverified","completeness":{"status":"partial","covered":1,"total":1,"denominator":{"observed_edges":1},"unresolved":["no-overlap"]},"edges":[{"kind":"POTENTIAL_CALL","precision":"Heuristic","caller":{"type":"scip","import_id":3,"symbol_uri":"sym","doc":null,"path":null,"byte_start":null,"byte_end":null},"callee":null,"caller_path":"main.py","caller_span":[49,57],"callee_name":"target","provider":"syntax","import_id":3,"tool":"scip-test","verification":"Unverified","unresolved":"no-overlap","evidence":{"path":"main.py","span":[49,57],"callee_span":[49,55],"terminal_span":[49,55],"reference_span":null,"digest":"d","provider":"syntax","imports":[{"id":3,"provider":"scip-test","version":"1.0","artifact_id":"a","source_revision":"s","content_identity":"c"}]}}],"truncated":true,"limits":{"limit":1,"offset":2,"next_offset":3}}"#
        );
    }

    #[test]
    fn trace_query_minimal_shape_is_byte_stable() {
        let snap = snapshot();
        let out = index::TraceQueryOut {
            impact_verdict: None,
            repo: "demo".to_string(),
            snapshot: snap,
            revision: "r1".to_string(),
            anchor: syntax_anchor(),
            destination: None,
            freshness: "fresh".to_string(),
            freshness_reasons: Vec::new(),
            verification: "Unverified".to_string(),
            completeness: completeness(),
            total_exact: true,
            paths: vec![index::TracePathOut {
                edges: Vec::new(),
                cycle: false,
                truncated: false,
                unresolved: Vec::new(),
                verification: "Unverified".to_string(),
            }],
            bridges: Vec::new(),
            nodes: 1,
            edge_visits: 1,
            cycle: false,
            truncated: false,
            unresolved: vec!["no-overlap".to_string()],
            depth: 3,
            limit: 50,
            offset: 0,
            next_offset: None,
        };
        assert_eq!(
            trace_query_json(&out),
            r#"{"schema_version":1,"repo":"demo","snapshot":{"id":7,"nonce":"n","revision":"r1"},"direction":"trace","anchor":{"kind":"syntax-fallback","path":"lib.py","line":1,"col":6,"symbol_uri":null,"import_id":null,"precision":"Heuristic","anchor_uncertainty":"syntax-only anchor: no semantic identity at this position","span":[0,26],"endpoint":{"type":"syntax_def","import_id":null,"symbol_uri":null,"doc":null,"path":"lib.py","byte_start":0,"byte_end":26}},"destination":null,"freshness":{"status":"fresh","reasons":[]},"verification":"Unverified","completeness":{"status":"partial","covered":1,"total":1,"total_exact":true,"denominator":{"observed_edges":1},"unresolved":["no-overlap"]},"paths":[{"edges":[],"cycle":false,"truncated":false,"verification":"Unverified","unresolved":[]}],"bridges":[],"visits":{"nodes":1,"edges":1},"budgets":{"depth":3,"max_depth":6,"nodes":500,"edges":3000,"paths":1000,"mappings":500,"time_ms":2000},"limits":{"limit":50,"offset":0,"next_offset":null},"cycle":false,"truncated":false,"unresolved":["no-overlap"],"disclaimer":"plausible-path-not-execution"}"#
        );
    }

    #[test]
    fn trace_query_impact_verdict_shape_is_byte_stable() {
        let snap = snapshot();
        let out = index::TraceQueryOut {
            impact_verdict: Some("plausible-impact"),
            repo: "demo".to_string(),
            snapshot: snap,
            revision: "r1".to_string(),
            anchor: syntax_anchor(),
            destination: Some(syntax_anchor()),
            freshness: "fresh".to_string(),
            freshness_reasons: Vec::new(),
            verification: "Unverified".to_string(),
            completeness: completeness(),
            total_exact: false,
            paths: vec![index::TracePathOut {
                edges: Vec::new(),
                cycle: true,
                truncated: true,
                unresolved: vec!["u".to_string()],
                verification: "Unverified".to_string(),
            }],
            bridges: Vec::new(),
            nodes: 2,
            edge_visits: 2,
            cycle: true,
            truncated: true,
            unresolved: vec!["u".to_string()],
            depth: 2,
            limit: 1,
            offset: 4,
            next_offset: Some(5),
        };
        assert_eq!(
            trace_query_json(&out),
            r#"{"verdict":"plausible-impact","schema_version":1,"repo":"demo","snapshot":{"id":7,"nonce":"n","revision":"r1"},"direction":"impact","anchor":{"kind":"syntax-fallback","path":"lib.py","line":1,"col":6,"symbol_uri":null,"import_id":null,"precision":"Heuristic","anchor_uncertainty":"syntax-only anchor: no semantic identity at this position","span":[0,26],"endpoint":{"type":"syntax_def","import_id":null,"symbol_uri":null,"doc":null,"path":"lib.py","byte_start":0,"byte_end":26}},"destination":{"kind":"syntax-fallback","path":"lib.py","line":1,"col":6,"symbol_uri":null,"import_id":null,"precision":"Heuristic","anchor_uncertainty":"syntax-only anchor: no semantic identity at this position","span":[0,26],"endpoint":{"type":"syntax_def","import_id":null,"symbol_uri":null,"doc":null,"path":"lib.py","byte_start":0,"byte_end":26}},"freshness":{"status":"fresh","reasons":[]},"verification":"Unverified","completeness":{"status":"partial","covered":1,"total":1,"total_exact":false,"denominator":{"observed_edges":1},"unresolved":["no-overlap"]},"paths":[{"distance":0,"scope":"transitive","edges":[],"cycle":true,"truncated":true,"verification":"Unverified","unresolved":["u"]}],"bridges":[],"visits":{"nodes":2,"edges":2},"budgets":{"depth":2,"max_depth":6,"nodes":500,"edges":3000,"paths":1000,"mappings":500,"time_ms":2000},"limits":{"limit":1,"offset":4,"next_offset":5},"cycle":true,"truncated":true,"unresolved":["u"],"disclaimer":"plausible-impact-not-execution"}"#
        );
    }

    #[test]
    fn record_json_minimal_shape_is_byte_stable() {
        let record = knowledge::RecordVersion {
            id: "d1".to_string(),
            revision: 1,
            kind: "decision".to_string(),
            title: "First".to_string(),
            body: "alpha".to_string(),
            status: "active".to_string(),
            author: "a".to_string(),
            created_unix: 100,
            superseded_by: None,
            retracted_reason: None,
            retracted_author: None,
            confirm_state: None,
            measurement: None,
            evidence: Vec::new(),
        };
        assert_eq!(
            record_json(&record, Path::new(".")),
            r#"{"id":"d1","revision":1,"kind":"decision","title":"First","body":"alpha","status":"active","author":"a","created_unix":100,"measurement":null,"evidence":[],"verification":"Unverified","support":"unknown"}"#
        );
    }

    #[cfg(feature = "semantic-knowledge")]
    #[test]
    fn semantic_search_reports_passage_embedding_truncation_per_hit() {
        let hit = crate::knowledge_vector::SemanticHit {
            record: knowledge::RecordVersion {
                id: "d1".to_string(),
                revision: 1,
                kind: "decision".to_string(),
                title: "First".to_string(),
                body: "alpha".to_string(),
                status: "active".to_string(),
                author: "a".to_string(),
                created_unix: 100,
                superseded_by: None,
                retracted_reason: None,
                retracted_author: None,
                confirm_state: None,
                measurement: None,
                evidence: Vec::new(),
            },
            embedding_truncated: true,
        };
        assert_eq!(
            knowledge_semantic_hits_result_json(
                Some("q"),
                None,
                None,
                &[hit],
                Path::new("."),
                1,
            ),
            "{\"query\":\"q\",\"kind\":null,\"include_archived\":false,\"hits\":[{\"id\":\"d1\",\"revision\":1,\"kind\":\"decision\",\"title\":\"First\",\"body\":\"alpha\",\"status\":\"active\",\"author\":\"a\",\"created_unix\":100,\"measurement\":null,\"evidence\":[],\"verification\":\"Unverified\",\"support\":\"unknown\",\"embedding_truncated\":true}],\"semantic\":{\"status\":\"semantic\",\"reason\":null,\"candidate_limit\":1000,\"candidates_returned\":1}}"
        );
    }

    #[test]
    fn record_json_measurement_and_lifecycle_shape_is_byte_stable() {
        let record = knowledge::RecordVersion {
            id: "d1".to_string(),
            revision: 2,
            kind: "benchmark".to_string(),
            title: "Perf".to_string(),
            body: "b".to_string(),
            status: "superseded".to_string(),
            author: "a".to_string(),
            created_unix: 200,
            superseded_by: Some(knowledge::SupersededRef {
                id: "d2".to_string(),
                revision: 3,
            }),
            retracted_reason: Some("r".to_string()),
            retracted_author: Some("b".to_string()),
            confirm_state: Some("candidate".to_string()),
            measurement: Some(knowledge::BenchmarkData {
                method: "m".to_string(),
                environment: "e".to_string(),
                sample_count: 3,
                metrics: vec![knowledge::MetricData {
                    name: "latency".to_string(),
                    value: 1.5,
                    unit: "ms".to_string(),
                }],
            }),
            evidence: Vec::new(),
        };
        assert_eq!(
            record_json(&record, Path::new(".")),
            r#"{"id":"d1","revision":2,"kind":"benchmark","title":"Perf","body":"b","status":"superseded","author":"a","created_unix":200,"measurement":{"method":"m","environment":"e","sample_count":3,"metrics":[{"name":"latency","value":1.5,"unit":"ms"}]},"evidence":[],"verification":"Unverified","support":"unknown","superseded_by":{"id":"d2","revision":3},"retracted_reason":"r","retracted_author":"b","confirm_state":"candidate"}"#
        );
    }

    #[test]
    fn relation_json_shape_is_byte_stable() {
        let relation = knowledge_store::RelationView {
            relation_id: "r1".to_string(),
            src_id: "d1".to_string(),
            src_revision: 1,
            dst_id: "d2".to_string(),
            dst_revision: 1,
            kind: "supports".to_string(),
            author: "a".to_string(),
            rationale: "because".to_string(),
            created_unix: 100,
            retracted: false,
            retraction_author: None,
            retraction_reason: None,
            confirm_state: None,
        };
        assert_eq!(
            relation_json(&relation),
            r#"{"relation_id":"r1","src_id":"d1","src_revision":1,"dst_id":"d2","dst_revision":1,"kind":"supports","author":"a","rationale":"because","created_unix":100,"retracted":false,"retraction_author":null,"retraction_reason":null}"#
        );
        let retracted = knowledge_store::RelationView {
            retracted: true,
            retraction_author: Some("b".to_string()),
            retraction_reason: Some("stale".to_string()),
            confirm_state: Some("confirmed".to_string()),
            ..relation
        };
        assert_eq!(
            relation_json(&retracted),
            r#"{"relation_id":"r1","src_id":"d1","src_revision":1,"dst_id":"d2","dst_revision":1,"kind":"supports","author":"a","rationale":"because","created_unix":100,"retracted":true,"retraction_author":"b","retraction_reason":"stale","confirm_state":"confirmed"}"#
        );
    }

    #[test]
    fn pagination_json_shape_is_byte_stable() {
        assert_eq!(
            pagination_json(50, 0, 3, true, Some(1)),
            r#"{"limit":50,"offset":0,"total":3,"truncated":true,"next_offset":1}"#
        );
        assert_eq!(
            pagination_json(50, 0, 0, false, None),
            r#"{"limit":50,"offset":0,"total":0,"truncated":false,"next_offset":null}"#
        );
    }

    #[test]
    fn knowledge_envelope_shape_is_byte_stable() {
        assert_eq!(
            knowledge_envelope("demo", 4, "show", r#"{"id":"d1"}"#, None),
            "{\"schema_version\":1,\"repo\":\"demo\",\"generation\":4,\"operation\":\"show\",\"result\":{\"id\":\"d1\"}}\n"
        );
        assert_eq!(
            knowledge_envelope(
                "demo",
                5,
                "search",
                r#"{"query":"x","hits":[]}"#,
                Some(pagination_json(50, 0, 0, false, None))
            ),
            "{\"schema_version\":1,\"repo\":\"demo\",\"generation\":5,\"operation\":\"search\",\"result\":{\"query\":\"x\",\"hits\":[]},\"pagination\":{\"limit\":50,\"offset\":0,\"total\":0,\"truncated\":false,\"next_offset\":null}}\n"
        );
    }

    #[test]
    fn knowledge_search_result_shape_is_byte_stable() {
        // KJSON-01: the query echo and the hits live in one object. Before the
        // CP5 correction the query object was closed before `,"hits"`, which
        // made every successful search output invalid JSON.
        assert_eq!(
            knowledge_search_result_json(Some("alpha"), None, None, false, &[], Path::new(".")),
            r#"{"query":"alpha","kind":null,"include_archived":false,"hits":[]}"#
        );
        assert_eq!(
            knowledge_search_result_json(
                Some("alpha"),
                Some("src/main.rs"),
                None,
                false,
                &[],
                Path::new(".")
            ),
            r#"{"query":"alpha","path":"src/main.rs","kind":null,"include_archived":false,"hits":[]}"#
        );
        assert_eq!(
            knowledge_search_result_json(
                None,
                Some("src/main.rs"),
                None,
                false,
                &[],
                Path::new(".")
            ),
            r#"{"query":null,"path":"src/main.rs","kind":null,"include_archived":false,"hits":[]}"#
        );
        let record = knowledge::RecordVersion {
            id: "d1".to_string(),
            revision: 1,
            kind: "decision".to_string(),
            title: "First".to_string(),
            body: "alpha".to_string(),
            status: "active".to_string(),
            author: "a".to_string(),
            created_unix: 100,
            superseded_by: None,
            retracted_reason: None,
            retracted_author: None,
            confirm_state: None,
            measurement: None,
            evidence: Vec::new(),
        };
        assert_eq!(
            knowledge_search_result_json(
                Some("alpha"),
                None,
                Some("benchmark"),
                true,
                &[record],
                Path::new(".")
            ),
            r#"{"query":"alpha","kind":"benchmark","include_archived":true,"hits":[{"id":"d1","revision":1,"kind":"decision","title":"First","body":"alpha","status":"active","author":"a","created_unix":100,"measurement":null,"evidence":[],"verification":"Unverified","support":"unknown"}]}"#
        );
    }

    #[test]
    fn knowledge_related_result_shape_is_byte_stable() {
        assert_eq!(
            knowledge_related_result_json("d1", None, "outbound", &[]),
            r#"{"id":"d1","revision":null,"direction":"outbound","relations":[]}"#
        );
        let relation = knowledge_store::RelationView {
            relation_id: "r1".to_string(),
            src_id: "d1".to_string(),
            src_revision: 1,
            dst_id: "d2".to_string(),
            dst_revision: 1,
            kind: "supports".to_string(),
            author: "a".to_string(),
            rationale: "because".to_string(),
            created_unix: 100,
            retracted: false,
            retraction_author: None,
            retraction_reason: None,
            confirm_state: None,
        };
        assert_eq!(
            knowledge_related_result_json("d1", Some(2), "inbound", &[relation]),
            r#"{"id":"d1","revision":2,"direction":"inbound","relations":[{"relation_id":"r1","src_id":"d1","src_revision":1,"dst_id":"d2","dst_revision":1,"kind":"supports","author":"a","rationale":"because","created_unix":100,"retracted":false,"retraction_author":null,"retraction_reason":null}]}"#
        );
    }

    #[test]
    fn index_status_no_index_shape_is_byte_stable() {
        let freshness = index::Freshness {
            status: "unknown".to_string(),
            reasons: vec!["no-index".to_string()],
            snapshot: None,
        };
        assert_eq!(
            index_status_json("demo", "C:\\repo", &freshness, false),
            r#"{"repo":"demo","root":"C:\\repo","freshness":"unknown","reasons":["no-index"],"fts":false,"snapshot":null}"#
        );
    }

    #[test]
    fn index_status_stale_shape_with_snapshot_is_byte_stable() {
        let freshness = index::Freshness {
            status: "stale".to_string(),
            reasons: vec!["root-moved".to_string(), "content-changed".to_string()],
            snapshot: Some(snapshot()),
        };
        assert_eq!(
            index_status_json("demo", "C:\\repo", &freshness, true),
            r#"{"repo":"demo","root":"C:\\repo","freshness":"stale","reasons":["root-moved","content-changed"],"fts":true,"snapshot":{"id":7,"created_unix":100,"head":"abc","branch":null,"dirty":false,"status_at_write":"fresh","nonce":"n"}}"#
        );
    }

    #[test]
    fn repo_status_shape_is_byte_stable() {
        let g = git::GitInfo {
            present: true,
            valid: false,
            head: None,
            branch: Some("main".to_string()),
            dirty: true,
            untracked: 3,
            toplevel: None,
            worktree: true,
        };
        let ig = ignore::IgnoreInfo {
            file_present: false,
            rules: 0,
        };
        assert_eq!(
            repo_status_json("de\"mo", "C:\\repo", &g, &ig),
            r#"{"name":"de\"mo","root":"C:\\repo","git":{"present":true,"valid":false,"head":null,"branch":"main","dirty":true,"untracked":3,"toplevel":null,"worktree":true},"ignore":{"file":false,"rules":0}}"#
        );
    }
}
