// SPDX-License-Identifier: Apache-2.0
//! End-to-end CLI tests: each case spawns the real binary with an isolated
//! `RILTRA_HOME` so the developer's registry is never touched.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-cli-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
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

use std::path::Path;

fn mkrepo(home: &Path, name: &str) -> PathBuf {
    let d = home.join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_fixture(dir: &Path) {
    let g = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("run git")
            .success());
    };
    g(&["init", "-b", "main", "."]);
    g(&["config", "user.email", "t@t"]);
    g(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    g(&["add", "a.txt"]);
    g(&["commit", "-m", "init"]);
}

// ---------------------------------------------------------------------------
// `riltra agent-block`. The expected block is typed here from the PR 10 brief;
// it is never read back from the binary or from the docs file.
// ---------------------------------------------------------------------------

const AGENT_BLOCK: &str = r#"## Code intelligence (Riltra)

Riltra is a local code map, not a source of truth. Confirm answers against the cited source; index freshness and evidence labels limit what an answer establishes.

- `riltra mcp` lists only a core tool set by default. The tools this block names that are outside it (`riltra_health`, `riltra_repo_status`, `riltra_code_packet`, `riltra_code_changes` and `riltra_knowledge_at_anchor`) are listed only by `riltra mcp --all-tools`. Add `--all-tools` to the client's MCP configuration so the model can call them; a client that lets the model call only listed tools cannot reach them otherwise. Calling by name helps only a client that calls by name directly.
- For a new folder, run `riltra start [--path <directory>] [--name <name>]` in a terminal. The wizard shows readiness and the effective `RILTRA_HOME`; registration and indexing each require a separate default-no approval. `--client` selects an integration only; it does not approve writes.
- In the wizard, choose a definition from the overview menu. Riltra builds a packet from that selection, so do not guess line or column coordinates. Entry points are candidates from indexed definitions or conventional locations, not proof of runtime flow. Document and manifest excerpts are labelled live evidence.
- `fresh` describes the selected index snapshot. On `stale`, `unknown`, `source-changed`, or a changed menu snapshot, reindex and select again.
- `no-index` needs an approved index. `unsupported-schema:<found>` means overview left the store untouched; compare `<found>` with `riltra_health.versions.code_index_schema` (the supported schema; health is environment-wide and has no repo recovery instruction). Call `riltra_repo_status` for this repo to distinguish `schema_outdated` (`<found>` is lower) from `schema_ahead` (`<found>` is higher). For `schema_outdated`, follow that response's CLI migration instruction (`riltra index status --repo <name>`). That command can migrate and write the store, so get the user's separate approval before running it; then verify the result and do not assume migration succeeded. For `schema_ahead`, leave the store untouched and use a Riltra build that supports it if one is available; do not reindex or rebuild it with a build that reports it as newer. If no definitions are found, do not claim the project language is unsupported.
- JSON or nonterminal `riltra start` prints a read-only overview; it does not register, index, set up a client, or produce a selected-definition packet. `riltra overview --repo <name>` is also read-only.
- Setup evidence is separate: `configuration`, `probe`, and `connection` are not interchangeable. `probe_scope: in_process_handler` tests this process's handler over bounded in-memory transport only; it does not test the installed executable, OS stdio, or a client connection. A connected client is not inferred.
- If setup says `manual_required`, inspect the named client's effective configuration and preserve any existing entry. Substitute absolute paths and follow the client's current documentation before running a command. Codex: `codex mcp add riltra --env RILTRA_HOME=<absolute-home> -- <absolute-riltra-binary> mcp`. Claude Code: `claude mcp add --env RILTRA_HOME=<absolute-home> --transport stdio --scope user riltra -- <absolute-riltra-binary> mcp`. OpenCode: `opencode mcp add riltra --global --env RILTRA_HOME=<absolute-home> -- <absolute-riltra-binary> mcp`; this preview is not proof that every OpenCode version or configuration layer is safe to modify. OpenCode automatic application remains disabled because native isolation and effective configuration layers have not been established for this task. See the [Codex](https://developers.openai.com/codex/mcp/), [Claude Code](https://code.claude.com/docs/en/mcp), and [OpenCode](https://opencode.ai/docs/mcp-servers/) documentation.
- For manual recovery, prepare the project with `riltra init --repo <path> --name <name>` and `riltra index --repo <name>`, then call `riltra overview --repo <name>`, choose a definition from its evidence, and pass its name/path to `riltra packet`. Recheck freshness before relying on the packet.
- Find definitions by name with `riltra_code_search` (definition names only; not string literals, error messages or imported names) or by description with `riltra_code_query`. For literal text use your own text search. A query result has no column: pass its `name` as `target` and `path` as `in`.
- For several selected locations, call `riltra_code_packet` with one to five explicit anchors and inspect every status; exit 0 does not mean all resolved or that a change is safe. If source is `source-changed`, reindex and select the anchor again. Use `follow_up.context` with `riltra_context`. When `follow_up.source` is present, validate repository containment, read the full file, compare its 16-digit lowercase FNV-1a 64 digest, then extract the half-open `requested_range` from that same buffer. Do not hash only the range; FNV is a change detector, not cryptographic proof.
- If a name is ambiguous or not found, inspect candidates and source; do not choose on the user's behalf. Before editing a symbol, run `riltra_code_impact`; after editing, run `riltra_code_changes` and inspect touched definitions.
- `UNKNOWN` or `partial` is not a clean check, and an empty result means “not established,” never “nothing exists.” `POTENTIAL_CALL` is a lead to verify in source, not proof of a call.
- Before changing code, call `riltra_knowledge_at_anchor`. Report a stored note as `Unverified` with its revision, not as established fact. Say which Riltra answers you used and which labels they carried.
"#;

const AGENT_BLOCK_MARKERS: [&str; 10] = [
    "`riltra start [--path <directory>] [--name <name>]`",
    "`unsupported-schema:<found>`",
    "`riltra_health.versions.code_index_schema`",
    "Call `riltra_repo_status` for this repo",
    "`schema_ahead` (`<found>` is higher)",
    "separate approval before running it",
    "`probe_scope: in_process_handler`",
    "OpenCode automatic application remains disabled",
    "`UNKNOWN` or `partial` is not a clean check",
    "`riltra_knowledge_at_anchor`.",
];

/// The fenced block under the snippet heading of the agent setup guide.
fn agent_setup_snippet() -> &'static str {
    const DOC: &str = include_str!("../../../docs/guide/agent-setup.md");
    let heading = DOC
        .find("## A snippet for your agent's instructions")
        .expect("snippet heading");
    let after = &DOC[heading..];
    let open = "```markdown\n";
    let start = after.find(open).expect("snippet fence") + open.len();
    let body = &after[start..];
    let end = body.find("\n```\n").expect("closing fence");
    &body[..=end]
}

/// Drive-letter paths, UNC shares, /home/ and /Users/ roots, and GitNexus.
fn leaks_local_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    let drive_letter = bytes.windows(3).enumerate().any(|(i, w)| {
        let is_url_scheme = (i >= 3 && bytes.get(i - 3..i + 4) == Some(&b"http://"[..]))
            || (i >= 4 && bytes.get(i - 4..i + 4) == Some(&b"https://"[..]));
        w[0].is_ascii_alphabetic()
            && w[1] == b':'
            && (w[2] == b'\\' || w[2] == b'/')
            && !is_url_scheme
    });
    let lower = text.to_ascii_lowercase();
    drive_letter
        || text.contains("\\\\")
        || lower.contains("/home/")
        || lower.contains("/users/")
        || lower.contains("gitnexus")
}

/// Every file under `root` with its bytes, sorted by path.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                files.push((path, bytes));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn agent_block_prints_the_frozen_block() {
    let h = home("agent-block");
    let (code, out, err) = run(&h, &["agent-block"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, AGENT_BLOCK);
    assert!(out.ends_with('\n') && !out.ends_with("\n\n"));
    assert!(err.is_empty(), "stderr must stay empty: {err}");
}

#[test]
fn agent_block_markers_occur_exactly_once() {
    let h = home("agent-block-markers");
    let (_, out, _) = run(&h, &["agent-block"]);
    for marker in AGENT_BLOCK_MARKERS {
        assert_eq!(out.matches(marker).count(), 1, "marker: {marker}");
    }
}

#[test]
fn agent_block_has_no_local_path_or_private_name() {
    let h = home("agent-block-leaks");
    let (_, out, _) = run(&h, &["agent-block"]);
    assert!(!leaks_local_path(&out), "leak in output: {out}");
}

#[test]
fn agent_block_leak_check_catches_planted_leaks_only() {
    // Built from pieces: a tracked file must not contain a local-path literal, even a planted one.
    let planted_leaks = [
        ["worktree: ", "C", ":\\", "Users", "\\dev", "\\repo"].concat(),
        ["located at ", "D", ":/", "work/repo"].concat(),
        ["prefix", "C", ":\\", "Users", "\\dev", "\\repo"].concat(),
        ["repo1", "D", ":/", "work/repo"].concat(),
        ["\\\\", "server", "\\share"].concat(),
        ["/ho", "me/dev/repo"].concat(),
        ["/Us", "ers/dev/repo"].concat(),
        "uses GitNexus".to_string(),
    ];
    for planted in &planted_leaks {
        assert!(leaks_local_path(planted), "missed: {planted}");
    }
    assert!(!leaks_local_path(
        "See https://example.test/docs/mcp-servers/ for current guidance"
    ));
    assert!(!leaks_local_path(
        "See http://example.test/docs/ for guidance"
    ));
    let url_with_local_path = concat!(
        "See https://example.test/files/",
        "C",
        ":/",
        "Users/dev/repo",
    );
    assert!(leaks_local_path(url_with_local_path));
    assert!(!leaks_local_path("Riltra: cited/source, ratio 1/2"));
}

#[test]
fn agent_block_refuses_any_argument() {
    let h = home("agent-block-args");
    for args in [
        vec!["agent-block", "extra"],
        vec!["agent-block", "--repo", "x"],
    ] {
        let (code, out, err) = run(&h, &args);
        assert_eq!(code, 2, "args: {args:?}");
        assert!(out.is_empty(), "stdout must be empty: {out}");
        assert!(!err.is_empty(), "usage error expected on stderr");
    }
}

#[test]
fn agent_block_is_print_only() {
    if !git_available() {
        return;
    }
    let h = home("agent-block-print-only");
    let repo = mkrepo(&h, "work");
    git_fixture(&repo);
    assert_eq!(run(&h, &["init", "--repo", repo.to_str().unwrap()]).0, 0);
    let home_before = snapshot(&h);
    let (code, _, _) = run(&h, &["agent-block"]);
    assert_eq!(code, 0);
    assert_eq!(snapshot(&h), home_before, "RILTRA_HOME or repo changed");

    let missing = std::env::temp_dir().join(format!(
        "riltra-cli-agent-block-missing-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&missing);
    let (code, _, _) = run(&missing, &["agent-block"]);
    assert_eq!(code, 0);
    assert!(!missing.exists(), "agent-block created RILTRA_HOME");
}

#[test]
fn agent_block_matches_the_agent_setup_snippet() {
    let h = home("agent-block-sync");
    let (code, out, _) = run(&h, &["agent-block"]);
    assert_eq!(code, 0);
    assert_eq!(agent_setup_snippet(), out);
}

#[test]
fn the_usage_lists_agent_block() {
    let h = home("agent-block-usage");
    let (code, out, _) = run(&h, &["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("  riltra agent-block\n"), "got: {out}");
}

#[test]
fn init_and_list_round_trip() {
    let h = home("roundtrip");
    let target = mkrepo(&h, "work");
    let (code, out, _) = run(&h, &["init", "--repo", target.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"name\":\"work\""));
    assert!(out.contains("\"registered\":true"));
    let (code, out, _) = run(&h, &["init", "--repo", target.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"registered\":false"));
    let (code, out, _) = run(&h, &["repo", "list"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"name\":\"work\""));
    assert!(out.starts_with("{\"repos\":["));
}

#[test]
fn init_rejects_missing_path_and_bad_name() {
    let h = home("init-err");
    let missing = h.join("nope");
    let (code, _, err) = run(&h, &["init", "--repo", missing.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(err.contains("error"));
    let target = mkrepo(&h, "work");
    let (code, _, _) = run(
        &h,
        &[
            "init",
            "--repo",
            target.to_str().unwrap(),
            "--name",
            "../evil",
        ],
    );
    assert_eq!(code, 2);
}

#[test]
fn status_unknown_repo_exits_3_without_stdout() {
    let h = home("unknown");
    let (code, out, err) = run(&h, &["repo", "status", "--repo", "nope"]);
    assert_eq!(code, 3);
    assert!(out.is_empty());
    assert!(err.contains("unknown repository"));
}

#[test]
fn status_plain_dir_reports_absent_git() {
    let h = home("plain");
    let target = mkrepo(&h, "plain");
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "plain"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"present\":false"));
    assert!(out.contains("\"ignore\":"));
}

#[test]
fn status_git_repo_reports_head_and_branch() {
    if !git_available() {
        return;
    }
    let h = home("gitrep");
    let target = mkrepo(&h, "code");
    git_fixture(&target);
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "code"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"present\":true"));
    assert!(out.contains("\"valid\":true"));
    assert!(out.contains("\"branch\":\"main\""));
    assert!(out.contains("\"dirty\":false"));
    assert!(out.contains("\"head\":\""));
}

#[test]
fn status_corrupt_git_reports_invalid_with_exit_3() {
    let h = home("corrupt");
    let target = mkrepo(&h, "broken");
    std::fs::create_dir_all(target.join(".git")).unwrap();
    std::fs::write(target.join(".git").join("HEAD"), "garbage").unwrap();
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "broken"]);
    assert_eq!(code, 3);
    assert!(out.contains("\"valid\":false"));
}

#[test]
fn doctor_healthy_dir_passes_and_env_doctor_reports_scope() {
    let h = home("doctor");
    let target = mkrepo(&h, "ok");
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, out, _) = run(&h, &["doctor", "--repo", "ok"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"pass\":true"));
    let (code, out, _) = run(&h, &["doctor"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"scope\":\"environment\""));
}

#[test]
fn repos_are_isolated_by_name() {
    let h = home("isolated");
    let a = mkrepo(&h, "a");
    let b = mkrepo(&h, "b");
    std::fs::write(a.join("same.txt"), "A").unwrap();
    std::fs::write(b.join("same.txt"), "B").unwrap();
    assert_eq!(
        run(&h, &["init", "--repo", a.to_str().unwrap(), "--name", "ra"]).0,
        0
    );
    assert_eq!(
        run(&h, &["init", "--repo", b.to_str().unwrap(), "--name", "rb"]).0,
        0
    );
    let (_, out_a, _) = run(&h, &["repo", "status", "--repo", "ra"]);
    let (_, out_b, _) = run(&h, &["repo", "status", "--repo", "rb"]);
    assert!(!out_a.contains("same.txt")); // status carries metadata, never file content
    assert_ne!(out_a, out_b);
}

#[test]
fn status_rejects_moved_root() {
    let h = home("moved");
    let target = mkrepo(&h, "work");
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    std::fs::rename(&target, h.join("gone")).unwrap();
    let (code, out, err) = run(&h, &["repo", "status", "--repo", "work"]);
    assert_eq!(code, 3);
    assert!(out.is_empty());
    assert!(err.contains("missing"));
}

#[test]
fn status_rejects_symlink_retarget_outside_root() {
    let h = home("retarget");
    let target = mkrepo(&h, "real");
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let outside = mkrepo(&h, "outside");
    std::fs::remove_dir(&target).unwrap();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&outside, &target).is_ok();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(&outside, &target).is_ok();
    if !made {
        return; // Privilege skip: NOT junction evidence (see junction test below).
    }
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "real"]);
    assert_eq!(code, 3);
    assert!(
        out.is_empty(),
        "must not fingerprint outside the recorded root"
    );
}

#[test]
fn status_rejects_windows_junction_retarget() {
    if cfg!(not(windows)) {
        return; // Junctions are Windows-only; unix coverage is the symlink test above.
    }
    let h = home("junction");
    let target = mkrepo(&h, "real");
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let outside = mkrepo(&h, "outside");
    std::fs::remove_dir(&target).unwrap();
    // mklink /J needs no elevation, so failure here is a real failure, not a skip.
    let mk = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&target)
        .arg(&outside)
        .output()
        .expect("run mklink");
    assert!(
        mk.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&mk.stderr)
    );
    assert!(target.is_dir(), "junction must resolve as a directory");
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "real"]);
    assert_eq!(code, 3);
    assert!(
        out.is_empty(),
        "must not fingerprint outside the recorded root"
    );
}

#[test]
fn init_fails_when_registry_home_is_a_file() {
    // Portable IO-failure path: the registry directory cannot be created.
    let h = home("reghome");
    let file = h.join("file");
    std::fs::write(&file, "x").unwrap();
    let target = mkrepo(&h, "work");
    let (code, _, err) = run(&file, &["init", "--repo", target.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(err.contains("error"));
}

#[cfg(unix)]
#[test]
fn unreadable_git_metadata_is_invalid() {
    use std::os::unix::fs::PermissionsExt;
    if !git_available() {
        return;
    }
    let h = home("noperm");
    let target = mkrepo(&h, "code");
    git_fixture(&target);
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    std::fs::set_permissions(target.join(".git"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "code"]);
    assert_eq!(code, 3);
    assert!(out.contains("\"valid\":false"));
}

#[test]
fn name_clash_across_roots_is_rejected() {
    let h = home("clash");
    let a = mkrepo(&h, "a");
    let b = mkrepo(&h, "b");
    assert_eq!(
        run(
            &h,
            &["init", "--repo", a.to_str().unwrap(), "--name", "dup"]
        )
        .0,
        0
    );
    let (code, _, err) = run(
        &h,
        &["init", "--repo", b.to_str().unwrap(), "--name", "dup"],
    );
    assert_eq!(code, 3);
    assert!(err.contains("already registered"));
    let (_, out, _) = run(&h, &["repo", "status", "--repo", "dup"]);
    let canon_a = a
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "\\\\");
    assert!(out.contains(&canon_a));
    assert!(!out.contains(
        &b.canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "\\\\")
    ));
}

#[test]
fn status_corrupt_gitfile_reports_invalid_with_exit_3() {
    let h = home("corruptfile");
    let target = mkrepo(&h, "broken");
    std::fs::write(target.join(".git"), "garbage").unwrap();
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "broken"]);
    assert_eq!(code, 3);
    assert!(out.contains("\"valid\":false"));
}

fn set_repo_config(dir: &Path, key: &str, value: &str) {
    assert!(Command::new("git")
        .args(["config", key, value])
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("git config")
        .success());
}

#[test]
fn status_and_doctor_never_run_fsmonitor() {
    if !git_available() {
        return;
    }
    let h = home("fsmonitor");
    let target = mkrepo(&h, "code");
    git_fixture(&target);
    let marker = target.join("fsmonitor-marker");
    #[cfg(windows)]
    {
        std::fs::write(
            target.join("w.ps1"),
            "New-Item -Path $args[0] -ItemType File -Force | Out-Null\n",
        )
        .unwrap();
        let script = target.join("w.ps1").to_string_lossy().replace('\\', "/");
        let goal = marker.to_string_lossy().replace('\\', "/");
        set_repo_config(
            &target,
            "core.fsmonitor",
            &format!("powershell -NoProfile -File {script} {goal}"),
        );
    }
    #[cfg(unix)]
    {
        set_repo_config(
            &target,
            "core.fsmonitor",
            &format!("sh -c 'touch \"$1\"' sh {}", marker.to_string_lossy()),
        );
    }
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (c1, _, _) = run(&h, &["repo", "status", "--repo", "code"]);
    assert_eq!(c1, 0);
    let (c2, _, _) = run(&h, &["doctor", "--repo", "code"]);
    assert_eq!(c2, 0);
    assert!(
        !marker.exists(),
        "fsmonitor must not run via status or doctor"
    );
}

#[test]
fn status_ignores_symlinked_ignore_outside_root() {
    let h = home("ignore-link");
    let target = mkrepo(&h, "work");
    let outside = mkrepo(&h, "outside");
    std::fs::write(outside.join("rules.txt"), "zzz\n").unwrap();
    let link = target.join(".riltraignore");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(outside.join("rules.txt"), &link).is_ok();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(outside.join("rules.txt"), &link).is_ok();
    if !made {
        return; // Privilege skip on Windows; unix runs unprivileged (see CI).
    }
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, out, _) = run(&h, &["repo", "status", "--repo", "work"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"ignore\":{\"file\":false,\"rules\":0}"));
}

/// Index JSON carries wall-clock `timings_ms`, which differ between runs.
/// Removes that object so the rest of the output can be compared byte-for-byte.
fn without_timings(json: &str) -> String {
    let key = "\"timings_ms\":";
    let start = json.find(key).expect("index output has timings_ms");
    let object_len = json[start..].find('}').expect("timings_ms object closes") + 1;
    format!("{}{}", &json[..start], &json[start + object_len..])
}

fn progress_repo(h: &Path) -> PathBuf {
    let target = mkrepo(h, "tiny");
    std::fs::write(target.join("a.py"), "def f():\n    return 1\n").unwrap();
    target
}

#[test]
fn scan_progress_keeps_stdout_and_writes_stderr_only_with_flag() {
    let h = home("scan-progress");
    let target = progress_repo(&h);
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, plain_out, plain_err) = run(&h, &["scan", "--repo", "tiny"]);
    assert_eq!(code, 0);
    let (code, prog_out, prog_err) = run(&h, &["scan", "--repo", "tiny", "--progress"]);
    assert_eq!(code, 0);
    assert_eq!(plain_out, prog_out);
    assert!(plain_err.is_empty(), "stderr without flag: {plain_err:?}");
    assert!(
        prog_err
            .lines()
            .any(|l| l.starts_with("progress: scan: done")),
        "stderr with flag: {prog_err:?}"
    );
}

#[test]
fn index_progress_keeps_stdout_and_writes_stderr_only_with_flag() {
    // Each run gets its own RILTRA_HOME so both builds are full rebuilds.
    let plain_home = home("index-plain");
    let prog_home = home("index-progress");
    let target = progress_repo(&plain_home);
    let target_arg = target.to_str().unwrap();
    assert_eq!(run(&plain_home, &["init", "--repo", target_arg]).0, 0);
    assert_eq!(run(&prog_home, &["init", "--repo", target_arg]).0, 0);
    let (code, plain_out, plain_err) = run(&plain_home, &["index", "--repo", "tiny"]);
    assert_eq!(code, 0);
    let (code, prog_out, prog_err) = run(&prog_home, &["index", "--repo", "tiny", "--progress"]);
    assert_eq!(code, 0);
    assert!(plain_out.contains("\"timings_ms\":"));
    assert_eq!(without_timings(&plain_out), without_timings(&prog_out));
    assert!(plain_err.is_empty(), "stderr without flag: {plain_err:?}");
    assert!(
        prog_err
            .lines()
            .any(|l| l.starts_with("progress: index: done")),
        "stderr with flag: {prog_err:?}"
    );
}

#[test]
fn index_status_rejects_progress_flag() {
    let h = home("status-progress");
    let target = progress_repo(&h);
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    let (code, out, _) = run(&h, &["index", "status", "--repo", "tiny", "--progress"]);
    assert_eq!(code, 2);
    assert!(out.is_empty());
}

#[test]
fn index_exits_3_while_the_writer_lock_is_held() {
    let h = home("index-writer-busy");
    let target = progress_repo(&h);
    assert_eq!(run(&h, &["init", "--repo", target.to_str().unwrap()]).0, 0);
    assert_eq!(run(&h, &["index", "--repo", "tiny"]).0, 0);
    // Hold the writer lock from outside the process, as a live rebuild would.
    let lock = h.join("index").join("tiny.db.lock");
    let held = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock)
        .unwrap();
    held.try_lock().expect("test takes the writer lock");
    let (code, out, err) = run(&h, &["index", "--repo", "tiny"]);
    assert_eq!(code, 3, "stderr: {err:?}");
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(err.contains("index-busy:"), "stderr: {err:?}");
    drop(held);
    assert_eq!(run(&h, &["index", "--repo", "tiny"]).0, 0);
}

// ---------------------------------------------------------------------------
// `riltra grep`. The golden is written by hand from GREP_FIXTURE: 65 bytes;
// `alpha()` is on line 1 (inside alpha) and line 6 (inside beta, lines 5-7).
// ---------------------------------------------------------------------------

const GREP_FIXTURE: &[u8] =
    b"def alpha():\n    return 1\n\n\ndef beta():\n    alpha()\n    return 2\n";

/// Repository `repo` with one Python file, initialized and indexed.
fn grep_fixture_indexed(h: &Path) -> PathBuf {
    let repo = mkrepo(h, "repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/app.py"), GREP_FIXTURE).unwrap();
    assert_eq!(
        run(
            h,
            &["init", "--repo", repo.to_str().unwrap(), "--name", "repo"]
        )
        .0,
        0
    );
    let (code, _, err) = run(h, &["index", "--repo", "repo"]);
    assert_eq!(code, 0, "stderr: {err:?}");
    repo
}

#[test]
fn grep_cli_matches_golden() {
    let h = home("grep-golden");
    grep_fixture_indexed(&h);
    let (code, out, err) = run(&h, &["grep", "--repo", "repo", "--pattern", "alpha()"]);
    assert_eq!(code, 0, "stderr: {err:?}");
    assert_eq!(out, include_str!("golden/grep-basic.json"));
}

#[test]
fn grep_cli_invalid_regex_exits_2() {
    let h = home("grep-regex");
    grep_fixture_indexed(&h);
    let (code, out, err) = run(&h, &["grep", "--repo", "repo", "--pattern", "(", "--regex"]);
    assert_eq!(code, 2, "stderr: {err:?}");
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(err.contains("pattern does not compile"), "stderr: {err:?}");
}

#[test]
fn grep_cli_bad_limit_exits_2() {
    let h = home("grep-limit");
    grep_fixture_indexed(&h);
    for limit in ["0", "201"] {
        let (code, out, _) = run(
            &h,
            &[
                "grep",
                "--repo",
                "repo",
                "--pattern",
                "alpha",
                "--limit",
                limit,
            ],
        );
        assert_eq!(code, 2, "limit {limit}");
        assert!(out.is_empty(), "limit {limit}: {out:?}");
    }
}

#[test]
fn grep_cli_unregistered_exits_3() {
    let h = home("grep-unknown");
    let (code, out, err) = run(&h, &["grep", "--repo", "nope", "--pattern", "alpha"]);
    assert_eq!(code, 3, "stderr: {err:?}");
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(err.contains("unknown repository"), "stderr: {err:?}");
}

#[test]
fn grep_cli_missing_root_exits_3() {
    let h = home("grep-moved");
    let repo = grep_fixture_indexed(&h);
    std::fs::rename(&repo, h.join("moved")).unwrap();
    let (code, out, err) = run(&h, &["grep", "--repo", "repo", "--pattern", "alpha"]);
    assert_eq!(code, 3, "stderr: {err:?}");
    assert!(out.is_empty(), "stdout: {out:?}");
    assert!(
        err.contains("registered root is missing"),
        "stderr: {err:?}"
    );
}

#[test]
fn grep_cli_without_index_searches_and_creates_nothing() {
    let h = home("grep-noindex");
    let repo = mkrepo(&h, "repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/app.py"), GREP_FIXTURE).unwrap();
    assert_eq!(
        run(
            &h,
            &["init", "--repo", repo.to_str().unwrap(), "--name", "repo"]
        )
        .0,
        0
    );
    let (code, out, err) = run(&h, &["grep", "--repo", "repo", "--pattern", "alpha()"]);
    assert_eq!(code, 0, "stderr: {err:?}");
    assert!(out.contains("\"in_reason\":\"no-index\""), "stdout: {out}");
    assert!(out.contains("\"snapshot\":null"), "stdout: {out}");
    assert!(
        !h.join("index").join("repo.db").exists(),
        "grep must not create an index"
    );
}
