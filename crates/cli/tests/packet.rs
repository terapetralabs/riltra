// SPDX-License-Identifier: Apache-2.0
//! Acceptance tests for the selected-anchor packet CLI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut path = std::env::temp_dir();
    path.push(format!("riltra-packet-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn run(home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_riltra"))
        .args(args)
        .env("RILTRA_HOME", home)
        .output()
        .expect("spawn riltra");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn fixture(home: &Path) {
    registered_fixture(home);
    let (code, _, err) = run(home, &["index", "--repo", "packet"]);
    assert_eq!(code, 0, "index: {err}");
}

fn registered_fixture(home: &Path) -> PathBuf {
    let root = home.join("packet-src");
    std::fs::create_dir_all(&root).unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/packet");
    for name in ["a.py", "b.py"] {
        std::fs::copy(dir.join(name), root.join(name)).unwrap();
    }
    let (code, _, err) = run(
        home,
        &["init", "--repo", root.to_str().unwrap(), "--name", "packet"],
    );
    assert_eq!(code, 0, "init: {err}");
    root
}

fn packet(home: &Path, anchors: &Value) -> (i32, String, String) {
    let anchors = serde_json::to_string(anchors).unwrap();
    run(home, &["packet", "--repo", "packet", "--anchors", &anchors])
}

fn packet_args(home: &Path, args: &[&str]) -> (i32, String, String) {
    let mut full = vec!["packet", "--repo", "packet"];
    full.extend_from_slice(args);
    run(home, &full)
}

fn error_head(err: &str) -> &str {
    err.lines().next().unwrap_or("")
}

#[test]
fn packet_rejects_invalid_selectors() {
    let h = home("invalid");
    fixture(&h);
    let cases = [
        json!([]),
        json!([
            {"target":"alpha"}, {"target":"alpha"}, {"target":"alpha"},
            {"target":"alpha"}, {"target":"alpha"}, {"target":"alpha"}
        ]),
        json!([{"target":"alpha", "path":"a.py", "line":4, "col":1}]),
        json!([{"path":"a.py", "line":4, "col":0}]),
    ];
    for anchors in cases {
        let (code, out, err) = packet(&h, &anchors);
        assert_eq!(
            code,
            2,
            "invalid selectors must exit 2: {}\n{out}",
            error_head(&err)
        );
        assert!(
            out.is_empty(),
            "invalid selectors must not emit partial output"
        );
        assert!(
            !err.contains("unknown arguments"),
            "packet command missing: {}",
            error_head(&err)
        );
        assert!(
            err.contains("anchor") || err.contains("selector"),
            "{}",
            error_head(&err)
        );
    }
}

#[test]
fn packet_keeps_mixed_anchor_outcomes() {
    let h = home("mixed");
    fixture(&h);
    let (code, out, err) = packet(
        &h,
        &json!([
            {"target":"alpha", "in":"a.py", "kind":"function"},
            {"target":"shared", "kind":"function"},
            {"target":"missing_name"}
        ]),
    );
    assert_eq!(
        code,
        0,
        "mixed resolution is a successful document: {}",
        error_head(&err)
    );
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["operation"], "packet");
    assert_eq!(doc["anchors"][0]["status"], "resolved");
    assert_eq!(doc["anchors"][0]["resolved"]["path"], "a.py");
    assert_eq!(doc["anchors"][1]["status"], "ambiguous");
    assert_eq!(doc["anchors"][2]["status"], "not-found");
    assert!(doc["anchors"][1]["total"].as_u64().unwrap() >= 2);
    assert_eq!(
        doc["anchors"][1]["context"],
        json!({
            "status": "not-applicable",
            "reason": "anchor-unresolved",
            "omitted_by_budget": 0,
            "data": null
        })
    );
}

#[test]
fn packet_duplicate_selectors_keep_outcomes_and_share_source() {
    let h = home("duplicate");
    fixture(&h);
    let selector = json!({"target":"alpha", "in":"a.py", "kind":"function"});
    let (code, out, err) = packet(&h, &json!([selector.clone(), selector]));
    assert_eq!(code, 0, "duplicate selectors are valid: {err}");
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    let anchors = doc["anchors"].as_array().expect("anchor outcomes");
    assert_eq!(anchors.len(), 2);
    assert_eq!(anchors[0]["status"], "resolved");
    assert_eq!(anchors[1]["status"], "resolved");
    assert_eq!(anchors[0]["snippet_id"], anchors[1]["snippet_id"]);
    assert_eq!(doc["snippets"].as_array().unwrap().len(), 1);
    assert_eq!(doc["snippets"][0]["anchor_indices"], json!([0, 1]));
}

#[test]
fn packet_cli_reads_multiple_selected_anchors() {
    let h = home("multiple");
    fixture(&h);
    let selectors = json!([
        {"target":"alpha", "in":"a.py", "kind":"function"},
        {"path":"b.py", "line":1, "col":1},
        {"target":"shared", "kind":"function"},
        {"target":"missing_name"}
    ]);
    let (code, out, err) = packet(&h, &selectors);
    assert_eq!(
        code,
        0,
        "packet document should include mixed outcomes: {}",
        error_head(&err)
    );
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    let anchors = doc["anchors"].as_array().expect("anchors array");
    assert_eq!(anchors.len(), 4);
    assert_eq!(anchors[0]["status"], "resolved");
    assert_eq!(anchors[1]["status"], "resolved");
    assert_eq!(anchors[2]["status"], "ambiguous");
    assert_eq!(anchors[3]["status"], "not-found");
    assert!(doc["snippets"].as_array().unwrap().len() >= 2);
}

#[test]
fn packet_cli_help_lists_command_and_all_unresolved_are_documents() {
    let h = home("help-unresolved");
    let (code, help, _) = run(&h, &["--help"]);
    assert_eq!(code, 0);
    assert!(help.contains("packet --repo <name> --anchors <json>"));
    fixture(&h);
    let (code, out, err) = packet(&h, &json!([{"target":"shared"}, {"target":"missing_name"}]));
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["anchors"][0]["status"], "ambiguous");
    assert_eq!(doc["anchors"][1]["status"], "not-found");
}

#[test]
fn packet_query_to_selected_packet_does_not_need_a_guessed_column() {
    let h = home("query-packet");
    fixture(&h);
    let (code, out, err) = run(&h, &["query", "--repo", "packet", "--text", "alpha"]);
    assert_eq!(code, 0, "query: {err}");
    let query: Value = serde_json::from_str(&out).expect("query JSON");
    let row = &query["results"][0];
    let name = row["name"].as_str().expect("query result name");
    let path = row["path"].as_str().expect("query result path");
    assert!(
        row.get("col").is_none(),
        "query must not fabricate a column"
    );

    let (code, out, err) = packet(&h, &json!([{"target":name, "in":path, "kind":"function"}]));
    assert_eq!(
        code,
        0,
        "selected query result should form a packet: {}",
        error_head(&err)
    );
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    assert_eq!(doc["anchors"][0]["status"], "resolved");
    assert_eq!(doc["anchors"][0]["resolved"]["path"], path);
}

#[test]
fn packet_cli_does_not_create_missing_index() {
    let h = home("no-index");
    registered_fixture(&h);
    assert!(!h.join("index").exists(), "fixture must be unindexed");
    let (code, out, err) = packet(&h, &json!([{"target":"alpha"}]));
    assert_eq!(
        code,
        0,
        "no-index is a packet document: {}",
        error_head(&err)
    );
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    assert_eq!(doc["anchors"][0]["status"], "no-index");
    assert!(!h.join("index").exists(), "packet must not create an index");
    let (code, out, _) = packet(&h, &json!([{"path":"../outside.py", "line":1, "col":1}]));
    assert_eq!(
        code, 3,
        "unsafe path must be refused before no-index response"
    );
    assert!(out.is_empty());
    assert!(
        !h.join("index").exists(),
        "refusal must not create an index"
    );
}

#[test]
fn packet_cli_maps_usage_and_data_errors() {
    let h = home("errors");
    fixture(&h);
    let mut cases: Vec<(&str, i32, i32, String, String)> = Vec::new();
    let (code, out, err) = run(&h, &["packet", "--anchors", "[]"]);
    cases.push(("missing-repo", 2, code, out, error_head(&err).to_string()));
    for (label, args) in [
        ("invalid-json", vec!["--anchors", "not-json"]),
        (
            "depth",
            vec!["--anchors", r#"[{"target":"alpha"}]"#, "--depth", "0"],
        ),
        (
            "limit",
            vec!["--anchors", r#"[{"target":"alpha"}]"#, "--limit", "101"],
        ),
        (
            "max-bytes",
            vec![
                "--anchors",
                r#"[{"target":"alpha"}]"#,
                "--max-bytes",
                "8191",
            ],
        ),
    ] {
        let (code, out, err) = packet_args(&h, &args);
        cases.push((label, 2, code, out, error_head(&err).to_string()));
    }

    let mut raw_over = String::from("[");
    raw_over.push_str(&" ".repeat(16_383));
    raw_over.push(']');
    let (code, out, err) = packet_args(&h, &["--anchors", &raw_over]);
    cases.push((
        "raw-anchor-limit",
        2,
        code,
        out,
        error_head(&err).to_string(),
    ));

    let large = "x".repeat(12_000);
    let anchors = serde_json::to_string(&json!([{"target":large}])).unwrap();
    let (code, out, err) = packet_args(&h, &["--anchors", &anchors, "--max-bytes", "8192"]);
    cases.push((
        "metadata-overflow",
        2,
        code,
        out,
        error_head(&err).to_string(),
    ));

    let root = h.join("packet-src");
    std::fs::remove_dir_all(&root).unwrap();
    let (code, out, err) = packet(&h, &json!([{"target":"alpha"}]));
    cases.push((
        "root-unavailable",
        3,
        code,
        out,
        error_head(&err).to_string(),
    ));

    let schema_home = home("schema-error");
    fixture(&schema_home);
    let db = schema_home.join("index/packet.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("PRAGMA user_version = 5")
        .unwrap();
    let (code, out, err) = packet(&schema_home, &json!([{"target":"alpha"}]));
    cases.push(("schema-error", 3, code, out, error_head(&err).to_string()));

    let corrupt_home = home("database-error");
    fixture(&corrupt_home);
    std::fs::write(
        corrupt_home.join("index/packet.db"),
        b"not a sqlite database",
    )
    .unwrap();
    let (code, out, err) = packet(&corrupt_home, &json!([{"target":"alpha"}]));
    cases.push(("database-error", 3, code, out, error_head(&err).to_string()));

    let wrong = cases
        .iter()
        .filter(|(_, expected, actual, _, _)| expected != actual)
        .collect::<Vec<_>>();
    assert!(
        wrong.is_empty(),
        "wrong packet error mapping: {wrong:#?}; all: {cases:#?}"
    );
    for (label, _expected, _, stdout, stderr) in cases {
        assert!(
            stdout.is_empty(),
            "{label} emitted partial output: {stdout}"
        );
        if label != "missing-repo" {
            assert!(
                !stderr.contains("unknown arguments"),
                "packet command missing for {label}: {stderr}"
            );
        }
        assert!(!stderr.is_empty(), "{label} should diagnose on stderr");
        if ["depth", "limit", "max-bytes"].contains(&label) {
            assert!(
                stderr.contains("invalid context bounds"),
                "{label}: {stderr}"
            );
        }
    }
}

#[test]
fn packet_anchor_bytes_use_shared_normalization() {
    let h = home("anchor-bytes");
    registered_fixture(&h);

    let exact_target = "a".repeat(16_369);
    let exact = json!([{"target":exact_target}]);
    assert_eq!(serde_json::to_string(&exact).unwrap().len(), 16_384);
    let (code, out, err) = packet(&h, &exact);
    assert_eq!(code, 0, "16384-byte selector is permitted: {err}");
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    assert_eq!(doc["anchors"][0]["status"], "no-index");

    let over_target = "a".repeat(16_370);
    let over = json!([{"target":over_target}]);
    assert_eq!(serde_json::to_string(&over).unwrap().len(), 16_385);
    let (code, out, err) = packet(&h, &over);
    assert_eq!(code, 2, "16385-byte selector is rejected: {err}\n{out}");
    assert!(out.is_empty());

    let selector = json!([{"target":"café \"quoted\"", "in":null, "kind":null}]);
    let (code, out, err) = packet(&h, &selector);
    assert_eq!(
        code, 0,
        "Unicode and escaped selector should round-trip: {err}"
    );
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    assert_eq!(doc["anchors"][0]["selector"]["target"], "café \"quoted\"");
    assert!(doc["anchors"][0]["selector"].get("in").is_none());
    assert!(doc["anchors"][0]["selector"].get("kind").is_none());

    let five = json!([
        {"target":"same"}, {"target":"same"}, {"target":"same"},
        {"target":"same"}, {"target":"same"}
    ]);
    let (code, out, err) = packet(&h, &five);
    assert_eq!(
        code, 0,
        "five duplicate selectors count as five inputs: {err}"
    );
    let doc: Value = serde_json::from_str(&out).expect("packet JSON");
    assert_eq!(doc["anchors"].as_array().unwrap().len(), 5);
}
