// SPDX-License-Identifier: Apache-2.0
//! M8 CP1/CP3/CP4/CP5/CP6: MCP stdio server. CP1 serves initialize; CP3
//! registers the first read-only tools; CP4 adds the graph tools (callers,
//! callees, trace, impact); CP5 adds the knowledge tools (search, show,
//! related); CP6 adds the composed `riltra_health` tool — fourteen tools total
//! (design gate §3) through the rmcp tool macros. Tool calls
//! reuse the CLI's response formatters byte-for-byte, and the index read path
//! (`store::open_read`) and the knowledge read path
//! (`knowledge_store::open_knowledge(…, false)`) never create, migrate, or
//! replace a database. stdout stays protocol-only; blocking work runs on the
//! tokio blocking pool. `riltra_code_semantic` is the one tool that starts a
//! subprocess (the operator's language server, `RILTRA_LSP_SERVER`); it
//! reads the repository and never touches the index or the knowledge store.

use std::path::{Path, PathBuf};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::schemars::JsonSchema;
use rmcp::{serve_server, tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;

#[cfg(feature = "semantic-knowledge")]
use crate::json;
use crate::knowledge_store_absent;
use crate::{
    context, doctor, git, ignore, index, knowledge, knowledge_binding, knowledge_search,
    knowledge_store, paths, registry, response, store,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct RepoStatusInput {
    /// Registered repository name.
    repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct CodeSearchInput {
    /// Registered repository name.
    repo: String,
    /// Symbol text to search for (the CLI's `search <repo> <symbol>` argument).
    query: String,
    /// Maximum hits to return (CLI default 50, maximum 1000).
    limit: Option<usize>,
    /// Hits to skip before returning results (default 0).
    offset: Option<usize>,
    /// Also list where the exact name occurs in C++ code (member accesses,
    /// the address of a member, same-file macro arguments): a name census,
    /// not a binding. Off by default; the document is unchanged without it.
    occurrences: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct EdgeInput {
    /// Registered repository name.
    repo: String,
    /// Repository-relative path of the anchor position (required unless `target` is given).
    path: Option<String>,
    /// 1-based line of the anchor position (required unless `target` is given).
    line: Option<u32>,
    /// 1-based column of the anchor position (required unless `target` is given).
    col: Option<u32>,
    /// Exact definition name to anchor on instead of a position; an ambiguous or unknown name answers a document, never a guess.
    target: Option<String>,
    /// Repository-relative path that the `target` definition must be in.
    #[serde(rename = "in")]
    in_path: Option<String>,
    /// Definition kind the `target` must have (for example `function`, `method`, `class`).
    kind: Option<String>,
    /// Optional exact SCIP symbol identity to check against the position.
    symbol: Option<String>,
    /// Maximum edges to return (CLI default 50, maximum 1000).
    limit: Option<usize>,
    /// Edges to skip before returning results (default 0).
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct TraceInput {
    /// Registered repository name.
    repo: String,
    /// Repository-relative path of the anchor position (required unless `target` is given).
    path: Option<String>,
    /// 1-based line of the anchor position (required unless `target` is given).
    line: Option<u32>,
    /// 1-based column of the anchor position (required unless `target` is given).
    col: Option<u32>,
    /// Exact definition name to anchor on instead of a position; an ambiguous or unknown name answers a document, never a guess.
    target: Option<String>,
    /// Repository-relative path that the `target` definition must be in.
    #[serde(rename = "in")]
    in_path: Option<String>,
    /// Definition kind the `target` must have (for example `function`, `method`, `class`).
    kind: Option<String>,
    /// Optional exact SCIP symbol identity to check against the position.
    symbol: Option<String>,
    /// Optional destination anchor `PATH:LINE:COL` (CLI `--to` grammar).
    to: Option<String>,
    /// Maximum traversal depth (CLI default 3, 1-6).
    depth: Option<usize>,
    /// Maximum paths to return (CLI default 50, maximum 1000).
    limit: Option<usize>,
    /// Paths to skip before returning results (default 0).
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct ImpactInput {
    /// Registered repository name.
    repo: String,
    /// Repository-relative path of the anchor position (required unless `target` is given).
    path: Option<String>,
    /// 1-based line of the anchor position (required unless `target` is given).
    line: Option<u32>,
    /// 1-based column of the anchor position (required unless `target` is given).
    col: Option<u32>,
    /// Exact definition name to anchor on instead of a position; an ambiguous or unknown name answers a document, never a guess.
    target: Option<String>,
    /// Repository-relative path that the `target` definition must be in.
    #[serde(rename = "in")]
    in_path: Option<String>,
    /// Definition kind the `target` must have (for example `function`, `method`, `class`).
    kind: Option<String>,
    /// Optional exact SCIP symbol identity to check against the position.
    symbol: Option<String>,
    /// Maximum traversal depth (CLI default 3, 1-6). Impact takes no
    /// destination, and unknown parameters are rejected.
    depth: Option<usize>,
    /// Maximum paths to return (CLI default 50, maximum 1000).
    limit: Option<usize>,
    /// Paths to skip before returning results (default 0).
    offset: Option<usize>,
    /// Add the `summary` object, counted over all traversal paths (not the page).
    #[serde(default)]
    summary: bool,
    /// With `summary`: count test-file paths too (a path heuristic, see the docs).
    #[serde(default)]
    include_tests: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct ContextInput {
    /// Registered repository name.
    repo: String,
    /// Repository-relative path of the anchor position (required unless `target` is given).
    path: Option<String>,
    /// 1-based line of the anchor position (required unless `target` is given).
    line: Option<u32>,
    /// 1-based byte column of the anchor position (required unless `target` is given).
    col: Option<u32>,
    /// Exact definition name to anchor on instead of a position; an ambiguous or unknown name answers a document, never a guess.
    target: Option<String>,
    /// Repository-relative path that the `target` definition must be in.
    #[serde(rename = "in")]
    in_path: Option<String>,
    /// Definition kind the `target` must have (for example `function`, `method`, `class`).
    kind: Option<String>,
    /// Optional exact SCIP symbol identity to check against the position.
    symbol: Option<String>,
    /// Optional literal query over active knowledge records.
    query: Option<String>,
    /// Add opt-in local semantic candidates; requires `query` and reports lexical fallback when unavailable.
    semantic: Option<bool>,
    /// Maximum graph depth (default 3, range 1-6).
    depth: Option<usize>,
    /// Maximum context nodes returned (default 20, range 1-100).
    limit: Option<usize>,
    /// Maximum final JSON bytes (default 32768, range 8192-131072).
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct KnowledgeSearchInput {
    /// Registered repository name.
    repo: String,
    /// Literal text searched in titles and bodies (CLI
    /// `knowledge search --query`). Text or path is required.
    query: Option<String>,
    /// Exact stored evidence path (CLI `knowledge search --path`). It is
    /// matched byte for byte, never normalized, and may be paired with a
    /// query to intersect text and path.
    path: Option<String>,
    /// Optional record kind filter (CLI `--kind`).
    kind: Option<String>,
    /// Use the local semantic index when available; otherwise report an FTS5 fallback.
    semantic: Option<bool>,
    /// Include archived records (CLI `--include-archived`; default false).
    include_archived: Option<bool>,
    /// Include superseded records (CLI `--include-superseded`; default false).
    include_superseded: Option<bool>,
    /// Include retracted records (CLI `--include-retracted`; default false).
    include_retracted: Option<bool>,
    /// Maximum hits to return (CLI default 50, maximum 1000).
    limit: Option<usize>,
    /// Hits to skip before returning results (default 0).
    offset: Option<usize>,
    /// Expected store generation; a stale value conflicts instead of serving
    /// a shifted page.
    generation: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct KnowledgeShowInput {
    /// Registered repository name.
    repo: String,
    /// Record id to read.
    id: String,
    /// Exact revision to read (CLI `--revision`; default is the current head).
    revision: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct KnowledgeRelatedInput {
    /// Registered repository name.
    repo: String,
    /// Record id whose relations are read.
    id: String,
    /// Exact revision whose bindings are read (default is the current head).
    revision: Option<u64>,
    /// `outbound` (authored links, the default) or `inbound` (links pointing
    /// at the record).
    direction: Option<String>,
    /// Include retracted relations (CLI `--include-retracted`; default false).
    include_retracted: Option<bool>,
    /// Maximum relations to return (CLI default 50, maximum 1000).
    limit: Option<usize>,
    /// Relations to skip before returning results (default 0).
    offset: Option<usize>,
    /// Expected store generation; a stale value conflicts instead of serving
    /// a shifted page.
    generation: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct KnowledgeAtInput {
    /// Registered repository name.
    repo: String,
    /// Repository-relative path containing the anchor.
    path: String,
    /// 1-based line and column of the source anchor.
    line: u32,
    col: u32,
    /// Optional exact SCIP symbol identity.
    symbol: Option<String>,
    /// Maximum bindings to return (default 50, maximum 1000).
    limit: Option<usize>,
    /// Bindings to skip before returning results (default 0).
    offset: Option<usize>,
    /// Expected store generation; a stale value conflicts instead of serving a shifted page.
    generation: Option<u64>,
}

/// Health takes no arguments; any custom field is a schema-level refusal.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct HealthInput {}

/// Which semantic question the client asked.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(crate = "rmcp::schemars")]
enum Operation {
    Definition,
    References,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(crate = "rmcp::schemars")]
struct SemanticInput {
    /// Registered repository name.
    repo: String,
    /// Repository-relative path of the anchor position.
    path: String,
    /// 1-based line of the anchor position.
    line: u32,
    /// 1-based column of the anchor position, in the position unit the
    /// language server negotiated (utf-16 by default). This is NOT the byte
    /// column the graph tools use.
    col: u32,
    /// "definition" or "references".
    operation: Operation,
}

struct RiltraMcp;

#[tool_router]
impl RiltraMcp {
    #[tool(
        name = "riltra_repo_list",
        description = "List registered repositories (same document as CLI `repo list`). When to call: first, to learn the `repo` name that every other tool takes.",
        annotations(
            title = "List registered repositories",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_repo_list(&self) -> CallToolResult {
        blocking(|| match registry::load(&paths::registry_path()) {
            Ok(reg) => doc(&registry::list_json(&reg)),
            Err(e) => tool_error("store_error", &e),
        })
        .await
    }

    #[tool(
        name = "riltra_repo_status",
        description = "Repository status (same document as CLI `repo status`) plus a read-only `index_status` member (same document as CLI `index status`). When to call: before relying on other answers, to see whether the index exists and still matches the source.",
        annotations(
            title = "Repository status",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_repo_status(
        &self,
        Parameters(input): Parameters<RepoStatusInput>,
    ) -> CallToolResult {
        blocking(move || repo_status_doc(&input)).await
    }

    #[tool(
        name = "riltra_code_search",
        description = "Search indexed symbols by name (same document as CLI `search`). A missing index reports `no-index` without creating storage. When to call: to find where a symbol is defined and to get the `path`, `line` and `col` that the graph tools take.",
        annotations(
            title = "Search indexed symbols",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_code_search(
        &self,
        Parameters(input): Parameters<CodeSearchInput>,
    ) -> CallToolResult {
        blocking(move || code_search_doc(&input)).await
    }

    #[tool(
        name = "riltra_code_callers",
        description = "Call sites that reference the anchor symbol (same document as CLI `callers`); heuristic edges stay `POTENTIAL_CALL`, never promoted to `CALLS`. A name may be given instead of a position (`target`, optionally `in` and `kind`). Each POTENTIAL_CALL edge also carries `candidates` and `candidate_scope`: a name-matched definition count and its place relative to the caller, never a resolution. When to call: before changing a function or method, to see what appears to call it.",
        annotations(
            title = "Find callers",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_code_callers(
        &self,
        Parameters(input): Parameters<EdgeInput>,
    ) -> CallToolResult {
        blocking(move || edge_doc(&input, GraphMode::Callers)).await
    }

    #[tool(
        name = "riltra_code_callees",
        description = "Symbols the anchor definition calls (same document as CLI `callees`). A name may be given instead of a position (`target`, optionally `in` and `kind`). Each POTENTIAL_CALL edge also carries `candidates` and `candidate_scope`: a name-matched definition count and its place relative to the caller, never a resolution. When to call: to see what a function or method appears to depend on.",
        annotations(
            title = "Find callees",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_code_callees(
        &self,
        Parameters(input): Parameters<EdgeInput>,
    ) -> CallToolResult {
        blocking(move || edge_doc(&input, GraphMode::Callees)).await
    }

    #[tool(
        name = "riltra_code_trace",
        description = "Transitive call paths from the anchor, optionally to a destination (same document as CLI `trace`); paths are plausible traversal, not execution. A name may be given instead of a position (`target`, optionally `in` and `kind`). Each POTENTIAL_CALL edge also carries `candidates` and `candidate_scope`: a name-matched definition count and its place relative to the caller, never a resolution. When to call: to check whether and how one symbol might reach another.",
        annotations(
            title = "Trace call paths",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_code_trace(&self, Parameters(input): Parameters<TraceInput>) -> CallToolResult {
        blocking(move || trace_doc(&input)).await
    }

    #[tool(
        name = "riltra_code_impact",
        description = "Reverse transitive callers of the anchor with a coverage verdict (same document as CLI `impact`); impact is plausible, not execution. A name may be given instead of a position (`target`, optionally `in` and `kind`). Each POTENTIAL_CALL edge also carries `candidates` and `candidate_scope`: a name-matched definition count and its place relative to the caller, never a resolution. When to call: before changing a symbol, to see what it might affect; `UNKNOWN` or `partial` means not established, not safe. Name-matched callers are listed as `candidate-name` paths and never followed further. `summary` adds counts over this traversal (a small `limit` caps candidates per node and can lower them); `include_tests` needs `summary`.",
        annotations(
            title = "Estimate impact",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_code_impact(
        &self,
        Parameters(input): Parameters<ImpactInput>,
    ) -> CallToolResult {
        blocking(move || impact_doc(&input)).await
    }

    #[tool(
        name = "riltra_context",
        description = "Compose bounded What, Impact and Why context from one pinned code-index snapshot and a separate knowledge generation. Explicit code bindings remain distinct from lexical and optional semantic relevance candidates; no cross-store transaction or causal link is implied. A name may be given instead of a position (`target`, optionally `in` and `kind`). Each POTENTIAL_CALL edge also carries `candidates` and `candidate_scope`: a name-matched definition count and its place relative to the caller, never a resolution. When to call: to inspect code impact and its recorded rationale together.",
        annotations(
            title = "Read graph context",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_context(&self, Parameters(input): Parameters<ContextInput>) -> CallToolResult {
        blocking(move || context_doc(&input)).await
    }

    #[tool(
        name = "riltra_knowledge_search",
        description = "Search knowledge records by literal text and/or an exact stored evidence path (same document as CLI `knowledge search`). Set semantic=true to use the local vector index; it requires provisioned assets at RILTRA_MODEL_DIR and falls back to FTS5 with an explicit reason when unavailable. A missing store answers with an empty result and creates nothing. When to call: to look for recorded decisions or notes by text or by an evidence path.",
        annotations(
            title = "Search knowledge records",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_knowledge_search(
        &self,
        Parameters(input): Parameters<KnowledgeSearchInput>,
    ) -> CallToolResult {
        blocking(move || knowledge_search_doc(&input)).await
    }

    #[tool(
        name = "riltra_knowledge_show",
        description = "Read one knowledge revision with its evidence freshness (same document as CLI `knowledge show`). Confirmation is reported as stored provenance, never as promotion. When to call: to read a record that a search, a relation or an anchor lookup returned.",
        annotations(
            title = "Read a knowledge revision",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_knowledge_show(
        &self,
        Parameters(input): Parameters<KnowledgeShowInput>,
    ) -> CallToolResult {
        blocking(move || knowledge_show_doc(&input)).await
    }

    #[tool(
        name = "riltra_knowledge_related",
        description = "One-hop knowledge relations in either direction (same document as CLI `knowledge related`); retracted relations are hidden unless requested. When to call: to follow the links from a record to the records it relates to.",
        annotations(
            title = "Read knowledge relations",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_knowledge_related(
        &self,
        Parameters(input): Parameters<KnowledgeRelatedInput>,
    ) -> CallToolResult {
        blocking(move || knowledge_related_doc(&input)).await
    }

    #[tool(
        name = "riltra_knowledge_at_anchor",
        description = "Read explicit code-anchor bindings to exact knowledge revisions (same document as CLI `knowledge at`). This operation is read-only and never creates or migrates a store; unlinked anchors remain unlinked. When to call: to check whether a decision is recorded for the code at a position.",
        annotations(
            title = "Read knowledge at a code anchor",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_knowledge_at_anchor(
        &self,
        Parameters(input): Parameters<KnowledgeAtInput>,
    ) -> CallToolResult {
        blocking(move || knowledge_at_doc(&input)).await
    }

    #[tool(
        name = "riltra_code_semantic",
        description = "Definition or references of the symbol at a position, answered by a language server (the same document as CLI `semantic query --lsp-server`). References are locations, not call edges: they are never edges, and they are never a completeness claim — the provider's project index and compilation database may be incomplete. An empty answer means the provider found nothing at this position; it is not a claim that the symbol does not exist. A path that does not exist is reported as a path-containment refusal (`document path escapes repository`), never as an empty answer. The answer carries the provider's verification state and its diagnostics. The language server is a local process that the operator configured; it reads the project and may write its own index cache, and what it runs is provider-dependent — some servers run the project's build tools — so this tool is not read-only. When to call: only when an operator has set `RILTRA_LSP_SERVER` to a language server, to get a definition or references from it.",
        annotations(
            title = "Semantic definition or references",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn riltra_code_semantic(
        &self,
        Parameters(input): Parameters<SemanticInput>,
    ) -> CallToolResult {
        blocking(move || semantic_doc(&input)).await
    }

    #[tool(
        name = "riltra_health",
        description = "Environment health: bootstrap status, environment doctor, versions, capabilities, limits, and the registered repository count. Read-only; creates and opens no storage. When to call: to check that Riltra is reachable and which versions and limits apply.",
        annotations(
            title = "Health",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn riltra_health(&self, Parameters(_): Parameters<HealthInput>) -> CallToolResult {
        blocking(health_doc).await
    }
}

#[tool_handler]
impl ServerHandler for RiltraMcp {
    fn get_info(&self) -> ServerConfig {
        // Override the SDK default (`Implementation::from_build_env` reads
        // rmcp's own crate name) with this binary's identity.
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(crate::NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        let mut result = ListToolsResult::with_all_items(sorted_tool_definitions());
        if supports_cache_hints {
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Public);
        }
        Ok(result)
    }
}

/// The optional semantic graph enables serde_json's `preserve_order`, while
/// the core build's default map sorts Schemars properties alphabetically.
/// Normalize advertised schemas for that graph; the core build already keeps
/// the same order without cloning them.
fn sorted_tool_definitions() -> Vec<Tool> {
    let tools = RiltraMcp::tool_router().list_all();
    #[cfg(feature = "semantic-knowledge")]
    let tools = {
        let mut tools = tools;
        for tool in &mut tools {
            tool.input_schema = std::sync::Arc::new(sorted_schema(tool.input_schema.as_ref()));
            tool.output_schema = tool
                .output_schema
                .as_ref()
                .map(|schema| std::sync::Arc::new(sorted_schema(schema.as_ref())));
        }
        tools
    };
    tools
}

#[cfg(feature = "semantic-knowledge")]
fn sorted_schema(schema: &rmcp::model::JsonObject) -> rmcp::model::JsonObject {
    let mut value = serde_json::Value::Object(schema.clone());
    json::sort_value_keys(&mut value);
    match value {
        serde_json::Value::Object(schema) => schema,
        _ => unreachable!("JSON schema root is an object"),
    }
}

/// Run synchronous domain work on the blocking pool. Where panics unwind
/// (debug and test builds), a panicked task is a classified `store_error`. The
/// release profile sets `panic = "abort"`, so in a release binary a panic ends
/// the process before any join error exists.
async fn blocking(f: impl FnOnce() -> CallToolResult + Send + 'static) -> CallToolResult {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => tool_error("store_error", &format!("internal task failure: {e}")),
    }
}

/// A success result: the shared formatter's bytes, parsed back to JSON.
fn doc(json_text: &str) -> CallToolResult {
    match serde_json::from_str(json_text) {
        Ok(value) => structured_doc(value),
        Err(e) => tool_error(
            "store_error",
            &format!("internal response assembly error: {e}"),
        ),
    }
}

fn structured_doc(value: serde_json::Value) -> CallToolResult {
    #[cfg(feature = "semantic-knowledge")]
    let value = {
        let mut value = value;
        json::sort_value_keys(&mut value);
        value
    };
    CallToolResult::structured(value)
}

/// A classified refusal: `{"kind": ..., "message": ...}` with CLI message text.
fn tool_error(kind: &str, message: &str) -> CallToolResult {
    let value = serde_json::json!({"kind": kind, "message": message});
    #[cfg(feature = "semantic-knowledge")]
    let value = {
        let mut value = value;
        json::sort_value_keys(&mut value);
        value
    };
    CallToolResult::structured_error(value)
}

fn repo_status_doc(input: &RepoStatusInput) -> CallToolResult {
    let home = paths::config_dir();
    let reg = match registry::load(&paths::registry_path()) {
        Ok(reg) => reg,
        Err(e) => return tool_error("store_error", &e),
    };
    let entry = match registry::find(&reg, &input.repo) {
        Some(entry) => entry,
        None => {
            return tool_error(
                "not_registered",
                &format!("unknown repository: '{}'", input.repo),
            )
        }
    };
    let recorded = entry.root.clone();
    let canon = match paths::canonical_root(Path::new(&recorded)) {
        Ok(canon) => canon,
        Err(_) => {
            return tool_error(
                "not_registered",
                &format!("registered root is missing: '{recorded}'"),
            )
        }
    };
    let root = canon.to_string_lossy().to_string();
    // The record must still resolve to itself: a moved root or a
    // symlink/junction retarget is rejected instead of fingerprinted.
    if root != recorded {
        return tool_error(
            "not_registered",
            &format!("registered root moved or retargeted (record '{recorded}')"),
        );
    }
    let info = git::fingerprint(&canon);
    let (_, ignore) = ignore::load_rules(&canon);
    let index_status = match index_status_doc(&home, &input.repo, &recorded) {
        Ok(text) => text,
        Err((kind, message)) => return tool_error(kind, &message),
    };
    let mut value: serde_json::Value = match serde_json::from_str(&response::repo_status_json(
        &input.repo,
        &root,
        &info,
        &ignore,
    )) {
        Ok(value) => value,
        Err(e) => {
            return tool_error(
                "store_error",
                &format!("internal response assembly error: {e}"),
            )
        }
    };
    match serde_json::from_str(&index_status) {
        Ok(index_value) => value["index_status"] = index_value,
        Err(e) => {
            return tool_error(
                "store_error",
                &format!("internal response assembly error: {e}"),
            )
        }
    }
    structured_doc(value)
}

/// One migration pointer for outdated schemas: actionable CLI instruction,
/// identical for both index-reading tools.
fn outdated_message(repo: &str, version: u32) -> String {
    format!(
        "index schema v{version} is outdated; run `riltra index status --repo {repo}` with the CLI to migrate, then retry"
    )
}

/// `index status` through the read-only path; `Err` carries the classified
/// refusal so every index-reading tool reports the same guard.
fn index_status_doc(
    home: &Path,
    repo: &str,
    recorded: &str,
) -> Result<String, (&'static str, String)> {
    match store::open_read(&store::db_path(home, repo)) {
        store::IndexRead::Missing => Ok(response::index_status_json(
            repo,
            recorded,
            &index::Freshness {
                status: "unknown".to_string(),
                reasons: vec!["no-index".to_string()],
                snapshot: None,
            },
            false,
        )),
        store::IndexRead::Outdated(version) => {
            Err(("schema_outdated", outdated_message(repo, version)))
        }
        store::IndexRead::Ahead(version) => Err((
            "schema_ahead",
            format!(
                "database schema v{version} is newer than supported v{}",
                store::SCHEMA_VERSION
            ),
        )),
        store::IndexRead::Error(message) => Err(("store_error", message)),
        store::IndexRead::Ready(mut conn) => {
            // The FTS flag reflects build capability, not revision content.
            let fts = store::fts_available(&conn);
            let tx = match conn.transaction() {
                Ok(tx) => tx,
                Err(e) => return Err(("store_error", e.to_string())),
            };
            let freshness = index::evaluate_tx(&tx, Path::new(recorded));
            let _ = tx.commit();
            Ok(response::index_status_json(repo, recorded, &freshness, fts))
        }
    }
}

fn code_search_doc(input: &CodeSearchInput) -> CallToolResult {
    let limit = input.limit.unwrap_or(index::DEFAULT_SEARCH_LIMIT);
    let offset = input.offset.unwrap_or(0);
    if limit > index::MAX_SEARCH_LIMIT {
        return tool_error(
            "invalid_input",
            &format!("--limit exceeds maximum {}", index::MAX_SEARCH_LIMIT),
        );
    }
    let home = paths::config_dir();
    let reg = match registry::load(&paths::registry_path()) {
        Ok(reg) => reg,
        Err(e) => return tool_error("store_error", &e),
    };
    let entry = match registry::find(&reg, &input.repo) {
        Some(entry) => entry,
        None => {
            return tool_error(
                "not_registered",
                &format!("unknown repository: '{}'", input.repo),
            )
        }
    };
    let recorded = entry.root.clone();
    match store::open_read(&store::db_path(&home, &input.repo)) {
        store::IndexRead::Missing => search_doc(
            &input.repo,
            &recorded,
            None,
            "unknown",
            &["no-index".to_string()],
            input,
            limit,
            offset,
            0,
            &[],
            None,
        ),
        store::IndexRead::Outdated(version) => {
            tool_error("schema_outdated", &outdated_message(&input.repo, version))
        }
        store::IndexRead::Ahead(version) => tool_error(
            "schema_ahead",
            &format!(
                "database schema v{version} is newer than supported v{}",
                store::SCHEMA_VERSION
            ),
        ),
        store::IndexRead::Error(message) => tool_error("store_error", &message),
        store::IndexRead::Ready(mut conn) => {
            // One read transaction across snapshot read, verdict, and fact
            // rows: the response can never mix two revisions.
            let tx = match conn.transaction() {
                Ok(tx) => tx,
                Err(e) => return tool_error("store_error", &e.to_string()),
            };
            let snapshot = match store::latest_snapshot(&tx) {
                Ok(Some(snapshot)) => snapshot,
                _ => {
                    return tool_error(
                        "store_error",
                        "index database unreadable; rebuild with `index`",
                    )
                }
            };
            let canon_root = match Path::new(&recorded).canonicalize() {
                Ok(canon) => canon,
                Err(_) => {
                    let _ = tx.commit();
                    return search_doc(
                        &input.repo,
                        &recorded,
                        Some(&snapshot),
                        "unknown",
                        &["root-unreadable".to_string()],
                        input,
                        limit,
                        offset,
                        0,
                        &[],
                        None,
                    );
                }
            };
            let (status, reasons) =
                index::evaluate_snapshot(&tx, &input.repo, &canon_root, &snapshot);
            let (total, hits) =
                match store::search_symbols(&tx, snapshot.id, &input.query, limit, offset) {
                    Ok(found) => found,
                    Err(e) => return tool_error("store_error", &e),
                };
            let census = if input.occurrences == Some(true) {
                match store::occurrences_for(&tx, snapshot.id, &input.query, limit, offset) {
                    Ok(list) => Some(list),
                    Err(e) => return tool_error("store_error", &e),
                }
            } else {
                None
            };
            let _ = tx.commit();
            search_doc(
                &input.repo,
                &snapshot.root,
                Some(&snapshot),
                &status,
                &reasons,
                input,
                limit,
                offset,
                total,
                &hits,
                census.as_ref(),
            )
        }
    }
}

/// One search document through the shared CP2 formatter.
#[allow(clippy::too_many_arguments)]
fn search_doc(
    repo: &str,
    root: &str,
    snapshot: Option<&store::Snapshot>,
    freshness: &str,
    reasons: &[String],
    input: &CodeSearchInput,
    limit: usize,
    offset: usize,
    total: usize,
    hits: &[store::Hit],
    occurrences: Option<&store::OccurrenceList>,
) -> CallToolResult {
    doc(&response::search_response_json(&response::SearchResponse {
        repo,
        root,
        snapshot,
        freshness,
        reasons,
        symbol: &input.query,
        limit,
        offset,
        total,
        hits,
        occurrences,
    }))
}

// --- CP4: graph tools --------------------------------------------------------

/// CLI-parity anchor check (`cmd_edge`/`cmd_graph` exit-2 texts).
// LanceDB's `serde_json/preserve_order` feature enlarges RMCP's upstream
// `CallToolResult`; keep this allowance limited to helpers that return it.
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn graph_anchor(line: u32, col: u32) -> Result<(), CallToolResult> {
    if line == 0 {
        return Err(tool_error(
            "invalid_input",
            &format!("invalid --line value: '{line}'"),
        ));
    }
    if col == 0 {
        return Err(tool_error(
            "invalid_input",
            &format!("invalid --col value: '{col}'"),
        ));
    }
    Ok(())
}

/// CLI-parity page-range check (`--limit` default 50/max 1000, `--offset`).
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn graph_page(
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<(usize, usize), CallToolResult> {
    let limit = limit.unwrap_or(index::DEFAULT_SEARCH_LIMIT);
    if !(1..=index::MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(tool_error(
            "invalid_input",
            &format!("--limit must be between 1 and {}", index::MAX_SEARCH_LIMIT),
        ));
    }
    let offset = offset.unwrap_or(0);
    if offset > i64::MAX as usize - index::MAX_SEARCH_LIMIT {
        return Err(tool_error(
            "invalid_input",
            "--offset exceeds supported range",
        ));
    }
    Ok((limit, offset))
}

/// CLI-parity depth check (`--depth` default 3, 1-6).
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn graph_depth(depth: Option<usize>) -> Result<usize, CallToolResult> {
    let depth = depth.unwrap_or(3);
    if !(1..=crate::edges::TRACE_MAX_DEPTH).contains(&depth) {
        return Err(tool_error(
            "invalid_input",
            &format!(
                "--depth must be between 1 and {}",
                crate::edges::TRACE_MAX_DEPTH
            ),
        ));
    }
    Ok(depth)
}

/// Same parse as the CLI's `trace_position`: a positive 1-based number or the
/// flag's refusal text.
fn positional(value: &str, flag: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("invalid {flag} value: '{value}'"))
}

fn bad_destination() -> CallToolResult {
    tool_error("invalid_input", "--to must be PATH:LINE:COL")
}

/// The CLI `--to` grammar: `PATH:LINE:COL`, all positive 1-based numbers.
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn parse_destination(to: &str) -> Result<(&str, u32, u32), CallToolResult> {
    let (rest, col) = to.rsplit_once(':').ok_or_else(bad_destination)?;
    let (path, line) = rest.rsplit_once(':').ok_or_else(bad_destination)?;
    if path.is_empty() {
        return Err(tool_error("invalid_input", "--to requires a path"));
    }
    let line = positional(line, "--to line").map_err(|e| tool_error("invalid_input", &e))?;
    let col = positional(col, "--to col").map_err(|e| tool_error("invalid_input", &e))?;
    Ok((path, line, col))
}

/// A graph or context call's anchor: a validated position, or a name that
/// resolves once the repository is known.
enum Anchor<'a> {
    At { path: &'a str, line: u32, col: u32 },
    Name(index::TargetQuery<'a>),
}

/// The `path/line/col` form or the `target` form of one call, never both.
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn anchor_of<'a>(
    path: Option<&'a str>,
    line: Option<u32>,
    col: Option<u32>,
    target: Option<&'a str>,
    in_path: Option<&'a str>,
    kind: Option<&'a str>,
) -> Result<Anchor<'a>, CallToolResult> {
    match (target, path, line, col) {
        (Some(target), None, None, None)
            if !target.trim().is_empty()
                && in_path.is_none_or(|p| !p.trim().is_empty())
                && kind.is_none_or(|k| !k.trim().is_empty()) =>
        {
            Ok(Anchor::Name(index::TargetQuery {
                target,
                in_path,
                kind,
            }))
        }
        (None, Some(path), Some(line), Some(col)) if in_path.is_none() && kind.is_none() => {
            graph_anchor(line, col)?;
            Ok(Anchor::At { path, line, col })
        }
        _ => Err(tool_error(
            "invalid_input",
            "give either path, line and col, or a non-empty target (with optional in and kind)",
        )),
    }
}

/// Resolve a `target` name on the stored snapshot with the CLI's store-state
/// classes. Read-only: a missing index answers the `no-index` document.
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn resolve_name(
    home: &Path,
    repo: &str,
    query: &index::TargetQuery,
) -> Result<index::TargetResolution, CallToolResult> {
    match store::open_read(&store::db_path(home, repo)) {
        store::IndexRead::Missing => Ok(index::TargetResolution::Doc(
            index::no_index_target_document(repo, query),
        )),
        store::IndexRead::Outdated(version) => Err(tool_error(
            "schema_outdated",
            &outdated_message(repo, version),
        )),
        store::IndexRead::Ahead(version) => Err(tool_error(
            "schema_ahead",
            &format!(
                "database schema v{version} is newer than supported v{}",
                store::SCHEMA_VERSION
            ),
        )),
        store::IndexRead::Error(message) => Err(tool_error("store_error", &message)),
        store::IndexRead::Ready(conn) => {
            index::resolve_target(&conn, repo, query).map_err(|e| tool_error("store_error", &e))
        }
    }
}

/// Missing index or snapshot: the CLI's no-snapshot text, classified
/// `no_index` (the read path never creates or migrates storage).
fn missing_index() -> CallToolResult {
    tool_error("no_index", "no snapshot: index the repository first")
}

/// Registry + recorded-root resolution for the graph and knowledge tools: the
/// same checks and messages as `repo_status` and the CLI's root resolution.
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn repo_root(repo: &str) -> Result<(PathBuf, PathBuf), CallToolResult> {
    let home = paths::config_dir();
    let reg = match registry::load(&paths::registry_path()) {
        Ok(reg) => reg,
        Err(e) => return Err(tool_error("store_error", &e)),
    };
    let entry = match registry::find(&reg, repo) {
        Some(entry) => entry,
        None => {
            return Err(tool_error(
                "not_registered",
                &format!("unknown repository: '{repo}'"),
            ))
        }
    };
    let recorded = entry.root.clone();
    let canon = match paths::canonical_root(Path::new(&recorded)) {
        Ok(canon) => canon,
        Err(_) => {
            return Err(tool_error(
                "not_registered",
                &format!("registered root is missing: '{recorded}'"),
            ))
        }
    };
    if canon.to_string_lossy() != recorded {
        return Err(tool_error(
            "not_registered",
            &format!("registered root moved or retargeted (record '{recorded}')"),
        ));
    }
    Ok((home, canon))
}

/// Classify a graph-query refusal by its CLI text (design gate §3): missing
/// snapshot -> `no_index`; anchor/identity/path refusals -> `invalid_input`;
/// everything else is `store_error` (fail-closed), message preserved.
fn graph_query_error(message: String) -> CallToolResult {
    let kind = if message.starts_with("no snapshot:") {
        "no_index"
    } else if message.starts_with("anchor")
        || message.starts_with("occurrence has no symbol identity")
        || message.starts_with("source has no identity")
        || message.starts_with("no anchor here")
        || message.contains("document path")
        || message.starts_with("invalid edge query bounds")
        || message.starts_with("invalid trace bounds")
    {
        "invalid_input"
    } else {
        "store_error"
    };
    tool_error(kind, &message)
}

/// Which graph document a call produces.
#[derive(Clone, Copy)]
enum GraphMode {
    Callers,
    Callees,
    Trace,
    Impact { summary: bool, include_tests: bool },
}

/// A validated request plus the paths the connection query needs; validation
/// happens before the read-only open, exactly like the CLI validators.
enum Prepared<'a> {
    Edge(index::EdgeRequest<'a>, PathBuf, String),
    Graph(index::TraceRequest<'a>, PathBuf, String, Option<String>),
}

#[allow(clippy::too_many_arguments)]
fn graph_doc(
    repo: &str,
    anchor: Anchor,
    symbol: Option<&str>,
    to: Option<&str>,
    depth: Option<usize>,
    limit: Option<usize>,
    offset: Option<usize>,
    mode: GraphMode,
) -> CallToolResult {
    let is_graph = matches!(mode, GraphMode::Trace | GraphMode::Impact { .. });
    let depth = if is_graph {
        match graph_depth(depth) {
            Ok(d) => d,
            Err(e) => return e,
        }
    } else {
        3
    };
    let (limit, offset) = match graph_page(limit, offset) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let destination = match to.map(parse_destination).transpose() {
        Ok(d) => d,
        Err(e) => return e,
    };
    let (home, canon) = match repo_root(repo) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // A name answers before the query runs, so the destination is checked here
    // exactly as the position form checks it (same refusal and class).
    if let (Anchor::Name(_), Some((dest, _, _))) = (&anchor, destination) {
        if let Err(e) = crate::semantic::validate_doc_path(&canon, dest) {
            return tool_error("invalid_input", &e);
        }
    }
    let (path, line, col) = match anchor {
        Anchor::At { path, line, col } => (path.to_string(), line, col),
        Anchor::Name(query) => match resolve_name(&home, repo, &query) {
            Ok(index::TargetResolution::One { path, line, col }) => (path, line, col),
            Ok(index::TargetResolution::Doc(answer)) => return doc(&answer),
            Err(e) => return e,
        },
    };
    let prepared = match mode {
        GraphMode::Callers | GraphMode::Callees => {
            let request = index::EdgeRequest {
                anchor: index::SpanQuery {
                    rel: &path,
                    line1: line,
                    col1: col,
                    refs: false,
                },
                symbol,
                direction: if matches!(mode, GraphMode::Callers) {
                    index::EdgeDir::Callers
                } else {
                    index::EdgeDir::Callees
                },
                limit,
                offset,
            };
            match index::validated_edge_anchor(&canon, &request) {
                Ok((canon, validated)) => Prepared::Edge(request, canon, validated),
                Err(e) => return tool_error("invalid_input", &e),
            }
        }
        GraphMode::Trace | GraphMode::Impact { .. } => {
            let request = index::TraceRequest {
                summary: matches!(mode, GraphMode::Impact { summary: true, .. }),
                include_tests: matches!(
                    mode,
                    GraphMode::Impact {
                        include_tests: true,
                        ..
                    }
                ),
                source: index::SpanQuery {
                    rel: &path,
                    line1: line,
                    col1: col,
                    refs: false,
                },
                symbol,
                destination: destination.map(|(p, l, c)| index::SpanQuery {
                    rel: p,
                    line1: l,
                    col1: c,
                    refs: false,
                }),
                depth,
                limit,
                offset,
            };
            match index::validated_trace_anchor(&canon, &request) {
                Ok((canon, rel, dest_path)) => Prepared::Graph(request, canon, rel, dest_path),
                Err(e) => return tool_error("invalid_input", &e),
            }
        }
    };
    match store::open_read(&store::db_path(&home, repo)) {
        store::IndexRead::Missing => missing_index(),
        store::IndexRead::Outdated(version) => {
            tool_error("schema_outdated", &outdated_message(repo, version))
        }
        store::IndexRead::Ahead(version) => tool_error(
            "schema_ahead",
            &format!(
                "database schema v{version} is newer than supported v{}",
                store::SCHEMA_VERSION
            ),
        ),
        store::IndexRead::Error(message) => tool_error("store_error", &message),
        store::IndexRead::Ready(mut conn) => match prepared {
            Prepared::Edge(request, canon, validated) => {
                match index::edge_query_conn(&mut conn, repo, &canon, &validated, &request, || {}) {
                    Ok(out) => doc(&response::edge_query_json(&out)),
                    Err(e) => graph_query_error(e),
                }
            }
            Prepared::Graph(request, canon, rel, dest_path) => {
                let started = std::time::Instant::now();
                match index::graph_query_conn(
                    &mut conn,
                    repo,
                    &canon,
                    &rel,
                    dest_path.as_deref(),
                    &request,
                    started,
                    || {},
                    matches!(mode, GraphMode::Impact { .. }),
                    matches!(mode, GraphMode::Impact { .. }),
                ) {
                    Ok(out) => doc(&response::trace_query_json(&out)),
                    Err(e) => graph_query_error(e),
                }
            }
        },
    }
}

fn edge_doc(input: &EdgeInput, mode: GraphMode) -> CallToolResult {
    let anchor = match anchor_of(
        input.path.as_deref(),
        input.line,
        input.col,
        input.target.as_deref(),
        input.in_path.as_deref(),
        input.kind.as_deref(),
    ) {
        Ok(anchor) => anchor,
        Err(e) => return e,
    };
    graph_doc(
        &input.repo,
        anchor,
        input.symbol.as_deref(),
        None,
        None,
        input.limit,
        input.offset,
        mode,
    )
}

fn trace_doc(input: &TraceInput) -> CallToolResult {
    let anchor = match anchor_of(
        input.path.as_deref(),
        input.line,
        input.col,
        input.target.as_deref(),
        input.in_path.as_deref(),
        input.kind.as_deref(),
    ) {
        Ok(anchor) => anchor,
        Err(e) => return e,
    };
    graph_doc(
        &input.repo,
        anchor,
        input.symbol.as_deref(),
        input.to.as_deref(),
        input.depth,
        input.limit,
        input.offset,
        GraphMode::Trace,
    )
}

fn impact_doc(input: &ImpactInput) -> CallToolResult {
    if input.include_tests && !input.summary {
        return tool_error("invalid_input", "include_tests needs summary");
    }
    let anchor = match anchor_of(
        input.path.as_deref(),
        input.line,
        input.col,
        input.target.as_deref(),
        input.in_path.as_deref(),
        input.kind.as_deref(),
    ) {
        Ok(anchor) => anchor,
        Err(e) => return e,
    };
    graph_doc(
        &input.repo,
        anchor,
        input.symbol.as_deref(),
        None,
        input.depth,
        input.limit,
        input.offset,
        GraphMode::Impact {
            summary: input.summary,
            include_tests: input.include_tests,
        },
    )
}

fn context_doc(input: &ContextInput) -> CallToolResult {
    let anchor = match anchor_of(
        input.path.as_deref(),
        input.line,
        input.col,
        input.target.as_deref(),
        input.in_path.as_deref(),
        input.kind.as_deref(),
    ) {
        Ok(anchor) => anchor,
        Err(e) => return e,
    };
    let depth = match graph_depth(input.depth) {
        Ok(depth) => depth,
        Err(e) => return e,
    };
    let limit = input.limit.unwrap_or(context::DEFAULT_LIMIT);
    if !(1..=context::MAX_LIMIT).contains(&limit) {
        return tool_error(
            "invalid_input",
            &format!("--limit must be between 1 and {}", context::MAX_LIMIT),
        );
    }
    let max_bytes = input.max_bytes.unwrap_or(context::DEFAULT_MAX_BYTES);
    if !(context::MIN_MAX_BYTES..=context::MAX_MAX_BYTES).contains(&max_bytes) {
        return tool_error(
            "invalid_input",
            &format!(
                "--max-bytes must be between {} and {}",
                context::MIN_MAX_BYTES,
                context::MAX_MAX_BYTES
            ),
        );
    }
    let (home, canon) = match repo_root(&input.repo) {
        Ok(value) => value,
        Err(e) => return e,
    };
    if let Err(e) = context::validate_options(
        depth,
        limit,
        max_bytes,
        input.query.as_deref(),
        input.semantic.unwrap_or(false),
    ) {
        return context_query_error(e);
    }
    let (path, line, col) = match anchor {
        Anchor::At { path, line, col } => (path.to_string(), line, col),
        Anchor::Name(query) => match resolve_name(&home, &input.repo, &query) {
            Ok(index::TargetResolution::One { path, line, col }) => (path, line, col),
            Ok(index::TargetResolution::Doc(answer)) => return doc(&answer),
            Err(e) => return e,
        },
    };
    let entry = registry::RepoEntry {
        name: input.repo.clone(),
        root: canon.to_string_lossy().into_owned(),
        added_unix: 0,
    };
    let request = context::ContextRequest {
        path: &path,
        line,
        col,
        symbol: input.symbol.as_deref(),
        query: input.query.as_deref(),
        semantic: input.semantic.unwrap_or(false),
        depth,
        limit,
        max_bytes,
    };
    if let Err(e) = context::validate_request(&canon, &request) {
        return context_query_error(e);
    }
    match store::open_read(&store::db_path(&home, &input.repo)) {
        store::IndexRead::Missing => missing_index(),
        store::IndexRead::Outdated(version) => {
            tool_error("schema_outdated", &outdated_message(&input.repo, version))
        }
        store::IndexRead::Ahead(version) => tool_error(
            "schema_ahead",
            &format!(
                "database schema v{version} is newer than supported v{}",
                store::SCHEMA_VERSION
            ),
        ),
        store::IndexRead::Error(message) => tool_error("store_error", &message),
        store::IndexRead::Ready(mut conn) => {
            let result = match context::query_conn(
                &mut conn,
                &home,
                &entry,
                &input.repo,
                &canon,
                &request,
            ) {
                Ok(result) => result,
                Err(e) => return context_query_error(e),
            };
            match response::context_query_json(&result) {
                Ok(value) => doc(&value),
                Err(e) => context_query_error(e),
            }
        }
    }
}

fn context_query_error(message: String) -> CallToolResult {
    if message.starts_with("invalid context bounds")
        || message.starts_with("context query")
        || message.starts_with("context semantic")
        || message.starts_with("context response envelope")
        || message.starts_with("context response metadata")
    {
        tool_error("invalid_input", &message)
    } else {
        graph_query_error(message)
    }
}

// --- semantic tool ------------------------------------------------------------

/// The language server executable: the operator's configuration, never the
/// request's. An absolute path only — no PATH lookup, no auto-install, no
/// client-supplied executable or argument.
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn provider_exe() -> Result<String, CallToolResult> {
    let exe = std::env::var("RILTRA_LSP_SERVER").unwrap_or_default();
    if exe.is_empty() || !Path::new(&exe).is_absolute() {
        return Err(tool_error(
            "provider_unavailable",
            "no language server configured; set RILTRA_LSP_SERVER to an absolute path",
        ));
    }
    Ok(exe)
}

/// Classify a semantic-query refusal by its CLI text: a document-path
/// problem → `invalid_input`; everything else fails closed as `store_error`,
/// message preserved.
fn semantic_query_error(message: String) -> CallToolResult {
    let kind = if message.contains("document path") {
        "invalid_input"
    } else {
        "store_error"
    };
    tool_error(kind, &message)
}

/// The semantic query: the same code path and the same document as the
/// CLI's `semantic query --lsp-server`. The checks run in a fixed order —
/// repository, position, document path, provider — so a bad path is
/// reported as such even when no provider is configured.
fn semantic_doc(input: &SemanticInput) -> CallToolResult {
    let (_home, canon) = match repo_root(&input.repo) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if let Err(e) = graph_anchor(input.line, input.col) {
        return e;
    }
    if let Err(e) = crate::semantic::validate_doc_path(&canon, &input.path) {
        return tool_error("invalid_input", &e);
    }
    let exe = match provider_exe() {
        Ok(exe) => exe,
        Err(e) => return e,
    };
    let span = index::SpanQuery {
        rel: &input.path,
        line1: input.line,
        col1: input.col,
        refs: matches!(input.operation, Operation::References),
    };
    match index::semantic_query_lsp(&canon, &span, &exe) {
        Ok(out) => doc(&response::semantic_query_json(&input.repo, &out)),
        Err(e) => semantic_query_error(e),
    }
}

// --- CP5: knowledge tools ----------------------------------------------------

/// CLI-parity page-range check for the knowledge tools (`--limit` default 50,
/// maximum 1000). The CLI has no extra exit-2 offset bound here, so the
/// store's own range check stays the guard.
#[cfg_attr(feature = "semantic-knowledge", allow(clippy::result_large_err))]
fn knowledge_page(
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<(usize, usize), CallToolResult> {
    let limit = limit.unwrap_or(knowledge::SEARCH_LIMIT_DEFAULT);
    if !(1..=knowledge::SEARCH_LIMIT_MAX).contains(&limit) {
        return Err(tool_error(
            "invalid_input",
            &format!(
                "--limit must be between 1 and {}",
                knowledge::SEARCH_LIMIT_MAX
            ),
        ));
    }
    Ok((limit, offset.unwrap_or(0)))
}

/// Classify a knowledge-query refusal by its CLI text (design gate §3): a
/// missing store is `store_missing`; the generation guard is `conflict`;
/// record/kind/query refusals are `invalid_input`; everything else fails
/// closed as `store_error`, message preserved.
fn knowledge_query_error(message: String) -> CallToolResult {
    let kind = if message.starts_with("no knowledge store") {
        "store_missing"
    } else if message.starts_with("knowledge generation conflict:") {
        "conflict"
    } else if message.starts_with("unknown record id:")
        || message.starts_with("unknown record revision:")
        || message.starts_with("record revision out of range:")
        || message.starts_with("invalid ")
        || message.starts_with("search query is ")
        || message.starts_with("search limit")
        || message.starts_with("related limit")
    {
        "invalid_input"
    } else {
        "store_error"
    };
    tool_error(kind, &message)
}

/// The CLI's `RepoEntry` for the knowledge store, built from the recorded
/// root exactly like `main.rs::knowledge_repo`.
fn knowledge_entry(repo: &str, root: &str) -> registry::RepoEntry {
    registry::RepoEntry {
        name: repo.to_string(),
        root: root.to_string(),
        added_unix: 0,
    }
}

fn knowledge_search_doc(input: &KnowledgeSearchInput) -> CallToolResult {
    // Validation order and texts mirror `main.rs::knowledge_search`.
    let semantic = input.semantic.unwrap_or(false);
    if semantic
        && input
            .query
            .as_deref()
            .is_none_or(|query| query.trim().is_empty())
    {
        return tool_error(
            "invalid_input",
            "semantic search requires a nonempty text query",
        );
    }
    if semantic
        && (input.include_archived.unwrap_or(false)
            || input.include_superseded.unwrap_or(false)
            || input.include_retracted.unwrap_or(false))
    {
        return tool_error(
            "invalid_input",
            "semantic search only supports active records",
        );
    }
    if input.query.is_none() && input.path.is_none() {
        return tool_error(
            "invalid_input",
            "search requires a text query or an evidence path",
        );
    }
    if let Some(path) = input.path.as_deref() {
        if let Err(e) = knowledge::validate_search_path(path) {
            return tool_error("invalid_input", &e);
        }
    }
    if let Some(kind) = input.kind.as_deref() {
        if let Err(e) = knowledge::validate_kind(kind) {
            return tool_error("invalid_input", &e);
        }
    }
    if let Some(query) = input.query.as_deref() {
        if query.len() > knowledge::MAX_QUERY_TEXT_BYTES {
            return tool_error(
                "invalid_input",
                &format!(
                    "search query is {} bytes, at most {} allowed",
                    query.len(),
                    knowledge::MAX_QUERY_TEXT_BYTES
                ),
            );
        }
    }
    // An empty or whitespace query beside a path is path-only: the result
    // echoes `query: null` instead of pretending text was searched.
    let query = input.query.as_deref().unwrap_or("");
    let echoed_query = if input.path.is_some() && query.trim().is_empty() {
        None
    } else {
        Some(query)
    };
    let (limit, offset) = match knowledge_page(input.limit, input.offset) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if semantic && offset != 0 {
        return tool_error(
            "invalid_input",
            "semantic search does not support a nonzero offset",
        );
    }
    let (home, canon) = match repo_root(&input.repo) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let root = canon.to_string_lossy().to_string();
    let db = knowledge_store::knowledge_db_path(&home, &input.repo);
    match knowledge_store_absent(&db) {
        // Genuinely absent store: empty success, and no database is created.
        // A nonzero generation expectation still conflicts against generation 0.
        Ok(true) => {
            if let Err(e) = knowledge_store::check_missing_store_generation(input.generation) {
                return knowledge_query_error(e);
            }
            let result = if semantic {
                response::knowledge_semantic_search_result_json(
                    echoed_query,
                    input.path.as_deref(),
                    input.kind.as_deref(),
                    &[],
                    &canon,
                    Some("knowledge-store-missing"),
                )
            } else {
                response::knowledge_search_result_json(
                    echoed_query,
                    input.path.as_deref(),
                    input.kind.as_deref(),
                    input.include_archived.unwrap_or(false),
                    &[],
                    &canon,
                )
            };
            return doc(&response::knowledge_envelope(
                &input.repo,
                0,
                "search",
                &result,
                Some(response::pagination_json(limit, offset, 0, false, None)),
            ));
        }
        Ok(false) => {}
        Err(e) => return tool_error("store_error", &e),
    }
    let entry = knowledge_entry(&input.repo, &root);
    let mut conn = match knowledge_store::open_knowledge(&entry, false) {
        Ok(conn) => conn,
        Err(e) => return knowledge_query_error(e),
    };
    let model_dir = std::env::var_os("RILTRA_MODEL_DIR").map(PathBuf::from);
    let search = match knowledge_search::search(
        &mut conn,
        &home,
        &entry,
        query,
        input.path.as_deref(),
        input.kind.as_deref(),
        input.include_archived.unwrap_or(false),
        input.include_superseded.unwrap_or(false),
        input.include_retracted.unwrap_or(false),
        limit,
        offset,
        input.generation,
        semantic,
        model_dir.as_deref(),
    ) {
        Ok(search) => search,
        Err(e) => return knowledge_query_error(e),
    };
    let (result, generation, pagination) = response::knowledge_search_output(
        search,
        echoed_query,
        input.path.as_deref(),
        input.kind.as_deref(),
        input.include_archived.unwrap_or(false),
        limit,
        offset,
        &canon,
    );
    doc(&response::knowledge_envelope(
        &input.repo,
        generation,
        "search",
        &result,
        Some(pagination),
    ))
}

fn knowledge_show_doc(input: &KnowledgeShowInput) -> CallToolResult {
    let (_home, canon) = match repo_root(&input.repo) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let entry = knowledge_entry(&input.repo, &canon.to_string_lossy());
    let conn = match knowledge_store::open_knowledge(&entry, false) {
        Ok(conn) => conn,
        Err(e) => return knowledge_query_error(e),
    };
    let (record, generation) =
        match knowledge_store::show_snapshot(&conn, &input.id, input.revision) {
            Ok(pair) => pair,
            Err(e) => return knowledge_query_error(e),
        };
    doc(&response::knowledge_envelope(
        &input.repo,
        generation,
        "show",
        &response::record_json(&record, &canon),
        None,
    ))
}

fn knowledge_related_doc(input: &KnowledgeRelatedInput) -> CallToolResult {
    let (limit, offset) = match knowledge_page(input.limit, input.offset) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let direction = input.direction.as_deref().unwrap_or("outbound");
    let dir = match direction {
        "outbound" => knowledge_store::RelationDirection::Outbound,
        "inbound" => knowledge_store::RelationDirection::Inbound,
        other => {
            return tool_error(
                "invalid_input",
                &format!("invalid --direction value: '{other}' (expected inbound or outbound)"),
            )
        }
    };
    let (_home, canon) = match repo_root(&input.repo) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let entry = knowledge_entry(&input.repo, &canon.to_string_lossy());
    let conn = match knowledge_store::open_knowledge(&entry, false) {
        Ok(conn) => conn,
        Err(e) => return knowledge_query_error(e),
    };
    let page = match knowledge_store::related(
        &conn,
        &input.id,
        input.revision,
        dir,
        input.include_retracted.unwrap_or(false),
        limit,
        offset,
        input.generation,
    ) {
        Ok(page) => page,
        Err(e) => return knowledge_query_error(e),
    };
    doc(&response::knowledge_envelope(
        &input.repo,
        page.generation,
        "related",
        &response::knowledge_related_result_json(
            &input.id,
            input.revision,
            direction,
            &page.relations,
        ),
        Some(response::pagination_json(
            limit,
            offset,
            page.total,
            page.truncated,
            page.next_offset,
        )),
    ))
}

fn knowledge_at_doc(input: &KnowledgeAtInput) -> CallToolResult {
    if input.line == 0 || input.col == 0 {
        return tool_error("invalid_input", "anchor line and column must be at least 1");
    }
    let (limit, offset) = match knowledge_page(input.limit, input.offset) {
        Ok(page) => page,
        Err(error) => return error,
    };
    let (home, canon) = match repo_root(&input.repo) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let entry = knowledge_entry(&input.repo, &canon.to_string_lossy());
    let path = match crate::semantic::validate_doc_path(&canon, &input.path) {
        Ok(path) => path,
        Err(error) => return tool_error("invalid_input", &error),
    };
    let db = knowledge_store::knowledge_db_path(&home, &input.repo);
    let page = match knowledge_store_absent(&db) {
        Ok(true) => {
            if let Err(error) = knowledge_store::check_missing_store_generation(input.generation) {
                return knowledge_query_error(error);
            }
            let mut page = knowledge_binding::empty_at(&path, input.line, input.col, 0, 0);
            page.limit = limit;
            page.offset = offset;
            page
        }
        Ok(false) => {
            let conn = match knowledge_store::open_knowledge(&entry, false) {
                Ok(conn) => conn,
                Err(error) => return knowledge_query_error(error),
            };
            match knowledge_binding::at(
                &conn,
                &home,
                &entry,
                &path,
                input.line,
                input.col,
                input.symbol.as_deref(),
                limit,
                offset,
                input.generation,
            ) {
                Ok(page) => page,
                Err(error) => return knowledge_query_error(error),
            }
        }
        Err(error) => return knowledge_query_error(error),
    };
    doc(&response::knowledge_envelope(
        &input.repo,
        page.generation,
        "at",
        &knowledge_binding::page_json(&page).to_string(),
        Some(response::pagination_json(
            page.limit,
            page.offset,
            page.total,
            page.truncated,
            page.next_offset,
        )),
    ))
}

// --- CP6: health -----------------------------------------------------------

/// The health document: composed from the CLI's own status and environment
/// doctor assemblies plus source-of-truth constants. The registry is only
/// read; a corrupt registry is classified `store_error` and never yields a
/// fabricated repository count.
fn health_doc() -> CallToolResult {
    let reg = match registry::load(&paths::registry_path()) {
        Ok(reg) => reg,
        Err(e) => return tool_error("store_error", &e),
    };
    let parse_json = |text: &str| -> Result<serde_json::Value, String> {
        serde_json::from_str(text).map_err(|e| format!("internal response assembly error: {e}"))
    };
    let status = match parse_json(&crate::status_json()) {
        Ok(value) => value,
        Err(e) => return tool_error("store_error", &e),
    };
    let (doctor_text, _) = doctor::environment_doctor();
    let doctor = match parse_json(&doctor_text) {
        Ok(value) => value,
        Err(e) => return tool_error("store_error", &e),
    };
    let value = serde_json::json!({
        "operation": "health",
        "status": status,
        "doctor": doctor,
        "versions": {
            "riltra": crate::VERSION,
            "code_index_schema": store::SCHEMA_VERSION,
            "knowledge_schema": knowledge_store::KNOWLEDGE_SCHEMA_VERSION,
        },
        // The capability flags describe the **potential** effects of the
        // whole server, not what every call does: `riltra_code_semantic`
        // starts a language server, which may write its own index cache and
        // may reach the network. Riltra itself still mutates nothing through
        // that path — the semantic query never opens the index or the
        // knowledge store (`index::semantic_query_lsp` takes no database
        // handle) — but the server as a whole is not read-only, may mutate
        // the machine (the provider's cache) and may use the network.
        "capabilities": {
            "transport": "stdio",
            "operations": ["tools"],
            "read_only": false,
            "mutations": true,
            "resources": false,
            "prompts": false,
            "network": true,
            "knowledge_schema_v1_readable": true,
            "tools": [
                "riltra_code_callees",
                "riltra_code_callers",
                "riltra_code_impact",
                "riltra_code_search",
                "riltra_code_semantic",
                "riltra_code_trace",
                "riltra_context",
                "riltra_health",
                "riltra_knowledge_at_anchor",
                "riltra_knowledge_related",
                "riltra_knowledge_search",
                "riltra_knowledge_show",
                "riltra_repo_list",
                "riltra_repo_status",
            ],
        },
        "limits": {
            "code_search": {
                "default_limit": index::DEFAULT_SEARCH_LIMIT,
                "max_limit": index::MAX_SEARCH_LIMIT,
            },
            "knowledge_search": {
                "default_limit": knowledge::SEARCH_LIMIT_DEFAULT,
                "max_limit": knowledge::SEARCH_LIMIT_MAX,
            },
            "graph_traversal": {
                "depth_default": 3,
                "depth_max": crate::edges::TRACE_MAX_DEPTH,
                "default_limit": index::DEFAULT_SEARCH_LIMIT,
                "max_limit": index::MAX_SEARCH_LIMIT,
            },
            "context": {
                "depth_default": crate::context::DEFAULT_DEPTH,
                "depth_max": crate::edges::TRACE_MAX_DEPTH,
                "default_limit": crate::context::DEFAULT_LIMIT,
                "max_limit": crate::context::MAX_LIMIT,
                "max_bytes_default": crate::context::DEFAULT_MAX_BYTES,
                "max_bytes_min": crate::context::MIN_MAX_BYTES,
                "max_bytes_max": crate::context::MAX_MAX_BYTES,
            },
        },
        "repo_count": reg.repos.len(),
    });
    #[cfg(feature = "semantic-knowledge")]
    let value = {
        let mut value = value;
        json::sort_value_keys(&mut value);
        value
    };
    let text = match serde_json::to_string(&value) {
        Ok(text) => text,
        Err(e) => {
            return tool_error(
                "store_error",
                &format!("internal response assembly error: {e}"),
            )
        }
    };
    doc(&text)
}

/// Run the MCP stdio server to completion. Exit codes follow the CLI:
/// 0 clean shutdown (client EOF/disconnect), 3 runtime failure.
pub(crate) fn run() -> i32 {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: mcp runtime: {e}");
            return 3;
        }
    };
    runtime.block_on(async {
        let service = match serve_server(RiltraMcp, rmcp::transport::io::stdio()).await {
            Ok(service) => service,
            Err(e) => {
                eprintln!("error: mcp initialize failed: {e}");
                return 3;
            }
        };
        match service.waiting().await {
            Ok(rmcp::service::QuitReason::JoinError(e)) => {
                eprintln!("error: mcp task failed: {e}");
                3
            }
            Ok(_) => 0,
            Err(e) => {
                eprintln!("error: mcp task failed: {e}");
                3
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{doc, sorted_tool_definitions, structured_doc, tool_error};

    fn assert_schema_keys_are_sorted(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                let keys: Vec<_> = object.keys().map(String::as_str).collect();
                assert!(
                    keys.windows(2).all(|pair| pair[0] <= pair[1]),
                    "schema keys are not sorted: {keys:?}"
                );
                for value in object.values() {
                    assert_schema_keys_are_sorted(value);
                }
            }
            serde_json::Value::Array(items) => {
                for value in items {
                    assert_schema_keys_are_sorted(value);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn tool_schemas_keep_core_key_order() {
        let tools = sorted_tool_definitions();
        let code_search = tools
            .iter()
            .find(|tool| tool.name == "riltra_code_search")
            .expect("code search tool");
        let properties = code_search
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("code search properties");
        let keys: Vec<_> = properties.keys().map(String::as_str).collect();
        assert_eq!(keys, ["limit", "occurrences", "offset", "query", "repo"]);

        for tool in tools {
            assert_schema_keys_are_sorted(&serde_json::Value::Object(
                tool.input_schema.as_ref().clone(),
            ));
            if let Some(schema) = tool.output_schema {
                assert_schema_keys_are_sorted(&serde_json::Value::Object(schema.as_ref().clone()));
            }
        }
    }

    #[test]
    fn mcp_structured_json_is_recursively_sorted_for_core_byte_compatibility() {
        let input = r#"{"z":{"y":1,"a":2},"items":[{"d":4,"b":3}],"a":0}"#;
        let expected = r#"{"a":0,"items":[{"b":3,"d":4}],"z":{"a":2,"y":1}}"#;
        let parsed = serde_json::from_str(input).unwrap();

        for result in [doc(input), structured_doc(parsed)] {
            let content = result.content[0].as_text().unwrap();
            assert_eq!(content.text, expected);
            let structured = result.structured_content.as_ref().unwrap();
            assert_eq!(serde_json::to_string(structured).unwrap(), expected);
        }

        let error = tool_error("invalid_input", "bad input");
        assert_eq!(
            error.content[0].as_text().unwrap().text,
            r#"{"kind":"invalid_input","message":"bad input"}"#
        );
    }
}
