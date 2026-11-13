// SPDX-License-Identifier: Apache-2.0
//! Git fingerprint via read-only plumbing (`rev-parse`, `status --porcelain`).
//! Never executes repository code; `GIT_TERMINAL_PROMPT=0` blocks auth prompts
//! and `--no-optional-locks` keeps status read-only.

use std::path::Path;

use crate::json::escape;

#[derive(Debug, Clone, PartialEq)]
pub struct GitInfo {
    pub present: bool,
    pub valid: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
    pub untracked: u32,
    pub toplevel: Option<String>,
    pub worktree: bool,
}

pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Read-only posture: our commands (rev-parse/status/branch) must never
        // execute repo-configured helpers. `status` invokes core.fsmonitor when
        // set; an empty override disables it for our subprocesses only
        // (verified by probe + status_never_runs_repo_configured_fsmonitor).
        // No other key needs overriding: none of our commands run hooks,
        // pagers, editors, or network helpers. Re-audit when adding commands.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn absent() -> GitInfo {
    GitInfo {
        present: false,
        valid: false,
        head: None,
        branch: None,
        dirty: false,
        untracked: 0,
        toplevel: None,
        worktree: false,
    }
}

/// How this root claims (or not) to be a repository.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DotGit {
    /// Real directory: this root IS the worktree top, nothing else qualifies.
    Dir,
    /// File gitlink (worktree/submodule) or symlink/junction: follow git.
    Link,
}

fn canonical_str(p: &Path) -> Option<String> {
    std::fs::canonicalize(p)
        .ok()
        .map(|c| c.to_string_lossy().to_string())
}

/// Fingerprint the repository claimed at `root`. Never borrows a parent
/// repository: without a `.git` entry the answer is absent even inside a
/// larger work tree, and a directory `.git` only validates when the discovered
/// toplevel is this root itself (corrupt metadata or parent bleed → invalid).
pub fn fingerprint_using(root: &Path, run: &dyn Fn(&Path, &[&str]) -> Option<String>) -> GitInfo {
    let claim = match std::fs::symlink_metadata(root.join(".git")) {
        Err(_) => return absent(),
        Ok(m) if m.file_type().is_dir() => DotGit::Dir,
        Ok(_) => DotGit::Link,
    };
    let canon_root = match canonical_str(root) {
        Some(c) => c,
        None => {
            return GitInfo {
                present: true,
                ..absent()
            }
        }
    };
    let discovered =
        run(root, &["rev-parse", "--show-toplevel"]).and_then(|s| canonical_str(Path::new(&s)));
    let toplevel = match (claim, discovered) {
        (DotGit::Dir, Some(t)) if t == canon_root => Some(t),
        (DotGit::Link, Some(t)) => Some(t),
        _ => None,
    };
    let Some(toplevel) = toplevel else {
        return GitInfo {
            present: true,
            ..absent()
        };
    };
    let head = run(root, &["rev-parse", "HEAD"])
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()));
    let branch = run(root, &["branch", "--show-current"]).filter(|s| !s.is_empty());
    let status = run(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    );
    // No status output without a successful run: an unverifiable work tree is
    // never reported clean, and the fingerprint is not valid (a failed status
    // used to fall through as clean + valid).
    let (dirty, untracked, status_ok) = match status {
        None => (false, 0, false),
        Some(text) => {
            let mut dirty = false;
            let mut untracked = 0u32;
            for line in text.lines() {
                if line.starts_with("??") {
                    untracked += 1;
                } else if !line.is_empty() {
                    dirty = true;
                }
            }
            (dirty, untracked, true)
        }
    };
    let git_dir =
        run(root, &["rev-parse", "--absolute-git-dir"]).and_then(|s| canonical_str(Path::new(&s)));
    // `--git-common-dir` may print relative (main repo) or absolute
    // (linked worktree); resolve against the root either way.
    let common_dir =
        run(root, &["rev-parse", "--git-common-dir"]).and_then(|s| canonical_str(&root.join(&s)));
    let worktree = match (git_dir, common_dir) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    };
    GitInfo {
        present: true,
        valid: status_ok,
        head,
        branch,
        dirty,
        untracked,
        toplevel: Some(toplevel),
        worktree,
    }
}
/// The fingerprint through the posture of `git_out`: what every command except `changes` uses.
pub fn fingerprint(root: &Path) -> GitInfo {
    fingerprint_using(root, &git_out)
}

fn opt_json(value: &Option<String>) -> String {
    match value {
        Some(s) => format!("\"{}\"", escape(s)),
        None => "null".to_string(),
    }
}

/// Fixed key order: present, valid, head, branch, dirty, untracked, toplevel, worktree.
pub fn git_json(g: &GitInfo) -> String {
    format!(
        "{{\"present\":{},\"valid\":{},\"head\":{},\"branch\":{},\"dirty\":{},\"untracked\":{},\"toplevel\":{},\"worktree\":{}}}",
        g.present,
        g.valid,
        opt_json(&g.head),
        opt_json(&g.branch),
        g.dirty,
        g.untracked,
        opt_json(&g.toplevel),
        g.worktree
    )
}

/// What one git call returned. `code` is None when git was killed by a signal.
pub struct GitRun {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Hardened git for `changes` and its freshness check. Every call carries the
/// same overrides: filter drivers (clean, smudge, process) emptied and marked not
/// required, the external diff and fsmonitor emptied, no pager, no optional locks,
/// literal pathspecs. Built-in conversions (`core.autocrlf`, `text`/`eol`) are not
/// drivers and keep working. Other commands keep the posture of `git_out`.
pub struct DiffGit {
    overrides: Vec<(String, String)>,
}

impl DiffGit {
    /// Reads the configured filter drivers (reading config runs nothing).
    pub fn new(root: &Path) -> Result<DiffGit, String> {
        let listed = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "--no-optional-locks",
                "config",
                "--name-only",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process|required)$",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| format!("git unavailable: {e}"))?;
        // Exit 1 is git's answer for "no key matches": no drivers configured.
        let text = match listed.status.code() {
            Some(0) => String::from_utf8_lossy(&listed.stdout).into_owned(),
            Some(1) => String::new(),
            _ => {
                return Err(format!(
                    "git config failed: {}",
                    String::from_utf8_lossy(&listed.stderr).trim()
                ))
            }
        };
        let mut drivers = std::collections::BTreeSet::new();
        for key in text.lines() {
            let key = key.strip_prefix("filter.").unwrap_or(key);
            if let Some((name, _)) = key.rsplit_once('.') {
                drivers.insert(name.to_string());
            }
        }
        let mut overrides = vec![
            ("core.fsmonitor".to_string(), String::new()),
            ("diff.external".to_string(), String::new()),
        ];
        for driver in drivers {
            // An empty clean/smudge/process command means no filter; required=false
            // keeps a missing helper from failing the call.
            for key in ["clean", "smudge", "process"] {
                overrides.push((format!("filter.{driver}.{key}"), String::new()));
            }
            overrides.push((format!("filter.{driver}.required"), "false".to_string()));
        }
        Ok(DiffGit { overrides })
    }

    /// Runs a hardened call in the repository at `root`.
    pub fn run(&self, root: &Path, args: &[String]) -> Result<GitRun, String> {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(root)
            .args(["--no-optional-locks", "--no-pager"]);
        self.collect(cmd, args, &[], None)
    }

    /// Runs a hardened call with `cwd` as the working directory and no repository
    /// argument, with `extra` command-scope overrides on top of the hardened ones:
    /// `diff --no-index` on files outside any repository.
    pub fn run_in_with(
        &self,
        cwd: &Path,
        args: &[String],
        extra: &[(&str, &str)],
    ) -> Result<GitRun, String> {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(cwd)
            .args(["--no-optional-locks", "--no-pager"]);
        self.collect(cmd, args, extra, None)
    }

    /// Runs a hardened call in `root` with `input` on its standard input.
    pub fn run_stdin(&self, root: &Path, args: &[String], input: &[u8]) -> Result<GitRun, String> {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(root)
            .args(["--no-optional-locks", "--no-pager"]);
        self.collect(cmd, args, &[], Some(input))
    }

    /// Trimmed stdout of a hardened call in `root`; `None` when git fails.
    pub fn text(&self, root: &Path, args: &[&str]) -> Option<String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let run = self.run(root, &args).ok()?;
        (run.code == Some(0)).then(|| String::from_utf8_lossy(&run.stdout).trim_end().to_string())
    }

    /// The value of a repository config key read through the hardened posture: `None`
    /// when the key is unset (git exits 1), an error when git cannot read the config.
    pub fn config_value(&self, root: &Path, key: &str) -> Result<Option<String>, String> {
        let args: Vec<String> = ["config", "--get", key]
            .iter()
            .map(|a| a.to_string())
            .collect();
        let run = self.run(root, &args)?;
        match run.code {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&run.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(format!("git config failed: {}", run.stderr)),
        }
    }

    /// The fingerprint of `root`, read through this hardened posture.
    pub fn fingerprint(&self, root: &Path) -> GitInfo {
        fingerprint_using(root, &|dir, args| self.text(dir, args))
    }

    fn collect(
        &self,
        mut cmd: std::process::Command,
        args: &[String],
        extra: &[(&str, &str)],
        input: Option<&[u8]>,
    ) -> Result<GitRun, String> {
        use std::io::Write as _;
        cmd.args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            // Paths reach git as literal names: a `[`, `*` or `?` in a file name is not a pattern.
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env_remove("GIT_EXTERNAL_DIFF");
        let mut overrides = self.overrides.clone();
        overrides.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        cmd.env("GIT_CONFIG_COUNT", overrides.len().to_string());
        for (i, (key, value)) in overrides.iter().enumerate() {
            cmd.env(format!("GIT_CONFIG_KEY_{i}"), key)
                .env(format!("GIT_CONFIG_VALUE_{i}"), value);
        }
        let out = match input {
            None => cmd.output().map_err(|e| format!("git unavailable: {e}"))?,
            Some(input) => {
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = cmd.spawn().map_err(|e| format!("git unavailable: {e}"))?;
                // The input goes to git on a helper thread while wait_with_output drains
                // stdout and stderr. Writing all the input first deadlocks once git's
                // output fills a pipe while git still waits for more input.
                let stdin = child.stdin.take();
                let payload = input.to_vec();
                let writer = std::thread::spawn(move || -> std::io::Result<()> {
                    match stdin {
                        Some(mut stdin) => stdin.write_all(&payload),
                        None => Ok(()),
                    }
                });
                let out = child
                    .wait_with_output()
                    .map_err(|e| format!("git failed: {e}"))?;
                let written = writer
                    .join()
                    .map_err(|_| "git input thread panicked".to_string())?;
                written.map_err(|e| format!("git input failed: {e}"))?;
                out
            }
        };
        Ok(GitRun {
            code: out.status.code(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Whether git finds `root` itself as its work-tree top. A `.git` that is not a
/// repository of its own would otherwise borrow a parent's. The call reads
/// repository discovery only and runs no repository code.
pub fn is_worktree_top(root: &Path) -> bool {
    let found = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();
    match found {
        Ok(out) if out.status.success() => {
            let top = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            let top = canonical_str(Path::new(&top));
            top.is_some() && top == canonical_str(root)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, git_available, git_json};
    use crate::testutil::{git_repo, tmpdir};

    #[test]
    fn fingerprints_clean_repo_with_branch() {
        let Some(dir) = git_repo("git-clean", "main") else {
            return;
        };
        let g = fingerprint(&dir);
        assert!(g.present && g.valid);
        assert_eq!(g.head.as_ref().map(String::len), Some(40));
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert!(!g.dirty);
        assert_eq!(g.untracked, 0);
        assert!(!g.worktree);
        assert!(g.toplevel.is_some());
    }

    #[test]
    fn detects_dirty_and_untracked() {
        let Some(dir) = git_repo("git-dirty", "main") else {
            return;
        };
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let g = fingerprint(&dir);
        assert!(g.valid && g.dirty);
        assert_eq!(g.untracked, 1);
    }

    #[test]
    fn detects_linked_worktree() {
        if !git_available() {
            return;
        }
        let Some(dir) = git_repo("git-wt", "main") else {
            return;
        };
        let wt = dir.join("wt");
        let st = std::process::Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&wt)
            .current_dir(&dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .expect("run git worktree");
        if !st.success() {
            return;
        }
        let g = fingerprint(&wt);
        assert!(g.present && g.valid && g.worktree);
        assert!(git_json(&g).contains("\"worktree\":true"));
    }

    #[test]
    fn non_git_dir_is_not_present_and_corrupt_is_invalid() {
        let dir = tmpdir("git-plain");
        let g = fingerprint(&dir);
        assert!(!g.present && !g.valid);
        assert!(g.head.is_none() && g.branch.is_none());

        let corrupt = tmpdir("git-corrupt");
        std::fs::create_dir_all(corrupt.join(".git")).unwrap();
        std::fs::write(corrupt.join(".git").join("HEAD"), "garbage-no-newline").unwrap();
        let c = fingerprint(&corrupt);
        assert!(c.present && !c.valid);

        // Broken worktree pointer: git refuses the gitfile and (unlike the
        // directory case) never walks up into a parent repository.
        let corrupt_file = tmpdir("git-corrupt-file");
        std::fs::write(corrupt_file.join(".git"), "garbage").unwrap();
        let f = fingerprint(&corrupt_file);
        assert!(f.present && !f.valid);
    }

    #[test]
    fn status_failure_is_never_clean_nor_valid() {
        let Some(dir) = git_repo("git-status-fail", "main") else {
            return;
        };
        // Real modification that status must report...
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        // ...then destroy the evidence channel: rev-parse still works,
        // but status cannot verify anything.
        let index = std::fs::File::options()
            .write(true)
            .open(dir.join(".git").join("index"))
            .unwrap();
        index.set_len(20).unwrap();
        let g = fingerprint(&dir);
        assert!(g.present);
        assert!(
            !g.valid,
            "unverifiable status must not be reported valid (and its clean/dirty flags are meaningless)"
        );
    }

    fn set_config(dir: &std::path::Path, key: &str, value: &str) {
        assert!(std::process::Command::new("git")
            .args(["config", key, value])
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .expect("git config")
            .success());
    }

    #[test]
    fn status_never_runs_repo_configured_fsmonitor() {
        let Some(dir) = git_repo("git-fsmonitor", "main") else {
            return;
        };
        let marker = dir.join("fsmonitor-marker");
        let _ = std::fs::remove_file(&marker);
        // Payload that provably executes pre-fix (verified by probe: marker
        // created by plain `git status` without mitigation).
        #[cfg(windows)]
        {
            std::fs::write(
                dir.join("w.ps1"),
                "New-Item -Path $args[0] -ItemType File -Force | Out-Null\n",
            )
            .unwrap();
            let script = dir.join("w.ps1").to_string_lossy().replace('\\', "/");
            let target = marker.to_string_lossy().replace('\\', "/");
            set_config(
                &dir,
                "core.fsmonitor",
                &format!("powershell -NoProfile -File {script} {target}"),
            );
        }
        #[cfg(unix)]
        {
            set_config(
                &dir,
                "core.fsmonitor",
                &format!("sh -c 'touch \"$1\"' sh {}", marker.to_string_lossy()),
            );
        }
        let g = fingerprint(&dir);
        assert!(
            g.present && g.valid,
            "mitigation must not break working repos"
        );
        assert!(
            !marker.exists(),
            "repo-configured fsmonitor must not execute during fingerprint"
        );
    }

    #[test]
    fn git_json_has_fixed_key_order_with_nulls() {
        let dir = tmpdir("git-json");
        let g = fingerprint(&dir);
        assert_eq!(
            git_json(&g),
            "{\"present\":false,\"valid\":false,\"head\":null,\"branch\":null,\"dirty\":false,\"untracked\":0,\"toplevel\":null,\"worktree\":false}"
        );
    }

    #[test]
    fn stdin_larger_than_a_pipe_is_drained_while_it_is_written() {
        let Some(dir) = crate::testutil::git_repo("git-stdin", "main") else {
            return;
        };
        let diff = super::DiffGit::new(&dir).expect("driver listing");
        // About 1 MB of path input; check-attr answers one record per path, so the output
        // is larger than a pipe buffer and a write-first helper would block forever.
        let mut input = Vec::new();
        for i in 0..25000 {
            input.extend_from_slice(
                format!("dir{:03}/a-fairly-long-file-name-{i:06}.txt", i % 50).as_bytes(),
            );
            input.push(0);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let args: Vec<String> = ["check-attr", "-z", "--stdin", "filter"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            let result = diff
                .run_stdin(&dir, &args, &input)
                .map(|run| (run.code, run.stdout.len()));
            tx.send(result).ok();
        });
        match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(result) => {
                let (code, len) = result.expect("check-attr run");
                assert_eq!(code, Some(0));
                assert!(
                    len > 1_000_000,
                    "the output must exceed a pipe buffer: {len}"
                );
            }
            Err(_) => {
                panic!("no result within 60 s: the stdin write and the stdout read deadlocked")
            }
        }
    }
}
