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
