// SPDX-License-Identifier: Apache-2.0
//! Tier A golden tests: self-authored fixtures per language. Each golden file
//! under `tests/golden/` was generated once and verified by hand against the
//! fixture source (names, spans, byte offsets) before being pinned.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-m2-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("m2")
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

/// Register the fixture tree once per test home; returns the home.
fn registered(home: &Path) -> PathBuf {
    let root = fixtures_root();
    let (code, _, err) = run(
        home,
        &["init", "--repo", root.to_str().unwrap(), "--name", "m2"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    home.to_path_buf()
}

/// The single `files[0]` entry of a one-`--path` scan envelope.
fn entry_of(stdout: &str) -> String {
    let start = stdout
        .find("\"files\":[")
        .expect("envelope must list files");
    let inner = &stdout[start + "\"files\":[".len()..];
    let end = inner
        .find("],\"summary\"")
        .expect("envelope must carry a summary");
    inner[..end].to_string()
}

fn check_golden(home: &Path, rel: &str, golden: &str) {
    let (code, out, err) = run(home, &["scan", "--repo", "m2", "--path", rel]);
    assert_eq!(code, 0, "scan {rel} failed: {err}");
    let entry = entry_of(&out);
    let expected = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("golden")
            .join(golden),
    )
    .unwrap_or_else(|_| panic!("missing golden {golden}; actual entry was:\n{entry}"));
    assert_eq!(entry, expected.trim_end(), "golden mismatch for {rel}");
}

#[test]
fn golden_python() {
    let h = home("py");
    registered(&h);
    check_golden(&h, "py/hello.py", "m2-py-hello.json");
}

#[test]
fn golden_javascript() {
    let h = home("js");
    registered(&h);
    check_golden(&h, "js/hello.js", "m2-js-hello.json");
}

#[test]
fn golden_typescript() {
    let h = home("ts");
    registered(&h);
    check_golden(&h, "ts/hello.ts", "m2-ts-hello.json");
}

#[test]
fn golden_rust() {
    let h = home("rs");
    registered(&h);
    check_golden(&h, "rs/hello.rs", "m2-rs-hello.json");
}

#[test]
fn golden_go() {
    let h = home("go");
    registered(&h);
    check_golden(&h, "go/hello.go", "m2-go-hello.json");
}

#[test]
fn golden_c() {
    let h = home("c");
    registered(&h);
    check_golden(&h, "c/main.c", "m2-c-main.json");
    check_golden(&h, "c/util.h", "m2-c-util.json");
}

#[test]
fn golden_cpp() {
    let h = home("cpp");
    registered(&h);
    check_golden(&h, "cpp/app.cpp", "m2-cpp-app.json");
    check_golden(&h, "cpp/qualified.cpp", "m2-cpp-qualified.json");
}

#[test]
fn qualified_cpp_names_are_recorded() {
    let h = home("qualified");
    registered(&h);
    let (code, out, err) = run(&h, &["scan", "--repo", "m2", "--path", "cpp/qualified.cpp"]);
    assert_eq!(code, 0, "{err}");
    let entry = entry_of(&out);
    for name in ["PatchMatch::Run", "PatchMatch::Problem::Print", "add"] {
        assert!(
            entry.contains(&format!("\"name\":\"{name}\"")),
            "missing qualified name {name} in: {entry}"
        );
    }
}

#[test]
fn malformed_python_recovers_with_errors() {
    let h = home("bad");
    registered(&h);
    let (code, out, err) = run(&h, &["scan", "--repo", "m2", "--path", "bad/bad.py"]);
    assert_eq!(code, 0, "{err}");
    let entry = entry_of(&out);
    assert!(entry.contains("\"errors\":1"), "got: {entry}");
    assert!(
        entry.contains("\"name\":\"good\""),
        "good def must survive: {entry}"
    );
    assert!(
        entry.contains("\"name\":\"helper\""),
        "call candidate must survive: {entry}"
    );
}

#[test]
fn unsupported_files_are_reported_never_parsed() {
    let h = home("unsup");
    registered(&h);
    let (code, out, err) = run(&h, &["scan", "--repo", "m2", "--path", "misc/note.xyz"]);
    assert_eq!(code, 0, "{err}");
    assert!(entry_of(&out).contains("\"reason\":\"unsupported-extension\""));
    let (code, out, err) = run(&h, &["scan", "--repo", "m2", "--path", "cuda/kernel.cu"]);
    assert_eq!(code, 0, "{err}");
    assert!(entry_of(&out).contains("\"reason\":\"cuda-experimental\""));
}

#[test]
fn scan_output_is_byte_deterministic() {
    let h = home("determ");
    registered(&h);
    let run_once = || run(&h, &["scan", "--repo", "m2", "--path", "py/hello.py"]).1;
    assert_eq!(run_once(), run_once());
}

#[test]
fn symbol_filter_selects_definitions() {
    let h = home("sym");
    registered(&h);
    let (code, out, err) = run(
        &h,
        &[
            "scan",
            "--repo",
            "m2",
            "--path",
            "py/hello.py",
            "--symbol",
            "Worker",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"name\":\"Worker\""), "got: {out}");
    assert!(out.contains("\"filter\":{\"symbol\":\"Worker\"}"));
}

/// Tier B hook: real MMSP validation runs only with an explicit root.
/// CI never sets it, so CI never depends on a local path.
#[test]
fn mmsp_subset_smoke_when_root_provided() {
    let Some(root) = std::env::var("RILTRA_MMSP_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let h = home("mmsp");
    let (code, _, err) = run(&h, &["init", "--repo", &root, "--name", "mmsp"]);
    assert_eq!(code, 0, "init mmsp failed: {err}");
    for rel in [
        "colmap/src/colmap/mvs/patch_match.cc",
        "colmap/src/colmap/mvs/patch_match.h",
        "colmap/src/colmap/controllers/feature_extraction.cc",
        "colmap/benchmark/reconstruction/evaluation/geometry.py",
        "colmap/doc/viewer_src/parser.ts",
    ] {
        let (code, out, err) = run(&h, &["scan", "--repo", "mmsp", "--path", rel]);
        assert_eq!(code, 0, "scan {rel} failed: {err}");
        assert!(out.contains("\"status\":\"ok\""), "got: {out}");
    }
}
