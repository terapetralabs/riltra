// SPDX-License-Identifier: Apache-2.0
//! Semantic provider detection and classification. Nothing in this module
//! starts a producer or a language server: lookups stop at "is there a
//! regular file with this name", and a configured server path is classified
//! from its file name alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths::{find_executable_in, is_regular_file};
use crate::scan::{plan_walk, PlanAction};
use crate::syntax::language_name;

/// A SCIP producer the guided setup may run. Its name is the executable name.
#[derive(Debug)]
pub struct Producer {
    pub name: &'static str,
    /// Manifest file that must exist at the repository root.
    pub needs: &'static str,
    /// Arguments when `needs` is present.
    pub args_met: &'static [&'static str],
    /// Arguments when `needs` is absent; `None` means no command is offered.
    pub args_unmet: Option<&'static [&'static str]>,
    /// Whether `<root>/node_modules/.bin` is searched before `PATH`.
    pub local_bin: bool,
}

/// One language family and its semantic capability status.
#[derive(Debug)]
pub struct Family {
    pub name: &'static str,
    pub languages: &'static [&'static str],
    pub scip_import: &'static str,
    pub lsp: &'static str,
    pub producer: Option<&'static Producer>,
    /// The language-server executable stem this family expects.
    pub expected_server: Option<&'static str>,
}

/// Where a producer executable was found. Never executed.
#[derive(Debug, PartialEq, Eq)]
pub enum Hit {
    /// Found under `<root>/node_modules/.bin`; holds the file name.
    Local(String),
    /// Found on `PATH`.
    Path(PathBuf),
}

/// Repository detection: language counts from parseable files and the
/// root-relevant manifests, plus the directory skips the walk reported.
#[derive(Debug)]
pub struct Detection {
    pub languages: BTreeMap<&'static str, usize>,
    pub manifests: Vec<String>,
    pub manifests_omitted: usize,
    pub unreadable_dirs: usize,
    pub escaped_dirs: usize,
}

static SCIP_TYPESCRIPT: Producer = Producer {
    name: "scip-typescript",
    needs: "tsconfig.json",
    args_met: &["index"],
    args_unmet: Some(&["index", "--infer-tsconfig"]),
    local_bin: true,
};

static RUST_ANALYZER: Producer = Producer {
    name: "rust-analyzer",
    needs: "Cargo.toml",
    args_met: &["scip", "."],
    args_unmet: None,
    local_bin: false,
};

static FAMILIES: [Family; 8] = [
    Family {
        name: "typescript",
        languages: &["typescript", "tsx"],
        scip_import: "verified-fixture",
        lsp: "contract-only",
        producer: Some(&SCIP_TYPESCRIPT),
        expected_server: None,
    },
    Family {
        name: "rust",
        languages: &["rust"],
        scip_import: "verified-fixture",
        lsp: "contract-only",
        producer: Some(&RUST_ANALYZER),
        expected_server: Some("rust-analyzer"),
    },
    Family {
        name: "python",
        languages: &["python"],
        scip_import: "contract-only",
        lsp: "contract-only",
        producer: None,
        expected_server: None,
    },
    Family {
        name: "javascript",
        languages: &["javascript"],
        scip_import: "contract-only",
        lsp: "contract-only",
        producer: None,
        expected_server: None,
    },
    Family {
        name: "c",
        languages: &["c"],
        scip_import: "contract-only",
        lsp: "contract-only",
        producer: None,
        expected_server: None,
    },
    Family {
        name: "cpp",
        languages: &["cpp"],
        scip_import: "contract-only",
        lsp: "contract-only",
        producer: None,
        expected_server: None,
    },
    Family {
        name: "go",
        languages: &["go"],
        scip_import: "not-tried",
        lsp: "not-tried",
        producer: None,
        expected_server: None,
    },
    Family {
        name: "java",
        languages: &["java"],
        scip_import: "not-tried",
        lsp: "not-tried",
        producer: None,
        expected_server: None,
    },
];

/// Capability table in plan order (D1).
pub fn families() -> &'static [Family] {
    &FAMILIES
}

/// Extensions tried for each executable location (never executed).
pub fn exec_exts() -> &'static [&'static str] {
    if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    }
}

/// Lookup order (D2): `<root>/node_modules/.bin` when the producer allows it,
/// then each `PATH` directory in order. Existence only; nothing runs.
pub fn find_producer(root: &Path, producer: &Producer, path_dirs: &[PathBuf]) -> Option<Hit> {
    if producer.local_bin {
        let local = [root.join("node_modules").join(".bin")];
        if let Some(found) = find_executable_in(producer.name, &local, exec_exts()) {
            let name = found
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Some(Hit::Local(name));
        }
    }
    find_executable_in(producer.name, path_dirs, exec_exts()).map(Hit::Path)
}

const KNOWN_SERVERS: [&str; 2] = ["rust-analyzer", "typescript-language-server"];

/// Classify a configured language-server path (D3) without starting it.
/// The stem is the file name without `.exe`, `.cmd` or `.bat`, lowercased on
/// Windows.
pub fn classify_lsp(value: Option<&str>, family: &Family) -> &'static str {
    let Some(value) = value else {
        return "not-configured";
    };
    let path = Path::new(value);
    if !path.is_absolute() {
        return "not-absolute";
    }
    if !is_regular_file(path) {
        return "not-found";
    }
    let stem = server_stem(path);
    if stem == "typescript-language-server" {
        return "launch-incompatible";
    }
    if Some(stem.as_str()) == family.expected_server {
        return "configured";
    }
    if KNOWN_SERVERS.contains(&stem.as_str()) {
        return "mismatch";
    }
    "unrecognized"
}

fn server_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = if cfg!(windows) {
        name.to_ascii_lowercase()
    } else {
        name
    };
    for ext in [".exe", ".cmd", ".bat"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    name
}

const MAX_MANIFESTS: usize = 20;
const MANIFEST_NAMES: [&str; 3] = ["Cargo.toml", "package.json", "tsconfig.json"];

/// Walk `root` with the shared scan policy and report languages and manifests
/// (D4). Only Parse-action files count as languages; manifests count for any
/// action except Skip.
pub fn detect(root: &Path) -> Result<Detection, String> {
    let (plan, skips) = plan_walk(root)?;
    let mut languages: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut manifests: Vec<String> = Vec::new();
    for file in &plan {
        if let PlanAction::Parse(lang) = file.action {
            *languages.entry(language_name(lang)).or_insert(0) += 1;
        }
        let is_manifest = file
            .full
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| MANIFEST_NAMES.contains(&n));
        if is_manifest && !matches!(file.action, PlanAction::Skip(_)) {
            manifests.push(file.rel.clone());
        }
    }
    manifests.sort();
    let total = manifests.len();
    manifests.truncate(MAX_MANIFESTS);
    Ok(Detection {
        languages,
        manifests,
        manifests_omitted: total.saturating_sub(MAX_MANIFESTS),
        unreadable_dirs: skips.unreadable,
        escaped_dirs: skips.escape,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::Language;
    use crate::testutil::tmpdir;

    fn family(name: &str) -> &'static Family {
        families()
            .iter()
            .find(|f| f.name == name)
            .expect("family exists")
    }

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn lsp_value(path: &Path) -> String {
        path.to_str().expect("utf-8 test path").to_string()
    }

    /// One D1 row: name, languages, scip_import, lsp, producer, expected server.
    type Row = (
        &'static str,
        &'static [&'static str],
        &'static str,
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
    );

    #[test]
    fn families_match_capability_table() {
        let rows: [Row; 8] = [
            (
                "typescript",
                &["typescript", "tsx"],
                "verified-fixture",
                "contract-only",
                Some("scip-typescript"),
                None,
            ),
            (
                "rust",
                &["rust"],
                "verified-fixture",
                "contract-only",
                Some("rust-analyzer"),
                Some("rust-analyzer"),
            ),
            (
                "python",
                &["python"],
                "contract-only",
                "contract-only",
                None,
                None,
            ),
            (
                "javascript",
                &["javascript"],
                "contract-only",
                "contract-only",
                None,
                None,
            ),
            ("c", &["c"], "contract-only", "contract-only", None, None),
            (
                "cpp",
                &["cpp"],
                "contract-only",
                "contract-only",
                None,
                None,
            ),
            ("go", &["go"], "not-tried", "not-tried", None, None),
            ("java", &["java"], "not-tried", "not-tried", None, None),
        ];
        let got = families();
        assert_eq!(got.len(), rows.len());
        for (f, (name, langs, scip, lsp, producer, server)) in got.iter().zip(rows.iter()) {
            assert_eq!(f.name, *name);
            assert_eq!(f.languages, *langs, "{name} languages");
            assert_eq!(f.scip_import, *scip, "{name} scip_import");
            assert_eq!(f.lsp, *lsp, "{name} lsp");
            assert_eq!(f.producer.map(|p| p.name), *producer, "{name} producer");
            assert_eq!(f.expected_server, *server, "{name} expected server");
        }
    }

    #[test]
    fn families_cover_every_language_exactly_once() {
        let all = [
            Language::Python,
            Language::JavaScript,
            Language::TypeScript,
            Language::Tsx,
            Language::Rust,
            Language::C,
            Language::Cpp,
            Language::Go,
            Language::Java,
        ];
        for lang in all {
            let name = language_name(lang);
            let hits = families()
                .iter()
                .filter(|f| f.languages.contains(&name))
                .count();
            assert_eq!(hits, 1, "{name} family count");
        }
        let total: usize = families().iter().map(|f| f.languages.len()).sum();
        assert_eq!(total, all.len());
    }

    #[test]
    fn producers_match_d2() {
        let ts = family("typescript").producer.expect("typescript producer");
        assert_eq!(ts.name, "scip-typescript");
        assert_eq!(ts.needs, "tsconfig.json");
        assert_eq!(ts.args_met, &["index"][..]);
        assert_eq!(ts.args_unmet, Some(&["index", "--infer-tsconfig"][..]));
        assert!(ts.local_bin);

        let rust = family("rust").producer.expect("rust producer");
        assert_eq!(rust.name, "rust-analyzer");
        assert_eq!(rust.needs, "Cargo.toml");
        assert_eq!(rust.args_met, &["scip", "."][..]);
        assert_eq!(rust.args_unmet, None);
        assert!(!rust.local_bin);

        for name in ["python", "javascript", "c", "cpp", "go", "java"] {
            assert!(family(name).producer.is_none(), "{name} has no producer");
        }
    }

    #[test]
    fn exec_exts_are_platform_specific() {
        if cfg!(windows) {
            assert_eq!(exec_exts(), &["", ".exe", ".cmd", ".bat"][..]);
        } else {
            assert_eq!(exec_exts(), &[""][..]);
        }
    }

    #[test]
    fn classify_lsp_not_configured_when_absent() {
        assert_eq!(classify_lsp(None, family("rust")), "not-configured");
    }

    #[test]
    fn classify_lsp_requires_an_absolute_path() {
        assert_eq!(
            classify_lsp(Some("rust-analyzer"), family("rust")),
            "not-absolute"
        );
    }

    #[test]
    fn classify_lsp_not_found_for_directory_or_missing_file() {
        let dir = tmpdir("ps-lsp-missing");
        let missing = dir.join("rust-analyzer");
        assert_eq!(
            classify_lsp(Some(&lsp_value(&dir)), family("rust")),
            "not-found"
        );
        assert_eq!(
            classify_lsp(Some(&lsp_value(&missing)), family("rust")),
            "not-found"
        );
    }

    #[test]
    fn classify_lsp_configured_only_for_the_expected_family() {
        let dir = tmpdir("ps-lsp-match");
        let server = write(&dir, "rust-analyzer", "x");
        let value = lsp_value(&server);
        assert_eq!(classify_lsp(Some(&value), family("rust")), "configured");
        assert_eq!(classify_lsp(Some(&value), family("typescript")), "mismatch");
        assert_eq!(classify_lsp(Some(&value), family("python")), "mismatch");
    }

    #[test]
    fn classify_lsp_typescript_server_is_launch_incompatible_for_every_family() {
        let dir = tmpdir("ps-lsp-launch");
        let server = write(&dir, "typescript-language-server.cmd", "x");
        let value = lsp_value(&server);
        for f in families() {
            assert_eq!(
                classify_lsp(Some(&value), f),
                "launch-incompatible",
                "{}",
                f.name
            );
        }
    }

    #[test]
    fn classify_lsp_unknown_server_is_unrecognized() {
        let dir = tmpdir("ps-lsp-unknown");
        let server = write(&dir, "my-server", "x");
        assert_eq!(
            classify_lsp(Some(&lsp_value(&server)), family("rust")),
            "unrecognized"
        );
    }

    #[test]
    fn classify_lsp_uses_the_stem_inside_a_directory_with_a_space() {
        let dir = tmpdir("ps-lsp-space");
        let server = write(&dir, "my servers/rust-analyzer.exe", "x");
        assert_eq!(
            classify_lsp(Some(&lsp_value(&server)), family("rust")),
            "configured"
        );
    }

    #[cfg(windows)]
    #[test]
    fn classify_lsp_stem_is_case_insensitive_on_windows() {
        let dir = tmpdir("ps-lsp-win-case");
        let server = write(&dir, "Rust-Analyzer.EXE", "x");
        assert_eq!(
            classify_lsp(Some(&lsp_value(&server)), family("rust")),
            "configured"
        );
    }

    #[test]
    fn find_producer_prefers_local_bin_for_scip_typescript() {
        let root = tmpdir("ps-producer-local");
        let path_root = tmpdir("ps-producer-local-path");
        let file = if cfg!(windows) {
            "scip-typescript.cmd"
        } else {
            "scip-typescript"
        };
        write(&root, &format!("node_modules/.bin/{file}"), "x");
        write(&path_root, file, "x");
        let dirs = [path_root];
        let ts = family("typescript").producer.expect("producer");
        assert_eq!(
            find_producer(&root, ts, &dirs),
            Some(Hit::Local(file.to_string()))
        );
    }

    #[test]
    fn find_producer_falls_back_to_path() {
        let root = tmpdir("ps-producer-path");
        let path_root = tmpdir("ps-producer-path-dir");
        let file = if cfg!(windows) {
            "scip-typescript.cmd"
        } else {
            "scip-typescript"
        };
        write(&path_root, file, "x");
        let dirs = [path_root.clone()];
        let ts = family("typescript").producer.expect("producer");
        assert_eq!(
            find_producer(&root, ts, &dirs),
            Some(Hit::Path(path_root.join(file)))
        );
    }

    #[test]
    fn find_producer_never_checks_local_bin_for_rust_analyzer() {
        let root = tmpdir("ps-producer-rust");
        let file = if cfg!(windows) {
            "rust-analyzer.exe"
        } else {
            "rust-analyzer"
        };
        write(&root, &format!("node_modules/.bin/{file}"), "x");
        let rust = family("rust").producer.expect("producer");
        assert_eq!(find_producer(&root, rust, &[]), None);
    }

    #[test]
    fn detect_counts_languages_and_manifests() {
        let root = tmpdir("ps-detect");
        write(&root, "a.ts", "export const a = 1;\n");
        write(&root, "b.tsx", "export const b = <div />;\n");
        write(&root, "src/lib.rs", "pub fn f() {}\n");
        write(&root, "x.py", "def f():\n    pass\n");
        write(&root, "Cargo.toml", "[package]\n");
        write(&root, "package.json", "{}\n");
        write(&root, "tsconfig.json", "{}\n");
        write(&root, ".riltraignore", "gen/\n");
        write(&root, "gen/c.ts", "export const c = 1;\n");
        let d = detect(&root).expect("detect");
        assert_eq!(
            d.languages,
            BTreeMap::from([("python", 1), ("rust", 1), ("tsx", 1), ("typescript", 1)])
        );
        assert_eq!(
            d.manifests,
            vec![
                "Cargo.toml".to_string(),
                "package.json".to_string(),
                "tsconfig.json".to_string()
            ]
        );
        assert_eq!(d.manifests_omitted, 0);
        assert_eq!((d.unreadable_dirs, d.escaped_dirs), (0, 0));
    }

    #[test]
    fn detect_caps_manifests_at_twenty() {
        let root = tmpdir("ps-detect-cap");
        for i in 0..25 {
            write(&root, &format!("pkg{i:02}/package.json"), "{}\n");
        }
        let d = detect(&root).expect("detect");
        assert_eq!(d.manifests.len(), 20);
        assert_eq!(d.manifests_omitted, 5);
        assert_eq!(d.manifests[0], "pkg00/package.json");
        assert_eq!((d.unreadable_dirs, d.escaped_dirs), (0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn detect_reports_unreadable_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmpdir("ps-detect-locked");
        let locked = root.join("locked");
        write(&root, "locked/src/lib.rs", "pub fn f() {}\n");
        let restore = || {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: mode 000 does not stop reads, nothing to prove.
            restore();
            return;
        }
        let result = detect(&root);
        restore();
        let d = result.expect("detect");
        assert!(
            d.unreadable_dirs >= 1,
            "unreadable_dirs = {}",
            d.unreadable_dirs
        );
        assert!(!d.languages.contains_key("rust"));
    }
}
