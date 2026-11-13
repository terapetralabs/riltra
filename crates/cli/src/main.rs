// SPDX-License-Identifier: Apache-2.0
// M1: native CLI + repository registry. No networking; repository code is never
// executed (git subprocesses are read-only plumbing invoked by explicit user
// commands with GIT_TERMINAL_PROMPT=0).
//
// Exit codes: 0 ok, 2 usage error, 3 repository/data error. Successful commands
// print JSON to stdout; `repo status` and `doctor` print JSON even on data
// errors so scripts stay machine-readable.

mod args;
mod backup;
mod context;
mod doctor;
mod edges;
mod git;
mod ignore;
mod index;
#[cfg(feature = "semantic-knowledge")]
#[allow(dead_code)] // The loader is consumed by the following optional vector-lifecycle task.
mod inference;
mod json;
mod knowledge;
mod knowledge_binding;
mod knowledge_search;
mod knowledge_store;
#[cfg(feature = "semantic-knowledge")]
mod knowledge_vector;
mod lsp;
mod mcp;
mod paths;
mod progress;
mod registry;
mod response;
mod scan;
mod scip;
mod scip_proto;
mod semantic;
mod store;
mod syntax;

#[cfg(test)]
mod testutil;

use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use args::Command;
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};

pub(crate) const NAME: &str = "riltra";
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const SCOPE: &str = "m1-registry";

pub(crate) fn usage() -> String {
    format!(
        "Usage:\n  {NAME} [--help] [--version] status\n  {NAME} init --repo <path> [--name <name>]\n  {NAME} repo list\n  {NAME} repo status --repo <name>\n  {NAME} doctor [--repo <name>]\n  {NAME} scan --repo <name> [--path <relpath>]... [--symbol <substr>] [--progress]\n  {NAME} index --repo <name> [--full] [--progress]\n  {NAME} index status --repo <name>\n  {NAME} search --repo <name> --symbol <substr> [--limit <n>] [--offset <n>] [--occurrences]
  {NAME} semantic import --repo <name> --scip <file> [--source-digest <hex>]
  {NAME} semantic status --repo <name>
  {NAME} semantic query --repo <name> --path <rel> --line <n> --col <n> [--refs] [--lsp-server <abs-path>]
  {NAME} callers --repo <name> (--path <rel> --line <n> --col <n> | --target <text> [--in <rel>] [--kind <kind>]) [--symbol <uri>] [--limit <n>] [--offset <n>]
  {NAME} callees --repo <name> (--path <rel> --line <n> --col <n> | --target <text> [--in <rel>] [--kind <kind>]) [--symbol <uri>] [--limit <n>] [--offset <n>]
  {NAME} trace --repo <name> (--path <rel> --line <n> --col <n> | --target <text> [--in <rel>] [--kind <kind>]) [--symbol <uri>] [--to <path:line:col>] [--depth <n>] [--limit <n>] [--offset <n>]\n  {NAME} impact --repo <name> (--path <rel> --line <n> --col <n> | --target <text> [--in <rel>] [--kind <kind>]) [--symbol <uri>] [--depth <n>] [--limit <n>] [--offset <n>]\n  {NAME} context --repo <name> (--path <rel> --line <n> --col <n> | --target <text> [--in <rel>] [--kind <kind>]) [--symbol <uri>] [--query <text>] [--semantic] [--depth <n>] [--limit <n>] [--max-bytes <n>]\n  {NAME} knowledge create --repo <name> --input -\n  {NAME} knowledge show --repo <name> --id <id> [--revision <n>]\n  {NAME} knowledge update --repo <name> --id <id> --expected-revision <n> --input -\n  {NAME} knowledge history --repo <name> --id <id> [--limit <n>] [--offset <n>] [--generation <n>]\n  {NAME} knowledge search --repo <name> [--query <text>] [--path <rel>] [--kind <kind>] [--semantic] [--include-archived] [--include-superseded] [--include-retracted] [--limit <n>] [--offset <n>] [--generation <n>]\n  {NAME} knowledge link --repo <name> --input -\n  {NAME} knowledge unlink --repo <name> --relation <id> --input -\n  {NAME} knowledge bind --repo <name> --path <rel> --line <n> --col <n> [--symbol <uri>] --input -\n  {NAME} knowledge unbind --repo <name> --binding <id> --input -\n  {NAME} knowledge at --repo <name> --path <rel> --line <n> --col <n> [--symbol <uri>] [--limit <n>] [--offset <n>] [--generation <n>]\n  {NAME} knowledge related --repo <name> --id <id> [--revision <n>] [--direction inbound|outbound] [--include-retracted] [--limit <n>] [--offset <n>] [--generation <n>]\n  {NAME} knowledge export --repo <name> [--format jsonl|md] [--out <path>]\n  {NAME} knowledge import --repo <name> --file <path>\n  {NAME} knowledge backup --repo <name> --out <path>\n  {NAME} knowledge restore --repo <name> --file <path> [--overwrite]\n  {NAME} knowledge verify --repo <name>\n  {NAME} knowledge vector build --repo <name> --model-dir <path>\n  {NAME} knowledge vector status --repo <name>\n  {NAME} knowledge vector clear --repo <name>\n  {NAME} mcp\n\nCommands:\n  status       Print deterministic bootstrap status as JSON\n  init         Register a local repository root\n  repo list    List registered repositories as JSON\n  repo status  Git/worktree fingerprint of a registered repository\n  doctor       Path-security and repository health checks as JSON\n  scan         Syntax facts (definitions/imports/calls) as JSON\n  index        Build the durable code index for a repository\n  index status Report index freshness (fresh/stale/unknown) as JSON\n  search       Search indexed symbols as JSON\n  semantic import Import a SCIP index against the live snapshot as JSON\n  semantic status Per-provider binding verification as JSON\n  semantic query Single-hop definition/reference lookup as JSON\n  callers Direct callers of the anchored identity as JSON\n  callees Direct callees of the anchored identity as JSON\n  trace Bounded plausible static paths with per-hop evidence\n  impact Conservative inbound impact with evidence and UNKNOWN default\n  context Bounded graph context with caller-to-anchor distances\n  mcp Serve the MCP stdio interface\n"
    )
}

fn version_line() -> String {
    format!("{NAME} {VERSION}")
}

// Single-line JSON with fixed key order so output is stable.
pub(crate) fn status_json() -> String {
    format!(
        "{{\"name\":\"{NAME}\",\"version\":\"{VERSION}\",\"scope\":\"{SCOPE}\",\"networking\":\"disabled\"}}"
    )
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cmd_init(repo: &str, name: Option<&str>, out: &mut String, err: &mut String) -> i32 {
    let reg_path = paths::registry_path();
    let mut reg = match registry::load(&reg_path) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let canon = match paths::canonical_root(Path::new(repo)) {
        Ok(c) => c,
        Err(e) => {
            err.push_str(&format!("error: cannot register '{repo}': {e}\n"));
            return 3;
        }
    };
    let root = canon.to_string_lossy().to_string();
    let name = match name {
        Some(n) => n.to_string(),
        None => match canon.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => {
                err.push_str("error: cannot derive a name for this root; pass --name\n");
                return 3;
            }
        },
    };
    if !paths::validate_name(&name) {
        err.push_str(&format!("error: invalid repository name: '{name}'\n"));
        return 2;
    }
    let is_new = match registry::add(&mut reg, &name, &root, now_unix()) {
        Ok(is_new) => is_new,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    if let Err(e) = registry::save(&reg_path, &reg) {
        err.push_str(&format!("error: {e}\n"));
        return 3;
    }
    *out = format!(
        "{{\"name\":\"{}\",\"root\":\"{}\",\"registered\":{}}}\n",
        json::escape(&name),
        json::escape(&root),
        is_new
    );
    0
}

fn cmd_repo_list(out: &mut String, err: &mut String) -> i32 {
    match registry::load(&paths::registry_path()) {
        Ok(reg) => {
            *out = registry::list_json(&reg) + "\n";
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn cmd_repo_status(name: &str, out: &mut String, err: &mut String) -> i32 {
    let reg = match registry::load(&paths::registry_path()) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let entry = match registry::find(&reg, name) {
        Some(e) => e,
        None => {
            err.push_str(&format!("error: unknown repository: '{name}'\n"));
            return 3;
        }
    };
    let recorded = entry.root.clone();
    let canon = match paths::canonical_root(Path::new(&recorded)) {
        Ok(c) => c,
        Err(_) => {
            err.push_str(&format!(
                "error: registered root is missing: '{recorded}'\n"
            ));
            return 3;
        }
    };
    let root = canon.to_string_lossy().to_string();
    // The record must still resolve to itself: a moved root or a
    // symlink/junction retarget is rejected instead of fingerprinted.
    if root != recorded {
        err.push_str(&format!(
            "error: registered root moved or retargeted (record '{recorded}')\n"
        ));
        return 3;
    }
    let info = git::fingerprint(&canon);
    let (_, ignore) = ignore::load_rules(&canon);
    *out = response::repo_status_json(name, &root, &info, &ignore) + "\n";
    if info.present && !info.valid {
        3
    } else {
        0
    }
}

fn resolve_recorded_root(name: &str, err: &mut String) -> Result<(String, String), i32> {
    let reg = match registry::load(&paths::registry_path()) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return Err(3);
        }
    };
    let entry = match registry::find(&reg, name) {
        Some(e) => e,
        None => {
            err.push_str(&format!("error: unknown repository: '{name}'\n"));
            return Err(3);
        }
    };
    let recorded = entry.root.clone();
    let canon = match paths::canonical_root(Path::new(&recorded)) {
        Ok(c) => c,
        Err(_) => {
            err.push_str(&format!(
                "error: registered root is missing: '{recorded}'\n"
            ));
            return Err(3);
        }
    };
    let root = canon.to_string_lossy().to_string();
    if root != recorded {
        err.push_str(&format!(
            "error: registered root moved or retargeted (record '{recorded}')\n"
        ));
        return Err(3);
    }
    Ok((root, recorded))
}

fn cmd_index(name: &str, full: bool, out: &mut String, err: &mut String) -> i32 {
    let home = paths::config_dir();
    let (root, _) = match resolve_recorded_root(name, err) {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    match index::build(&home, name, Path::new(&root), full) {
        Ok(summary) => {
            *out = format!(
                "{{\"repo\":\"{}\",\"root\":\"{}\",\"snapshot\":{},\"status_at_write\":\"{}\",\"mode\":\"{}\",\"reused\":{},\"reparsed\":{},\"added\":{},\"removed\":{},\"fallback_reason\":\"{}\",\"timings_ms\":{{\"diff\":{},\"ingest\":{},\"verify\":{},\"promote\":{}}},\"files\":{{\"discovered\":{},\"processed\":{},\"definitions\":{},\"imports\":{},\"calls\":{},\"errors\":{},\"skipped_ignored\":{},\"skipped_large\":{},\"skipped_binary\":{},\"skipped_symlink\":{},\"skipped_escape\":{},\"skipped_unreadable\":{},\"unsupported\":{}}}}}",
                json::escape(name),
                json::escape(&root),
                summary.snapshot_id,
                summary.status_at_write,
                summary.mode,
                summary.reused,
                summary.reparsed,
                summary.added,
                summary.removed,
                summary.fallback_reason,
                summary.timings_ms.diff,
                summary.timings_ms.ingest,
                summary.timings_ms.verify,
                summary.timings_ms.promote,
                summary.files_discovered,
                summary.processed,
                summary.definitions,
                summary.imports,
                summary.calls,
                summary.errors,
                summary.skipped_ignored,
                summary.skipped_large,
                summary.skipped_binary,
                summary.skipped_symlink,
                summary.skipped_escape,
                summary.skipped_unreadable,
                summary.unsupported_total
            ) + "\n";
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn cmd_index_status(name: &str, out: &mut String, err: &mut String) -> i32 {
    let home = paths::config_dir();
    let reg = match registry::load(&paths::registry_path()) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let entry = match registry::find(&reg, name) {
        Some(e) => e,
        None => {
            err.push_str(&format!("error: unknown repository: '{name}'\n"));
            return 3;
        }
    };
    let recorded = entry.root.clone();
    let freshness = index::evaluate(&home, name, Path::new(&recorded));
    // The FTS flag reflects build capability, not revision content, so a
    // separate connection is safe here.
    let fts = store::open_live(&store::db_path(&home, name))
        .map(|conn| store::fts_available(&conn))
        .unwrap_or(false);
    *out = response::index_status_json(name, &recorded, &freshness, fts) + "\n";
    0
}

fn parse_count(value: Option<&str>, fallback: usize, arg: &str) -> Result<usize, String> {
    match value {
        None => Ok(fallback),
        Some(text) => text
            .parse::<usize>()
            .map_err(|_| format!("invalid --{arg} value: '{text}'")),
    }
}

fn cmd_search(
    name: &str,
    symbol: &str,
    limit: Option<&str>,
    offset: Option<&str>,
    occurrences: bool,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let limit = match parse_count(limit, index::DEFAULT_SEARCH_LIMIT, "limit") {
        Ok(n) => n,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let offset = match parse_count(offset, 0, "offset") {
        Ok(n) => n,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    if limit > index::MAX_SEARCH_LIMIT {
        err.push_str(&format!(
            "error: --limit exceeds maximum {}\n",
            index::MAX_SEARCH_LIMIT
        ));
        return 2;
    }
    let home = paths::config_dir();
    let reg = match registry::load(&paths::registry_path()) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let entry = match registry::find(&reg, name) {
        Some(e) => e,
        None => {
            err.push_str(&format!("error: unknown repository: '{name}'\n"));
            return 3;
        }
    };
    let recorded = entry.root.clone();
    let db = store::db_path(&home, name);
    if !db.exists() {
        *out = response::search_response_json(&response::SearchResponse {
            repo: name,
            root: &recorded,
            snapshot: None,
            freshness: "unknown",
            reasons: &["no-index".to_string()],
            symbol,
            limit,
            offset,
            total: 0,
            hits: &[],
            occurrences: None,
        }) + "\n";
        return 0;
    }
    // One read transaction across snapshot read, verdict, and fact rows:
    // the response can never mix two revisions.
    let mut conn = match store::open_live(&db) {
        Ok(c) => c,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let snapshot = match store::latest_snapshot(&tx) {
        Ok(Some(snapshot)) => snapshot,
        _ => {
            err.push_str("error: index database unreadable; rebuild with `index`\n");
            return 3;
        }
    };
    let canon_root = match Path::new(&recorded).canonicalize() {
        Ok(c) => c,
        Err(_) => {
            let _ = tx.commit();
            *out = response::search_response_json(&response::SearchResponse {
                repo: name,
                root: &recorded,
                snapshot: Some(&snapshot),
                freshness: "unknown",
                reasons: &["root-unreadable".to_string()],
                symbol,
                limit,
                offset,
                total: 0,
                hits: &[],
                occurrences: None,
            }) + "\n";
            return 0;
        }
    };
    let (status, reasons) = index::evaluate_snapshot(&tx, name, &canon_root, &snapshot);
    let (total, hits) = match store::search_symbols(&tx, snapshot.id, symbol, limit, offset) {
        Ok(found) => found,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let census = if occurrences {
        match store::occurrences_for(&tx, snapshot.id, symbol, limit, offset) {
            Ok(list) => Some(list),
            Err(e) => {
                err.push_str(&format!("error: {e}\n"));
                return 3;
            }
        }
    } else {
        None
    };
    let _ = tx.commit();
    *out = response::search_response_json(&response::SearchResponse {
        repo: name,
        root: &snapshot.root,
        snapshot: Some(&snapshot),
        freshness: &status,
        reasons: &reasons,
        symbol,
        limit,
        offset,
        total,
        hits: &hits,
        occurrences: census.as_ref(),
    }) + "\n";
    0
}

fn cmd_scan(
    name: &str,
    paths: &[String],
    symbol: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let reg = match registry::load(&paths::registry_path()) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let entry = match registry::find(&reg, name) {
        Some(e) => e,
        None => {
            err.push_str(&format!("error: unknown repository: '{name}'\n"));
            return 3;
        }
    };
    let recorded = entry.root.clone();
    let canon = match paths::canonical_root(Path::new(&recorded)) {
        Ok(c) => c,
        Err(_) => {
            err.push_str(&format!(
                "error: registered root is missing: '{recorded}'\n"
            ));
            return 3;
        }
    };
    if canon.to_string_lossy() != recorded {
        err.push_str(&format!(
            "error: registered root moved or retargeted (record '{recorded}')\n"
        ));
        return 3;
    }
    let opts = scan::ScanOptions {
        paths: paths.to_vec(),
        symbol: symbol.map(str::to_string),
    };
    match scan::scan_repo(name, &canon, &opts) {
        Ok(json) => {
            *out = json + "\n";
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn cmd_doctor(repo: Option<&str>, out: &mut String, err: &mut String) -> i32 {
    match repo {
        None => {
            let (text, pass) = doctor::environment_doctor();
            *out = format!("{text}\n");
            if pass {
                0
            } else {
                3
            }
        }
        Some(name) => {
            let reg = match registry::load(&paths::registry_path()) {
                Ok(r) => r,
                Err(e) => {
                    err.push_str(&format!("error: {e}\n"));
                    return 3;
                }
            };
            let entry = match registry::find(&reg, name) {
                Some(e) => e,
                None => {
                    err.push_str(&format!("error: unknown repository: '{name}'\n"));
                    return 3;
                }
            };
            let recorded = entry.root.clone();
            let canon = match paths::canonical_root(Path::new(&recorded)) {
                Ok(c) => c,
                Err(_) => {
                    *out = doctor::doctor_json(
                        name,
                        &recorded,
                        &[doctor::Check {
                            name: "root-exists-dir".to_string(),
                            pass: false,
                            detail: format!("root {recorded} is missing or not a directory"),
                        }],
                    ) + "\n";
                    return 3;
                }
            };
            let info = git::fingerprint(&canon);
            let (_, ignore) = ignore::load_rules(&canon);
            let root = canon.to_string_lossy().to_string();
            let checks = doctor::run_checks(name, &canon, &recorded, &info, &ignore);
            let pass = checks.iter().all(|c| c.pass);
            *out = doctor::doctor_json(name, &root, &checks) + "\n";
            if pass {
                0
            } else {
                3
            }
        }
    }
}

/// Runs one scan or index command, printing progress lines to stderr when
/// `enabled` (the `--progress` flag). The final `done` line is written after
/// the work on every return path, including error exits.
fn with_progress(label: &str, enabled: bool, work: impl FnOnce() -> i32) -> i32 {
    if enabled {
        progress::enable();
        progress::start(label);
    }
    let code = work();
    if enabled {
        progress::finish();
    }
    code
}

fn dispatch(cmd: Command, out: &mut String, err: &mut String) -> i32 {
    match cmd {
        Command::Help => {
            *out = usage();
            0
        }
        Command::Version => {
            *out = version_line() + "\n";
            0
        }
        Command::Status => {
            *out = status_json() + "\n";
            0
        }
        Command::Init { repo, name } => cmd_init(&repo, name.as_deref(), out, err),
        Command::RepoList => cmd_repo_list(out, err),
        Command::RepoStatus { repo } => cmd_repo_status(&repo, out, err),
        Command::Doctor { repo } => cmd_doctor(repo.as_deref(), out, err),
        Command::Scan {
            repo,
            paths,
            symbol,
            progress,
        } => with_progress("scan", progress, || {
            cmd_scan(&repo, &paths, symbol.as_deref(), out, err)
        }),
        Command::Index {
            repo,
            full,
            progress,
        } => with_progress("index", progress, || cmd_index(&repo, full, out, err)),
        Command::IndexStatus { repo } => cmd_index_status(&repo, out, err),
        Command::Search {
            repo,
            symbol,
            limit,
            offset,
            occurrences,
        } => cmd_search(
            &repo,
            &symbol,
            limit.as_deref(),
            offset.as_deref(),
            occurrences,
            out,
            err,
        ),
        Command::Semantic { op } => cmd_semantic(op, out, err),
        Command::Edge { op } => cmd_edge(op, out, err),
        Command::Trace(args) => cmd_trace(args, out, err),
        Command::Impact(args) => cmd_graph(args, out, err, true),
        Command::Context(args) => cmd_context(args, out, err),
        Command::Knowledge { op } => cmd_knowledge(op, out, err),
        Command::Mcp => mcp::run(),
    }
}

fn cmd_semantic(op: args::SemanticOp, out: &mut String, err: &mut String) -> i32 {
    match op {
        args::SemanticOp::Import { repo, scip, claim } => {
            cmd_semantic_import(&repo, &scip, claim.as_deref(), out, err)
        }
        args::SemanticOp::Status { repo } => cmd_semantic_status(&repo, out, err),
        args::SemanticOp::Query {
            repo,
            path,
            line,
            col,
            refs,
            lsp_server,
        } => {
            let query = QueryArgs {
                path: &path,
                line: &line,
                col: &col,
                refs,
                lsp_server: lsp_server.as_deref(),
            };
            cmd_semantic_query(&repo, &query, out, err)
        }
    }
}

/// Bundled `semantic query` arguments: one struct instead of an
/// eight-parameter tail.
struct QueryArgs<'a> {
    path: &'a str,
    line: &'a str,
    col: &'a str,
    refs: bool,
    lsp_server: Option<&'a str>,
}

/// The anchor a command runs on: the parsed `--path/--line/--col` position, or
/// the one definition a `--target` name resolves to. `Doc` is the non-resolved
/// answer that replaces the command's output (exit 0).
fn resolve_anchor(
    home: &Path,
    repo: &str,
    position: Option<(String, u32, u32)>,
    target: Option<&str>,
    in_path: Option<&str>,
    kind: Option<&str>,
    err: &mut String,
) -> Result<index::TargetResolution, i32> {
    if let Some((path, line, col)) = position {
        return Ok(index::TargetResolution::One { path, line, col });
    }
    let Some(target) = target else {
        // The parsers require a position or a target, so this is a bug guard.
        err.push_str(&format!("{}\n", usage()));
        return Err(2);
    };
    resolve_by_name(
        home,
        repo,
        &index::TargetQuery {
            target,
            in_path,
            kind,
        },
        err,
    )
}

/// Resolve a `--target` name on the latest snapshot. Storage is only read: a
/// missing index answers `no-index` and nothing is created or migrated.
fn resolve_by_name(
    home: &Path,
    repo: &str,
    query: &index::TargetQuery,
    err: &mut String,
) -> Result<index::TargetResolution, i32> {
    match store::open_read(&store::db_path(home, repo)) {
        store::IndexRead::Missing => Ok(index::TargetResolution::Doc(
            index::no_index_target_document(repo, query),
        )),
        store::IndexRead::Outdated(version) => {
            err.push_str(&format!("error: index schema v{version} is outdated\n"));
            Err(3)
        }
        store::IndexRead::Ahead(version) => {
            err.push_str(&format!(
                "error: database schema v{version} is newer than supported v{}\n",
                store::SCHEMA_VERSION
            ));
            Err(3)
        }
        store::IndexRead::Error(e) => {
            err.push_str(&format!("error: {e}\n"));
            Err(3)
        }
        store::IndexRead::Ready(conn) => index::resolve_target(&conn, repo, query).map_err(|e| {
            err.push_str(&format!("error: {e}\n"));
            3
        }),
    }
}

fn cmd_edge(op: args::EdgeOp, out: &mut String, err: &mut String) -> i32 {
    let (repo, path, line, col, target, in_path, kind, symbol, limit, offset, dir) = match op {
        args::EdgeOp::Callers {
            repo,
            path,
            line,
            col,
            target,
            in_path,
            kind,
            symbol,
            limit,
            offset,
        } => (
            repo,
            path,
            line,
            col,
            target,
            in_path,
            kind,
            symbol,
            limit,
            offset,
            index::EdgeDir::Callers,
        ),
        args::EdgeOp::Callees {
            repo,
            path,
            line,
            col,
            target,
            in_path,
            kind,
            symbol,
            limit,
            offset,
        } => (
            repo,
            path,
            line,
            col,
            target,
            in_path,
            kind,
            symbol,
            limit,
            offset,
            index::EdgeDir::Callees,
        ),
    };
    let home = paths::config_dir();
    let (root, _) = match resolve_recorded_root(&repo, err) {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    let position = match (path.as_deref(), line.as_deref(), col.as_deref()) {
        (Some(p), Some(l), Some(c)) => {
            match (trace_position(l, "--line"), trace_position(c, "--col")) {
                (Ok(l), Ok(c)) => Some((p.to_string(), l, c)),
                (Err(e), _) | (_, Err(e)) => {
                    err.push_str(&format!("error: {e}\n"));
                    return 2;
                }
            }
        }
        _ => None,
    };
    let limit_number = match parse_count(limit.as_deref(), index::DEFAULT_SEARCH_LIMIT, "limit") {
        Ok(n) => n,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let offset_number = match parse_count(offset.as_deref(), 0, "offset") {
        Ok(n) => n,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    if limit_number == 0 || limit_number > index::MAX_SEARCH_LIMIT {
        err.push_str(&format!(
            "error: --limit must be between 1 and {}\n",
            index::MAX_SEARCH_LIMIT
        ));
        return 2;
    }
    if offset_number > i64::MAX as usize - index::MAX_SEARCH_LIMIT {
        err.push_str("error: --offset exceeds supported range\n");
        return 2;
    }
    let (path, line_number, col_number) = match resolve_anchor(
        &home,
        &repo,
        position,
        target.as_deref(),
        in_path.as_deref(),
        kind.as_deref(),
        err,
    ) {
        Ok(index::TargetResolution::One { path, line, col }) => (path, line, col),
        Ok(index::TargetResolution::Doc(doc)) => {
            *out = doc + "\n";
            return 0;
        }
        Err(code) => return code,
    };
    let request = index::EdgeRequest {
        anchor: index::SpanQuery {
            rel: &path,
            line1: line_number,
            col1: col_number,
            refs: false,
        },
        symbol: symbol.as_deref(),
        direction: dir,
        limit: limit_number,
        offset: offset_number,
    };
    match index::edge_query(&home, &repo, Path::new(&root), &request) {
        Ok(o) => {
            *out = response::edge_query_json(&o) + "\n";
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn trace_position(value: &str, flag: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("invalid {flag} value: '{value}'"))
}

fn cmd_trace(args: args::TraceArgs, out: &mut String, err: &mut String) -> i32 {
    cmd_graph(args, out, err, false)
}

fn cmd_graph(args: args::TraceArgs, out: &mut String, err: &mut String, impact: bool) -> i32 {
    let parsed = (|| {
        let position = match (&args.path, &args.line, &args.col) {
            (Some(path), Some(line), Some(col)) => Some((
                path.clone(),
                trace_position(line, "--line")?,
                trace_position(col, "--col")?,
            )),
            _ => None,
        };
        let depth = parse_count(args.depth.as_deref(), 3, "depth")?;
        if !(1..=edges::TRACE_MAX_DEPTH).contains(&depth) {
            return Err("--depth must be between 1 and 6".to_string());
        }
        let limit = parse_count(args.limit.as_deref(), index::DEFAULT_SEARCH_LIMIT, "limit")?;
        if !(1..=index::MAX_SEARCH_LIMIT).contains(&limit) {
            return Err("--limit must be between 1 and 1000".to_string());
        }
        let offset = parse_count(args.offset.as_deref(), 0, "offset")?;
        if offset > i64::MAX as usize - index::MAX_SEARCH_LIMIT {
            return Err("--offset exceeds supported range".to_string());
        }
        let to = args
            .to
            .as_deref()
            .map(|to| {
                let (rest, col) = to.rsplit_once(':').ok_or("--to must be PATH:LINE:COL")?;
                let (path, line) = rest.rsplit_once(':').ok_or("--to must be PATH:LINE:COL")?;
                if path.is_empty() {
                    return Err("--to requires a path".to_string());
                }
                Ok((
                    path.to_string(),
                    trace_position(line, "--to line")?,
                    trace_position(col, "--to col")?,
                ))
            })
            .transpose()?;
        Ok((position, depth, limit, offset, to))
    })();
    let (position, depth, limit, offset, to) = match parsed {
        Ok(p) => p,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let (root, _) = match resolve_recorded_root(&args.repo, err) {
        Ok(p) => p,
        Err(code) => return code,
    };
    // A name answers before the query runs, so the destination is checked here
    // exactly as the position form checks it (same refusal and exit code).
    if let (Some(_), Some((dest, _, _))) = (&args.target, &to) {
        if let Err(e) = crate::semantic::validate_doc_path(Path::new(&root), dest) {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    }
    let (path, line, col) = match resolve_anchor(
        &paths::config_dir(),
        &args.repo,
        position,
        args.target.as_deref(),
        args.in_path.as_deref(),
        args.kind.as_deref(),
        err,
    ) {
        Ok(index::TargetResolution::One { path, line, col }) => (path, line, col),
        Ok(index::TargetResolution::Doc(doc)) => {
            *out = doc + "\n";
            return 0;
        }
        Err(code) => return code,
    };
    let request = index::TraceRequest {
        source: index::SpanQuery {
            rel: &path,
            line1: line,
            col1: col,
            refs: false,
        },
        symbol: args.symbol.as_deref(),
        destination: to.as_ref().map(|(p, l, c)| index::SpanQuery {
            rel: p,
            line1: *l,
            col1: *c,
            refs: false,
        }),
        depth,
        limit,
        offset,
        summary: args.summary,
        include_tests: args.include_tests,
    };
    let query = if impact {
        index::impact_query
    } else {
        index::trace_query
    };
    match query(&paths::config_dir(), &args.repo, Path::new(&root), &request) {
        Ok(result) => {
            *out = response::trace_query_json(&result) + "\n";
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn cmd_context(args: args::ContextArgs, out: &mut String, err: &mut String) -> i32 {
    let parsed = (|| {
        let position = match (&args.path, &args.line, &args.col) {
            (Some(path), Some(line), Some(col)) => Some((
                path.clone(),
                trace_position(line, "--line")?,
                trace_position(col, "--col")?,
            )),
            _ => None,
        };
        let depth = parse_count(args.depth.as_deref(), context::DEFAULT_DEPTH, "depth")?;
        if !(1..=edges::TRACE_MAX_DEPTH).contains(&depth) {
            return Err("--depth must be between 1 and 6".to_string());
        }
        let limit = parse_count(args.limit.as_deref(), context::DEFAULT_LIMIT, "limit")?;
        if !(1..=context::MAX_LIMIT).contains(&limit) {
            return Err(format!(
                "--limit must be between 1 and {}",
                context::MAX_LIMIT
            ));
        }
        let max_bytes = parse_count(
            args.max_bytes.as_deref(),
            context::DEFAULT_MAX_BYTES,
            "max-bytes",
        )?;
        if !(context::MIN_MAX_BYTES..=context::MAX_MAX_BYTES).contains(&max_bytes) {
            return Err(format!(
                "--max-bytes must be between {} and {}",
                context::MIN_MAX_BYTES,
                context::MAX_MAX_BYTES
            ));
        }
        Ok((position, depth, limit, max_bytes))
    })();
    let (position, depth, limit, max_bytes) = match parsed {
        Ok(values) => values,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let (root, entry) = match knowledge_repo(&args.repo, err) {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    if let Err(e) = context::validate_options(
        depth,
        limit,
        max_bytes,
        args.query.as_deref(),
        args.semantic,
    ) {
        err.push_str(&format!("error: {e}\n"));
        return 2;
    }
    let home = paths::config_dir();
    let (path, line, col) = match resolve_anchor(
        &home,
        &args.repo,
        position,
        args.target.as_deref(),
        args.in_path.as_deref(),
        args.kind.as_deref(),
        err,
    ) {
        Ok(index::TargetResolution::One { path, line, col }) => (path, line, col),
        Ok(index::TargetResolution::Doc(doc)) => {
            *out = doc + "\n";
            return 0;
        }
        Err(code) => return code,
    };
    let request = context::ContextRequest {
        path: &path,
        line,
        col,
        symbol: args.symbol.as_deref(),
        query: args.query.as_deref(),
        semantic: args.semantic,
        depth,
        limit,
        max_bytes,
    };
    if let Err(e) = context::validate_request(Path::new(&root), &request) {
        err.push_str(&format!("error: {e}\n"));
        return if e.starts_with("invalid context bounds")
            || e.starts_with("context query")
            || e.starts_with("context semantic")
        {
            2
        } else {
            3
        };
    }
    match store::open_read(&store::db_path(&home, &args.repo)) {
        store::IndexRead::Missing => {
            err.push_str("error: no snapshot: index the repository first\n");
            3
        }
        store::IndexRead::Outdated(version) => {
            err.push_str(&format!("error: index schema v{version} is outdated\n"));
            3
        }
        store::IndexRead::Ahead(version) => {
            err.push_str(&format!(
                "error: database schema v{version} is newer than supported v{}\n",
                store::SCHEMA_VERSION
            ));
            3
        }
        store::IndexRead::Error(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
        store::IndexRead::Ready(mut conn) => {
            let result = match context::query_conn(
                &mut conn,
                &home,
                &entry,
                &args.repo,
                Path::new(&root),
                &request,
            ) {
                Ok(result) => result,
                Err(e) => {
                    err.push_str(&format!("error: {e}\n"));
                    return 3;
                }
            };
            match response::context_query_json(&result) {
                Ok(value) => {
                    *out = value + "\n";
                    0
                }
                Err(e) => {
                    err.push_str(&format!("error: {e}\n"));
                    3
                }
            }
        }
    }
}

fn cmd_semantic_import(
    name: &str,
    scip: &str,
    claim: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let home = paths::config_dir();
    let (root, _) = match resolve_recorded_root(name, err) {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    match index::semantic_import(&home, name, Path::new(&root), Path::new(scip), claim) {
        Ok(o) => {
            let skipped: Vec<String> = o
                .skipped
                .iter()
                .map(|s| format!("\"{}\"", json::escape(s)))
                .collect();
            *out = format!(
                "{{\"repo\":\"{}\",\"snapshot\":{},\"import\":{},\"provider\":\"{}\",\"provider_version\":\"{}\",\"documents\":{},\"definitions\":{},\"references\":{},\"skipped\":[{}],\"verification\":\"{}\",\"fallback_reason\":\"none\"}}\n",
                json::escape(name),
                o.snapshot_id,
                o.import_id,
                json::escape(&o.provider_name),
                json::escape(&o.provider_version),
                o.documents,
                o.definitions,
                o.references,
                skipped.join(","),
                o.verification
            );
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn cmd_semantic_status(name: &str, out: &mut String, err: &mut String) -> i32 {
    let home = paths::config_dir();
    let (root, _) = match resolve_recorded_root(name, err) {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    match index::semantic_status(&home, name, Path::new(&root)) {
        Ok((snapshot, rows)) => {
            let mut items = Vec::new();
            for row in &rows {
                items.push(format!(
                    "{{\"import\":{},\"provider\":\"{}\",\"provider_version\":\"{}\",\"capability\":\"{}\",\"verification\":\"{}\",\"artifact_id\":\"{}\",\"claimed_source_digest\":\"{}\",\"content_identity_match\":{},\"source_revision_match\":{},\"limitation\":\"{}\"}}",
                    row.import_id,
                    json::escape(&row.provider_name),
                    json::escape(&row.provider_version),
                    json::escape(&row.capability),
                    row.verification,
                    json::escape(&row.artifact_id),
                    json::escape(&row.claimed_source_digest),
                    row.content_match,
                    row.revision_match,
                    json::escape(&row.limitation)
                ));
            }
            *out = format!(
                "{{\"repo\":\"{}\",\"snapshot\":{},\"imports\":[{}]}}\n",
                json::escape(name),
                match &snapshot {
                    Some(s) => s.id.to_string(),
                    None => "null".to_string(),
                },
                items.join(",")
            );
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn cmd_semantic_query(
    name: &str,
    query: &QueryArgs<'_>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let home = paths::config_dir();
    let (root, _) = match resolve_recorded_root(name, err) {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    let line_number: u32 = match query.line.parse() {
        Ok(n) => n,
        Err(_) => {
            err.push_str(&format!("error: invalid --line value: '{}'\n", query.line));
            return 2;
        }
    };
    let col_number: u32 = match query.col.parse() {
        Ok(n) => n,
        Err(_) => {
            err.push_str(&format!("error: invalid --col value: '{}'\n", query.col));
            return 2;
        }
    };
    let span = index::SpanQuery {
        rel: query.path,
        line1: line_number,
        col1: col_number,
        refs: query.refs,
    };
    let answer = if let Some(exe) = query.lsp_server {
        index::semantic_query_lsp(Path::new(&root), &span, exe)
    } else {
        index::semantic_query(&home, name, Path::new(&root), &span)
    };
    match answer {
        Ok(o) => {
            *out = response::semantic_query_json(name, &o);
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn run(args: &[String], out: &mut String, err: &mut String) -> i32 {
    match args::parse(args) {
        Err(usage) => {
            *err = format!("error: unknown arguments\n{usage}");
            2
        }
        Ok(cmd) => dispatch(cmd, out, err),
    }
}

// ---------------------------------------------------------------------------
// M7 knowledge CLI: thin dispatch over the Task 1-6 store/model functions.
// Exit codes: 0 success (including empty search), 2 invalid arguments/input,
// 3 repository/storage errors, missing IDs, revision/generation conflicts.
// ---------------------------------------------------------------------------

fn cmd_knowledge(op: args::KnowledgeOp, out: &mut String, err: &mut String) -> i32 {
    match op {
        args::KnowledgeOp::Create { repo, input } => knowledge_create(&repo, &input, out, err),
        args::KnowledgeOp::Show { repo, id, revision } => {
            knowledge_show(&repo, &id, revision.as_deref(), out, err)
        }
        args::KnowledgeOp::Update {
            repo,
            id,
            expected_revision,
            input,
        } => knowledge_update(&repo, &id, &expected_revision, &input, out, err),
        args::KnowledgeOp::History {
            repo,
            id,
            limit,
            offset,
            generation,
        } => knowledge_history(
            &repo,
            &id,
            limit.as_deref(),
            offset.as_deref(),
            generation.as_deref(),
            out,
            err,
        ),
        args::KnowledgeOp::Search {
            repo,
            query,
            path,
            kind,
            semantic,
            include_archived,
            include_superseded,
            include_retracted,
            limit,
            offset,
            generation,
        } => knowledge_search(
            &repo,
            query.as_deref(),
            path.as_deref(),
            kind.as_deref(),
            semantic,
            include_archived,
            include_superseded,
            include_retracted,
            limit.as_deref(),
            offset.as_deref(),
            generation.as_deref(),
            out,
            err,
        ),
        args::KnowledgeOp::Link { repo, input } => knowledge_link(&repo, &input, out, err),
        args::KnowledgeOp::Unlink {
            repo,
            relation,
            input,
        } => knowledge_unlink(&repo, &relation, &input, out, err),
        args::KnowledgeOp::Bind {
            repo,
            path,
            line,
            col,
            symbol,
            input,
        } => knowledge_binding::create_cli(
            &repo,
            &path,
            &line,
            &col,
            symbol.as_deref(),
            &input,
            out,
            err,
        ),
        args::KnowledgeOp::Unbind {
            repo,
            binding,
            input,
        } => knowledge_binding::retract_cli(&repo, &binding, &input, out, err),
        args::KnowledgeOp::At {
            repo,
            path,
            line,
            col,
            symbol,
            limit,
            offset,
            generation,
        } => knowledge_binding::at_cli(
            &repo,
            &path,
            &line,
            &col,
            symbol.as_deref(),
            limit.as_deref(),
            offset.as_deref(),
            generation.as_deref(),
            out,
            err,
        ),
        args::KnowledgeOp::Export {
            repo,
            format,
            out: out_path,
        } => knowledge_export(&repo, format.as_deref(), out_path.as_deref(), out, err),
        args::KnowledgeOp::Import { repo, file } => knowledge_import(&repo, &file, out, err),
        args::KnowledgeOp::Backup {
            repo,
            out: out_path,
        } => knowledge_backup(&repo, &out_path, out, err),
        args::KnowledgeOp::Restore {
            repo,
            file,
            overwrite,
        } => knowledge_restore(&repo, &file, overwrite, out, err),
        args::KnowledgeOp::Verify { repo } => knowledge_verify(&repo, out, err),
        #[cfg(feature = "semantic-knowledge")]
        args::KnowledgeOp::VectorBuild { repo, model_dir } => {
            knowledge_vector::build_cli(&repo, &model_dir, out, err)
        }
        #[cfg(not(feature = "semantic-knowledge"))]
        args::KnowledgeOp::VectorBuild { .. } => vector_feature_unavailable(err),
        #[cfg(feature = "semantic-knowledge")]
        args::KnowledgeOp::VectorStatus { repo } => knowledge_vector::status_cli(&repo, out, err),
        #[cfg(not(feature = "semantic-knowledge"))]
        args::KnowledgeOp::VectorStatus { .. } => vector_feature_unavailable(err),
        #[cfg(feature = "semantic-knowledge")]
        args::KnowledgeOp::VectorClear { repo } => knowledge_vector::clear_cli(&repo, out, err),
        #[cfg(not(feature = "semantic-knowledge"))]
        args::KnowledgeOp::VectorClear { .. } => vector_feature_unavailable(err),
        args::KnowledgeOp::Related {
            repo,
            id,
            revision,
            direction,
            include_retracted,
            limit,
            offset,
            generation,
        } => knowledge_related(
            &repo,
            &id,
            revision.as_deref(),
            &direction,
            include_retracted,
            limit.as_deref(),
            offset.as_deref(),
            generation.as_deref(),
            out,
            err,
        ),
    }
}

#[cfg(not(feature = "semantic-knowledge"))]
fn vector_feature_unavailable(err: &mut String) -> i32 {
    err.push_str("error: knowledge vector commands require the semantic-knowledge feature\n");
    3
}

/// Load the registry entry and canonical root for `--repo`, returning a
/// `RepoEntry` usable by `open_knowledge`.
fn knowledge_repo(name: &str, err: &mut String) -> Result<(String, registry::RepoEntry), i32> {
    let (root, _) = resolve_recorded_root(name, err)?;
    Ok((
        root.clone(),
        registry::RepoEntry {
            name: name.to_string(),
            root,
            added_unix: 0,
        },
    ))
}

/// Open the knowledge store. `create=true` is used only by explicit
/// mutations; reads never create a database.
fn knowledge_open(
    name: &str,
    create: bool,
    err: &mut String,
) -> Result<(String, rusqlite::Connection), i32> {
    let (root, entry) = knowledge_repo(name, err)?;
    let conn = knowledge_store::open_knowledge(&entry, create).map_err(|e| {
        err.push_str(&format!("error: {e}\n"));
        3
    })?;
    Ok((root, conn))
}

/// Whether the knowledge store database is *genuinely* absent for a read.
/// A missing path yields `Ok(true)`, which read commands answer with an empty
/// result and no file creation. Any other state — symlink (dangling or live,
/// both an escape), directory, fifo, or an uninspectable path — is an error so
/// a hostile or corrupt path fails closed instead of reading as empty.
pub(crate) fn knowledge_store_absent(db: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(db) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(format!(
            "cannot inspect knowledge store '{}': {e}",
            db.display()
        )),
        Ok(md) if md.file_type().is_symlink() => Err(format!(
            "knowledge store path escapes via link: '{}'",
            db.display()
        )),
        Ok(md) if md.is_file() => Ok(false),
        Ok(_) => Err(format!(
            "knowledge store path is not a file: '{}'",
            db.display()
        )),
    }
}

/// Read strict mutation JSON from stdin (`--input -`). Only `-` is supported.
///
/// The read is bounded to [`knowledge::MAX_MUTATION_JSON_BYTES`] (1 MiB) plus
/// one detection byte: input past that limit is rejected without buffering the
/// whole oversized stream.
fn knowledge_input(flag: &str) -> Result<Vec<u8>, String> {
    if flag != "-" {
        return Err(format!(
            "invalid --input value: '{flag}' (only '-' is supported)"
        ));
    }
    let cap = knowledge::MAX_MUTATION_JSON_BYTES;
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .take(cap as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    if buf.len() > cap {
        return Err(format!(
            "mutation input too large: exceeds the {cap} byte ({} MiB) limit",
            cap / (1024 * 1024)
        ));
    }
    Ok(buf)
}

fn knowledge_page_args(
    limit: Option<&str>,
    offset: Option<&str>,
    generation: Option<&str>,
    err: &mut String,
) -> Result<(usize, usize, Option<u64>), i32> {
    let limit = match parse_count(limit, knowledge::SEARCH_LIMIT_DEFAULT, "limit") {
        Ok(n) => n,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return Err(2);
        }
    };
    if limit == 0 || limit > knowledge::SEARCH_LIMIT_MAX {
        err.push_str(&format!(
            "error: --limit must be between 1 and {}\n",
            knowledge::SEARCH_LIMIT_MAX
        ));
        return Err(2);
    }
    let offset = match parse_count(offset, 0, "offset") {
        Ok(n) => n,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return Err(2);
        }
    };
    let generation = match generation {
        None => None,
        Some(text) => match text.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                err.push_str(&format!("error: invalid --generation value: '{text}'\n"));
                return Err(2);
            }
        },
    };
    Ok((limit, offset, generation))
}

fn parse_optional_u64(
    value: Option<&str>,
    flag: &str,
    err: &mut String,
) -> Result<Option<u64>, i32> {
    match value {
        None => Ok(None),
        Some(text) => match text.parse::<u64>() {
            Ok(n) => Ok(Some(n)),
            Err(_) => {
                err.push_str(&format!("error: invalid --{flag} value: '{text}'\n"));
                Err(2)
            }
        },
    }
}

/// A write failure is a storage error (exit 3); an evidence-capture failure
/// and an invalid lifecycle transition (self-supersession, cycle, retracted
/// endpoint, double retraction) are input errors (exit 2). Unknown endpoints
/// and targets stay in the storage/conflict class (exit 3).
fn knowledge_write_error(e: &str) -> i32 {
    if e.starts_with("cannot capture evidence") || e.starts_with("invalid lifecycle transition") {
        2
    } else {
        3
    }
}

// --- strict JSON for link/unlink inputs ------------------------------------

/// A JSON object that preserves every key occurrence, so duplicate and
/// unknown keys can be rejected (a plain `serde_json::Value` map collapses
/// duplicates to the last one).
struct RawObject(Vec<(String, serde_json::Value)>);

impl<'de> Deserialize<'de> for RawObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RawVisitor;
        impl<'de> Visitor<'de> for RawVisitor {
            type Value = RawObject;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A>(self, mut map: A) -> Result<RawObject, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut fields = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    let value = map.next_value::<serde_json::Value>()?;
                    fields.push((key, value));
                }
                Ok(RawObject(fields))
            }
        }
        deserializer.deserialize_map(RawVisitor)
    }
}

pub(crate) fn strict_object(
    bytes: &[u8],
    known: &[&str],
) -> Result<Vec<(String, serde_json::Value)>, String> {
    if bytes.len() > knowledge::MAX_MUTATION_JSON_BYTES {
        return Err(format!(
            "mutation input too large: {} bytes, at most {} allowed",
            bytes.len(),
            knowledge::MAX_MUTATION_JSON_BYTES
        ));
    }
    let parsed: RawObject =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut seen: Vec<String> = Vec::new();
    for (key, _) in &parsed.0 {
        if seen.iter().any(|k| k == key) {
            return Err(format!("duplicate field `{key}`"));
        }
        if !known.contains(&key.as_str()) {
            return Err(format!("unknown field `{key}`"));
        }
        seen.push(key.clone());
    }
    Ok(parsed.0)
}

fn field_value<'a>(
    fields: &'a [(String, serde_json::Value)],
    key: &str,
) -> Result<&'a serde_json::Value, String> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .ok_or_else(|| format!("missing required field `{key}`"))
}

pub(crate) fn field_str(
    fields: &[(String, serde_json::Value)],
    key: &str,
) -> Result<String, String> {
    match field_value(fields, key)? {
        serde_json::Value::String(s) => Ok(s.clone()),
        _ => Err(format!("field `{key}` must be a string")),
    }
}

pub(crate) fn field_u64(fields: &[(String, serde_json::Value)], key: &str) -> Result<u64, String> {
    match field_value(fields, key)? {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| format!("field `{key}` must be a non-negative integer")),
        _ => Err(format!("field `{key}` must be a non-negative integer")),
    }
}

struct LinkInput {
    relation_id: String,
    src_id: String,
    src_revision: u64,
    dst_id: String,
    dst_revision: u64,
    kind: String,
    author: String,
    rationale: String,
    confirm: bool,
}

fn parse_link_input(bytes: &[u8]) -> Result<LinkInput, String> {
    let fields = strict_object(
        bytes,
        &[
            "relation_id",
            "src_id",
            "src_revision",
            "dst_id",
            "dst_revision",
            "kind",
            "author",
            "rationale",
            "confirm_state",
        ],
    )?;
    // The only expressible confirmation request is `"confirmed"`; anything
    // else is rejected exactly like an unknown value.
    let confirm = match fields.iter().find(|(k, _)| k == "confirm_state") {
        None => false,
        Some((_, serde_json::Value::String(s))) if s == "confirmed" => true,
        Some(_) => return Err("field `confirm_state` must be the string \"confirmed\"".to_string()),
    };
    let input = LinkInput {
        relation_id: field_str(&fields, "relation_id")?,
        src_id: field_str(&fields, "src_id")?,
        src_revision: field_u64(&fields, "src_revision")?,
        dst_id: field_str(&fields, "dst_id")?,
        dst_revision: field_u64(&fields, "dst_revision")?,
        kind: field_str(&fields, "kind")?,
        author: field_str(&fields, "author")?,
        rationale: field_str(&fields, "rationale")?,
        confirm,
    };
    knowledge::validate_relation_id(&input.relation_id)?;
    knowledge::validate_relation_kind(&input.kind)?;
    knowledge::validate_id(&input.src_id)?;
    knowledge::validate_id(&input.dst_id)?;
    if input.author.is_empty() || input.author.len() > knowledge::MAX_AUTHOR_BYTES {
        return Err(format!(
            "field `author` must be 1..={} bytes",
            knowledge::MAX_AUTHOR_BYTES
        ));
    }
    if input.rationale.is_empty() || input.rationale.len() > knowledge::MAX_RATIONALE_BYTES {
        return Err(format!(
            "field `rationale` must be 1..={} bytes",
            knowledge::MAX_RATIONALE_BYTES
        ));
    }
    Ok(input)
}

struct UnlinkInput {
    author: String,
    reason: String,
}

fn parse_unlink_input(bytes: &[u8]) -> Result<UnlinkInput, String> {
    let fields = strict_object(bytes, &["author", "reason"])?;
    let input = UnlinkInput {
        author: field_str(&fields, "author")?,
        reason: field_str(&fields, "reason")?,
    };
    if input.author.is_empty() || input.author.len() > knowledge::MAX_AUTHOR_BYTES {
        return Err(format!(
            "field `author` must be 1..={} bytes",
            knowledge::MAX_AUTHOR_BYTES
        ));
    }
    if input.reason.is_empty() || input.reason.len() > knowledge::MAX_RATIONALE_BYTES {
        return Err(format!(
            "field `reason` must be 1..={} bytes",
            knowledge::MAX_RATIONALE_BYTES
        ));
    }
    Ok(input)
}

/// `knowledge export`: canonical JSONL straight to stdout, or a Markdown
/// wrapper; with `--out` the file is written and the envelope reports it. A
/// genuinely absent store exports an empty metadata-only payload and creates
/// nothing, exactly like the other read commands.
fn knowledge_export(
    repo: &str,
    format: Option<&str>,
    out_path: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let format = match format {
        None | Some("jsonl") => knowledge_store::ExportFormat::Jsonl,
        Some("md") => knowledge_store::ExportFormat::Markdown,
        Some(other) => {
            err.push_str(&format!(
                "error: unknown export format '{other}' (jsonl or md)\n"
            ));
            return 2;
        }
    };
    let (_root, entry) = match knowledge_repo(repo, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let db = knowledge_store::knowledge_db_path(&paths::config_dir(), &entry.name);
    let (doc, generation): (knowledge_store::ExportDocument, u64) =
        match knowledge_store_absent(&db) {
            Ok(true) => (knowledge_store::empty_export(&entry.name, format), 0),
            Ok(false) => {
                let mut conn = match knowledge_store::open_knowledge(&entry, false) {
                    Ok(c) => c,
                    Err(e) => {
                        err.push_str(&format!("error: {e}\n"));
                        return 3;
                    }
                };
                // The export reads its generation from the same snapshot as
                // the payload, so the envelope's generation always describes
                // the exported data.
                let doc = match knowledge_store::export_document(&mut conn, format) {
                    Ok(d) => d,
                    Err(e) => {
                        err.push_str(&format!("error: {e}\n"));
                        return 3;
                    }
                };
                let generation = doc.generation;
                (doc, generation)
            }
            Err(e) => {
                err.push_str(&format!("error: {e}\n"));
                return 3;
            }
        };
    let Some(path) = out_path else {
        *out = doc.text;
        return 0;
    };
    if let Err(e) = std::fs::write(path, &doc.text) {
        err.push_str(&format!("error: cannot write export file '{path}': {e}\n"));
        return 3;
    }
    let format_name = match format {
        knowledge_store::ExportFormat::Jsonl => "jsonl",
        knowledge_store::ExportFormat::Markdown => "md",
    };
    let result = format!(
        "{{\"format\":\"{format_name}\",\"path\":\"{}\",\"records\":{},\"relations\":{},\"code_bindings\":{},\"bytes\":{}}}",
        json::escape(path),
        doc.records,
        doc.relations,
        doc.code_bindings,
        doc.text.len()
    );
    *out = response::knowledge_envelope(&entry.name, generation, "export", &result, None);
    0
}

/// `knowledge import`: extract and validate the payload before any store is
/// touched, then merge under one transaction. Malformed input is a
/// usage-class failure (2); corrupt coherence, conflicts, and storage
/// failures fail closed with 3.
fn knowledge_import(repo: &str, file: &str, out: &mut String, err: &mut String) -> i32 {
    let text = match read_import_file(file) {
        Ok(t) => t,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let payload_text = match knowledge_store::extract_import_payload(&text) {
        Ok(t) => t,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let payload = match knowledge_store::parse_import_payload(&payload_text) {
        Ok(p) => p,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return import_error_exit(&e);
        }
    };
    let (_root, mut conn) = match knowledge_open(repo, true, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let report = match knowledge_store::import_payload(&mut conn, &payload) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return import_error_exit(&e);
        }
    };
    let result = format!(
        "{{\"records_inserted\":{},\"revisions_appended\":{},\"relations_inserted\":{},\"code_bindings_inserted\":{},\"records_unchanged\":{},\"relations_unchanged\":{},\"code_bindings_unchanged\":{}}}",
        report.records_inserted,
        report.revisions_appended,
        report.relations_inserted,
        report.code_bindings_inserted,
        report.records_unchanged,
        report.relations_unchanged,
        report.code_bindings_unchanged
    );
    *out = response::knowledge_envelope(repo, report.generation, "import", &result, None);
    0
}

/// Resolve `--repo`, refuse an absent store, and open the live read
/// connection: the shared prologue of backup and verify, which operate on
/// the store itself rather than treating an absent store as empty.
fn knowledge_existing_store(
    repo: &str,
    err: &mut String,
) -> Result<(String, registry::RepoEntry, rusqlite::Connection), i32> {
    let (root, entry) = knowledge_repo(repo, err)?;
    let db = knowledge_store::knowledge_db_path(&paths::config_dir(), &entry.name);
    match knowledge_store_absent(&db) {
        Ok(true) => {
            err.push_str(&format!(
                "error: no knowledge store for repository '{repo}'\n"
            ));
            return Err(3);
        }
        Ok(false) => {}
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return Err(3);
        }
    }
    let conn = knowledge_open(repo, false, err)?;
    Ok((root, entry, conn.1))
}

/// `knowledge backup`: consistent snapshot of the live store plus a manifest
/// bound to the registered repository and canonical root. Read-only against
/// the store; the manifest describes the snapshot itself.
fn knowledge_backup(repo: &str, out_path: &str, out: &mut String, err: &mut String) -> i32 {
    let (root, entry, conn) = match knowledge_existing_store(repo, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let base = Path::new(out_path);
    let (manifest, generation) = match backup::backup_to(&conn, &entry, Path::new(&root), base) {
        Ok(v) => v,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let backup_paths = backup::backup_paths(base);
    let result = format!(
        "{{\"db\":\"{}\",\"manifest\":\"{}\",\"schema_version\":{},\"records\":{},\"relations\":{},\"code_bindings\":{}}}",
        json::escape(&backup_paths.db.to_string_lossy()),
        json::escape(&backup_paths.manifest.to_string_lossy()),
        manifest.schema_version,
        manifest.records,
        manifest.relations,
        manifest.code_bindings.unwrap_or(0)
    );
    *out = response::knowledge_envelope(&entry.name, generation, "backup", &result, None);
    0
}

/// `knowledge verify`: structural verification of the live store. Every
/// refusal carries the specific structural reason (exit 3).
fn knowledge_verify(repo: &str, out: &mut String, err: &mut String) -> i32 {
    let (root, entry, conn) = match knowledge_existing_store(repo, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let check = match knowledge_store::verify_store(&conn, &entry, Path::new(&root)) {
        Ok(c) => c,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let generation = match knowledge_store::read_generation(&conn) {
        Ok(g) => g,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let result = format!(
        "{{\"status\":\"ok\",\"schema_version\":{},\"records\":{},\"relations\":{},\"code_bindings\":{}}}",
        check.schema_version, check.records, check.relations, check.code_bindings
    );
    *out = response::knowledge_envelope(&entry.name, generation, "verify", &result, None);
    0
}

/// `knowledge restore`: offline restore of a backup into this repository's
/// store. Every refusal leaves the existing store untouched.
fn knowledge_restore(
    repo: &str,
    file: &str,
    overwrite: bool,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let (root, entry) = match knowledge_repo(repo, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    match backup::restore_from(
        &paths::config_dir(),
        &entry,
        Path::new(&root),
        Path::new(file),
        overwrite,
    ) {
        Ok(report) => {
            let db = knowledge_store::knowledge_db_path(&paths::config_dir(), &entry.name);
            let result = format!(
                "{{\"path\":\"{}\",\"schema_version\":{},\"records\":{},\"relations\":{},\"code_bindings\":{},\"overwrote\":{}}}",
                json::escape(&db.to_string_lossy()),
                report.schema_version,
                report.records,
                report.relations,
                report.code_bindings,
                report.overwrote
            );
            *out = response::knowledge_envelope(
                &entry.name,
                report.generation,
                "restore",
                &result,
                None,
            );
            0
        }
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            3
        }
    }
}

fn import_error_exit(message: &str) -> i32 {
    if message.starts_with("invalid import payload") {
        2
    } else {
        3
    }
}

/// Bounded read of an import file: regular file only, cap enforced on the
/// metadata first and again while reading so a growing file cannot slip past.
fn read_import_file(file: &str) -> Result<String, String> {
    let path = Path::new(file);
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read import file '{file}': {e}"))?;
    if !meta.is_file() {
        return Err(format!("import file is not a regular file: '{file}'"));
    }
    if meta.len() > knowledge_store::MAX_IMPORT_FILE_BYTES {
        return Err(format!(
            "invalid import payload: import file is too large: {} bytes, at most {} allowed",
            meta.len(),
            knowledge_store::MAX_IMPORT_FILE_BYTES
        ));
    }
    let handle =
        std::fs::File::open(path).map_err(|e| format!("cannot read import file '{file}': {e}"))?;
    let mut buf = Vec::new();
    use std::io::Read;
    handle
        .take(knowledge_store::MAX_IMPORT_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read import file '{file}': {e}"))?;
    if buf.len() as u64 > knowledge_store::MAX_IMPORT_FILE_BYTES {
        return Err(format!(
            "invalid import payload: import file is too large: over {} bytes allowed",
            knowledge_store::MAX_IMPORT_FILE_BYTES
        ));
    }
    String::from_utf8(buf)
        .map_err(|_| format!("invalid import payload: import file is not valid UTF-8: '{file}'"))
}

// --- subcommands -----------------------------------------------------------

fn knowledge_create(repo: &str, input: &str, out: &mut String, err: &mut String) -> i32 {
    let bytes = match knowledge_input(input) {
        Ok(b) => b,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let validated = match knowledge::validate_create_json(&bytes) {
        Ok(v) => v,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let id = validated.record.id.clone();
    let (root, mut conn) = match knowledge_open(repo, true, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let (revision, generation) = match knowledge_store::create_validated(&mut conn, &validated) {
        Ok(v) => v,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return knowledge_write_error(&e);
        }
    };
    let record = match knowledge_store::show_record(&conn, &id, Some(revision)) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    *out = response::knowledge_envelope(
        repo,
        generation,
        "create",
        &response::record_json(&record, Path::new(&root)),
        None,
    );
    0
}

fn knowledge_show(
    repo: &str,
    id: &str,
    revision: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let revision = match parse_optional_u64(revision, "revision", err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let (root, conn) = match knowledge_open(repo, false, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let (record, generation) = match knowledge_store::show_snapshot(&conn, id, revision) {
        Ok(pair) => pair,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    *out = response::knowledge_envelope(
        repo,
        generation,
        "show",
        &response::record_json(&record, Path::new(&root)),
        None,
    );
    0
}

fn knowledge_update(
    repo: &str,
    id: &str,
    expected: &str,
    input: &str,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let bytes = match knowledge_input(input) {
        Ok(b) => b,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let validated = match knowledge::validate_update_json(&bytes) {
        Ok(v) => v,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let expected_revision = match parse_count(Some(expected), 0, "expected-revision") {
        Ok(n) => match u64::try_from(n) {
            Ok(v) => v,
            Err(_) => {
                err.push_str("error: invalid --expected-revision value\n");
                return 2;
            }
        },
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    // A missing store is a storage error, and must not be created by a failed
    // update: check existence before opening read-write.
    if !knowledge_store::knowledge_db_path(&paths::config_dir(), repo).is_file() {
        err.push_str(&format!(
            "error: no knowledge store for repository '{repo}'\n"
        ));
        return 3;
    }
    let (root, mut conn) = match knowledge_open(repo, true, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let (revision, generation) =
        match knowledge_store::update_validated(&mut conn, id, expected_revision, &validated) {
            Ok(v) => v,
            Err(e) => {
                err.push_str(&format!("error: {e}\n"));
                return knowledge_write_error(&e);
            }
        };
    let record = match knowledge_store::show_record(&conn, id, Some(revision)) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    *out = response::knowledge_envelope(
        repo,
        generation,
        "update",
        &response::record_json(&record, Path::new(&root)),
        None,
    );
    0
}

fn knowledge_history(
    repo: &str,
    id: &str,
    limit: Option<&str>,
    offset: Option<&str>,
    generation: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let (limit, offset, generation) = match knowledge_page_args(limit, offset, generation, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let (root, conn) = match knowledge_open(repo, false, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let page = match knowledge_store::history(&conn, id, limit, offset, generation) {
        Ok(p) => p,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let revisions: Vec<String> = page
        .revisions
        .iter()
        .map(|r| response::record_json(r, Path::new(&root)))
        .collect();
    let result = format!(
        "{{\"id\":\"{}\",\"revisions\":[{}]}}",
        json::escape(id),
        revisions.join(",")
    );
    let end = offset.saturating_add(page.revisions.len());
    let next_offset = if end < page.total { Some(end) } else { None };
    let pagination =
        response::pagination_json(limit, offset, page.total, page.truncated, next_offset);
    *out =
        response::knowledge_envelope(repo, page.generation, "history", &result, Some(pagination));
    0
}

#[allow(clippy::too_many_arguments)]
fn knowledge_search(
    repo: &str,
    query: Option<&str>,
    path: Option<&str>,
    kind: Option<&str>,
    semantic: bool,
    include_archived: bool,
    include_superseded: bool,
    include_retracted: bool,
    limit: Option<&str>,
    offset: Option<&str>,
    generation: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    if semantic && query.is_none_or(|query| query.trim().is_empty()) {
        err.push_str("error: semantic search requires a nonempty text query\n");
        return 2;
    }
    if semantic && (include_archived || include_superseded || include_retracted) {
        err.push_str("error: semantic search only supports active records\n");
        return 2;
    }
    if query.is_none() && path.is_none() {
        err.push_str("error: search requires a text query or an evidence path\n");
        return 2;
    }
    if let Some(p) = path {
        if let Err(e) = knowledge::validate_search_path(p) {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    }
    if let Some(k) = kind {
        if let Err(e) = knowledge::validate_kind(k) {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    }
    if let Some(q) = query {
        if q.len() > knowledge::MAX_QUERY_TEXT_BYTES {
            err.push_str(&format!(
                "error: search query is {} bytes, at most {} allowed\n",
                q.len(),
                knowledge::MAX_QUERY_TEXT_BYTES
            ));
            return 2;
        }
    }
    // An empty or whitespace query beside a path is path-only: the result
    // echoes `query: null` instead of pretending text was searched.
    let query = query.unwrap_or("");
    let echoed_query = if path.is_some() && query.trim().is_empty() {
        None
    } else {
        Some(query)
    };
    let (limit, offset, generation) = match knowledge_page_args(limit, offset, generation, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    if semantic && offset != 0 {
        err.push_str("error: semantic search does not support a nonzero offset\n");
        return 2;
    }
    // Validate registration and root binding before answering: an unknown or
    // retargeted repository must not look like a store that happens to be
    // empty.
    let (root, entry) = match knowledge_repo(repo, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let db = knowledge_store::knowledge_db_path(&paths::config_dir(), repo);
    match knowledge_store_absent(&db) {
        // Genuinely absent store: empty success, and no database is created.
        // A nonzero generation expectation still conflicts against generation 0.
        Ok(true) => {
            if let Err(e) = knowledge_store::check_missing_store_generation(generation) {
                err.push_str(&format!("error: {e}\n"));
                return 3;
            }
            let result = if semantic {
                response::knowledge_semantic_search_result_json(
                    echoed_query,
                    path,
                    kind,
                    &[],
                    Path::new(&root),
                    Some("knowledge-store-missing"),
                )
            } else {
                response::knowledge_search_result_json(
                    echoed_query,
                    path,
                    kind,
                    include_archived,
                    &[],
                    Path::new(&root),
                )
            };
            *out = response::knowledge_envelope(
                repo,
                0,
                "search",
                &result,
                Some(response::pagination_json(limit, offset, 0, false, None)),
            );
            return 0;
        }
        Ok(false) => {}
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    }
    let mut conn = match knowledge_store::open_knowledge(&entry, false) {
        Ok(c) => c,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let model_dir = std::env::var_os("RILTRA_MODEL_DIR").map(std::path::PathBuf::from);
    let search = match knowledge_search::search(
        &mut conn,
        &paths::config_dir(),
        &entry,
        query,
        path,
        kind,
        include_archived,
        include_superseded,
        include_retracted,
        limit,
        offset,
        generation,
        semantic,
        model_dir.as_deref(),
    ) {
        Ok(search) => search,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let (search_result, generation, pagination) = response::knowledge_search_output(
        search,
        echoed_query,
        path,
        kind,
        include_archived,
        limit,
        offset,
        Path::new(&root),
    );
    *out =
        response::knowledge_envelope(repo, generation, "search", &search_result, Some(pagination));
    0
}

fn knowledge_link(repo: &str, input: &str, out: &mut String, err: &mut String) -> i32 {
    let bytes = match knowledge_input(input) {
        Ok(b) => b,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let link = match parse_link_input(&bytes) {
        Ok(l) => l,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let (_root, mut conn) = match knowledge_open(repo, true, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let generation = match knowledge_store::link(
        &mut conn,
        &link.relation_id,
        &link.src_id,
        link.src_revision,
        &link.dst_id,
        link.dst_revision,
        &link.kind,
        &link.author,
        &link.rationale,
        link.confirm,
    ) {
        Ok(g) => g,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let relation = match knowledge_store::relation_by_id(&conn, &link.relation_id) {
        Ok(r) => r,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    *out = response::knowledge_envelope(
        repo,
        generation,
        "link",
        &response::relation_json(&relation),
        None,
    );
    0
}

fn knowledge_unlink(
    repo: &str,
    relation: &str,
    input: &str,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let bytes = match knowledge_input(input) {
        Ok(b) => b,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    let unlink = match parse_unlink_input(&bytes) {
        Ok(u) => u,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 2;
        }
    };
    if let Err(e) = knowledge::validate_relation_reference(relation) {
        err.push_str(&format!("error: {e}\n"));
        return 2;
    }
    let (_root, mut conn) = match knowledge_open(repo, true, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let generation =
        match knowledge_store::unlink(&mut conn, relation, &unlink.author, &unlink.reason) {
            Ok(g) => g,
            Err(e) => {
                err.push_str(&format!("error: {e}\n"));
                return 3;
            }
        };
    // The retraction event is the result; endpoints stay retrievable through
    // `related --include-retracted`.
    let result = format!(
        "{{\"relation_id\":\"{}\",\"retracted\":true,\"retraction_author\":\"{}\",\"retraction_reason\":\"{}\"}}",
        json::escape(relation),
        json::escape(&unlink.author),
        json::escape(&unlink.reason)
    );
    *out = response::knowledge_envelope(repo, generation, "unlink", &result, None);
    0
}

#[allow(clippy::too_many_arguments)]
fn knowledge_related(
    repo: &str,
    id: &str,
    revision: Option<&str>,
    direction: &str,
    include_retracted: bool,
    limit: Option<&str>,
    offset: Option<&str>,
    generation: Option<&str>,
    out: &mut String,
    err: &mut String,
) -> i32 {
    let (limit, offset, generation) = match knowledge_page_args(limit, offset, generation, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let revision = match parse_optional_u64(revision, "revision", err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let dir = match direction {
        "outbound" => knowledge_store::RelationDirection::Outbound,
        "inbound" => knowledge_store::RelationDirection::Inbound,
        _ => {
            err.push_str(&format!(
                "error: invalid --direction value: '{direction}' (expected inbound or outbound)\n"
            ));
            return 2;
        }
    };
    let (_root, conn) = match knowledge_open(repo, false, err) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let page = match knowledge_store::related(
        &conn,
        id,
        revision,
        dir,
        include_retracted,
        limit,
        offset,
        generation,
    ) {
        Ok(p) => p,
        Err(e) => {
            err.push_str(&format!("error: {e}\n"));
            return 3;
        }
    };
    let result = response::knowledge_related_result_json(id, revision, direction, &page.relations);
    let pagination =
        response::pagination_json(limit, offset, page.total, page.truncated, page.next_offset);
    *out =
        response::knowledge_envelope(repo, page.generation, "related", &result, Some(pagination));
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = String::new();
    let mut err = String::new();
    let code = run(&args, &mut out, &mut err);
    if !out.is_empty() {
        print!("{out}");
    }
    if !err.is_empty() {
        eprint!("{err}");
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(args: &[&str]) -> (i32, String, String) {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut out = String::new();
        let mut err = String::new();
        let code = run(&owned, &mut out, &mut err);
        (code, out, err)
    }

    #[test]
    fn help_lists_status() {
        let (code, out, _) = exec(&["--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("status"), "help must list status");
    }

    #[test]
    fn version_matches_package() {
        let (code, out, _) = exec(&["--version"]);
        assert_eq!(code, 0);
        assert_eq!(out, format!("{NAME} {VERSION}\n"));
    }

    #[test]
    fn status_json_is_deterministic() {
        let (code, out, _) = exec(&["status"]);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            format!(
                "{{\"name\":\"{NAME}\",\"version\":\"{VERSION}\",\"scope\":\"{SCOPE}\",\"networking\":\"disabled\"}}\n"
            )
        );
    }

    #[test]
    fn unknown_args_exit_2() {
        let (code, _, err) = exec(&["index"]);
        assert_eq!(code, 2);
        assert!(err.contains("unknown arguments"));
    }
}
