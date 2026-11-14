// SPDX-License-Identifier: Apache-2.0
//! `riltra changes` (Task 5 core): which definitions a git change touches.
//! Expectations are written by hand from the edits, never copied from output.
//! Symbols come from the live parse; caller counts need an index.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-changes-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn git_available() -> bool {
    let present = Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !present {
        eprintln!("skipped: git is not installed");
    }
    present
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, rel: &str, content: &str) {
    std::fs::write(dir.join(rel), content).unwrap();
}

/// Repository with one commit holding `files`. Line endings are pinned so the
/// machine's global `core.autocrlf` cannot change the fixture.
fn repo(files: &[(&str, &str)]) -> PathBuf {
    let dir = scratch("repo");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    git(&dir, &["config", "user.name", "t"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    git(&dir, &["config", "core.autocrlf", "false"]);
    for (rel, content) in files {
        std::fs::create_dir_all(dir.join(rel).parent().unwrap()).unwrap();
        write(&dir, rel, content);
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    dir
}

fn riltra(home: &Path, args: &[&str]) -> (i32, String, String) {
    riltra_env(home, args, &[])
}

fn riltra_env(home: &Path, args: &[&str], env: &[(&str, String)]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_riltra"));
    cmd.args(args).env("RILTRA_HOME", home);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn riltra");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Registers `dir` as repository `r` in a fresh home.
fn setup(dir: &Path) -> PathBuf {
    let home = scratch("home");
    let (code, _, err) = riltra(
        &home,
        &["init", "--repo", &dir.display().to_string(), "--name", "r"],
    );
    assert_eq!(code, 0, "init failed: {err}");
    home
}

/// Runs `changes --repo r` with `extra` and returns (exit, document, stderr).
fn changes(home: &Path, extra: &[&str]) -> (i32, Value, String) {
    let mut args = vec!["changes", "--repo", "r"];
    args.extend_from_slice(extra);
    let (code, out, err) = riltra(home, &args);
    let doc = if code == 0 {
        serde_json::from_str(out.trim()).expect("changes prints one JSON document")
    } else {
        Value::Null
    };
    (code, doc, err)
}

fn file<'a>(doc: &'a Value, path: &str) -> &'a Value {
    doc["files"]
        .as_array()
        .expect("files array")
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no file {path} in {doc}"))
}

fn def<'a>(f: &'a Value, name: &str) -> &'a Value {
    f["definitions"]
        .as_array()
        .expect("definitions array")
        .iter()
        .find(|d| d["name"] == name)
        .unwrap_or_else(|| panic!("no definition {name} in {f}"))
}

fn names(f: &Value) -> Vec<String> {
    f["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect()
}

/// One language's fixture. `alpha`, `beta`, `gamma` are 1-based (start, end)
/// lines of the three definitions in each version that needs them.
struct Lang {
    ext: &'static str,
    base: &'static str,
    edit_body: &'static str,
    edit_between: &'static str,
    appended: &'static str,
    deleted: &'static str,
    alpha: (u32, u32),
    beta: (u32, u32),
    gamma: (u32, u32),
}

const PYTHON: Lang = Lang {
    ext: "py",
    base: "def alpha():\n    return 1\n\n\ndef beta():\n    return 2\n",
    edit_body: "def alpha():\n    return 10\n\n\ndef beta():\n    return 2\n",
    edit_between: "def alpha():\n    return 1\n# between\n\ndef beta():\n    return 2\n",
    appended: "def alpha():\n    return 1\n\n\ndef beta():\n    return 2\n\n\ndef gamma():\n    return 3\n",
    deleted: "def alpha():\n    return 1\n",
    alpha: (1, 2),
    beta: (5, 6),
    gamma: (9, 10),
};

const RUST: Lang = Lang {
    ext: "rs",
    base: "fn alpha() -> i32 {\n    1\n}\n\nfn beta() -> i32 {\n    2\n}\n",
    edit_body: "fn alpha() -> i32 {\n    10\n}\n\nfn beta() -> i32 {\n    2\n}\n",
    edit_between: "fn alpha() -> i32 {\n    1\n}\n// between\nfn beta() -> i32 {\n    2\n}\n",
    appended: "fn alpha() -> i32 {\n    1\n}\n\nfn beta() -> i32 {\n    2\n}\n\nfn gamma() -> i32 {\n    3\n}\n",
    deleted: "fn alpha() -> i32 {\n    1\n}\n",
    alpha: (1, 3),
    beta: (5, 7),
    gamma: (9, 11),
};

const TYPESCRIPT: Lang = Lang {
    ext: "ts",
    base: "function alpha(): number {\n  return 1;\n}\n\nfunction beta(): number {\n  return 2;\n}\n",
    edit_body: "function alpha(): number {\n  return 10;\n}\n\nfunction beta(): number {\n  return 2;\n}\n",
    edit_between: "function alpha(): number {\n  return 1;\n}\n// between\nfunction beta(): number {\n  return 2;\n}\n",
    appended: "function alpha(): number {\n  return 1;\n}\n\nfunction beta(): number {\n  return 2;\n}\n\nfunction gamma(): number {\n  return 3;\n}\n",
    deleted: "function alpha(): number {\n  return 1;\n}\n",
    alpha: (1, 3),
    beta: (5, 7),
    gamma: (9, 11),
};

fn span(doc_span: &Value, start: u32, end: u32) {
    assert_eq!(doc_span, &json!({"start_line": start, "end_line": end}));
}

fn run_language_cases(lang: &Lang, tag: &str) {
    let name = format!("m.{}", lang.ext);

    // 1. Edit inside a function body: that definition only, header untouched.
    let dir = repo(&[(&name, lang.base)]);
    write(&dir, &name, lang.edit_body);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag} case 1: {err}");
    let f = file(&doc, &name);
    assert_eq!(f["status"], "modified", "{tag} case 1");
    assert_eq!(f["symbols"], "established", "{tag} case 1");
    assert_eq!(names(f), ["alpha"], "{tag} case 1 lists only alpha");
    let a = def(f, "alpha");
    assert_eq!(a["change"], "modified", "{tag} case 1");
    assert_eq!(a["header_touched"], false, "{tag} case 1");
    span(&a["old_span"], lang.alpha.0, lang.alpha.1);
    span(&a["new_span"], lang.alpha.0, lang.alpha.1);
    assert_eq!(f["hunks_outside_definitions"], json!([]), "{tag} case 1");

    // 2. Edit between two definitions: no definition, one outside hunk.
    let dir = repo(&[(&name, lang.base)]);
    write(&dir, &name, lang.edit_between);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag} case 2: {err}");
    let f = file(&doc, &name);
    assert_eq!(f["definitions"], json!([]), "{tag} case 2");
    assert_eq!(f["symbols"], "established", "{tag} case 2");
    assert_eq!(
        f["hunks_outside_definitions"].as_array().unwrap().len(),
        1,
        "{tag} case 2"
    );

    // 3. Added function: added, new span set, old span null.
    let dir = repo(&[(&name, lang.base)]);
    write(&dir, &name, lang.appended);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag} case 3: {err}");
    let f = file(&doc, &name);
    assert_eq!(names(f), ["gamma"], "{tag} case 3 lists only gamma");
    let g = def(f, "gamma");
    assert_eq!(g["change"], "added", "{tag} case 3");
    assert_eq!(g["old_span"], json!(null), "{tag} case 3");
    span(&g["new_span"], lang.gamma.0, lang.gamma.1);
    assert_eq!(g["callers_reason"], "added", "{tag} case 3");

    // 4. Deleted function: removed, old span set, new span null.
    let dir = repo(&[(&name, lang.base)]);
    write(&dir, &name, lang.deleted);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag} case 4: {err}");
    let f = file(&doc, &name);
    assert_eq!(names(f), ["beta"], "{tag} case 4 lists only beta");
    let b = def(f, "beta");
    assert_eq!(b["change"], "removed", "{tag} case 4");
    assert_eq!(b["header_touched"], true, "{tag} case 4");
    span(&b["old_span"], lang.beta.0, lang.beta.1);
    assert_eq!(b["new_span"], json!(null), "{tag} case 4");

    // 5. Rename with unchanged content: renamed, old path set, nothing listed.
    let renamed = format!("n.{}", lang.ext);
    let dir = repo(&[(&name, lang.base)]);
    git(&dir, &["mv", &name, &renamed]);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "all"]);
    assert_eq!(code, 0, "{tag} case 5: {err}");
    let f = file(&doc, &renamed);
    assert_eq!(f["change"], "renamed", "{tag} case 5");
    assert_eq!(f["status"], "renamed", "{tag} case 5");
    assert_eq!(f["old_path"], name.as_str(), "{tag} case 5");
}

#[test]
fn python_cases_one_to_five() {
    if !git_available() {
        return;
    }
    run_language_cases(&PYTHON, "python");
}

#[test]
fn rust_cases_one_to_five() {
    if !git_available() {
        return;
    }
    run_language_cases(&RUST, "rust");
}

#[test]
fn typescript_cases_one_to_five() {
    if !git_available() {
        return;
    }
    run_language_cases(&TYPESCRIPT, "typescript");
}

#[test]
fn untracked_file_is_one_added_hunk_and_absent_from_staged() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    write(&dir, "extra.py", "def delta():\n    return 4\n");
    let home = setup(&dir);
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "extra.py");
    assert_eq!(f["change"], "added");
    assert_eq!(f["hunks"], 1);
    assert_eq!(names(f), ["delta"]);
    let d = def(f, "delta");
    assert_eq!(d["change"], "added");
    assert_eq!(d["header_touched"], true);
    span(&d["new_span"], 1, 2);
    let (code, doc, err) = changes(&home, &["--scope", "staged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["total_files"], 0, "untracked files are never in staged");
}

#[test]
fn binary_file_is_not_established() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("blob.py", "x = 1\n")]);
    std::fs::write(dir.join("blob.py"), b"x = 1\n\0\0\n").unwrap();
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "blob.py");
    assert_eq!(f["status"], "binary");
    assert_eq!(f["reason"], "binary");
    assert_eq!(f["symbols"], "not-established");
    assert_eq!(f["definitions"], json!([]));
    assert_eq!(f["hunks_outside_definitions"], json!([]));
}

#[test]
fn crlf_file_with_one_edit_reports_one_based_lines() {
    if !git_available() {
        return;
    }
    let base = "def alpha():\r\n    return 1\r\n\r\n\r\ndef beta():\r\n    return 2\r\n";
    let dir = repo(&[("m.py", base)]);
    write(
        &dir,
        "m.py",
        "def alpha():\r\n    return 1\r\n\r\n\r\ndef beta():\r\n    return 20\r\n",
    );
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let b = def(file(&doc, "m.py"), "beta");
    assert_eq!(b["change"], "modified");
    assert_eq!(b["header_touched"], false);
    span(&b["old_span"], 5, 6);
    span(&b["new_span"], 5, 6);
}

#[test]
fn syntax_error_is_parse_error_with_hunks_outside() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    write(&dir, "m.py", "def alpha(:\n    return 1\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "m.py");
    assert_eq!(f["status"], "parse-error");
    assert_eq!(f["reason"], "syntax-error");
    assert_eq!(f["symbols"], "not-established");
    assert_eq!(f["definitions"], json!([]));
    assert_eq!(f["hunks_outside_definitions"].as_array().unwrap().len(), 1);
}

#[test]
fn each_scope_sees_only_its_own_sides() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    write(
        &dir,
        "m.py",
        "def alpha():\n    return 1\n\n\ndef beta():\n    return 2\n",
    );
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "add beta"]);
    let head = git_out(&dir, &["rev-parse", "HEAD"]);
    // Staged: beta body. Unstaged on top: alpha body.
    write(
        &dir,
        "m.py",
        "def alpha():\n    return 1\n\n\ndef beta():\n    return 22\n",
    );
    git(&dir, &["add", "m.py"]);
    write(
        &dir,
        "m.py",
        "def alpha():\n    return 11\n\n\ndef beta():\n    return 22\n",
    );
    let home = setup(&dir);

    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(names(file(&doc, "m.py")), ["alpha"]);
    assert_eq!(doc["base"], json!(null));
    assert_eq!(doc["head"], head.as_str());

    let (code, doc, err) = changes(&home, &["--scope", "staged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(names(file(&doc, "m.py")), ["beta"]);

    let (code, doc, err) = changes(&home, &["--scope", "all"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(names(file(&doc, "m.py")), ["alpha", "beta"]);

    let (code, doc, err) = changes(&home, &["--scope", "compare", "--base", "HEAD~1"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "m.py");
    assert_eq!(names(f), ["alpha", "beta"]);
    assert_eq!(def(f, "alpha")["change"], "modified");
    assert_eq!(def(f, "beta")["change"], "added");
    assert_eq!(doc["base"], "HEAD~1");
    assert_eq!(doc["scope"], "compare");
}

#[test]
fn invalid_arguments_and_non_repositories_fail_with_their_exit_codes() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    let home = setup(&dir);
    for extra in [
        vec!["--scope", "compare", "--base", "-x"],
        vec!["--scope", "compare", "--base=--output=x"],
        vec!["--scope", "compare", "--base", "no-such-ref"],
        vec!["--scope", "compare"],
        vec!["--scope", "all", "--base", "HEAD"],
        vec!["--limit", "0"],
        vec!["--limit", "1001"],
        vec!["--scope", "sideways"],
        vec!["--frobnicate", "1"],
    ] {
        let (code, _, err) = changes(&home, &extra);
        assert_eq!(code, 2, "{extra:?} must be a usage error: {err}");
    }
    let plain = scratch("plain");
    let plain_home = setup(&plain);
    let (code, _, err) = changes(&plain_home, &[]);
    assert_eq!(code, 3, "a directory that is not a git repository: {err}");
}

#[test]
fn repository_configured_drivers_never_run() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        (".gitattributes", "*.py filter=x\n*.py diff=x\n"),
        ("m.py", "def alpha():\n    return 1\n"),
    ]);
    let home = setup(&dir);
    let marker = |name: &str| home.join(format!("{name}-marker"));
    let sh = |script: &str, name: &str| format!("sh -c '{script}' \"{}\"", marker(name).display());
    git(
        &dir,
        &[
            "config",
            "filter.x.clean",
            &sh("touch \"$0\"; cat", "clean"),
        ],
    );
    git(
        &dir,
        &[
            "config",
            "diff.x.textconv",
            &sh("touch \"$0\"; cat \"$1\"", "textconv"),
        ],
    );
    git(
        &dir,
        &["config", "diff.external", &sh("touch \"$0\"", "external")],
    );
    let external = sh("touch \"$0\"", "external");
    // Staged edit: `git add` runs the clean filter during setup, so markers
    // are cleared after setup and before every measured run.
    write(&dir, "m.py", "def alpha():\n    return 2\n");
    git(&dir, &["add", "m.py"]);
    write(&dir, "m.py", "def alpha():\n    return 3\n");
    write(&dir, "u.py", "def delta():\n    return 4\n");
    // Setup ran git add/commit with the clean filter configured, which left markers
    // behind. Clear them and prove they are gone, then show that plain git (no
    // overrides) produces each one. Git uses an external diff instead of a textconv
    // driver, so the external diff is unset for the first diff run and set again for
    // the second. The fsmonitor hook is set only for the status run.
    for name in ["clean", "textconv", "external", "fsmonitor"] {
        let _ = std::fs::remove_file(marker(name));
    }
    for name in ["clean", "textconv", "external", "fsmonitor"] {
        assert!(
            !marker(name).exists(),
            "precondition: the {name} marker must be cleared before the control"
        );
    }
    git(&dir, &["config", "--unset", "diff.external"]);
    let _ = Command::new("git")
        .args(["diff", "--", "m.py"])
        .current_dir(&dir)
        .output();
    assert!(
        marker("clean").exists(),
        "control: plain git diff must run the clean filter"
    );
    assert!(
        marker("textconv").exists(),
        "control: plain git diff must run the textconv driver"
    );
    git(
        &dir,
        &["config", "diff.external", &sh("touch \"$0\"", "external")],
    );
    let _ = Command::new("git")
        .args(["diff", "--", "m.py"])
        .current_dir(&dir)
        .output();
    assert!(
        marker("external").exists(),
        "control: plain git diff must run the external diff"
    );
    git(
        &dir,
        &["config", "core.fsmonitor", &sh("touch \"$0\"", "fsmonitor")],
    );
    let _ = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&dir)
        .output();
    assert!(
        marker("fsmonitor").exists(),
        "control: plain git status must run the fsmonitor hook"
    );
    for name in ["clean", "textconv", "external", "fsmonitor"] {
        let _ = std::fs::remove_file(marker(name));
    }
    let scopes: [&[&str]; 4] = [
        &["--scope", "unstaged"],
        &["--scope", "staged"],
        &["--scope", "all"],
        &["--scope", "compare", "--base", "HEAD"],
    ];
    for extra in scopes {
        for name in ["clean", "textconv", "external", "fsmonitor"] {
            let _ = std::fs::remove_file(marker(name));
        }
        let mut args = vec!["changes", "--repo", "r"];
        args.extend_from_slice(extra);
        let (code, _, err) = riltra_env(&home, &args, &[("GIT_EXTERNAL_DIFF", external.clone())]);
        assert_eq!(code, 0, "{extra:?}: {err}");
        for name in ["clean", "textconv", "external", "fsmonitor"] {
            assert!(
                !marker(name).exists(),
                "{extra:?}: the {name} marker was created: repository-configured code ran"
            );
        }
    }
}

#[test]
fn built_in_crlf_conversion_is_not_a_change() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        (".gitattributes", "* text eol=crlf\n"),
        ("m.py", "def alpha():\n    return 1\n"),
    ]);
    write(&dir, "m.py", "def alpha():\r\n    return 1\r\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        doc["total_files"], 0,
        "git diff shows no change here: {doc}"
    );
}

#[test]
fn built_in_crlf_conversion_with_one_real_edit_is_modified() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        (".gitattributes", "* text eol=crlf\n"),
        ("m.py", "def alpha():\n    return 1\n"),
    ]);
    write(&dir, "m.py", "def alpha():\r\n    return 10\r\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let a = def(file(&doc, "m.py"), "alpha");
    assert_eq!(a["change"], "modified");
    span(&a["new_span"], 1, 2);
}

/// `callers` total for one name (the count the index holds).
fn callers_total(home: &Path, target: &str, in_path: &str) -> Value {
    let (code, doc, err) = riltra_pair(
        home,
        &[
            "callers", "--repo", "r", "--target", target, "--in", in_path,
        ],
    );
    assert_eq!(code, 0, "{err}");
    doc["completeness"]["total"].clone()
}

#[test]
fn caller_counts_are_read_from_the_index_not_the_working_tree() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        (
            "lib.py",
            "def save_all():\n    return 1\n\n\ndef gone():\n    return 3\n",
        ),
        (
            "a.py",
            "from lib import save_all, gone\n\n\ndef run_a():\n    save_all()\n    gone()\n",
        ),
    ]);
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{err}");
    let save_total = callers_total(&home, "save_all", "lib.py");
    let gone_total = callers_total(&home, "gone", "lib.py");
    assert_eq!(save_total, json!(1));
    assert_eq!(gone_total, json!(1));

    // Body edit of save_all, removal of gone, and a function added after indexing.
    write(&dir, "lib.py", "def save_all():\n    return 2\n");
    write(&dir, "c.py", "def fresh():\n    return 5\n");
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["index_freshness"]["status"], "stale");
    let lib = file(&doc, "lib.py");
    let saved = def(lib, "save_all");
    assert_eq!(saved["change"], "modified");
    assert_eq!(saved["callers"], json!({"inbound_edges": save_total}));
    let gone_def = def(lib, "gone");
    assert_eq!(gone_def["change"], "removed");
    assert_eq!(gone_def["callers"], json!({"inbound_edges": gone_total}));
    let fresh = def(file(&doc, "c.py"), "fresh");
    assert_eq!(fresh["callers"], json!(null));
    assert_eq!(fresh["callers_reason"], "added");
}

#[test]
fn a_deleted_file_still_gets_its_indexed_caller_count() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        ("lib.py", "def save_all():\n    return 1\n"),
        ("a.py", "def run_a():\n    return 1\n"),
        (
            "b.py",
            "from a import run_a\n\n\ndef run_b():\n    run_a()\n",
        ),
    ]);
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{err}");
    let run_total = callers_total(&home, "run_a", "a.py");
    assert_eq!(run_total, json!(1));
    std::fs::remove_file(dir.join("a.py")).unwrap();
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let a = file(&doc, "a.py");
    assert_eq!(a["status"], "deleted");
    let run = def(a, "run_a");
    assert_eq!(run["change"], "removed");
    assert_eq!(run["callers"], json!({"inbound_edges": run_total}));
    assert_eq!(doc["index_freshness"]["status"], "stale");
}

#[test]
fn two_indexed_definitions_with_one_name_are_ambiguous() {
    if !git_available() {
        return;
    }
    let dir = repo(&[(
        "d.py",
        "def dup():\n    return 1\n\n\ndef dup():\n    return 2\n",
    )]);
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{err}");
    write(
        &dir,
        "d.py",
        "def dup():\n    return 10\n\n\ndef dup():\n    return 2\n",
    );
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let first = def(file(&doc, "d.py"), "dup");
    assert_eq!(first["callers"], json!(null));
    assert_eq!(first["callers_reason"], "ambiguous-in-index");
}

fn riltra_pair(home: &Path, args: &[&str]) -> (i32, Value, String) {
    let (code, out, err) = riltra(home, args);
    let doc = if code == 0 {
        serde_json::from_str(out.trim()).expect("json")
    } else {
        Value::Null
    };
    (code, doc, err)
}

#[test]
fn no_index_answers_without_creating_a_database() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("lib.py", "def save_all():\n    return 1\n")]);
    let home = setup(&dir);
    let before = listing(&home);
    write(&dir, "lib.py", "def save_all():\n    return 2\n");
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        doc["index_freshness"],
        json!({"status": "no-index", "reasons": []})
    );
    let saved = def(file(&doc, "lib.py"), "save_all");
    assert_eq!(saved["callers"], json!(null));
    assert_eq!(saved["callers_reason"], "no-index");
    assert_eq!(
        listing(&home),
        before,
        "changes must not create files under the home"
    );
}

#[test]
fn unreadable_index_degrades_only_the_caller_counts() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("lib.py", "def save_all():\n    return 1\n")]);
    let home = setup(&dir);
    std::fs::create_dir_all(home.join("index")).unwrap();
    std::fs::write(home.join("index").join("r.db"), b"this is not sqlite").unwrap();
    write(&dir, "lib.py", "def save_all():\n    return 2\n");
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "the structural answer still succeeds: {err}");
    assert_eq!(
        doc["index_freshness"],
        json!({"status": "unavailable", "reasons": ["store-error"]})
    );
    let saved = def(file(&doc, "lib.py"), "save_all");
    assert_eq!(saved["change"], "modified");
    assert_eq!(saved["callers"], json!(null));
    assert_eq!(saved["callers_reason"], "index-unavailable");
}

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

#[test]
fn limit_caps_the_listed_files_and_reports_the_total() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        ("a.py", "def a():\n    return 1\n"),
        ("b.py", "def b():\n    return 1\n"),
        ("c.py", "def c():\n    return 1\n"),
    ]);
    for name in ["a.py", "b.py", "c.py"] {
        write(&dir, name, "def x():\n    return 2\n");
    }
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged", "--limit", "1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["files"].as_array().unwrap().len(), 1);
    assert_eq!(doc["files"][0]["path"], "a.py");
    assert_eq!(doc["total_files"], 3);
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["limits"], json!({"limit": 1}));
    assert_eq!(doc["disclaimer"], "structure-of-change-not-impact");
}

#[test]
fn a_root_below_the_git_top_level_is_refused_with_the_top_level_message() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("sub/m.py", "def alpha():\n    return 1\n")]);
    let home = setup(&dir.join("sub"));
    let (code, _, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 3, "{err}");
    assert!(
        err.contains("registered root is not the top level of a git work tree"),
        "{err}"
    );
}

#[test]
fn binary_is_judged_before_language_and_size_comes_first() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("keep.txt", "keep\n")]);
    std::fs::write(dir.join("blob.bin"), b"\x7fELF\0\0bin").unwrap();
    std::fs::write(dir.join("nul.py"), b"x = 1\0\n").unwrap();
    let mut big = vec![b'a'; 1024 * 1024 + 1];
    big[0] = 0;
    std::fs::write(dir.join("big.bin"), big).unwrap();
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(file(&doc, "blob.bin")["status"], "binary");
    assert_eq!(file(&doc, "blob.bin")["reason"], "binary");
    assert_eq!(file(&doc, "nul.py")["status"], "binary");
    assert_eq!(file(&doc, "nul.py")["reason"], "binary");
    assert_eq!(file(&doc, "big.bin")["status"], "unsupported");
    assert_eq!(file(&doc, "big.bin")["reason"], "too-large");
}

#[test]
fn a_committed_blob_over_the_cap_is_too_large_without_reading_it() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("big.bin", "small\n")]);
    std::fs::write(dir.join("big.bin"), vec![b'a'; 1024 * 1024 + 1]).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "big"]);
    write(&dir, "big.bin", "tiny\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "big.bin");
    assert_eq!(f["status"], "unsupported");
    assert_eq!(f["reason"], "too-large");
}

#[test]
fn a_submodule_pointer_is_unsupported_and_exits_zero() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("lib.py", "def alpha():\n    return 1\n")]);
    let first = "1111111111111111111111111111111111111111";
    let second = "2222222222222222222222222222222222222222";
    let spec_first = format!("160000,{first},sub");
    let spec_second = format!("160000,{second},sub");
    git(
        &dir,
        &["update-index", "--add", "--cacheinfo", spec_first.as_str()],
    );
    git(&dir, &["commit", "-q", "-m", "submodule"]);
    git(&dir, &["update-index", "--cacheinfo", spec_second.as_str()]);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "staged"]);
    assert_eq!(code, 0, "{err}");
    let sub = file(&doc, "sub");
    assert_eq!(sub["status"], "unsupported");
    assert_eq!(sub["reason"], "submodule");
    assert_eq!(sub["definitions"], json!([]));
    assert_eq!(sub["hunks"], 0);
}

#[test]
fn an_unmerged_path_is_unsupported_and_exits_zero() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    git(&dir, &["checkout", "-q", "-b", "feature"]);
    write(&dir, "m.py", "def alpha():\n    return 2\n");
    git(&dir, &["commit", "-q", "-am", "feature"]);
    git(&dir, &["checkout", "-q", "main"]);
    write(&dir, "m.py", "def alpha():\n    return 3\n");
    git(&dir, &["commit", "-q", "-am", "main"]);
    let merged = Command::new("git")
        .args(["merge", "feature"])
        .current_dir(&dir)
        .output()
        .expect("run git merge");
    assert!(!merged.status.success(), "the merge must conflict");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "m.py");
    assert_eq!(f["status"], "unsupported");
    assert_eq!(f["reason"], "unmerged");
    assert_eq!(f["change"], "modified");
}

#[test]
fn a_renamed_file_keeps_its_indexed_caller_count() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        ("lib.py", "def save_all():\n    return 1\n"),
        (
            "a.py",
            "from lib import save_all\n\n\ndef run_a():\n    save_all()\n",
        ),
    ]);
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{err}");
    let before = callers_total(&home, "save_all", "lib.py");
    assert_eq!(before, json!(1));
    git(&dir, &["mv", "lib.py", "lib2.py"]);
    write(&dir, "lib2.py", "def save_all():\n    return 2\n");
    let (code, doc, err) = changes(&home, &["--scope", "all"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "lib2.py");
    assert_eq!(f["change"], "renamed");
    assert_eq!(
        def(f, "save_all")["callers"],
        json!({"inbound_edges": before})
    );
}

#[test]
fn an_empty_untracked_file_has_no_hunks_and_a_nested_repository_is_unreadable() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    write(&dir, "empty.py", "");
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q", "."]);
    write(&nested, "inner.py", "x = 1\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let empty = file(&doc, "empty.py");
    assert_eq!(empty["hunks"], 0);
    assert_eq!(empty["hunks_outside_definitions"], json!([]));
    let nested_entry = file(&doc, "nested/");
    assert_eq!(nested_entry["status"], "unsupported");
    assert_eq!(nested_entry["reason"], "unreadable");
}

#[test]
fn core_autocrlf_true_makes_no_line_ending_change_and_a_real_edit_is_modified() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    git(&dir, &["config", "core.autocrlf", "true"]);
    write(&dir, "m.py", "def alpha():\r\n    return 1\r\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["total_files"], 0, "{doc}");
    write(&dir, "m.py", "def alpha():\r\n    return 10\r\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let a = def(file(&doc, "m.py"), "alpha");
    assert_eq!(a["change"], "modified");
    span(&a["new_span"], 1, 2);
}

#[test]
fn nested_definitions_are_each_listed_when_touched() {
    if !git_available() {
        return;
    }
    let dir = repo(&[(
        "m.py",
        "class Box:\n    def save(self):\n        return 1\n",
    )]);
    write(
        &dir,
        "m.py",
        "class Box:\n    def save(self):\n        return 2\n",
    );
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "m.py");
    assert_eq!(names(f), ["Box", "save"]);
    assert_eq!(def(f, "Box")["change"], "modified");
    assert_eq!(def(f, "save")["change"], "modified");
}

#[test]
fn two_callers_in_other_files_are_counted_from_the_index() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        ("lib.py", "def save_all():\n    return 1\n"),
        (
            "a.py",
            "from lib import save_all\n\n\ndef run_a():\n    save_all()\n",
        ),
        (
            "b.py",
            "from lib import save_all\n\n\ndef run_b():\n    save_all()\n",
        ),
    ]);
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{err}");
    let before = callers_total(&home, "save_all", "lib.py");
    assert_eq!(before, json!(2));
    write(&dir, "lib.py", "def save_all():\n    return 2\n");
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["index_freshness"]["status"], "stale");
    assert_eq!(
        def(file(&doc, "lib.py"), "save_all")["callers"],
        json!({"inbound_edges": before})
    );
}

/// A repository whose `m.py` is in a merge conflict (the merge is not committed).
fn conflicted_repo() -> PathBuf {
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    git(&dir, &["checkout", "-q", "-b", "feature"]);
    write(&dir, "m.py", "def alpha():\n    return 2\n");
    git(&dir, &["commit", "-q", "-am", "feature"]);
    git(&dir, &["checkout", "-q", "main"]);
    write(&dir, "m.py", "def alpha():\n    return 3\n");
    git(&dir, &["commit", "-q", "-am", "main"]);
    let merged = Command::new("git")
        .args(["merge", "feature"])
        .current_dir(&dir)
        .output()
        .expect("run git merge");
    assert!(!merged.status.success(), "the merge must conflict");
    dir
}

#[test]
fn a_conflicted_path_is_one_entry_in_every_scope() {
    if !git_available() {
        return;
    }
    let home = setup(&conflicted_repo());
    let scopes: [&[&str]; 4] = [
        &["--scope", "unstaged"],
        &["--scope", "staged"],
        &["--scope", "all"],
        &["--scope", "compare", "--base", "HEAD"],
    ];
    for extra in scopes {
        let (code, doc, err) = changes(&home, extra);
        assert_eq!(code, 0, "{extra:?}: {err}");
        let entries: Vec<&Value> = doc["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["path"] == "m.py")
            .collect();
        assert_eq!(entries.len(), 1, "{extra:?}: one entry per path, got {doc}");
        assert_eq!(doc["total_files"], 1, "{extra:?}: {doc}");
        assert_eq!(entries[0]["status"], "unsupported", "{extra:?}");
        assert_eq!(entries[0]["reason"], "unmerged", "{extra:?}");
    }
}

#[test]
fn glob_characters_in_names_are_literal_paths() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        ("m1.py", "def one():\n    return 1\n"),
        ("m[1].py", "def two():\n    return 1\n"),
    ]);
    write(&dir, "m1.py", "def one():\n    return 2\n");
    write(&dir, "m[1].py", "def two():\n    return 2\n");
    write(&dir, "u[1].py", "def three():\n    return 3\n");
    let home = setup(&dir);
    let scopes: [&[&str]; 2] = [&["--scope", "unstaged"], &["--scope", "all"]];
    for extra in scopes {
        let (code, doc, err) = changes(&home, extra);
        assert_eq!(code, 0, "{extra:?}: {err}");
        for (path, name) in [("m1.py", "one"), ("m[1].py", "two")] {
            let f = file(&doc, path);
            assert_eq!(f["hunks"], 1, "{extra:?} {path}: only its own hunk");
            assert_eq!(
                f["hunks_outside_definitions"],
                json!([]),
                "{extra:?} {path}"
            );
            assert_eq!(names(f), [name], "{extra:?} {path}");
        }
        assert_eq!(names(file(&doc, "u[1].py")), ["three"], "{extra:?}");
    }
}

#[test]
fn a_symlink_blob_is_unsupported_and_its_target_is_never_parsed() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    write(&dir, "link-target.txt", "lib.py");
    let sha = git_out(&dir, &["hash-object", "-w", "link-target.txt"]);
    let spec = format!("120000,{sha},m.py");
    git(&dir, &["update-index", "--cacheinfo", spec.as_str()]);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "staged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "m.py");
    assert_eq!(f["status"], "unsupported");
    assert_eq!(f["reason"], "symlink");
    assert_eq!(f["definitions"], json!([]));
    assert_eq!(f["hunks_outside_definitions"], json!([]));
}

#[test]
fn argument_errors_name_their_cause() {
    if !git_available() {
        return;
    }
    let home = setup(&repo(&[("m.py", "def alpha():\n    return 1\n")]));
    let cases: [(&[&str], &str); 4] = [
        (
            &["--scope", "compare", "--base", "-x"],
            "refusing a ref that starts with '-'",
        ),
        (&["--limit", "0"], "--limit must be between 1 and 1000"),
        (
            &["--scope", "compare"],
            "--scope compare needs --base <ref>",
        ),
        (
            &["--scope", "all", "--base", "HEAD"],
            "--base is only valid with --scope compare",
        ),
    ];
    for (extra, cause) in cases {
        let mut args = vec!["changes", "--repo", "r"];
        args.extend_from_slice(extra);
        let (code, _, err) = riltra(&home, &args);
        assert_eq!(code, 2, "{extra:?}: {err}");
        assert!(err.contains(cause), "{extra:?} must name its cause: {err}");
        assert!(
            !err.contains("Usage:"),
            "{extra:?} must not print the usage dump: {err}"
        );
    }
}

/// Number of `files` entries for one path.
fn entries(doc: &Value, path: &str) -> usize {
    doc["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["path"] == path)
        .count()
}

#[test]
fn index_freshness_runs_no_repository_hook_and_reports_a_real_status() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{err}");
    // Every repository-run hook is configured only after the index exists, and the
    // worktree then changes, so git has to inspect the file.
    let marker = |name: &str| home.join(format!("{name}-marker"));
    let sh = |script: &str, name: &str| format!("sh -c '{script}' \"{}\"", marker(name).display());
    write(&dir, ".gitattributes", "*.py filter=x\n*.py diff=x\n");
    git(
        &dir,
        &[
            "config",
            "filter.x.clean",
            &sh("touch \"$0\"; cat", "clean"),
        ],
    );
    git(
        &dir,
        &[
            "config",
            "diff.x.textconv",
            &sh("touch \"$0\"; cat \"$1\"", "textconv"),
        ],
    );
    git(
        &dir,
        &["config", "diff.external", &sh("touch \"$0\"", "external")],
    );
    git(
        &dir,
        &["config", "core.fsmonitor", &sh("touch \"$0\"", "fsmonitor")],
    );
    write(&dir, "m.py", "def alpha():\n    return 2\n");
    // Positive control: plain git status, with no overrides, must run the clean filter here.
    let _ = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&dir)
        .output();
    assert!(
        marker("clean").exists(),
        "control: plain git status must run the clean filter"
    );
    for name in ["clean", "textconv", "external", "fsmonitor"] {
        let _ = std::fs::remove_file(marker(name));
    }
    let scopes: [&[&str]; 4] = [
        &["--scope", "unstaged"],
        &["--scope", "staged"],
        &["--scope", "all"],
        &["--scope", "compare", "--base", "HEAD"],
    ];
    for extra in scopes {
        for name in ["clean", "textconv", "external", "fsmonitor"] {
            let _ = std::fs::remove_file(marker(name));
        }
        let (code, doc, err) = changes(&home, extra);
        assert_eq!(code, 0, "{extra:?}: {err}");
        for name in ["clean", "textconv", "external", "fsmonitor"] {
            assert!(
                !marker(name).exists(),
                "{extra:?}: the {name} marker was created: a repository hook ran during changes"
            );
        }
        assert_eq!(
            doc["index_freshness"]["status"], "stale",
            "{extra:?}: {doc}"
        );
    }
}

#[test]
fn a_path_deleted_from_the_index_and_recreated_is_one_entry_or_none() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        ("m.py", "def alpha():\n    return 1\n"),
        ("keep.py", "def keep():\n    return 1\n"),
    ]);
    git(&dir, &["rm", "-q", "--cached", "m.py"]);
    write(&dir, "m.py", "def alpha():\n    return 1\n");
    let home = setup(&dir);

    // Identical bytes: all and compare drop the path entirely.
    let (code, doc, err) = changes(&home, &["--scope", "all"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(entries(&doc, "m.py"), 0, "{doc}");
    assert_eq!(doc["total_files"], 0, "{doc}");
    let (code, doc, err) = changes(&home, &["--scope", "compare", "--base", "HEAD"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(entries(&doc, "m.py"), 0, "{doc}");
    assert_eq!(doc["total_files"], 0, "{doc}");
    // Staged sees the delete; unstaged sees the new file as added.
    let (code, doc, err) = changes(&home, &["--scope", "staged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(entries(&doc, "m.py"), 1, "{doc}");
    assert_eq!(file(&doc, "m.py")["change"], "deleted");
    assert_eq!(doc["total_files"], 1, "{doc}");
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(entries(&doc, "m.py"), 1, "{doc}");
    assert_eq!(file(&doc, "m.py")["change"], "added");
    assert_eq!(file(&doc, "m.py")["hunks"], 1);

    // One function edited, and keep.py edited too: m.py is one modified entry.
    write(&dir, "m.py", "def alpha():\n    return 2\n");
    write(&dir, "keep.py", "def keep():\n    return 2\n");
    for extra in [
        &["--scope", "all"][..],
        &["--scope", "compare", "--base", "HEAD"][..],
    ] {
        let (code, doc, err) = changes(&home, extra);
        assert_eq!(code, 0, "{extra:?}: {err}");
        assert_eq!(entries(&doc, "m.py"), 1, "{extra:?}: {doc}");
        assert_eq!(doc["total_files"], 2, "{extra:?}: {doc}");
        let f = file(&doc, "m.py");
        assert_eq!(f["change"], "modified", "{extra:?}");
        assert_eq!(f["status"], "modified", "{extra:?}");
        assert_eq!(f["hunks"], 1, "{extra:?}");
        assert_eq!(f["hunks_outside_definitions"], json!([]), "{extra:?}");
        assert_eq!(names(f), ["alpha"], "{extra:?}");
        let alpha = def(f, "alpha");
        assert_eq!(alpha["change"], "modified");
        assert_eq!(alpha["header_touched"], false);
        span(&alpha["old_span"], 1, 2);
        span(&alpha["new_span"], 1, 2);
    }
    let (code, doc, err) = changes(&home, &["--scope", "all", "--limit", "1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["total_files"], 2, "{doc}");
    assert_eq!(doc["truncated"], true, "{doc}");
    assert_eq!(doc["files"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(doc["files"][0]["path"], "keep.py", "{doc}");
    let (code, doc, err) = changes(&home, &["--scope", "staged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["total_files"], 1, "{doc}");
    assert_eq!(file(&doc, "m.py")["change"], "deleted");
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(doc["total_files"], 2, "{doc}");
    assert_eq!(entries(&doc, "m.py"), 1, "{doc}");
    assert_eq!(file(&doc, "m.py")["change"], "added");
}

#[test]
fn an_empty_git_directory_is_refused_and_never_answered_from_a_parent() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    let sub = dir.join("sub");
    std::fs::create_dir_all(sub.join(".git")).unwrap();
    write(&sub, "m.py", "def alpha():\n    return 2\n");
    let (code, _, err) = changes(&setup(&sub), &["--scope", "unstaged"]);
    assert_eq!(code, 3, "{err}");
    assert!(
        err.contains("registered root is not the top level of a git work tree"),
        "{err}"
    );
}

#[test]
fn a_gitlink_replaced_by_a_plain_file_is_refused_as_a_submodule_in_every_scope() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("lib.py", "def alpha():\n    return 1\n")]);
    let sha = "1111111111111111111111111111111111111111";
    let spec = format!("160000,{sha},sub");
    git(
        &dir,
        &["update-index", "--add", "--cacheinfo", spec.as_str()],
    );
    git(&dir, &["commit", "-q", "-m", "submodule"]);
    git(&dir, &["rm", "-q", "--cached", "sub"]);
    write(&dir, "sub", "def beta():\n    return 2\n");
    let home = setup(&dir);
    let scopes: [&[&str]; 2] = [
        &["--scope", "all"],
        &["--scope", "compare", "--base", "HEAD"],
    ];
    for extra in scopes {
        let (code, doc, err) = changes(&home, extra);
        assert_eq!(code, 0, "{extra:?}: {err}");
        assert_eq!(entries(&doc, "sub"), 1, "{extra:?}: {doc}");
        let f = file(&doc, "sub");
        assert_eq!(f["status"], "unsupported", "{extra:?}");
        assert_eq!(f["reason"], "submodule", "{extra:?}");
        assert_eq!(f["definitions"], json!([]), "{extra:?}");
        assert_eq!(f["hunks"], 0, "{extra:?}");
    }
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(entries(&doc, "sub"), 1, "{doc}");
    assert_eq!(file(&doc, "sub")["change"], "added");
}

/// The CRLF-only recreation of a re-added file, under given system/global settings.
fn crlf_only_recreation(dir: &Path) -> Value {
    let cfg = scratch("gitcfg").join("global.gitconfig");
    std::fs::write(&cfg, "[core]\n\tautocrlf = true\n").unwrap();
    let home = setup(dir);
    let args = ["changes", "--repo", "r", "--scope", "all"];
    let (code, out, err) = riltra_env(
        &home,
        &args,
        &[
            ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
            ("GIT_CONFIG_GLOBAL", cfg.display().to_string()),
        ],
    );
    assert_eq!(code, 0, "{err}");
    serde_json::from_str(out.trim()).expect("one JSON document")
}

#[test]
fn a_crlf_only_recreation_is_modified_whatever_system_and_global_config_say() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    git(&dir, &["rm", "-q", "--cached", "m.py"]);
    write(&dir, "m.py", "def alpha():\r\n    return 1\r\n");
    let doc = crlf_only_recreation(&dir);
    assert_eq!(entries(&doc, "m.py"), 1, "{doc}");
    let f = file(&doc, "m.py");
    assert_eq!(f["change"], "modified", "{doc}");
    assert_eq!(f["hunks"], 1, "{doc}");
    assert_eq!(names(f), ["alpha"], "{doc}");
    let alpha = def(f, "alpha");
    assert_eq!(alpha["change"], "modified");
    assert_eq!(alpha["header_touched"], true);
    span(&alpha["old_span"], 1, 2);
    span(&alpha["new_span"], 1, 2);
}

#[test]
fn a_logically_identical_crlf_recreation_under_autocrlf_true_is_no_entry() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    git(&dir, &["config", "core.autocrlf", "true"]);
    git(&dir, &["rm", "-q", "--cached", "m.py"]);
    write(&dir, "m.py", "def alpha():\r\n    return 1\r\n");
    let home = setup(&dir);
    for extra in [
        &["--scope", "all"][..],
        &["--scope", "compare", "--base", "HEAD"][..],
    ] {
        let (code, doc, err) = changes(&home, extra);
        assert_eq!(code, 0, "{extra:?}: {err}");
        assert_eq!(entries(&doc, "m.py"), 0, "{extra:?}: {doc}");
        assert_eq!(doc["total_files"], 0, "{extra:?}: {doc}");
    }
}

#[test]
fn a_really_edited_crlf_recreation_under_autocrlf_true_is_one_entry_with_its_hunk() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("m.py", "def alpha():\n    return 1\n")]);
    git(&dir, &["config", "core.autocrlf", "true"]);
    git(&dir, &["rm", "-q", "--cached", "m.py"]);
    write(&dir, "m.py", "def alpha():\r\n    return 2\r\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "all"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(entries(&doc, "m.py"), 1, "{doc}");
    let f = file(&doc, "m.py");
    assert_eq!(f["change"], "modified");
    assert_eq!(f["hunks"], 1);
    let alpha = def(f, "alpha");
    assert_eq!(alpha["change"], "modified");
    span(&alpha["new_span"], 1, 2);
}

#[test]
fn an_identical_file_above_the_size_cap_is_no_entry() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("keep.txt", "keep\n")]);
    let big = vec![b'a'; 1024 * 1024 + 1];
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "big"]);
    git(&dir, &["rm", "-q", "--cached", "big.bin"]);
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    let home = setup(&dir);
    for extra in [
        &["--scope", "all"][..],
        &["--scope", "compare", "--base", "HEAD"][..],
    ] {
        let (code, doc, err) = changes(&home, extra);
        assert_eq!(code, 0, "{extra:?}: {err}");
        assert_eq!(entries(&doc, "big.bin"), 0, "{extra:?}: {doc}");
        assert_eq!(doc["total_files"], 0, "{extra:?}: {doc}");
    }
}

#[test]
fn a_filter_managed_file_is_unsupported_and_no_driver_runs() {
    if !git_available() {
        return;
    }
    let dir = repo(&[
        (".gitattributes", "*.py filter=x\n"),
        ("m.py", "def alpha():\n    return 1\n"),
    ]);
    // The driver is configured after the commits, so the setup commits cannot run it.
    let markers = scratch("filter-markers");
    let marker = markers.join("clean-marker");
    let script = format!("sh -c 'touch \"$0\"; cat' \"{}\"", marker.display());
    git(&dir, &["config", "filter.x.clean", script.as_str()]);
    write(&dir, "m.py", "def alpha():\n    return 2\n");
    let _ = std::fs::remove_file(&marker);
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !marker.exists(),
        "the clean driver must not run during changes"
    );
    let f = file(&doc, "m.py");
    assert_eq!(f["status"], "unsupported", "{doc}");
    assert_eq!(f["reason"], "filter-managed", "{doc}");
    assert_eq!(f["definitions"], json!([]), "{doc}");
    assert_eq!(f["hunks"], 0, "{doc}");
    assert_eq!(f["hunks_outside_definitions"], json!([]), "{doc}");
}

/// Runs the binary with its output in files, so only riltra's own git pipes can
/// deadlock. A child still running after 180 s is killed and the test fails.
fn riltra_timed(home: &Path, args: &[&str]) -> (i32, String, String) {
    let dir = scratch("timed");
    let out_path = dir.join("stdout.txt");
    let err_path = dir.join("stderr.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_riltra"))
        .args(args)
        .env("RILTRA_HOME", home)
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .expect("spawn riltra");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        if let Some(status) = child.try_wait().expect("poll riltra") {
            let out = std::fs::read_to_string(&out_path).unwrap_or_default();
            let err = std::fs::read_to_string(&err_path).unwrap_or_default();
            return (status.code().unwrap_or(-1), out, err);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("riltra did not finish within 180 s: the child pipes deadlocked");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn three_thousand_untracked_paths_list_without_deadlock() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("keep.txt", "keep\n")]);
    for i in 0..3000 {
        write(
            &dir,
            &format!("u{i:04}-a-fairly-long-file-name-that-fills-the-pipe-buffer-0123456789.py"),
            "def f():\n    return 1\n",
        );
    }
    let home = setup(&dir);
    let (code, out, err) = riltra_timed(
        &home,
        &[
            "changes", "--repo", "r", "--scope", "unstaged", "--limit", "50",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("one JSON document");
    assert_eq!(doc["total_files"], 3000, "{doc}");
    assert_eq!(doc["files"].as_array().unwrap().len(), 50, "{doc}");
    assert_eq!(doc["truncated"], true, "{doc}");
}

#[test]
fn three_thousand_tracked_modified_paths_list_without_deadlock() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("keep.txt", "keep\n")]);
    for i in 0..3000 {
        write(
            &dir,
            &format!("t{i:04}-a-fairly-long-file-name-that-fills-the-pipe-buffer-0123456789.py"),
            "def f():\n    return 1\n",
        );
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "many"]);
    for i in 0..3000 {
        write(
            &dir,
            &format!("t{i:04}-a-fairly-long-file-name-that-fills-the-pipe-buffer-0123456789.py"),
            "def f():\n    return 2\n",
        );
    }
    let home = setup(&dir);
    let (code, out, err) = riltra_timed(
        &home,
        &[
            "changes", "--repo", "r", "--scope", "unstaged", "--limit", "50",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(out.trim()).expect("one JSON document");
    assert_eq!(doc["total_files"], 3000, "{doc}");
    assert_eq!(doc["files"].as_array().unwrap().len(), 50, "{doc}");
}

/// A directory link (junction on Windows, symlink on Unix). `false` when the link cannot be made.
#[cfg(windows)]
fn link_dir(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn link_dir(link: &Path, target: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[test]
fn an_identity_read_never_follows_a_link_out_of_the_root() {
    if !git_available() {
        return;
    }
    let identical = "def beta():\n    return 1\n";
    let mut answers = Vec::new();
    for outside_content in [identical, "def beta():\n    return 9\n"] {
        let dir = repo(&[("a/b.py", identical), ("keep.txt", "keep\n")]);
        git(&dir, &["rm", "-q", "--cached", "a/b.py"]);
        std::fs::rename(dir.join("a"), dir.join("a_old")).unwrap();
        let outside = scratch("outside");
        write(&outside, "b.py", outside_content);
        let link = dir.join("a");
        if !link_dir(&link, &outside) {
            eprintln!("skipped: this platform cannot create a directory link here");
            return;
        }
        let (code, doc, err) = changes(&setup(&dir), &["--scope", "all"]);
        assert_eq!(code, 0, "{err}");
        let entries: Vec<Value> = doc["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["path"] == "a/b.py")
            .cloned()
            .collect();
        answers.push(entries);
    }
    assert_eq!(
        answers[0], answers[1],
        "identical and different outside content must give the same answer"
    );
    assert_eq!(answers[0].len(), 1, "{:?}", answers[0]);
    assert_eq!(answers[0][0]["status"], "unsupported");
    assert_eq!(answers[0][0]["reason"], "unreadable");
}

#[test]
fn an_identical_file_above_the_identity_bound_is_listed_not_proven() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("keep.txt", "keep\n")]);
    let big = vec![b'a'; 64 * 1024 * 1024 + 1];
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "big"]);
    git(&dir, &["rm", "-q", "--cached", "big.bin"]);
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "all"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(entries(&doc, "big.bin"), 1, "{doc}");
    let f = file(&doc, "big.bin");
    assert_eq!(f["status"], "unsupported", "{doc}");
    assert_eq!(f["reason"], "too-large", "{doc}");
}

/// Entries of `path` in a changes document.
fn entries_at(doc: &Value, path: &str) -> Vec<Value> {
    doc["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["path"] == path)
        .cloned()
        .collect()
}

#[test]
fn the_reason_behind_a_link_does_not_depend_on_the_outside_size() {
    if !git_available() {
        return;
    }
    let mut answers = Vec::new();
    for outside_bytes in [10usize, 2 * 1024 * 1024] {
        let dir = repo(&[
            ("a/b.py", "def beta():\n    return 1\n"),
            ("keep.txt", "keep\n"),
        ]);
        git(&dir, &["rm", "-q", "--cached", "a/b.py"]);
        std::fs::rename(dir.join("a"), dir.join("a_old")).unwrap();
        let outside = scratch("outside-size");
        std::fs::write(outside.join("b.py"), vec![b'#'; outside_bytes]).unwrap();
        if !link_dir(&dir.join("a"), &outside) {
            eprintln!("skipped: this platform cannot create a directory link here");
            return;
        }
        let (code, doc, err) = changes(&setup(&dir), &["--scope", "all"]);
        assert_eq!(code, 0, "{err}");
        answers.push(entries_at(&doc, "a/b.py"));
    }
    assert_eq!(
        answers[0], answers[1],
        "a 10-byte and a 2 MiB outside file must give the same answer"
    );
    assert_eq!(answers[0].len(), 1, "{:?}", answers[0]);
    assert_eq!(answers[0][0]["status"], "unsupported");
    assert_eq!(answers[0][0]["reason"], "unreadable");
}

#[test]
fn a_tracked_path_behind_a_link_is_unreadable_whatever_the_outside_content() {
    if !git_available() {
        return;
    }
    let identical = "def beta():\n    return 1\n";
    for scope in [&["--scope", "unstaged"][..], &["--scope", "all"][..]] {
        let mut answers = Vec::new();
        for outside_content in [identical, "def beta():\n    return 9\n"] {
            let dir = repo(&[("a/b.py", identical), ("keep.txt", "keep\n")]);
            std::fs::rename(dir.join("a"), dir.join("a_old")).unwrap();
            let outside = scratch("outside-tracked");
            write(&outside, "b.py", outside_content);
            if !link_dir(&dir.join("a"), &outside) {
                eprintln!("skipped: this platform cannot create a directory link here");
                return;
            }
            let (code, doc, err) = changes(&setup(&dir), scope);
            assert_eq!(code, 0, "{scope:?}: {err}");
            answers.push(entries_at(&doc, "a/b.py"));
        }
        assert_eq!(
            answers[0], answers[1],
            "{scope:?}: identical and different outside content must give the same entry"
        );
        assert_eq!(answers[0].len(), 1, "{scope:?}: {:?}", answers[0]);
        assert_eq!(answers[0][0]["status"], "unsupported", "{scope:?}");
        assert_eq!(answers[0][0]["reason"], "unreadable", "{scope:?}");
        assert_eq!(
            answers[0][0]["hunks"], 0,
            "{scope:?}: no git diff may read through the link"
        );
    }
}

#[test]
fn a_differing_duplicate_above_the_scan_cap_has_no_hunks() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("keep.txt", "keep\n")]);
    let before = "before\n".repeat(400_000);
    std::fs::write(dir.join("big.txt"), &before).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "big"]);
    git(&dir, &["rm", "-q", "--cached", "big.txt"]);
    std::fs::write(dir.join("big.txt"), "after\n".repeat(400_000)).unwrap();
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "all"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "big.txt");
    assert_eq!(f["status"], "unsupported", "{doc}");
    assert_eq!(f["reason"], "too-large", "{doc}");
    assert_eq!(f["hunks"], 0, "{doc}");
    assert_eq!(f["hunks_outside_definitions"], json!([]), "{doc}");
}

// ---------------------------------------------------------------------------
// PR 5: JavaScript, C, C++, Go and Java. Every expected row is written by hand
// from the fixture source and the grammar's node ranges, never copied from tool
// output. A row reads `name kind change h=<header_touched> old>new`, spans as
// `start-end`, `-` for none. Rows are listed in the tool's order (start line of
// the side that exists, then name).

struct Shapes {
    tag: &'static str,
    ext: &'static str,
    /// Every shape of the language in one file.
    all: &'static str,
    /// Rows when the whole file is new / deleted.
    added: &'static [&'static str],
    removed: &'static [&'static str],
    /// Replace `.0` (one line, once) by `.1`: rows for a body edit.
    body: (&'static str, &'static str, &'static [&'static str]),
    /// The same on the first line of a definition.
    head: (&'static str, &'static str, &'static [&'static str]),
    /// Text appended to `all`: rows for the added definition.
    tail: (&'static str, &'static [&'static str]),
    /// Text cut from `all`: rows for the removed definitions.
    drop: (&'static str, &'static [&'static str]),
    /// Replace `.0` by `.1` to break the syntax.
    broken: (&'static str, &'static str),
}

const JS: Shapes = Shapes {
    tag: "javascript",
    ext: "js",
    all: "function alpha() {\n  return 1;\n}\n\nclass Box {\n  read() {\n    return 2;\n  }\n  onTick = () => {\n    return 3;\n  };\n}\n\nconst arrow = () => {\n  return 4;\n};\n\nconst expr = function () {\n  return 5;\n};\n",
    added: &[
        "alpha function added h=true ->1-3",
        "Box class added h=true ->5-12",
        "read method added h=true ->6-8",
        "onTick method added h=true ->9-11",
        "arrow function added h=true ->14-16",
        "expr function added h=true ->18-20",
    ],
    removed: &[
        "alpha function removed h=true 1-3>-",
        "Box class removed h=true 5-12>-",
        "read method removed h=true 6-8>-",
        "onTick method removed h=true 9-11>-",
        "arrow function removed h=true 14-16>-",
        "expr function removed h=true 18-20>-",
    ],
    body: (
        "return 2;",
        "return 20;",
        &[
            "Box class modified h=false 5-12>5-12",
            "read method modified h=false 6-8>6-8",
        ],
    ),
    head: (
        "function alpha() {",
        "function alpha(a) {",
        &["alpha function modified h=true 1-3>1-3"],
    ),
    tail: (
        "\nfunction gamma() {\n  return 6;\n}\n",
        &["gamma function added h=true ->22-24"],
    ),
    drop: (
        "\nconst expr = function () {\n  return 5;\n};\n",
        &["expr function removed h=true 18-20>-"],
    ),
    broken: ("function alpha() {", "function alpha( {"),
};

const GO: Shapes = Shapes {
    tag: "go",
    ext: "go",
    all: "package m\n\ntype Widget struct {\n\tn int\n}\n\ntype Runner interface {\n\tRun() int\n}\n\nfunc alpha() int {\n\treturn 1\n}\n\nfunc (w *Widget) Run() int {\n\treturn w.n\n}\n",
    added: &[
        "Widget struct added h=true ->3-5",
        "Runner interface added h=true ->7-9",
        "alpha function added h=true ->11-13",
        "Run method added h=true ->15-17",
    ],
    removed: &[
        "Widget struct removed h=true 3-5>-",
        "Runner interface removed h=true 7-9>-",
        "alpha function removed h=true 11-13>-",
        "Run method removed h=true 15-17>-",
    ],
    body: (
        "return 1",
        "return 10",
        &["alpha function modified h=false 11-13>11-13"],
    ),
    head: (
        "func alpha() int {",
        "func alpha() int64 {",
        &["alpha function modified h=true 11-13>11-13"],
    ),
    tail: (
        "\nfunc gamma() int {\n\treturn 3\n}\n",
        &["gamma function added h=true ->19-21"],
    ),
    drop: (
        "\nfunc (w *Widget) Run() int {\n\treturn w.n\n}\n",
        &["Run method removed h=true 15-17>-"],
    ),
    broken: ("func alpha() int {", "func alpha( int {"),
};

const JAVA: Shapes = Shapes {
    tag: "java",
    ext: "java",
    all: "interface Shape {\n    int area();\n}\n\nenum Color {\n    RED, GREEN\n}\n\nrecord Point(int x, int y) {\n    Point {\n        if (x < 0) {\n            throw new IllegalArgumentException();\n        }\n    }\n}\n\nclass Box {\n    Box() {\n    }\n\n    @Override\n    public String toString() {\n        return \"box\";\n    }\n}\n",
    added: &[
        "Shape interface added h=true ->1-3",
        "area method added h=true ->2-2",
        "Color type added h=true ->5-7",
        "Point class added h=true ->9-15",
        "Point method added h=true ->10-14",
        "Box class added h=true ->17-25",
        "Box method added h=true ->18-19",
        "toString method added h=true ->21-24",
    ],
    removed: &[
        "Shape interface removed h=true 1-3>-",
        "area method removed h=true 2-2>-",
        "Color type removed h=true 5-7>-",
        "Point class removed h=true 9-15>-",
        "Point method removed h=true 10-14>-",
        "Box class removed h=true 17-25>-",
        "Box method removed h=true 18-19>-",
        "toString method removed h=true 21-24>-",
    ],
    body: (
        "return \"box\";",
        "return \"boxed\";",
        &[
            "Box class modified h=false 17-25>17-25",
            "toString method modified h=false 21-24>21-24",
        ],
    ),
    // The annotation is part of the method declaration, so it is the first line.
    head: (
        "@Override",
        "@Deprecated",
        &[
            "Box class modified h=false 17-25>17-25",
            "toString method modified h=true 21-24>21-24",
        ],
    ),
    tail: (
        "\nclass Gamma {\n}\n",
        &["Gamma class added h=true ->27-28"],
    ),
    drop: (
        "\nclass Box {\n    Box() {\n    }\n\n    @Override\n    public String toString() {\n        return \"box\";\n    }\n}\n",
        &[
            "Box class removed h=true 17-25>-",
            "Box method removed h=true 18-19>-",
            "toString method removed h=true 21-24>-",
        ],
    ),
    broken: ("int area();", "int area(;"),
};

const C: Shapes = Shapes {
    tag: "c",
    ext: "c",
    all: "struct point {\n    int x;\n};\n\nenum color {\n    RED,\n    GREEN\n};\n\ntypedef unsigned int uint;\n\nint alpha(void) {\n    return 1;\n}\n",
    added: &[
        "point struct added h=true ->1-3",
        "color type added h=true ->5-8",
        "uint type added h=true ->10-10",
        "alpha function added h=true ->12-14",
    ],
    removed: &[
        "point struct removed h=true 1-3>-",
        "color type removed h=true 5-8>-",
        "uint type removed h=true 10-10>-",
        "alpha function removed h=true 12-14>-",
    ],
    body: (
        "return 1;",
        "return 10;",
        &["alpha function modified h=false 12-14>12-14"],
    ),
    head: (
        "int alpha(void) {",
        "long alpha(void) {",
        &["alpha function modified h=true 12-14>12-14"],
    ),
    tail: (
        "\nint gamma(void) {\n    return 3;\n}\n",
        &["gamma function added h=true ->16-18"],
    ),
    drop: (
        "\nint alpha(void) {\n    return 1;\n}\n",
        &["alpha function removed h=true 12-14>-"],
    ),
    broken: ("int alpha(void) {", "int alpha(void {"),
};

// The namespace wrapper has no entry of its own; its members do. The field's
// span is its identifier, a method defined in the class body is a `function`,
// and the out-of-line definition keeps its qualified name.
const CPP: Shapes = Shapes {
    tag: "cpp",
    ext: "cpp",
    all: "namespace app {\n\nusing Id = int;\n\nclass Widget {\npublic:\n    int count;\n    int run() { return count; }\n    void reset();\n};\n\nvoid Widget::reset() {\n    count = 0;\n}\n\nint helper(int a) {\n    return a;\n}\n\n}\n",
    added: &[
        "Id type added h=true ->3-3",
        "Widget class added h=true ->5-10",
        "count field added h=true ->7-7",
        "run function added h=true ->8-8",
        "Widget::reset function added h=true ->12-14",
        "helper function added h=true ->16-18",
    ],
    removed: &[
        "Id type removed h=true 3-3>-",
        "Widget class removed h=true 5-10>-",
        "count field removed h=true 7-7>-",
        "run function removed h=true 8-8>-",
        "Widget::reset function removed h=true 12-14>-",
        "helper function removed h=true 16-18>-",
    ],
    body: (
        "count = 0;",
        "count = 1;",
        &["Widget::reset function modified h=false 12-14>12-14"],
    ),
    head: (
        "int helper(int a) {",
        "int helper(int b) {",
        &["helper function modified h=true 16-18>16-18"],
    ),
    tail: (
        "\nint gamma() {\n    return 3;\n}\n",
        &["gamma function added h=true ->22-24"],
    ),
    drop: (
        "\nint helper(int a) {\n    return a;\n}\n",
        &["helper function removed h=true 16-18>-"],
    ),
    broken: ("int helper(int a) {", "int helper(int a {"),
};

fn rows(f: &Value) -> Vec<String> {
    let lines = |v: &Value| {
        if v.is_null() {
            "-".to_string()
        } else {
            format!("{}-{}", v["start_line"], v["end_line"])
        }
    };
    f["definitions"]
        .as_array()
        .expect("definitions array")
        .iter()
        .map(|d| {
            format!(
                "{} {} {} h={} {}>{}",
                d["name"].as_str().unwrap(),
                d["kind"].as_str().unwrap(),
                d["change"].as_str().unwrap(),
                d["header_touched"],
                lines(&d["old_span"]),
                lines(&d["new_span"])
            )
        })
        .collect()
}

fn owned(want: &[&str]) -> Vec<String> {
    want.iter().map(|s| s.to_string()).collect()
}

/// `all` with the one occurrence of `from` replaced by `to`.
fn edit(s: &Shapes, from: &str, to: &str) -> String {
    assert_eq!(
        s.all.matches(from).count(),
        1,
        "{}: {from:?} must occur once in the fixture",
        s.tag
    );
    s.all.replacen(from, to, 1)
}

/// Runs `changes` over `dir`, checks the definition rows of `name` and returns its entry.
fn check(tag: &str, case: &str, dir: &Path, name: &str, scope: &str, want: &[&str]) -> Value {
    let (code, doc, err) = changes(&setup(dir), &["--scope", scope]);
    assert_eq!(code, 0, "{tag} {case}: {err}");
    let f = file(&doc, name).clone();
    assert_eq!(f["symbols"], "established", "{tag} {case}");
    assert_eq!(rows(&f), owned(want), "{tag} {case}");
    f
}

fn matrix(s: &Shapes) {
    let tag = s.tag;
    let name = format!("m.{}", s.ext);
    let body = edit(s, s.body.0, s.body.1);

    // Added: a new file, then a definition appended to an existing file.
    let dir = repo(&[("keep.txt", "x\n")]);
    write(&dir, &name, s.all);
    let f = check(tag, "new file", &dir, &name, "unstaged", s.added);
    assert_eq!(f["status"], "added", "{tag} new file");
    assert!(
        f["definitions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["callers"].is_null() && d["callers_reason"] == "added"),
        "{tag}: an added definition has no caller lookup"
    );
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &format!("{}{}", s.all, s.tail.0));
    check(
        tag,
        "appended definition",
        &dir,
        &name,
        "unstaged",
        s.tail.1,
    );

    // Removed: a deleted file, then definitions cut from an existing file.
    let dir = repo(&[(&name, s.all)]);
    std::fs::remove_file(dir.join(&name)).unwrap();
    let f = check(tag, "deleted file", &dir, &name, "unstaged", s.removed);
    assert_eq!(f["status"], "deleted", "{tag} deleted file");
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &edit(s, s.drop.0, ""));
    check(tag, "cut definitions", &dir, &name, "unstaged", s.drop.1);

    // Modified: a body edit, and an edit on the first line of a definition.
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &body);
    let f = check(tag, "body edit", &dir, &name, "unstaged", s.body.2);
    assert_eq!(f["hunks_outside_definitions"], json!([]), "{tag} body edit");
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &edit(s, s.head.0, s.head.1));
    check(tag, "first-line edit", &dir, &name, "unstaged", s.head.2);

    // A path below a directory.
    let nested = format!("src/deep/m.{}", s.ext);
    let dir = repo(&[(&nested, s.all)]);
    write(&dir, &nested, &body);
    check(tag, "nested path", &dir, &nested, "unstaged", s.body.2);

    // Rename with a change: the old path is reported, definitions match across the move.
    let moved = format!("n.{}", s.ext);
    let dir = repo(&[(&name, s.all)]);
    git(&dir, &["mv", &name, &moved]);
    write(&dir, &moved, &body);
    let f = check(tag, "rename with change", &dir, &moved, "all", s.body.2);
    assert_eq!(f["change"], "renamed", "{tag} rename");
    assert_eq!(f["old_path"], name.as_str(), "{tag} rename");

    // CRLF line endings report the same rows.
    let crlf = s.all.replace('\n', "\r\n");
    let dir = repo(&[(&name, crlf.as_str())]);
    write(&dir, &name, &crlf.replacen(s.body.0, s.body.1, 1));
    check(tag, "CRLF body edit", &dir, &name, "unstaged", s.body.2);

    // Staged scope reads the staged blob, not the working tree.
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &body);
    git(&dir, &["add", &name]);
    write(&dir, &name, &edit(s, s.head.0, s.head.1));
    check(tag, "staged blob", &dir, &name, "staged", s.body.2);

    // A file that cannot be parsed has a reason and no symbols; the hunk is outside.
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &edit(s, s.broken.0, s.broken.1));
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag} parse error: {err}");
    let f = file(&doc, &name);
    assert_eq!(f["status"], "parse-error", "{tag}");
    assert_eq!(f["reason"], "syntax-error", "{tag}");
    assert_eq!(f["symbols"], "not-established", "{tag}");
    assert_eq!(f["definitions"], json!([]), "{tag}");
    assert_eq!(
        f["hunks_outside_definitions"].as_array().unwrap().len(),
        1,
        "{tag}"
    );

    // An unsupported neighbour stays unsupported and changes nothing for the listed language.
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &body);
    write(&dir, "n.jsx", "export const A = () => <div />;\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag} neighbour: {err}");
    let n = file(&doc, "n.jsx");
    assert_eq!(n["status"], "unsupported", "{tag}");
    assert_eq!(n["reason"], "language-unsupported", "{tag}");
    assert_eq!(rows(file(&doc, &name)), owned(s.body.2), "{tag} neighbour");
}

#[test]
fn javascript_change_operations() {
    if git_available() {
        matrix(&JS);
    }
}

#[test]
fn go_change_operations() {
    if git_available() {
        matrix(&GO);
    }
}

#[test]
fn java_change_operations() {
    if git_available() {
        matrix(&JAVA);
    }
}

#[test]
fn c_change_operations() {
    if git_available() {
        matrix(&C);
    }
}

#[test]
fn cpp_change_operations() {
    if git_available() {
        matrix(&CPP);
    }
}

/// One edit of `all` in `m.<ext>`, unstaged, with the rows and the outside-hunk count it must give.
fn boundary(s: &Shapes, case: &str, from: &str, to: &str, want: &[&str], outside: usize) {
    let name = format!("m.{}", s.ext);
    let dir = repo(&[(&name, s.all)]);
    write(&dir, &name, &edit(s, from, to));
    let f = check(s.tag, case, &dir, &name, "unstaged", want);
    assert_eq!(
        f["hunks_outside_definitions"].as_array().unwrap().len(),
        outside,
        "{} {case}",
        s.tag
    );
}

#[test]
fn span_boundaries_follow_the_grammar() {
    if !git_available() {
        return;
    }
    // Java: the signature line sits below the annotation, so it is not the first line.
    boundary(
        &JAVA,
        "signature below an annotation",
        "public String toString() {",
        "public String toString(int n) {",
        &[
            "Box class modified h=false 17-25>17-25",
            "toString method modified h=false 21-24>21-24",
        ],
        0,
    );
    // Java: the line before the annotation is inside the class, outside the method.
    boundary(
        &JAVA,
        "line before an annotation",
        "    }\n\n    @Override",
        "    }\n    // note\n    @Override",
        &["Box class modified h=false 17-25>17-25"],
        0,
    );
    // Go: a doc comment is outside the function span; the added line shifts nothing it covers.
    boundary(
        &GO,
        "doc comment above a function",
        "func alpha() int {",
        "// alpha returns one.\nfunc alpha() int {",
        &[],
        1,
    );
}

#[test]
fn cpp_template_line_is_outside_the_function_span() {
    if !git_available() {
        return;
    }
    let base = "template <typename T>\nT twice(T v) {\n    return v;\n}\n";
    // The template line is not part of the stored function span (2-4).
    let dir = repo(&[("t.cpp", base)]);
    write(&dir, "t.cpp", &base.replacen("typename", "class", 1));
    let f = check("cpp", "template line", &dir, "t.cpp", "unstaged", &[]);
    assert_eq!(f["hunks_outside_definitions"].as_array().unwrap().len(), 1);
    // The signature line is the first line of the stored span.
    let dir = repo(&[("t.cpp", base)]);
    write(
        &dir,
        "t.cpp",
        &base.replacen("T twice(T v)", "T twice(T w)", 1),
    );
    check(
        "cpp",
        "template signature",
        &dir,
        "t.cpp",
        "unstaged",
        &["twice function modified h=true 2-4>2-4"],
    );
}

#[test]
fn cpp_field_span_is_its_identifier() {
    if !git_available() {
        return;
    }
    // One line: changing the type edits the line of the field's identifier.
    let one = "class W {\n  int n;\n};\n";
    let dir = repo(&[("w.h", one)]);
    write(&dir, "w.h", &one.replace("int n", "long n"));
    check(
        "cpp",
        "one-line field in a header",
        &dir,
        "w.h",
        "unstaged",
        &[
            "W class modified h=false 1-3>1-3",
            "n field modified h=true 2-2>2-2",
        ],
    );
    // Two lines: `int` is outside the identifier span, so the field is not touched; the class is.
    let two = "class W {\n  int\n  n;\n};\n";
    let dir = repo(&[("w.hpp", two)]);
    write(&dir, "w.hpp", &two.replace("int", "long"));
    let f = check(
        "cpp",
        "two-line field in a class",
        &dir,
        "w.hpp",
        "unstaged",
        &["W class modified h=false 1-4>1-4"],
    );
    assert_eq!(f["hunks_outside_definitions"], json!([]));
    // Two lines at file scope: no stored definition holds the hunk.
    let scope = "int\nvalue;\n";
    let dir = repo(&[("v.cpp", scope)]);
    write(&dir, "v.cpp", &scope.replace("int", "long"));
    let f = check(
        "cpp",
        "two-line declaration at file scope",
        &dir,
        "v.cpp",
        "unstaged",
        &[],
    );
    assert_eq!(f["hunks_outside_definitions"].as_array().unwrap().len(), 1);
}

#[test]
fn a_java_field_is_not_a_definition() {
    if !git_available() {
        return;
    }
    // The parser stores no Java field: editing one touches only the enclosing class.
    let base = "class F {\n    int count;\n\n    int get() {\n        return count;\n    }\n}\n";
    let dir = repo(&[("F.java", base)]);
    write(&dir, "F.java", &base.replace("int count;", "long count;"));
    let f = check(
        "java",
        "field edit",
        &dir,
        "F.java",
        "unstaged",
        &["F class modified h=false 1-7>1-7"],
    );
    assert_eq!(f["hunks_outside_definitions"], json!([]));
}

#[test]
fn a_header_that_does_not_parse_as_cpp_is_a_parse_error() {
    if !git_available() {
        return;
    }
    let dir = repo(&[("api.h", "int f(void);\n")]);
    write(&dir, "api.h", "int f(void;\n");
    let (code, doc, err) = changes(&setup(&dir), &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, "api.h");
    assert_eq!(f["status"], "parse-error");
    assert_eq!(f["reason"], "syntax-error");
    assert_eq!(f["language"], "cpp");
    assert_eq!(f["definitions"], json!([]));
}

/// One file with `target` and a caller of it in the same file: the index links a caller to the
/// definition through the definition's own file (and, for C and C++, through includes).
struct Linked {
    tag: &'static str,
    lib_name: &'static str,
    lib: &'static str,
    from: &'static str,
    to: &'static str,
    fresh: &'static str,
    fresh_name: &'static str,
}

fn linked_cases(l: &Linked) {
    let tag = l.tag;
    let dir = repo(&[(l.lib_name, l.lib)]);
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{tag}: {err}");
    let total = callers_total(&home, "target", l.lib_name);
    assert!(
        total.as_u64().is_some_and(|n| n >= 1),
        "{tag}: the fixture caller must be indexed, got {total}"
    );

    // Modified: the index count. Added after indexing: no lookup, so no count.
    write(
        &dir,
        l.lib_name,
        &format!("{}{}", l.lib.replacen(l.from, l.to, 1), l.fresh),
    );
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag}: {err}");
    let lib = file(&doc, l.lib_name);
    let target = def(lib, "target");
    assert_eq!(target["change"], "modified", "{tag}");
    assert_eq!(target["callers"], json!({"inbound_edges": total}), "{tag}");
    let fresh = def(lib, l.fresh_name);
    assert_eq!(fresh["change"], "added", "{tag}");
    assert_eq!(fresh["callers"], json!(null), "{tag}");
    assert_eq!(fresh["callers_reason"], "added", "{tag}");

    // Removed: the same count.
    write(&dir, l.lib_name, "");
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{tag}: {err}");
    let target = def(file(&doc, l.lib_name), "target");
    assert_eq!(target["change"], "removed", "{tag}");
    assert_eq!(target["callers"], json!({"inbound_edges": total}), "{tag}");
}

#[test]
fn indexed_caller_counts_reach_every_new_language() {
    if !git_available() {
        return;
    }
    linked_cases(&Linked {
        tag: "javascript",
        lib_name: "lib.js",
        lib: "export function target() {\n  return 1;\n}\n\nexport function user() {\n  return target();\n}\n",
        from: "return 1;",
        to: "return 2;",
        fresh: "\nexport function fresh() {\n  return 3;\n}\n",
        fresh_name: "fresh",
    });
    linked_cases(&Linked {
        tag: "go",
        lib_name: "lib.go",
        lib: "package m\n\nfunc target() int {\n\treturn 1\n}\n\nfunc user() int {\n\treturn target()\n}\n",
        from: "return 1",
        to: "return 2",
        fresh: "\nfunc fresh() int {\n\treturn 3\n}\n",
        fresh_name: "fresh",
    });
    linked_cases(&Linked {
        tag: "java",
        lib_name: "Lib.java",
        lib: "class Lib {\n    static int target() {\n        return 1;\n    }\n\n    int user() {\n        return target();\n    }\n}\n",
        from: "return 1;",
        to: "return 2;",
        fresh: "\nclass Fresh {\n}\n",
        fresh_name: "Fresh",
    });
    linked_cases(&Linked {
        tag: "c",
        lib_name: "lib.c",
        lib: "int target(void) {\n    return 1;\n}\n\nint user(void) {\n    return target();\n}\n",
        from: "return 1;",
        to: "return 2;",
        fresh: "\nint fresh(void) {\n    return 3;\n}\n",
        fresh_name: "fresh",
    });
    linked_cases(&Linked {
        tag: "cpp",
        lib_name: "lib.cpp",
        lib: "int target() {\n    return 1;\n}\n\nint user() {\n    return target();\n}\n",
        from: "return 1;",
        to: "return 2;",
        fresh: "\nint fresh() {\n    return 3;\n}\n",
        fresh_name: "fresh",
    });
}

#[test]
fn overloads_in_one_file_stay_ambiguous_in_the_index() {
    if !git_available() {
        return;
    }
    for (name, base, from, to) in [
        (
            "d.cpp",
            "int dup(int a) {\n    return a;\n}\n\nint dup(long a) {\n    return 1;\n}\n",
            "return a;",
            "return a + 1;",
        ),
        (
            "D.java",
            "class D {\n    int dup(int a) {\n        return a;\n    }\n\n    int dup(long a) {\n        return 1;\n    }\n}\n",
            "return a;",
            "return a + 1;",
        ),
    ] {
        let dir = repo(&[(name, base)]);
        let home = setup(&dir);
        let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
        assert_eq!(code, 0, "{name}: {err}");
        write(&dir, name, &base.replacen(from, to, 1));
        let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
        assert_eq!(code, 0, "{name}: {err}");
        let first = def(file(&doc, name), "dup");
        assert_eq!(first["callers"], json!(null), "{name}");
        assert_eq!(first["callers_reason"], "ambiguous-in-index", "{name}");
    }
}

/// A caller of the free function, in the same file as the class and the function.
const FIELD_USER: &str = "\nint user() {\n    return target();\n}\n";

/// A C++ class whose field shares its name with a free function that has a caller.
fn field_repo() -> PathBuf {
    repo(&[(
        "lib.cpp",
        &format!(
            "class Widget {{\npublic:\n    int target;\n}};\n\nint target() {{\n    return 1;\n}}\n{FIELD_USER}"
        ),
    )])
}

#[test]
fn a_cpp_field_never_gets_a_call_count_even_beside_a_function_of_its_name() {
    if !git_available() {
        return;
    }
    let dir = field_repo();
    let home = setup(&dir);
    let (code, _, err) = riltra(&home, &["index", "--repo", "r"]);
    assert_eq!(code, 0, "{err}");
    let lib = "lib.cpp";

    // Modified field and modified function of one name: the function keeps its count.
    let edited = format!(
        "class Widget {{\npublic:\n    long target;\n}};\n\nint target() {{\n    return 2;\n}}\n{FIELD_USER}"
    );
    write(&dir, lib, &edited);
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let f = file(&doc, lib);
    assert_eq!(
        rows(f),
        owned(&[
            "Widget class modified h=false 1-4>1-4",
            "target field modified h=true 3-3>3-3",
            "target function modified h=false 6-8>6-8",
        ])
    );
    let field = &f["definitions"][1];
    assert_eq!(field["callers"], json!(null));
    assert_eq!(field["callers_reason"], "data-member");
    let function = &f["definitions"][2];
    // One caller (`user`) in the fixture.
    assert_eq!(
        function["callers"],
        json!({"inbound_edges": 1}),
        "the function keeps the index count: {function}"
    );

    // Removed field: the same reason.
    write(
        &dir,
        lib,
        &format!(
            "class Widget {{\npublic:\n}};\n\nint target() {{\n    return 1;\n}}\n{FIELD_USER}"
        ),
    );
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let field = def(file(&doc, lib), "target");
    assert_eq!(field["kind"], "field");
    assert_eq!(field["change"], "removed");
    assert_eq!(field["callers"], json!(null));
    assert_eq!(field["callers_reason"], "data-member");

    // Added field: no lookup is made, so the existing reason stands.
    write(
        &dir,
        lib,
        &format!(
            "class Widget {{\npublic:\n    int target;\n    int extra;\n}};\n\nint target() {{\n    return 1;\n}}\n{FIELD_USER}"
        ),
    );
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let extra = def(file(&doc, lib), "extra");
    assert_eq!(extra["kind"], "field");
    assert_eq!(extra["change"], "added");
    assert_eq!(extra["callers"], json!(null));
    assert_eq!(extra["callers_reason"], "added");
}

#[test]
fn a_cpp_field_without_a_usable_index_keeps_the_existing_reason() {
    if !git_available() {
        return;
    }
    let edited = format!(
        "class Widget {{\npublic:\n    long target;\n}};\n\nint target() {{\n    return 2;\n}}\n{FIELD_USER}"
    );
    // No index: not `data-member`.
    let dir = field_repo();
    let home = setup(&dir);
    write(&dir, "lib.cpp", &edited);
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let field = &file(&doc, "lib.cpp")["definitions"][1];
    assert_eq!(field["kind"], "field");
    assert_eq!(field["callers_reason"], "no-index");
    // Unreadable index: not `data-member` either.
    let dir = field_repo();
    let home = setup(&dir);
    std::fs::create_dir_all(home.join("index")).unwrap();
    std::fs::write(home.join("index").join("r.db"), b"this is not sqlite").unwrap();
    write(&dir, "lib.cpp", &edited);
    let (code, doc, err) = changes(&home, &["--scope", "unstaged"]);
    assert_eq!(code, 0, "{err}");
    let field = &file(&doc, "lib.cpp")["definitions"][1];
    assert_eq!(field["callers_reason"], "index-unavailable");
}
