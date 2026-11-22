// SPDX-License-Identifier: Apache-2.0
//! `riltra docs scan` (Stage 7, Task 2): Markdown sections, links, proposals and
//! existing-record freshness. Each case is a real git repository in a scratch
//! directory, with storage under an isolated `RILTRA_HOME`. Expectations are written
//! from the fixtures, never copied from output.
// Shares only the process runner and snapshot helpers with the other stage tests.
#[allow(dead_code)]
#[path = "common/stage2.rs"]
mod stage2;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use stage2::Scratch;

const CALC: &str = "def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n";
const OTHER: &str = "def keep():\n    return 0\n";
const DOC: &str = "# Storage\nAdd uses [add](../../pkg/calc.py#L1).\n## Notes\nSee [file](../../pkg/calc.py) and [s](../../pkg/calc.py#L4) and [o](../../../x.rs#L1).\n";
const LINKS: &str = "# Links\n[add](../pkg/calc.py#L1) and [keep](../pkg/other.py#L1)\n";
const DEF_ROLE: u64 = 1;

/// One SCIP occurrence: symbol, 0-based line, start and end column, symbol roles.
type Occ = (&'static str, i64, i64, i64, u64);

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

fn cli(home: &Path, args: &[String]) -> Output {
    stage2::run(home, args, &[])
}

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// A committed git repository registered as `r` in a fresh home.
struct Fixture {
    scratch: Scratch,
    root: PathBuf,
    home: PathBuf,
}

fn fixture(tag: &str, files: &[(&str, &str)]) -> Option<Fixture> {
    if !git_available() {
        return None;
    }
    let scratch = Scratch::new(tag);
    let root = scratch.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    for (rel, text) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(&root, &["init", "-q", "-b", "main", "."]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["config", "user.email", "t@t"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root_text = root.display().to_string();
    let init = cli(
        &home,
        &words(&["init", "--repo", &root_text, "--name", "r"]),
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    Some(Fixture {
        scratch,
        root,
        home,
    })
}

impl Fixture {
    fn index(&self) {
        let out = cli(&self.home, &words(&["index", "--repo", "r"]));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Writes a SCIP index over the given documents and imports it into the live snapshot.
    fn import(&self, docs: &[(&str, Vec<Occ>)]) {
        let mut metadata = Vec::new();
        let mut tool = Vec::new();
        str_field(1, "riltra-docs-test", &mut tool);
        str_field(2, "1.0", &mut tool);
        len_field(2, &tool, &mut metadata);
        str_field(
            3,
            &format!("file://{}", self.root.to_string_lossy().replace('\\', "/")),
            &mut metadata,
        );
        varint_field(4, 1, &mut metadata);
        let mut payload = Vec::new();
        len_field(1, &metadata, &mut payload);
        for (rel, occurrences) in docs {
            let mut document = Vec::new();
            str_field(1, rel, &mut document);
            for (symbol, line, start, end, roles) in occurrences {
                len_field(
                    2,
                    &occurrence(symbol, *line, *start, *end, *roles),
                    &mut document,
                );
            }
            varint_field(6, 1, &mut document);
            len_field(2, &document, &mut payload);
        }
        let scip = self.scratch.path().join("index.scip");
        std::fs::write(&scip, &payload).unwrap();
        let out = cli(
            &self.home,
            &words(&[
                "semantic",
                "import",
                "--repo",
                "r",
                "--scip",
                &scip.to_string_lossy(),
            ]),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn scan(&self, paths: &[&str]) -> Output {
        let mut args = words(&["docs", "scan", "--repo", "r"]);
        for p in paths {
            args.push("--path".to_string());
            args.push(p.to_string());
        }
        cli(&self.home, &args)
    }

    fn scan_json(&self, paths: &[&str]) -> Value {
        let out = self.scan(paths);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("scan prints JSON")
    }

    fn create_record(&self, id: &str, path: &str, byte_start: usize, byte_end: usize) {
        let input = format!(
            "{{\"id\":\"{id}\",\"kind\":\"decision\",\"title\":\"Storage\",\"body\":\"stored\",\"status\":\"active\",\"author\":\"a\",\"evidence\":[{{\"path\":\"{path}\",\"byte_start\":{byte_start},\"byte_end\":{byte_end}}}]}}"
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_riltra"))
            .args(["knowledge", "create", "--repo", "r", "--input", "-"])
            .env("RILTRA_HOME", &self.home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn doc<'a>(report: &'a Value, path: &str) -> &'a Value {
    report["documents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == path)
        .unwrap_or_else(|| panic!("document {path} missing from {report}"))
}

fn section<'a>(document: &'a Value, id: &str) -> &'a Value {
    document["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["section_id"] == id)
        .unwrap_or_else(|| panic!("section {id} missing"))
}

fn statuses(section: &Value) -> Vec<String> {
    section["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["status"].as_str().unwrap().to_string())
        .collect()
}

fn occurrence(symbol: &str, line: i64, start: i64, end: i64, roles: u64) -> Vec<u8> {
    let mut range = Vec::new();
    varint_field(1, line as u64, &mut range);
    varint_field(2, start as u64, &mut range);
    varint_field(3, end as u64, &mut range);
    let mut out = Vec::new();
    str_field(2, symbol, &mut out);
    varint_field(3, roles, &mut out);
    len_field(8, &range, &mut out);
    out
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
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

const ADD: &str = "scip-kt test pkg 1.0.0 pkg/calc.py/add().";
const SUB: &str = "scip-kt test pkg 1.0.0 pkg/calc.py/sub().";
const KEEP: &str = "scip-kt test pkg 1.0.0 pkg/other.py/keep().";

/// Every file of a home or repository, as sorted (path, bytes).
fn listing(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    stage2::snapshot(dir)
}

fn calc_defs() -> Vec<Occ> {
    vec![(ADD, 0, 4, 7, DEF_ROLE), (SUB, 3, 4, 7, DEF_ROLE)]
}

#[test]
fn scan_lists_sections_links_and_proposals() {
    let Some(f) = fixture("lists", &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)]) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let d = doc(&report, "docs/adr/0001.md");
    assert_eq!(d["status"], "ok");
    assert_eq!(
        d["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["section_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        vec!["Storage@1".to_string(), "Storage / Notes@1".to_string()]
    );
    assert_eq!(statuses(section(d, "Storage@1")), vec!["definition"]);
    assert_eq!(
        statuses(section(d, "Storage / Notes@1")),
        vec!["file", "definition", "outside-repo"]
    );
    let proposals = report["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 2, "{report}");
    for p in proposals {
        assert_eq!(p["state"], "proposed");
        assert_eq!(p["relation"], "documents");
    }
    let names: Vec<&str> = proposals
        .iter()
        .map(|p| p["target"]["definition"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["add", "sub"]);
    assert_eq!(report["knowledge"]["status"], "no-store");
    assert!(!f.home.join("knowledge").join("r.db").exists());
}

#[test]
fn scan_counts_outside_repo_link() {
    let Some(f) = fixture(
        "outside",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let d = doc(&report, "docs/adr/0001.md");
    assert_eq!(d["link_counts"]["outside-repo"], 1);
    let proposals = report["proposals"].as_array().unwrap();
    assert!(
        proposals
            .iter()
            .all(|p| p["target"]["path"] != "../../x.rs"),
        "{report}"
    );
}

#[test]
fn scan_marks_edited_target_source_changed_and_keeps_clean_target() {
    let Some(f) = fixture(
        "edited",
        &[
            ("pkg/calc.py", CALC),
            ("pkg/other.py", OTHER),
            ("docs/links.md", LINKS),
        ],
    ) else {
        return;
    };
    f.index();
    f.import(&[
        ("pkg/calc.py", calc_defs()),
        ("pkg/other.py", vec![(KEEP, 0, 4, 8, DEF_ROLE)]),
    ]);

    // Control: before the edit, the calc.py link is a definition.
    let before = f.scan_json(&[]);
    let links_before = statuses(section(doc(&before, "docs/links.md"), "Links@1"));
    assert_eq!(links_before, vec!["definition", "definition"], "{before}");

    let edited = CALC.replace("return a + b", "total = a + b\n    return total");
    f.write("pkg/calc.py", &edited);
    let report = f.scan_json(&[]);
    assert_eq!(report["index_freshness"]["status"], "stale", "{report}");
    let links = section(doc(&report, "docs/links.md"), "Links@1")["links"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(links[0]["status"], "source-changed", "{report}");
    assert_eq!(links[1]["status"], "definition", "{report}");
    assert!(
        report["proposals"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["target"]["path"] != "pkg/calc.py"),
        "{report}"
    );
    assert!(
        report["proposals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["target"]["path"] == "pkg/other.py"),
        "{report}"
    );
    let actions: Vec<Value> = report["next_actions"].as_array().unwrap().clone();
    assert!(
        actions
            .iter()
            .any(|a| a["command"] == "riltra index --repo r"),
        "{report}"
    );
}

#[test]
fn scan_no_index_classifies_links_no_index() {
    let Some(f) = fixture(
        "noindex",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    let home_before = listing(&f.home);
    let report = f.scan_json(&[]);
    assert!(report["snapshot"].is_null(), "{report}");
    let d = doc(&report, "docs/adr/0001.md");
    assert_eq!(statuses(section(d, "Storage@1")), vec!["no-index"]);
    assert_eq!(
        statuses(section(d, "Storage / Notes@1")),
        vec!["no-index", "no-index", "outside-repo"]
    );
    assert!(report["proposals"].as_array().unwrap().is_empty());
    assert_eq!(listing(&f.home), home_before, "scan changed the home");
}

#[test]
fn scan_lists_stale_record_and_no_recorded_section() {
    let Some(f) = fixture("records", &[("docs/adr/0001.md", DOC)]) else {
        return;
    };
    let storage_end = DOC.find("## Notes").unwrap();
    f.create_record("s1", "docs/adr/0001.md", 0, storage_end);

    let report = f.scan_json(&[]);
    let d = doc(&report, "docs/adr/0001.md");
    assert_eq!(section(d, "Storage@1")["recorded"]["id"], "s1", "{report}");
    assert_eq!(section(d, "Storage@1")["recorded"]["revision"], 1);
    assert!(section(d, "Storage / Notes@1")["recorded"].is_null());

    // Insert a line into the section: its digest and span change.
    f.write(
        "docs/adr/0001.md",
        &DOC.replacen("# Storage\n", "# Storage\nAn added line.\n", 1),
    );
    let report = f.scan_json(&[]);
    let d = doc(&report, "docs/adr/0001.md");
    assert!(section(d, "Storage@1")["recorded"].is_null(), "{report}");
    let stale = d["stale_records"].as_array().unwrap();
    assert_eq!(stale.len(), 1, "{report}");
    assert_eq!(stale[0]["id"], "s1");
    assert_eq!(stale[0]["evidence"][0]["freshness"], "stale");
}

#[test]
fn scan_writes_only_wal_sidecars() {
    let Some(f) = fixture(
        "sidecars",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    f.create_record("s1", "docs/adr/0001.md", 0, DOC.find("## Notes").unwrap());
    let home_before = listing(&f.home);
    let root_before = listing(&f.root);
    let out = f.scan(&[]);
    assert_eq!(out.status.code(), Some(0));
    for (before, after) in [
        (home_before, listing(&f.home)),
        (root_before, listing(&f.root)),
    ] {
        for (path, bytes) in &after {
            let name = path.to_string_lossy();
            let same = before.iter().any(|(p, b)| p == path && b == bytes);
            assert!(
                same || name.ends_with("-wal") || name.ends_with("-shm"),
                "scan changed {name}"
            );
        }
        for (path, _) in &before {
            assert!(
                after.iter().any(|(p, _)| p == path),
                "scan removed {path:?}"
            );
        }
    }
}

#[test]
fn scan_is_deterministic() {
    let Some(f) = fixture(
        "determinism",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let first = f.scan(&[]).stdout;
    let second = f.scan(&[]).stdout;
    assert!(!first.is_empty());
    assert_eq!(first, second);
}

#[test]
fn scan_walk_skips_ignored_and_symlinked_docs() {
    let Some(f) = fixture(
        "walk",
        &[
            ("docs/adr/0001.md", DOC),
            (".riltraignore", "docs/private/\n"),
        ],
    ) else {
        return;
    };
    f.write("docs/private/x.md", "# Secret\n");
    let link = f.root.join("docs").join("link.md");
    let linked = link_file(&f.root.join("docs/adr/0001.md"), &link);
    let report = f.scan_json(&[]);
    let paths: Vec<&str> = report["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"docs/adr/0001.md"), "{paths:?}");
    assert!(!paths.contains(&"docs/private/x.md"), "{paths:?}");
    if linked {
        assert!(!paths.contains(&"docs/link.md"), "{paths:?}");
    } else {
        eprintln!("skipped symlink part: links cannot be created here");
    }
}

#[cfg(unix)]
fn link_file(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(windows)]
fn link_file(target: &Path, link: &Path) -> bool {
    std::os::windows::fs::symlink_file(target, link).is_ok()
}

#[cfg(unix)]
fn link_dir(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// A directory junction: unlike a symbolic link it needs no privilege on Windows.
#[cfg(windows)]
fn link_dir(target: &Path, link: &Path) -> bool {
    if std::os::windows::fs::symlink_dir(target, link).is_ok() {
        return true;
    }
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

#[test]
fn scan_usage_errors() {
    let Some(f) = fixture(
        "usage",
        &[("src/a.rs", "fn a() {}\n"), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    assert_eq!(f.scan(&["src/a.rs"]).status.code(), Some(2));
    assert_eq!(f.scan(&["../x.md"]).status.code(), Some(2));
    let many: Vec<String> = (0..21).map(|i| format!("docs/a{i}.md")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(f.scan(&refs).status.code(), Some(2));
    let unknown = cli(&f.home, &words(&["docs", "scan", "--repo", "nope"]));
    assert_eq!(unknown.status.code(), Some(3));
}

#[test]
fn scan_reports_missing_explicit_path() {
    let Some(f) = fixture("missing", &[("docs/adr/0001.md", DOC)]) else {
        return;
    };
    let report = f.scan_json(&["docs/gone.md"]);
    let d = doc(&report, "docs/gone.md");
    assert_eq!(d["status"], "missing");
    assert!(d["sections"].as_array().unwrap().is_empty());
}

#[test]
fn scan_rejects_explicit_path_through_link_out_of_root() {
    let Some(f) = fixture("escape", &[("docs/adr/0001.md", DOC)]) else {
        return;
    };
    let outside = f.scratch.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x.md"), "# X\n").unwrap();
    if !link_dir(&outside, &f.root.join("docs").join("link")) {
        eprintln!("skipped: directory links cannot be created here");
        return;
    }
    assert_eq!(f.scan(&["docs/link/x.md"]).status.code(), Some(2));
    // Control: the same command shape with a document inside the root passes.
    assert_eq!(f.scan(&["docs/adr/0001.md"]).status.code(), Some(0));
}

#[test]
fn scan_does_not_offer_return_type_occurrence() {
    let src = "struct Thing {};\nThing make() { return {}; }\n";
    let doc_text = "# Cpp\n[make](../src/thing.cpp#L2)\n";
    let Some(f) = fixture(
        "rettype",
        &[("src/thing.cpp", src), ("docs/cpp.md", doc_text)],
    ) else {
        return;
    };
    f.index();
    let thing = "scip-kt test pkg 1.0.0 src/thing.cpp/Thing#";
    let make = "scip-kt test pkg 1.0.0 src/thing.cpp/make().";
    f.import(&[(
        "src/thing.cpp",
        vec![
            (thing, 0, 7, 12, DEF_ROLE),
            (thing, 1, 0, 5, 0),
            (make, 1, 6, 10, DEF_ROLE),
        ],
    )]);
    let report = f.scan_json(&[]);
    let proposals = report["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "{report}");
    assert_eq!(proposals[0]["target"]["definition"]["name"], "make");
    assert_eq!(proposals[0]["target"]["anchor"]["kind"], "semantic");
    assert_eq!(proposals[0]["target"]["anchor"]["symbol_uri"], make);
    assert_eq!(proposals[0]["target"]["anchor"]["byte_start"], 23);
    assert_eq!(proposals[0]["target"]["anchor"]["byte_end"], 27);
}

#[test]
fn scan_offers_nothing_when_the_make_occurrence_is_dropped() {
    let src = "struct Thing {};\nThing make() { return {}; }\n";
    let doc_text = "# Cpp\n[make](../src/thing.cpp#L2)\n";
    let Some(f) = fixture(
        "rettype-dropped",
        &[("src/thing.cpp", src), ("docs/cpp.md", doc_text)],
    ) else {
        return;
    };
    f.index();
    let thing = "scip-kt test pkg 1.0.0 src/thing.cpp/Thing#";
    // The `make` definition occurrence is absent; the reference to `Thing` remains.
    f.import(&[(
        "src/thing.cpp",
        vec![(thing, 0, 7, 12, DEF_ROLE), (thing, 1, 0, 5, 0)],
    )]);
    let report = f.scan_json(&[]);
    let link = &section(doc(&report, "docs/cpp.md"), "Cpp@1")["links"][0];
    assert_eq!(link["status"], "anchor-unproven", "{report}");
    assert!(link["definition"].is_null(), "{report}");
    assert!(link["anchor"].is_null(), "{report}");
    assert!(
        report["proposals"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert!(!report.to_string().contains("Thing"), "{report}");

    // Control: the resolver itself, at line 2 column 1 (the start of `make`), returns `Thing`.
    // This is the trap the scan avoids; reached through `callers`, which resolves the same
    // anchor without writing.
    let out = stage2::run(
        &f.home,
        &[
            "callers".to_string(),
            "--repo".to_string(),
            "r".to_string(),
            "--path".to_string(),
            "src/thing.cpp".to_string(),
            "--line".to_string(),
            "2".to_string(),
            "--col".to_string(),
            "1".to_string(),
        ],
        &[],
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("Thing"),
        "control: {text} {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn scan_does_not_offer_nested_same_name_occurrence() {
    let src = "def foo():\n    def foo():\n        return 1\n    return foo\n";
    let doc_outer = "# Nest\n[outer](../pkg/nest.py#L1)\n";
    let doc_inner = "# Nest\n[inner](../pkg/nest.py#L2)\n";
    let Some(f) = fixture(
        "nested",
        &[
            ("pkg/nest.py", src),
            ("docs/outer.md", doc_outer),
            ("docs/inner.md", doc_inner),
        ],
    ) else {
        return;
    };
    f.index();
    let inner = "scip-kt test pkg 1.0.0 pkg/nest.py/inner().";
    let outer = "scip-kt test pkg 1.0.0 pkg/nest.py/outer().";

    // Partial import: only the inner definition occurrence.
    f.import(&[("pkg/nest.py", vec![(inner, 1, 8, 11, DEF_ROLE)])]);
    let report = f.scan_json(&[]);
    // Outer link: no owned occurrence, so no proposal.
    assert_eq!(
        statuses(section(doc(&report, "docs/outer.md"), "Nest@1")),
        vec!["anchor-unproven"],
        "{report}"
    );
    // Control 2: the inner link is offered the inner definition's occurrence.
    let proposals = report["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "{report}");
    assert_eq!(proposals[0]["doc"], "docs/inner.md");
    assert_eq!(proposals[0]["target"]["definition"]["name"], "foo");
    assert_eq!(proposals[0]["target"]["anchor"]["byte_start"], 19);
    assert_eq!(proposals[0]["target"]["anchor"]["byte_end"], 22);

    // Complete import: the outer occurrence is offered for the outer definition.
    f.import(&[(
        "pkg/nest.py",
        vec![(outer, 0, 4, 7, DEF_ROLE), (inner, 1, 8, 11, DEF_ROLE)],
    )]);
    let report = f.scan_json(&[]);
    let outer_proposal = report["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["doc"] == "docs/outer.md")
        .unwrap_or_else(|| panic!("{report}"))
        .clone();
    assert_eq!(outer_proposal["target"]["anchor"]["byte_start"], 4);
    assert_eq!(outer_proposal["target"]["anchor"]["symbol_uri"], outer);
}

#[test]
fn scan_storage_error_in_resolver_exits_3() {
    let Some(f) = fixture(
        "storage",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    assert_eq!(
        f.scan(&[]).status.code(),
        Some(0),
        "control scan must pass first"
    );

    let db = f.home.join("index").join("r.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("DROP TABLE semantic_facts").unwrap();
    drop(conn);

    let out = f.scan(&["docs/adr/0001.md"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("semantic_facts"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn scan_snapshot_generation_moves_only_on_semantic_import() {
    let Some(f) = fixture(
        "generation",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let first = f.scan_json(&[])["snapshot"]["generation"].clone();
    // Control: with no import between them, the generation is unchanged.
    let again = f.scan_json(&[])["snapshot"]["generation"].clone();
    assert_eq!(first, again);
    assert!(first.is_i64(), "{first}");
    f.import(&[("pkg/calc.py", calc_defs())]);
    let after = f.scan_json(&[])["snapshot"]["generation"].clone();
    assert_ne!(first, after, "a semantic import must move the generation");
}

// ---------------------------------------------------------------------------
// Task 3: `riltra docs ingest` (one transaction per item).
// ---------------------------------------------------------------------------

const STORAGE_SECTION: &str = "# Storage\nAdd uses [add](../../pkg/calc.py#L1).\n";

impl Fixture {
    fn ingest(&self, input: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new(env!("CARGO_BIN_EXE_riltra"))
            .args(["docs", "ingest", "--repo", "r", "--input", "-"])
            .env("RILTRA_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn riltra");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().expect("wait for riltra")
    }

    /// `knowledge show` of one record, or `None` when the record cannot be shown.
    fn show(&self, id: &str) -> Option<Value> {
        let out = cli(
            &self.home,
            &words(&["knowledge", "show", "--repo", "r", "--id", id]),
        );
        if !out.status.success() {
            return None;
        }
        Some(serde_json::from_slice(&out.stdout).expect("show prints JSON"))
    }

    /// `knowledge at` of one line and column.
    fn at(&self, path: &str, line: &str, col: &str) -> Value {
        let out = cli(
            &self.home,
            &words(&[
                "knowledge",
                "at",
                "--repo",
                "r",
                "--path",
                path,
                "--line",
                line,
                "--col",
                col,
            ]),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("at prints JSON")
    }

    /// The store generation; 0 while no store exists.
    fn generation(&self) -> u64 {
        let out = cli(
            &self.home,
            &words(&[
                "knowledge",
                "at",
                "--repo",
                "r",
                "--path",
                "pkg/calc.py",
                "--line",
                "1",
                "--col",
                "1",
            ]),
        );
        if !out.status.success() {
            return 0;
        }
        let v: Value = serde_json::from_slice(&out.stdout).expect("at prints JSON");
        v["generation"].as_u64().unwrap_or(0)
    }

    /// `knowledge create` from JSON on stdin.
    fn knowledge_create(&self, input: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new(env!("CARGO_BIN_EXE_riltra"))
            .args(["knowledge", "create", "--repo", "r", "--input", "-"])
            .env("RILTRA_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn riltra");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().expect("wait for riltra")
    }

    /// Run `riltra` with `args` and `input` on stdin.
    fn stdin_cli(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new(env!("CARGO_BIN_EXE_riltra"))
            .args(args)
            .env("RILTRA_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn riltra");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().expect("wait for riltra")
    }

    /// `knowledge show` of one revision, or `None` when that revision does not exist.
    fn show_revision(&self, id: &str, revision: u64) -> Option<Value> {
        let rev = revision.to_string();
        let out = cli(
            &self.home,
            &words(&[
                "knowledge",
                "show",
                "--repo",
                "r",
                "--id",
                id,
                "--revision",
                &rev,
            ]),
        );
        if !out.status.success() {
            return None;
        }
        Some(serde_json::from_slice(&out.stdout).expect("show prints JSON"))
    }
}

/// The scan proposal for one section and one definition name.
fn proposal_of<'a>(report: &'a Value, section_id: &str, name: &str) -> &'a Value {
    report["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["section_id"] == section_id && p["target"]["definition"]["name"] == name)
        .unwrap_or_else(|| panic!("no proposal {section_id} {name}: {report}"))
}

/// One ingest item built from a scan proposal, as the reviewed values.
fn item_of(record_id: &str, p: &Value, kind: &str, bindings: Vec<Value>) -> Value {
    json!({
        "doc": p["doc"],
        "section_id": p["section_id"],
        "section_digest": p["section_digest"],
        "record_id": record_id,
        "kind": kind,
        "author": "tester",
        "bindings": bindings,
    })
}

fn binding_of(target: &Value, rationale: &str) -> Value {
    json!({"target": target, "relation": "documents", "rationale": rationale})
}

fn input_of(items: Vec<Value>) -> String {
    json!({ "items": items }).to_string()
}

fn code_of(out: &Output) -> Option<i32> {
    out.status.code()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn ingest_creates_record_and_binding_atomically() {
    let Some(f) = fixture(
        "ingest-create",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    assert_eq!(f.generation(), 0);
    let input = input_of(vec![item_of(
        "doc-storage",
        &p,
        "decision",
        vec![binding_of(&p["target"], "documents add")],
    )]);
    let out = f.ingest(&input);
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["operation"], "docs-ingest");
    assert_eq!(v["generation"], 1);
    let item = &v["result"]["items"][0];
    assert_eq!(item["record_id"], "doc-storage");
    assert_eq!(item["revision"], 1);
    assert_eq!(item["action"], "created");
    assert_eq!(item["bindings"].as_array().unwrap().len(), 1);
    assert_eq!(v["result"]["confirm_state"], "candidate");
    assert_eq!(v["result"]["verification"], "Unverified");

    let shown = f.show("doc-storage").expect("record exists");
    assert_eq!(shown["result"]["confirm_state"], "candidate");
    assert_eq!(shown["result"]["verification"], "Unverified");
    assert_eq!(shown["result"]["body"], STORAGE_SECTION);
    let ev = &shown["result"]["evidence"][0];
    assert_eq!(ev["path"], "docs/adr/0001.md");
    assert_eq!(ev["byte_start"], 0);
    assert_eq!(ev["byte_end"], STORAGE_SECTION.len());
    assert_eq!(ev["digest"], p["section_digest"]);

    let line = p["target"]["anchor"]["line"].to_string();
    let col = p["target"]["anchor"]["col"].to_string();
    let text = f.at("pkg/calc.py", &line, &col).to_string();
    assert!(
        text.contains(&format!(
            "docs-ingest:docs/adr/0001.md#Storage@1@{}",
            p["section_digest"].as_str().unwrap()
        )),
        "{text}"
    );
    assert_eq!(f.generation(), 1, "one bump per item");
}

#[test]
fn ingest_refuses_changed_section_and_writes_nothing() {
    let Some(f) = fixture(
        "ingest-changed",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    f.write("docs/adr/0001.md", &DOC.replace("Add uses", "Put uses"));
    let input = input_of(vec![item_of(
        "doc-storage",
        &p,
        "decision",
        vec![binding_of(&p["target"], "documents add")],
    )]);
    let out = f.ingest(&input);
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("section-changed"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(f.generation(), 0);
    assert!(f.show("doc-storage").is_none());
    assert!(!f.home.join("knowledge").join("r.db").exists());
}

#[test]
fn ingest_binds_clean_target_in_dirty_worktree() {
    let Some(f) = fixture(
        "ingest-dirty",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    // Only the Markdown changes, in a section the item does not cite.
    f.write(
        "docs/adr/0001.md",
        &DOC.replace("See [file]", "Now see [file]"),
    );
    let input = input_of(vec![item_of(
        "doc-storage",
        &p,
        "decision",
        vec![binding_of(&p["target"], "documents add")],
    )]);
    let out = f.ingest(&input);
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let shown = f.show("doc-storage").expect("record exists");
    assert_eq!(shown["result"]["revision"], 1);
}

#[test]
fn ingest_refuses_binding_to_edited_target() {
    let Some(f) = fixture(
        "ingest-edited",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    // Control: the item without a binding alone succeeds.
    let control = f.ingest(&input_of(vec![item_of(
        "first",
        &p,
        "decision",
        Vec::new(),
    )]));
    assert_eq!(code_of(&control), Some(0), "{}", stderr_of(&control));
    // The target changes after the scan; the whole input is refused.
    f.write("pkg/calc.py", &CALC.replace("a + b", "a * b"));
    let out = f.ingest(&input_of(vec![
        item_of("third", &p, "decision", Vec::new()),
        item_of(
            "second",
            &p,
            "decision",
            vec![binding_of(&p["target"], "documents add")],
        ),
    ]));
    assert_eq!(code_of(&out), Some(3), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("source changed; reindex before creating a binding"),
        "{}",
        stderr_of(&out)
    );
    assert!(f.show("third").is_none());
    assert!(f.show("second").is_none());
}

#[test]
fn reingest_keeps_earlier_revision_bindings() {
    let Some(f) = fixture(
        "ingest-reingest",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let out = f.ingest(&input_of(vec![item_of(
        "doc-storage",
        &p,
        "decision",
        vec![binding_of(&p["target"], "documents add")],
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    f.write("docs/adr/0001.md", &DOC.replace("Add uses", "Put uses"));
    let report = f.scan_json(&[]);
    let p2 = proposal_of(&report, "Storage@1", "add").clone();
    let mut item = item_of(
        "doc-storage",
        &p2,
        "decision",
        vec![binding_of(&p2["target"], "documents add again")],
    );
    item["expected_revision"] = json!(1);
    let out = f.ingest(&input_of(vec![item]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let item = &v["result"]["items"][0];
    assert_eq!(item["revision"], 2);
    assert_eq!(item["action"], "updated");
    assert_eq!(item["earlier_revision_bindings"], 1);
    let line = p2["target"]["anchor"]["line"].to_string();
    let col = p2["target"]["anchor"]["col"].to_string();
    let text = f.at("pkg/calc.py", &line, &col).to_string();
    assert!(text.contains("\"record_revision\":1"), "{text}");
    assert!(text.contains("\"retracted\":false"), "{text}");
}

#[test]
fn ingest_refuses_target_replaced_and_reindexed() {
    let Some(f) = fixture(
        "ingest-replaced",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let old = proposal_of(&report, "Storage@1", "add").clone();
    f.write("pkg/calc.py", "def sub(a, b):\n    return a - b\n");
    f.index();
    let out = f.ingest(&input_of(vec![item_of(
        "doc-storage",
        &old,
        "decision",
        vec![binding_of(&old["target"], "documents add")],
    )]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("binding-target-changed"),
        "{}",
        stderr_of(&out)
    );
    assert!(f.show("doc-storage").is_none());
    // Control: the same item built from a fresh scan binds the new definition.
    let fresh = f.scan_json(&[]);
    let new = proposal_of(&fresh, "Storage@1", "sub").clone();
    let out = f.ingest(&input_of(vec![item_of(
        "doc-storage",
        &new,
        "decision",
        vec![binding_of(&new["target"], "documents sub")],
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
}

#[test]
fn ingest_refuses_kind_mismatch_on_update() {
    let Some(f) = fixture(
        "ingest-kind",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let out = f.ingest(&input_of(vec![item_of(
        "doc-storage",
        &p,
        "constraint",
        Vec::new(),
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let mut wrong = item_of("doc-storage", &p, "decision", Vec::new());
    wrong["expected_revision"] = json!(1);
    let out = f.ingest(&input_of(vec![wrong]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("kind-mismatch"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(f.show("doc-storage").unwrap()["result"]["revision"], 1);
    // Control: the same kind writes revision 2.
    let mut same = item_of("doc-storage", &p, "constraint", Vec::new());
    same["expected_revision"] = json!(1);
    let out = f.ingest(&input_of(vec![same]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    assert_eq!(f.show("doc-storage").unwrap()["result"]["revision"], 2);

    // A benchmark record (made with `knowledge create`) is never overwritten by an ingest update.
    let bench = r#"{"id":"bench-one","kind":"benchmark","title":"Perf","body":"measured","status":"active","author":"a","evidence":[],"measurement":{"method":"m","environment":"e","sample_count":3,"metrics":{"latency":{"value":1.5,"unit":"ms"}}}}"#;
    let created = f.knowledge_create(bench);
    assert_eq!(code_of(&created), Some(0), "{}", stderr_of(&created));
    let mut onto_bench = item_of("bench-one", &p, "decision", Vec::new());
    onto_bench["expected_revision"] = json!(1);
    let out = f.ingest(&input_of(vec![onto_bench]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("kind-mismatch: bench-one is benchmark"),
        "{}",
        stderr_of(&out)
    );
    let bench_head = f.show("bench-one").unwrap();
    assert_eq!(bench_head["result"]["revision"], 1);
    assert!(
        f.show_revision("bench-one", 2).is_none(),
        "no revision 2: the measurement is not carried into a new revision"
    );
    assert!(bench_head.to_string().contains("latency"), "{bench_head}");

    // Control: a decision record of matching kind takes the same update.
    let out = f.ingest(&input_of(vec![item_of(
        "dec-one",
        &p,
        "decision",
        Vec::new(),
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let mut matching = item_of("dec-one", &p, "decision", Vec::new());
    matching["expected_revision"] = json!(1);
    let out = f.ingest(&input_of(vec![matching]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    assert_eq!(f.show("dec-one").unwrap()["result"]["revision"], 2);
}

#[test]
fn ingest_without_bindings_reads_candidate_on_new_and_v1_store() {
    // New home: no store exists before the ingest.
    let Some(f) = fixture(
        "ingest-candidate-new",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let out = f.ingest(&input_of(vec![item_of(
        "doc-new",
        &p,
        "decision",
        Vec::new(),
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let shown = f.show("doc-new").unwrap().to_string();
    assert!(shown.contains("\"confirm_state\":\"candidate\""), "{shown}");

    // Store created by `knowledge create` (schema 1, no confirm_state yet).
    let Some(g) = fixture(
        "ingest-candidate-v1",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    g.create_record("old-one", "docs/adr/0001.md", 0, 10);
    let before = g.show("old-one").unwrap().to_string();
    assert!(!before.contains("confirm_state"), "{before}");
    g.index();
    g.import(&[("pkg/calc.py", calc_defs())]);
    let report = g.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let out = g.ingest(&input_of(vec![item_of(
        "doc-v1",
        &p,
        "decision",
        Vec::new(),
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let shown = g.show("doc-v1").unwrap().to_string();
    assert!(shown.contains("\"confirm_state\":\"candidate\""), "{shown}");
}

#[test]
fn ingest_rejects_confirm_and_unknown_keys() {
    let Some(f) = fixture(
        "ingest-unknown",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let good = item_of("doc-a", &p, "decision", Vec::new());

    let mut confirm = good.clone();
    confirm["confirm"] = json!(true);
    let out = f.ingest(&input_of(vec![confirm]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("unknown field `items[0].confirm`"),
        "{}",
        stderr_of(&out)
    );

    let unknown_top = json!({"items": [good.clone()], "extra": 1});
    let out = f.ingest(&unknown_top.to_string());
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("unknown field `$.extra`"),
        "{}",
        stderr_of(&out)
    );

    let dup_a = item_of("doc-same", &p, "decision", Vec::new());
    let dup_b = item_of("doc-same", &p, "decision", Vec::new());
    let out = f.ingest(&input_of(vec![dup_a, dup_b]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("duplicate record id in input: 'doc-same'"),
        "{}",
        stderr_of(&out)
    );

    let many: Vec<Value> = (0..51)
        .map(|i| item_of(&format!("doc-{i}"), &p, "decision", Vec::new()))
        .collect();
    let out = f.ingest(&input_of(many));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("items must hold 1..=50 entries"),
        "{}",
        stderr_of(&out)
    );

    let mut no_digest = item_of(
        "doc-nodigest",
        &p,
        "decision",
        vec![binding_of(&p["target"], "documents add")],
    );
    no_digest["bindings"][0]["target"]["anchor"]
        .as_object_mut()
        .unwrap()
        .remove("source_digest");
    let out = f.ingest(&input_of(vec![no_digest]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out)
            .contains("missing required field `items[0].bindings[0].target.anchor.source_digest`"),
        "{}",
        stderr_of(&out)
    );
    assert!(f.show("doc-a").is_none());
    assert!(f.show("doc-nodigest").is_none());

    // Control: the same good base item with a fresh id is written.
    let out = f.ingest(&input_of(vec![item_of(
        "doc-control",
        &p,
        "decision",
        Vec::new(),
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    assert!(f.show("doc-control").is_some());
}

#[test]
fn ingest_rejects_nested_duplicate_and_unknown_keys() {
    let Some(f) = fixture(
        "ingest-nested",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let target = serde_json::to_string(&p["target"]).unwrap();
    let section = p["section_id"].as_str().unwrap();
    let digest = p["section_digest"].as_str().unwrap();
    let head = format!(
        "{{\"items\":[{{\"doc\":\"docs/adr/0001.md\",\"section_id\":\"{section}\",\"section_digest\":\"{digest}\","
    );
    let tail = "\"kind\":\"decision\",\"author\":\"tester\",\"bindings\":[";
    let bound = |binding: &str| format!("{head}\"record_id\":\"doc-n\",{tail}{binding}]}}]}}");
    let good = format!("{{\"target\":{target},\"relation\":\"documents\",\"rationale\":\"r\"}}");
    let cases = [
        // Two record_id keys in one item (raw text: a JSON builder would merge them).
        format!("{head}\"record_id\":\"doc-n\",\"record_id\":\"doc-m\",{tail}]}}]}}"),
        bound(&good.replace(
            "\"relation\":\"documents\"",
            "\"relation\":\"documents\",\"relation\":\"documents\"",
        )),
        bound(&good.replace("\"rationale\":\"r\"", "\"rationale\":\"r\",\"x\":1")),
        bound(&good.replace("\"target\":{", "\"target\":{\"x\":1,")),
        bound(&good.replace("\"byte_start\":", "\"byte_start\":0,\"byte_start\":")),
    ];
    for case in &cases {
        let out = f.ingest(case);
        assert_eq!(code_of(&out), Some(2), "{case}: {}", stderr_of(&out));
        let err = stderr_of(&out);
        assert!(
            err.contains("duplicate") || err.contains("unknown"),
            "{err}"
        );
        assert!(f.show("doc-n").is_none());
        assert!(!f.home.join("knowledge").join("r.db").exists());
    }
    // Control: the same input without the duplicate succeeds.
    let out = f.ingest(&bound(&good));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
}

#[test]
fn ingest_batch_with_invalid_second_item_writes_nothing() {
    let Some(f) = fixture(
        "ingest-batch",
        &[
            ("pkg/calc.py", CALC),
            ("docs/adr/0001.md", DOC),
            ("docs/bare.md", "#\nSee [add](../pkg/calc.py#L1).\n"),
        ],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let good = proposal_of(&report, "Storage@1", "add").clone();
    let bare = report["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["doc"] == "docs/bare.md")
        .expect("bare heading proposal")
        .clone();
    assert_eq!(bare["ingest_refusal"], "empty-heading");
    let bad_rationale = item_of(
        "bad-rationale",
        &good,
        "decision",
        vec![json!({"target": good["target"], "relation": "documents", "rationale": ""})],
    );
    let mut long_author = item_of("bad-author", &good, "decision", Vec::new());
    long_author["author"] = json!("a".repeat(300));
    let second_items = [
        item_of("bad-heading", &bare, "decision", Vec::new()),
        bad_rationale,
        long_author,
    ];
    for second in second_items {
        let id = second["record_id"].as_str().unwrap().to_string();
        let out = f.ingest(&input_of(vec![
            item_of("first", &good, "decision", Vec::new()),
            second,
        ]));
        assert_eq!(code_of(&out), Some(2), "{id}: {}", stderr_of(&out));
        assert!(
            f.show("first").is_none(),
            "{id}: first item must not be written"
        );
        assert_eq!(f.generation(), 0);
    }
    let out = f.ingest(&input_of(vec![item_of(
        "first",
        &good,
        "decision",
        Vec::new(),
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
}

#[test]
fn ingest_rejects_long_author_without_bindings() {
    let Some(f) = fixture(
        "ingest-author",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let mut create = item_of("long-create", &p, "decision", Vec::new());
    create["author"] = json!("a".repeat(300));
    let out = f.ingest(&input_of(vec![create]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(f.show("long-create").is_none());
    assert_eq!(f.generation(), 0);
    let mut at_limit = item_of("ok-author", &p, "decision", Vec::new());
    at_limit["author"] = json!("a".repeat(256));
    let out = f.ingest(&input_of(vec![at_limit]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let mut update = item_of("ok-author", &p, "decision", Vec::new());
    update["author"] = json!("a".repeat(300));
    update["expected_revision"] = json!(1);
    let out = f.ingest(&input_of(vec![update]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert_eq!(f.show("ok-author").unwrap()["result"]["revision"], 1);
}

#[test]
fn ingest_batch_with_duplicate_binding_writes_nothing() {
    let Some(f) = fixture(
        "ingest-dupbinding",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let b = binding_of(&p["target"], "documents add");
    let twice = item_of("second", &p, "decision", vec![b.clone(), b.clone()]);
    let out = f.ingest(&input_of(vec![
        item_of("first", &p, "decision", Vec::new()),
        twice,
    ]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("duplicate code binding in item"),
        "{}",
        stderr_of(&out)
    );
    assert!(f.show("first").is_none());
    assert_eq!(f.generation(), 0);
    let out = f.ingest(&input_of(vec![item_of("second", &p, "decision", vec![b])]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
}

#[test]
fn crlf_section_digest_matches_evidence_capture() {
    let crlf = "# CRLF\r\nSee [add](../pkg/calc.py#L1).\r\n";
    let Some(f) = fixture(
        "ingest-crlf",
        &[("pkg/calc.py", CALC), ("docs/crlf.md", crlf)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "CRLF@1", "add").clone();
    let out = f.ingest(&input_of(vec![item_of(
        "doc-crlf",
        &p,
        "decision",
        Vec::new(),
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let shown = f.show("doc-crlf").unwrap();
    assert_eq!(
        shown["result"]["evidence"][0]["digest"],
        p["section_digest"]
    );
}

#[test]
fn scan_and_ingest_scip_go_struct_target() {
    let Some(f) = fixture(
        "ingest-go",
        &[
            ("p/box.go", "package p\ntype Box struct {}\n"),
            ("docs/go.md", "# Go\n[box](../p/box.go#L2)\n"),
        ],
    ) else {
        return;
    };
    f.index();
    let sym = "scip-go test pkg 1.0.0 p/Box#";
    f.import(&[("p/box.go", vec![(sym, 1, 5, 8, DEF_ROLE)])]);
    let report = f.scan_json(&[]);
    let p = report["proposals"].as_array().unwrap()[0].clone();
    assert_eq!(p["target"]["definition"]["byte_start"], 15);
    assert_eq!(p["target"]["definition"]["byte_end"], 28);
    assert_eq!(p["target"]["anchor"]["kind"], "semantic");
    assert_eq!(p["target"]["anchor"]["byte_start"], 15);
    assert_eq!(p["target"]["anchor"]["byte_end"], 18);
    let again = f.scan_json(&[]);
    assert_eq!(again["proposals"][0], p, "second scan repeats the proposal");

    // Control: the syntax span in place of the semantic anchor is refused.
    let mut forged = item_of(
        "forged",
        &p,
        "decision",
        vec![binding_of(&p["target"], "box")],
    );
    forged["bindings"][0]["target"]["anchor"] = json!({
        "kind": "syntax-fallback",
        "precision": p["target"]["anchor"]["precision"],
        "line": p["target"]["anchor"]["line"],
        "col": p["target"]["anchor"]["col"],
        "byte_start": 15,
        "byte_end": 28,
        "symbol_uri": p["target"]["anchor"]["symbol_uri"],
        "display_name": p["target"]["anchor"]["display_name"],
        "source_digest": p["target"]["anchor"]["source_digest"],
    });
    let out = f.ingest(&input_of(vec![forged]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("binding-target-changed"),
        "{}",
        stderr_of(&out)
    );

    let out = f.ingest(&input_of(vec![item_of(
        "box",
        &p,
        "decision",
        vec![binding_of(&p["target"], "box")],
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
}

#[test]
fn ingest_binds_make_not_the_return_type() {
    let src = "struct Thing {};\nThing make() { return {}; }\n";
    let doc_text = "# Cpp\n[make](../src/thing.cpp#L2)\n";
    let Some(f) = fixture(
        "ingest-rettype",
        &[("src/thing.cpp", src), ("docs/cpp.md", doc_text)],
    ) else {
        return;
    };
    f.index();
    let thing = "scip-kt test pkg 1.0.0 src/thing.cpp/Thing#";
    let make = "scip-kt test pkg 1.0.0 src/thing.cpp/make().";
    f.import(&[(
        "src/thing.cpp",
        vec![
            (thing, 0, 7, 12, DEF_ROLE),
            (thing, 1, 0, 5, 0),
            (make, 1, 6, 10, DEF_ROLE),
        ],
    )]);
    let report = f.scan_json(&[]);
    let p = report["proposals"].as_array().unwrap()[0].clone();
    assert_eq!(p["target"]["definition"]["name"], "make");
    let out = f.ingest(&input_of(vec![item_of(
        "make-rec",
        &p,
        "decision",
        vec![binding_of(&p["target"], "make")],
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let text = f.at("src/thing.cpp", "2", "7").to_string();
    assert!(text.contains("\"record_id\":\"make-rec\""), "{text}");
}

#[test]
fn ingest_refuses_inner_nested_anchor_for_outer_definition() {
    let src = "def foo():\n    def foo():\n        return 1\n    return foo\n";
    // One document holds both links, so the inner link's reviewed section is available.
    let doc_text = "# Nest\n[outer](../pkg/nest.py#L1)\n[inner](../pkg/nest.py#L2)\n";
    let Some(f) = fixture(
        "ingest-nested-anchor",
        &[("pkg/nest.py", src), ("docs/nest.md", doc_text)],
    ) else {
        return;
    };
    f.index();
    let inner = "scip-kt test pkg 1.0.0 pkg/nest.py/inner().";
    f.import(&[("pkg/nest.py", vec![(inner, 1, 8, 11, DEF_ROLE)])]);
    let report = f.scan_json(&[]);
    let proposals = report["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "{report}");
    let inner_p = proposals[0].clone();
    assert_eq!(inner_p["target"]["anchor"]["byte_start"], 19);

    // The outer definition with the inner anchor: the proof refuses it.
    let mut forged = inner_p.clone();
    forged["target"]["definition"] = json!({
        "name": "foo",
        "kind": inner_p["target"]["definition"]["kind"],
        "line": 1,
        "col": 1,
        "byte_start": 0,
        "byte_end": 57,
    });
    let out = f.ingest(&input_of(vec![item_of(
        "forged",
        &forged,
        "decision",
        vec![binding_of(&forged["target"], "outer")],
    )]));
    assert_eq!(code_of(&out), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("binding-target-changed"),
        "{}",
        stderr_of(&out)
    );
    assert!(f.show("forged").is_none());

    // Control: the reviewed inner proposal binds.
    let out = f.ingest(&input_of(vec![item_of(
        "inner",
        &inner_p,
        "decision",
        vec![binding_of(&inner_p["target"], "inner")],
    )]));
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
}

// ---------------------------------------------------------------------------
// Task 4: `record_evidence` counts on code-binding objects.
// ---------------------------------------------------------------------------

#[test]
fn binding_reports_stale_record_evidence_after_doc_edit() {
    let Some(f) = fixture(
        "record-evidence-stale",
        &[("pkg/calc.py", CALC), ("docs/adr/0001.md", DOC)],
    ) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let report = f.scan_json(&[]);
    let p = proposal_of(&report, "Storage@1", "add").clone();
    let input = input_of(vec![item_of(
        "doc-storage",
        &p,
        "decision",
        vec![binding_of(&p["target"], "documents add")],
    )]);
    let out = f.ingest(&input);
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));

    let line = p["target"]["anchor"]["line"].to_string();
    let col = p["target"]["anchor"]["col"].to_string();
    let before = f.at("pkg/calc.py", &line, &col);
    assert_eq!(
        before["result"]["bindings"][0]["record_evidence"],
        json!({"total": 1, "fresh": 1, "stale": 0, "unresolved": 0}),
        "{before}"
    );
    assert_eq!(
        before["result"]["bindings"][0]["freshness"], "fresh",
        "{before}"
    );

    let packet_before = packet_binding(&f);
    assert_eq!(
        packet_before["record_evidence"],
        json!({"total": 1, "fresh": 1, "stale": 0, "unresolved": 0}),
        "{packet_before}"
    );
    assert_eq!(packet_before["freshness"], "fresh", "{packet_before}");

    f.write("docs/adr/0001.md", &DOC.replace("Add uses", "Add is used"));
    let after = f.at("pkg/calc.py", &line, &col);
    assert_eq!(
        after["result"]["bindings"][0]["record_evidence"],
        json!({"total": 1, "fresh": 0, "stale": 1, "unresolved": 0}),
        "{after}"
    );
    assert_eq!(
        after["result"]["bindings"][0]["freshness"], "fresh",
        "{after}"
    );
    let packet_after = packet_binding(&f);
    assert_eq!(
        packet_after["record_evidence"],
        json!({"total": 1, "fresh": 0, "stale": 1, "unresolved": 0}),
        "{packet_after}"
    );
    assert_eq!(packet_after["freshness"], "fresh", "{packet_after}");
}

/// The single knowledge binding under the packet anchor at the binding name (line 1, col 5). Fails loudly when
/// the anchor, its context, or the bindings path is absent.
fn packet_binding(f: &Fixture) -> Value {
    let out = cli(
        &f.home,
        &words(&[
            "packet",
            "--repo",
            "r",
            "--anchors",
            r#"[{"path":"pkg/calc.py","line":1,"col":5}]"#,
        ]),
    );
    assert_eq!(code_of(&out), Some(0), "{}", stderr_of(&out));
    let doc: Value = serde_json::from_slice(&out.stdout).expect("packet prints JSON");
    let anchor = &doc["anchors"][0];
    assert_eq!(anchor["status"], "resolved", "{doc}");
    let bindings = anchor["context"]["data"]["knowledge"]["bindings"]["bindings"]
        .as_array()
        .unwrap_or_else(|| panic!("packet anchor has no knowledge bindings path: {doc}"));
    assert_eq!(bindings.len(), 1, "{doc}");
    bindings[0].clone()
}

#[test]
fn binding_without_record_evidence_reports_zero_total() {
    let Some(f) = fixture("record-evidence-zero", &[("pkg/calc.py", CALC)]) else {
        return;
    };
    f.index();
    f.import(&[("pkg/calc.py", calc_defs())]);
    let created = f.knowledge_create(
        r#"{"id":"plain","kind":"decision","title":"Plain","body":"no evidence","status":"active","author":"a"}"#,
    );
    assert_eq!(code_of(&created), Some(0), "{}", stderr_of(&created));
    let bound = f.stdin_cli(
        &[
            "knowledge", "bind", "--repo", "r", "--path", "pkg/calc.py", "--line", "1", "--col",
            "5", "--input", "-",
        ],
        r#"{"record_id":"plain","record_revision":1,"relation":"documents","author":"a","rationale":"r","provenance":"p"}"#,
    );
    assert_eq!(code_of(&bound), Some(0), "{}", stderr_of(&bound));
    let page = f.at("pkg/calc.py", "1", "5");
    assert_eq!(
        page["result"]["bindings"][0]["record_evidence"],
        json!({"total": 0, "fresh": 0, "stale": 0, "unresolved": 0}),
        "{page}"
    );
}
