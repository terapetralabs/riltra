// SPDX-License-Identifier: Apache-2.0
//! `riltra checks` CLI (stage 6, Task 3): test candidates and coverage limits for one git change.
//! Expectations are written from the edits, never copied from output. Each repository is a real
//! git repository in a scratch directory; storage lives under an isolated `RILTRA_HOME`.
// This file builds its own git fixture, so it uses only `run` and `snapshot` from the shared helpers.
#[allow(dead_code)]
#[path = "common/stage2.rs"]
mod stage2;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use stage2::Scratch;

const CALC: &str = "def add(a, b):\n    return a + b\n";
/// The body of `add` changes; its first line, where its definition starts, does not move.
const CALC_EDITED: &str = "def add(a, b):\n    total = a + b\n    return total\n";
const TEST_CALC: &str =
    "from pkg.calc import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n";

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

/// A committed git repository: `pkg/calc.py` defines `add`, `tests/test_calc.py` calls it.
fn project(scratch: &Scratch) -> PathBuf {
    let root = scratch.path().join("project");
    std::fs::create_dir_all(root.join("pkg")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    git(&root, &["init", "-q", "-b", "main", "."]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["config", "user.email", "t@t"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("pkg/calc.py"), CALC).unwrap();
    std::fs::write(root.join("tests/test_calc.py"), TEST_CALC).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    root
}

fn riltra(home: &Path, args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    stage2::run(home, &args, &[])
}

/// Registers `root` as repository `r` in a fresh home, and builds its index when asked.
fn setup(scratch: &Scratch, root: &Path, index: bool) -> PathBuf {
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root_text = root.display().to_string();
    let init = riltra(&home, &["init", "--repo", &root_text, "--name", "r"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    if index {
        let built = riltra(&home, &["index", "--repo", "r"]);
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
    }
    home
}

fn checks(home: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["checks", "--repo", "r"];
    args.extend_from_slice(extra);
    riltra(home, &args)
}

/// The one JSON document a successful `checks` run prints.
fn document(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .expect("checks prints one JSON document")
}

fn candidate<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    doc["test_candidates"]
        .as_array()
        .expect("test_candidates array")
        .iter()
        .find(|c| c["path"] == path)
}

fn definition<'a>(doc: &'a Value, path: &str, name: &str) -> &'a Value {
    doc["changed"]["definitions"]
        .as_array()
        .expect("definitions array")
        .iter()
        .find(|d| d["path"] == path && d["name"] == name)
        .unwrap_or_else(|| panic!("no definition {name} of {path} in {doc}"))
}

fn limit_names(doc: &Value) -> Vec<String> {
    doc["coverage_limits"]
        .as_array()
        .expect("coverage_limits array")
        .iter()
        .map(|l| l["limit"].as_str().unwrap().to_string())
        .collect()
}

fn commands(doc: &Value) -> Vec<Option<String>> {
    doc["next_actions"]
        .as_array()
        .expect("next_actions array")
        .iter()
        .map(|a| a["command"].as_str().map(str::to_string))
        .collect()
}

/// The reason of a candidate that names a definition called `name`.
fn name_reason(candidate: &Value, name: &str) -> Value {
    candidate["reasons"]
        .as_array()
        .expect("reasons array")
        .iter()
        .find(|r| r["source"]["type"] == "definition" && r["source"]["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no definition-sourced reason for {name} in {candidate}"))
}

/// Sorted (relative path, bytes) of every file under `root`, `.git` excluded.
fn worktree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    stage2::snapshot(root)
        .into_iter()
        .filter(|(path, _)| !path.starts_with(".git"))
        .collect()
}

/// Relative names present in only one listing, or whose bytes differ between them.
fn changed_names(before: &[(PathBuf, Vec<u8>)], after: &[(PathBuf, Vec<u8>)]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for entry in before.iter().filter(|e| !after.contains(*e)) {
        names.insert(entry.0.to_string_lossy().replace('\x5c', "/"));
    }
    for entry in after.iter().filter(|e| !before.contains(*e)) {
        names.insert(entry.0.to_string_lossy().replace('\x5c', "/"));
    }
    names.into_iter().collect()
}

#[test]
fn checks_cli_no_index_creates_nothing() {
    if !git_available() {
        return;
    }
    let scratch = Scratch::new("checks-no-index");
    let root = project(&scratch);
    let home = setup(&scratch, &root, false);
    std::fs::write(root.join("pkg/calc.py"), CALC_EDITED).unwrap();
    let before = stage2::snapshot(&home);
    let doc = document(&checks(&home, &[]));
    assert_eq!(doc["index_freshness"]["status"], "no-index");
    assert!(doc["snapshot"].is_null(), "{doc}");
    assert!(
        !home.join("index").join("r.db").exists(),
        "checks must not create the index"
    );
    assert_eq!(
        stage2::snapshot(&home),
        before,
        "checks must not write storage"
    );
}

#[test]
fn checks_cli_syntax_only_repo() {
    if !git_available() {
        return;
    }
    let scratch = Scratch::new("checks-syntax");
    let root = project(&scratch);
    let home = setup(&scratch, &root, true);
    std::fs::write(root.join("pkg/calc.py"), CALC_EDITED).unwrap();
    let doc = document(&checks(&home, &[]));

    // The snapshot definition is traversed after the edit: analysed, not anchor-unavailable.
    assert_eq!(definition(&doc, "pkg/calc.py", "add")["status"], "analysed");
    assert_eq!(doc["index_freshness"]["status"], "stale", "{doc}");

    let c = candidate(&doc, "tests/test_calc.py").expect("the calling test is a candidate");
    assert_eq!(c["basis"], "name-candidate", "{c}");
    assert_eq!(c["state"], "suggested");
    let by_name = name_reason(c, "add");
    assert_eq!(by_name["basis"], "name-candidate");
    assert_eq!(by_name["source"]["path"], "pkg/calc.py");
    assert_eq!(by_name["detail"], "distance-1");
    let by_stem = c["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["basis"] == "path-convention")
        .expect("the same-stem file-name reason");
    assert_eq!(by_stem["source"]["type"], "file");
    assert_eq!(by_stem["source"]["path"], "pkg/calc.py");
    assert_eq!(by_stem["detail"], "same-stem");

    let limits = limit_names(&doc);
    for name in ["index-stale", "no-semantic-import", "partial-graph"] {
        assert!(limits.iter().any(|l| l == name), "{name} in {limits:?}");
    }
    let actions = commands(&doc);
    assert!(
        actions.contains(&Some("riltra index --repo r".to_string())),
        "{actions:?}"
    );
    assert!(
        actions.contains(&Some("riltra semantic setup --repo r".to_string())),
        "{actions:?}"
    );
    let last = doc["next_actions"].as_array().unwrap().last().unwrap();
    assert!(last["command"].is_null(), "{last}");
    assert!(last["reason"]
        .as_str()
        .unwrap()
        .starts_with("run the checks your project requires"));
}

#[test]
fn checks_cli_writes_only_wal_sidecars() {
    if !git_available() {
        return;
    }
    let scratch = Scratch::new("checks-wal");
    let root = project(&scratch);
    let home = setup(&scratch, &root, true);
    std::fs::write(root.join("pkg/calc.py"), CALC_EDITED).unwrap();
    let home_before = stage2::snapshot(&home);
    let repo_before = worktree(&root);
    document(&checks(&home, &[]));
    let diff = changed_names(&home_before, &stage2::snapshot(&home));
    assert!(
        diff.iter()
            .all(|name| name.ends_with("-wal") || name.ends_with("-shm")),
        "checks changed non-sidecar storage: {diff:?}"
    );
    assert_eq!(
        worktree(&root),
        repo_before,
        "checks must not change the worktree"
    );

    // Control: a planted file shows up in the same diff helper.
    let mut with_plant = stage2::snapshot(&home);
    with_plant.push((PathBuf::from("planted.txt"), b"x".to_vec()));
    assert_eq!(
        changed_names(&stage2::snapshot(&home), &with_plant),
        vec!["planted.txt".to_string()]
    );
}

#[test]
fn checks_cli_is_deterministic() {
    if !git_available() {
        return;
    }
    let scratch = Scratch::new("checks-deterministic");
    let root = project(&scratch);
    let home = setup(&scratch, &root, true);
    std::fs::write(root.join("pkg/calc.py"), CALC_EDITED).unwrap();
    let first = checks(&home, &[]);
    let second = checks(&home, &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        first.stdout, second.stdout,
        "two runs must print the same bytes"
    );
}

#[test]
fn checks_cli_usage_and_refusals() {
    if !git_available() {
        return;
    }
    let scratch = Scratch::new("checks-refusals");
    let root = project(&scratch);
    let home = setup(&scratch, &root, true);

    let no_base = checks(&home, &["--scope", "compare"]);
    assert_eq!(
        no_base.status.code(),
        Some(2),
        "--scope compare needs --base"
    );
    let unknown_ref = checks(&home, &["--scope", "compare", "--base", "no-such-ref-xyz"]);
    assert_eq!(
        unknown_ref.status.code(),
        Some(2),
        "an unknown ref is a usage error"
    );
    let unknown_repo = riltra(&home, &["checks", "--repo", "nope"]);
    assert_eq!(
        unknown_repo.status.code(),
        Some(3),
        "an unknown repository is exit 3"
    );

    // A registered root below the git top level is refused, as `changes` refuses it.
    let init = riltra(
        &home,
        &[
            "init",
            "--repo",
            &root.join("pkg").display().to_string(),
            "--name",
            "sub",
        ],
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let below_top = riltra(&home, &["checks", "--repo", "sub"]);
    assert_eq!(
        below_top.status.code(),
        Some(3),
        "a subdirectory root is exit 3"
    );
}

#[test]
fn checks_removed_file_keeps_snapshot_candidates() {
    if !git_available() {
        return;
    }
    let scratch = Scratch::new("checks-removed");
    let root = project(&scratch);
    let home = setup(&scratch, &root, true);

    // Control: the edited body traverses the snapshot definition, and its caller is a candidate.
    std::fs::write(root.join("pkg/calc.py"), CALC_EDITED).unwrap();
    let edited = document(&checks(&home, &[]));
    let control = candidate(&edited, "tests/test_calc.py").expect("control candidate");
    let control_reason = name_reason(control, "add");

    // The file is gone from the worktree: restore the committed text, then delete it.
    git(&root, &["checkout", "--", "pkg/calc.py"]);
    std::fs::remove_file(root.join("pkg/calc.py")).unwrap();
    let output = checks(&home, &[]);
    let doc = document(&output);

    let removed = definition(&doc, "pkg/calc.py", "add");
    assert_eq!(removed["change"], "removed", "{removed}");
    assert_eq!(removed["status"], "analysed", "{removed}");
    assert!(
        limit_names(&doc).iter().any(|l| l == "index-stale"),
        "{:?}",
        limit_names(&doc)
    );
    let kept = candidate(&doc, "tests/test_calc.py")
        .expect("the snapshot caller is still a candidate after the file is removed");
    let reason = name_reason(kept, "add");
    assert_eq!(reason["basis"], "name-candidate");
    assert_eq!(reason["source"]["path"], "pkg/calc.py");
    assert_eq!(
        reason, control_reason,
        "the reason must match the control run"
    );
}

#[test]
fn checks_cli_has_no_run_or_passed_state() {
    if !git_available() {
        return;
    }
    let scratch = Scratch::new("checks-states");
    let root = project(&scratch);
    let home = setup(&scratch, &root, true);
    std::fs::write(root.join("pkg/calc.py"), CALC_EDITED).unwrap();
    let doc = document(&checks(&home, &[]));
    let mut states = Vec::new();
    collect_states(&doc, &mut states);
    assert!(!states.is_empty(), "the document carries states: {doc}");
    for state in &states {
        assert!(
            state == "found" || state == "suggested",
            "unexpected state '{state}'"
        );
    }
    assert!(states.iter().any(|s| s == "suggested"), "{states:?}");
}

/// Every `state` value anywhere in the document, depth first.
fn collect_states(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if key == "state" {
                    if let Some(text) = item.as_str() {
                        out.push(text.to_string());
                    }
                }
                collect_states(item, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_states(item, out)),
        _ => {}
    }
}
