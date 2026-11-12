// SPDX-License-Identifier: Apache-2.0
//! By-name anchors for `callers`, `callees`, `trace`, `impact` and `context`:
//! `--target <name> [--in <rel>] [--kind <kind>]` resolves to one definition
//! (answering exactly like the position form) or answers `ambiguous`,
//! `not-found` or `no-index` as a document with exit 0.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-workflow-{tag}-{}-{n}", std::process::id()));
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

/// Every file under `dir` (relative, sorted): the no-creation assertions.
fn listing(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                out.push(path.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Fixture: `save` is defined twice in `a.py` (a function and `Box.save`) and
/// once in `b.py`; `save_all` lives in `b.py` and is called from `c.py`.
fn fixture(home: &Path, name: &str) -> PathBuf {
    let dir = home.join(format!("{name}-src"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.py"),
        "def save(x):\n    return x\n\n\nclass Box:\n    def save(self):\n        return self\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.py"),
        "def save(x):\n    return x\n\n\ndef save_all(items):\n    for item in items:\n        save(item)\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("c.py"),
        "from b import save_all\n\n\ndef run():\n    save_all([])\n",
    )
    .unwrap();
    dir
}

fn indexed(home: &Path, name: &str) {
    let dir = fixture(home, name);
    let (code, _out, err) = run(
        home,
        &["init", "--repo", &dir.to_string_lossy(), "--name", name],
    );
    assert_eq!(code, 0, "init: {err}");
    let (code, _out, err) = run(home, &["index", "--repo", name]);
    assert_eq!(code, 0, "index: {err}");
}

fn json_of(out: &str) -> Value {
    serde_json::from_str(out.trim_end()).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{out}"))
}

/// The definition span start of `symbol` as `search` reports it: (path, line, col).
fn span_of(
    home: &Path,
    repo: &str,
    symbol: &str,
    path: &str,
    kind: &str,
) -> (String, String, String) {
    let (code, out, err) = run(home, &["search", "--repo", repo, "--symbol", symbol]);
    assert_eq!(code, 0, "search: {err}");
    let hit = json_of(&out)["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["path"] == path && h["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} {symbol} in {path}: {out}"))
        .clone();
    (
        path.to_string(),
        hit["span"]["start_line"].to_string(),
        hit["span"]["start_col"].to_string(),
    )
}

/// The position form of `cmd` for `span`, plus any extra arguments.
fn position_args<'a>(
    cmd: &'a str,
    repo: &'a str,
    span: &'a (String, String, String),
) -> Vec<&'a str> {
    vec![
        cmd,
        "--repo",
        repo,
        "--path",
        span.0.as_str(),
        "--line",
        span.1.as_str(),
        "--col",
        span.2.as_str(),
    ]
}

#[test]
fn unique_name_resolves_to_the_position_form_byte_for_byte() {
    let h = home("unique");
    indexed(&h, "w");
    let span = span_of(&h, "w", "save_all", "b.py", "function");
    let (code, by_name, err) = run(&h, &["callers", "--repo", "w", "--target", "save_all"]);
    assert_eq!(code, 0, "{err}");
    let (code, by_position, err) = run(&h, &position_args("callers", "w", &span));
    assert_eq!(code, 0, "{err}");
    assert_eq!(by_name, by_position);
    assert!(json_of(&by_name).get("status").is_none(), "{by_name}");
}

#[test]
fn ambiguous_exact_name_answers_a_document_in_fixed_key_order() {
    let h = home("ambiguous");
    indexed(&h, "w");
    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "save"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with(
            "{\"schema_version\":1,\"repo\":\"w\",\"status\":\"ambiguous\",\"target\":\"save\",\"in\":null,\"kind\":null,\"total\":3,\"total_exact\":true,\"candidates\":[{\"path\":\"a.py\",\"line\":1,\"col\":1,\"kind\":\"function\",\"reason\":\"exact-name\"},{\"path\":\"a.py\",\"line\":6,"
        ),
        "{out}"
    );
    let doc = json_of(&out);
    assert_eq!(doc["truncated"], false, "{doc}");
    let candidates = doc["candidates"].as_array().unwrap();
    let seen: Vec<(&str, u64, &str, &str)> = candidates
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap(),
                c["line"].as_u64().unwrap(),
                c["kind"].as_str().unwrap(),
                c["reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("a.py", 1, "function", "exact-name"),
            ("a.py", 6, "function", "exact-name"),
            ("b.py", 1, "function", "exact-name"),
        ],
        "{out}"
    );
}

#[test]
fn filters_narrow_exact_matches_and_an_unmatched_kind_is_not_found() {
    let h = home("filters");
    indexed(&h, "w");
    let (code, out, err) = run(
        &h,
        &["callers", "--repo", "w", "--target", "save", "--in", "b.py"],
    );
    assert_eq!(code, 0, "{err}");
    let span = span_of(&h, "w", "save", "b.py", "function");
    let (_, by_position, _) = run(&h, &position_args("callers", "w", &span));
    assert_eq!(out, by_position);

    // Python functions and methods are stored as `function`; Go methods are `method`.
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "w", "--target", "save_all", "--kind", "function",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let span = span_of(&h, "w", "save_all", "b.py", "function");
    let (_, by_position, _) = run(&h, &position_args("callers", "w", &span));
    assert_eq!(out, by_position);

    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "w", "--target", "save", "--kind", "class",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "not-found", "{doc}");
    assert_eq!(doc["kind"], "class", "{doc}");
}

#[test]
fn substring_matches_never_auto_resolve_even_when_there_is_one() {
    let h = home("substring");
    indexed(&h, "w");
    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "sav"]);
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "ambiguous", "{doc}");
    assert_eq!(doc["total"], 4, "{doc}");
    assert_eq!(doc["total_exact"], true, "{doc}");
    assert_eq!(doc["candidates"].as_array().unwrap().len(), 4, "{doc}");
    assert!(
        doc["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["reason"] == "name-contains"),
        "{doc}"
    );

    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "save_al"]);
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "ambiguous", "{doc}");
    assert_eq!(doc["total"], 1, "{doc}");
    assert_eq!(doc["candidates"][0]["reason"], "name-contains", "{doc}");
    assert_eq!(doc["candidates"][0]["path"], "b.py", "{doc}");
}

#[test]
fn unknown_name_is_not_found_with_exit_zero() {
    let h = home("notfound");
    indexed(&h, "w");
    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "nothing_here"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "{\"schema_version\":1,\"repo\":\"w\",\"status\":\"not-found\",\"target\":\"nothing_here\",\"in\":null,\"kind\":null,\"total\":0,\"total_exact\":true,\"candidates\":[],\"truncated\":false}\n"
    );
}

#[test]
fn missing_index_is_no_index_and_creates_no_database() {
    let h = home("noindex");
    let dir = fixture(&h, "w");
    let (code, _out, err) = run(
        &h,
        &["init", "--repo", &dir.to_string_lossy(), "--name", "w"],
    );
    assert_eq!(code, 0, "{err}");
    let before = listing(&h);
    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "save"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "{\"schema_version\":1,\"repo\":\"w\",\"status\":\"no-index\",\"target\":\"save\",\"in\":null,\"kind\":null,\"total\":0,\"total_exact\":true,\"candidates\":[],\"truncated\":false}\n"
    );
    assert_eq!(listing(&h), before, "lookup must not create storage");
}

#[test]
fn exclusive_and_invalid_forms_are_usage_errors() {
    let h = home("usage");
    indexed(&h, "w");
    for args in [
        vec!["callers", "--repo", "w", "--target", "x", "--line", "1"],
        vec![
            "callers", "--repo", "w", "--target", "x", "--path", "a.py", "--line", "1", "--col",
            "1",
        ],
        vec![
            "callers", "--repo", "w", "--path", "a.py", "--line", "1", "--col", "1", "--in", "a.py",
        ],
        vec!["callers", "--repo", "w", "--in", "a.py"],
        vec!["callers", "--repo", "w", "--target", ""],
        vec!["callers", "--repo", "w", "--target", "   "],
        vec!["callers", "--repo", "w", "--path", "a.py"],
        vec!["trace", "--repo", "w", "--target", "x", "--line", "1"],
        vec!["impact", "--repo", "w", "--kind", "method"],
        vec!["context", "--repo", "w", "--target", "x", "--path", "a.py"],
    ] {
        let (code, _out, _err) = run(&h, &args);
        assert_eq!(code, 2, "{args:?}");
    }
}

#[test]
fn all_five_commands_resolve_the_same_name() {
    let h = home("five");
    indexed(&h, "w");
    let span = span_of(&h, "w", "save_all", "b.py", "function");
    for cmd in ["callers", "callees"] {
        let (code, by_name, err) = run(&h, &[cmd, "--repo", "w", "--target", "save_all"]);
        assert_eq!(code, 0, "{cmd}: {err}");
        let (_, by_position, _) = run(&h, &position_args(cmd, "w", &span));
        assert_eq!(by_name, by_position, "{cmd}");
    }
    let mut trace = position_args("trace", "w", &span);
    trace.extend(["--to", "b.py:1:1"]);
    let (code, by_position, err) = run(&h, &trace);
    assert_eq!(code, 0, "trace position: {err}");
    let (code, by_name, err) = run(
        &h,
        &[
            "trace", "--repo", "w", "--target", "save_all", "--to", "b.py:1:1",
        ],
    );
    assert_eq!(code, 0, "trace name: {err}");
    assert_eq!(json_of(&by_name), json_of(&by_position), "trace");
    for cmd in ["impact", "context"] {
        let (code, by_name, err) = run(&h, &[cmd, "--repo", "w", "--target", "save_all"]);
        assert_eq!(code, 0, "{cmd}: {err}");
        assert_eq!(
            json_of(&by_name)["status"],
            Value::Null,
            "{cmd} resolved, not a status document"
        );
        let (_, by_position, _) = run(&h, &position_args(cmd, "w", &span));
        assert_eq!(json_of(&by_name), json_of(&by_position), "{cmd}");
    }
}

// --- Fix round 1 ------------------------------------------------------------

/// `count` Python definitions named `dup` in one file (exact-name total).
fn dup_repo(home: &Path, name: &str, count: usize) {
    let dir = home.join(format!("{name}-src"));
    std::fs::create_dir_all(&dir).unwrap();
    let body: String = (0..count).map(|_| "def dup():\n    pass\n\n\n").collect();
    std::fs::write(dir.join("dup.py"), body).unwrap();
    let (code, _out, err) = run(
        home,
        &["init", "--repo", &dir.to_string_lossy(), "--name", name],
    );
    assert_eq!(code, 0, "init: {err}");
    let (code, _out, err) = run(home, &["index", "--repo", name]);
    assert_eq!(code, 0, "index: {err}");
}

/// More than 1000 functions `item_0000`.. in `big.py`, one `item_z` in `z.py`.
fn big_repo(home: &Path, name: &str) {
    let dir = home.join(format!("{name}-src"));
    std::fs::create_dir_all(&dir).unwrap();
    let body: String = (0..1050)
        .map(|n| format!("def item_{n:04}():\n    pass\n\n\n"))
        .collect();
    std::fs::write(dir.join("big.py"), body).unwrap();
    std::fs::write(dir.join("z.py"), "def item_z():\n    pass\n").unwrap();
    let (code, _out, err) = run(
        home,
        &["init", "--repo", &dir.to_string_lossy(), "--name", name],
    );
    assert_eq!(code, 0, "init: {err}");
    let (code, _out, err) = run(home, &["index", "--repo", name]);
    assert_eq!(code, 0, "index: {err}");
}

#[test]
fn substring_filters_are_applied_in_sql_over_every_match() {
    let h = home("big-filter");
    big_repo(&h, "big");
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "big", "--target", "item_", "--in", "z.py",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "ambiguous", "{doc}");
    assert_eq!(doc["total"], 1, "{doc}");
    assert_eq!(doc["total_exact"], true, "{doc}");
    assert_eq!(doc["candidates"][0]["path"], "z.py", "{doc}");
    assert_eq!(doc["candidates"][0]["reason"], "name-contains", "{doc}");
}

#[test]
fn substring_kind_total_counts_every_match_and_truncates_the_list() {
    let h = home("big-kind");
    big_repo(&h, "big");
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "big", "--target", "item_", "--kind", "function",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "ambiguous", "{doc}");
    assert_eq!(doc["total"], 1051, "{doc}");
    assert_eq!(doc["total_exact"], true, "{doc}");
    assert_eq!(doc["candidates"].as_array().unwrap().len(), 20, "{doc}");
    assert_eq!(doc["truncated"], true, "{doc}");
    let (code, out, err) = run(
        &h,
        &[
            "callers", "--repo", "big", "--target", "item_", "--kind", "class",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_of(&out)["status"], "not-found", "{out}");
}

#[test]
fn exact_total_of_twenty_is_not_truncated() {
    let h = home("dup20");
    dup_repo(&h, "d", 20);
    let (code, out, err) = run(&h, &["callers", "--repo", "d", "--target", "dup"]);
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["total"], 20, "{doc}");
    assert_eq!(doc["candidates"].as_array().unwrap().len(), 20, "{doc}");
    assert_eq!(doc["truncated"], false, "{doc}");
}

#[test]
fn exact_total_of_twenty_one_is_truncated_at_twenty_listed() {
    let h = home("dup21");
    dup_repo(&h, "d", 21);
    let (code, out, err) = run(&h, &["callers", "--repo", "d", "--target", "dup"]);
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["total"], 21, "{doc}");
    assert_eq!(doc["candidates"].as_array().unwrap().len(), 20, "{doc}");
    assert_eq!(doc["truncated"], true, "{doc}");
}

#[test]
fn go_kind_is_the_stored_kind_per_language() {
    let h = home("go-kind");
    let dir = h.join("g-src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("store.go"),
        "package main\n\nfunc save() {}\n\ntype Box struct{}\n\nfunc (b Box) save() {}\n",
    )
    .unwrap();
    let (code, _out, err) = run(
        &h,
        &["init", "--repo", &dir.to_string_lossy(), "--name", "g"],
    );
    assert_eq!(code, 0, "init: {err}");
    let (code, _out, err) = run(&h, &["index", "--repo", "g"]);
    assert_eq!(code, 0, "index: {err}");
    let method = span_of(&h, "g", "save", "store.go", "method");
    let function = span_of(&h, "g", "save", "store.go", "function");
    let (code, by_name, err) = run(
        &h,
        &[
            "callers", "--repo", "g", "--target", "save", "--kind", "method",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let (_, by_position, _) = run(&h, &position_args("callers", "g", &method));
    assert_eq!(by_name, by_position, "method");
    let (code, by_name, err) = run(
        &h,
        &[
            "callers", "--repo", "g", "--target", "save", "--kind", "function",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let (_, by_position, _) = run(&h, &position_args("callers", "g", &function));
    assert_eq!(by_name, by_position, "function");
}

#[test]
fn ready_database_without_a_snapshot_is_no_index() {
    let h = home("nosnap");
    indexed(&h, "w");
    let conn = rusqlite::Connection::open(h.join("index").join("w.db")).unwrap();
    conn.execute("DELETE FROM snapshots", []).unwrap();
    drop(conn);
    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "save"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "{\"schema_version\":1,\"repo\":\"w\",\"status\":\"no-index\",\"target\":\"save\",\"in\":null,\"kind\":null,\"total\":0,\"total_exact\":true,\"candidates\":[],\"truncated\":false}\n"
    );
}

#[test]
fn blank_in_or_kind_is_a_usage_error() {
    let h = home("blank-filters");
    indexed(&h, "w");
    for args in [
        vec!["callers", "--repo", "w", "--target", "save", "--in", " "],
        vec!["callees", "--repo", "w", "--target", "save", "--kind", ""],
        vec!["trace", "--repo", "w", "--target", "save", "--in", "\t"],
        vec!["impact", "--repo", "w", "--target", "save", "--kind", "  "],
        vec!["context", "--repo", "w", "--target", "save", "--kind", ""],
    ] {
        let (code, _out, _err) = run(&h, &args);
        assert_eq!(code, 2, "{args:?}");
    }
}

#[test]
fn position_form_with_kind_is_a_usage_error() {
    let h = home("position-kind");
    indexed(&h, "w");
    let (code, _out, _err) = run(
        &h,
        &[
            "callers", "--repo", "w", "--path", "b.py", "--line", "1", "--col", "1", "--kind",
            "function",
        ],
    );
    assert_eq!(code, 2);
}

#[test]
fn invalid_to_refuses_the_same_way_in_both_forms() {
    let h = home("to-parity");
    indexed(&h, "w");
    let (pc, _po, pe) = run(
        &h,
        &[
            "trace",
            "--repo",
            "w",
            "--path",
            "b.py",
            "--line",
            "1",
            "--col",
            "1",
            "--to",
            "../escape:1:1",
        ],
    );
    let (nc, _no, ne) = run(
        &h,
        &[
            "trace",
            "--repo",
            "w",
            "--target",
            "save",
            "--to",
            "../escape:1:1",
        ],
    );
    assert_eq!(pc, 3, "{pe}");
    assert_eq!(nc, pc, "{ne}");
    assert_eq!(ne, pe);
}

#[test]
fn context_options_are_checked_before_a_name_answers() {
    let h = home("context-options");
    indexed(&h, "w");
    for tail in [&["--semantic"][..], &["--query", ""][..]] {
        let mut position = vec![
            "context", "--repo", "w", "--path", "b.py", "--line", "1", "--col", "1",
        ];
        position.extend_from_slice(tail);
        let mut name = vec!["context", "--repo", "w", "--target", "save"];
        name.extend_from_slice(tail);
        let (pc, _po, pe) = run(&h, &position);
        let (nc, no, ne) = run(&h, &name);
        assert_eq!(pc, 2, "{pe}");
        assert_eq!(nc, 2, "{ne} {no}");
    }
}

// --- Fix round 2 ------------------------------------------------------------

#[test]
fn substring_matches_are_true_substrings_of_the_name() {
    let h = home("true-substring");
    indexed(&h, "w");
    for term in ["save a", "save all"] {
        let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", term]);
        assert_eq!(code, 0, "{err}");
        let doc = json_of(&out);
        assert_eq!(doc["status"], "not-found", "{term}: {doc}");
    }
}

#[test]
fn substring_matching_is_case_insensitive_and_exact_names_are_not() {
    let h = home("case-substring");
    indexed(&h, "w");
    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "SAVE_ALL"]);
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "ambiguous", "{doc}");
    assert_eq!(doc["total"], 1, "{doc}");
    assert_eq!(doc["candidates"][0]["path"], "b.py", "{doc}");
    assert_eq!(doc["candidates"][0]["reason"], "name-contains", "{doc}");
}

#[test]
fn underscore_inside_a_substring_is_a_literal_character() {
    let h = home("underscore-substring");
    indexed(&h, "w");
    let (code, out, err) = run(&h, &["callers", "--repo", "w", "--target", "ave_al"]);
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "ambiguous", "{doc}");
    assert_eq!(doc["total"], 1, "{doc}");
    assert_eq!(doc["candidates"][0]["reason"], "name-contains", "{doc}");
}

// --- Fix round 3 ------------------------------------------------------------

#[test]
fn like_metacharacters_in_a_target_are_literal() {
    let h = home("like-escape");
    let dir = h.join("esc-src");
    std::fs::create_dir_all(&dir).unwrap();
    // Decoys: `axbc` is matched by an unescaped `a_b`, `x10y` by an unescaped
    // `10%`, `cab` by the pattern an unescaped `a\b` turns into (`%ab%`).
    std::fs::write(
        dir.join("esc.py"),
        "def a_bc():\n    pass\n\n\ndef axbc():\n    pass\n\n\ndef x10y():\n    pass\n\n\ndef cab():\n    pass\n",
    )
    .unwrap();
    let (code, _out, err) = run(
        &h,
        &["init", "--repo", &dir.to_string_lossy(), "--name", "esc"],
    );
    assert_eq!(code, 0, "init: {err}");
    let (code, _out, err) = run(&h, &["index", "--repo", "esc"]);
    assert_eq!(code, 0, "index: {err}");

    // `_` is literal: only `a_bc` contains `a_b`, not the `axbc` decoy.
    let (code, out, err) = run(&h, &["callers", "--repo", "esc", "--target", "a_b"]);
    assert_eq!(code, 0, "{err}");
    let doc = json_of(&out);
    assert_eq!(doc["status"], "ambiguous", "{doc}");
    assert_eq!(doc["total"], 1, "{doc}");
    assert_eq!(doc["candidates"][0]["line"], 1, "{doc}");
    assert_eq!(doc["candidates"][0]["reason"], "name-contains", "{doc}");

    // `%` is literal: `10%` is in no name, though `x10y` would match `%10%%`.
    let (code, out, err) = run(&h, &["callers", "--repo", "esc", "--target", "10%"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_of(&out)["status"], "not-found", "{out}");

    // A backslash is literal and never an error: no name contains `a\b`.
    let (code, out, err) = run(&h, &["callers", "--repo", "esc", "--target", "a\\b"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(json_of(&out)["status"], "not-found", "{out}");
}
