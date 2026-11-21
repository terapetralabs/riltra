// SPDX-License-Identifier: Apache-2.0
#[path = "common/stage2.rs"]
mod stage2;

use serde_json::Value;

#[test]
fn start_json_orients_a_new_folder_without_registering_or_indexing() {
    let scratch = stage2::Scratch::new("start");
    let home = scratch.path().join("home");
    let root = scratch.path().join("project");
    stage2::project(&root);
    let before = stage2::snapshot(&root);
    let args = [
        "start".into(),
        "--path".into(),
        root.display().to_string(),
        "--name".into(),
        "demo".into(),
        "--json".into(),
    ];
    let result = stage2::run(&home, &args, &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["repo"], "demo");
    assert_eq!(report["snapshot"]["status"], "no-index");
    assert!(!home.exists(), "JSON mode must not create RILTRA_HOME");
    assert_eq!(
        stage2::snapshot(&root),
        before,
        "start must not modify the project"
    );
}
