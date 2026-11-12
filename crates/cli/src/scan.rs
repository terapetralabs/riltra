// SPDX-License-Identifier: Apache-2.0
//! M2 repository scan: deterministic walk plus per-file syntax facts.
//!
//! Policy (explicit, stable): sorted entries, symlinks never followed,
//! `.git` never entered, `.riltraignore` honored, per-file size cap,
//! binary/non-UTF8 refused, unknown extensions and CUDA reported as
//! unsupported — never parsed. The envelope is byte-deterministic for
//! identical input (no timings inside); durations are measured externally.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ignore::{is_ignored, load_rules};
use crate::json::escape;
use crate::paths::contains;
use crate::syntax::{classify, file_facts_json, language_name, parse_source, FileKind, Language};

pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_UNSUPPORTED_EXAMPLES: usize = 50;

pub struct ScanOptions {
    /// Explicit relative paths; empty means a full walk of the root.
    pub paths: Vec<String>,
    /// Case-sensitive substring filter on definition names (ok entries only).
    pub symbol: Option<String>,
}

/// One scanned file: its JSON plus its definition names (for `--symbol`).
struct Entry {
    json: String,
    def_names: Vec<String>,
}

/// Walk decision per file, shared by JSON scanning and index building so both
/// enforce identical policy (sorted walk, no symlink following, `.git` and
/// ignore skips, containment). Content checks (size, binary) stay with the
/// reader as a TOCTOU guard.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanAction {
    Parse(Language),
    Skip(&'static str),
    Unsupported(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFile {
    pub rel: String,
    pub full: PathBuf,
    pub action: PlanAction,
}

/// Full deterministic walk of `root`, returning one plan entry per file.
/// Directory-level skips (escapes, unreadable dirs) ride along separately
/// since directories produce no file entries.
pub fn plan_walk(root: &Path) -> Result<(Vec<PlannedFile>, DirSkips), String> {
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve root: {e}"))?;
    let (rules, _) = load_rules(root);
    let mut plan = Vec::new();
    let mut skips = DirSkips::default();
    walk_plan(&canon_root, &canon_root, "", &rules, &mut plan, &mut skips);
    Ok((plan, skips))
}

#[derive(Debug, Default)]
pub struct DirSkips {
    pub escape: usize,
    pub unreadable: usize,
}

#[derive(Default)]
struct Counters {
    discovered: usize,
    processed: usize,
    definitions: usize,
    imports: usize,
    calls: usize,
    errors: u32,
    skipped_ignored: usize,
    skipped_large: usize,
    skipped_binary: usize,
    skipped_symlink: usize,
    skipped_escape: usize,
    skipped_unreadable: usize,
    unsupported_total: usize,
}

fn skipped_json(rel: &str, reason: &str) -> String {
    format!(
        "{{\"path\":\"{}\",\"status\":\"skipped\",\"reason\":\"{}\"}}",
        escape(rel),
        reason
    )
}

fn unsupported_json(rel: &str, reason: &str) -> String {
    format!(
        "{{\"path\":\"{}\",\"status\":\"unsupported\",\"reason\":\"{}\"}}",
        escape(rel),
        reason
    )
}

pub(crate) fn unsupported_reason(kind: &FileKind) -> &'static str {
    match kind {
        FileKind::Supported(_) => "supported",
        FileKind::CudaExperimental => "cuda-experimental",
        FileKind::UnsupportedExtension => "unsupported-extension",
    }
}

/// Render one planned file into JSON, enforcing content checks at read time
/// (size cap, binary refusal) as a TOCTOU guard behind the walk decision.
fn render_planned(
    full: &Path,
    rel: &str,
    action: &PlanAction,
    entries: &mut Vec<Entry>,
    counters: &mut Counters,
    unsupported: &mut Vec<(String, String)>,
    languages: &mut BTreeMap<String, u32>,
) {
    counters.discovered += 1;
    if let PlanAction::Skip(reason) = action {
        match *reason {
            "ignored" => counters.skipped_ignored += 1,
            "symlink-not-followed" => counters.skipped_symlink += 1,
            _ => counters.skipped_unreadable += 1,
        }
        entries.push(Entry {
            json: skipped_json(rel, reason),
            def_names: Vec::new(),
        });
        return;
    }
    // Content gates run before the extension verdict, exactly like the
    // original scan order: a huge or binary `.xyz` reports skipped, not
    // unsupported.
    let too_large = std::fs::metadata(full)
        .map(|m| m.len() > MAX_FILE_BYTES)
        .unwrap_or(false);
    if too_large {
        counters.skipped_large += 1;
        entries.push(Entry {
            json: skipped_json(rel, "too-large"),
            def_names: Vec::new(),
        });
        return;
    }
    let bytes = match std::fs::read(full) {
        Ok(b) => b,
        Err(_) => {
            counters.skipped_unreadable += 1;
            entries.push(Entry {
                json: skipped_json(rel, "unreadable"),
                def_names: Vec::new(),
            });
            return;
        }
    };
    if bytes.len() as u64 > MAX_FILE_BYTES {
        counters.skipped_large += 1;
        entries.push(Entry {
            json: skipped_json(rel, "too-large"),
            def_names: Vec::new(),
        });
        return;
    }
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        counters.skipped_binary += 1;
        entries.push(Entry {
            json: skipped_json(rel, "binary-or-non-utf8"),
            def_names: Vec::new(),
        });
        return;
    }
    match action {
        // Skips returned early above; listed for exhaustiveness only.
        PlanAction::Skip(_) => {}
        PlanAction::Unsupported(reason) => {
            counters.unsupported_total += 1;
            unsupported.push((rel.to_string(), reason.to_string()));
            entries.push(Entry {
                json: unsupported_json(rel, reason),
                def_names: Vec::new(),
            });
        }
        PlanAction::Parse(lang) => {
            let facts = parse_source(*lang, &bytes);
            crate::progress::tick();
            counters.processed += 1;
            counters.definitions += facts.definitions.len();
            counters.imports += facts.imports.len();
            counters.calls += facts.calls.len();
            counters.errors += facts.errors;
            *languages
                .entry(language_name(*lang).to_string())
                .or_insert(0) += 1;
            let def_names: Vec<String> = facts
                .definitions
                .iter()
                .filter_map(|f| f.name.clone())
                .collect();
            entries.push(Entry {
                json: file_facts_json(rel, &facts),
                def_names,
            });
        }
    }
}

fn sorted_entries(rd: std::fs::ReadDir) -> Vec<(OsString, std::fs::FileType)> {
    let mut entries: Vec<(OsString, std::fs::FileType)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_type().ok().map(|t| (e.file_name(), t)))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

fn walk_plan(
    canon_root: &Path,
    dir: &Path,
    prefix: &str,
    rules: &[String],
    plan: &mut Vec<PlannedFile>,
    skips: &mut DirSkips,
) {
    let dir_entries = match std::fs::read_dir(dir) {
        Ok(rd) => sorted_entries(rd),
        Err(_) => {
            skips.unreadable += 1;
            return;
        }
    };
    for (name, file_type) in dir_entries {
        let name = name.to_string_lossy().replace('\\', "/");
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let full = dir.join(&name);
        if file_type.is_symlink() {
            plan.push(PlannedFile {
                rel,
                full,
                action: PlanAction::Skip("symlink-not-followed"),
            });
            continue;
        }
        if file_type.is_dir() {
            if name == ".git" || is_ignored(rules, &rel) {
                continue; // default + rule skips: subtrees never entered, uncounted
            }
            match full.canonicalize() {
                Ok(c) if contains(canon_root, &c) => {
                    walk_plan(canon_root, &full, &rel, rules, plan, skips)
                }
                _ => {
                    skips.escape += 1;
                }
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if is_ignored(rules, &rel) {
            plan.push(PlannedFile {
                rel,
                full,
                action: PlanAction::Skip("ignored"),
            });
            continue;
        }
        let action = match classify(Path::new(&rel)) {
            FileKind::Supported(lang) => PlanAction::Parse(lang),
            kind => PlanAction::Unsupported(unsupported_reason(&kind)),
        };
        plan.push(PlannedFile { rel, full, action });
    }
}

fn normalize_rel(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("empty --path".to_string());
    }
    let rel = raw.replace('\\', "/");
    let rel = rel.strip_prefix("./").unwrap_or(&rel).to_string();
    if rel.is_empty() || rel == "." || Path::new(&rel).is_absolute() {
        return Err(format!("--path must be a relative file path, got '{raw}'"));
    }
    Ok(rel)
}

/// Scan `root` (already canonical). `name` labels the envelope. Errors only
/// for unusable explicit `--path` values; per-file problems are data.
pub fn scan_repo(name: &str, root: &Path, opts: &ScanOptions) -> Result<String, String> {
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve root: {e}"))?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut counters = Counters::default();
    let mut unsupported: Vec<(String, String)> = Vec::new();
    let mut languages: BTreeMap<String, u32> = BTreeMap::new();

    if opts.paths.is_empty() {
        let (plan, skips) = plan_walk(&canon_root)?;
        counters.skipped_escape += skips.escape;
        counters.skipped_unreadable += skips.unreadable;
        for item in &plan {
            render_planned(
                &item.full,
                &item.rel,
                &item.action,
                &mut entries,
                &mut counters,
                &mut unsupported,
                &mut languages,
            );
        }
    } else {
        for raw in &opts.paths {
            let rel = normalize_rel(raw)?;
            let full = canon_root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            let canon = full
                .canonicalize()
                .map_err(|_| format!("--path not found: '{raw}'"))?;
            if !contains(&canon_root, &canon) {
                return Err(format!("--path escapes repository root: '{raw}'"));
            }
            if !canon.is_file() {
                return Err(format!("--path must be a file: '{raw}'"));
            }
            let action = match classify(Path::new(&rel)) {
                FileKind::Supported(lang) => PlanAction::Parse(lang),
                kind => PlanAction::Unsupported(unsupported_reason(&kind)),
            };
            render_planned(
                &canon,
                &rel,
                &action,
                &mut entries,
                &mut counters,
                &mut unsupported,
                &mut languages,
            );
        }
    }

    if let Some(substr) = &opts.symbol {
        entries.retain(|entry| {
            entry.json.contains("\"status\":\"skipped\"")
                || entry.json.contains("\"status\":\"unsupported\"")
                || entry.def_names.iter().any(|n| n.contains(substr.as_str()))
        });
    }

    unsupported.sort();
    unsupported.truncate(MAX_UNSUPPORTED_EXAMPLES);
    let unsupported_json: Vec<String> = unsupported
        .iter()
        .map(|(p, r)| format!("{{\"path\":\"{}\",\"reason\":\"{}\"}}", escape(p), r))
        .collect();
    let languages_json: Vec<String> = languages
        .iter()
        .map(|(l, n)| format!("\"{l}\":{n}"))
        .collect();
    let root_s = canon_root.to_string_lossy().to_string();
    let files_json: Vec<String> = entries.into_iter().map(|e| e.json).collect();
    Ok(format!(
        "{{\"repo\":\"{}\",\"root\":\"{}\",\"filter\":{{\"symbol\":{}}},\"policy\":{{\"max_file_bytes\":{},\"follow_symlinks\":false,\"default_skips\":[\".git\"]}},\"files_discovered\":{},\"files\":[{}],\"summary\":{{\"processed\":{},\"definitions\":{},\"imports\":{},\"calls\":{},\"errors\":{},\"skipped_ignored\":{},\"skipped_large\":{},\"skipped_binary\":{},\"skipped_symlink\":{},\"skipped_escape\":{},\"skipped_unreadable\":{},\"unsupported_count\":{},\"unsupported_examples\":[{}],\"languages\":{{{}}}}}}}",
        escape(name),
        escape(&root_s),
        match &opts.symbol {
            Some(s) => format!("\"{}\"", escape(s)),
            None => "null".to_string(),
        },
        MAX_FILE_BYTES,
        counters.discovered,
        files_json.join(","),
        counters.processed,
        counters.definitions,
        counters.imports,
        counters.calls,
        counters.errors,
        counters.skipped_ignored,
        counters.skipped_large,
        counters.skipped_binary,
        counters.skipped_symlink,
        counters.skipped_escape,
        counters.skipped_unreadable,
        counters.unsupported_total,
        unsupported_json.join(","),
        languages_json.join(",")
    ))
}

#[cfg(test)]
mod tests {
    use super::{scan_repo, ScanOptions};
    use crate::testutil::tmpdir;

    fn no_filter() -> ScanOptions {
        ScanOptions {
            paths: Vec::new(),
            symbol: None,
        }
    }

    fn write_tree() -> std::path::PathBuf {
        let dir = tmpdir("scan-tree");
        std::fs::create_dir_all(dir.join(".git").join("objects")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("a.py"), b"def f():\n    return g()\n").unwrap();
        std::fs::write(dir.join("b.xyz"), b"nope").unwrap();
        std::fs::write(dir.join("big.py"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        std::fs::write(dir.join("bin.py"), b"\x00\x01\x02").unwrap();
        dir
    }

    #[test]
    fn applies_policy_skips_and_counts() {
        let dir = write_tree();
        let json = scan_repo("t", &dir, &no_filter()).unwrap();
        assert!(json.contains("\"files_discovered\":4"), "got: {json}");
        assert!(json.contains("\"processed\":1"));
        assert!(json.contains("\"skipped_large\":1"));
        assert!(json.contains("\"skipped_binary\":1"));
        assert!(json.contains("\"unsupported_count\":1"));
        assert!(json.contains("\"languages\":{\"python\":1}"));
        assert!(!json.contains(".git/"), ".git must never be entered");
    }

    #[test]
    fn skips_symlinks_without_following() {
        let dir = tmpdir("scan-link");
        let outside = tmpdir("scan-link-out");
        std::fs::write(outside.join("s.py"), b"def h():\n    pass\n").unwrap();
        std::fs::write(dir.join("a.py"), b"def f():\n    pass\n").unwrap();
        let link = dir.join("evil.py");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.join("s.py"), &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(outside.join("s.py"), &link).is_ok();
        if !made {
            return;
        }
        let json = scan_repo("t", &dir, &no_filter()).unwrap();
        assert!(json.contains("\"skipped_symlink\":1"), "got: {json}");
        assert!(!json.contains("def h"), "linked content must not leak in");
    }

    #[test]
    fn rejects_escaping_and_missing_paths() {
        let dir = tmpdir("scan-paths");
        std::fs::write(dir.join("a.py"), b"x = 1\n").unwrap();
        let opts = ScanOptions {
            paths: vec!["../evil.py".to_string()],
            symbol: None,
        };
        assert!(scan_repo("t", &dir, &opts).is_err());
        let opts = ScanOptions {
            paths: vec!["missing.py".to_string()],
            symbol: None,
        };
        assert!(scan_repo("t", &dir, &opts).is_err());
        let opts = ScanOptions {
            paths: vec![".".to_string()],
            symbol: None,
        };
        assert!(scan_repo("t", &dir, &opts).is_err());
    }

    #[test]
    fn symbol_filter_keeps_matching_definitions() {
        let dir = tmpdir("scan-symbol");
        std::fs::write(dir.join("a.py"), b"def alpha():\n    pass\n").unwrap();
        std::fs::write(dir.join("b.py"), b"def beta():\n    pass\n").unwrap();
        let opts = ScanOptions {
            paths: Vec::new(),
            symbol: Some("alpha".to_string()),
        };
        let json = scan_repo("t", &dir, &opts).unwrap();
        assert!(json.contains("\"name\":\"alpha\""), "got: {json}");
        assert!(!json.contains("\"name\":\"beta\""), "got: {json}");
    }

    #[test]
    fn full_output_is_byte_deterministic() {
        let dir = write_tree();
        let a = scan_repo("t", &dir, &no_filter()).unwrap();
        let b = scan_repo("t", &dir, &no_filter()).unwrap();
        assert_eq!(a, b);
    }
}
