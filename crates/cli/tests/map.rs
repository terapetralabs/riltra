// SPDX-License-Identifier: Apache-2.0
//! `riltra map` CLI: read-only storage, deterministic JSON, Markdown parity
//! and exit codes (stage 5, Task 3).
#[path = "common/stage2.rs"]
mod stage2;

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Output;
use std::time::SystemTime;

type Entry = (String, u64, SystemTime);

fn args(extra: &[&str]) -> Vec<String> {
    let mut all = vec!["map".to_string(), "--repo".to_string(), "demo".to_string()];
    all.extend(extra.iter().map(|s| s.to_string()));
    all
}

fn init_project(scratch: &stage2::Scratch, home: &Path, index: bool) {
    let root = scratch.path().join("project");
    stage2::project(&root);
    let init = stage2::run(
        home,
        &[
            "init".into(),
            "--repo".into(),
            root.display().to_string(),
            "--name".into(),
            "demo".into(),
        ],
        &[],
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    if index {
        let built = stage2::run(home, &["index".into(), "--repo".into(), "demo".into()], &[]);
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
    }
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// Sorted (relative path, length, mtime) of every file under `root`.
fn listing(root: &Path) -> Vec<Entry> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<Entry>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            if meta.is_dir() {
                visit(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, meta.len(), meta.modified().unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Relative names present in only one listing, or changed between them.
fn changed(before: &[Entry], after: &[Entry]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for entry in before {
        if !after.contains(entry) {
            names.insert(entry.0.clone());
        }
    }
    for entry in after {
        if !before.contains(entry) {
            names.insert(entry.0.clone());
        }
    }
    names.into_iter().collect()
}

#[test]
fn map_cli_no_index_creates_nothing() {
    let scratch = stage2::Scratch::new("map-no-index");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    init_project(&scratch, &home, false);

    let db = home.join("index").join("demo.db");
    let before = stage2::snapshot(&home);
    let output = stage2::run(&home, &args(&[]), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout_text(&output).contains("\"status\":\"no-index\""));
    assert!(!db.exists(), "map must not create the index");
    assert_eq!(
        stage2::snapshot(&home),
        before,
        "map must not write storage"
    );

    // Control: a file at that path makes the same existence check report it.
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"").unwrap();
    assert!(db.exists());
}

#[test]
fn map_cli_writes_only_wal_sidecars() {
    let scratch = stage2::Scratch::new("map-wal");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    init_project(&scratch, &home, true);

    let registry = std::fs::read(home.join("registry.json")).unwrap();
    let before = listing(&home);
    let output = stage2::run(&home, &args(&[]), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = listing(&home);

    let diff = changed(&before, &after);
    assert!(
        diff.iter()
            .all(|name| name.ends_with("-wal") || name.ends_with("-shm")),
        "map changed non-sidecar storage: {diff:?}"
    );
    assert_eq!(std::fs::read(home.join("registry.json")).unwrap(), registry);

    // Control: a planted file shows up in the same diff helper.
    let planted = home.join("planted.txt");
    std::fs::write(&planted, b"x").unwrap();
    let with_plant = listing(&home);
    assert_eq!(
        changed(&after, &with_plant),
        vec!["planted.txt".to_string()]
    );
    std::fs::remove_file(&planted).unwrap();
}

#[test]
fn map_cli_syntax_only_repo() {
    let scratch = stage2::Scratch::new("map-syntax");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    init_project(&scratch, &home, true);

    let output = stage2::run(&home, &args(&[]), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_str(&stdout_text(&output)).unwrap();
    assert_eq!(report["links"], serde_json::json!([]));
    assert!(
        report["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == "setup_semantic_provider"),
        "{report}"
    );
}

#[test]
fn map_cli_markdown_matches_json() {
    let scratch = stage2::Scratch::new("map-markdown");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    init_project(&scratch, &home, true);

    let json = stage2::run(&home, &args(&[]), &[]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let report: Value = serde_json::from_str(&stdout_text(&json)).unwrap();

    let markdown = stage2::run(&home, &args(&["--format", "markdown"]), &[]);
    assert!(
        markdown.status.success(),
        "{}",
        String::from_utf8_lossy(&markdown.stderr)
    );
    let text = stdout_text(&markdown);
    assert!(text.starts_with("# Area map: demo\n"), "{text}");
    let areas = report["areas"].as_array().unwrap();
    assert!(!areas.is_empty());
    for area in areas {
        let name = area["area"].as_str().unwrap();
        assert!(
            text.contains(&format!("| {name} |")),
            "{name} missing: {text}"
        );
    }
}

#[test]
fn map_cli_unknown_repo_exits_3() {
    let scratch = stage2::Scratch::new("map-unknown");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    init_project(&scratch, &home, true);

    let output = stage2::run(&home, &["map".into(), "--repo".into(), "nope".into()], &[]);
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
}

#[test]
fn map_cli_is_deterministic() {
    let scratch = stage2::Scratch::new("map-deterministic");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    init_project(&scratch, &home, true);

    let first = stage2::run(&home, &args(&[]), &[]);
    let second = stage2::run(&home, &args(&[]), &[]);
    assert!(first.status.success() && second.status.success());
    assert_eq!(first.stdout, second.stdout);
}
