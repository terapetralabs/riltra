// SPDX-License-Identifier: Apache-2.0
//! `riltra grep` companions end to end: SCIP-keyed callers, the first-page
//! limit of the callers query, graph-proof gaps, and the golden document.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(40_000);

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-comp-{tag}-{}-{n}", std::process::id()));
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

/// Write `files` under a new repository directory and register it as `name`.
fn repo_with(home: &Path, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = home.join(name);
    for (rel, bytes) in files {
        let full = dir.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, bytes).unwrap();
    }
    let (code, _, err) = run(
        home,
        &["init", "--repo", &dir.to_string_lossy(), "--name", name],
    );
    assert_eq!(code, 0, "init: {err}");
    dir
}

fn index(home: &Path, name: &str) {
    let (code, _, err) = run(home, &["index", "--repo", name]);
    assert_eq!(code, 0, "index: {err}");
}

/// Run `grep` and return its exit code and parsed document (when it exits 0).
fn grep(home: &Path, repo: &str, pattern: &str) -> (i32, Option<Value>) {
    let (code, out, err) = run(home, &["grep", "--repo", repo, "--pattern", pattern]);
    if code == 0 {
        (code, Some(serde_json::from_str(&out).expect("grep JSON")))
    } else {
        assert!(err.starts_with("error:"), "{err}");
        (code, None)
    }
}

fn file_of<'a>(doc: &'a Value, path: &str) -> &'a Value {
    doc["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no hit in {path}: {doc}"))
}

/// `(path, basis)` of the caller companions of `path`, in document order.
fn callers_of(doc: &Value, path: &str) -> Vec<(String, String)> {
    file_of(doc, path)["companions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["why"] == "caller")
        .map(|c| {
            (
                c["path"].as_str().unwrap().to_string(),
                c["basis"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pair(path: &str, basis: &str) -> (String, String) {
    (path.to_string(), basis.to_string())
}

// --- SCIP index bytes, as in tests/semantic.rs --------------------------------

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

fn single_range(line: i64, start: i64, end: i64) -> Vec<u8> {
    let mut r = Vec::new();
    varint_field(1, line as u64, &mut r);
    varint_field(2, start as u64, &mut r);
    varint_field(3, end as u64, &mut r);
    r
}

fn occurrence(symbol: &str, roles: u64, line: i64, start: i64, end: i64) -> Vec<u8> {
    let mut o = Vec::new();
    if !symbol.is_empty() {
        str_field(2, symbol, &mut o);
    }
    varint_field(3, roles, &mut o);
    len_field(8, &single_range(line, start, end), &mut o);
    o
}

fn symbol_info(symbol: &str, display: &str) -> Vec<u8> {
    let mut s = Vec::new();
    str_field(1, symbol, &mut s);
    str_field(6, display, &mut s);
    s
}

fn document(rel: &str, occs: &[Vec<u8>], symbols: &[Vec<u8>]) -> Vec<u8> {
    let mut d = Vec::new();
    str_field(1, rel, &mut d);
    for o in occs {
        len_field(2, o, &mut d);
    }
    for s in symbols {
        len_field(3, s, &mut d);
    }
    varint_field(6, 1, &mut d);
    d
}

/// Full SCIP index payload with UTF-8 positions and the given documents.
fn index_bytes(root: &Path, docs: &[Vec<u8>]) -> Vec<u8> {
    let mut tool = Vec::new();
    str_field(1, "scip-test-cli", &mut tool);
    str_field(2, "1.0", &mut tool);
    let mut md = Vec::new();
    len_field(2, &tool, &mut md);
    str_field(
        3,
        &format!("file://{}", root.to_string_lossy().replace('\\', "/")),
        &mut md,
    );
    varint_field(4, 1, &mut md);
    let mut out = Vec::new();
    len_field(1, &md, &mut out);
    for d in docs {
        len_field(2, d, &mut out);
    }
    out
}

/// Import a SCIP index built from `docs` into the repository `name`.
fn scip_import(home: &Path, dir: &Path, name: &str, docs: &[Vec<u8>]) {
    let scip = dir.join("index.scip");
    std::fs::write(&scip, index_bytes(dir, docs)).unwrap();
    let (code, _, err) = run(
        home,
        &[
            "semantic",
            "import",
            "--repo",
            name,
            "--scip",
            &scip.to_string_lossy(),
        ],
    );
    assert_eq!(code, 0, "semantic import: {err}");
}

const HELPER: &str = "scip-test test test-pkg 1.0.0 a/helper().";

#[test]
fn grep_scip_python_calls_win_over_potential_calls() {
    let h = home("scip-python");
    let files: &[(&str, &[u8])] = &[
        ("a.py", b"def helper():\n    return 1\n"),
        // Name-matched only: POTENTIAL_CALL. Sorts before b.py by path.
        (
            "aa_caller.py",
            b"from a import helper\n\ndef early():\n    return helper()\n",
        ),
        // Proven by the SCIP occurrence: CALLS.
        (
            "b.py",
            b"from a import helper\n\ndef late():\n    return helper()\n",
        ),
    ];
    let dir = repo_with(&h, "sem", files);
    index(&h, "sem");
    let a = document(
        "a.py",
        &[occurrence(HELPER, 0x1, 0, 4, 10)],
        &[symbol_info(HELPER, "helper")],
    );
    let b = document("b.py", &[occurrence(HELPER, 0x0, 3, 11, 17)], &[]);
    scip_import(&h, &dir, "sem", &[a, b]);

    let (code, doc) = grep(&h, "sem", "def helper");
    assert_eq!(code, 0);
    let doc = doc.unwrap();
    let callers = callers_of(&doc, "a.py");
    assert_eq!(callers.first(), Some(&pair("b.py", "CALLS")), "{callers:?}");
    assert!(
        callers.contains(&pair("aa_caller.py", "POTENTIAL_CALL")),
        "{callers:?}"
    );
    assert_eq!(doc["caller_gaps"], serde_json::json!([]));
}

#[test]
fn grep_scip_typescript_and_c_definitions_do_not_fail_the_search() {
    let h = home("scip-ts-c");
    let files: &[(&str, &[u8])] = &[
        ("a.ts", b"export class Box {\n  fill() {}\n}\nnew Box().fill();\n"),
        (
            "c.c",
            b"typedef int MyType;\nMyType helper(void) {\n    return 1;\n}\nint main(void) {\n    return helper();\n}\n",
        ),
    ];
    let dir = repo_with(&h, "sem", files);
    index(&h, "sem");
    let fill = "scip-test test test-pkg 1.0.0 a/Box#fill().";
    let c_helper = "scip-test test test-pkg 1.0.0 c/helper().";
    let a = document(
        "a.ts",
        &[
            occurrence(fill, 0x1, 1, 2, 6),
            occurrence(fill, 0x0, 3, 8, 12),
        ],
        &[symbol_info(fill, "fill")],
    );
    let c = document(
        "c.c",
        &[
            occurrence(c_helper, 0x1, 1, 7, 13),
            occurrence(c_helper, 0x0, 5, 11, 17),
        ],
        &[symbol_info(c_helper, "helper")],
    );
    scip_import(&h, &dir, "sem", &[a, c]);

    // A definition the resolver maps through its SCIP identity must not turn
    // the whole search into an error (exit 3).
    let (code, doc) = grep(&h, "sem", "fill() {}");
    assert_eq!(code, 0, "TypeScript class method failed the search");
    assert!(doc.unwrap()["caller_gaps"].is_array());
    let (code, doc) = grep(&h, "sem", "MyType helper");
    assert_eq!(code, 0, "C function with a return type failed the search");
    assert!(doc.unwrap()["caller_gaps"].is_array());
}

#[test]
fn grep_callers_past_the_first_page_keep_production_callers() {
    let h = home("page");
    let mut test_file = String::from("from lib import helper\n\n");
    for i in 1..=60 {
        test_file.push_str(&format!("def t{i}():\n    return helper()\n\n"));
    }
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("lib.py", b"def helper():\n    return 1\n".to_vec()),
        // Test-file edges sort first and fill the callers query's first page.
        ("aaa/test_many.py", test_file.into_bytes()),
        (
            "prod.py",
            b"from lib import helper\n\ndef prod():\n    return helper()\n".to_vec(),
        ),
    ];
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    repo_with(&h, "page", &refs);
    index(&h, "page");

    let (code, doc) = grep(&h, "page", "def helper");
    assert_eq!(code, 0);
    let callers = callers_of(&doc.unwrap(), "lib.py");
    assert_eq!(callers, [pair("prod.py", "POTENTIAL_CALL")], "{callers:?}");
}

#[test]
fn grep_graph_proof_mismatch_skips_callers_and_names_the_gap() {
    let h = home("proof");
    let files: &[(&str, &[u8])] = &[
        ("lib.py", b"def helper():\n    return 1\n"),
        (
            "app.py",
            b"from lib import helper\n\ndef run():\n    return helper()\n",
        ),
    ];
    repo_with(&h, "proof", files);
    index(&h, "proof");
    let db = h.join("index").join("proof.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE meta SET value = 'stale-proof' WHERE key = 'graph_proof'",
        [],
    )
    .unwrap();
    drop(conn);

    let (code, doc) = grep(&h, "proof", "def helper");
    assert_eq!(code, 0, "a skipped caller lookup must not fail the search");
    let doc = doc.unwrap();
    assert_eq!(
        doc["caller_gaps"],
        serde_json::json!(["graph-proof-incompatible"])
    );
    let companions = file_of(&doc, "lib.py")["companions"].as_array().unwrap();
    assert!(companions
        .iter()
        .any(|c| c["why"] == "importer" && c["path"] == "app.py"));
    assert!(callers_of(&doc, "lib.py").is_empty());
}

#[test]
fn grep_cli_companions_match_golden() {
    let h = home("golden");
    let files: &[(&str, &[u8])] = &[
        ("lib/core.py", b"def helper():\n    return 1\n"),
        ("lib/core.pyi", b"def helper() -> int: ...\n"),
        (
            "app.py",
            b"from lib.core import helper\n\ndef run():\n    return helper()\n",
        ),
    ];
    repo_with(&h, "golden", files);
    index(&h, "golden");
    let (code, out, err) = run(&h, &["grep", "--repo", "golden", "--pattern", "helper()"]);
    assert_eq!(code, 0, "stderr: {err:?}");
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/grep-companions.json"
    );
    let golden = std::fs::read_to_string(path).expect("golden file");
    assert_eq!(out, golden);
}
