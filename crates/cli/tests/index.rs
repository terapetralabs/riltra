// SPDX-License-Identifier: Apache-2.0
//! Tier A index tests: atomic rebuilds, freshness verdicts, corruption,
//! concurrency, and isolation — all through the real binary with isolated
//! `RILTRA_HOME` directories, so every spawn is a true reopen.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-idx-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
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

fn mkrepo(home: &Path, name: &str) -> PathBuf {
    let d = home.join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("run git")
            .success(),
        "git {args:?} failed"
    );
}

fn git_repo(home: &Path, name: &str, branch: &str) -> Option<PathBuf> {
    if !git_available() {
        return None;
    }
    let dir = mkrepo(home, name);
    git(&dir, &["init", "-b", branch, "."]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    git(&dir, &["add", "a.txt"]);
    git(&dir, &["commit", "-m", "init"]);
    Some(dir)
}

fn fixture_source(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("m2")
        .join(rel)
}

fn copy_fixture(dir: &Path, rel: &str) {
    let dest = dir.join(rel);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::copy(fixture_source(rel), &dest).unwrap();
}

fn init_and_index(home: &Path, dir: &Path, name: &str) -> String {
    let (code, _, err) = run(
        home,
        &["init", "--repo", dir.to_str().unwrap(), "--name", name],
    );
    assert_eq!(code, 0, "init failed: {err}");
    let (code, out, err) = run(home, &["index", "--repo", name]);
    assert_eq!(code, 0, "index failed: {err}");
    out
}

/// Envelope `"snapshot":{"id":N,...}` (first occurrence is the envelope-level
/// one); per-hit entries stay `"snapshot":N`.
fn envelope_snapshot(out: &str) -> String {
    let key = "\"snapshot\":{\"id\":";
    let pos = out.find(key).expect("envelope must carry snapshot");
    let rest = &out[pos + key.len()..];
    rest.chars().take_while(|c| c.is_ascii_digit()).collect()
}

/// Every per-hit `"snapshot":N` in the output must equal the envelope id.
/// Requires at least one hit: vacuous consistency proves nothing.
fn snapshot_ids_consistent(out: &str) -> bool {
    let id = envelope_snapshot(out);
    let mut rest = out;
    let mut found_hit = false;
    while let Some(pos) = rest.find("\"snapshot\":") {
        rest = &rest[pos + "\"snapshot\":".len()..];
        // Skip the envelope object form `"snapshot":{...}`.
        if rest.starts_with('{') {
            rest = &rest[1..];
            continue;
        }
        let value: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if value.is_empty() || value != id {
            return false;
        }
        found_hit = true;
    }
    found_hit
}

#[test]
fn search_response_carries_one_revision_identity() {
    let h = home("nonce");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    let (_, first, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    assert!(first.contains("\"nonce\":\""), "got: {first}");
    assert!(
        !first.contains("\"nonce\":\"\""),
        "nonce must not be empty: {first}"
    );
    assert!(snapshot_ids_consistent(&first));
    std::fs::write(repo.join("b.py"), b"def g():\n    pass\n").unwrap();
    init_and_index(&h, &repo, "r");
    let (_, second, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    let nonce_of = |out: &str| -> String {
        let key = "\"nonce\":\"";
        let pos = out.find(key).expect("nonce in envelope");
        let rest = &out[pos + key.len()..];
        rest[..rest.find('"').unwrap()].to_string()
    };
    assert_ne!(
        nonce_of(&first),
        nonce_of(&second),
        "revisions must be distinguishable"
    );
    assert!(snapshot_ids_consistent(&second));
}

#[test]
fn build_reopen_search_is_consistent() {
    let h = home("reopen");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    let (_, first, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    let (_, second, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    assert_eq!(first, second, "reopen must serve identical results");
    assert!(first.contains("\"freshness\":\"fresh\""));
    assert!(first.contains("\"name\":\"Worker\""));
    assert!(snapshot_ids_consistent(&first));
}

/// A C++ header with a data member, a method declaration and a
/// function-pointer member. The data member is on line 3, its identifier
/// starts at column 10; the class spans the bytes 0..68.
const CPP_FIELDS: &str =
    "class Options {\n public:\n  double max_triangulation_angle = -1.0f;\n};\n";

#[test]
fn a_cpp_data_member_is_searchable_as_a_field() {
    let h = home("field-search");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    init_and_index(&h, &repo, "r");
    let (code, out, err) = run(
        &h,
        &[
            "search",
            "--repo",
            "r",
            "--symbol",
            "max_triangulation_angle",
        ],
    );
    assert_eq!(code, 0, "search failed: {err}");
    assert!(out.contains("\"kind\":\"field\""), "got: {out}");
    assert!(
        out.contains("\"name\":\"max_triangulation_angle\""),
        "got: {out}"
    );
    assert!(out.contains("options.h"), "got: {out}");
    // The symbol points at its NAME: line 3, column 10.
    assert!(out.contains("\"start_line\":3"), "got: {out}");
    assert!(out.contains("\"start_col\":10"), "got: {out}");
}

#[test]
fn a_cpp_method_or_function_pointer_member_is_not_a_field() {
    let h = home("field-exclude");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("options.h"),
        "class Options {\n public:\n  int process(int x);\n\n  int (*callback)();\n};\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    for name in ["process", "callback"] {
        let (code, out, err) = run(&h, &["search", "--repo", "r", "--symbol", name]);
        assert_eq!(code, 0, "search failed: {err}");
        assert!(
            !out.contains("\"kind\":\"field\""),
            "{name} must not be indexed as a data member: {out}"
        );
    }
}

fn edge_doc(h: &Path, command: &str, path: &str, line: &str, col: &str) -> serde_json::Value {
    let (code, out, err) = run(
        h,
        &[
            command, "--repo", "r", "--path", path, "--line", line, "--col", col,
        ],
    );
    assert_eq!(code, 0, "{command} failed: {err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
}

#[test]
fn a_position_on_a_data_member_name_anchors_the_member_not_its_class() {
    // The anchor of a position on a data member's NAME is the member itself,
    // so callers and impact answer about the member and never about the
    // enclosing class (the class spans 0..68, the member name 34..57).
    let h = home("field-anchor");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    init_and_index(&h, &repo, "r");
    for command in ["callers", "callees"] {
        let doc = edge_doc(&h, command, "options.h", "3", "10");
        assert_eq!(
            doc["anchor"]["span"],
            serde_json::json!([34, 57]),
            "{command}: {doc}"
        );
        assert_eq!(doc["anchor"]["endpoint"]["byte_start"], 34, "{command}");
    }
    // The anchor says what it is and where the references are listed.
    let doc = edge_doc(&h, "callers", "options.h", "3", "10");
    let note = doc["anchor"]["anchor_uncertainty"].as_str().unwrap();
    assert!(
        note.contains("data member 'max_triangulation_angle'"),
        "{note}"
    );
    assert!(note.contains("search --occurrences"), "{note}");
}

#[test]
fn a_position_in_a_class_body_outside_any_member_name_still_anchors_the_class() {
    let h = home("field-anchor-class");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    init_and_index(&h, &repo, "r");
    // ` public:` (line 2) and the type of the member (line 3, column 3) are
    // inside the class but on no member name.
    for (line, col) in [("2", "2"), ("3", "4")] {
        let doc = edge_doc(&h, "callers", "options.h", line, col);
        assert_eq!(
            doc["anchor"]["span"],
            serde_json::json!([0, 68]),
            "{line}:{col}"
        );
        assert_eq!(
            doc["anchor"]["anchor_uncertainty"],
            "syntax-only anchor: no semantic identity at this position"
        );
    }
}

#[test]
fn impact_at_a_data_member_is_unknown_and_anchored_at_the_member() {
    let h = home("field-anchor-impact");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    std::fs::write(repo.join("use.cc"), OCC_USE_CC).unwrap();
    init_and_index(&h, &repo, "r");
    let doc = edge_doc(&h, "impact", "options.h", "3", "10");
    assert_eq!(doc["verdict"], "UNKNOWN");
    assert_eq!(doc["anchor"]["span"], serde_json::json!([34, 57]));
    assert_eq!(doc["completeness"]["status"], "partial");
    assert_eq!(doc["paths"], serde_json::json!([]));
    assert!(
        !doc.to_string().contains("\"occurrences\""),
        "the graph carries no occurrence data, only the pointer in the note: {doc}"
    );
    assert!(doc["anchor"]["anchor_uncertainty"]
        .as_str()
        .unwrap()
        .contains("search --occurrences"));
}

#[test]
fn callers_of_an_out_of_line_method_match_calls_by_the_method_name() {
    // `Widget::run` is defined out of line, so its symbol name is the
    // qualified text, while a call is recorded by its terminal name `run`.
    let h = home("qualified-callers");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("widget.cc"),
        "struct Widget {\n  void run();\n  void again();\n};\nvoid Widget::run() {}\nvoid Widget::again() {\n  run();\n}\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    let doc = edge_doc(&h, "callers", "widget.cc", "5", "8");
    let edges = doc["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{doc}");
    assert_eq!(edges[0]["kind"], "POTENTIAL_CALL");
    assert_eq!(edges[0]["precision"], "Heuristic");
    assert_eq!(edges[0]["callee_name"], "run");
    assert_eq!(
        edges[0]["callee"],
        serde_json::Value::Null,
        "never resolved"
    );
}

#[test]
fn callers_of_an_out_of_line_method_reach_the_files_that_include_its_declaring_header() {
    // `Widget::run` is defined in widget.cc but declared in widget.h, which
    // main.cc includes: the call in main.cc is a candidate. other.cc includes
    // nothing, so its `run` call is out of scope.
    let h = home("declaring-header");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("widget.h"),
        "struct Widget {\n  void run();\n};\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("widget.cc"),
        "#include \"widget.h\"\nvoid Widget::run() {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("main.cc"),
        "#include \"widget.h\"\nvoid go(Widget& w) {\n  w.run();\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("other.cc"),
        "void stray(Other& o) {\n  o.run();\n}\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    let doc = edge_doc(&h, "callers", "widget.cc", "2", "8");
    let callers: Vec<&str> = doc["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["caller_path"].as_str().unwrap())
        .collect();
    assert_eq!(callers, vec!["main.cc"], "{doc}");
    assert_eq!(doc["edges"][0]["kind"], "POTENTIAL_CALL");
    assert_eq!(doc["edges"][0]["callee"], serde_json::Value::Null);
}

#[test]
fn callers_of_a_data_member_never_list_a_same_named_call() {
    // A member is not callable: a call named like the member (here the method
    // `Box::value`) is no caller of the member `Holder::value`.
    let h = home("field-anchor-nocall");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("types.h"),
        "struct Holder {\n  int value;\n};\nstruct Box {\n  int value() const;\n};\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("use.cc"),
        "#include \"types.h\"\nint f(Box& b) {\n  return b.value();\n}\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    let doc = edge_doc(&h, "callers", "types.h", "2", "7");
    assert_eq!(doc["anchor"]["span"], serde_json::json!([22, 27]), "{doc}");
    assert_eq!(doc["edges"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["completeness"]["total"], 0);
}

#[test]
fn callers_in_module_scope_include_unresolved_python_alias_calls() {
    let h = home("python-alias-module-call");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("impl.py"), "def target():\n    pass\n").unwrap();
    std::fs::write(
        repo.join("use.py"),
        "from impl import target as renamed\nrenamed()\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");

    let doc = edge_doc(&h, "callers", "impl.py", "1", "5");
    let edges = doc["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{doc}");
    assert_eq!(edges[0]["kind"], "POTENTIAL_CALL", "{doc}");
    assert_eq!(edges[0]["precision"], "Heuristic", "{doc}");
    assert_eq!(edges[0]["verification"], "Unverified", "{doc}");
    assert_eq!(edges[0]["unresolved"], "owner-unproven", "{doc}");
    assert_eq!(edges[0]["caller"], serde_json::Value::Null, "{doc}");
    assert_eq!(edges[0]["caller_path"], "use.py", "{doc}");
    assert_eq!(doc["completeness"]["status"], "partial", "{doc}");
}

#[test]
fn rust_workspace_imports_link_from_the_crate_source_root() {
    // `use crate::helper` is relative to the importing crate, not the repo
    // root. A same-named file in another crate and an external `use b::helper`
    // must not become callers of this crate's definition.
    let h = home("rust-workspace-import");
    let repo = mkrepo(&h, "work");
    std::fs::create_dir_all(repo.join("crates/a/src")).unwrap();
    std::fs::create_dir_all(repo.join("crates/a/tests")).unwrap();
    std::fs::create_dir_all(repo.join("crates/b/src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, dir) in [("a", "crates/a"), ("b", "crates/b")] {
        std::fs::write(
            repo.join(dir).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
    }
    std::fs::write(repo.join("crates/a/src/helper.rs"), "pub fn target() {}\n").unwrap();
    std::fs::write(repo.join("crates/a/src/other.rs"), "pub fn target() {}\n").unwrap();
    std::fs::write(
        repo.join("crates/a/src/main.rs"),
        "mod helper;\nuse crate::helper;\nfn main() {\n    helper::target();\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("crates/a/src/grouped.rs"),
        "use crate::helper::{self};\n\npub fn run() {\n    helper::target();\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("crates/a/tests/helper.rs"),
        "pub fn target() {}\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("crates/a/tests/integration.rs"),
        "use b::helper;\n#[test]\nfn check() {\n    helper::target();\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("crates/b/src/helper.rs"), "pub fn target() {}\n").unwrap();
    init_and_index(&h, &repo, "r");

    let doc = edge_doc(&h, "callers", "crates/a/src/helper.rs", "1", "8");
    let edges = doc["edges"].as_array().unwrap();
    let mut callers: Vec<&str> = edges
        .iter()
        .map(|e| e["caller_path"].as_str().unwrap())
        .collect();
    callers.sort_unstable();
    assert_eq!(
        callers,
        vec!["crates/a/src/grouped.rs", "crates/a/src/main.rs"],
        "{doc}"
    );
    for edge in edges {
        assert_eq!(edge["kind"], "POTENTIAL_CALL", "{edge}");
        assert_eq!(edge["precision"], "Heuristic", "{edge}");
        assert_eq!(edge["verification"], "Unverified", "{edge}");
    }

    // The other crate's same-named definition has no proven caller here.
    let other = edge_doc(&h, "callers", "crates/b/src/helper.rs", "1", "8");
    assert_eq!(other["edges"], serde_json::json!([]), "{other}");

    // A same-named file that nothing imports stays unrelated.
    let unrelated = edge_doc(&h, "callers", "crates/a/tests/helper.rs", "1", "8");
    assert_eq!(unrelated["edges"], serde_json::json!([]), "{unrelated}");
}

#[test]
fn an_index_from_the_previous_parser_version_is_rebuilt() {
    let h = home("field-parser");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    init_and_index(&h, &repo, "r");
    // An index written by the previous parser version: the old parser
    // version, and no field facts in it.
    let db = h.join("index").join("r.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE snapshots SET parser_version = 1", [])
            .unwrap();
        conn.execute("DELETE FROM symbols WHERE kind = 'field'", [])
            .unwrap();
    }
    let (code, out, err) = run(
        &h,
        &[
            "search",
            "--repo",
            "r",
            "--symbol",
            "max_triangulation_angle",
        ],
    );
    assert_eq!(code, 0, "search failed: {err}");
    assert!(
        !out.contains("\"kind\":\"field\""),
        "the old index must not have field facts: {out}"
    );
    let (code, out, err) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0, "index status failed: {err}");
    assert!(out.contains("\"freshness\":\"stale\""), "got: {out}");
    assert!(out.contains("parser-changed"), "got: {out}");
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert!(out.contains("\"mode\":\"fallback-full\""), "got: {out}");
    assert!(out.contains("parser-changed"), "got: {out}");
    let (code, out, err) = run(
        &h,
        &[
            "search",
            "--repo",
            "r",
            "--symbol",
            "max_triangulation_angle",
        ],
    );
    assert_eq!(code, 0, "search failed: {err}");
    assert!(out.contains("\"kind\":\"field\""), "got: {out}");
}

/// `.inl` is C++ source (a template body that headers include): it is indexed
/// like a `.h`, and a fragment that is not a complete unit keeps its errors.
#[test]
fn inl_files_are_indexed_as_cpp_and_fragments_keep_their_errors() {
    let h = home("inl-index");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("widget.inl"),
        "inline int Widget::size() const { return count_; }\nvoid Widget::run() {\n  helper();\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("tail.inl"), "  count_ = 0;\n}\n").unwrap();
    init_and_index(&h, &repo, "r");
    let (code, out, err) = run(&h, &["scan", "--repo", "r"]);
    assert_eq!(code, 0, "scan failed: {err}");
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let entry = |path: &str| {
        doc["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == path)
            .unwrap_or_else(|| panic!("{path} missing from scan: {out}"))
            .clone()
    };
    let widget = entry("widget.inl");
    assert_eq!(widget["language"], "cpp", "got: {widget}");
    assert_eq!(widget["errors"], 0, "got: {widget}");
    let tail = entry("tail.inl");
    assert_eq!(tail["language"], "cpp", "got: {tail}");
    assert!(tail["errors"].as_u64().unwrap() > 0, "got: {tail}");
    let (code, out, err) = run(&h, &["search", "--repo", "r", "--symbol", "run"]);
    assert_eq!(code, 0, "search failed: {err}");
    assert!(out.contains("\"name\":\"Widget::run\""), "got: {out}");
    assert!(out.contains("\"path\":\"widget.inl\""), "got: {out}");
}

#[test]
fn inl_edits_make_the_index_stale_until_it_is_rebuilt() {
    let h = home("inl-stale");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("widget.inl"),
        "int Widget::size() { return 1; }\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    std::fs::write(
        repo.join("widget.inl"),
        "int Widget::count() { return 2; }\n",
    )
    .unwrap();
    let (code, out, err) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0, "index status failed: {err}");
    assert!(out.contains("\"freshness\":\"stale\""), "got: {out}");
    assert!(out.contains("content-changed"), "got: {out}");
    init_and_index(&h, &repo, "r");
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("\"freshness\":\"fresh\""), "got: {out}");
    let (_, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "count"]);
    assert!(out.contains("\"name\":\"Widget::count\""), "got: {out}");
    let (_, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "size"]);
    assert!(out.contains("\"total\":0"), "got: {out}");
}

#[test]
fn inl_support_rebuilds_an_index_from_parser_version_five() {
    let h = home("inl-parser");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("widget.inl"),
        "int Widget::size() { return 1; }\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    // Parser version 5 did not index `.inl` files, so its index has no facts
    // for them.
    let db = h.join("index").join("r.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE snapshots SET parser_version = 5", [])
            .unwrap();
        conn.execute("DELETE FROM symbols", []).unwrap();
    }
    let (code, out, err) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0, "index status failed: {err}");
    assert!(out.contains("\"freshness\":\"stale\""), "got: {out}");
    assert!(out.contains("parser-changed"), "got: {out}");
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert!(out.contains("\"mode\":\"fallback-full\""), "got: {out}");
    let (_, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "size"]);
    assert!(out.contains("\"name\":\"Widget::size\""), "got: {out}");
}

#[test]
fn an_index_from_the_previous_parser_version_has_no_occurrences_until_it_is_rebuilt() {
    let h = home("occ-parser");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    std::fs::write(
        repo.join("use.cc"),
        "#include \"options.h\"\ndouble read(Options& o) {\n  return o.max_triangulation_angle;\n}\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    let db = h.join("index").join("r.db");
    let occurrences = || -> i64 {
        rusqlite::Connection::open(&db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM reference_occurrences", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert_eq!(occurrences(), 1, "the member access is stored by the index");
    // An index written by the previous parser version has no occurrence rows.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE snapshots SET parser_version = parser_version - 1",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM reference_occurrences", [])
            .unwrap();
    }
    assert_eq!(occurrences(), 0);
    let (code, out, err) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0, "index status failed: {err}");
    assert!(out.contains("\"freshness\":\"stale\""), "got: {out}");
    assert!(out.contains("parser-changed"), "got: {out}");
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert!(out.contains("\"mode\":\"fallback-full\""), "got: {out}");
    assert!(out.contains("parser-changed"), "got: {out}");
    assert_eq!(occurrences(), 1, "the rebuild stores the occurrence again");
}

/// Uses of the `CPP_FIELDS` member: a member access in a same-file macro
/// argument (line 4), the address of the member (line 5), a plain member read
/// (line 6) and a parameter of the same name passed to the macro (line 9).
const OCC_USE_CC: &str = "#include \"options.h\"\n#define PRINT(x) (x)\ndouble read(Options& o) {\n  PRINT(o.max_triangulation_angle);\n  double* p = &o.max_triangulation_angle;\n  return o.max_triangulation_angle;\n}\nvoid show(double max_triangulation_angle) {\n  PRINT(max_triangulation_angle);\n}\n";

fn occurrence_repo(tag: &str) -> (PathBuf, PathBuf) {
    let h = home(tag);
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    std::fs::write(repo.join("use.cc"), OCC_USE_CC).unwrap();
    init_and_index(&h, &repo, "r");
    (h, repo)
}

fn occurrence_search(h: &Path, name: &str, extra: &[&str]) -> (String, serde_json::Value) {
    let mut args = vec!["search", "--repo", "r", "--symbol", name, "--occurrences"];
    args.extend_from_slice(extra);
    let (code, out, err) = run(h, &args);
    assert_eq!(code, 0, "search failed: {err}");
    let doc = serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"));
    (out, doc)
}

#[test]
fn search_occurrences_lists_each_use_with_its_role_and_the_ambiguity_counts() {
    let (h, repo) = occurrence_repo("occ-list");
    let (out, doc) = occurrence_search(&h, "max_triangulation_angle", &[]);
    // The section follows `truncated`, with a fixed key order.
    assert!(
        out.contains("\"truncated\":false,\"occurrences\":{\"scope\":\"repository-wide-exact-name\",\"binding\":\"unbound\",\"field_declarations\":1,\"symbol_declarations\":1,\"total\":4,\"truncated\":false,\"items\":[{\"path\":\"use.cc\",\"role\":\"member-access\",\"confidence\":\"observed\",\"span\":{\"start_line\":4,\"start_col\":11,"),
        "got: {out}"
    );
    let occ = &doc["occurrences"];
    let items: Vec<(String, String, String, u64, u64)> = occ["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["path"].as_str().unwrap().to_string(),
                i["role"].as_str().unwrap().to_string(),
                i["confidence"].as_str().unwrap().to_string(),
                i["span"]["start_line"].as_u64().unwrap(),
                i["span"]["start_col"].as_u64().unwrap(),
            )
        })
        .collect();
    let row = |role: &str, confidence: &str, line: u64, col: u64| {
        (
            "use.cc".to_string(),
            role.to_string(),
            confidence.to_string(),
            line,
            col,
        )
    };
    assert_eq!(
        items,
        vec![
            row("member-access", "observed", 4, 11),
            row("address-of-binding", "observed", 5, 18),
            row("member-access", "observed", 6, 12),
            row("macro-argument", "candidate", 9, 9),
        ]
    );
    // The recorded bytes address exactly the identifier.
    let source = std::fs::read(repo.join("use.cc")).unwrap();
    for item in occ["items"].as_array().unwrap() {
        let start = item["bytes"][0].as_u64().unwrap() as usize;
        let end = item["bytes"][1].as_u64().unwrap() as usize;
        assert_eq!(&source[start..end], b"max_triangulation_angle");
    }
    assert_eq!(
        occ["items"][0]["provenance"],
        serde_json::json!({"provider": "syntax", "grammar": "cpp:0.23.4", "language": "cpp"})
    );
    // The declaration is a search hit and never an occurrence.
    let results = doc["results"].as_array().unwrap();
    assert!(results
        .iter()
        .any(|r| r["kind"] == "field" && r["path"] == "options.h"));
    assert_eq!(doc["freshness"], "fresh");
}

#[test]
fn search_without_the_occurrences_flag_has_no_occurrences_member() {
    let (h, _repo) = occurrence_repo("occ-absent-flag");
    let (code, out, err) = run(
        &h,
        &[
            "search",
            "--repo",
            "r",
            "--symbol",
            "max_triangulation_angle",
        ],
    );
    assert_eq!(code, 0, "search failed: {err}");
    assert!(!out.contains("occurrences"), "got: {out}");
}

#[test]
fn an_absent_name_has_an_explicit_empty_occurrence_list() {
    let (h, _repo) = occurrence_repo("occ-empty");
    let (_, doc) = occurrence_search(&h, "no_such_member", &[]);
    let occ = &doc["occurrences"];
    assert_eq!(occ["total"], 0);
    assert_eq!(occ["truncated"], false);
    assert_eq!(occ["items"], serde_json::json!([]));
    assert_eq!(occ["field_declarations"], 0);
    assert_eq!(occ["scope"], "repository-wide-exact-name");
    assert_eq!(occ["binding"], "unbound");
    assert_eq!(
        doc["freshness"], "fresh",
        "the verdict travels with the list"
    );
}

#[test]
fn the_occurrence_lookup_is_exact_while_the_symbol_list_keeps_prefix_matching() {
    let h = home("occ-exact");
    let repo = mkrepo(&h, "work");
    std::fs::write(
        repo.join("options.h"),
        "class Options {\n public:\n  double angle = 1.0;\n  double angle_rad = 2.0;\n};\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("use.cc"),
        "#include \"options.h\"\ndouble read(Options& o) {\n  return o.angle + o.angle_rad;\n}\n",
    )
    .unwrap();
    init_and_index(&h, &repo, "r");
    let (_, doc) = occurrence_search(&h, "angle", &[]);
    assert_eq!(doc["total"], 2, "the symbol list matches both field names");
    let occ = &doc["occurrences"];
    assert_eq!(occ["symbol_declarations"], 1, "exact name only");
    assert_eq!(occ["total"], 1, "only the exact name's use: {occ}");
}

#[test]
fn occurrence_paging_applies_to_each_list_with_its_own_totals() {
    let (h, _repo) = occurrence_repo("occ-page");
    // Symbol list: one hit, so offset 1 is past it; occurrence list: 4 rows.
    let (_, doc) = occurrence_search(
        &h,
        "max_triangulation_angle",
        &["--limit", "1", "--offset", "1"],
    );
    assert_eq!(doc["total"], 1);
    assert_eq!(doc["results"], serde_json::json!([]));
    assert_eq!(doc["truncated"], false);
    let occ = &doc["occurrences"];
    assert_eq!(occ["total"], 4);
    assert_eq!(occ["items"].as_array().unwrap().len(), 1);
    assert_eq!(occ["items"][0]["span"]["start_line"], 5, "second row");
    assert_eq!(occ["truncated"], true);
    let (_, last) = occurrence_search(
        &h,
        "max_triangulation_angle",
        &["--limit", "2", "--offset", "3"],
    );
    assert_eq!(last["occurrences"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(last["occurrences"]["truncated"], false);
}

#[test]
fn search_occurrences_is_deterministic() {
    let (h, _repo) = occurrence_repo("occ-determinism");
    let (first, _) = occurrence_search(&h, "max_triangulation_angle", &[]);
    let (second, _) = occurrence_search(&h, "max_triangulation_angle", &[]);
    assert_eq!(first, second);
}

#[test]
fn search_occurrences_without_an_index_adds_no_member() {
    let h = home("occ-no-index");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("options.h"), CPP_FIELDS).unwrap();
    let (code, _, err) = run(
        &h,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    let (code, out, err) = run(
        &h,
        &[
            "search",
            "--repo",
            "r",
            "--symbol",
            "max_triangulation_angle",
            "--occurrences",
        ],
    );
    assert_eq!(code, 0, "search failed: {err}");
    assert!(out.contains("\"freshness\":\"unknown\""), "got: {out}");
    assert!(out.contains("no-index"), "got: {out}");
    assert!(
        !out.contains("occurrences"),
        "the flag cannot manufacture facts: {out}"
    );
}

#[test]
fn an_old_index_answers_stale_with_no_occurrences_until_it_is_rebuilt() {
    let (h, _repo) = occurrence_repo("occ-stale");
    let db = h.join("index").join("r.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE snapshots SET parser_version = parser_version - 1",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM reference_occurrences", [])
            .unwrap();
    }
    let (_, stale) = occurrence_search(&h, "max_triangulation_angle", &[]);
    assert_eq!(stale["freshness"], "stale");
    assert!(stale["reasons"].to_string().contains("parser-changed"));
    assert_eq!(
        stale["occurrences"]["total"], 0,
        "the stale reason travels with it"
    );
    let (code, _, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    let (_, fresh) = occurrence_search(&h, "max_triangulation_angle", &[]);
    assert_eq!(fresh["freshness"], "fresh");
    assert_eq!(fresh["occurrences"]["total"], 4);
}

#[test]
fn the_usage_line_documents_the_occurrences_flag() {
    let h = home("occ-usage");
    let (code, out, _) = run(&h, &["--help"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(
            "search --repo <name> --symbol <substr> [--limit <n>] [--offset <n>] [--occurrences]"
        ),
        "got: {out}"
    );
}

#[test]
fn dirty_without_head_change_is_stale_but_served() {
    let h = home("dirty");
    let Some(repo) = git_repo(&h, "code", "main") else {
        return;
    };
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
    let (code, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"freshness\":\"stale\""), "got: {out}");
    assert!(
        out.contains("dirty") || out.contains("content-changed"),
        "got: {out}"
    );
    let (code, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    assert_eq!(code, 0, "stale snapshots stay queryable with their flag");
    assert!(out.contains("\"freshness\":\"stale\""));
    assert!(out.contains("\"name\":\"Worker\""));
}

#[test]
fn rename_delete_and_new_files_are_stale_then_fresh() {
    let h = home("rename");
    let repo = mkrepo(&h, "work");
    std::fs::write(repo.join("a.py"), b"def alpha_fn():\n    pass\n").unwrap();
    init_and_index(&h, &repo, "r");
    std::fs::rename(repo.join("a.py"), repo.join("b.py")).unwrap();
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("\"freshness\":\"stale\""), "got: {out}");
    assert!(
        out.contains("file-added") || out.contains("file-removed"),
        "got: {out}"
    );
    std::fs::remove_file(repo.join("b.py")).unwrap();
    std::fs::write(repo.join("c.py"), b"def gamma_fn():\n    pass\n").unwrap();
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("\"freshness\":\"stale\""), "got: {out}");
    init_and_index(&h, &repo, "r");
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("\"freshness\":\"fresh\""), "got: {out}");
    let (_, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "alpha_fn"]);
    assert!(out.contains("\"total\":0"), "got: {out}");
    let (_, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "gamma_fn"]);
    assert!(out.contains("\"name\":\"gamma_fn\""), "got: {out}");
}

#[test]
fn branch_head_and_ignore_changes_are_stale() {
    let h = home("branch");
    let Some(repo) = git_repo(&h, "code", "main") else {
        return;
    };
    init_and_index(&h, &repo, "r");
    git(&repo, &["commit", "--allow-empty", "-m", "second"]);
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("head-changed"), "got: {out}");
    git(&repo, &["checkout", "-b", "feature"]);
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("branch-changed"), "got: {out}");
    std::fs::write(repo.join(".riltraignore"), "a.txt\n").unwrap();
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("config-changed"), "got: {out}");
}

#[test]
fn corrupt_staging_leftover_does_not_block_rebuild() {
    let h = home("staging");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    let (_, before, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    let staging = h.join("index").join("r.db.staging");
    std::fs::write(&staging, b"corrupt leftover").unwrap();
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "rebuild must overwrite corrupt staging: {err}");
    assert!(out.contains("\"status_at_write\":\"fresh\""), "got: {out}");
    assert!(!staging.exists(), "staging must be consumed by promote");
    let (_, after, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    assert!(
        after.contains("\"name\":\"Worker\""),
        "old data intact: {before} vs {after}"
    );
}

#[test]
fn corrupt_live_db_reports_unknown_and_recovers() {
    let h = home("corrupt");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    std::fs::write(h.join("index").join("r.db"), b"not a database").unwrap();
    let (code, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"freshness\":\"unknown\""), "got: {out}");
    assert!(out.contains("db-unreadable"), "got: {out}");
    let (code, _, _) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    assert_eq!(code, 3, "unreadable db must fail search loudly");
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "rebuild must recover from corrupt db: {err}");
    assert!(out.contains("\"status_at_write\":\"fresh\""), "got: {out}");
}

#[test]
fn concurrent_queries_during_rebuild_stay_consistent() {
    let h = home("concurrent");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    copy_fixture(&repo, "js/hello.js");
    init_and_index(&h, &repo, "r");
    let home_clone = h.clone();
    let builder = std::thread::spawn(move || {
        for _ in 0..2 {
            let (code, _, err) = run(&home_clone, &["index", "--repo", "r"]);
            assert_eq!(code, 0, "rebuild failed: {err}");
        }
    });
    let mut checked = 0;
    for _ in 0..30 {
        let (code, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "a"]);
        assert_eq!(code, 0);
        assert!(
            snapshot_ids_consistent(&out),
            "mixed snapshots in one response: {out}"
        );
        checked += 1;
    }
    builder.join().unwrap();
    assert!(checked > 0);
}

#[test]
fn two_repositories_stay_isolated() {
    let h = home("isolated");
    let a = mkrepo(&h, "a");
    let b = mkrepo(&h, "b");
    std::fs::write(a.join("x.py"), b"def only_in_a():\n    pass\n").unwrap();
    std::fs::write(b.join("x.py"), b"def only_in_b():\n    pass\n").unwrap();
    init_and_index(&h, &a, "ra");
    init_and_index(&h, &b, "rb");
    assert!(h.join("index").join("ra.db").exists());
    assert!(h.join("index").join("rb.db").exists());
    let (_, out_a, _) = run(&h, &["search", "--repo", "ra", "--symbol", "only_in_b"]);
    assert!(out_a.contains("\"total\":0"), "got: {out_a}");
    let (_, out_b, _) = run(&h, &["search", "--repo", "rb", "--symbol", "only_in_a"]);
    assert!(out_b.contains("\"total\":0"), "got: {out_b}");
    let (_, out_a, _) = run(&h, &["search", "--repo", "ra", "--symbol", "only_in_a"]);
    assert!(out_a.contains("\"name\":\"only_in_a\""), "got: {out_a}");
}

#[test]
fn search_spans_match_m2_goldens() {
    let h = home("golden");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "c/main.c");
    init_and_index(&h, &repo, "r");
    let (_, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "add"]);
    // Values copied from the verified tests/golden/m2-c-main.json contract.
    assert!(out.contains("\"name\":\"add\""), "got: {out}");
    assert!(
        out.contains("\"start_line\":6,\"start_col\":1,\"end_line\":8,\"end_col\":2"),
        "got: {out}"
    );
    assert!(out.contains("\"bytes\":[106,149]"), "got: {out}");
    assert!(out.contains("\"provider\":\"syntax\""), "got: {out}");
    assert!(out.contains("\"freshness\":\"fresh\""), "got: {out}");
}

#[test]
fn unknown_states_are_never_fresh() {
    let h = home("neverfresh");
    let repo = mkrepo(&h, "work");
    let (code, _, _) = run(
        &h,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0);
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("\"freshness\":\"unknown\""), "got: {out}");
    assert!(out.contains("no-index"), "got: {out}");
    let (_, out, _) = run(&h, &["index", "status", "--repo", "nope"]);
    assert!(out.is_empty(), "unknown repos fail loudly, no JSON");
    init_and_index(&h, &repo, "r");
    std::fs::rename(&repo, h.join("gone")).unwrap();
    let (_, out, _) = run(&h, &["index", "status", "--repo", "r"]);
    assert!(out.contains("\"freshness\":\"unknown\""), "got: {out}");
    assert!(out.contains("root-unreadable"), "got: {out}");
}

#[test]
fn pagination_contract_holds() {
    let h = home("paging");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    copy_fixture(&repo, "js/hello.js");
    init_and_index(&h, &repo, "r");
    let (code, out, _) = run(
        &h,
        &["search", "--repo", "r", "--symbol", "a", "--limit", "1"],
    );
    assert_eq!(code, 0);
    assert!(out.contains("\"truncated\":true"), "got: {out}");
    assert!(out.contains("\"limit\":1"), "got: {out}");
    let (_, page2, _) = run(
        &h,
        &[
            "search", "--repo", "r", "--symbol", "a", "--limit", "1", "--offset", "1",
        ],
    );
    assert_ne!(out, page2);
    let (code, _, err) = run(
        &h,
        &["search", "--repo", "r", "--symbol", "a", "--limit", "abc"],
    );
    assert_eq!(code, 2, "bad limit must be a usage error: {err}");
    let (code, _, _) = run(
        &h,
        &["search", "--repo", "r", "--symbol", "a", "--limit", "99999"],
    );
    assert_eq!(code, 2);
}

#[test]
fn index_output_contract_and_unknown_repo() {
    let h = home("contract");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    let (code, _, _) = run(
        &h,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0);
    let (code, out, _) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"status_at_write\":\"fresh\""), "got: {out}");
    assert!(out.contains("\"processed\":1"), "got: {out}");
    let (code, _, err) = run(&h, &["index", "--repo", "nope"]);
    assert_eq!(code, 3, "unknown repo must fail: {err}");
}

#[test]
fn cli_cold_start_reports_cold_start() {
    let h = home("coldstart");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    let (code, _, err) = run(
        &h,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert!(out.contains("\"mode\":\"full\""), "got: {out}");
    assert!(
        out.contains("\"fallback_reason\":\"cold-start\""),
        "got: {out}"
    );
}

#[test]
fn cli_noop_reports_zero_reparsed() {
    let h = home("noop");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert!(out.contains("\"mode\":\"incremental\""), "got: {out}");
    assert!(out.contains("\"reparsed\":0"), "got: {out}");
    assert!(out.contains("\"fallback_reason\":\"none\""), "got: {out}");
    assert!(out.contains("\"timings_ms\":{\"diff\":"), "got: {out}");
}

#[test]
fn cli_full_flag_forces_full() {
    let h = home("forcedfull");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    let (code, out, err) = run(&h, &["index", "--repo", "r", "--full"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert!(out.contains("\"mode\":\"full\""), "got: {out}");
    assert!(
        out.contains("\"fallback_reason\":\"forced-full\""),
        "got: {out}"
    );
}

/// Replace volatile envelope values (nonce, snapshot ids, timestamps) so
/// two independent builds can be compared string-for-string.
fn normalize_search(out: &str) -> String {
    let mut s = out.to_string();
    if let Some(pos) = s.find("\"nonce\":\"") {
        let start = pos + "\"nonce\":\"".len();
        if let Some(end) = s[start..].find('"') {
            let nonce = s[start..start + end].to_string();
            s = s.replace(&nonce, "NONCE");
        }
    }
    // Rewrite every `"key":<digits>` occurrence for volatile numeric keys.
    for key in ["\"id\":", "\"created_unix\":", "\"snapshot\":"] {
        let mut result = String::new();
        let mut rest = s.as_str();
        while let Some(pos) = rest.find(key) {
            result.push_str(&rest[..pos + key.len()]);
            rest = &rest[pos + key.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            rest = &rest[digits.len()..];
            result.push('0');
        }
        result.push_str(rest);
        s = result;
    }
    s
}

fn files_counters(out: &str) -> String {
    let pos = out.find("\"files\":").expect("index JSON must carry files");
    out[pos..].trim_end().to_string()
}

#[test]
fn cli_search_parity_full_vs_incremental() {
    // Guard test (Task 5): PASS-first-run expected.
    let h = home("paritycli");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    let (code, _, err) = run(
        &h,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    // Evolve through an incremental path in the default home.
    let (code, full_first, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    std::fs::write(repo.join("extra.py"), b"def helper():\n    pass\n").unwrap();
    let (code, inc_out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert!(
        inc_out.contains("\"mode\":\"incremental\""),
        "got: {inc_out}"
    );
    // Independent full rebuild of the same final tree in a second home.
    let h2 = home("paritycli-full");
    let (code, _, err) = run(
        &h2,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    let (code, full_out, err) = run(&h2, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "index failed: {err}");
    assert_eq!(
        files_counters(&inc_out),
        files_counters(&full_out),
        "fact-bearing counters must match"
    );
    let _ = full_first;
    for symbol in ["Worker", "helper", "missing-symbol-zzz"] {
        let (_, inc_search, _) = run(&h, &["search", "--repo", "r", "--symbol", symbol]);
        let (_, full_search, _) = run(&h2, &["search", "--repo", "r", "--symbol", symbol]);
        assert_eq!(
            normalize_search(&inc_search),
            normalize_search(&full_search),
            "search parity for {symbol}"
        );
    }
}

#[test]
fn concurrent_search_during_incremental_refresh_stays_consistent() {
    let h = home("concurrent-inc");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    copy_fixture(&repo, "js/hello.js");
    init_and_index(&h, &repo, "r");
    let home_clone = h.clone();
    let builder = std::thread::spawn(move || {
        for _ in 0..2 {
            let (code, out, err) = run(&home_clone, &["index", "--repo", "r"]);
            assert_eq!(code, 0, "rebuild failed: {err}");
            assert!(
                out.contains("\"mode\":\"incremental\""),
                "expected incremental refresh: {out}"
            );
        }
    });
    let mut checked = 0;
    for _ in 0..30 {
        let (code, out, _) = run(&h, &["search", "--repo", "r", "--symbol", "a"]);
        assert_eq!(code, 0);
        assert!(
            snapshot_ids_consistent(&out),
            "mixed snapshots in one response: {out}"
        );
        checked += 1;
    }
    builder.join().unwrap();
    assert!(checked > 0);
}

#[test]
fn incremental_two_repositories_stay_isolated() {
    let h = home("isolated-inc");
    let a = mkrepo(&h, "a");
    let b = mkrepo(&h, "b");
    std::fs::write(a.join("x.py"), b"def only_in_a():\n    pass\n").unwrap();
    std::fs::write(b.join("x.py"), b"def only_in_b():\n    pass\n").unwrap();
    init_and_index(&h, &a, "ra");
    init_and_index(&h, &b, "rb");
    let (code, out, err) = run(&h, &["index", "--repo", "ra"]);
    assert_eq!(code, 0, "incremental index failed: {err}");
    assert!(out.contains("\"mode\":\"incremental\""), "got: {out}");
    let (code, out, err) = run(&h, &["index", "--repo", "rb"]);
    assert_eq!(code, 0, "incremental index failed: {err}");
    assert!(out.contains("\"mode\":\"incremental\""), "got: {out}");
    let (_, out_a, _) = run(&h, &["search", "--repo", "ra", "--symbol", "only_in_b"]);
    assert!(out_a.contains("\"total\":0"), "got: {out_a}");
    let (_, out_b, _) = run(&h, &["search", "--repo", "rb", "--symbol", "only_in_a"]);
    assert!(out_b.contains("\"total\":0"), "got: {out_b}");
}

#[test]
fn incremental_recovers_partial_staging() {
    let h = home("staging-inc");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    let staging = h.join("index").join("r.db.staging");
    std::fs::write(&staging, b"corrupt leftover").unwrap();
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "rebuild must overwrite corrupt staging: {err}");
    assert!(!staging.exists(), "staging must be consumed by promote");
    let _ = out;
}

#[test]
fn incremental_recovers_corrupt_live_as_fallback() {
    let h = home("corruptlive-inc");
    let repo = mkrepo(&h, "work");
    copy_fixture(&repo, "py/hello.py");
    init_and_index(&h, &repo, "r");
    std::fs::write(h.join("index").join("r.db"), b"not a database").unwrap();
    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "recovery rebuild must succeed: {err}");
    assert!(out.contains("\"mode\":\"fallback-full\""), "got: {out}");
    assert!(
        out.contains("\"fallback_reason\":\"db-unreadable\""),
        "got: {out}"
    );
    let (code, out, err) = run(&h, &["search", "--repo", "r", "--symbol", "Worker"]);
    assert_eq!(code, 0, "search after recovery failed: {err}");
    assert!(out.contains("\"total\":1"), "got: {out}");
}

// ---- `riltra watch` ----

const WAIT: Duration = Duration::from_secs(60);

/// A running `riltra watch`. Stdout lines arrive over a channel from a reader
/// thread; stderr is drained on its own thread so a full pipe cannot block the
/// child. Dropping the handle kills and reaps the child.
struct WatchChild {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
    stderr: Option<JoinHandle<String>>,
}

impl WatchChild {
    fn spawn(home: &Path, args: &[&str], env: &[(&str, &str)]) -> WatchChild {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_riltra"));
        cmd.args(args)
            .env("RILTRA_HOME", home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("spawn riltra watch");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let stderr = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        WatchChild {
            child,
            lines,
            seen: Vec::new(),
            stderr: Some(stderr),
        }
    }

    /// The first stdout line that contains `needle`. On timeout the child is
    /// killed and the test panics with every line received so far.
    fn wait_line(&mut self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.kill();
                    panic!(
                        "no stdout line containing {needle:?} within {timeout:?}; lines so far:\n{}",
                        self.seen.join("\n")
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.kill();
                    let stderr = self
                        .stderr
                        .take()
                        .and_then(|h| h.join().ok())
                        .unwrap_or_default();
                    panic!(
                        "riltra watch exited before a line containing {needle:?}; lines so far:\n{}\nstderr:\n{stderr}",
                        self.seen.join("\n")
                    );
                }
            }
        }
    }

    /// Waits for the child to exit and returns its code and stderr. On timeout
    /// the child is killed and the test fails. Stdout lines still in the pipe
    /// are collected into `seen` for `count`.
    fn wait_exit(&mut self, timeout: Duration) -> (i32, String) {
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll riltra watch") {
                break status;
            }
            if Instant::now() >= deadline {
                self.kill();
                panic!(
                    "riltra watch did not exit within {timeout:?}; lines so far:\n{}",
                    self.seen.join("\n")
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            self.seen.push(line);
        }
        let stderr = self
            .stderr
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        (status.code().unwrap_or(-1), stderr)
    }

    /// Stdout lines received so far that contain `needle`.
    fn count(&self, needle: &str) -> usize {
        self.seen
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for WatchChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A fresh directory outside every Riltra home, for repositories under test.
fn workspace(tag: &str) -> PathBuf {
    home(&format!("ws-{tag}"))
}

fn write_rust(repo: &Path, text: &str) {
    let dir = repo.join("src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("lib.rs"), text).unwrap();
}

/// Appends `text` to `path`, changing its length and mtime.
fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// The snapshot `(id, nonce)` reported by `riltra index status`. Every promoted
/// build gets a new nonce; the id is per database and can repeat across builds.
#[cfg(debug_assertions)]
fn status_identity(home: &Path, name: &str) -> (String, String) {
    let (code, out, err) = run(home, &["index", "status", "--repo", name]);
    assert_eq!(code, 0, "index status failed: {err}");
    let key = "\"nonce\":\"";
    let pos = out.find(key).expect("snapshot carries a nonce");
    let nonce: String = out[pos + key.len()..]
        .chars()
        .take_while(|c| *c != '"')
        .collect();
    (envelope_snapshot(&out), nonce)
}

#[test]
fn watch_starts_on_unindexed_repo() {
    let ws = workspace("unindexed");
    let h = home("watch-unindexed");
    let repo = mkrepo(&ws, "repo");
    write_rust(&repo, "fn first() {}\n");
    let (code, _, err) = run(
        &h,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    let mut watch = WatchChild::spawn(&h, &["watch", "--repo", "r", "--max-builds", "1"], &[]);
    let started = watch.wait_line("\"event\":\"started\"", WAIT);
    assert!(started.contains("\"status\":\"unknown\""), "{started}");
    watch.wait_line("\"event\":\"built\"", WAIT);
    let (code, err) = watch.wait_exit(WAIT);
    assert_eq!(code, 0, "watch failed: {err}");
}

#[test]
fn watch_indexes_new_definition() {
    let ws = workspace("new-definition");
    let h = home("watch-new-definition");
    let Some(repo) = git_repo(&ws, "repo", "main") else {
        return;
    };
    write_rust(&repo, "fn initial_marker() {}\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "add source"]);
    init_and_index(&h, &repo, "r");
    let mut watch = WatchChild::spawn(
        &h,
        &[
            "watch",
            "--repo",
            "r",
            "--interval-ms",
            "100",
            "--debounce-ms",
            "200",
            "--max-builds",
            "1",
        ],
        &[],
    );
    // Fresh at start, so the startup build cannot use up --max-builds.
    let started = watch.wait_line("\"event\":\"started\"", WAIT);
    assert!(started.contains("\"status\":\"fresh\""), "{started}");
    append(&repo.join("src").join("lib.rs"), "fn watched_marker() {}\n");
    let (code, err) = watch.wait_exit(WAIT);
    assert_eq!(code, 0, "watch failed: {err}");
    assert_eq!(watch.count("\"event\":\"built\""), 1, "{:?}", watch.seen);
    let (code, out, err) = run(&h, &["search", "--repo", "r", "--symbol", "watched_marker"]);
    assert_eq!(code, 0, "search failed: {err}");
    assert!(out.contains("watched_marker"), "got: {out}");
    let (code, out, err) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0, "index status failed: {err}");
    assert!(out.contains("\"dirty\""), "worktree is dirty: {out}");
}

#[test]
fn watch_rebuilds_after_commit_without_new_files() {
    let ws = workspace("commit-only");
    let h = home("watch-commit-only");
    let Some(repo) = git_repo(&ws, "repo", "main") else {
        return;
    };
    init_and_index(&h, &repo, "r");
    let mut watch = WatchChild::spawn(
        &h,
        &[
            "watch",
            "--repo",
            "r",
            "--interval-ms",
            "100",
            "--debounce-ms",
            "200",
            "--max-builds",
            "1",
        ],
        &[],
    );
    watch.wait_line("\"event\":\"started\"", WAIT);
    git(&repo, &["commit", "--allow-empty", "-m", "x"]);
    let (code, err) = watch.wait_exit(WAIT);
    assert_eq!(code, 0, "watch failed: {err}");
    assert_eq!(watch.count("\"event\":\"built\""), 1, "{:?}", watch.seen);
    let (code, out, err) = run(&h, &["index", "status", "--repo", "r"]);
    assert_eq!(code, 0, "index status failed: {err}");
    assert!(out.contains("\"freshness\":\"fresh\""), "got: {out}");
}

#[test]
fn second_watch_is_refused() {
    let ws = workspace("second-watch");
    let h = home("watch-second");
    let repo = mkrepo(&ws, "repo");
    write_rust(&repo, "fn held() {}\n");
    init_and_index(&h, &repo, "r");
    let mut first = WatchChild::spawn(&h, &["watch", "--repo", "r"], &[]);
    first.wait_line("\"event\":\"started\"", WAIT);
    let mut second = WatchChild::spawn(&h, &["watch", "--repo", "r"], &[]);
    let (code, err) = second.wait_exit(WAIT);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("watch-busy"), "{err}");
}

#[test]
fn index_reports_busy_while_writer_lock_held() {
    let ws = workspace("writer-busy");
    let h = home("watch-writer-busy");
    let repo = mkrepo(&ws, "repo");
    write_rust(&repo, "fn before() {}\n");
    init_and_index(&h, &repo, "r");
    let lock_path = h.join("index").join("r.db.lock");
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    held.try_lock().expect("test takes the writer lock");
    let (code, _, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("index-busy"), "{err}");

    let mut watch = WatchChild::spawn(
        &h,
        &[
            "watch",
            "--repo",
            "r",
            "--interval-ms",
            "100",
            "--debounce-ms",
            "200",
            "--max-builds",
            "1",
        ],
        &[],
    );
    watch.wait_line("\"event\":\"started\"", WAIT);
    append(
        &repo.join("src").join("lib.rs"),
        "fn edited_while_locked() {}\n",
    );
    watch.wait_line("\"event\":\"busy\"", WAIT);
    held.unlock().expect("release the writer lock");
    watch.wait_line("\"event\":\"built\"", WAIT);
    let (code, err) = watch.wait_exit(WAIT);
    assert_eq!(code, 0, "watch failed: {err}");
}

#[test]
fn watch_refuses_root_inside_home() {
    let ws = workspace("inside-home");
    // The home is the workspace itself, so the registered repository sits inside it.
    let repo = mkrepo(&ws, "repo");
    write_rust(&repo, "fn inside() {}\n");
    let (code, _, err) = run(
        &ws,
        &["init", "--repo", repo.to_str().unwrap(), "--name", "r"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    let mut watch = WatchChild::spawn(&ws, &["watch", "--repo", "r"], &[]);
    let (code, err) = watch.wait_exit(WAIT);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("watch-root-overlaps-home"), "{err}");
}

// Needs the debug-only RILTRA_TEST_HOLD_AFTER_STAGING_MS hook, so it runs only in debug builds.
#[cfg(debug_assertions)]
#[test]
fn kill_during_build_keeps_previous_snapshot() {
    let ws = workspace("kill-build");
    let h = home("watch-kill-build");
    let repo = mkrepo(&ws, "repo");
    write_rust(&repo, "fn existing_marker() {}\n");
    init_and_index(&h, &repo, "r");
    let s1 = status_identity(&h, "r");
    let (code, out, err) = run(
        &h,
        &["search", "--repo", "r", "--symbol", "existing_marker"],
    );
    assert_eq!(code, 0, "search failed: {err}");
    assert!(out.contains("existing_marker"), "got: {out}");

    let mut watch = WatchChild::spawn(
        &h,
        &[
            "watch",
            "--repo",
            "r",
            "--interval-ms",
            "100",
            "--debounce-ms",
            "200",
        ],
        &[("RILTRA_TEST_HOLD_AFTER_STAGING_MS", "30000")],
    );
    watch.wait_line("\"event\":\"started\"", WAIT);
    append(
        &repo.join("src").join("lib.rs"),
        "fn edited_during_hold() {}\n",
    );
    // The staging database appears once the rebuild has started, so the kill
    // lands mid-build.
    let staging = h.join("index").join("r.db.staging");
    let deadline = Instant::now() + WAIT;
    while !staging.exists() {
        assert!(Instant::now() < deadline, "rebuild never staged a database");
        std::thread::sleep(Duration::from_millis(50));
    }
    watch.kill();

    assert_eq!(
        status_identity(&h, "r"),
        s1,
        "previous snapshot must stay live"
    );
    let (code, out, err) = run(
        &h,
        &["search", "--repo", "r", "--symbol", "existing_marker"],
    );
    assert_eq!(code, 0, "search after kill failed: {err}");
    assert!(out.contains("existing_marker"), "got: {out}");

    let (code, out, err) = run(&h, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "recovery index failed: {err}");
    let recovered = status_identity(&h, "r");
    assert_ne!(
        recovered.1, s1.1,
        "the recovery build must promote a new snapshot: {out}"
    );
    for suffix in ["", "-wal", "-shm"] {
        let leftover = h.join("index").join(format!("r.db.staging{suffix}"));
        assert!(!leftover.exists(), "{} survived", leftover.display());
    }
}
