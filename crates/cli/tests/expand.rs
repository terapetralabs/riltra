// SPDX-License-Identifier: Apache-2.0
//! PR 9c integration tests: `query --expand` and the MCP `expand` input, one
//! hop along proven edges from the lexical results. The SCIP documents are
//! hand-built over a small Python repository; no external indexer runs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(40_000);

const LIB: &str =
    "def target():\n    return 1\n\ndef lonely():\n    return 2\n\ndef solo():\n    return 3\n";
const MAIN: &str = "from lib import target\n\ndef caller():\n    return target()\n\nalias = solo\n";
/// A same-named decoy: `relay` calls `target` by name only. Its name call is
/// syntax-only and no SCIP occurrence proves it; the test asserts the absence
/// of any row, not the POTENTIAL_CALL row itself (not queried here).
const RELAY: &str = "def relay():\n    return target()\n";
const OTHER: &str = "def target():\n    return 9\n";
const WIDGET: &str = "class Widget:\n    pass\n";

const MCP_INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"riltra-expand-test","version":"0.0.0"}}}"#;
const MCP_INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-expand-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn run(home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_riltra"))
        .args(args)
        .env("RILTRA_HOME", home)
        .env_remove("RILTRA_MODEL_DIR")
        .output()
        .expect("spawn riltra");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn repo_with(home: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = home.join(name);
    for (rel, content) in files {
        let p = d.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
    }
    let (code, _, err) = run(
        home,
        &["init", "--repo", &d.to_string_lossy(), "--name", name],
    );
    assert_eq!(code, 0, "init failed: {err}");
    d
}

fn do_index(home: &Path) {
    let (code, _, err) = run(home, &["index", "--repo", "sem"]);
    assert_eq!(code, 0, "index failed: {err}");
}

// Minimal proto3 writer for exactly the fields the importer reads (field
// numbers pinned by scip.proto; the same shape as tests/edges.rs).
fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn tag(field: u32, wire: u32, out: &mut Vec<u8>) {
    varint(((field << 3) | wire) as u64, out);
}

fn len_field(field: u32, bytes: &[u8], out: &mut Vec<u8>) {
    tag(field, 2, out);
    varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn str_field(field: u32, value: &str, out: &mut Vec<u8>) {
    len_field(field, value.as_bytes(), out);
}

fn varint_field(field: u32, value: u64, out: &mut Vec<u8>) {
    tag(field, 0, out);
    varint(value, out);
}

fn occurrence(symbol: &str, roles: u64, line: i64, start: i64, end: i64) -> Vec<u8> {
    let mut r = Vec::new();
    varint_field(1, line as u64, &mut r);
    varint_field(2, start as u64, &mut r);
    varint_field(3, end as u64, &mut r);
    let mut o = Vec::new();
    str_field(2, symbol, &mut o);
    varint_field(3, roles, &mut o);
    len_field(8, &r, &mut o);
    o
}

/// The SCIP symbol of `name` defined in `file`.
fn sym(file: &str, name: &str) -> String {
    format!("scip-test test test-pkg 1.0.0 {file}/{name}().")
}

/// One SCIP document: its path, its occurrences `(symbol, roles, line, start,
/// end)` and its symbol table `(symbol, display name)`.
fn scip_document(
    path: &str,
    occurrences: &[(&str, u64, i64, i64, i64)],
    symbols: &[(&str, &str)],
) -> Vec<u8> {
    let mut doc = Vec::new();
    str_field(1, path, &mut doc);
    for &(symbol, roles, line, start, end) in occurrences {
        len_field(2, &occurrence(symbol, roles, line, start, end), &mut doc);
    }
    for &(symbol, name) in symbols {
        let mut info = Vec::new();
        str_field(1, symbol, &mut info);
        str_field(6, name, &mut info);
        len_field(3, &info, &mut doc);
    }
    varint_field(6, 1, &mut doc);
    doc
}

/// Writes one SCIP index of `docs` under the home (never into the repository)
/// and returns its path. The project root is the repository directory.
fn scip_file(h: &Path, name: &str, dir: &Path, docs: &[Vec<u8>]) -> String {
    let mut md = Vec::new();
    let mut tool = Vec::new();
    str_field(1, "scip-test", &mut tool);
    str_field(2, "1.0", &mut tool);
    len_field(2, &tool, &mut md);
    str_field(
        3,
        &format!("file://{}", dir.to_string_lossy().replace('\\', "/")),
        &mut md,
    );
    varint_field(4, 1, &mut md);
    let mut out = Vec::new();
    len_field(1, &md, &mut out);
    for doc in docs {
        len_field(2, doc, &mut out);
    }
    let scratch = h.join("scip");
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join(name);
    std::fs::write(&file, &out).unwrap();
    file.to_string_lossy().to_string()
}

fn import(h: &Path, scip: &str) {
    let (code, _, err) = run(h, &["semantic", "import", "--repo", "sem", "--scip", scip]);
    assert_eq!(code, 0, "{err}");
}

fn lib_doc() -> Vec<u8> {
    scip_document(
        "lib.py",
        &[
            (&sym("lib.py", "target"), 0x1, 0, 4, 10),
            (&sym("lib.py", "lonely"), 0x1, 3, 4, 10),
            (&sym("lib.py", "solo"), 0x1, 6, 4, 8),
        ],
        &[
            (&sym("lib.py", "target"), "target"),
            (&sym("lib.py", "lonely"), "lonely"),
            (&sym("lib.py", "solo"), "solo"),
        ],
    )
}

/// `caller` (line 3) calls `target` (inside the body), and the module-level
/// `alias = solo` is a reference with no enclosing callable.
fn main_doc() -> Vec<u8> {
    scip_document(
        "main.py",
        &[
            (&sym("main.py", "caller"), 0x1, 2, 4, 10),
            (&sym("lib.py", "target"), 0x0, 3, 11, 17),
            (&sym("lib.py", "solo"), 0x0, 5, 8, 12),
        ],
        &[(&sym("main.py", "caller"), "caller")],
    )
}

/// The repository used by the integration tests, with the unindexed documents
/// `notes.other` and `notes2.other` that the import also covers.
fn s_repo(h: &Path) -> PathBuf {
    repo_with(
        h,
        "sem",
        &[
            ("lib.py", LIB),
            ("main.py", MAIN),
            ("relay.py", RELAY),
            ("other.py", OTHER),
            ("widget.py", WIDGET),
            ("notes.other", "first notes\n"),
            ("notes2.other", "second notes\n"),
        ],
    )
}

/// An unindexed document with one definition fact. The import stores facts per
/// document, so a document without occurrences would not be checked at all.
fn notes_doc(path: &str) -> Vec<u8> {
    let memo = sym(path, "memo");
    scip_document(path, &[(&memo, 0x1, 0, 0, 4)], &[(&memo, "memo")])
}

/// Index, then import the fixture SCIP covering the repository's indexed files
/// and `notes.other`.
fn scip_imported(h: &Path, dir: &Path) {
    do_index(h);
    let notes = notes_doc("notes.other");
    let scip = scip_file(h, "index.scip", dir, &[lib_doc(), main_doc(), notes]);
    import(h, &scip);
}

fn query_doc(h: &Path, text: &str, expand: bool) -> String {
    let mut args = vec!["query", "--repo", "sem", "--text", text];
    if expand {
        args.push("--expand");
    }
    let (code, out, err) = run(h, &args);
    assert_eq!(code, 0, "{err}");
    out
}

fn query_value(h: &Path, text: &str, expand: bool) -> Value {
    let doc = query_doc(h, text, expand);
    serde_json::from_str(doc.trim_end()).unwrap_or_else(|e| panic!("json: {e}: {doc}"))
}

/// The raw `"results"` member of a query document, for byte comparison.
fn results_slice(doc: &str) -> &str {
    let start = doc.find("\"results\":").expect("results key");
    let end = doc.find(",\"total\":").expect("total key");
    &doc[start..end]
}

/// One `riltra_code_query` call on a fresh `riltra mcp` process. Returns the
/// JSON-RPC response with id 2.
fn mcp_query(h: &Path, arguments: &Value) -> Value {
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "riltra_code_query", "arguments": arguments}
    });
    let input = format!("{MCP_INIT}\n{MCP_INITIALIZED}\n{call}\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_riltra"))
        .arg("mcp")
        .env("RILTRA_HOME", h)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn riltra mcp");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().expect("wait riltra mcp");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["id"] == 2)
        .unwrap_or_else(|| panic!("no response to the query: {stdout}"))
}

fn mcp_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no content text: {response}"))
        .to_string()
}

/// The CLI and MCP documents of one query are the same bytes; returns them.
fn assert_parity(h: &Path, text: &str) -> String {
    let (code, cli, err) = run(h, &["query", "--repo", "sem", "--text", text, "--expand"]);
    assert_eq!(code, 0, "{err}");
    let mcp = mcp_text(&mcp_query(
        h,
        &json!({"repo": "sem", "text": text, "expand": true}),
    ));
    assert_eq!(cli.trim_end_matches('\n'), mcp, "CLI and MCP bytes differ");
    mcp
}

fn expansion(doc: &Value) -> &Value {
    &doc["expansion"]
}

fn expected_row(path: &str, line: u64, name: &str, via: Value) -> Value {
    json!({
        "path": path,
        "line": line,
        "name": name,
        "kind": "function",
        "via": via
    })
}

fn via(path: &str, line: u64, name: &str, edge: &str, direction: &str) -> Value {
    json!({
        "seed": {"path": path, "line": line, "name": name},
        "edge": edge,
        "direction": direction,
        "precision": "Exact",
        "evidence_label": "Unverified"
    })
}

// ---- follows proven edges ----------------------------------------------------

#[test]
fn expand_follows_proven_calls_in_both_directions() {
    let h = home("both");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    // The seed `caller` calls `target`: the callee row.
    let callee = query_value(&h, "caller", true);
    assert_eq!(expansion(&callee)["status"], "available", "{callee}");
    assert_eq!(
        expansion(&callee)["rows"],
        json!([expected_row(
            "lib.py",
            1,
            "target",
            via("main.py", 3, "caller", "CALLS", "callee")
        )]),
        "{callee}"
    );
    // The seed `target` is called by `caller`: the caller row.
    let caller = query_value(&h, "target", true);
    assert_eq!(expansion(&caller)["status"], "available", "{caller}");
    assert_eq!(
        expansion(&caller)["rows"],
        json!([expected_row(
            "main.py",
            3,
            "caller",
            via("lib.py", 1, "target", "CALLS", "caller")
        )]),
        "{caller}"
    );
}

#[test]
fn expand_never_follows_name_matched_candidates() {
    let h = home("decoy");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    // `relay` reaches `target` only by name (POTENTIAL_CALL): the decoy in
    // other.py must not appear, and nothing proven leaves `relay`.
    // Control: the store really holds the name-matched edge, so an empty
    // expansion proves it was ignored, not that it never existed.
    let (code, callees, err) = run(
        &h,
        &[
            "callees", "--repo", "sem", "--path", "relay.py", "--line", "1", "--col", "5",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(callees.contains("\"kind\":\"POTENTIAL_CALL\""), "{callees}");
    assert!(!callees.contains("\"kind\":\"CALLS\""), "{callees}");
    let doc = query_value(&h, "relay", true);
    assert_eq!(expansion(&doc)["status"], "no-eligible-edge", "{doc}");
    assert_eq!(expansion(&doc)["rows"], json!([]), "{doc}");
    assert!(!query_doc(&h, "relay", true).contains("other.py"));
}

// ---- statuses ----------------------------------------------------------------

#[test]
fn expand_reports_none_imported_no_seed_and_no_index() {
    let h = home("statuses");
    let dir = s_repo(&h);
    do_index(&h);
    let none = query_value(&h, "caller", true);
    assert_eq!(expansion(&none)["status"], "none-imported", "{none}");
    assert_eq!(expansion(&none)["seeds"]["considered"], 1, "{none}");
    assert_eq!(expansion(&none)["seeds"]["items"], json!([]), "{none}");
    scip_imported(&h, &dir);
    let empty = query_value(&h, "nothingmatches", true);
    assert_eq!(expansion(&empty)["status"], "no-seed", "{empty}");
    assert_eq!(expansion(&empty)["seeds"]["considered"], 0, "{empty}");

    let bare = home("no-index");
    s_repo(&bare);
    let (code, out, err) = run(
        &bare,
        &["query", "--repo", "sem", "--text", "caller", "--expand"],
    );
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(doc["reason"], "no-index", "{doc}");
    assert_eq!(expansion(&doc)["status"], "none-imported", "{doc}");
    assert_eq!(expansion(&doc)["seeds"]["considered"], 0, "{doc}");
    assert_eq!(expansion(&doc)["rows"], json!([]), "{doc}");
}

#[test]
fn expand_is_stale_after_a_source_edit_and_adds_no_row() {
    let h = home("stale");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    let before = query_doc(&h, "caller", true);
    assert_eq!(
        expansion(&serde_json::from_str(before.trim_end()).unwrap())["status"],
        "available"
    );
    std::fs::write(dir.join("lib.py"), LIB.replace("return 1", "return 7")).unwrap();
    let after = query_doc(&h, "caller", true);
    let doc: Value = serde_json::from_str(after.trim_end()).unwrap();
    assert_eq!(doc["freshness"], "stale", "{doc}");
    assert_eq!(expansion(&doc)["status"], "stale", "{doc}");
    assert_eq!(
        expansion(&doc)["reasons"],
        json!(["source-not-fresh"]),
        "{doc}"
    );
    assert_eq!(expansion(&doc)["rows"], json!([]), "{doc}");
    assert_eq!(results_slice(&before), results_slice(&after));
}

#[test]
fn expand_skips_seeds_that_cannot_be_mapped_and_says_why() {
    let h = home("skips");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    // A class is never callable: all seeds skipped is `no-seed`.
    let only_class = query_value(&h, "widget", true);
    assert_eq!(expansion(&only_class)["status"], "no-seed", "{only_class}");
    assert_eq!(
        expansion(&only_class)["seeds"]["skipped"],
        1,
        "{only_class}"
    );
    assert_eq!(
        expansion(&only_class)["seeds"]["items"][0]["skipped"],
        "not-callable",
        "{only_class}"
    );
    // Mixed: the callable seed still expands.
    let mixed = query_value(&h, "widget caller", true);
    let items = expansion(&mixed)["seeds"]["items"]
        .as_array()
        .unwrap()
        .clone();
    let widget = items
        .iter()
        .find(|i| i["name"] == "Widget")
        .expect("Widget seed");
    assert_eq!(widget["skipped"], "not-callable", "{mixed}");
    assert_eq!(widget["scip"], false, "{mixed}");
    let caller = items
        .iter()
        .find(|i| i["name"] == "caller")
        .expect("caller seed");
    assert!(caller["skipped"].is_null(), "{mixed}");
    assert_eq!(expansion(&mixed)["status"], "available", "{mixed}");
    assert_eq!(expansion(&mixed)["seeds"]["mapped"], 1, "{mixed}");
    assert_eq!(expansion(&mixed)["seeds"]["skipped"], 1, "{mixed}");
    assert_eq!(expansion(&mixed)["rows"][0]["name"], "target", "{mixed}");
}

// ---- edges that add nothing --------------------------------------------------

#[test]
fn expand_references_add_no_row_and_are_counted() {
    let h = home("refs");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    // `solo` is only referenced at module level: no caller, so no row, and the
    // relation is counted as unmapped.
    let doc = query_value(&h, "solo", true);
    assert_eq!(expansion(&doc)["status"], "no-eligible-edge", "{doc}");
    assert_eq!(expansion(&doc)["rows"], json!([]), "{doc}");
    let item = &expansion(&doc)["seeds"]["items"][0];
    assert_eq!(item["scip"], true, "{doc}");
    assert_eq!(item["eligible"], 0, "{doc}");
    assert_eq!(item["unmapped"], 1, "{doc}");
}

#[test]
fn expand_deduplicates_and_never_repeats_a_result() {
    let h = home("dedup");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    // Both ends of the proven call are already results: nothing is added.
    let doc = query_value(&h, "caller target", true);
    assert_eq!(expansion(&doc)["status"], "no-eligible-edge", "{doc}");
    assert_eq!(expansion(&doc)["added"], 0, "{doc}");
    assert_eq!(expansion(&doc)["rows"], json!([]), "{doc}");
    let caller = expansion(&doc)["seeds"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "caller")
        .expect("caller seed")
        .clone();
    assert_eq!(caller["eligible"], 0, "{doc}");
}

#[test]
fn expand_limits_are_reported_with_the_defaults() {
    let h = home("limits");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    let raw = query_doc(&h, "caller", true);
    assert!(
        raw.contains(r#""limits":{"seeds":5,"per_seed":5,"total":25,"edge_read":200}"#),
        "{raw}"
    );
}

// ---- default output and parity -----------------------------------------------

#[test]
fn query_without_expand_is_byte_identical_and_expand_appends_after_basis() {
    let h = home("plain");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    let plain = query_doc(&h, "caller", false);
    assert!(!plain.contains("expansion"), "{plain}");
    let with = query_doc(&h, "caller", true);
    let prefix = &plain[..plain.len() - "}\n".len()];
    assert!(
        with.starts_with(prefix) && with[prefix.len()..].starts_with(",\"expansion\":{"),
        "expansion must be appended after basis:\n{plain}\n{with}"
    );
    let absent = mcp_text(&mcp_query(&h, &json!({"repo": "sem", "text": "caller"})));
    let off = mcp_text(&mcp_query(
        &h,
        &json!({"repo": "sem", "text": "caller", "expand": false}),
    ));
    assert_eq!(absent, off);
    assert_eq!(absent, plain.trim_end_matches('\n'));
}

#[test]
fn cli_and_mcp_expand_bytes_are_equal_for_every_status() {
    let available = home("parity-avail");
    let dir = s_repo(&available);
    scip_imported(&available, &dir);
    let doc = assert_parity(&available, "caller");
    assert!(doc.contains("\"status\":\"available\""), "{doc}");

    std::fs::write(dir.join("lib.py"), LIB.replace("return 1", "return 7")).unwrap();
    let stale = assert_parity(&available, "caller");
    assert!(stale.contains("\"status\":\"stale\""), "{stale}");

    let none = home("parity-none");
    s_repo(&none);
    do_index(&none);
    let imported_later = assert_parity(&none, "caller");
    assert!(
        imported_later.contains("\"status\":\"none-imported\""),
        "{imported_later}"
    );

    let bare = home("parity-noindex");
    s_repo(&bare);
    let no_index = assert_parity(&bare, "caller");
    assert!(no_index.contains("\"reason\":\"no-index\""), "{no_index}");
}

// ---- refusals and writes -----------------------------------------------------

#[test]
fn expand_with_an_outdated_or_unbuilt_index_is_refused_exactly_like_query() {
    let unbuilt = home("refuse-unbuilt");
    let dir = s_repo(&unbuilt);
    scip_imported(&unbuilt, &dir);
    let db = unbuilt.join("index").join("sem.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DELETE FROM concept_fts", []).unwrap();
    drop(conn);
    let plain = run(&unbuilt, &["query", "--repo", "sem", "--text", "caller"]);
    let expand = run(
        &unbuilt,
        &["query", "--repo", "sem", "--text", "caller", "--expand"],
    );
    assert_eq!(plain.0, 3, "{}", plain.2);
    assert_eq!(expand, plain);

    let outdated = home("refuse-outdated");
    let dir = s_repo(&outdated);
    scip_imported(&outdated, &dir);
    let db = outdated.join("index").join("sem.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA user_version = 4").unwrap();
    drop(conn);
    let plain = run(&outdated, &["query", "--repo", "sem", "--text", "caller"]);
    let expand = run(
        &outdated,
        &["query", "--repo", "sem", "--text", "caller", "--expand"],
    );
    assert_eq!(plain.0, 3, "{}", plain.2);
    assert_eq!(expand, plain);
}

#[test]
fn expand_never_writes() {
    let h = home("writes");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    let db = h.join("index").join("sem.db");
    // The database is WAL: a write may sit in the `-wal` file, not the main one.
    // Any read connection may create an empty `-wal`, so the baseline is taken
    // after a plain query: only content or length changes count.
    let wal = h.join("index").join("sem.db-wal");
    let snapshot = || (std::fs::read(&db).unwrap(), std::fs::read(&wal).ok());
    query_doc(&h, "caller", false);
    let before = snapshot();
    query_doc(&h, "caller", true);
    query_doc(&h, "target", true);
    let after = snapshot();
    assert!(
        before.0 == after.0,
        "--expand must not change the main database file"
    );
    assert_eq!(before.1, after.1, "--expand must not change the WAL");
}

#[test]
fn cli_and_mcp_report_an_expansion_read_failure_as_a_store_error() {
    let h = home("read-failure");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    let db = h.join("index").join("sem.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DROP TABLE code_edges", []).unwrap();
    drop(conn);
    // The lexical query does not read the edge table, so it still answers.
    let (plain_code, plain_out, plain_err) =
        run(&h, &["query", "--repo", "sem", "--text", "caller"]);
    assert_eq!(plain_code, 0, "{plain_err}");
    assert!(plain_out.contains("\"results\""), "{plain_out}");
    let (code, out, err) = run(
        &h,
        &["query", "--repo", "sem", "--text", "caller", "--expand"],
    );
    assert_eq!(code, 3, "{out}{err}");
    assert!(out.is_empty(), "no lexical document on stdout: {out}");
    assert!(err.starts_with("error: "), "{err}");
    let response = mcp_query(
        &h,
        &json!({"repo": "sem", "text": "caller", "expand": true}),
    );
    assert_eq!(response["result"]["isError"], true, "{response}");
    let doc: Value = serde_json::from_str(&mcp_text(&response)).unwrap();
    assert_eq!(doc["kind"], "store_error", "{doc}");
}

// ---- staleness of the imports under the seeds (S1-S3) -----------------------

#[test]
fn s1_expand_is_stale_when_a_seeds_import_document_changed_and_no_edge_exists() {
    let h = home("s1");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    let before = query_value(&h, "lonely", true);
    assert_eq!(before["freshness"], "fresh", "{before}");
    assert_eq!(expansion(&before)["status"], "no-eligible-edge", "{before}");
    std::fs::write(dir.join("notes.other"), "first notes, changed\n").unwrap();
    let after = query_value(&h, "lonely", true);
    assert_eq!(
        after["freshness"], "fresh",
        "the snapshot check is blind here: {after}"
    );
    assert_eq!(expansion(&after)["status"], "stale", "{after}");
    assert_eq!(
        expansion(&after)["reasons"],
        json!(["import-stale"]),
        "{after}"
    );
    assert_eq!(expansion(&after)["rows"], json!([]), "{after}");
    assert_eq!(before["results"], after["results"]);
}

#[test]
fn s2_expand_is_stale_when_the_seeds_only_relation_is_an_unmapped_reference() {
    let h = home("s2");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    let before = query_value(&h, "solo", true);
    assert_eq!(before["freshness"], "fresh", "{before}");
    assert_eq!(expansion(&before)["status"], "no-eligible-edge", "{before}");
    assert_eq!(
        expansion(&before)["seeds"]["items"][0]["unmapped"],
        1,
        "{before}"
    );
    std::fs::write(dir.join("notes.other"), "first notes, changed\n").unwrap();
    let after = query_value(&h, "solo", true);
    assert_eq!(after["freshness"], "fresh", "{after}");
    assert_eq!(expansion(&after)["status"], "stale", "{after}");
    assert_eq!(
        expansion(&after)["reasons"],
        json!(["import-stale"]),
        "{after}"
    );
}

#[test]
fn s3_expand_does_not_check_imports_unrelated_to_any_seed() {
    let h = home("s3");
    let dir = s_repo(&h);
    scip_imported(&h, &dir);
    // A second import that covers only `notes2.other` and defines no seed.
    let unrelated = notes_doc("notes2.other");
    let scip = scip_file(&h, "unrelated.scip", &dir, &[unrelated]);
    import(&h, &scip);
    let before = query_value(&h, "lonely", true);
    assert_eq!(before["freshness"], "fresh", "{before}");
    assert_eq!(expansion(&before)["status"], "no-eligible-edge", "{before}");
    std::fs::write(dir.join("notes2.other"), "second notes, changed\n").unwrap();
    let after = query_value(&h, "lonely", true);
    assert_eq!(after["freshness"], "fresh", "{after}");
    assert_eq!(expansion(&after)["status"], "no-eligible-edge", "{after}");
}
