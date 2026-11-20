// SPDX-License-Identifier: Apache-2.0
#[path = "common/stage2.rs"]
mod stage2;

use serde_json::Value;

#[test]
fn client_selection_in_json_mode_leaves_profiles_and_project_unchanged() {
    let scratch = stage2::Scratch::new("agent-setup");
    let home = scratch.path().join("riltra-home");
    let root = scratch.path().join("project");
    let profiles = scratch.path().join("profiles");
    let codex = profiles.join("codex");
    let claude = profiles.join("claude");
    let opencode = profiles.join("opencode");
    for dir in [&codex, &claude, &opencode] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(codex.join("config.toml"), "# isolated profile\n").unwrap();
    std::fs::write(claude.join("settings.json"), "{}\n").unwrap();
    std::fs::write(opencode.join("opencode.json"), "{}\n").unwrap();
    stage2::project(&root);
    let project_before = stage2::snapshot(&root);
    let profiles_before = stage2::snapshot(&profiles);
    let opencode_config = opencode.join("opencode.json");
    let env = [
        ("CODEX_HOME", codex.as_path()),
        ("CLAUDE_CONFIG_DIR", claude.as_path()),
        ("XDG_CONFIG_HOME", profiles.as_path()),
        ("APPDATA", profiles.as_path()),
        ("OPENCODE_CONFIG", opencode_config.as_path()),
    ];

    for client in ["codex", "claude", "opencode"] {
        let args = [
            "start".into(),
            "--path".into(),
            root.display().to_string(),
            "--name".into(),
            "demo".into(),
            "--client".into(),
            client.into(),
            "--json".into(),
        ];
        let result = stage2::run(&home, &args, &env);
        assert!(
            result.status.success(),
            "{client}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["repo"], "demo");
        assert_eq!(report["snapshot"]["status"], "no-index");
    }

    assert!(
        !home.exists(),
        "client selection must not create RILTRA_HOME"
    );
    assert_eq!(
        stage2::snapshot(&profiles),
        profiles_before,
        "client profiles changed"
    );
    assert_eq!(stage2::snapshot(&root), project_before, "project changed");
}
