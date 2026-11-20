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
