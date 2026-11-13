// SPDX-License-Identifier: Apache-2.0
//! Test-only helpers: unique temp dirs and git fixtures. Left on disk under
//! the OS temp dir on purpose (debuggable, OS-managed).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique, existing, empty directory. Parallel-safe via pid + counter.
pub fn tmpdir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-{}-{}-{}", tag, std::process::id(), n));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create tmpdir");
    p
}

pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Minimal git repo with one commit on `branch`. `None` when git is absent
/// (tests skip); hard failure otherwise so broken fixtures stay loud.
pub fn git_repo(tag: &str, branch: &str) -> Option<PathBuf> {
    if !git_available() {
        return None;
    }
    let dir = tmpdir(tag);
    let g = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("run git");
        assert!(st.success(), "git {args:?} failed");
    };
    g(&["init", "-b", branch, "."]);
    g(&["config", "user.email", "t@t"]);
    g(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
    g(&["add", "a.txt"]);
    g(&["commit", "-m", "init"]);
    Some(dir)
}

#[path = "../tests/common/mock_lsp.rs"]
mod mock_lsp;
/// The mock language server binary, built once per test process so it can
/// never go stale. A missing binary fails loudly, never skips.
pub use mock_lsp::mock_lsp_bin;
