// SPDX-License-Identifier: Apache-2.0
//! Paired files for a grep hit (`companions` in the grep document). Three
//! matchers name them, each with its own evidence label:
//!
//! - `name-pair` (`path-convention`): same base name, extension of the same
//!   group (C/C++ headers, JS/TS, lex/yacc, Python stubs). From the walk only.
//! - `importer` (`import-text-match`): a file whose indexed import text names
//!   the file of the hit. A guess from text, never a proven import.
//! - `caller` (`CALLS` or `POTENTIAL_CALL`): a file that calls a definition
//!   enclosing a hit. The same edge query the `callers` tool runs.

use std::collections::HashMap;
use std::path::Path;

use crate::{index, scan, semantic, store};

/// One paired file: why it was named, on what basis, and through which name.
pub(crate) struct Companion {
    pub path: String,
    pub why: &'static str,
    pub basis: &'static str,
    pub via: String,
}

/// Per kind of `why`, at most this many companions.
const MAX_PER_KIND: usize = 3;

/// Extension groups of the name pairs. A pair shares its base name and has
/// an extension from the same group. Longer extensions are tried first, so
/// `index.d.ts` has base `index`, not `index.d`.
const GROUPS: [&[&str]; 4] = [
    &[".c", ".h", ".cc", ".cpp", ".hpp", ".hh"],
    &[".js", ".mjs", ".cjs", ".ts", ".tsx", ".d.ts"],
    &[".y", ".l"],
    &[".py", ".pyi"],
];
const C_GROUP: usize = 0;
const JS_GROUP: usize = 1;
const PYTHON_GROUP: usize = 3;

/// The base name and group of a path whose file name has a group extension.
fn pair_key(path: &str) -> Option<(&str, usize)> {
    let name = file_name(path);
    let mut best: Option<(&str, usize)> = None;
    let mut best_len = 0;
    for (group, exts) in GROUPS.iter().enumerate() {
        for ext in exts.iter() {
            if name.len() > ext.len() && name.ends_with(ext) && ext.len() > best_len {
                best_len = ext.len();
                best = Some((&name[..name.len() - ext.len()], group));
            }
        }
    }
    best
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Files of the walk that pair with `path` by base name and extension group,
/// excluding the file itself and test files. A file in the same folder comes
/// first, then the rest by path.
pub(crate) fn name_pairs(path: &str, walk: &[String]) -> Vec<Companion> {
    let Some((stem, group)) = pair_key(path) else {
        return Vec::new();
    };
    let folder = parent(path);
    let mut found: Vec<(bool, &str)> = walk
        .iter()
        .filter(|cand| cand.as_str() != path && !index::is_test_path(cand))
        .filter(|cand| pair_key(cand) == Some((stem, group)))
        .map(|cand| (parent(cand) != folder, cand.as_str()))
        .collect();
    found.sort();
    found
        .into_iter()
        .take(MAX_PER_KIND)
        .map(|(_, cand)| Companion {
            path: cand.to_string(),
            why: "name-pair",
            basis: "path-convention",
            via: stem.to_string(),
        })
        .collect()
}

/// Files whose indexed import text names `path`, one per importing file.
/// Reads the imports table only; `Err` on a storage failure.
// shortcut: no importer rule for Go package roots (`.` import path) or
// Rust `mod` declarations; `mod` is not an import fact in the index, so the
// text needed to match it is not stored. Upgrade when mod items are indexed.
pub(crate) fn importers(
    tx: &rusqlite::Transaction<'_>,
    snapshot_id: i64,
    path: &str,
) -> Result<Vec<Companion>, String> {
    let names: HashMap<i64, String> = store::file_id_map(tx, snapshot_id)?
        .into_iter()
        .map(|(rel, (id, _))| (id, rel))
        .collect();
    let mut found: Vec<(String, String)> = Vec::new();
    for row in store::imports_for(tx, snapshot_id)? {
        let importer = names
            .get(&row.file_id)
            .ok_or_else(|| format!("import row of unknown file id {}", row.file_id))?;
        if importer == path || index::is_test_path(importer) {
            continue;
        }
        if found.iter().any(|(seen, _)| seen == importer) {
            continue;
        }
        // The text is `None` only for bytes that are not UTF-8: such an import
        // cannot be matched as text and is not a guess either.
        let Some(text) = row.text.as_deref() else {
            continue;
        };
        if let Some(via) = import_match(path, importer, text) {
            found.push((importer.clone(), via));
        }
    }
    found.sort();
    Ok(found
        .into_iter()
        .take(MAX_PER_KIND)
        .map(|(importer, via)| Companion {
            path: importer,
            why: "importer",
            basis: "import-text-match",
            via,
        })
        .collect())
}

/// The spelling in one import's text that names `target`, or `None`. The rule
/// follows the target's language; a language without a rule never matches.
fn import_match(target: &str, importer: &str, text: &str) -> Option<String> {
    if let Some((_, group)) = pair_key(target) {
        return match group {
            PYTHON_GROUP => python_import(target, text),
            JS_GROUP => js_import(target, importer, text),
            C_GROUP => c_include(target, text),
            _ => None,
        };
    }
    if target.ends_with(".go") {
        return go_import(target, text);
    }
    if target.ends_with(".rs") {
        return rust_import(target, text);
    }
    None
}

/// Python: the dotted module of the file (`a/b/c.py` or `a/b/c/__init__.py`
/// is `a.b.c`), named by `import a.b.c` or by `from a.b import c`.
fn python_import(target: &str, text: &str) -> Option<String> {
    let stem = target
        .strip_suffix(".pyi")
        .or_else(|| target.strip_suffix(".py"))?;
    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
    let module = stem.replace('/', ".");
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ';'))
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.contains(&module.as_str()) {
        return Some(module);
    }
    let (parent_module, last) = module.rsplit_once('.')?;
    let from_form = tokens.first() == Some(&"from") && tokens.get(1) == Some(&parent_module);
    let names_last = tokens
        .iter()
        .skip_while(|t| **t != "import")
        .skip(1)
        .any(|t| *t == last);
    (from_form && names_last).then_some(module)
}

/// JS/TS: a relative specifier that resolves from the importer's folder to
/// the target, with any extension of the group or as a folder `index`.
fn js_import(target: &str, importer: &str, text: &str) -> Option<String> {
    let (stem, _) = pair_key(target)?;
    let dir = parent(target);
    let target_stem = if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    };
    quoted(text)
        .into_iter()
        .filter(|spec| spec.starts_with("./") || spec.starts_with("../"))
        .find(|spec| {
            let Some(resolved) = resolve(parent(importer), spec) else {
                return false;
            };
            let resolved_stem = strip_group_ext(&resolved, JS_GROUP);
            resolved == target
                || resolved_stem == target_stem
                || format!("{resolved_stem}/index") == target_stem
        })
        .map(str::to_string)
}

/// C/C++: `#include "x"` where `x` is the target's path or ends at one of its
/// folders (a spelling from the include path, not resolved).
fn c_include(target: &str, text: &str) -> Option<String> {
    quoted(text)
        .into_iter()
        .find(|spec| target == *spec || target.ends_with(&format!("/{spec}")))
        .map(str::to_string)
}

/// Go: a quoted import path that ends at the target's package folder.
fn go_import(target: &str, text: &str) -> Option<String> {
    let dir = parent(target);
    if dir.is_empty() {
        return None;
    }
    quoted(text)
        .into_iter()
        .find(|spec| *spec == dir || spec.ends_with(&format!("/{dir}")))
        .map(str::to_string)
}

/// Rust: `use crate::a::b` for `src/a/b.rs` (or `src/a/b/mod.rs`). The `mod b`
/// declaration in the parent file is not matched: `mod` is not an import fact.
fn rust_import(target: &str, text: &str) -> Option<String> {
    let rel = match target.rsplit_once("/src/") {
        Some((_, rest)) => rest,
        None => target.strip_prefix("src/")?,
    };
    let stem = rel.strip_suffix(".rs")?;
    let stem = stem.strip_suffix("/mod").unwrap_or(stem);
    if stem == "lib" || stem == "main" {
        return None;
    }
    let needle = format!("crate::{}", stem.replace('/', "::"));
    let matched = text.match_indices(&needle).any(|(at, _)| {
        !text[at + needle.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
    });
    matched.then_some(needle)
}

/// Quoted substrings of `text`, quotes removed.
fn quoted(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(['"', '\'']) {
        let Some(quote) = rest[start..].chars().next() else {
            break;
        };
        let after = &rest[start + quote.len_utf8()..];
        let Some(end) = after.find(quote) else {
            break;
        };
        out.push(&after[..end]);
        rest = &after[end + quote.len_utf8()..];
    }
    out
}

/// `spec` resolved against the folder `dir`; `None` when `..` leaves the root.
fn resolve(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for segment in spec.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// `path` without its extension when that extension is in `group`.
fn strip_group_ext(path: &str, group: usize) -> &str {
    let mut best = path;
    let mut best_len = 0;
    for ext in GROUPS[group] {
        if path.len() > ext.len() && path.ends_with(ext) && ext.len() > best_len {
            best_len = ext.len();
            best = &path[..path.len() - ext.len()];
        }
    }
    best
}

/// Files that call a definition of `path` that encloses one of its hits.
/// `defs` are those enclosing definitions. Only the edge query of the
/// `callers` tool is used, with its freshness verdict passed on. Returns `Err`
/// when the resolver does not map a definition's first byte back to that same
/// definition; the position is verified at runtime, not proven by construction.
///
/// The anchor is the first byte of each definition. The resolver's syntax
/// fallback picks the narrowest definition that holds that byte, which is the
/// definition itself unless a narrower row starts at the same byte. The
/// returned span is checked against the definition, so such a case is an
/// `Err`, never a wrong caller list.
pub(crate) fn callers(
    tx: &rusqlite::Transaction<'_>,
    name: &str,
    canon: &Path,
    snapshot_id: i64,
    path: &str,
    defs: &[store::SymbolSpan],
    freshness: (&str, &[String]),
) -> Result<Vec<Companion>, String> {
    if defs.is_empty() {
        return Ok(Vec::new());
    }
    // The edge query reads the latest snapshot of `tx`; it must be the one
    // the grep was answered from.
    if store::latest_snapshot(tx)?.map(|s| s.id) != Some(snapshot_id) {
        return Err("snapshot changed during grep".to_string());
    }
    let validated = semantic::validate_doc_path(canon, path)?;
    let source = semantic::confined_read(canon, &validated, scan::MAX_FILE_BYTES)?;
    // (0 = CALLS, 1 = POTENTIAL_CALL), caller path, the definition's name.
    let mut found: Vec<(u8, String, String)> = Vec::new();
    for def in defs {
        let start = usize::try_from(def.byte_start)
            .map_err(|_| format!("definition '{}' in {path} has a bad span", def.name))?;
        let end = usize::try_from(def.byte_end)
            .map_err(|_| format!("definition '{}' in {path} has a bad span", def.name))?;
        let (line1, col1) = position_of(&source, start)?;
        let request = index::EdgeRequest {
            anchor: index::SpanQuery {
                rel: &validated,
                line1,
                col1,
                refs: false,
            },
            symbol: None,
            direction: index::EdgeDir::Callers,
            limit: index::DEFAULT_SEARCH_LIMIT,
            offset: 0,
        };
        let out = index::edge_query_tx(
            tx,
            name,
            canon,
            &validated,
            &request,
            || {},
            Some(freshness),
        )?;
        if out.anchor.kind != index::AnchorKind::SyntaxFallback
            || out.anchor.span_bytes != Some((start, end))
        {
            return Err(format!(
                "companion anchor for '{}' in {path} did not resolve to its definition",
                def.name
            ));
        }
        for hit in &out.edges {
            let rank = match hit.edge.edge_kind.as_str() {
                "CALLS" => 0,
                "POTENTIAL_CALL" => 1,
                _ => continue,
            };
            let caller = &hit.edge.caller_path;
            if caller == path || index::is_test_path(caller) {
                continue;
            }
            found.push((rank, caller.clone(), def.name.clone()));
        }
    }
    // CALLS first, then by path; one companion per caller file.
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut companions: Vec<Companion> = Vec::new();
    for (rank, caller, via) in found {
        if companions.iter().any(|c| c.path == caller) {
            continue;
        }
        if companions.len() == MAX_PER_KIND {
            break;
        }
        companions.push(Companion {
            path: caller,
            why: "caller",
            basis: if rank == 0 { "CALLS" } else { "POTENTIAL_CALL" },
            via,
        });
    }
    Ok(companions)
}

/// The 1-based line and byte column of `offset`. The resolver maps them back
/// to `offset` (its column is a byte column from the line start).
fn position_of(source: &[u8], offset: usize) -> Result<(u32, u32), String> {
    let before = source
        .get(..offset)
        .ok_or_else(|| "definition offset is outside the source".to_string())?;
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |p| p + 1);
    let lines = before.iter().filter(|b| **b == b'\n').count();
    Ok(((lines + 1) as u32, (offset - line_start + 1) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{grep, store, testutil::tmpdir};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn walk(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn paths_of(companions: &[Companion]) -> Vec<&str> {
        companions.iter().map(|c| c.path.as_str()).collect()
    }

    /// A repository directory with the given files (relative paths).
    fn repo_with(files: &[(&str, &[u8])]) -> PathBuf {
        let root = tmpdir("companions-repo");
        for (rel, bytes) in files {
            let full = root.join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, bytes).unwrap();
        }
        root
    }

    /// Index `root` under a fresh home as repository `r`; returns the home.
    fn indexed(root: &Path) -> PathBuf {
        let home = tmpdir("companions-home");
        index::build(&home, "r", root, false).expect("index build");
        home
    }

    #[test]
    fn name_pairs_follow_extension_groups() {
        let all = walk(&[
            "index.d.ts",
            "index.js",
            "x.h",
            "x.c",
            "grammar.l",
            "grammar.y",
            "parser.y",
            "lexer.l",
            "compile.c",
        ]);
        let pairs = name_pairs("index.js", &all);
        assert_eq!(paths_of(&pairs), ["index.d.ts"]);
        assert_eq!(pairs[0].why, "name-pair");
        assert_eq!(pairs[0].basis, "path-convention");
        assert_eq!(pairs[0].via, "index");
        assert_eq!(paths_of(&name_pairs("x.c", &all)), ["x.h"]);
        assert_eq!(paths_of(&name_pairs("grammar.y", &all)), ["grammar.l"]);
        // Near miss: a different base name does not pair, even in a group.
        assert!(name_pairs("parser.y", &all).is_empty());
    }

    #[test]
    fn name_pairs_same_folder_first_and_no_tests() {
        let all = walk(&["lib/util.py", "src/util.pyi", "tests/util.py"]);
        let pairs = name_pairs("src/util.py", &all);
        assert_eq!(paths_of(&pairs), ["src/util.pyi", "lib/util.py"]);
    }

    #[test]
    fn importers_python_dotted_module() {
        let root = repo_with(&[
            ("pkg/core.py", b"def run():\n    return 1\n"),
            ("app.py", b"import pkg.core\n"),
            ("lone.py", b"from pkg import core\n"),
            ("near.py", b"import pkg.corex\n"),
        ]);
        let home = indexed(&root);
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let found = importers(&tx, snap.id, "pkg/core.py").unwrap();
        assert_eq!(paths_of(&found), ["app.py", "lone.py"]);
        assert_eq!(found[0].basis, "import-text-match");
        assert_eq!(found[0].why, "importer");
        assert_eq!(found[0].via, "pkg.core");
    }

    #[test]
    fn importers_js_relative_with_index() {
        let root = repo_with(&[
            ("lib/ui/index.ts", b"export const x = 1;\n"),
            ("app.ts", b"import { x } from \"./lib/ui\";\n"),
            (
                "deep/page.ts",
                b"import { x } from \"../lib/ui/index.js\";\n",
            ),
            ("bare.ts", b"import { x } from \"lib/ui\";\n"),
            ("near.ts", b"import { x } from \"./lib/uix\";\n"),
        ]);
        let home = indexed(&root);
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let found = importers(&tx, snap.id, "lib/ui/index.ts").unwrap();
        assert_eq!(paths_of(&found), ["app.ts", "deep/page.ts"]);
    }

    #[test]
    fn importers_c_include() {
        let root = repo_with(&[
            ("inc/foo.h", b"int foo(void);\n"),
            (
                "src/main.c",
                b"#include \"inc/foo.h\"\nint main(void) { return foo(); }\n",
            ),
            ("src/other.c", b"#include \"foo.h\"\n"),
            ("src/far.c", b"#include <foo.h>\n"),
        ]);
        let home = indexed(&root);
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let found = importers(&tx, snap.id, "inc/foo.h").unwrap();
        assert_eq!(paths_of(&found), ["src/main.c", "src/other.c"]);
    }

    #[test]
    fn importers_go_package_dir() {
        let root = repo_with(&[
            ("p/q/q.go", b"package q\n\nfunc Q() {}\n"),
            (
                "main.go",
                b"package main\n\nimport \"example.com/m/p/q\"\n\nfunc main() { q.Q() }\n",
            ),
            ("x.go", b"package x\n\nimport \"example.com/m/p/qq\"\n"),
        ]);
        let home = indexed(&root);
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let found = importers(&tx, snap.id, "p/q/q.go").unwrap();
        assert_eq!(paths_of(&found), ["main.go"]);
    }

    #[test]
    fn importers_rust_use_crate() {
        let root = repo_with(&[
            ("src/a/b.rs", b"pub fn thing() {}\n"),
            (
                "src/main.rs",
                b"mod a;\nuse crate::a::b::thing;\nfn main() { thing(); }\n",
            ),
            ("src/other.rs", b"use crate::a::bx;\n"),
        ]);
        let home = indexed(&root);
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let found = importers(&tx, snap.id, "src/a/b.rs").unwrap();
        assert_eq!(paths_of(&found), ["src/main.rs"]);
    }

    /// Three files: `lib.py` defines `helper`; `a.py` and `b.py` call it.
    fn caller_fixture() -> PathBuf {
        repo_with(&[
            ("lib.py", b"def helper():\n    return 1\n"),
            (
                "a.py",
                b"from lib import helper\n\ndef use_a():\n    return helper()\n",
            ),
            (
                "b.py",
                b"import lib\n\ndef use_b():\n    return lib.helper()\n",
            ),
        ])
    }

    #[test]
    fn companion_callers_match_callers_tool() {
        let root = caller_fixture();
        let home = indexed(&root);
        // The callers tool's answer for the anchor inside `helper`.
        let request = index::EdgeRequest {
            anchor: index::SpanQuery {
                rel: "lib.py",
                line1: 1,
                col1: 5,
                refs: false,
            },
            symbol: None,
            direction: index::EdgeDir::Callers,
            limit: index::DEFAULT_SEARCH_LIMIT,
            offset: 0,
        };
        let tool = index::edge_query(&home, "r", &root, &request).expect("callers tool");
        let tool_files: BTreeSet<String> = tool
            .edges
            .iter()
            .map(|hit| hit.edge.caller_path.clone())
            .collect();
        assert_eq!(
            tool_files,
            BTreeSet::from(["a.py".to_string(), "b.py".to_string()])
        );

        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let defs: Vec<store::SymbolSpan> = store::symbols_in_files(&tx, snap.id, &["lib.py"])
            .unwrap()
            .remove("lib.py")
            .unwrap()
            .into_iter()
            .filter(|s| s.name == "helper")
            .collect();
        let canon = root.canonicalize().unwrap();
        let found = callers(&tx, "r", &canon, snap.id, "lib.py", &defs, ("fresh", &[])).unwrap();
        let companion_files: BTreeSet<String> = found.iter().map(|c| c.path.clone()).collect();
        assert_eq!(companion_files, tool_files);
        assert!(found.iter().all(|c| c.why == "caller" && c.via == "helper"));
        assert!(found
            .iter()
            .all(|c| c.basis == "CALLS" || c.basis == "POTENTIAL_CALL"));
    }

    #[test]
    fn companions_on_stale_snapshot_still_resolve_unchanged_files() {
        let root = caller_fixture();
        let home = indexed(&root);
        std::fs::write(
            root.join("a.py"),
            b"from lib import helper\n\n# edited after the index\ndef use_a():\n    return helper()\n",
        )
        .unwrap();
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let req = grep::GrepRequest {
            pattern: "helper",
            regex: false,
            ignore_case: false,
            path_prefix: None,
            limit: grep::DEFAULT_LIMIT,
        };
        let result = grep::grep_repo(&root, Some(&tx), &req).expect("grep");
        assert_eq!(result.freshness, "stale");
        let lib = result.files.iter().find(|f| f.path == "lib.py").unwrap();
        assert!(lib.fresh, "lib.py did not change");
        let callers_of_lib: Vec<&str> = lib
            .companions
            .iter()
            .filter(|c| c.why == "caller")
            .map(|c| c.path.as_str())
            .collect();
        assert!(callers_of_lib.contains(&"b.py"), "{callers_of_lib:?}");
    }

    #[test]
    fn grep_without_index_gives_name_pairs_only() {
        let root = repo_with(&[
            ("index.js", b"export const a = 1;\n// needle\n"),
            ("index.d.ts", b"export declare const a: number;\n"),
        ]);
        let req = grep::GrepRequest {
            pattern: "needle",
            regex: false,
            ignore_case: false,
            path_prefix: None,
            limit: grep::DEFAULT_LIMIT,
        };
        let result = grep::grep_repo(&root, None, &req).expect("grep");
        let file = result.files.iter().find(|f| f.path == "index.js").unwrap();
        assert_eq!(paths_of(&file.companions), ["index.d.ts"]);
        assert!(file.companions.iter().all(|c| c.why == "name-pair"));
    }
}
