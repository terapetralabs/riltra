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
fn grep_scip_typescript_and_c_methods_name_their_callers() {
    let h = home("scip-ts-c");
    let files: &[(&str, &[u8])] = &[
        ("a.ts", b"export function fill() {}\n"),
        (
            "b.ts",
            b"import { fill } from \"./a\";\nexport function run() {\n  fill();\n}\n",
        ),
        ("c.h", b"typedef int MyType;\nMyType helper(void);\n"),
        (
            "c.c",
            b"#include \"c.h\"\nMyType helper(void) {\n    return 1;\n}\n",
        ),
        (
            "main.c",
            b"#include \"c.h\"\nint main(void) {\n    return helper();\n}\n",
        ),
    ];
    let dir = repo_with(&h, "sem", files);
    index(&h, "sem");
    let fill = "scip-test test test-pkg 1.0.0 a/fill().";
    let c_helper = "scip-test test test-pkg 1.0.0 c/helper().";
    let a = document(
        "a.ts",
        &[occurrence(fill, 0x1, 0, 16, 20)],
        &[symbol_info(fill, "fill")],
    );
    let b = document("b.ts", &[occurrence(fill, 0x0, 2, 2, 6)], &[]);
    let c = document(
        "c.c",
        &[occurrence(c_helper, 0x1, 1, 7, 13)],
        &[symbol_info(c_helper, "helper")],
    );
    let main = document("main.c", &[occurrence(c_helper, 0x0, 2, 11, 17)], &[]);
    scip_import(&h, &dir, "sem", &[a, b, c, main]);

    // The method and the C function are named by their SCIP identity: the
    // search succeeds and the caller with CALLS is listed for each.
    let (code, doc) = grep(&h, "sem", "fill() {}");
    assert_eq!(code, 0, "TypeScript class method failed the search");
    let doc = doc.unwrap();
    assert_eq!(callers_of(&doc, "a.ts"), [pair("b.ts", "CALLS")]);
    assert_eq!(doc["caller_gaps"], serde_json::json!([]));
    let (code, doc) = grep(&h, "sem", "MyType helper");
    assert_eq!(code, 0, "C function with a return type failed the search");
    let doc = doc.unwrap();
    assert_eq!(callers_of(&doc, "c.c"), [pair("main.c", "CALLS")]);
    assert_eq!(doc["caller_gaps"], serde_json::json!([]));
}

#[test]
fn grep_scip_go_method_is_named_by_its_name_not_its_receiver() {
    let h = home("scip-go");
    let err_go: &[u8] =
        b"package p\n\ntype Error struct{}\n\nfunc (e *Error) Error() string {\n\treturn \"x\"\n}\n";
    let use_go: &[u8] = b"package p\n\nfunc use(e *Error) string {\n\treturn e.Error()\n}\n";
    let dir = repo_with(&h, "gosem", &[("p/err.go", err_go), ("p/use.go", use_go)]);
    index(&h, "gosem");
    let type_sym = "scip-test test test-pkg 1.0.0 p/Error#";
    let method_sym = "scip-test test test-pkg 1.0.0 p/Error#Error().";
    // The receiver type `Error` is the first whole word of the method, but it
    // is a reference to the type. The method's own name is the definition.
    let err_doc = document(
        "p/err.go",
        &[
            occurrence(type_sym, 0x1, 2, 5, 10),
            occurrence(method_sym, 0x1, 4, 16, 21),
            occurrence(type_sym, 0x0, 4, 9, 14),
        ],
        &[
            symbol_info(type_sym, "Error"),
            symbol_info(method_sym, "Error"),
        ],
    );
    let use_doc = document(
        "p/use.go",
        &[
            occurrence(type_sym, 0x0, 2, 12, 17),
            occurrence(method_sym, 0x0, 3, 10, 15),
        ],
        &[],
    );
    scip_import(&h, &dir, "gosem", &[err_doc, use_doc]);

    let (code, doc) = grep(&h, "gosem", "func (e *Error) Error");
    assert_eq!(code, 0, "Go method failed the search");
    let doc = doc.unwrap();
    assert_eq!(callers_of(&doc, "p/err.go"), [pair("p/use.go", "CALLS")]);
    assert_eq!(doc["caller_gaps"], serde_json::json!([]));
}

#[test]
fn grep_rust_use_trees_of_any_depth_do_not_crash_the_search() {
    let h = home("rust-deep");
    // 20,000 path segments that name the module `a` first, a chain that never
    // names it, and 20,000 nested braces. None may exhaust the call stack.
    let mut chain = String::from("use crate::");
    for _ in 0..20_000 {
        chain.push_str("a::");
    }
    chain.push_str("a;\n");
    let mut other_chain = String::from("use crate::");
    for _ in 0..20_000 {
        other_chain.push_str("b::");
    }
    other_chain.push_str("b;\n");
    let mut braces = String::from("use crate::");
    for _ in 0..20_000 {
        braces.push('{');
    }
    braces.push('x');
    for _ in 0..20_000 {
        braces.push('}');
    }
    braces.push_str(";\n");
    let files: &[(&str, &[u8])] = &[
        ("src/a.rs", b"pub fn thing() {}\n"),
        ("src/chain.rs", chain.as_bytes()),
        ("src/other.rs", other_chain.as_bytes()),
        ("src/braces.rs", braces.as_bytes()),
    ];
    repo_with(&h, "deep", files);
    index(&h, "deep");
    let (code, doc) = grep(&h, "deep", "pub fn thing");
    assert_eq!(code, 0, "a deep use tree failed the search");
    let doc = doc.unwrap();
    let importers: Vec<&str> = file_of(&doc, "src/a.rs")["companions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["why"] == "importer")
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(importers, ["src/chain.rs"]);
}

#[test]
fn grep_oversize_rust_parent_is_skipped_not_fatal() {
    let h = home("rust-big-parent");
    // src/lib.rs declares `mod a` but is over the size limit, so it is not
    // read; the search still succeeds and names no importer from it.
    let mut big = String::from("pub mod a;\n");
    big.push_str(&"// padding\n".repeat(140_000));
    let files: &[(&str, &[u8])] = &[
        ("src/lib.rs", big.as_bytes()),
        ("src/a.rs", b"pub fn thing() {}\n"),
    ];
    repo_with(&h, "big", files);
    index(&h, "big");
    let (code, doc) = grep(&h, "big", "pub fn thing");
    assert_eq!(code, 0, "an oversize parent failed the search");
    let doc = doc.unwrap();
    let importers: Vec<&str> = file_of(&doc, "src/a.rs")["companions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["why"] == "importer")
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(importers.is_empty(), "{importers:?}");
}

#[test]
fn grep_many_callers_keep_the_same_caller_set() {
    let h = home("many-callers");
    let mut many = String::from("from lib import helper\n\n");
    for i in 1..=2_500 {
        many.push_str(&format!("def f{i}():\n    return helper()\n\n"));
    }
    let mut test_file = String::from("from lib import helper\n\n");
    for i in 1..=50 {
        test_file.push_str(&format!("def t{i}():\n    return helper()\n\n"));
    }
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("lib.py", b"def helper():\n    return 1\n".to_vec()),
        ("many.py", many.into_bytes()),
        ("aaa/test_x.py", test_file.into_bytes()),
        (
            "prod.py",
            b"from lib import helper\n\ndef prod():\n    return helper()\n".to_vec(),
        ),
    ];
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    repo_with(&h, "many", &refs);
    index(&h, "many");
    let (code, doc) = grep(&h, "many", "def helper");
    assert_eq!(code, 0);
    let callers = callers_of(&doc.unwrap(), "lib.py");
    assert_eq!(
        callers,
        [
            pair("many.py", "POTENTIAL_CALL"),
            pair("prod.py", "POTENTIAL_CALL"),
        ]
    );
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
fn grep_rust_callers_name_their_caller_files() {
    let h = home("rust-callers");
    // Rust call edges carry the caller's file in `caller_file` only; the
    // `caller_path` column is NULL for them. The caller companion must read the file.
    let files: &[(&str, &[u8])] = &[
        ("src/a.rs", b"pub fn helper() -> u32 {\n    1\n}\n"),
        (
            "src/b.rs",
            b"use crate::a::helper;\n\npub fn run() -> u32 {\n    helper()\n}\n",
        ),
    ];
    repo_with(&h, "rustc", files);
    index(&h, "rustc");
    let (code, doc) = grep(&h, "rustc", "pub fn helper");
    assert_eq!(code, 0, "a Rust caller edge failed the search");
    let callers = callers_of(&doc.unwrap(), "src/a.rs");
    assert_eq!(callers, [pair("src/b.rs", "POTENTIAL_CALL")], "{callers:?}");
}

#[test]
fn grep_module_level_calls_name_their_caller_files() {
    let h = home("module-level");
    // A call outside any definition has no caller definition: its edge keeps
    // the file in `caller_file` and has no `caller_path`.
    let files: &[(&str, &[u8])] = &[
        ("lib.py", b"def helper():\n    return 1\n"),
        ("top.py", b"from lib import helper\n\nVALUE = helper()\n"),
    ];
    repo_with(&h, "top", files);
    index(&h, "top");
    let (code, doc) = grep(&h, "top", "def helper");
    assert_eq!(code, 0, "a module-level call failed the search");
    let callers = callers_of(&doc.unwrap(), "lib.py");
    assert_eq!(callers, [pair("top.py", "POTENTIAL_CALL")], "{callers:?}");
}

#[test]
fn grep_scip_cpp_out_of_line_method_names_its_callers() {
    let h = home("scip-cpp");
    // The definition is `Widget::run`, defined out of line. The parser keeps the
    // qualified name; the SCIP definition occurrence covers only `run`.
    let files: &[(&str, &[u8])] = &[
        (
            "widget.h",
            b"class Widget {\npublic:\n    void run();\n};\n",
        ),
        (
            "widget.cc",
            b"#include \"widget.h\"\nvoid Widget::run() {\n}\n",
        ),
        (
            "m.cc",
            b"#include \"widget.h\"\nvoid use(Widget& w) {\n  w.run();\n}\n",
        ),
    ];
    let dir = repo_with(&h, "cppsem", files);
    index(&h, "cppsem");
    let run = "scip-test test test-pkg 1.0.0 widget/Widget#run().";
    let widget = document(
        "widget.cc",
        &[occurrence(run, 0x1, 1, 13, 16)],
        &[symbol_info(run, "run")],
    );
    let m = document("m.cc", &[occurrence(run, 0x0, 2, 4, 7)], &[]);
    scip_import(&h, &dir, "cppsem", &[widget, m]);

    let (code, doc) = grep(&h, "cppsem", "void Widget::run");
    assert_eq!(code, 0, "C++ out-of-line method failed the search");
    let doc = doc.unwrap();
    assert_eq!(callers_of(&doc, "widget.cc"), [pair("m.cc", "CALLS")]);
    assert_eq!(doc["caller_gaps"], serde_json::json!([]));
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
