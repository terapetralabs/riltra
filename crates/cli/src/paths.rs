// SPDX-License-Identifier: Apache-2.0
//! Path security: canonical roots, containment checks, registry key validation.
//! Symlinks/junctions resolve via `canonicalize`, so an escape through a link
//! never compares equal to (or under) the registered root.

use std::io;
use std::path::{Path, PathBuf};

/// Config dir for the registry. `RILTRA_HOME` (used by tests) points at the
/// directory itself; otherwise the OS per-user config location applies.
pub fn config_dir() -> PathBuf {
    if let Ok(h) = std::env::var("RILTRA_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    #[cfg(windows)]
    if let Ok(appdata) = std::env::var("APPDATA") {
        let mut p = PathBuf::from(appdata);
        p.push("riltra");
        return p;
    }
    #[cfg(not(windows))]
    if let Ok(home) = std::env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".config");
        p.push("riltra");
        return p;
    }
    let mut p = std::env::temp_dir();
    p.push("riltra");
    p
}

/// Pure override form of [`config_dir`] (testable without touching env).
// shortcut: test seam today; production caller when config grows (post-M1).
#[allow(dead_code)]
pub fn config_dir_from(home_override: Option<&str>) -> PathBuf {
    match home_override {
        Some(h) => {
            let mut p = PathBuf::from(h);
            p.push("riltra");
            p
        }
        None => config_dir(),
    }
}

pub fn registry_path() -> PathBuf {
    let mut p = config_dir();
    p.push("registry.json");
    p
}

/// Resolve `p` to its canonical directory form. Fails for missing paths and
/// non-directories so only real roots get registered.
pub fn canonical_root(p: &Path) -> io::Result<PathBuf> {
    let c = p.canonicalize()?;
    if c.is_dir() {
        Ok(c)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ))
    }
}

/// True only when `candidate` resolves to the root itself or below it.
/// Missing paths and symlink escapes resolve outside (or fail) → false.
/// M1-enforced by ignore-file loading; M2 file-scoped queries reuse it.
pub fn contains(canonical_root: &Path, candidate: &Path) -> bool {
    match candidate.canonicalize() {
        Ok(c) => c == *canonical_root || c.starts_with(canonical_root),
        Err(_) => false,
    }
}

/// First regular file named `name` plus one of `exts`, searched in `dirs` in
/// order (each location tries every extension). Only existence is checked;
/// nothing is executed.
pub fn find_executable_in(name: &str, dirs: &[PathBuf], exts: &[&str]) -> Option<PathBuf> {
    dirs.iter().find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|candidate| is_regular_file(candidate))
            .or_else(|| {
                if cfg!(windows) {
                    case_insensitive_match(dir, name, exts)
                } else {
                    None
                }
            })
    })
}

/// True when `path` resolves to a regular file (existence only, never opened).
pub fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Windows fallback for directories whose entry names differ only in case
/// from the requested name (exact candidates were already tried).
fn case_insensitive_match(dir: &Path, name: &str, exts: &[&str]) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    exts.iter().find_map(|ext| {
        let wanted = format!("{name}{ext}").to_ascii_lowercase();
        entries
            .iter()
            .find(|path| {
                path.file_name()
                    .is_some_and(|f| f.to_string_lossy().to_ascii_lowercase() == wanted)
                    && is_regular_file(path)
            })
            .cloned()
    })
}

/// Directories listed in `PATH`, in order.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Registry keys are plain names: no separators, dots, controls, or empties.
pub fn validate_name(name: &str) -> bool {
    !(name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 128
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(|c| c.is_control()))
}

#[cfg(test)]
mod tests {
    use super::{canonical_root, config_dir_from, contains, find_executable_in, validate_name};
    use crate::testutil::tmpdir;

    #[test]
    fn contains_accepts_root_itself_and_children() {
        let dir = tmpdir("paths-in");
        let root = canonical_root(&dir).unwrap();
        let child = dir.join("sub").join("f.txt");
        std::fs::create_dir_all(child.parent().unwrap()).unwrap();
        std::fs::write(&child, "x").unwrap();
        assert!(contains(&root, &root));
        assert!(contains(&root, &child));
    }

    #[test]
    fn contains_rejects_parent_and_sibling_escape() {
        let dir = tmpdir("paths-out");
        let root = canonical_root(&dir).unwrap();
        assert!(!contains(&root, &dir.join("..").join("evil")));
        assert!(!contains(&root, &dir.join("..")));
    }

    #[test]
    fn contains_rejects_missing_and_file_root() {
        let dir = tmpdir("paths-missing");
        let root = canonical_root(&dir).unwrap();
        assert!(!contains(&root, &dir.join("nope.txt")));
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(canonical_root(&file).is_err());
    }

    #[test]
    fn contains_rejects_symlink_escape() {
        let dir = tmpdir("paths-link");
        let outside = tmpdir("paths-link-out");
        let root = canonical_root(&dir).unwrap();
        let link = dir.join("escape");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&outside, &link).is_ok();
        if !made {
            return; // OS refused symlinks here; nothing to prove.
        }
        assert!(!contains(&root, &link));
        assert!(!contains(&root, &link.join("x")));
    }

    #[test]
    fn config_dir_prefers_override_then_os_default() {
        let p = config_dir_from(Some("/tmp/riltra-test-home"));
        assert!(p.ends_with("riltra"));
        let d = config_dir_from(None);
        assert!(d.ends_with("riltra"));
    }

    #[test]
    fn validate_name_rejects_separators_and_dots() {
        assert!(validate_name("my-repo_1"));
        assert!(!validate_name(""));
        assert!(!validate_name("."));
        assert!(!validate_name(".."));
        assert!(!validate_name("../evil"));
        assert!(!validate_name("a/b"));
        assert!(!validate_name("a\\b"));
    }

    fn touch(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn find_executable_in_takes_first_match_in_dir_order() {
        let a = tmpdir("exe-order-a");
        let b = tmpdir("exe-order-b");
        touch(&a.join("tool"));
        touch(&b.join("tool"));
        let found = find_executable_in("tool", &[b.clone(), a], &[""]);
        assert_eq!(found, Some(b.join("tool")));
    }

    #[test]
    fn find_executable_in_ignores_directories() {
        let a = tmpdir("exe-dir-a");
        let b = tmpdir("exe-dir-b");
        std::fs::create_dir_all(a.join("tool")).unwrap();
        touch(&b.join("tool"));
        let found = find_executable_in("tool", &[a, b.clone()], &[""]);
        assert_eq!(found, Some(b.join("tool")));
    }

    #[test]
    fn find_executable_in_tries_extensions_in_order() {
        let dir = tmpdir("exe-ext");
        touch(&dir.join("tool.cmd"));
        let dirs = std::slice::from_ref(&dir);
        assert_eq!(
            find_executable_in("tool", dirs, &["", ".exe", ".cmd"]),
            Some(dir.join("tool.cmd"))
        );
        assert_eq!(find_executable_in("tool", dirs, &["", ".exe"]), None);
    }

    #[cfg(windows)]
    #[test]
    fn find_executable_in_matches_windows_names_case_insensitively() {
        let dir = tmpdir("exe-win-case");
        touch(&dir.join("Rust-Analyzer.EXE"));
        let found = find_executable_in("rust-analyzer", std::slice::from_ref(&dir), &["", ".exe"])
            .expect("case-insensitive match");
        let name = found.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.eq_ignore_ascii_case("rust-analyzer.exe"));
    }

    #[cfg(unix)]
    #[test]
    fn find_executable_in_is_case_sensitive_on_unix() {
        let dir = tmpdir("exe-unix-case");
        touch(&dir.join("Rust-Analyzer"));
        // macOS APFS is case-insensitive by default; there the filesystem itself matches.
        if dir.join("rust-analyzer").exists() {
            return;
        }
        assert_eq!(
            find_executable_in("rust-analyzer", std::slice::from_ref(&dir), &[""]),
            None
        );
    }
}
