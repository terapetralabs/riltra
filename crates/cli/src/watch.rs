// SPDX-License-Identifier: Apache-2.0
//! Source signature for `riltra watch`: the polled description of what an
//! incremental index run would read. Two polls with equal signatures mean no
//! build is due. It reads only `scan::plan_walk` (ignore rules, symlinks not
//! followed, `.git` skipped), `index::config_fingerprint` and the read-only
//! `git::head_branch`, and it writes nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::git;
use crate::index;
use crate::scan::{self, PlanAction};

/// What a poll compares. `files` maps a repository-relative path to
/// (length, mtime in nanoseconds since the epoch, 0 when the platform reports none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub files: BTreeMap<String, (u64, u128)>,
    pub config: String,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// Riltra's own outputs under a home that sits inside the watched root. A build
/// writes them, so counting them would make every build trigger the next one.
pub struct Excluded {
    index_dir: PathBuf,
    knowledge_dir: PathBuf,
    registry: PathBuf,
}

impl Excluded {
    /// `canon_home` must already be canonical; the caller canonicalizes it once.
    pub fn new(canon_home: &Path) -> Excluded {
        Excluded {
            index_dir: canon_home.join("index"),
            knowledge_dir: canon_home.join("knowledge"),
            registry: canon_home.join("registry.json"),
        }
    }

    fn covers(&self, canon_file: &Path) -> bool {
        canon_file.starts_with(&self.index_dir)
            || canon_file.starts_with(&self.knowledge_dir)
            || canon_file == self.registry
    }
}

/// Refuses a root that is the home or sits inside it. Home inside the root is
/// allowed: the signature leaves out Riltra's own outputs instead.
pub fn check_overlap(canon_root: &Path, canon_home: &Path) -> Result<(), String> {
    // Path::starts_with compares whole components, so `/a/bc` is not inside `/a/b`.
    if canon_root.starts_with(canon_home) {
        return Err(format!(
            "watch-root-overlaps-home: {} is the Riltra home or inside it; watch a repository outside it",
            canon_root.display()
        ));
    }
    Ok(())
}

/// The signature of `root` as the next build would see it. Only files the walk
/// would parse count (C6). A file that disappears between the walk and the stat
/// is left out; the next poll reconciles it.
pub fn signature(root: &Path, excluded: &Excluded) -> Result<Signature, String> {
    let (plan, _skips) = scan::plan_walk(root)?;
    let mut files = BTreeMap::new();
    for planned in plan {
        if !matches!(planned.action, PlanAction::Parse(_)) {
            continue;
        }
        let canon = match std::fs::canonicalize(&planned.full) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot resolve {}: {e}", planned.rel)),
        };
        if excluded.covers(&canon) {
            continue;
        }
        let meta = match std::fs::symlink_metadata(&planned.full) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot stat {}: {e}", planned.rel)),
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        files.insert(planned.rel, (meta.len(), mtime));
    }
    let (head, branch) = git::head_branch(root);
    Ok(Signature {
        files,
        config: index::config_fingerprint(root),
        head,
        branch,
    })
}

#[cfg(test)]
mod tests {
    use super::{check_overlap, signature, Excluded, Signature};
    use crate::git;
    use crate::testutil::{git_repo, tmpdir};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// A watched root with `a.rs`, and a home outside it.
    fn rooted(tag: &str) -> (PathBuf, Excluded) {
        let root = tmpdir(tag);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let home = tmpdir(&format!("{tag}-home"));
        std::fs::write(home.join("registry.json"), "{}\n").unwrap();
        std::fs::create_dir_all(home.join("index")).unwrap();
        let excluded = Excluded::new(&home.canonicalize().unwrap());
        (root, excluded)
    }

    /// A home outside the root, for tests that need no home of their own.
    fn outside_home(tag: &str) -> Excluded {
        let home = tmpdir(&format!("{tag}-home"));
        Excluded::new(&home.canonicalize().unwrap())
    }

    fn has(sig: &Signature, name: &str) -> bool {
        sig.files.keys().any(|k| k.ends_with(name))
    }

    fn git(root: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .expect("run git");
        assert!(st.success(), "git {args:?} failed");
    }

    #[test]
    fn signature_detects_add() {
        let (root, ex) = rooted("watch-add");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
        assert!(has(&s1, "b.rs"));
    }

    #[test]
    fn signature_detects_delete() {
        let (root, ex) = rooted("watch-delete");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::remove_file(root.join("a.rs")).unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
        assert!(!has(&s1, "a.rs"));
    }

    #[test]
    fn signature_detects_rename() {
        let (root, ex) = rooted("watch-rename");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::rename(root.join("a.rs"), root.join("b.rs")).unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
        assert!(has(&s1, "b.rs") && !has(&s1, "a.rs"));
    }

    #[test]
    fn signature_detects_edit_with_new_length() {
        let (root, ex) = rooted("watch-edit");
        let s0 = signature(&root, &ex).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("a.rs"))
            .unwrap();
        f.write_all(b"fn more() {}\n").unwrap();
        drop(f);
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0, s1);
    }

    #[test]
    fn signature_detects_ignore_rule_change() {
        let (root, ex) = rooted("watch-rules");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::write(root.join(".riltraignore"), "gen/\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_ne!(s0.config, s1.config);
    }

    #[test]
    fn signature_ignores_ignored_paths() {
        let (root, ex) = rooted("watch-ignored");
        std::fs::write(root.join(".riltraignore"), "gen/\n").unwrap();
        let s0 = signature(&root, &ex).unwrap();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::write(root.join("gen").join("x.rs"), "fn x() {}\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
    }

    #[test]
    fn signature_excludes_only_riltra_outputs() {
        let root = tmpdir("watch-home-inside");
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let home = root.join(".rhome");
        std::fs::create_dir_all(home.join("index")).unwrap();
        std::fs::write(home.join("registry.json"), "{}\n").unwrap();
        let ex = Excluded::new(&home.canonicalize().unwrap());
        let s0 = signature(&root, &ex).unwrap();
        // Riltra's own writes: a database, its lock, the knowledge store, the registry.
        // The `.rs` files would be parsed if they were not excluded, so they test the
        // exclusion itself rather than the unsupported-file rule.
        std::fs::write(home.join("index").join("r.db"), b"x").unwrap();
        std::fs::write(home.join("index").join("r.db.lock"), b"").unwrap();
        std::fs::write(home.join("index").join("extra.rs"), "fn e() {}\n").unwrap();
        std::fs::create_dir_all(home.join("knowledge")).unwrap();
        std::fs::write(home.join("knowledge").join("r.db"), b"x").unwrap();
        std::fs::write(home.join("knowledge").join("extra.rs"), "fn k() {}\n").unwrap();
        std::fs::write(home.join("registry.json"), "{\"v\":2}\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
        // Any other file under home is still part of the build's input.
        std::fs::write(home.join("notes.rs"), "fn n() {}\n").unwrap();
        let s2 = signature(&root, &ex).unwrap();
        assert_ne!(s1, s2);
        assert!(has(&s2, "notes.rs"));
    }

    #[test]
    fn signature_detects_head_change() {
        let Some(root) = git_repo("watch-head", "main") else {
            return;
        };
        let ex = outside_home("watch-head");
        let s0 = signature(&root, &ex).unwrap();
        git(&root, &["commit", "--allow-empty", "-m", "empty"]);
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0.files, s1.files);
        assert_ne!(s0.head, s1.head);
        assert!(s1.head.is_some());
    }

    #[test]
    fn overlap_is_refused() {
        let root = tmpdir("watch-overlap");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let home = root.join(".rhome");
        std::fs::create_dir_all(&home).unwrap();
        let canon_root = root.canonicalize().unwrap();
        let canon_sub = root.join("sub").canonicalize().unwrap();
        let canon_home = home.canonicalize().unwrap();
        let refused = "watch-root-overlaps-home:";
        assert!(check_overlap(&canon_root, &canon_root)
            .unwrap_err()
            .starts_with(refused));
        assert!(check_overlap(&canon_sub, &canon_root)
            .unwrap_err()
            .starts_with(refused));
        assert!(check_overlap(&canon_root, &canon_home).is_ok());
    }

    #[test]
    fn signature_is_deterministic() {
        let (root, ex) = rooted("watch-deterministic");
        let s0 = signature(&root, &ex).unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
    }

    #[test]
    fn signature_ignores_unsupported_files() {
        let (root, ex) = rooted("watch-unsupported");
        let s0 = signature(&root, &ex).unwrap();
        std::fs::write(root.join("notes.xyz"), "plain\n").unwrap();
        let s1 = signature(&root, &ex).unwrap();
        assert_eq!(s0, s1);
    }

    #[test]
    fn head_branch_matches_fingerprint() {
        let Some(root) = git_repo("watch-refs", "main") else {
            return;
        };
        let fp = git::fingerprint(&root);
        assert_eq!(git::head_branch(&root), (fp.head, fp.branch));
        git(&root, &["checkout", "-b", "other"]);
        let fp = git::fingerprint(&root);
        assert_eq!(
            git::head_branch(&root),
            (fp.head.clone(), fp.branch.clone())
        );
        assert_eq!(fp.branch.as_deref(), Some("other"));
    }

    #[test]
    fn head_branch_none_outside_own_repo() {
        let Some(repo) = git_repo("watch-nested", "main") else {
            return;
        };
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(git::head_branch(&sub), (None, None));
        let fp = git::fingerprint(&sub);
        assert_eq!((fp.head, fp.branch), (None, None));
        let plain = tmpdir("watch-plain");
        assert_eq!(git::head_branch(&plain), (None, None));
    }

    #[test]
    fn head_branch_filters_short_head() {
        let root = tmpdir("watch-short-head");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let canon = root.canonicalize().unwrap().to_string_lossy().to_string();
        let run = |_dir: &Path, args: &[&str]| -> Option<String> {
            match args {
                ["rev-parse", "--show-toplevel"] => Some(canon.clone()),
                ["rev-parse", "HEAD"] => Some("abc".to_string()),
                ["branch", "--show-current"] => Some("main".to_string()),
                _ => None,
            }
        };
        assert_eq!(
            git::head_branch_using(&root, &run),
            (None, Some("main".to_string()))
        );
    }
}
