// SPDX-License-Identifier: Apache-2.0
//! Semantic provider detection and classification. Nothing in this module
//! starts a producer or a language server: lookups stop at "is there a
//! regular file with this name", and a configured server path is classified
//! from its file name alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::index::{SemanticStatusRow, SetupIndex};
use crate::json;
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

/// Extensions tried for each executable location (never executed). Windows
/// leaves out "" because npm also writes an extensionless sh shim, which is not
/// runnable from PowerShell and would otherwise win over the `.cmd` shim.
pub fn exec_exts() -> &'static [&'static str] {
    if cfg!(windows) {
        &[".exe", ".cmd", ".bat"]
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

/// Shell that rendered commands are written for (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    PosixSh,
    PowerShell,
}

impl Shell {
    /// `powershell` on Windows, `posix-sh` everywhere else.
    pub fn current() -> Shell {
        if cfg!(windows) {
            Shell::PowerShell
        } else {
            Shell::PosixSh
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Shell::PosixSh => "posix-sh",
            Shell::PowerShell => "powershell",
        }
    }
}

/// Quote one argument for `shell` (D9). Flags are literal tokens and never
/// pass through here.
pub fn quote(shell: Shell, arg: &str) -> String {
    match shell {
        Shell::PosixSh => {
            let bare = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
            if bare {
                arg.to_string()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        }
        Shell::PowerShell => {
            let bare = !arg.is_empty()
                && !arg.starts_with('-')
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c));
            if bare {
                arg.to_string()
            } else {
                format!("'{}'", arg.replace('\'', "''"))
            }
        }
    }
}

/// The program token of a command (D9): `bare` for `riltra` and PATH hits, and
/// the `node_modules/.bin` form for a local hit.
pub fn render_program(shell: Shell, hit: Option<&Hit>, bare: &str) -> String {
    match (shell, hit) {
        (Shell::PosixSh, Some(Hit::Local(file))) => format!("./node_modules/.bin/{file}"),
        (Shell::PowerShell, Some(Hit::Local(file))) => {
            format!("& '.\\node_modules\\.bin\\{file}'")
        }
        _ => bare.to_string(),
    }
}

/// One guided producer after lookup. Nothing here runs the producer.
struct Guided {
    producer: &'static Producer,
    found: bool,
    /// Absolute path of the file that was found.
    path: Option<PathBuf>,
    needs_met: bool,
    /// The command for the arguments that apply, `None` when no command is offered.
    command: Option<String>,
}

fn guided(root: &Path, shell: Shell, producer: &'static Producer, path_dirs: &[PathBuf]) -> Guided {
    let hit = find_producer(root, producer, path_dirs);
    let needs_met = is_regular_file(&root.join(producer.needs));
    let args = if needs_met {
        Some(producer.args_met)
    } else {
        producer.args_unmet
    };
    let path = hit.as_ref().map(|found| match found {
        Hit::Local(file) => root.join("node_modules").join(".bin").join(file),
        Hit::Path(path) => path.clone(),
    });
    let command = match (hit.as_ref(), args) {
        (Some(found), Some(args)) => {
            let mut command = render_program(shell, Some(found), producer.name);
            for arg in args {
                command.push(' ');
                command.push_str(arg);
            }
            Some(command)
        }
        _ => None,
    };
    Guided {
        producer,
        found: hit.is_some(),
        path,
        needs_met,
        command,
    }
}

/// One `next` entry: a fixed step text and an optional command.
struct Step {
    text: String,
    command: Option<String>,
}

impl Step {
    fn plain(text: String) -> Step {
        Step {
            text,
            command: None,
        }
    }

    fn with(text: &str, command: &str) -> Step {
        Step {
            text: text.to_string(),
            command: Some(command.to_string()),
        }
    }
}

/// Index fields of the report, flattened from `SetupIndex`.
struct IndexView<'a> {
    status: &'static str,
    snapshot: Option<i64>,
    freshness: Option<&'a str>,
    reasons: &'a [String],
    rows: &'a [SemanticStatusRow],
}

/// Imports listed in the report; later ones are counted in a step.
const MAX_IMPORTS: usize = 20;

/// JSON string or `null`.
fn json_opt(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("\"{}\"", json::escape(v)),
        None => "null".to_string(),
    }
}

fn producer_json(g: &Guided) -> String {
    let path = g.path.as_ref().map(|p| p.to_string_lossy().into_owned());
    format!(
        "{{\"name\":\"{}\",\"state\":\"{}\",\"path\":{},\"needs\":\"{}\",\"needs_met\":{},\"command\":{}}}",
        g.producer.name,
        if g.found { "found" } else { "not-found" },
        json_opt(path.as_deref()),
        g.producer.needs,
        g.needs_met,
        json_opt(g.command.as_deref()),
    )
}

/// The D4 setup report as one line of JSON, with the D5 next steps. Pure
/// apart from file-existence checks; nothing is executed.
pub fn report(
    repo: &str,
    root: &Path,
    detection: &Detection,
    index: &SetupIndex,
    lsp: (Option<&str>, &'static str),
    path_dirs: &[PathBuf],
    shell: Shell,
) -> String {
    let riltra = render_program(shell, None, "riltra");
    let repo_arg = quote(shell, repo);
    let index_cmd = format!("{riltra} index --repo {repo_arg}");
    let import_cmd = format!(
        "{riltra} semantic import --repo {repo_arg} --scip {}",
        quote(shell, "index.scip")
    );

    let view = match index {
        SetupIndex::NoIndex => IndexView {
            status: "no-index",
            snapshot: None,
            freshness: None,
            reasons: &[],
            rows: &[],
        },
        SetupIndex::SchemaOutdated(_) => IndexView {
            status: "schema_outdated",
            snapshot: None,
            freshness: None,
            reasons: &[],
            rows: &[],
        },
        SetupIndex::SchemaAhead(_) => IndexView {
            status: "schema_ahead",
            snapshot: None,
            freshness: None,
            reasons: &[],
            rows: &[],
        },
        SetupIndex::Ready(state) => IndexView {
            status: "ready",
            snapshot: state.snapshot.as_ref().map(|s| s.id),
            freshness: state.freshness.as_deref(),
            reasons: &state.reasons,
            rows: &state.rows,
        },
    };

    // Families present: any of their languages has a parsed file (D1 order).
    let present: Vec<&'static Family> = families()
        .iter()
        .filter(|f| {
            f.languages
                .iter()
                .any(|lang| detection.languages.contains_key(lang))
        })
        .collect();
    let guided_by_family: Vec<Option<Guided>> = present
        .iter()
        .map(|f| f.producer.map(|p| guided(root, shell, p, path_dirs)))
        .collect();
    let lsp_state = |family: &Family| classify_lsp(lsp.0, family);
    let state_of = |name: &str| {
        present
            .iter()
            .find(|f| f.name == name)
            .map(|f| lsp_state(f))
    };

    // D5 next steps, in rule order.
    let mut steps: Vec<Step> = Vec::new();
    match index {
        SetupIndex::NoIndex | SetupIndex::SchemaOutdated(_) => {
            steps.push(Step::with("build the code index", &index_cmd))
        }
        SetupIndex::SchemaAhead(_) => steps.push(Step::plain(
            "this index was written by a newer riltra; upgrade riltra".to_string(),
        )),
        SetupIndex::Ready(state) if state.freshness.as_deref() != Some("fresh") => steps.push(
            Step::with("rebuild the code index so imports can match it", &index_cmd),
        ),
        SetupIndex::Ready(_) => {}
    }
    let scan_complete = detection.unreadable_dirs == 0 && detection.escaped_dirs == 0;
    if !scan_complete {
        steps.push(Step::plain(
            "some directories could not be read or point outside the repository; languages and manifests may be incomplete".to_string(),
        ));
    }
    for guided_entry in guided_by_family.iter().flatten() {
        let producer = guided_entry.producer.name;
        let unverified = view
            .rows
            .iter()
            .any(|r| r.provider_name == producer && r.verification == "Unverified");
        if unverified {
            continue;
        }
        if !guided_entry.found {
            steps.push(Step::plain(format!(
                "install {producer} yourself; riltra does not install providers"
            )));
        } else if let Some(command) = &guided_entry.command {
            steps.push(Step {
                text: format!("produce a SCIP index with {producer}"),
                command: Some(command.clone()),
            });
            steps.push(Step {
                text: "import the SCIP index".to_string(),
                command: Some(import_cmd.clone()),
            });
        } else {
            steps.push(Step::plain(format!(
                "{producer} scip needs a {} at the repository root",
                guided_entry.producer.needs
            )));
        }
    }
    for row in view.rows.iter().filter(|r| r.verification == "Stale") {
        steps.push(Step::plain(format!(
            "re-run {} and import again; import {} no longer matches the source",
            row.provider_name, row.import_id
        )));
    }
    if state_of("rust") == Some("not-configured") {
        steps.push(Step::plain(
            "optional: set RILTRA_LSP_SERVER to the absolute path of rust-analyzer for riltra_code_semantic".to_string(),
        ));
    }
    if state_of("typescript") == Some("launch-incompatible") {
        steps.push(Step::plain(
            "typescript-language-server needs --stdio, which riltra does not pass; use the SCIP path for TypeScript".to_string(),
        ));
    }
    if view.rows.len() > MAX_IMPORTS {
        steps.push(Step::plain(format!(
            "{} more imports are not listed here; the semantic status report lists every import",
            view.rows.len() - MAX_IMPORTS
        )));
    }

    let languages: Vec<String> = detection
        .languages
        .iter()
        .map(|(name, files)| {
            format!(
                "{{\"language\":\"{}\",\"files\":{files}}}",
                json::escape(name)
            )
        })
        .collect();
    let manifests: Vec<String> = detection
        .manifests
        .iter()
        .map(|m| format!("\"{}\"", json::escape(m)))
        .collect();
    let providers: Vec<String> = present
        .iter()
        .zip(guided_by_family.iter())
        .map(|(family, guided_entry)| {
            let producer = match guided_entry {
                Some(g) => producer_json(g),
                None => "null".to_string(),
            };
            format!(
                "{{\"family\":\"{}\",\"capabilities\":{{\"scip_import\":\"{}\",\"lsp\":\"{}\"}},\"producer\":{producer},\"lsp\":{{\"source\":\"{}\",\"state\":\"{}\",\"expected\":{}}}}}",
                family.name,
                family.scip_import,
                family.lsp,
                lsp.1,
                lsp_state(family),
                json_opt(family.expected_server),
            )
        })
        .collect();
    let imports: Vec<String> = view
        .rows
        .iter()
        .take(MAX_IMPORTS)
        .map(|r| {
            format!(
                "{{\"import\":{},\"provider\":\"{}\",\"provider_version\":\"{}\",\"verification\":\"{}\"}}",
                r.import_id,
                json::escape(&r.provider_name),
                json::escape(&r.provider_version),
                json::escape(&r.verification),
            )
        })
        .collect();
    let next: Vec<String> = steps
        .iter()
        .map(|s| {
            format!(
                "{{\"step\":\"{}\",\"command\":{}}}",
                json::escape(&s.text),
                json_opt(s.command.as_deref()),
            )
        })
        .collect();
    let reasons: Vec<String> = view
        .reasons
        .iter()
        .map(|r| format!("\"{}\"", json::escape(r)))
        .collect();
    let snapshot = match view.snapshot {
        Some(id) => id.to_string(),
        None => "null".to_string(),
    };
    format!(
        "{{\"repo\":\"{}\",\"shell\":\"{}\",\"index\":{{\"status\":\"{}\",\"snapshot\":{},\"freshness\":{},\"reasons\":[{}]}},\"scan\":{{\"complete\":{},\"unreadable_dirs\":{},\"escaped_dirs\":{}}},\"languages\":[{}],\"manifests\":{{\"items\":[{}],\"omitted\":{}}},\"providers\":[{}],\"imports\":[{}],\"next\":[{}],\"note\":\"riltra read files, the index, git metadata and PATH only; it installed nothing and ran no provider\"}}",
        json::escape(repo),
        shell.label(),
        view.status,
        snapshot,
        json_opt(view.freshness),
        reasons.join(","),
        scan_complete,
        detection.unreadable_dirs,
        detection.escaped_dirs,
        languages.join(","),
        manifests.join(","),
        detection.manifests_omitted,
        providers.join(","),
        imports.join(","),
        next.join(","),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{SemanticState, SemanticStatusRow, SetupIndex};
    use crate::json;
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
            assert_eq!(exec_exts(), &[".exe", ".cmd", ".bat"][..]);
        } else {
            assert_eq!(exec_exts(), &[""][..]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn find_producer_skips_extensionless_npm_shim_on_windows() {
        let root = tmpdir("ps-producer-shim-local");
        write(&root, "node_modules/.bin/scip-typescript", "#!/bin/sh\n");
        write(
            &root,
            "node_modules/.bin/scip-typescript.cmd",
            "@echo off\n",
        );
        let ts = family("typescript").producer.expect("producer");
        assert_eq!(
            find_producer(&root, ts, &[]),
            Some(Hit::Local("scip-typescript.cmd".to_string()))
        );
    }

    #[cfg(windows)]
    #[test]
    fn find_producer_skips_extensionless_shim_on_path_on_windows() {
        let root = tmpdir("ps-producer-shim-path");
        let path_root = tmpdir("ps-producer-shim-path-dir");
        write(&path_root, "scip-typescript", "#!/bin/sh\n");
        let cmd = write(&path_root, "scip-typescript.cmd", "@echo off\n");
        let ts = family("typescript").producer.expect("producer");
        assert_eq!(
            find_producer(&root, ts, std::slice::from_ref(&path_root)),
            Some(Hit::Path(cmd))
        );
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

    /// Executable file name for `stem` on this platform's lookup extensions.
    fn exe(stem: &str) -> String {
        if cfg!(windows) {
            format!("{stem}.cmd")
        } else {
            stem.to_string()
        }
    }

    fn detection(langs: &[(&'static str, usize)], manifests: &[&str]) -> Detection {
        Detection {
            languages: langs.iter().copied().collect(),
            manifests: manifests.iter().map(|m| m.to_string()).collect(),
            manifests_omitted: 0,
            unreadable_dirs: 0,
            escaped_dirs: 0,
        }
    }

    fn ready(freshness: &str, rows: Vec<SemanticStatusRow>) -> SetupIndex {
        SetupIndex::Ready(SemanticState {
            snapshot: None,
            freshness: Some(freshness.to_string()),
            reasons: Vec::new(),
            rows,
        })
    }

    fn import_row(id: i64, provider: &str, verification: &str) -> SemanticStatusRow {
        SemanticStatusRow {
            import_id: id,
            provider_name: provider.to_string(),
            provider_version: "1.0.0".to_string(),
            capability: "scip-import".to_string(),
            verification: verification.to_string(),
            artifact_id: "artifact".to_string(),
            claimed_source_digest: "digest".to_string(),
            content_match: true,
            revision_match: true,
            limitation: "limited".to_string(),
        }
    }

    /// The text between `"next":[` and `],"note"`, exactly as written.
    fn next_part(json: &str) -> &str {
        let open = "\"next\":[";
        let start = json.find(open).expect("next key") + open.len();
        let end = json[start..].find("],\"note\"").expect("note key") + start;
        &json[start..end]
    }

    const LSP_OPTIONAL: &str = "{\"step\":\"optional: set RILTRA_LSP_SERVER to the absolute path of rust-analyzer for riltra_code_semantic\",\"command\":null}";

    #[test]
    fn render_quote_posix_sh_follows_d9() {
        assert_eq!(quote(Shell::PosixSh, "r"), "r");
        assert_eq!(quote(Shell::PosixSh, "index.scip"), "index.scip");
        assert_eq!(quote(Shell::PosixSh, "."), ".");
        assert_eq!(quote(Shell::PosixSh, "-x"), "-x");
        assert_eq!(quote(Shell::PosixSh, ""), "''");
        assert_eq!(quote(Shell::PosixSh, "my repo;x"), "'my repo;x'");
        assert_eq!(quote(Shell::PosixSh, "it's"), "'it'\\''s'");
    }

    #[test]
    fn render_quote_powershell_follows_d9() {
        assert_eq!(quote(Shell::PowerShell, "r"), "r");
        assert_eq!(quote(Shell::PowerShell, "index.scip"), "index.scip");
        assert_eq!(quote(Shell::PowerShell, "."), ".");
        assert_eq!(quote(Shell::PowerShell, "-x"), "'-x'");
        assert_eq!(quote(Shell::PowerShell, ""), "''");
        assert_eq!(quote(Shell::PowerShell, "my repo;x"), "'my repo;x'");
        assert_eq!(quote(Shell::PowerShell, "it's"), "'it''s'");
    }

    #[test]
    fn render_program_follows_d9() {
        let path_hit = Hit::Path(PathBuf::from("/usr/bin/rust-analyzer"));
        let local_plain = Hit::Local("scip-typescript".to_string());
        let local_cmd = Hit::Local("scip-typescript.cmd".to_string());
        assert_eq!(render_program(Shell::PosixSh, None, "riltra"), "riltra");
        assert_eq!(render_program(Shell::PowerShell, None, "riltra"), "riltra");
        assert_eq!(
            render_program(Shell::PosixSh, Some(&path_hit), "rust-analyzer"),
            "rust-analyzer"
        );
        assert_eq!(
            render_program(Shell::PowerShell, Some(&path_hit), "rust-analyzer"),
            "rust-analyzer"
        );
        assert_eq!(
            render_program(Shell::PosixSh, Some(&local_plain), "scip-typescript"),
            "./node_modules/.bin/scip-typescript"
        );
        assert_eq!(
            render_program(Shell::PowerShell, Some(&local_cmd), "scip-typescript"),
            "& '.\\node_modules\\.bin\\scip-typescript.cmd'"
        );
    }

    #[test]
    fn shell_labels_match_d4() {
        assert_eq!(Shell::PosixSh.label(), "posix-sh");
        assert_eq!(Shell::PowerShell.label(), "powershell");
    }

    #[test]
    fn report_shape_no_index() {
        let root = tmpdir("ps-report-shape");
        write(&root, "Cargo.toml", "[package]\n");
        let det = detection(&[("rust", 1)], &["Cargo.toml"]);
        let out = report(
            "r",
            &root,
            &det,
            &SetupIndex::NoIndex,
            (None, "none"),
            &[],
            Shell::PosixSh,
        );
        let expected = concat!(
            "{\"repo\":\"r\",\"shell\":\"posix-sh\",",
            "\"index\":{\"status\":\"no-index\",\"snapshot\":null,\"freshness\":null,\"reasons\":[]},",
            "\"scan\":{\"complete\":true,\"unreadable_dirs\":0,\"escaped_dirs\":0},",
            "\"languages\":[{\"language\":\"rust\",\"files\":1}],",
            "\"manifests\":{\"items\":[\"Cargo.toml\"],\"omitted\":0},",
            "\"providers\":[{\"family\":\"rust\",\"capabilities\":{\"scip_import\":\"verified-fixture\",\"lsp\":\"contract-only\"},",
            "\"producer\":{\"name\":\"rust-analyzer\",\"state\":\"not-found\",\"path\":null,\"needs\":\"Cargo.toml\",\"needs_met\":true,\"command\":null},",
            "\"lsp\":{\"source\":\"none\",\"state\":\"not-configured\",\"expected\":\"rust-analyzer\"}}],",
            "\"imports\":[],",
            "\"next\":[",
            "{\"step\":\"build the code index\",\"command\":\"riltra index --repo r\"},",
            "{\"step\":\"install rust-analyzer yourself; riltra does not install providers\",\"command\":null},",
            "{\"step\":\"optional: set RILTRA_LSP_SERVER to the absolute path of rust-analyzer for riltra_code_semantic\",\"command\":null}",
            "],",
            "\"note\":\"riltra read files, the index, git metadata and PATH only; it installed nothing and ran no provider\"}"
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn report_found_producer_steps() {
        let root = tmpdir("ps-report-found");
        write(&root, "Cargo.toml", "[package]\n");
        let path_dir = tmpdir("ps-report-found-path");
        write(&path_dir, &exe("rust-analyzer"), "x");
        let det = detection(&[("rust", 1)], &["Cargo.toml"]);
        let dirs = [path_dir];
        let out = report(
            "r",
            &root,
            &det,
            &ready("fresh", Vec::new()),
            (None, "none"),
            &dirs,
            Shell::PosixSh,
        );
        assert!(out.contains("\"state\":\"found\""), "{out}");
        assert!(
            out.contains(
                "\"index\":{\"status\":\"ready\",\"snapshot\":null,\"freshness\":\"fresh\",\"reasons\":[]}"
            ),
            "{out}"
        );
        assert_eq!(
            next_part(&out),
            format!(
                "{}{}{}",
                "{\"step\":\"produce a SCIP index with rust-analyzer\",\"command\":\"rust-analyzer scip .\"},",
                "{\"step\":\"import the SCIP index\",\"command\":\"riltra semantic import --repo r --scip index.scip\"},",
                LSP_OPTIONAL
            )
        );
    }

    #[test]
    fn report_unverified_import_skips_producer_steps() {
        let root = tmpdir("ps-report-unverified");
        write(&root, "Cargo.toml", "[package]\n");
        let path_dir = tmpdir("ps-report-unverified-path");
        write(&path_dir, &exe("rust-analyzer"), "x");
        let det = detection(&[("rust", 1)], &["Cargo.toml"]);
        let rows = vec![import_row(4, "rust-analyzer", "Unverified")];
        let dirs = [path_dir];
        let found = report(
            "r",
            &root,
            &det,
            &ready("fresh", rows),
            (None, "none"),
            &dirs,
            Shell::PosixSh,
        );
        assert_eq!(next_part(&found), LSP_OPTIONAL);
        let rows = vec![import_row(4, "rust-analyzer", "Unverified")];
        let missing = report(
            "r",
            &root,
            &det,
            &ready("fresh", rows),
            (None, "none"),
            &[],
            Shell::PosixSh,
        );
        assert_eq!(next_part(&missing), LSP_OPTIONAL);
    }

    #[test]
    fn report_stale_import_step() {
        let root = tmpdir("ps-report-stale");
        write(&root, "Cargo.toml", "[package]\n");
        let path_dir = tmpdir("ps-report-stale-path");
        write(&path_dir, &exe("rust-analyzer"), "x");
        let det = detection(&[("rust", 1)], &["Cargo.toml"]);
        let rows = vec![import_row(9, "rust-analyzer", "Stale")];
        let dirs = [path_dir];
        let out = report(
            "r",
            &root,
            &det,
            &ready("fresh", rows),
            (None, "none"),
            &dirs,
            Shell::PosixSh,
        );
        assert_eq!(
            next_part(&out),
            format!(
                "{}{}{}{}",
                "{\"step\":\"produce a SCIP index with rust-analyzer\",\"command\":\"rust-analyzer scip .\"},",
                "{\"step\":\"import the SCIP index\",\"command\":\"riltra semantic import --repo r --scip index.scip\"},",
                "{\"step\":\"re-run rust-analyzer and import again; import 9 no longer matches the source\",\"command\":null},",
                LSP_OPTIONAL
            )
        );
    }

    #[test]
    fn report_ts_without_tsconfig() {
        let root = tmpdir("ps-report-ts");
        let path_dir = tmpdir("ps-report-ts-path");
        write(&path_dir, &exe("scip-typescript"), "x");
        let det = detection(&[("typescript", 1)], &[]);
        let dirs = [path_dir];
        let out = report(
            "r",
            &root,
            &det,
            &ready("fresh", Vec::new()),
            (None, "none"),
            &dirs,
            Shell::PosixSh,
        );
        assert!(
            out.contains("\"producer\":{\"name\":\"scip-typescript\",\"state\":\"found\""),
            "{out}"
        );
        assert!(
            out.contains("\"needs\":\"tsconfig.json\",\"needs_met\":false,\"command\":\"scip-typescript index --infer-tsconfig\""),
            "{out}"
        );
        assert_eq!(
            next_part(&out),
            concat!(
                "{\"step\":\"produce a SCIP index with scip-typescript\",\"command\":\"scip-typescript index --infer-tsconfig\"},",
                "{\"step\":\"import the SCIP index\",\"command\":\"riltra semantic import --repo r --scip index.scip\"}"
            )
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn report_local_producer_command_posix_sh() {
        let root = tmpdir("ps-report-local-posix");
        write(&root, "node_modules/.bin/scip-typescript", "x");
        let det = detection(&[("typescript", 1)], &[]);
        let out = report(
            "r",
            &root,
            &det,
            &ready("fresh", Vec::new()),
            (None, "none"),
            &[],
            Shell::PosixSh,
        );
        let cmd = "./node_modules/.bin/scip-typescript index --infer-tsconfig";
        assert!(out.contains("\"state\":\"found\""), "{out}");
        assert!(out.contains(&format!("\"command\":\"{cmd}\"}}")), "{out}");
        assert!(
            out.contains(&format!(
                "\"step\":\"produce a SCIP index with scip-typescript\",\"command\":\"{cmd}\""
            )),
            "{out}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn report_local_producer_command_powershell() {
        let root = tmpdir("ps-report-local-ps");
        write(&root, "node_modules/.bin/scip-typescript.cmd", "x");
        write(&root, "tsconfig.json", "{}\n");
        let det = detection(&[("typescript", 1)], &["tsconfig.json"]);
        let out = report(
            "r",
            &root,
            &det,
            &ready("fresh", Vec::new()),
            (None, "none"),
            &[],
            Shell::PowerShell,
        );
        let cmd = "& '.\\node_modules\\.bin\\scip-typescript.cmd' index";
        assert!(
            out.contains(&format!("\"command\":\"{}\"}}", json::escape(cmd))),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "\"step\":\"produce a SCIP index with scip-typescript\",\"command\":\"{}\"",
                json::escape(cmd)
            )),
            "{out}"
        );
    }

    #[test]
    fn report_quotes_repo_name() {
        let root = tmpdir("ps-report-quote");
        write(&root, "Cargo.toml", "[package]\n");
        let path_dir = tmpdir("ps-report-quote-path");
        write(&path_dir, &exe("rust-analyzer"), "x");
        let det = detection(&[("rust", 1)], &["Cargo.toml"]);
        let dirs = [path_dir];
        let out = report(
            "my repo;x",
            &root,
            &det,
            &SetupIndex::NoIndex,
            (None, "none"),
            &dirs,
            Shell::PosixSh,
        );
        assert!(out.contains("\"repo\":\"my repo;x\""), "{out}");
        assert_eq!(
            next_part(&out),
            format!(
                "{}{}{}{}",
                "{\"step\":\"build the code index\",\"command\":\"riltra index --repo 'my repo;x'\"},",
                "{\"step\":\"produce a SCIP index with rust-analyzer\",\"command\":\"rust-analyzer scip .\"},",
                "{\"step\":\"import the SCIP index\",\"command\":\"riltra semantic import --repo 'my repo;x' --scip index.scip\"},",
                LSP_OPTIONAL
            )
        );
    }

    #[test]
    fn report_partial_scan_step() {
        let root = tmpdir("ps-report-partial");
        let det = Detection {
            languages: BTreeMap::new(),
            manifests: Vec::new(),
            manifests_omitted: 0,
            unreadable_dirs: 2,
            escaped_dirs: 0,
        };
        let out = report(
            "r",
            &root,
            &det,
            &SetupIndex::NoIndex,
            (None, "none"),
            &[],
            Shell::PosixSh,
        );
        assert!(
            out.contains("\"scan\":{\"complete\":false,\"unreadable_dirs\":2,\"escaped_dirs\":0}"),
            "{out}"
        );
        assert_eq!(
            next_part(&out),
            concat!(
                "{\"step\":\"build the code index\",\"command\":\"riltra index --repo r\"},",
                "{\"step\":\"some directories could not be read or point outside the repository; languages and manifests may be incomplete\",\"command\":null}"
            )
        );
    }

    #[test]
    fn report_mixed_and_empty() {
        let root = tmpdir("ps-report-mixed");
        let det = detection(&[("python", 1), ("rust", 1), ("typescript", 1)], &[]);
        let out = report(
            "r",
            &root,
            &det,
            &SetupIndex::NoIndex,
            (None, "none"),
            &[],
            Shell::PosixSh,
        );
        let ts = out.find("\"family\":\"typescript\"").expect("typescript");
        let rs = out.find("\"family\":\"rust\"").expect("rust");
        let py = out.find("\"family\":\"python\"").expect("python");
        assert!(ts < rs && rs < py, "{out}");
        assert!(
            out.contains(
                "\"family\":\"python\",\"capabilities\":{\"scip_import\":\"contract-only\",\"lsp\":\"contract-only\"},\"producer\":null"
            ),
            "{out}"
        );

        let empty = detection(&[], &[]);
        let out = report(
            "r",
            &root,
            &empty,
            &SetupIndex::NoIndex,
            (None, "none"),
            &[],
            Shell::PosixSh,
        );
        assert!(out.contains("\"languages\":[]"), "{out}");
        assert!(out.contains("\"providers\":[]"), "{out}");
    }
}
