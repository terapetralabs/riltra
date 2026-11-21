// SPDX-License-Identifier: Apache-2.0
#[path = "common/stage2.rs"]
mod stage2;

use serde_json::Value;

#[test]
fn overview_cli_reports_no_index_without_writing_storage() {
    let scratch = stage2::Scratch::new("overview");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = scratch.path().join("project");
    stage2::project(&root);
    let init = stage2::run(
        &home,
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

    let before = stage2::snapshot(&home);
    let run_overview = || {
        stage2::run(
            &home,
            &["overview".into(), "--repo".into(), "demo".into()],
            &[],
        )
    };
    let first = run_overview();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run_overview();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let text = String::from_utf8(first.stdout).unwrap();
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["repo"], "demo");
    assert_eq!(report["snapshot"]["status"], "no-index");
    assert_eq!(text, String::from_utf8(second.stdout).unwrap());
    assert_eq!(
        stage2::snapshot(&home),
        before,
        "overview must stay read-only"
    );
}

#[test]
fn overview_lists_monorepo_member_manifests_from_the_index() {
    let scratch = stage2::Scratch::new("overview-monorepo");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/project_orientation/monorepo");
    let init = stage2::run(
        &home,
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
    let index = stage2::run(
        &home,
        &["index".into(), "--repo".into(), "demo".into()],
        &[],
    );
    assert!(
        index.status.success(),
        "{}",
        String::from_utf8_lossy(&index.stderr)
    );
    let overview = stage2::run(
        &home,
        &["overview".into(), "--repo".into(), "demo".into()],
        &[],
    );
    assert!(
        overview.status.success(),
        "{}",
        String::from_utf8_lossy(&overview.stderr)
    );
    let text = String::from_utf8(overview.stdout).unwrap();
    let report: Value = serde_json::from_str(&text).unwrap();
    let sources = report["build_test_sources"].as_array().unwrap();
    for path in ["crates/alpha/Cargo.toml", "crates/beta/Cargo.toml"] {
        assert!(
            sources
                .iter()
                .any(|source| source["path"] == path && source["status"] == "available"),
            "{path} is not an available build_test_source"
        );
    }
}
