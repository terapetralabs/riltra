// SPDX-License-Identifier: Apache-2.0
//! M6 graph lifecycle CLI tests (Task 3): import rollback, incremental
//! modes, and re-import after rebuild. Edge *content* is asserted in
//! `src/edges.rs` unit tests (in-crate store access); these tests pin the
//! CLI-observable lifecycle and Task 4 callers/callees evidence contract.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "common/mock_lsp.rs"]
mod mock_lsp;
#[path = "common/rust_alias.rs"]
mod rust_alias;

static COUNTER: AtomicU64 = AtomicU64::new(20_000);

#[path = "m6_corpus/mod.rs"]
mod task7;
#[path = "m6_corpus/upgrade.rs"]
mod task7_upgrade;

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-edges-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
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

fn run_stdin(home: &Path, args: &[&str], input: &[u8]) -> (i32, String, String) {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_riltra"))
        .args(args)
        .env("RILTRA_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn riltra");
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().expect("wait riltra");
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

fn do_index(home: &Path) -> String {
    let (code, out, err) = run(home, &["index", "--repo", "sem"]);
    assert_eq!(code, 0, "index failed: {err}");
    out
}

fn assert_json_keys_are_sorted(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let keys: Vec<_> = object.keys().map(String::as_str).collect();
            assert!(
                keys.windows(2).all(|pair| pair[0] <= pair[1]),
                "JSON object keys are not sorted: {keys:?}"
            );
            for value in object.values() {
                assert_json_keys_are_sorted(value);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_json_keys_are_sorted(item);
            }
        }
        _ => {}
    }
}

#[test]
fn r1_context_reports_scoped_impact_and_unavailable_knowledge() {
    let h = home("context-chain");
    let lines = trace_fixture(&h, &[vec![1], vec![2], vec![]]);
    let anchor_line = lines[2].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &anchor_line,
            "--col",
            "6",
            "--depth",
            "3",
            "--limit",
            "20",
            "--query",
            "why is this called",
            "--semantic",
        ],
    );
    assert_eq!(code, 0, "context failed: {err}");
    let value: serde_json::Value = serde_json::from_str(&out).expect("context JSON");
    assert_json_keys_are_sorted(&value);
    assert_eq!(value["operation"], "context");
    assert_eq!(value["distance_scope"], "inspected_subgraph");
    assert_eq!(value["impact"]["verdict"], "HIGH");
    assert_eq!(value["query"]["status"], "unavailable");
    assert_eq!(value["query"]["reason"], "knowledge-store-missing");
    assert_eq!(value["semantic"]["status"], "unavailable");
    assert_eq!(value["knowledge"]["status"], "unavailable");
    assert!(value["knowledge"]["snapshot"]["generation"].is_null());
    assert!(value["knowledge"]["snapshot"]["schema_version"].is_null());
    assert!(value["knowledge"]["bindings"]["total"].is_null());
    assert!(value["knowledge"]["bindings"]["schema_version"].is_null());
    assert_eq!(value["knowledge"]["bindings"]["total_exact"], false);
    assert!(value["knowledge"]["bindings"]["truncated"].is_null());
    let mut distances: Vec<usize> = value["graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["distance"].as_u64().unwrap() as usize)
        .collect();
    distances.sort_unstable();
    assert_eq!(distances, [0, 1, 2]);
    let (repeat_code, repeat, repeat_err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &anchor_line,
            "--col",
            "6",
            "--depth",
            "3",
            "--limit",
            "20",
            "--query",
            "why is this called",
            "--semantic",
        ],
    );
    assert_eq!(repeat_code, 0, "{repeat_err}");
    assert_eq!(out, repeat, "context JSON ordering must be deterministic");

    let source = h.join("sem").join("graph.py");
    let mut contents = std::fs::read_to_string(&source).unwrap();
    contents.push_str("# changed after indexing\n");
    std::fs::write(&source, contents).unwrap();
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &anchor_line,
            "--col",
            "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let stale: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(stale["freshness"]["status"], "stale", "{stale}");
    assert_eq!(stale["impact"]["verdict"], "UNKNOWN", "{stale}");
    assert!(
        stale["graph"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|edge| edge["verification"] == "Stale"),
        "{stale}"
    );
}

#[test]
fn r1_context_empty_graph_keeps_anchor_and_unknown_coverage() {
    let h = home("context-empty");
    let lines = trace_fixture(&h, &[vec![]]);
    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["impact"]["verdict"], "UNKNOWN", "{value}");
    assert_eq!(value["impact"]["completeness"]["status"], "partial");
    assert_eq!(value["graph"]["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(value["graph"]["nodes"][0]["distance"], 0);
    assert_eq!(value["graph"]["edges"], serde_json::json!([]));
    assert_eq!(value["knowledge"]["status"], "unavailable");
    assert_eq!(value["knowledge"]["reason"], "knowledge-store-missing");
    assert_eq!(value["knowledge"]["bindings"]["status"], "unavailable");
}

#[test]
fn r3_context_keeps_explicit_bindings_separate_from_unlinked_matches() {
    let h = home("r3-context-why");
    let lines = trace_fixture(&h, &[vec![], vec![]]);
    let create_bound = br#"{"id":"bound","kind":"decision","title":"Read boundary","body":"keep the store local","status":"active","author":"a","evidence":[]}"#;
    assert_eq!(
        run_stdin(
            &h,
            &["knowledge", "create", "--repo", "sem", "--input", "-"],
            create_bound
        )
        .0,
        0
    );
    let create_candidate = br#"{"id":"candidate","kind":"decision","title":"SQLite migration","body":"local database migration","status":"active","author":"a","evidence":[]}"#;
    assert_eq!(
        run_stdin(
            &h,
            &["knowledge", "create", "--repo", "sem", "--input", "-"],
            create_candidate
        )
        .0,
        0
    );
    let create_bound2 = br#"{"id":"bound2","kind":"decision","title":"Read budget","body":"preserve the query limit","status":"active","author":"a","evidence":[]}"#;
    assert_eq!(
        run_stdin(
            &h,
            &["knowledge", "create", "--repo", "sem", "--input", "-"],
            create_bound2
        )
        .0,
        0
    );
    let binding = br#"{"record_id":"bound","record_revision":1,"relation":"constrains","author":"reviewer","rationale":"This decision constrains the code anchor.","provenance":"review"}"#;
    let (code, _, err) = run_stdin(
        &h,
        &[
            "knowledge",
            "bind",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &lines[0].to_string(),
            "--col",
            "6",
            "--input",
            "-",
        ],
        binding,
    );
    assert_eq!(code, 0, "bind failed: {err}");
    let binding2 = br#"{"record_id":"bound2","record_revision":1,"relation":"documents","author":"reviewer","rationale":"This second explicit link is preserved when limited.","provenance":"review"}"#;
    let (code, _, err) = run_stdin(
        &h,
        &[
            "knowledge",
            "bind",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &lines[0].to_string(),
            "--col",
            "6",
            "--input",
            "-",
        ],
        binding2,
    );
    assert_eq!(code, 0, "second bind failed: {err}");
    let knowledge_store = h.join("knowledge").join("sem.db");
    let knowledge_before = std::fs::read(&knowledge_store).unwrap();

    let context = |line: usize| {
        let line = line.to_string();
        let (code, out, err) = run(
            &h,
            &[
                "context",
                "--repo",
                "sem",
                "--path",
                "graph.py",
                "--line",
                &line,
                "--col",
                "6",
                "--query",
                "database migration",
            ],
        );
        assert_eq!(code, 0, "context failed: {err}");
        serde_json::from_str::<serde_json::Value>(&out).unwrap()
    };

    let linked = context(lines[0]);
    assert_eq!(
        linked["knowledge"]["bindings"]["status"], "linked",
        "{linked}"
    );
    assert_eq!(
        linked["knowledge"]["bindings"]["bindings"][0]["record_id"],
        "bound"
    );
    assert_eq!(linked["query"]["hits"][0]["id"], "candidate", "{linked}");
    assert_eq!(
        linked["query"]["hits"][0]["relevance"], "candidate",
        "{linked}"
    );

    let unlinked = context(lines[1]);
    assert_eq!(
        unlinked["knowledge"]["bindings"]["status"], "no-link-proven",
        "{unlinked}"
    );
    assert_eq!(
        unlinked["knowledge"]["bindings"]["bindings"],
        serde_json::json!([])
    );
    assert_eq!(
        unlinked["query"]["hits"][0]["id"], "candidate",
        "{unlinked}"
    );

    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &line,
            "--col",
            "6",
            "--query",
            "database migration",
            "--limit",
            "1",
        ],
    );
    assert_eq!(code, 0, "limited context failed: {err}");
    let limited: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(limited["knowledge"]["bindings"]["total"], 2, "{limited}");
    assert_eq!(
        limited["knowledge"]["bindings"]["truncated"], true,
        "{limited}"
    );
    assert_eq!(
        limited["knowledge"]["bindings"]["omitted_by_limit"], 1,
        "{limited}"
    );
    assert!(limited["truncation"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "binding-limit"));

    let source = h.join("sem").join("graph.py");
    let source_before = std::fs::read(&source).unwrap();
    let second_line_start = source_before
        .split_inclusive(|byte| *byte == b'\n')
        .take(lines[1] - 1)
        .map(<[u8]>::len)
        .sum::<usize>();
    std::fs::write(&source, "# changed after the code snapshot\n").unwrap();
    let stale = context(lines[0]);
    assert_eq!(
        stale["knowledge"]["bindings"]["status"], "unresolved",
        "a stale pinned anchor does not establish a current binding: {stale}"
    );
    assert_eq!(
        stale["knowledge"]["bindings"]["bindings"][0]["freshness"], "stale",
        "{stale}"
    );
    assert_eq!(
        stale["snapshot"]["nonce"],
        stale["knowledge"]["bindings"]["bindings"][0]["anchor"]["snapshot_nonce"],
        "binding lookup must keep the code snapshot's pinned anchor"
    );
    assert_eq!(std::fs::read(&knowledge_store).unwrap(), knowledge_before);

    let tamper = rusqlite::Connection::open(&knowledge_store).unwrap();
    tamper
        .execute(
            "INSERT INTO code_bindings (
                 repo,repo_root_digest,snapshot_id,snapshot_nonce,snapshot_digest,source_digest,
                 path,byte_start,byte_end,cursor_line,cursor_col,symbol_uri,display_name,
                 anchor_kind,precision,record_id,record_revision,relation,author,rationale,provenance,created_unix
             ) SELECT repo,repo_root_digest,snapshot_id,snapshot_nonce,snapshot_digest,source_digest,
                 path,?1,?2,?3,6,symbol_uri,display_name,anchor_kind,precision,'missing-record',1,
                 'documents','test','orphan row','review',created_unix
             FROM code_bindings LIMIT 1",
            rusqlite::params![
                i64::try_from(second_line_start + 4).unwrap(),
                i64::try_from(second_line_start + 6).unwrap(),
                i64::try_from(lines[1]).unwrap()
            ],
        )
        .unwrap();
    drop(tamper);
    let orphan_binding = context(lines[1]);
    assert_eq!(
        orphan_binding["knowledge"]["bindings"]["status"], "unavailable",
        "unreadable binding rows must not prove an absent link: {orphan_binding}"
    );
    assert_eq!(
        orphan_binding["knowledge"]["bindings"]["unresolved_reason"],
        "knowledge-changed-or-unavailable",
        "{orphan_binding}"
    );

    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &line,
            "--col",
            "6",
            "--query",
            "database migration",
            "--semantic",
        ],
    );
    assert_eq!(code, 0, "semantic context failed: {err}");
    let fallback: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(fallback["query"]["status"], "lexical", "{fallback}");
    assert_eq!(
        fallback["query"]["hits"][0]["relevance"], "candidate",
        "{fallback}"
    );
    assert_eq!(fallback["semantic"]["status"], "fallback", "{fallback}");
}

#[test]
fn r3_context_does_not_prove_no_link_for_a_changed_pinned_anchor() {
    let h = home("r3-context-unlinked-stale-anchor");
    let lines = trace_fixture(&h, &[vec![], vec![]]);
    let create = br#"{"id":"bound","kind":"decision","title":"Read boundary","body":"keep the store local","status":"active","author":"a","evidence":[]}"#;
    assert_eq!(
        run_stdin(
            &h,
            &["knowledge", "create", "--repo", "sem", "--input", "-"],
            create
        )
        .0,
        0
    );
    let binding = br#"{"record_id":"bound","record_revision":1,"relation":"constrains","author":"reviewer","rationale":"This decision constrains the code anchor.","provenance":"review"}"#;
    let (code, _, err) = run_stdin(
        &h,
        &[
            "knowledge",
            "bind",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &lines[0].to_string(),
            "--col",
            "6",
            "--input",
            "-",
        ],
        binding,
    );
    assert_eq!(code, 0, "bind failed: {err}");

    let context = |line: usize| {
        let line = line.to_string();
        let (code, out, err) = run(
            &h,
            &[
                "context", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
            ],
        );
        assert_eq!(code, 0, "context failed: {err}");
        serde_json::from_str::<serde_json::Value>(&out).unwrap()
    };

    let fresh_unlinked = context(lines[1]);
    assert_eq!(
        fresh_unlinked["knowledge"]["bindings"]["status"], "no-link-proven",
        "{fresh_unlinked}"
    );
    let source = h.join("sem").join("graph.py");
    std::fs::write(&source, "# changed after the code snapshot\n").unwrap();
    let changed = context(lines[1]);
    assert_eq!(
        changed["knowledge"]["bindings"]["status"], "unresolved",
        "a changed live source cannot support no-link proof: {changed}"
    );
    assert_eq!(
        changed["knowledge"]["bindings"]["unresolved_reason"],
        "anchor unresolved: source changed since pinned code snapshot",
        "{changed}"
    );
}

#[test]
fn r3_context_does_not_invent_snapshot_metadata_after_binding_read_error() {
    let h = home("r3-context-binding-read-error");
    let lines = trace_fixture(&h, &[vec![]]);
    let create = br#"{"id":"bound","kind":"decision","title":"Read boundary","body":"keep the store local","status":"active","author":"a","evidence":[]}"#;
    assert_eq!(
        run_stdin(
            &h,
            &["knowledge", "create", "--repo", "sem", "--input", "-"],
            create
        )
        .0,
        0
    );
    let binding = br#"{"record_id":"bound","record_revision":1,"relation":"constrains","author":"reviewer","rationale":"This decision constrains the code anchor.","provenance":"review"}"#;
    let (code, _, err) = run_stdin(
        &h,
        &[
            "knowledge",
            "bind",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &lines[0].to_string(),
            "--col",
            "6",
            "--input",
            "-",
        ],
        binding,
    );
    assert_eq!(code, 0, "bind failed: {err}");
    let knowledge_store = h.join("knowledge").join("sem.db");
    let tamper = rusqlite::Connection::open(&knowledge_store).unwrap();
    tamper
        .execute("DELETE FROM meta WHERE key='knowledge_generation'", [])
        .unwrap();
    drop(tamper);

    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
        ],
    );
    assert_eq!(code, 0, "context failed: {err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["knowledge"]["status"], "unavailable", "{value}");
    assert!(
        value["knowledge"]["snapshot"]["generation"].is_null(),
        "{value}"
    );
    assert!(
        value["knowledge"]["snapshot"]["schema_version"].is_null(),
        "{value}"
    );
    assert!(
        value["knowledge"]["bindings"]["schema_version"].is_null(),
        "{value}"
    );
}

#[test]
fn r3_context_rejects_same_span_binding_for_a_different_symbol() {
    let h = home("r3-context-binding-identity");
    let lines = trace_fixture(&h, &[vec![]]);
    let create = br#"{"id":"bound","kind":"decision","title":"Read boundary","body":"keep the store local","status":"active","author":"a","evidence":[]}"#;
    assert_eq!(
        run_stdin(
            &h,
            &["knowledge", "create", "--repo", "sem", "--input", "-"],
            create
        )
        .0,
        0
    );
    let binding = br#"{"record_id":"bound","record_revision":1,"relation":"constrains","author":"reviewer","rationale":"This decision constrains the code anchor.","provenance":"review"}"#;
    let (code, _, err) = run_stdin(
        &h,
        &[
            "knowledge",
            "bind",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &lines[0].to_string(),
            "--col",
            "6",
            "--input",
            "-",
        ],
        binding,
    );
    assert_eq!(code, 0, "bind failed: {err}");
    let knowledge_store = h.join("knowledge").join("sem.db");
    let tamper = rusqlite::Connection::open(&knowledge_store).unwrap();
    tamper
        .execute(
            "UPDATE code_bindings SET snapshot_id=snapshot_id+1,snapshot_nonce='promoted-snapshot'
             WHERE binding_id=(SELECT min(binding_id) FROM code_bindings)",
            [],
        )
        .unwrap();
    drop(tamper);

    let line = lines[0].to_string();
    let (code, compatible_out, err) = run(
        &h,
        &[
            "context", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
        ],
    );
    assert_eq!(code, 0, "context failed: {err}");
    let compatible: serde_json::Value = serde_json::from_str(&compatible_out).unwrap();
    assert_eq!(
        compatible["knowledge"]["bindings"]["status"], "linked",
        "same bytes and symbol remain compatible across snapshot promotion: {compatible}"
    );
    let tamper = rusqlite::Connection::open(&knowledge_store).unwrap();
    tamper
        .execute(
            "UPDATE code_bindings
             SET symbol_uri='scip-test test test-pkg 1.0.0 graph.py/other().'
             WHERE binding_id=(SELECT min(binding_id) FROM code_bindings)",
            [],
        )
        .unwrap();
    drop(tamper);
    let (code, out, err) = run(
        &h,
        &[
            "context", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
        ],
    );
    assert_eq!(code, 0, "context failed: {err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        value["knowledge"]["bindings"]["status"], "unresolved",
        "{value}"
    );
    assert_eq!(
        value["knowledge"]["bindings"]["bindings"][0]["freshness"], "unresolved",
        "{value}"
    );
    assert_eq!(
        value["knowledge"]["bindings"]["unresolved_reason"],
        "stored binding identity does not match pinned code anchor",
        "{value}"
    );
}

#[test]
fn r3_context_preserves_code_when_knowledge_store_is_corrupt() {
    let h = home("r3-context-corrupt-knowledge");
    let lines = trace_fixture(&h, &[vec![]]);
    let knowledge_dir = h.join("knowledge");
    std::fs::create_dir_all(&knowledge_dir).unwrap();
    let db = knowledge_dir.join("sem.db");
    std::fs::write(&db, b"not a sqlite database").unwrap();
    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
            "--query", "decision",
        ],
    );
    assert_eq!(
        code, 0,
        "code context should survive unavailable knowledge: {err}"
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["snapshot"]["id"], 1, "{value}");
    assert_eq!(value["knowledge"]["status"], "unavailable", "{value}");
    assert_eq!(
        value["knowledge"]["reason"], "knowledge-store-unavailable",
        "{value}"
    );
    assert_eq!(value["query"]["status"], "unavailable", "{value}");
    assert_eq!(std::fs::read(&db).unwrap(), b"not a sqlite database");
}

#[test]
fn r3_context_caps_final_utf8_json_and_truncates_snippets_at_character_boundaries() {
    let h = home("r3-context-byte-budget");
    let lines = trace_fixture(&h, &[vec![]]);
    let body = "界".repeat(5000);
    let input = format!(
        "{{\"id\":\"candidate\",\"kind\":\"decision\",\"title\":\"database choice\",\"body\":\"{body}\",\"status\":\"active\",\"author\":\"a\",\"evidence\":[]}}"
    );
    assert_eq!(
        run_stdin(
            &h,
            &["knowledge", "create", "--repo", "sem", "--input", "-"],
            input.as_bytes()
        )
        .0,
        0
    );
    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &line,
            "--col",
            "6",
            "--query",
            "database",
            "--max-bytes",
            "8192",
        ],
    );
    assert_eq!(code, 0, "context failed: {err}");
    assert!(
        out.trim_end().len() <= 8192,
        "{} JSON bytes",
        out.trim_end().len()
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["anchor"]["span"], serde_json::json!([4, 6]));
    assert_eq!(value["graph"]["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(value["query"]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(value["query"]["hits"][0]["snippet_truncated"], true);
    assert!(value["truncation"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "knowledge-snippets"));
    assert!(value["query"]["hits"][0]["snippet"]
        .as_str()
        .unwrap()
        .chars()
        .all(|ch| ch == '界'));
}

#[test]
fn r3_context_budget_prunes_relevance_then_whole_binding_rows() {
    let h = home("r3-context-binding-budget");
    let lines = trace_fixture(&h, &[vec![]]);
    for (id, title) in [
        ("bound-first", "Small rationale"),
        ("bound-second", "Large rationale"),
    ] {
        let create = serde_json::json!({
            "id": id,
            "kind": "decision",
            "title": title,
            "body": "record body",
            "status": "active",
            "author": "a",
            "evidence": []
        })
        .to_string();
        assert_eq!(
            run_stdin(
                &h,
                &["knowledge", "create", "--repo", "sem", "--input", "-"],
                create.as_bytes()
            )
            .0,
            0
        );
    }
    for (id, rationale) in [
        ("bound-first", "small rationale".to_string()),
        ("bound-second", "R".repeat(12_000)),
    ] {
        let binding = serde_json::json!({
            "record_id": id,
            "record_revision": 1,
            "relation": "documents",
            "author": "reviewer",
            "rationale": rationale,
            "provenance": "review"
        })
        .to_string();
        let (code, _, err) = run_stdin(
            &h,
            &[
                "knowledge",
                "bind",
                "--repo",
                "sem",
                "--path",
                "graph.py",
                "--line",
                &lines[0].to_string(),
                "--col",
                "6",
                "--input",
                "-",
            ],
            binding.as_bytes(),
        );
        assert_eq!(code, 0, "bind failed: {err}");
    }
    for index in 0..40 {
        let record = serde_json::json!({
            "id": format!("candidate-{index:03}"),
            "kind": "decision",
            "title": format!("Budget query {index}"),
            "body": "small relevance candidate",
            "status": "active",
            "author": "a",
            "evidence": []
        })
        .to_string();
        assert_eq!(
            run_stdin(
                &h,
                &["knowledge", "create", "--repo", "sem", "--input", "-"],
                record.as_bytes()
            )
            .0,
            0
        );
    }

    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &line,
            "--col",
            "6",
            "--query",
            "budget",
            "--limit",
            "100",
            "--max-bytes",
            "8192",
        ],
    );
    assert_eq!(code, 0, "context failed: {err}");
    assert!(
        out.trim_end().len() <= 8192,
        "{} bytes",
        out.trim_end().len()
    );
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["anchor"]["span"], serde_json::json!([4, 6]));
    assert_eq!(value["knowledge"]["bindings"]["total"], 2, "{value}");
    assert_eq!(
        value["knowledge"]["bindings"]["total_exact"], true,
        "{value}"
    );
    assert_eq!(value["knowledge"]["bindings"]["truncated"], true, "{value}");
    assert_eq!(
        value["knowledge"]["bindings"]["omitted_by_limit"], 0,
        "{value}"
    );
    assert_eq!(
        value["knowledge"]["bindings"]["omitted_by_budget"], 1,
        "{value}"
    );
    assert_eq!(
        value["knowledge"]["bindings"]["bindings"][0]["record_id"], "bound-first",
        "the oldest binding is retained deterministically: {value}"
    );
    assert_eq!(value["query"]["hits"], serde_json::json!([]), "{value}");
    assert!(value["truncation"]["omitted_query_hits"].as_u64().unwrap() > 0);
    assert_eq!(value["truncation"]["omitted_bindings_by_budget"], 1);
    let reasons = value["truncation"]["reasons"].as_array().unwrap();
    assert!(reasons
        .iter()
        .any(|reason| reason == "knowledge-candidates"));
    assert!(reasons.iter().any(|reason| reason == "binding-budget"));
}

#[test]
fn r1_context_caps_final_escaped_json_and_reports_dropped_items() {
    let h = home("context-byte-budget");
    let mut graph = vec![vec![]];
    graph.extend((0..80).map(|_| vec![0]));
    let lines = trace_fixture(&h, &graph);
    let line = lines[0].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "sem",
            "--path",
            "graph.py",
            "--line",
            &line,
            "--col",
            "6",
            "--limit",
            "100",
            "--max-bytes",
            "8192",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        out.trim_end().len() <= 8192,
        "{} JSON bytes",
        out.trim_end().len()
    );
    assert_eq!(value["truncation"]["truncated"], true, "{value}");
    assert!(value["truncation"]["omitted_edges"].as_u64().unwrap() > 0);
    assert_eq!(
        value["impact"]["verdict"], "HIGH",
        "coverage stays independent of output pruning"
    );
}

#[test]
fn r1_context_rejects_reserved_query_and_bounds_without_creating_an_index() {
    let h = home("context-read-only-missing");
    repo_with(&h, "sem", &[("graph.py", "def f0():\n    pass\n")]);
    for extra in [
        vec!["--depth", "0"],
        vec!["--depth", "7"],
        vec!["--limit", "101"],
        vec!["--max-bytes", "8191"],
        vec!["--max-bytes", "131073"],
        vec!["--query", "   "],
        vec!["--semantic"],
    ] {
        let mut args = vec![
            "context", "--repo", "sem", "--path", "graph.py", "--line", "1", "--col", "6",
        ];
        args.extend(extra);
        assert_eq!(run(&h, &args).0, 2, "{args:?}");
    }
    let (code, _, err) = run(
        &h,
        &[
            "context", "--repo", "sem", "--path", "graph.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("no snapshot"), "{err}");
    assert!(
        !h.join("knowledge").exists(),
        "context must not create knowledge state"
    );
    assert!(
        !h.join("index").exists(),
        "read-only context must not create an index"
    );
}

// Minimal proto3 writer for exactly the fields the importer reads (field
// numbers pinned by scip.proto; same shape as tests/semantic.rs).
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

const T_URI: &str = "scip-test test test-pkg 1.0.0 lib.py/target().";
const C_URI: &str = "scip-test test test-pkg 1.0.0 main.py/caller().";

fn scip_for(dir: &Path) -> String {
    let mut sym = Vec::new();
    str_field(1, T_URI, &mut sym);
    str_field(6, "target", &mut sym);
    let lib_occ = occurrence(T_URI, 0x1, 0, 4, 10);
    let mut doc_lib_full = Vec::new();
    str_field(1, "lib.py", &mut doc_lib_full);
    len_field(2, &lib_occ, &mut doc_lib_full);
    len_field(3, &sym, &mut doc_lib_full);
    varint_field(6, 1, &mut doc_lib_full);

    let mut sym_c = Vec::new();
    str_field(1, C_URI, &mut sym_c);
    str_field(6, "caller", &mut sym_c);
    let mut doc_main = Vec::new();
    str_field(1, "main.py", &mut doc_main);
    for o in [
        occurrence(C_URI, 0x1, 2, 4, 10),
        occurrence(T_URI, 0x0, 3, 11, 17),
    ] {
        len_field(2, &o, &mut doc_main);
    }
    len_field(3, &sym_c, &mut doc_main);
    len_field(3, &sym, &mut doc_main);
    varint_field(6, 1, &mut doc_main);

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
    len_field(2, &doc_lib_full, &mut out);
    len_field(2, &doc_main, &mut out);
    let scip = dir.join("index.scip");
    std::fs::write(&scip, &out).unwrap();
    scip.to_string_lossy().to_string()
}

fn edge_repo(h: &Path) -> PathBuf {
    repo_with(
        h,
        "sem",
        &[
            ("lib.py", "def target():\n    return 1\n"),
            (
                "main.py",
                "from lib import target\n\ndef caller():\n    return target()\n",
            ),
        ],
    )
}

#[test]
fn cli_graph_import_failure_rolls_back() {
    let h = home("rollback");
    edge_repo(&h);
    do_index(&h);
    let bad = h.join("sem").join("bad.scip");
    std::fs::write(&bad, b"not a scip payload").unwrap();
    let (code, _, _) = run(
        &h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &bad.to_string_lossy(),
        ],
    );
    assert_ne!(code, 0, "corrupt SCIP must fail the import");
    let (code, out, _) = run(&h, &["semantic", "status", "--repo", "sem"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("\"imports\":[]"),
        "failed import stores nothing: {out}"
    );
}

#[test]
fn cli_graph_incremental_modes_hold_with_edges() {
    let h = home("modes");
    edge_repo(&h);
    do_index(&h);
    let (code, out, err) = run(&h, &["index", "--repo", "sem"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"mode\":\"incremental\""), "{out}");
    std::fs::write(
        h.join("sem").join("lib.py"),
        "def target():\n    return 2\n",
    )
    .unwrap();
    let (code, out, err) = run(&h, &["index", "--repo", "sem"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"mode\":\"incremental\""), "{out}");
    let (code, out, err) = run(&h, &["index", "--repo", "sem", "--full"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"mode\":\"full\""), "{out}");
}

#[test]
fn cli_graph_import_after_rebuild_rebinds() {
    // Import → rebuild (facts carry) → re-import on the new snapshot: the
    // lifecycle the graph re-derivation relies on, CLI-observed.
    // lifecycle the graph re-derivation relies on, CLI-observed.
    let h = home("rebind");
    let dir = edge_repo(&h);
    do_index(&h);
    let scip = scip_for(&dir);
    let (code, _, err) = run(
        &h,
        &["semantic", "import", "--repo", "sem", "--scip", &scip],
    );
    assert_eq!(code, 0, "{err}");
    std::fs::write(dir.join("lib.py"), "def target():\n    return 2\n").unwrap();
    let (code, _, err) = run(&h, &["index", "--repo", "sem"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run(
        &h,
        &["semantic", "import", "--repo", "sem", "--scip", &scip],
    );
    assert_eq!(code, 0, "re-import on the new snapshot: {err}");
    let (code, out, _) = run(&h, &["semantic", "status", "--repo", "sem"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"verification\""), "{out}");
}

// ---------------------------------------------------------------------------
// Task 4: callers/callees CLI.

fn scip_imported(h: &Path) -> PathBuf {
    let dir = edge_repo(h);
    do_index(h);
    let scip = scip_for(&dir);
    let (code, _, err) = run(h, &["semantic", "import", "--repo", "sem", "--scip", &scip]);
    assert_eq!(code, 0, "{err}");
    dir
}

#[test]
fn cli_callers_finds_scip_callers_without_reindex() {
    let h = home("callers");
    scip_imported(&h);
    // Anchor on the target definition (line 1, col 6 → def occurrence).
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"schema_version\":1"), "{out}");
    assert!(out.contains("\"direction\":\"callers\""), "{out}");
    assert!(out.contains("\"kind\":\"CALLS\""), "{out}");
    assert!(out.contains("\"precision\":\"Exact\""), "{out}");
    assert!(out.contains("main.py"), "{out}");
    assert!(out.contains("\"verification\":\"Unverified\""), "{out}");
    assert!(!out.contains("Current"), "never Current: {out}");
    assert!(
        out.contains("\"status\":\"partial\""),
        "unsupported SCIP artifact: {out}"
    );
    assert!(out.contains("\"truncated\":false"), "{out}");
    // --symbol agreeing with the position identity returns the same answer.
    let (code, out2, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "1", "--col", "6",
            "--symbol", T_URI,
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, out2, "agreeing --symbol must not change the answer");
}

#[test]
fn cli_callers_resolves_rust_local_function_alias() {
    let h = home("rust-alias");
    let dir = h.join("sem");
    rust_alias::write_fixture(&dir);
    let (code, _, err) = run(
        &h,
        &["init", "--repo", &dir.to_string_lossy(), "--name", "sem"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    do_index(&h);
    let scip = h.join("rust-alias.scip");
    std::fs::write(&scip, rust_alias::scip_payload(&dir)).unwrap();
    let (code, _, err) = run(
        &h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &scip.to_string_lossy(),
        ],
    );
    assert_eq!(code, 0, "semantic import failed: {err}");
    let (code, _, err) = run(
        &h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &scip.to_string_lossy(),
        ],
    );
    assert_eq!(code, 0, "identical re-import failed: {err}");

    let callers = |line: &str| {
        let (code, out, err) = run(
            &h,
            &[
                "callers",
                "--repo",
                "sem",
                "--path",
                "src/lib.rs",
                "--line",
                line,
                "--col",
                "20",
            ],
        );
        assert_eq!(code, 0, "callers failed: {err}");
        serde_json::from_str::<serde_json::Value>(&out).unwrap()
    };
    let a = callers("1");
    let a_calls: Vec<_> = a["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["kind"] == "CALLS")
        .collect();
    assert_eq!(a_calls.len(), 2, "direct + alias to a::target: {a}");
    let alias = a_calls
        .iter()
        .find(|edge| edge["caller_span"] == serde_json::json!([171, 174]))
        .expect("alias caller");
    assert_eq!(
        a_calls
            .iter()
            .filter(|edge| edge["caller_span"] == serde_json::json!([171, 174]))
            .count(),
        1,
        "re-import must not duplicate the alias edge"
    );
    assert_eq!(
        alias["callee"]["symbol_uri"],
        "rust-analyzer cargo precision_fixture 0.1.0 a/target()."
    );
    assert_eq!(
        alias["caller"]["symbol_uri"],
        "rust-analyzer cargo precision_fixture 0.1.0 alias()."
    );
    assert_eq!(
        alias["evidence"]["terminal_span"],
        serde_json::json!([171, 172])
    );
    assert_eq!(
        alias["evidence"]["reference_span"],
        serde_json::json!([163, 169])
    );
    assert_eq!(alias["verification"], "Unverified");
    assert!(a["edges"].as_array().unwrap().iter().any(|edge| {
        edge["kind"] == "REFERENCES" && edge["caller_span"] == serde_json::json!([211, 217])
    }));
    assert!(!a["edges"].as_array().unwrap().iter().any(|edge| {
        edge["kind"] == "POTENTIAL_CALL" && edge["caller_span"] == serde_json::json!([171, 174])
    }));

    let b = callers("2");
    let b_calls: Vec<_> = b["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["kind"] == "CALLS")
        .collect();
    assert_eq!(b_calls.len(), 1, "b::target is a distinct identity: {b}");
    assert_eq!(
        b_calls[0]["callee"]["symbol_uri"],
        "rust-analyzer cargo precision_fixture 0.1.0 b/target()."
    );
    assert_eq!(
        b_calls[0]["caller"]["symbol_uri"],
        "rust-analyzer cargo precision_fixture 0.1.0 other()."
    );
}

#[test]
fn cli_callees_finds_scip_callees() {
    let h = home("callees");
    scip_imported(&h);
    // Anchor on the caller definition (main.py line 3, col 6).
    let (code, out, err) = run(
        &h,
        &[
            "callees", "--repo", "sem", "--path", "main.py", "--line", "3", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"direction\":\"callees\""), "{out}");
    assert!(out.contains("\"kind\":\"CALLS\""), "{out}");
    assert!(out.contains("lib.py"), "{out}");
    assert!(out.contains("\"callee_name\":\"target\""), "{out}");
}

#[test]
fn cli_java_calls_have_owners_in_methods_and_constructors() {
    let h = home("java-owner");
    repo_with(
        &h,
        "sem",
        &[(
            "A.java",
            "class A {
    void target() {}
    void caller() { target(); }
    A() { target(); }
}
",
        )],
    );
    do_index(&h);
    // Anchor on the method `caller` (line 3, col 10) and the constructor (line 4, col 5).
    for (line, col) in [("3", "10"), ("4", "5")] {
        let (code, out, err) = run(
            &h,
            &[
                "callees", "--repo", "sem", "--path", "A.java", "--line", line, "--col", col,
            ],
        );
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("\"callee_name\":\"target\""), "{out}");
        assert!(out.contains("POTENTIAL_CALL"), "syntax-only: {out}");
        assert!(!out.contains("owner-unproven"), "{out}");
    }
}

#[test]
fn cli_syntax_fallback_without_scip() {
    let h = home("fallback");
    edge_repo(&h);
    do_index(&h);
    // No SCIP import: anchor falls back to the syntax definition, explicit.
    let (code, out, err) = run(
        &h,
        &[
            "callees", "--repo", "sem", "--path", "main.py", "--line", "3", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"kind\":\"syntax-fallback\""), "{out}");
    assert!(out.contains("\"precision\":\"Heuristic\""), "{out}");
    assert!(
        out.contains("\"anchor_uncertainty\":\"syntax-only"),
        "{out}"
    );
    assert!(out.contains("\"kind\":\"POTENTIAL_CALL\""), "{out}");
    assert!(out.contains("\"provider\":\"syntax\""), "{out}");
    assert!(
        !out.contains("\"kind\":\"CALLS\""),
        "no proven calls without evidence: {out}"
    );
}

#[test]
fn cli_same_name_no_false_link() {
    let h = home("samename");
    repo_with(
        &h,
        "sem",
        &[
            ("a.py", "def foo():\n    return 1\n"),
            (
                "b.py",
                "def foo():\n    return 2\n\ndef bar():\n    foo()\n",
            ),
        ],
    );
    do_index(&h);
    // Callers of a.py/foo: bar calls b.py/foo — must not link by name.
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "a.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"edges\":[]"), "{out}");
    assert!(
        out.contains("\"status\":\"partial\""),
        "empty is not coverage proof: {out}"
    );
    // Callers of b.py/foo: bar, same file, honest POTENTIAL.
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "b.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"caller_path\":\"b.py\""), "{out}");
    assert!(out.contains("\"callee_name\":\"foo\""), "{out}");
    assert!(out.contains("\"kind\":\"POTENTIAL_CALL\""), "{out}");
    assert!(!out.contains("\"caller_path\":\"a.py\""), "{out}");
}

#[test]
fn cli_cpp_template_call_names_the_function_not_the_type_argument() {
    let h = home("cpp-template");
    let dir = repo_with(
        &h,
        "sem",
        &[(
            "patch.cpp",
            "void run_patch(Options options) {\n  auto c = std::make_shared<mvs::PatchMatchController>(options);\n  make<mvs::PatchMatchController>(options);\n}\n",
        )],
    );
    do_index(&h);
    let v = query_json(&h, "callees", "patch.cpp", &[]);
    let src = std::fs::read(dir.join("patch.cpp")).unwrap();
    let text = |span: &serde_json::Value| {
        let a = span[0].as_u64().unwrap() as usize;
        let b = span[1].as_u64().unwrap() as usize;
        String::from_utf8(src[a..b].to_vec()).unwrap()
    };
    let edges = v["edges"].as_array().expect("edges");
    // Two calls, plus the labelled candidate for the factory's argument.
    assert_eq!(edges.len(), 3, "{v}");
    // Only the factory call yields a candidate; the generic `make<T>` yields none.
    let candidates = edges
        .iter()
        .filter(|e| e["unresolved"] == "factory-template-argument")
        .count();
    assert_eq!(candidates, 1, "{v}");
    // (callee as written, called name, reason): a qualified call has a
    // receiver (`dynamic`), an unqualified one has none (`no-overlap`).
    for (callee, terminal, reason) in [
        (
            "std::make_shared<mvs::PatchMatchController>",
            "make_shared",
            "dynamic",
        ),
        ("make<mvs::PatchMatchController>", "make", "no-overlap"),
    ] {
        let edge = edges
            .iter()
            .find(|e| text(&e["evidence"]["callee_span"]) == callee)
            .unwrap_or_else(|| panic!("edge for {callee}: {v}"));
        assert_eq!(edge["callee_name"], terminal, "{edge}");
        assert_eq!(text(&edge["evidence"]["terminal_span"]), terminal, "{edge}");
        // Syntax only: a candidate, never a proven call.
        assert_eq!(edge["kind"], "POTENTIAL_CALL", "{edge}");
        assert_eq!(edge["precision"], "Heuristic", "{edge}");
        assert_eq!(edge["unresolved"], reason, "{edge}");
    }
    assert!(!v.to_string().contains("\"CALLS\""), "{v}");
}

#[test]
fn cli_factory_template_argument_is_a_labelled_candidate() {
    let h = home("factory-candidate");
    let dir = repo_with(
        &h,
        "sem",
        &[(
            "patch.cpp",
            "void caller(int options) {\n  auto cuda = std::make_unique<PatchMatchCuda>(options);\n}\nstruct PatchMatchCuda {};\n",
        )],
    );
    do_index(&h);
    let v = query_json(&h, "callees", "patch.cpp", &[]);
    let src = std::fs::read(dir.join("patch.cpp")).unwrap();
    let text = |span: &serde_json::Value| {
        let a = span[0].as_u64().unwrap() as usize;
        let b = span[1].as_u64().unwrap() as usize;
        String::from_utf8(src[a..b].to_vec()).unwrap()
    };
    let edges = v["edges"].as_array().expect("edges");
    assert_eq!(edges.len(), 2, "{v}");
    assert!(edges.iter().all(|e| e["kind"] != "CALLS"), "{v}");
    // The called function keeps the name it is called by.
    let call = edges
        .iter()
        .find(|e| e["callee_name"] == "make_unique")
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(
        text(&call["evidence"]["callee_span"]),
        "std::make_unique<PatchMatchCuda>",
        "{call}"
    );
    assert_eq!(
        text(&call["evidence"]["terminal_span"]),
        "make_unique",
        "{call}"
    );
    assert_eq!(call["kind"], "POTENTIAL_CALL", "{call}");
    // The constructed type is a labelled candidate at its own span.
    let made = edges
        .iter()
        .find(|e| e["callee_name"] == "PatchMatchCuda")
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(made["kind"], "POTENTIAL_CALL", "{made}");
    assert_eq!(made["precision"], "Heuristic", "{made}");
    assert_eq!(made["verification"], "Unverified", "{made}");
    assert_eq!(made["unresolved"], "factory-template-argument", "{made}");
    assert!(made["callee"].is_null(), "{made}");
    for field in ["span", "callee_span", "terminal_span"] {
        assert_eq!(
            text(&made["evidence"][field]),
            "PatchMatchCuda",
            "{field}: {made}"
        );
    }
}

#[test]
fn cli_factory_template_argument_stays_a_candidate_under_scip() {
    const CALLER: &str = "scip-test test test-pkg 1.0.0 patch.cpp/caller().";
    const CUDA: &str = "scip-test test test-pkg 1.0.0 patch.cpp/PatchMatchCuda#";
    let h = home("factory-scip");
    let dir = repo_with(
        &h,
        "sem",
        &[(
            "patch.cpp",
            "void caller(int options) {\n  auto cuda = std::make_unique<PatchMatchCuda>(options);\n}\nstruct PatchMatchCuda {};\n",
        )],
    );
    do_index(&h);
    // SCIP names the type where it is used (line 1, columns 31 to 45) and
    // where it is defined (line 3).
    let doc = test_document(
        "patch.cpp",
        &[
            occurrence(CALLER, 0x1, 0, 5, 11),
            occurrence(CUDA, 0x0, 1, 31, 45),
            occurrence(CUDA, 0x1, 3, 7, 21),
        ],
        &[(CALLER, "caller"), (CUDA, "PatchMatchCuda")],
    );
    import_documents(&h, &dir, "scip-test", &[doc]);
    let v = query_json(&h, "callees", "patch.cpp", &[]);
    let edges = v["edges"].as_array().expect("edges");
    // A type reference is not a call: no CALLS edge, and the candidate keeps
    // its label.
    assert!(edges.iter().all(|e| e["kind"] != "CALLS"), "{v}");
    let made = edges
        .iter()
        .find(|e| e["callee_name"] == "PatchMatchCuda")
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(made["kind"], "POTENTIAL_CALL", "{made}");
    assert_eq!(made["unresolved"], "factory-template-argument", "{made}");
}

#[test]
fn cli_scip_template_call_proves_the_function_not_the_type_argument() {
    const CALLER: &str = "scip-test test test-pkg 1.0.0 patch.cpp/caller().";
    const MAKE: &str = "scip-test test test-pkg 1.0.0 patch.cpp/make_it().";
    const THING: &str = "scip-test test test-pkg 1.0.0 patch.cpp/Thing#";
    let h = home("scip-template");
    let dir = repo_with(
        &h,
        "sem",
        &[(
            "patch.cpp",
            "void caller(int options) {\n  ns::make_it<Thing>(options);\n}\nvoid make_it(int options) {}\nstruct Thing {};\n",
        )],
    );
    do_index(&h);
    // SCIP references two names inside the one call: the function and the
    // template type argument.
    let doc = test_document(
        "patch.cpp",
        &[
            occurrence(CALLER, 0x1, 0, 5, 11),
            occurrence(MAKE, 0x0, 1, 6, 13),
            occurrence(THING, 0x0, 1, 14, 19),
            occurrence(MAKE, 0x1, 3, 5, 12),
            occurrence(THING, 0x1, 4, 7, 12),
        ],
        &[(CALLER, "caller"), (MAKE, "make_it"), (THING, "Thing")],
    );
    import_documents(&h, &dir, "scip-test", &[doc]);
    let v = query_json(&h, "callees", "patch.cpp", &[]);
    let edges = v["edges"].as_array().expect("edges");
    assert_eq!(edges.len(), 2, "{v}");
    // The one proven call goes to the function, on SCIP evidence at the
    // terminal.
    let calls: Vec<_> = edges.iter().filter(|e| e["kind"] == "CALLS").collect();
    assert_eq!(calls.len(), 1, "{v}");
    let call = calls[0];
    assert_eq!(call["precision"], "Exact");
    assert_eq!(call["provider"], "scip-import");
    assert_eq!(call["callee"]["symbol_uri"], MAKE);
    assert_eq!(call["callee_name"], "make_it");
    let src = std::fs::read(dir.join("patch.cpp")).unwrap();
    for (field, expected) in [
        ("span", "ns::make_it<Thing>(options)"),
        ("callee_span", "ns::make_it<Thing>"),
        ("terminal_span", "make_it"),
        ("reference_span", "make_it"),
    ] {
        let span = call["evidence"][field].as_array().expect(field);
        let a = span[0].as_u64().unwrap() as usize;
        let b = span[1].as_u64().unwrap() as usize;
        assert_eq!(&src[a..b], expected.as_bytes(), "{field}");
    }
    // The type argument stays an unresolved candidate: no target, no proof.
    let other: Vec<_> = edges.iter().filter(|e| e["kind"] != "CALLS").collect();
    assert_eq!(other.len(), 1, "{v}");
    assert_eq!(other[0]["kind"], "POTENTIAL_CALL");
    assert_eq!(other[0]["precision"], "Heuristic");
    assert_eq!(other[0]["unresolved"], "target-mismatch");
    assert!(other[0]["callee"].is_null(), "{v}");
    // Callers of the type (its definition is on line 5): no proven call.
    let (code, out, err) = run(
        &h,
        &[
            "callers",
            "--repo",
            "sem",
            "--path",
            "patch.cpp",
            "--line",
            "5",
            "--col",
            "8",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let t: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert!(
        t["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .all(|e| e["kind"] != "CALLS"),
        "{t}"
    );
}

#[test]
fn cli_stale_edit_reports_stale_not_current() {
    let h = home("stale");
    scip_imported(&h);
    std::fs::write(
        h.join("sem").join("lib.py"),
        "def target():\n    return 9\n",
    )
    .unwrap();
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"verification\":\"Stale\""), "{out}");
    assert!(!out.contains("Current"), "{out}");
}

#[test]
fn cli_pagination_is_deterministic() {
    let h = home("paging");
    repo_with(
        &h,
        "sem",
        &[
            ("t.py", "def target():\n    return 1\n"),
            ("c1.py", "from t import target\n\ndef c1():\n    target()\n"),
            ("c2.py", "from t import target\n\ndef c2():\n    target()\n"),
            ("c3.py", "from t import target\n\ndef c3():\n    target()\n"),
        ],
    );
    do_index(&h);
    let args = [
        "callers", "--repo", "sem", "--path", "t.py", "--line", "1", "--col", "6",
    ];
    let (code, full, err) = run(&h, &args);
    assert_eq!(code, 0, "{err}");
    assert!(
        full.contains("c1") && full.contains("c2") && full.contains("c3"),
        "{full}"
    );
    let (code, p0, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "t.py", "--line", "1", "--col", "6", "--limit",
            "2", "--offset", "0",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(p0.contains("\"truncated\":true"), "{p0}");
    let (code, p1, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "t.py", "--line", "1", "--col", "6", "--limit",
            "2", "--offset", "2",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(p1.contains("\"truncated\":false"), "{p1}");
    // Pages are disjoint and repeatable: same query twice is byte-identical.
    let (code, p0b, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "t.py", "--line", "1", "--col", "6", "--limit",
            "2", "--offset", "0",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(p0, p0b, "pagination must be deterministic");
}

#[test]
fn cli_anchor_errors_are_explicit() {
    let h = home("anchors");
    let dir = edge_repo(&h);
    do_index(&h);
    // Position with no occurrence and no enclosing definition.
    std::fs::write(dir.join("empty.py"), "\n\n\n").unwrap();
    do_index(&h);
    let (code, _, _) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "empty.py", "--line", "2", "--col", "1",
        ],
    );
    assert_eq!(code, 3, "anchorless position must fail as data error");
    // --symbol contradicting the position identity.
    let (code, _, _) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "1", "--col", "6",
            "--symbol", C_URI,
        ],
    );
    assert_eq!(
        code, 3,
        "contradicting --symbol must fail, never silently switch"
    );
}

#[test]
fn cli_cross_repo_isolation() {
    let h = home("xrepo");
    scip_imported(&h);
    repo_with(&h, "other", &[("q.py", "def qq():\n    pass\n")]);
    let (code, _, err) = run(&h, &["index", "--repo", "other"]);
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "other", "--path", "q.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"edges\":[]"), "{out}");
    assert!(!out.contains("main.py"), "no cross-repo leakage: {out}");
}

#[test]
fn cli_local_symbol_stays_in_document() {
    let h = home("locals");
    let dir = repo_with(&h, "sem", &[("m.py", "def f():\n    return 1\n")]);
    do_index(&h);
    // Hand-built SCIP with a document-local symbol: def + ref in m.py.
    let local = "local foo";
    let mut sym = Vec::new();
    str_field(1, local, &mut sym);
    str_field(6, "foo", &mut sym);
    let mut doc = Vec::new();
    str_field(1, "m.py", &mut doc);
    for o in [
        occurrence(local, 0x1, 0, 4, 5),
        occurrence(local, 0x0, 1, 11, 12),
    ] {
        len_field(2, &o, &mut doc);
    }
    len_field(3, &sym, &mut doc);
    varint_field(6, 1, &mut doc);
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
    let mut payload = Vec::new();
    len_field(1, &md, &mut payload);
    len_field(2, &doc, &mut payload);
    let scip = dir.join("index.scip");
    std::fs::write(&scip, &payload).unwrap();
    let (code, _, err) = run(
        &h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &scip.to_string_lossy(),
        ],
    );
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "m.py", "--line", "1", "--col", "5",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("m.py"), "{out}");
    assert!(out.contains("\"kind\":\"REFERENCES\""), "{out}");
    assert!(!out.contains("ghost.py"), "{out}");
}

fn query_json(h: &Path, direction: &str, path: &str, extra: &[&str]) -> serde_json::Value {
    let mut args = vec![
        direction, "--repo", "sem", "--path", path, "--line", "1", "--col", "6",
    ];
    args.extend_from_slice(extra);
    let (code, out, err) = run(h, &args);
    assert_eq!(code, 0, "{err}");
    serde_json::from_str(&out).expect("valid JSON")
}

#[test]
fn task4_json_contract_and_source_evidence() {
    let h = home("contract");
    let dir = scip_imported(&h);
    let v = query_json(&h, "callers", "lib.py", &[]);
    assert_eq!(v["schema_version"], 1);
    assert!(v["snapshot"]["revision"].is_string());
    assert_eq!(v["freshness"]["status"], "fresh");
    assert_eq!(v["limits"]["limit"], 50);
    assert!(v["anchor"]["anchor_uncertainty"].is_null());
    assert_eq!(v["anchor"]["endpoint"]["symbol_uri"], T_URI);
    let e = &v["edges"][0];
    assert_eq!(e["kind"], "CALLS");
    assert_eq!(e["precision"], "Exact");
    assert_eq!(e["verification"], "Unverified");
    assert_eq!(e["evidence"]["path"], "main.py");
    let src = std::fs::read(dir.join("main.py")).unwrap();
    for (field, expected) in [
        ("span", "target()"),
        ("terminal_span", "target"),
        ("reference_span", "target"),
    ] {
        let span = e["evidence"][field].as_array().expect(field);
        let a = span[0].as_u64().unwrap() as usize;
        let b = span[1].as_u64().unwrap() as usize;
        assert_eq!(&src[a..b], expected.as_bytes());
    }
    assert!(e["evidence"]["digest"].is_string());
    assert_eq!(e["evidence"]["imports"][0]["provider"], "scip-test");
    assert!(e["evidence"]["imports"][0]["source_revision"].is_string());
    assert!(v["completeness"]["denominator"].is_object());
    assert_eq!(v, query_json(&h, "callers", "lib.py", &[]));
}

#[test]
fn task4_unresolved_and_empty_are_not_complete() {
    let h = home("unknown");
    edge_repo(&h);
    do_index(&h);
    let v = query_json(&h, "callers", "lib.py", &[]);
    assert_eq!(v["completeness"]["status"], "partial");
    assert!(
        v["completeness"]["denominator"]["unresolved_target"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(v["edges"][0]["callee"].is_null());
    let h2 = home("empty-unknown");
    repo_with(&h2, "sem", &[("a.py", "def unused():\n    pass\n")]);
    do_index(&h2);
    let v = query_json(&h2, "callers", "a.py", &[]);
    assert_eq!(v["edges"], serde_json::json!([]));
    assert_eq!(v["completeness"]["status"], "partial");
}

#[test]
fn task4_syntax_fallback_rejects_changed_source() {
    let h = home("changed-fallback");
    let dir = edge_repo(&h);
    do_index(&h);
    std::fs::write(
        dir.join("lib.py"),
        "# changed offsets\ndef other():\n    pass\n",
    )
    .unwrap();
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "2", "--col", "6",
        ],
    );
    assert_eq!(code, 3, "{out} {err}");
    assert!(err.contains("source-changed"), "{err}");
}

#[test]
fn cuda_destination_reports_its_unsupported_reason_not_source_changed() {
    let h = home("cuda-destination");
    let dir = edge_repo(&h);
    std::fs::write(dir.join("kernel.cu"), "__global__ void kernel() {}\n").unwrap();
    do_index(&h);
    let (code, out, err) = run(
        &h,
        &[
            "trace",
            "--repo",
            "sem",
            "--path",
            "lib.py",
            "--line",
            "1",
            "--col",
            "5",
            "--to",
            "kernel.cu:1:17",
        ],
    );
    assert_eq!(code, 3, "{out} {err}");
    assert!(err.contains("cuda-experimental"), "{err}");
    assert!(!err.contains("source-changed"), "{err}");
}

#[test]
fn changed_destination_still_reports_source_changed() {
    let h = home("changed-destination");
    let dir = edge_repo(&h);
    do_index(&h);
    std::fs::write(
        dir.join("lib.py"),
        "# changed offsets\ndef other():\n    pass\n",
    )
    .unwrap();
    let (code, out, err) = run(
        &h,
        &[
            "trace",
            "--repo",
            "sem",
            "--path",
            "main.py",
            "--line",
            "3",
            "--col",
            "5",
            "--to",
            "lib.py:2:5",
        ],
    );
    assert_eq!(code, 3, "{out} {err}");
    assert!(err.contains("source-changed"), "{err}");
}

#[test]
fn unsupported_extension_destination_reports_its_reason_not_source_changed() {
    let h = home("unsupported-destination");
    let dir = edge_repo(&h);
    std::fs::write(dir.join("notes.md"), "# notes\n").unwrap();
    do_index(&h);
    let (code, out, err) = run(
        &h,
        &[
            "trace",
            "--repo",
            "sem",
            "--path",
            "lib.py",
            "--line",
            "1",
            "--col",
            "5",
            "--to",
            "notes.md:1:1",
        ],
    );
    assert_eq!(code, 3, "{out} {err}");
    assert!(err.contains("unsupported-extension"), "{err}");
    assert!(!err.contains("source-changed"), "{err}");
}

#[test]
fn unsupported_extension_anchor_reports_its_reason_not_source_changed() {
    let h = home("unsupported-anchor");
    let dir = edge_repo(&h);
    std::fs::write(dir.join("notes.md"), "# notes\n").unwrap();
    do_index(&h);
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "notes.md", "--line", "1", "--col", "1",
        ],
    );
    assert_eq!(code, 3, "{out} {err}");
    assert!(err.contains("unsupported-extension"), "{err}");
    assert!(!err.contains("source-changed"), "{err}");
}

#[test]
fn missing_unsupported_destination_fails_path_validation_before_the_refusal() {
    let h = home("missing-destination");
    edge_repo(&h);
    do_index(&h);
    let (code, out, err) = run(
        &h,
        &[
            "trace",
            "--repo",
            "sem",
            "--path",
            "lib.py",
            "--line",
            "1",
            "--col",
            "5",
            "--to",
            "absent.md:1:1",
        ],
    );
    assert_eq!(code, 3, "{out} {err}");
    assert!(err.contains("escapes repository"), "{err}");
    assert!(!err.contains("unsupported-extension"), "{err}");
}

#[test]
fn task4_rejects_zero_positions_and_limit() {
    let h = home("bad-args");
    scip_imported(&h);
    for extra in [
        ["--line", "0"],
        ["--col", "0"],
        ["--limit", "0"],
        ["--limit", "1001"],
        ["--offset", "18446744073709551615"],
    ] {
        let mut args = vec![
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "1", "--col", "6",
        ];
        args.extend(extra);
        let (code, out, err) = run(&h, &args);
        assert_eq!(code, 2, "{extra:?}: {out} {err}");
        assert!(
            err.contains(extra[0]),
            "error must identify the invalid flag: {err}"
        );
    }
}

#[test]
fn task4_pagination_beyond_one_thousand() {
    let h = home("large-page");
    let mut source = String::from("def target():\n    pass\n\ndef caller():\n");
    for _ in 0..1005 {
        source.push_str("    target()\n");
    }
    repo_with(&h, "sem", &[("a.py", &source)]);
    do_index(&h);
    let p0 = query_json(&h, "callers", "a.py", &["--limit", "1000"]);
    assert_eq!(p0["completeness"]["total"], 1005);
    assert_eq!(p0["edges"].as_array().unwrap().len(), 1000);
    assert_eq!(p0["limits"]["next_offset"], 1000);
    let p1 = query_json(
        &h,
        "callers",
        "a.py",
        &["--limit", "1000", "--offset", "1000"],
    );
    assert_eq!(p1["edges"].as_array().unwrap().len(), 5);
    assert_eq!(p1["truncated"], false);
    assert!(p1["limits"]["next_offset"].is_null());
    assert_ne!(
        p0["edges"][999]["caller_span"],
        p1["edges"][0]["caller_span"]
    );
    let end = query_json(&h, "callers", "a.py", &["--offset", "1006"]);
    assert_eq!(end["edges"], serde_json::json!([]));
    assert_eq!(end["completeness"]["total"], 1005);
}

#[test]
fn task4_import_name_is_not_module_identity() {
    let h = home("module-binding");
    repo_with(
        &h,
        "sem",
        &[
            ("lib.py", "def target():\n    pass\n"),
            (
                "other.py",
                "from unrelated import lib\n\ndef caller():\n    target()\n",
            ),
        ],
    );
    do_index(&h);
    let v = query_json(&h, "callers", "lib.py", &[]);
    assert_eq!(
        v["edges"],
        serde_json::json!([]),
        "imported name is not the imported module"
    );
}

fn test_document(path: &str, occurrences: &[Vec<u8>], symbols: &[(&str, &str)]) -> Vec<u8> {
    let mut doc = Vec::new();
    str_field(1, path, &mut doc);
    for o in occurrences {
        len_field(2, o, &mut doc);
    }
    for (uri, name) in symbols {
        let mut info = Vec::new();
        str_field(1, uri, &mut info);
        str_field(6, name, &mut info);
        len_field(3, &info, &mut doc);
    }
    varint_field(6, 1, &mut doc);
    doc
}

fn import_documents(h: &Path, dir: &Path, tool_name: &str, docs: &[Vec<u8>]) {
    import_documents_encoded(h, dir, tool_name, docs, 1);
}

fn import_documents_encoded(
    h: &Path,
    dir: &Path,
    tool_name: &str,
    docs: &[Vec<u8>],
    encoding: u64,
) {
    let mut tool = Vec::new();
    str_field(1, tool_name, &mut tool);
    str_field(2, "1.0", &mut tool);
    let mut md = Vec::new();
    len_field(2, &tool, &mut md);
    str_field(
        3,
        &format!("file://{}", dir.to_string_lossy().replace('\\', "/")),
        &mut md,
    );
    varint_field(4, encoding, &mut md);
    let mut payload = Vec::new();
    len_field(1, &md, &mut payload);
    for doc in docs {
        len_field(2, doc, &mut payload);
    }
    let path = h.join(format!("{tool_name}.scip"));
    std::fs::write(&path, payload).unwrap();
    let (code, _, err) = run(
        h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &path.to_string_lossy(),
        ],
    );
    assert_eq!(code, 0, "{err}");
}

#[test]
fn task4_cross_import_identity_and_ambiguous_anchor() {
    let h = home("import-isolation");
    let dir = edge_repo(&h);
    do_index(&h);
    let target = || {
        test_document(
            "lib.py",
            &[occurrence(T_URI, 1, 0, 4, 10)],
            &[(T_URI, "target")],
        )
    };
    import_documents(&h, &dir, "tool-a", &[target()]);
    let main = test_document(
        "main.py",
        &[
            occurrence(C_URI, 1, 2, 4, 10),
            occurrence(T_URI, 0, 3, 11, 17),
        ],
        &[(T_URI, "target"), (C_URI, "caller")],
    );
    import_documents(&h, &dir, "tool-b", &[main]);
    let v = query_json(&h, "callers", "lib.py", &[]);
    assert!(
        v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "CALLS"),
        "definition from another import must not prove CALLS: {v}"
    );
    assert!(
        v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["provider"] == "syntax" || e["import_id"] == v["anchor"]["import_id"]),
        "semantic candidates must preserve the anchor's import scope: {v}"
    );
    import_documents(&h, &dir, "tool-c", &[target()]);
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "1", "--col", "6",
            "--symbol", T_URI,
        ],
    );
    assert_eq!(code, 3, "{out} {err}");
    assert!(err.contains("ambiguous"), "{err}");
}

#[test]
fn task4_last_page_is_not_full_coverage() {
    let h = home("coverage-page");
    let src = "def target():\n    pass\n\ndef caller():\n    target()\n    target()\n";
    let dir = repo_with(&h, "sem", &[("a.py", src)]);
    do_index(&h);
    let doc = test_document(
        "a.py",
        &[
            occurrence(T_URI, 1, 0, 4, 10),
            occurrence(C_URI, 1, 3, 4, 10),
            occurrence(T_URI, 0, 4, 4, 10),
            occurrence(T_URI, 0, 5, 4, 10),
        ],
        &[(T_URI, "target"), (C_URI, "caller")],
    );
    import_documents(&h, &dir, "coverage", &[doc]);
    let full = query_json(&h, "callers", "a.py", &[]);
    assert_eq!(
        full["completeness"]["status"], "complete",
        "bounded observed closure can be complete: {full}"
    );
    let last = query_json(&h, "callers", "a.py", &["--limit", "1", "--offset", "1"]);
    assert_eq!(last["truncated"], false);
    assert_eq!(last["completeness"]["covered"], 1);
    assert_eq!(last["completeness"]["total"], 2);
    assert_eq!(
        last["completeness"]["status"], "partial",
        "earlier page is still omitted"
    );
}

#[test]
fn task4_unresolved_elsewhere_is_not_proof_of_complete_callers() {
    let h = home("unknown-closure");
    let src = "def target():\n    pass\n\ndef caller():\n    target()\n    mystery()\n";
    let dir = repo_with(&h, "sem", &[("a.py", src)]);
    do_index(&h);
    let doc = test_document(
        "a.py",
        &[
            occurrence(T_URI, 1, 0, 4, 10),
            occurrence(C_URI, 1, 3, 4, 10),
            occurrence(T_URI, 0, 4, 4, 10),
        ],
        &[(T_URI, "target"), (C_URI, "caller")],
    );
    import_documents(&h, &dir, "unknown", &[doc]);
    let v = query_json(&h, "callers", "a.py", &[]);
    assert_eq!(
        v["completeness"]["status"], "partial",
        "a missing callee might target this anchor: {v}"
    );
    assert!(
        v["completeness"]["denominator"]["unresolved_target"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn task4_local_identity_never_crosses_documents() {
    let h = home("local-isolation");
    let source = "def foo():\n    foo()\n";
    let dir = repo_with(&h, "sem", &[("a.py", source), ("b.py", source)]);
    do_index(&h);
    let doc = |path| {
        test_document(
            path,
            &[
                occurrence("local foo", 1, 0, 4, 7),
                occurrence("local foo", 0, 1, 4, 7),
            ],
            &[("local foo", "foo")],
        )
    };
    import_documents(&h, &dir, "locals", &[doc("a.py"), doc("b.py")]);
    for path in ["a.py", "b.py"] {
        let v = query_json(&h, "callers", path, &[]);
        assert_eq!(v["anchor"]["endpoint"]["doc"], path);
        assert_eq!(v["edges"].as_array().unwrap().len(), 1);
        for e in v["edges"].as_array().unwrap() {
            assert_eq!(e["callee"]["doc"], path);
            assert_eq!(e["caller_path"], path);
        }
    }
}

#[test]
fn task4_stale_empty_page_keeps_anchor_verdict() {
    let h = home("stale-empty");
    let dir = scip_imported(&h);
    std::fs::write(dir.join("lib.py"), "def target():\n    return 99\n").unwrap();
    let v = query_json(&h, "callers", "lib.py", &["--offset", "99"]);
    assert_eq!(v["edges"], serde_json::json!([]));
    assert_eq!(v["verification"], "Stale");
    assert_eq!(v["freshness"]["status"], "stale");
    assert_eq!(v["completeness"]["status"], "partial");
    assert!(
        v["completeness"]["denominator"]["stale_imports"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn task4_unicode_terminal_and_unknown_owner_have_no_fabrication() {
    let h = home("unicode-owner");
    let source = "def caller():\n    obj.méthod()\n\nobj.méthod()\n";
    let dir = repo_with(&h, "sem", &[("a.py", source)]);
    do_index(&h);
    // Query the enclosing caller: syntax candidate, no target identity.
    let v = query_json(&h, "callees", "a.py", &[]);
    assert_eq!(v["edges"].as_array().unwrap().len(), 1);
    let e = &v["edges"][0];
    let span = &e["evidence"]["terminal_span"];
    assert_eq!(
        &source.as_bytes()[span[0].as_u64().unwrap() as usize..span[1].as_u64().unwrap() as usize],
        "méthod".as_bytes()
    );
    assert!(e["callee"].is_null());
    assert_eq!(e["kind"], "POTENTIAL_CALL");
    // A reference to an actual definition at module scope cannot gain a caller.
    let uri = "scip-test test pkg 1 a.py/méthod().";
    let doc = test_document(
        "a.py",
        &[occurrence(uri, 1, 0, 4, 10), occurrence(uri, 0, 3, 4, 11)],
        &[(uri, "méthod")],
    );
    import_documents(&h, &dir, "owner", &[doc]);
    let v = query_json(&h, "callers", "a.py", &[]);
    assert!(v["edges"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["kind"] != "CALLS"));
    assert!(v["edges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["caller"].is_null()
            && e["callee"].is_null()
            && e["unresolved"] == "owner-unproven"));
    let unresolved = v["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["unresolved"] == "owner-unproven")
        .unwrap();
    assert_eq!(
        unresolved["evidence"]["imports"].as_array().unwrap().len(),
        1,
        "unknown endpoints must retain their proven producer import"
    );
}

#[test]
fn task4_lsp_is_ephemeral_for_graph_queries() {
    let h = home("ephemeral");
    let source = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\nfn helper() {}\nfn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n";
    let dir = repo_with(&h, "sem", &[("main.rs", source)]);
    do_index(&h);
    let before = query_json(&h, "callees", "main.rs", &[]);
    let mock = mock_lsp::mock_lsp_bin();
    let result = Command::new(env!("CARGO_BIN_EXE_riltra"))
        .args([
            "semantic",
            "query",
            "--repo",
            "sem",
            "--path",
            "main.rs",
            "--line",
            "2",
            "--col",
            "5",
            "--lsp-server",
            &mock.to_string_lossy(),
        ])
        .env("RILTRA_HOME", &h)
        .env("RILTRA_MOCK_LSP", "success")
        .env("RILTRA_MOCK_ROOT", dir.to_string_lossy().replace('\\', "/"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let live: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(live["fallback"].is_null(), "{live}");
    assert_eq!(live["verification"], "Unverified");
    let after = query_json(&h, "callees", "main.rs", &[]);
    assert_eq!(
        before, after,
        "LSP query must not persist graph identity or edges"
    );
    assert_eq!(after["anchor"]["kind"], "syntax-fallback");
    let (code, out, err) = run(&h, &["semantic", "status", "--repo", "sem"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !out.contains("lsp-subprocess"),
        "ephemeral provider cannot persist: {out}"
    );
}

fn normalized_edge_json(mut value: serde_json::Value) -> serde_json::Value {
    let nonce = value["snapshot"]["nonce"].as_str().unwrap().to_string();
    fn visit(value: &mut serde_json::Value, nonce: &str) {
        match value {
            serde_json::Value::String(s) => *s = s.replace(nonce, "<nonce>"),
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, nonce);
                }
            }
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    if key == "artifact_id" {
                        *value = serde_json::json!("<artifact>");
                    } else {
                        visit(value, nonce);
                    }
                }
            }
            _ => {}
        }
    }
    visit(&mut value, &nonce);
    value
}

#[test]
fn task4_full_json_goldens() {
    let h = home("golden");
    let dir = edge_repo(&h);
    do_index(&h);
    let check = |direction, path, extra: &[&str], golden: &str| {
        let actual = normalized_edge_json(query_json(&h, direction, path, extra));
        impact_golden(golden, &actual);
    };
    check("callers", "lib.py", &[], "m6-callers-syntax.json");
    check(
        "callees",
        "main.py",
        &["--line", "3"],
        "m6-callees-syntax.json",
    );
    let scip = scip_for(&dir);
    let (code, _, err) = run(
        &h,
        &["semantic", "import", "--repo", "sem", "--scip", &scip],
    );
    assert_eq!(code, 0, "{err}");
    check("callers", "lib.py", &[], "m6-callers-scip.json");
    check(
        "callees",
        "main.py",
        &["--line", "3"],
        "m6-callees-scip.json",
    );
}

#[test]
fn task4_semantic_callees_keeps_unresolved_syntax_owner_edges() {
    let h = home("semantic-unresolved");
    let dir = repo_with(&h, "sem", &[("a.py", "def caller():\n    mystery()\n")]);
    do_index(&h);
    let doc = test_document(
        "a.py",
        &[occurrence(C_URI, 1, 0, 4, 10)],
        &[(C_URI, "caller")],
    );
    import_documents(&h, &dir, "caller-only", &[doc]);
    let v = query_json(&h, "callees", "a.py", &[]);
    assert_eq!(v["anchor"]["kind"], "semantic");
    assert_eq!(
        v["edges"].as_array().unwrap().len(),
        1,
        "semantic anchor must retain its proven syntax owner's unresolved calls: {v}"
    );
    assert_eq!(v["edges"][0]["kind"], "POTENTIAL_CALL");
    assert_eq!(v["edges"][0]["callee_name"], "mystery");
    assert!(v["edges"][0]["callee"].is_null());
}

#[test]
fn task4_semantic_anchor_uses_byte_columns_for_all_scip_encodings() {
    for encoding in [1, 2, 3] {
        let h = home(&format!("encoding-{encoding}"));
        let dir = repo_with(
            &h,
            "sem",
            &[("a.py", "# prefix\ndef méthod():\n    pass\n")],
        );
        do_index(&h);
        // Identifier raw UTF8 bytes are [13,20); UTF16/UTF32 producer cols end at 10.
        let end = if encoding == 1 { 11 } else { 10 };
        let mut doc = test_document(
            "a.py",
            &[occurrence(C_URI, 1, 1, 4, end)],
            &[(C_URI, "méthod")],
        );
        varint_field(6, encoding, &mut doc);
        import_documents(&h, &dir, "positions", &[doc]);
        let v = query_json(&h, "callees", "a.py", &["--line", "2", "--col", "11"]);
        assert_eq!(
            v["anchor"]["kind"], "semantic",
            "last UTF8 column is still inside the identifier: encoding {encoding}: {v}"
        );
        let end = query_json(&h, "callees", "a.py", &["--line", "2", "--col", "12"]);
        assert_eq!(
            end["anchor"]["kind"], "syntax-fallback",
            "half-open byte boundary"
        );
        std::fs::write(dir.join("a.py"), "# prefix\ndef méthod():\n    return 99\n").unwrap();
        let stale = query_json(&h, "callees", "a.py", &["--line", "2", "--col", "11"]);
        assert_eq!(
            stale["anchor"]["kind"], "semantic",
            "snapshot syntax coordinates retain the old byte position"
        );
        assert_eq!(stale["verification"], "Stale");
    }
}

#[test]
fn task4_utf16_disk_source_keeps_raw_byte_evidence() {
    let h = home("utf16-disk");
    let dir = repo_with(&h, "sem", &[("a.py", "placeholder")]);
    let text = "# prefix\ndef méthod():\n    pass\n";
    let raw: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    std::fs::write(dir.join("a.py"), &raw).unwrap();
    do_index(&h);
    let mut doc = test_document(
        "a.py",
        &[occurrence(C_URI, 1, 1, 4, 10)],
        &[(C_URI, "méthod")],
    );
    varint_field(6, 2, &mut doc);
    import_documents_encoded(&h, &dir, "utf16", &[doc], 2);
    let v = query_json(&h, "callees", "a.py", &["--line", "2", "--col", "11"]);
    assert_eq!(v["anchor"]["kind"], "semantic");
    let start = v["anchor"]["span"][0].as_u64().unwrap() as usize;
    let end = v["anchor"]["span"][1].as_u64().unwrap() as usize;
    let units: Vec<u16> = raw[start..end]
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    assert_eq!(String::from_utf16(&units).unwrap(), "méthod");
}

#[test]
fn task4_binary_source_is_an_explicit_coverage_gap() {
    let h = home("binary-coverage");
    let src = "def target():\n    pass\n\ndef caller():\n    target()\n";
    let dir = repo_with(&h, "sem", &[("a.py", src)]);
    std::fs::write(dir.join("binary.py"), b"\0not inspectable code").unwrap();
    do_index(&h);
    let doc = test_document(
        "a.py",
        &[
            occurrence(T_URI, 1, 0, 4, 10),
            occurrence(C_URI, 1, 3, 4, 10),
            occurrence(T_URI, 0, 4, 4, 10),
        ],
        &[(T_URI, "target"), (C_URI, "caller")],
    );
    import_documents(&h, &dir, "binary", &[doc]);
    let v = query_json(&h, "callers", "a.py", &[]);
    assert_eq!(
        v["completeness"]["status"], "partial",
        "skipped code prevents completeness proof: {v}"
    );
    assert!(
        v["completeness"]["denominator"]["skipped_files"]
            .as_u64()
            .unwrap()
            > 0
    );
}

fn trace_fixture(h: &Path, graph: &[Vec<usize>]) -> Vec<usize> {
    trace_fixture_with(h, graph, &[])
}

/// `trace_fixture` plus `extra` files that are indexed but left out of the SCIP
/// import, so their calls keep syntax-only `POTENTIAL_CALL` rows.
fn trace_fixture_with(h: &Path, graph: &[Vec<usize>], extra: &[(&str, &str)]) -> Vec<usize> {
    let names: Vec<String> = (0..graph.len()).map(|i| format!("f{i}")).collect();
    let uris: Vec<String> = names
        .iter()
        .map(|n| format!("scip-test test test-pkg 1.0.0 graph.py/{n}()."))
        .collect();
    let mut src = String::new();
    let mut occurrences = Vec::new();
    let mut lines = Vec::new();
    let mut line = 0;
    for (i, targets) in graph.iter().enumerate() {
        lines.push(line + 1);
        src.push_str(&format!("def {}():\n", names[i]));
        occurrences.push(occurrence(
            &uris[i],
            1,
            line as i64,
            4,
            (4 + names[i].len()) as i64,
        ));
        line += 1;
        if targets.is_empty() {
            src.push_str("    pass\n");
            line += 1;
        }
        for target in targets {
            src.push_str(&format!("    {}()\n", names[*target]));
            occurrences.push(occurrence(
                &uris[*target],
                0,
                line as i64,
                4,
                (4 + names[*target].len()) as i64,
            ));
            line += 1;
        }
        src.push('\n');
        line += 1;
    }
    let mut files = vec![("graph.py", src.as_str())];
    files.extend_from_slice(extra);
    let dir = repo_with(h, "sem", &files);
    do_index(h);
    let symbols: Vec<_> = uris
        .iter()
        .zip(names.iter())
        .map(|(u, n)| (u.as_str(), n.as_str()))
        .collect();
    import_documents(
        h,
        &dir,
        "trace",
        &[test_document("graph.py", &occurrences, &symbols)],
    );
    lines
}

fn trace_json(h: &Path, line: usize, extra: &[&str]) -> serde_json::Value {
    let line = line.to_string();
    let mut args = vec![
        "trace", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
    ];
    args.extend_from_slice(extra);
    let (code, out, err) = run(h, &args);
    assert_eq!(code, 0, "{out} {err}");
    serde_json::from_str(&out).expect("trace JSON")
}

#[test]
fn task5_trace_destination_and_outbound_chain() {
    let h = home("trace-chain");
    let lines = trace_fixture(&h, &[vec![1], vec![2], vec![]]);
    let target = format!("graph.py:{}:6", lines[2]);
    let v = trace_json(&h, lines[0], &["--to", &target]);
    assert_eq!(v["disclaimer"], "plausible-path-not-execution");
    assert_eq!(v["paths"].as_array().unwrap().len(), 1);
    assert_eq!(v["paths"][0]["edges"].as_array().unwrap().len(), 2);
    assert_eq!(v["paths"][0]["edges"][0]["kind"], "CALLS");
    assert_eq!(
        v["paths"][0]["edges"][1]["evidence"]["imports"][0]["provider"],
        "trace"
    );
    assert_eq!(v["verification"], "Unverified");
    let enumeration = trace_json(&h, lines[0], &["--depth", "2"]);
    assert_eq!(enumeration["paths"].as_array().unwrap().len(), 1);
    assert_eq!(
        enumeration["paths"][0]["edges"].as_array().unwrap().len(),
        2
    );
    assert_eq!(enumeration["truncated"], false);
}

#[test]
fn task5_trace_cycle_depth_and_unreachable_are_explicit() {
    let h = home("trace-cycle");
    let lines = trace_fixture(&h, &[vec![1], vec![0], vec![]]);
    let cycle = trace_json(&h, lines[0], &[]);
    assert_eq!(cycle["paths"][0]["cycle"], true);
    assert!(cycle["visits"]["nodes"].as_u64().unwrap() <= 500);
    let target = format!("graph.py:{}:6", lines[2]);
    let absent = trace_json(&h, lines[0], &["--to", &target]);
    assert_eq!(absent["paths"], serde_json::json!([]));
    assert!(absent["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "target-unreachable"));
    let short = trace_json(&h, lines[0], &["--depth", "1"]);
    assert_eq!(short["truncated"], true);
    assert!(short["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "depth-budget"));
}

#[test]
fn task5_trace_diamond_pagination_and_repeatability() {
    let h = home("trace-diamond");
    let lines = trace_fixture(&h, &[vec![1, 2], vec![3], vec![3], vec![]]);
    let target = format!("graph.py:{}:6", lines[3]);
    let full = trace_json(&h, lines[0], &["--to", &target]);
    assert_eq!(
        full["paths"].as_array().unwrap().len(),
        2,
        "shared endpoint is not a cycle"
    );
    let p0 = trace_json(&h, lines[0], &["--to", &target, "--limit", "1"]);
    let p1 = trace_json(
        &h,
        lines[0],
        &["--to", &target, "--limit", "1", "--offset", "1"],
    );
    assert_eq!(p0["paths"][0], full["paths"][0]);
    assert_eq!(p1["paths"][0], full["paths"][1]);
    assert_eq!(p0["limits"]["next_offset"], 1);
    assert!(p1["limits"]["next_offset"].is_null());
    assert_eq!(full, trace_json(&h, lines[0], &["--to", &target]));
}

#[test]
fn task5_trace_syntax_unknown_target_is_not_a_path_to_another_name() {
    let h = home("trace-syntax");
    repo_with(
        &h,
        "sem",
        &[("graph.py", "def f0():\n    f1()\n\ndef f1():\n    pass\n")],
    );
    do_index(&h);
    let v = trace_json(&h, 1, &[]);
    assert_eq!(v["anchor"]["kind"], "syntax-fallback");
    assert_eq!(v["paths"][0]["edges"][0]["kind"], "POTENTIAL_CALL");
    assert!(v["paths"][0]["edges"][0]["callee"].is_null());
    let to = trace_json(&h, 1, &["--to", "graph.py:4:6"]);
    assert_eq!(to["paths"], serde_json::json!([]));
    assert!(to["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "unresolved-target"));
}

#[test]
fn task5_trace_verified_syntax_bridge_and_stale_label() {
    let h = home("trace-bridge");
    let lines = trace_fixture(&h, &[vec![1], vec![2], vec![]]);
    // Whitespace in f0's body has no semantic occurrence: a syntax fallback
    // can connect to the persisted semantic owner only by validated mapping.
    let v = trace_json(
        &h,
        lines[0],
        &["--line", &(lines[0] + 1).to_string(), "--col", "1"],
    );
    assert_eq!(v["anchor"]["kind"], "syntax-fallback");
    assert_eq!(v["paths"][0]["edges"].as_array().unwrap().len(), 2);
    assert!(!v["bridges"].as_array().unwrap().is_empty());
    let file = h.join("sem/graph.py");
    let mut source = std::fs::read_to_string(&file).unwrap();
    source.push_str("# edit\n");
    std::fs::write(file, source).unwrap();
    let stale = trace_json(&h, lines[0], &[]);
    assert_eq!(stale["verification"], "Stale");
}

#[test]
fn task5_trace_rejects_invalid_depth_and_destination() {
    let h = home("trace-invalid");
    trace_fixture(&h, &[vec![]]);
    for extra in [
        ["--depth", "0"],
        ["--depth", "7"],
        ["--to", "bad-position"],
        ["--to", "graph.py:0:1"],
        ["--limit", "0"],
    ] {
        let mut args = vec![
            "trace", "--repo", "sem", "--path", "graph.py", "--line", "1", "--col", "6",
        ];
        args.extend(extra);
        let (code, out, err) = run(&h, &args);
        assert_eq!(code, 2, "{extra:?}: {out} {err}");
    }
}

#[test]
fn task5_trace_destination_fallback_and_max_depth_boundary() {
    let h = home("trace-max-depth");
    let lines = trace_fixture(
        &h,
        &[
            vec![1],
            vec![2],
            vec![3],
            vec![4],
            vec![5],
            vec![6],
            vec![7],
            vec![],
        ],
    );
    let target = format!("graph.py:{}:6", lines[6]);
    let exact = trace_json(&h, lines[0], &["--depth", "6", "--to", &target]);
    assert_eq!(exact["paths"][0]["edges"].as_array().unwrap().len(), 6);
    assert_eq!(
        exact["truncated"], false,
        "reached destination needs no expansion beyond depth"
    );
    let later = format!("graph.py:{}:6", lines[7]);
    let bounded = trace_json(&h, lines[0], &["--depth", "6", "--to", &later]);
    assert_eq!(bounded["paths"], serde_json::json!([]));
    assert_eq!(bounded["truncated"], true);
    assert!(!bounded["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "target-unreachable"));
    let whitespace = format!("graph.py:{}:1", lines[6] + 1);
    let fallback = trace_json(&h, lines[0], &["--depth", "6", "--to", &whitespace]);
    assert_eq!(fallback["destination"]["kind"], "syntax-fallback");
    assert_eq!(fallback["paths"][0]["edges"].as_array().unwrap().len(), 6);
}

#[test]
fn task5_trace_ambiguous_import_mapping_does_not_join() {
    let h = home("trace-imports");
    let lines = trace_fixture(&h, &[vec![1], vec![]]);
    let f0 = "scip-test test test-pkg 1.0.0 graph.py/f0().";
    let f1 = "scip-test test test-pkg 1.0.0 graph.py/f1().";
    let doc = test_document(
        "graph.py",
        &[
            occurrence(f0, 1, 0, 4, 6),
            occurrence(f1, 0, 1, 4, 6),
            occurrence(f1, 1, 3, 4, 6),
        ],
        &[(f0, "f0"), (f1, "f1")],
    );
    import_documents(&h, &h.join("sem"), "trace-second", &[doc]);
    let v = trace_json(&h, lines[0], &["--line", "2", "--col", "1"]);
    assert_eq!(v["anchor"]["kind"], "syntax-fallback");
    assert_eq!(v["paths"], serde_json::json!([]));
    assert!(v["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "ambiguous-owner-mapping"));
    let (code, _, err) = run(
        &h,
        &[
            "trace", "--repo", "sem", "--path", "graph.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 3);
    assert!(err.contains("ambiguous"));
}

#[test]
fn task5_trace_local_cycles_stay_in_document() {
    let h = home("trace-locals");
    let src = "def f0():\n    f0()\n";
    let dir = repo_with(&h, "sem", &[("graph.py", src), ("other.py", src)]);
    do_index(&h);
    let doc = |path| {
        test_document(
            path,
            &[
                occurrence("local f0", 1, 0, 4, 6),
                occurrence("local f0", 0, 1, 4, 6),
            ],
            &[("local f0", "f0")],
        )
    };
    import_documents(&h, &dir, "local-trace", &[doc("graph.py"), doc("other.py")]);
    let v = trace_json(&h, 1, &[]);
    assert_eq!(v["visits"]["nodes"], 1);
    assert_eq!(v["paths"][0]["cycle"], true);
    assert_eq!(v["paths"][0]["edges"][0]["callee"]["doc"], "graph.py");
    assert!(v["bridges"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b["syntax"]["path"] == "graph.py"));
}

#[test]
fn task5_trace_full_json_goldens() {
    let syntax_home = home("trace-golden-syntax");
    repo_with(
        &syntax_home,
        "sem",
        &[(
            "graph.py",
            "def f0():\n    f1()\n\ndef f1():\n    f2()\n\ndef f2():\n    pass\n\n",
        )],
    );
    do_index(&syntax_home);
    let syntax = normalized_edge_json(trace_json(&syntax_home, 1, &[]));
    impact_golden("m6-trace-syntax.json", &syntax);

    let semantic_home = home("trace-golden-scip");
    trace_fixture(&semantic_home, &[vec![1], vec![2], vec![]]);
    let semantic = normalized_edge_json(trace_json(&semantic_home, 1, &["--to", "graph.py:7:6"]));
    impact_golden("m6-trace-scip.json", &semantic);
}

fn impact_json(h: &Path, line: usize, extra: &[&str]) -> serde_json::Value {
    let line = line.to_string();
    let mut args = vec![
        "impact", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
    ];
    args.extend_from_slice(extra);
    let (code, out, err) = run(h, &args);
    assert_eq!(code, 0, "{out} {err}");
    serde_json::from_str(&out).expect("impact JSON")
}

fn assert_unknown_impact(v: &serde_json::Value) {
    assert_eq!(v["verdict"], "UNKNOWN", "{v}");
    assert_eq!(v["completeness"]["status"], "partial", "{v}");
    assert!(!v.to_string().contains("\"SAFE\""));
    assert!(!v.to_string().contains("\"LOW\""));
}

#[test]
fn task6_impact_direct_and_transitive_are_proven_without_reindex() {
    let h = home("impact-chain");
    let lines = trace_fixture(&h, &[vec![1], vec![2], vec![]]);
    let v = impact_json(&h, lines[2], &[]);
    assert_eq!(v["direction"], "impact");
    assert_eq!(v["verdict"], "HIGH");
    assert_eq!(v["verification"], "Unverified");
    assert_eq!(v["completeness"]["status"], "partial");
    assert_eq!(v["paths"].as_array().unwrap().len(), 2);
    assert_eq!(v["paths"][0]["edges"].as_array().unwrap().len(), 1);
    assert_eq!(v["paths"][1]["edges"].as_array().unwrap().len(), 2);
    assert_eq!(v["paths"][0]["scope"], "direct");
    assert_eq!(v["paths"][1]["scope"], "transitive");
    assert_eq!(v["paths"][1]["distance"], 2);
    assert_eq!(v["paths"][0]["edges"][0]["callee_name"], "f2");
    assert_eq!(v["paths"][1]["edges"][1]["callee_name"], "f1");
    for path in v["paths"].as_array().unwrap() {
        for edge in path["edges"].as_array().unwrap() {
            assert_eq!(edge["kind"], "CALLS");
            assert_eq!(edge["precision"], "Exact");
            assert_eq!(edge["evidence"]["imports"][0]["provider"], "trace");
        }
    }
    assert_eq!(v["disclaimer"], "plausible-impact-not-execution");
    assert_eq!(v, impact_json(&h, lines[2], &[]));
}

#[test]
fn task6_impact_zero_false_safe_syntax_corpus() {
    // The third field counts the calls to `f0` in the source: each is one candidate.
    let cases = [
        ("leaf", "def f0():\n    pass\n", 0),
        ("recursion", "def f0():\n    f0()\n", 1),
        ("cycle", "def f0():\n    f1()\n\ndef f1():\n    f0()\n", 1),
        ("dynamic", "def f0():\n    obj.run()\n", 0),
        ("missing", "def f0():\n    missing()\n", 0),
        (
            "same-name",
            "def f0():\n    pass\n\nclass C:\n    def f0(self):\n        pass\n",
            0,
        ),
        ("owner-unproven", "def f0():\n    pass\n\nf0()\n", 1),
        ("alias", "def f0():\n    pass\n\na = f0\na()\n", 0),
    ];
    for (name, source, f0_calls) in cases {
        let h = home(&format!("impact-corpus-{name}"));
        repo_with(&h, "sem", &[("graph.py", source)]);
        do_index(&h);
        let v = impact_json(&h, 1, &[]);
        assert_unknown_impact(&v);
        assert_eq!(v["anchor"]["kind"], "syntax-fallback");
        // A name alone may list candidates (`candidate-name`), never a proven link.
        let paths = v["paths"].as_array().unwrap();
        assert!(
            paths.iter().all(|p| p["basis"] == "candidate-name"),
            "no proven name linking: {v}"
        );
        assert_eq!(
            paths.len(),
            f0_calls,
            "{name}: one candidate per call to f0: {v}"
        );
    }
}

#[test]
fn task6_impact_unknown_for_stale_depth_and_coverage_gaps() {
    for kind in ["stale", "depth", "unsupported", "missing-provider", "leaf"] {
        let h = home(&format!("impact-unknown-{kind}"));
        let lines = trace_fixture(&h, &[vec![1], vec![2], vec![]]);
        let dir = h.join("sem");
        match kind {
            "stale" => {
                let mut source = std::fs::read_to_string(dir.join("graph.py")).unwrap();
                source.push_str("# edited\n");
                std::fs::write(dir.join("graph.py"), source).unwrap();
            }
            "unsupported" => {
                std::fs::write(dir.join("unknown.xyz"), "unsupported code").unwrap();
                do_index(&h);
            }
            "missing-provider" => {
                std::fs::write(dir.join("extra.py"), "def extra():\n    pass\n").unwrap();
                do_index(&h);
            }
            _ => {}
        }
        let extra = if kind == "depth" {
            vec!["--depth", "1"]
        } else {
            vec![]
        };
        let line = if kind == "leaf" { lines[0] } else { lines[2] };
        let v = impact_json(&h, line, &extra);
        assert_unknown_impact(&v);
        if kind == "stale" {
            assert_eq!(v["verification"], "Stale");
        }
        if kind == "depth" {
            assert_eq!(v["truncated"], true);
        }
        if kind == "unsupported" || kind == "missing-provider" {
            let field = if kind == "unsupported" {
                "unsupported_files"
            } else {
                "missing_provider_files"
            };
            assert_eq!(v["completeness"]["denominator"][field], 1, "{v}");
        }
    }
}

#[test]
fn task6_impact_cycles_diamond_pagination_and_empty_pages() {
    let h = home("impact-diamond");
    let lines = trace_fixture(&h, &[vec![1, 2], vec![3], vec![3], vec![]]);
    let full = impact_json(&h, lines[3], &[]);
    assert_eq!(full["verdict"], "HIGH");
    assert_eq!(full["paths"].as_array().unwrap().len(), 4);
    let first = impact_json(&h, lines[3], &["--limit", "1"]);
    assert_unknown_impact(&first);
    assert_eq!(first["limits"]["next_offset"], 1);
    let next = impact_json(&h, lines[3], &["--offset", "1"]);
    assert_eq!(first["paths"][0], full["paths"][0]);
    assert_eq!(next["paths"][0], full["paths"][1]);
    let empty = impact_json(&h, lines[3], &["--offset", "4"]);
    assert_unknown_impact(&empty);
    assert_eq!(empty["paths"], serde_json::json!([]));
    let h = home("impact-cycle");
    trace_fixture(&h, &[vec![1], vec![0]]);
    let cycle = impact_json(&h, 1, &[]);
    assert_eq!(cycle["cycle"], true);
    assert_eq!(cycle["verdict"], "HIGH");
    assert!(cycle["visits"]["nodes"].as_u64().unwrap() <= 500);
}

#[test]
fn task6_impact_reference_unknown_owner_retains_site_evidence() {
    let h = home("impact-reference");
    let dir = repo_with(
        &h,
        "sem",
        &[("graph.py", "def f0():\n    pass\n\nx = f0\n")],
    );
    do_index(&h);
    let uri = "scip-test test test-pkg 1.0.0 graph.py/f0().";
    import_documents(
        &h,
        &dir,
        "refs",
        &[test_document(
            "graph.py",
            &[occurrence(uri, 1, 0, 4, 6), occurrence(uri, 0, 3, 4, 6)],
            &[(uri, "f0")],
        )],
    );
    let v = impact_json(&h, 1, &[]);
    assert_unknown_impact(&v);
    assert_eq!(v["paths"][0]["edges"][0]["kind"], "REFERENCES");
    assert!(v["paths"][0]["edges"][0]["caller"].is_null());
    assert_eq!(
        v["paths"][0]["edges"][0]["evidence"]["span"],
        serde_json::json!([24, 26])
    );
}

#[test]
fn task6_impact_unmapped_midpoint_is_explicitly_unknown() {
    let h = home("impact-midpoint");
    let dir = repo_with(
        &h,
        "sem",
        &[("graph.py", "def f0():\n    f1()\n\ndef f1():\n    pass\n")],
    );
    do_index(&h);
    let uri = "scip-test test test-pkg 1.0.0 graph.py/f1().";
    // Target proof exists, callable owner has syntax identity only. Its name
    // cannot bridge to a semantic definition absent from this import.
    import_documents(
        &h,
        &dir,
        "midpoint",
        &[test_document(
            "graph.py",
            &[occurrence(uri, 0, 1, 4, 6), occurrence(uri, 1, 3, 4, 6)],
            &[(uri, "f1")],
        )],
    );
    let v = impact_json(&h, 4, &[]);
    assert_unknown_impact(&v);
    assert_eq!(v["paths"][0]["edges"][0]["kind"], "CALLS");
    assert!(v["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "semantic-midpoint-unavailable"));
}

#[test]
fn task6_impact_ambiguous_semantic_dispatch_counts_unknowns() {
    let h = home("impact-ambiguous");
    let dir = repo_with(
        &h,
        "sem",
        &[(
            "graph.py",
            "def f0():\n    f1()\n\ndef f1():\n    pass\n\ndef f2():\n    pass\n",
        )],
    );
    do_index(&h);
    let uris: Vec<_> = (0..3)
        .map(|n| format!("scip-test test test-pkg 1.0.0 graph.py/f{n}()."))
        .collect();
    import_documents(
        &h,
        &dir,
        "ambiguous",
        &[test_document(
            "graph.py",
            &[
                occurrence(&uris[0], 1, 0, 4, 6),
                occurrence(&uris[1], 0, 1, 4, 6),
                occurrence(&uris[2], 0, 1, 4, 6),
                occurrence(&uris[1], 1, 3, 4, 6),
                occurrence(&uris[2], 1, 6, 4, 6),
            ],
            &[(&uris[0], "f0"), (&uris[1], "f1"), (&uris[2], "f2")],
        )],
    );
    let v = impact_json(&h, 4, &[]);
    assert_unknown_impact(&v);
    assert_eq!(v["completeness"]["denominator"]["ambiguous_calls"], 1);
    assert_eq!(v["completeness"]["denominator"]["unresolved_target"], 1);
    // The ambiguous call is the one name-matched candidate (C1 lists SCIP
    // anchors' candidates); no proven or unproven path may come from it.
    let paths = v["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 1, "{v}");
    assert_eq!(paths[0]["basis"], "candidate-name", "{v}");
}

#[test]
fn task6_impact_owner_mapping_import_and_local_scope() {
    let h = home("impact-bridge");
    let lines = trace_fixture(&h, &[vec![1], vec![]]);
    let fallback = impact_json(&h, lines[1], &["--line", "5", "--col", "1"]);
    assert_unknown_impact(&fallback);
    assert_eq!(fallback["paths"][0]["edges"][0]["kind"], "CALLS");
    assert!(!fallback["bridges"].as_array().unwrap().is_empty());
    let f0 = "scip-test test test-pkg 1.0.0 graph.py/f0().";
    let f1 = "scip-test test test-pkg 1.0.0 graph.py/f1().";
    import_documents(
        &h,
        &h.join("sem"),
        "second",
        &[test_document(
            "graph.py",
            &[
                occurrence(f0, 1, 0, 4, 6),
                occurrence(f1, 0, 1, 4, 6),
                occurrence(f1, 1, 3, 4, 6),
            ],
            &[(f0, "f0"), (f1, "f1")],
        )],
    );
    let ambiguous = impact_json(&h, 5, &["--col", "1"]);
    assert_unknown_impact(&ambiguous);
    assert_eq!(ambiguous["paths"], serde_json::json!([]));
    let (code, _, err) = run(
        &h,
        &[
            "impact", "--repo", "sem", "--path", "graph.py", "--line", "4", "--col", "6",
        ],
    );
    assert_eq!(code, 3);
    assert!(err.contains("ambiguous"));

    let h = home("impact-local");
    let src = "def f0():\n    f0()\n";
    let dir = repo_with(&h, "sem", &[("graph.py", src), ("other.py", src)]);
    do_index(&h);
    let doc = |p| {
        test_document(
            p,
            &[
                occurrence("local f0", 1, 0, 4, 6),
                occurrence("local f0", 0, 1, 4, 6),
            ],
            &[("local f0", "f0")],
        )
    };
    import_documents(&h, &dir, "locals", &[doc("graph.py"), doc("other.py")]);
    let v = impact_json(&h, 1, &[]);
    assert_eq!(v["verdict"], "HIGH");
    assert_eq!(v["visits"]["nodes"], 1);
    assert_eq!(v["paths"].as_array().unwrap().len(), 1);
    assert_eq!(v["paths"][0]["edges"][0]["caller"]["doc"], "graph.py");
}

#[test]
fn task6_impact_rejects_bad_bounds_destination_and_escaping_path() {
    let h = home("impact-args");
    trace_fixture(&h, &[vec![]]);
    for extra in [
        vec!["--depth", "0"],
        vec!["--depth", "7"],
        vec!["--limit", "1001"],
        vec!["--to", "graph.py:1:6"],
    ] {
        let mut args = vec![
            "impact", "--repo", "sem", "--path", "graph.py", "--line", "1", "--col", "6",
        ];
        args.extend(extra);
        assert_eq!(run(&h, &args).0, 2);
    }
    assert_eq!(
        run(
            &h,
            &[
                "impact",
                "--repo",
                "sem",
                "--path",
                "../outside.py",
                "--line",
                "1",
                "--col",
                "6"
            ]
        )
        .0,
        3
    );
}

#[test]
fn task6_impact_full_json_goldens() {
    let h = home("impact-golden-syntax");
    repo_with(
        &h,
        "sem",
        &[(
            "graph.py",
            "def f0():\n    f1()\n\ndef f1():\n    f2()\n\ndef f2():\n    pass\n\n",
        )],
    );
    do_index(&h);
    let syntax = normalized_edge_json(impact_json(&h, 7, &[]));
    // Truth first: the syntax-only caller f1 is a name-matched candidate, never proven.
    assert_eq!(syntax["verdict"], "UNKNOWN", "{syntax}");
    let paths = syntax["paths"].as_array().unwrap();
    assert!(!paths.is_empty(), "{syntax}");
    assert!(
        paths.iter().all(|p| p["basis"] == "candidate-name"),
        "{syntax}"
    );
    impact_golden("m6-impact-syntax.json", &syntax);
    let h = home("impact-golden-scip");
    trace_fixture(&h, &[vec![1], vec![2], vec![]]);
    for (extra, name) in [
        (vec![], "m6-impact-scip.json"),
        (vec!["--depth", "1"], "m6-impact-truncated.json"),
    ] {
        let actual = normalized_edge_json(impact_json(&h, 7, &extra));
        // Truth first: the SCIP chain is proven and lists no candidates.
        let paths = actual["paths"].as_array().unwrap();
        assert!(paths.iter().all(|p| p["basis"] == "proven"), "{actual}");
        assert_eq!(actual["candidates"]["returned"], 0, "{actual}");
        impact_golden(name, &actual);
    }
}

/// The golden contract for the M6 and task4-6 files: after the truth assertions, `RILTRA_UPDATE_M6_GOLDENS=1`
/// rewrites the file (same writer as the M6 corpus), then the file is compared.
fn impact_golden(name: &str, actual: &serde_json::Value) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("RILTRA_UPDATE_M6_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::write(&path, serde_json::to_string_pretty(actual).unwrap() + "\n").unwrap();
    }
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(actual, &expected, "{name} differs");
}

// --- Task 3A: candidates from a SCIP-resolved anchor and at the depth boundary ---

#[test]
fn task3a_scip_anchor_lists_its_name_matched_callers_as_candidates() {
    let h = home("task3a-scip-anchor");
    let lines = trace_fixture_with(
        &h,
        &[vec![1], vec![2], vec![]],
        &[("extra.py", "from graph import f2\n\n\ndef g():\n    f2()\n")],
    );
    let line = lines[2].to_string();
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "graph.py", "--line", &line, "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let callers: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        callers["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "POTENTIAL_CALL" && e["caller_path"] == "extra.py"),
        "precondition: callers lists the name-matched call: {out}"
    );
    let v = impact_json(&h, lines[2], &[]);
    assert_eq!(v["anchor"]["kind"], "semantic", "{v}");
    let paths = v["paths"].as_array().unwrap();
    assert!(
        paths
            .iter()
            .any(|p| p["basis"] == "candidate-name" && p["edges"][0]["caller_path"] == "extra.py"),
        "the SCIP anchor must list its name-matched caller: {v}"
    );
    assert!(paths.iter().any(|p| p["basis"] == "proven"), "{v}");
}

#[test]
fn task3a_depth_boundary_probe_states_a_candidate_beyond_depth() {
    let h = home("task3a-depth-boundary");
    // f1 is proven to call f2; g calls f1 by name only, so the chain reaches
    // beyond `--depth 1` through a candidate edge alone.
    let lines = trace_fixture_with(
        &h,
        &[vec![], vec![2], vec![]],
        &[("extra.py", "from graph import f1\n\n\ndef g():\n    f1()\n")],
    );
    let full = impact_json(&h, lines[2], &[]);
    assert!(
        full["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["basis"] == "candidate-name" && p["distance"] == 2),
        "precondition: g is a candidate two hops out: {full}"
    );
    let v = impact_json(&h, lines[2], &["--depth", "1"]);
    assert_eq!(v["paths"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["paths"][0]["basis"], "proven", "{v}");
    assert_eq!(v["candidates"]["complete"], false, "{v}");
    assert!(v["candidates"]["total"].is_null(), "{v}");
    assert!(
        v["unresolved"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "depth-budget"),
        "{v}"
    );
}

// --- M9 CP3: E8 - C/C++ include resolution in the candidate scope ---------
//
// A C/C++ include is written against an include root the repository does not
// record, so its literal rarely equals the target's repository-relative path.
// The conservative rule: a UNIQUE ROOTED path suffix may widen the caller
// candidate scope, and it never proves an identity. Ambiguous, missing and
// bare-filename includes stay unresolved.

const E8_HEADER: &str = "int Thing(int x) { return x + 1; }\n";
const E8_CALLER: &str = "#include \"proj/thing.h\"\n\nint go() { return Thing(1); }\n";

fn e8_callers(h: &Path, path: &str) -> serde_json::Value {
    let (code, out, err) = run(
        h,
        &[
            "callers", "--repo", "sem", "--path", path, "--line", "1", "--col", "5",
        ],
    );
    assert_eq!(code, 0, "{err}");
    serde_json::from_str(&out).expect("valid JSON")
}

fn e8_edges(v: &serde_json::Value) -> &Vec<serde_json::Value> {
    v["edges"].as_array().expect("edges array")
}

#[test]
fn e8_rooted_include_adds_candidate_caller() {
    let h = home("e8root");
    repo_with(
        &h,
        "sem",
        &[
            ("src/proj/thing.h", E8_HEADER),
            ("src/proj/main.cc", E8_CALLER),
        ],
    );
    do_index(&h);
    let v = e8_callers(&h, "src/proj/thing.h");
    let edges = e8_edges(&v);
    assert_eq!(edges.len(), 1, "{v}");
    let e = &edges[0];
    assert_eq!(e["kind"], "POTENTIAL_CALL");
    assert_eq!(e["callee_name"], "Thing");
    assert_eq!(e["caller_path"], "src/proj/main.cc");
    assert_eq!(e["precision"], "Heuristic");
    assert!(
        e["unresolved"].is_string(),
        "a candidate stays unresolved: {v}"
    );
    assert!(
        e["callee"].is_null(),
        "a candidate never proves identity: {v}"
    );
    assert!(e["evidence"]["path"].is_string(), "source provenance: {v}");
}

#[test]
fn e8_candidate_scope_keeps_the_anchor_name_filter() {
    let h = home("e8name");
    repo_with(
        &h,
        "sem",
        &[
            ("src/proj/thing.h", E8_HEADER),
            (
                "src/proj/main.cc",
                "#include \"proj/thing.h\"\n\nint go() { return Thing(1) + Other(1); }\n",
            ),
        ],
    );
    do_index(&h);
    let v = e8_callers(&h, "src/proj/thing.h");
    let edges = e8_edges(&v);
    // The importing file IS in the candidate scope, but only the rows whose
    // callee name is the anchor's name are selected.
    assert_eq!(edges.len(), 1, "{v}");
    assert_eq!(edges[0]["callee_name"], "Thing");
    assert_eq!(edges[0]["caller_path"], "src/proj/main.cc");
    assert!(
        !edges.iter().any(|e| e["callee_name"] == "Other"),
        "the anchor's name must filter the candidate scope: {v}"
    );
}

#[test]
fn e8_ambiguous_suffix_stays_unresolved() {
    let h = home("e8ambig");
    repo_with(
        &h,
        "sem",
        &[
            ("src/proj/thing.h", E8_HEADER),
            ("src/proj/main.cc", E8_CALLER),
            // A second file with the same suffix: the literal names two.
            ("other/proj/thing.h", E8_HEADER),
        ],
    );
    do_index(&h);
    let v = e8_callers(&h, "src/proj/thing.h");
    assert_eq!(e8_edges(&v).len(), 0, "ambiguity is never resolved: {v}");
}

#[test]
fn e8_misleading_bare_filename_is_not_a_candidate() {
    let h = home("e8bare");
    repo_with(
        &h,
        "sem",
        &[
            ("src/util/thing.h", E8_HEADER),
            // A bare literal is the one form a system header can shadow.
            (
                "src/thirdparty/lbp.c",
                "#include \"thing.h\"\n\nint go() { return Thing(1); }\n",
            ),
        ],
    );
    do_index(&h);
    let v = e8_callers(&h, "src/util/thing.h");
    assert_eq!(
        e8_edges(&v).len(),
        0,
        "a bare filename must not claim the repository file: {v}"
    );
}

#[test]
fn e8_missing_header_yields_no_candidate() {
    let h = home("e8missing");
    repo_with(
        &h,
        "sem",
        &[
            ("src/proj/thing.h", E8_HEADER),
            (
                "src/proj/main.cc",
                "#include \"proj/absent.h\"\n\nint go() { return Thing(1); }\n",
            ),
        ],
    );
    do_index(&h);
    let v = e8_callers(&h, "src/proj/thing.h");
    assert_eq!(
        e8_edges(&v).len(),
        0,
        "an include that names no indexed file links nothing: {v}"
    );
}

#[test]
fn e8_differing_include_roots_are_not_merged() {
    let h = home("e8roots");
    repo_with(
        &h,
        "sem",
        &[
            ("one/proj/thing.h", E8_HEADER),
            ("two/proj/thing.h", E8_HEADER),
            (
                "src/proj/main.cc",
                "#include \"two/proj/thing.h\"\n\nint go() { return Thing(1); }\n",
            ),
        ],
    );
    do_index(&h);
    // The literal names one file, so that file's scope gains the caller.
    let named = e8_callers(&h, "two/proj/thing.h");
    assert_eq!(e8_edges(&named).len(), 1, "{named}");
    assert_eq!(e8_edges(&named)[0]["caller_path"], "src/proj/main.cc");
    // The other file under the same suffix is a different header.
    let other = e8_callers(&h, "one/proj/thing.h");
    assert_eq!(
        e8_edges(&other).len(),
        0,
        "two roots must not merge into one identity: {other}"
    );
}

#[test]
fn e8_windows_and_unix_include_paths_resolve_alike() {
    let h = home("e8win");
    repo_with(
        &h,
        "sem",
        &[
            ("src/proj/thing.h", E8_HEADER),
            (
                "src/proj/main.cc",
                "#include \"proj\\thing.h\"\n\nint go() { return Thing(1); }\n",
            ),
        ],
    );
    do_index(&h);
    let v = e8_callers(&h, "src/proj/thing.h");
    assert_eq!(
        e8_edges(&v).len(),
        1,
        "a Windows separator must resolve like the Unix form: {v}"
    );
    assert_eq!(e8_edges(&v)[0]["caller_path"], "src/proj/main.cc");
}

#[test]
fn e8_cross_repository_candidate_scopes_stay_isolated() {
    let h = home("e8xrepo");
    repo_with(
        &h,
        "sem",
        &[
            ("src/proj/thing.h", E8_HEADER),
            ("src/proj/main.cc", E8_CALLER),
        ],
    );
    repo_with(
        &h,
        "other",
        &[
            ("other/proj/thing.h", E8_HEADER),
            (
                "other/proj/main.cc",
                "#include \"proj/thing.h\"\n\nint go() { return Thing(1); }\n",
            ),
        ],
    );
    do_index(&h);
    let (code, _, err) = run(&h, &["index", "--repo", "other"]);
    assert_eq!(code, 0, "{err}");
    let mine = e8_callers(&h, "src/proj/thing.h");
    assert_eq!(e8_edges(&mine).len(), 1, "{mine}");
    assert_eq!(e8_edges(&mine)[0]["caller_path"], "src/proj/main.cc");
    let (code, out, err) = run(
        &h,
        &[
            "callers",
            "--repo",
            "other",
            "--path",
            "other/proj/thing.h",
            "--line",
            "1",
            "--col",
            "5",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let theirs: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(e8_edges(&theirs).len(), 1, "{theirs}");
    assert_eq!(
        e8_edges(&theirs)[0]["caller_path"],
        "other/proj/main.cc",
        "no cross-repository candidate: {theirs}"
    );
    assert!(
        !out.contains("src/proj/main.cc"),
        "no cross-repository leakage: {out}"
    );
}

#[test]
fn semantic_empty_display_keeps_module_scope_call_candidate() {
    let h = home("empty-display-owner");
    let source = "function target() {}\ntarget();\n";
    let dir = repo_with(&h, "sem", &[("a.ts", source)]);
    do_index(&h);
    let uri = "scip-test test pkg 1 a.ts/target().";
    let doc = test_document(
        "a.ts",
        &[occurrence(uri, 1, 0, 9, 15), occurrence(uri, 0, 1, 0, 6)],
        &[(uri, "")],
    );
    import_documents(&h, &dir, "empty-display", &[doc]);
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "a.ts", "--line", "1", "--col", "10",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["anchor"]["kind"], "semantic");
    assert!(
        v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "POTENTIAL_CALL" && e["unresolved"] == "owner-unproven"),
        "{v}"
    );
    assert!(
        v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "CALLS"),
        "{v}"
    );
    std::fs::write(
        dir.join("a.ts"),
        "// edited\nfunction target() {}\ntarget();\n",
    )
    .unwrap();
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "a.ts", "--line", "1", "--col", "10",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let stale: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(stale["freshness"]["status"], "stale");
    assert!(
        stale["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "CALLS"),
        "{stale}"
    );
}

#[test]
fn semantic_empty_display_does_not_link_same_name_in_unrelated_file() {
    let h = home("empty-display-collision");
    let source = "function target() {}\ntarget();\n";
    let dir = repo_with(&h, "sem", &[("a.ts", source), ("b.ts", source)]);
    do_index(&h);
    let a = "scip-test test pkg 1 a.ts/target().";
    let b = "scip-test test pkg 1 b.ts/target().";
    let doc = |path, uri| {
        test_document(
            path,
            &[occurrence(uri, 1, 0, 9, 15), occurrence(uri, 0, 1, 0, 6)],
            &[(uri, "")],
        )
    };
    import_documents(&h, &dir, "collision", &[doc("a.ts", a), doc("b.ts", b)]);
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "a.ts", "--line", "1", "--col", "10",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let edges = v["edges"].as_array().unwrap();
    assert!(edges.iter().any(|e| e["kind"] == "POTENTIAL_CALL"), "{v}");
    assert!(
        edges
            .iter()
            .all(|e| e["caller_path"] == "a.ts" && e["kind"] != "CALLS"),
        "{v}"
    );
}

/// The raw text of the first `CALLS` edge object in a callers document.
fn first_calls_edge_text(out: &str) -> &str {
    let start = out.find("{\"kind\":\"CALLS\"").expect("a CALLS edge");
    let mut stream =
        serde_json::Deserializer::from_str(&out[start..]).into_iter::<serde_json::Value>();
    stream.next().unwrap().unwrap();
    &out[start..start + stream.byte_offset()]
}

/// Replaces every `artifact_id` value with `<artifact>`, as `normalized_edge_json` does.
fn mask_artifact(text: &str) -> String {
    const KEY: &str = "\"artifact_id\":\"";
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        let (head, tail) = rest.split_at(at + KEY.len());
        out.push_str(head);
        out.push_str("<artifact>");
        rest = &tail[tail.find('"').expect("closing quote of artifact_id")..];
    }
    out.push_str(rest);
    out
}

/// The CALLS edge of the SCIP fixture as the a54fd1e tree (before the candidate
/// keys existed) printed it, with the run nonce masked as `normalized_edge_json` masks it.
const CALLS_EDGE_AT_A54FD1E: &str = r##"{"kind":"CALLS","precision":"Exact","caller":{"type":"scip","import_id":1,"symbol_uri":"scip-test test test-pkg 1.0.0 main.py/caller().","doc":null,"path":null,"byte_start":null,"byte_end":null},"callee":{"type":"scip","import_id":1,"symbol_uri":"scip-test test test-pkg 1.0.0 lib.py/target().","doc":null,"path":null,"byte_start":null,"byte_end":null},"caller_path":"main.py","caller_span":[49,57],"callee_name":"target","provider":"scip-import","import_id":1,"tool":"scip-test","verification":"Unverified","unresolved":null,"evidence":{"path":"main.py","span":[49,57],"callee_span":[49,55],"terminal_span":[49,55],"reference_span":[49,55],"digest":"efdc560edb1437c7","provider":"scip-import","imports":[{"id":1,"provider":"scip-test","version":"1.0","artifact_id":"<artifact>","source_revision":"head=-|branch=-|dirty=false|untracked=0|cfg=9150916a063f224e|nonce=<nonce>","content_identity":"9a80fe22347c0ef3"}]}}"##;

#[test]
fn candidate_keys_are_never_added_to_scip_edges() {
    let h = home("candidate-scip");
    scip_imported(&h);
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "sem", "--path", "lib.py", "--line", "1", "--col", "6",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let doc: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
    let edges = doc["edges"].as_array().unwrap();
    assert!(edges.iter().any(|e| e["kind"] == "CALLS"), "{out}");
    let nonce = doc["snapshot"]["nonce"].as_str().unwrap();
    let calls = mask_artifact(&first_calls_edge_text(&out).replace(nonce, "<nonce>"));
    assert_eq!(calls, CALLS_EDGE_AT_A54FD1E, "the CALLS edge bytes changed");
    for edge in edges.iter().filter(|e| e["kind"] != "POTENTIAL_CALL") {
        assert!(edge.get("candidates").is_none(), "{edge}");
        assert!(edge.get("candidate_scope").is_none(), "{edge}");
    }
}
