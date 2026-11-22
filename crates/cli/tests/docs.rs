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

use serde_json::Value;
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
