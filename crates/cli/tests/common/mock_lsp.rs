// SPDX-License-Identifier: Apache-2.0
//! Locates the mock language server built from `examples/mock_lsp.rs`.
//!
//! Shared by the unit tests (through `testutil`) and the integration tests,
//! which cannot call `testutil` because it is `#[cfg(test)]` in a binary
//! crate. The path comes from cargo's own build report, never from the
//! location of the test executable: newer cargo versions place build
//! products in a per-package build directory that no longer sits beside
//! `target/debug/examples`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Build the mock server once per test process and return its path. A build
/// or lookup failure panics, so a missing server fails loudly, never skips.
pub fn mock_lsp_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = Command::new("cargo")
            .args([
                "build",
                "--offline",
                "--example",
                "mock_lsp",
                "--message-format=json",
            ])
            .stderr(Stdio::inherit())
            .output()
            .expect("cargo build mock server");
        assert!(out.status.success(), "mock server must build");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let exe = stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "mock_lsp")
            .filter_map(|v| v["executable"].as_str().map(PathBuf::from))
            .next_back()
            .expect("cargo did not report the mock server executable");
        assert!(exe.is_file(), "mock server missing: {}", exe.display());
        exe
    })
    .clone()
}
