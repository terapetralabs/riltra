// SPDX-License-Identifier: Apache-2.0
//! Paired files for a grep hit (`companions` in the grep document). Three
//! matchers name them, each with its own evidence label:
//!
//! - `name-pair` (`path-convention`): same base name, extension of the same
//!   group (C/C++ headers, JS/TS, lex/yacc, Python stubs). From the walk only.
//! - `importer` (`import-text-match`): a file whose indexed import text names
//!   the file of the hit, or a Rust parent file that declares it with `mod`.
//!   A guess from text, never a proven import.
//! - `caller` (`CALLS` or `POTENTIAL_CALL`): a file that calls a definition
//!   enclosing a hit, found with the anchor resolution and edge selection of
//!   the `callers` tool.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::{index, scan, semantic, store};

/// One paired file: why it was named, on what basis, and through which name.
pub(crate) struct Companion {
    pub path: String,
    pub why: &'static str,
    pub basis: &'static str,
    pub via: String,
}

/// The caller companions of one file, and the reasons lookups were skipped.
pub(crate) struct CallerFinds {
    pub companions: Vec<Companion>,
    pub gaps: Vec<&'static str>,
}

/// A caller lookup skipped because the snapshot's graph proof is not current.
pub(crate) const GAP_GRAPH_PROOF: &str = "graph-proof-incompatible";
/// A definition the resolver does not map back to itself, so it is skipped.
pub(crate) const GAP_NOT_RESOLVED: &str = "definition-not-resolved";

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

/// Files whose import text, or Rust parent `mod` declaration, names `path`,
/// one entry per file. Uses the snapshot's inputs, loaded once per request. A
/// Rust parent that is over the size limit or cannot be read is skipped. `Err`
/// only when an import row names a file the snapshot does not hold.
pub(crate) fn importers(
    inputs: &index::CallerInputs,
    canon: &Path,
    walk: &[String],
    path: &str,
) -> Result<Vec<Companion>, String> {
    let names: HashMap<i64, &str> = inputs
        .id_map
        .iter()
        .map(|(rel, (id, _))| (*id, rel.as_str()))
        .collect();
    // Importer path → the spelling that names `path`.
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for row in &inputs.imports {
        let importer = *names
            .get(&row.file_id)
            .ok_or_else(|| format!("import row of unknown file id {}", row.file_id))?;
        if importer == path || index::is_test_path(importer) || found.contains_key(importer) {
            continue;
        }
        // The text is `None` only for bytes that are not UTF-8: such an import
        // cannot be matched as text, and is not guessed at either.
        let Some(text) = row.text.as_deref() else {
            continue;
        };
        if let Some(via) = import_match(path, importer, text) {
            found.insert(importer.to_string(), via);
        }
    }
    for (importer, via) in mod_declarations(canon, walk, path) {
        found.entry(importer).or_insert(via);
    }
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
        return rust_import(target, importer, text);
    }
    None
}

/// Python: the dotted module of the file (`a/b/c.py` or `a/b/c/__init__.py` is
/// `a.b.c`). Named by the module position of `import a.b.c [as x]`, or of
/// `from a.b import c`, where `c` is a name that submodule `a.b.c` can match.
/// Names after `import` are never aliases: the `as` part is skipped.
fn python_import(target: &str, text: &str) -> Option<String> {
    let stem = target
        .strip_suffix(".pyi")
        .or_else(|| target.strip_suffix(".py"))?;
    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
    let module = stem.replace('/', ".");
    let statement = text.replace(['(', ')'], " ");
    let first = statement.split_whitespace().next()?;
    let names = keyword_rest(&statement, "import")?;
    let matched = match first {
        "import" => item_heads(names).contains(&module.as_str()),
        "from" => {
            let from_module = statement.split_whitespace().nth(1)?;
            let names_last = module
                .rsplit_once('.')
                .is_some_and(|(parent_module, last)| {
                    from_module == parent_module && item_heads(names).contains(&last)
                });
            from_module == module.as_str() || names_last
        }
        _ => false,
    };
    matched.then_some(module)
}

/// The first word of each comma-separated item: a module of `import` or a name
/// of `from ... import`. An `as` alias is a later word, so it is never read.
fn item_heads(list: &str) -> Vec<&str> {
    list.split(',')
        .filter_map(|item| item.split_whitespace().next())
        .collect()
}

/// The text after the first whole-word occurrence of `keyword`.
fn keyword_rest<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    text.match_indices(keyword)
        .find(|(at, _)| {
            let before = text[..*at].chars().next_back();
            let after = text[*at + keyword.len()..].chars().next();
            !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
        })
        .map(|(at, _)| &text[at + keyword.len()..])
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

/// Go: a quoted import path that ends at the target's package folder. A bare
/// path such as the standard library's `errors` is not a local package.
fn go_import(target: &str, text: &str) -> Option<String> {
    let dir = parent(target);
    if dir.is_empty() {
        return None;
    }
    quoted(text)
        .into_iter()
        .find(|spec| spec.ends_with(&format!("/{dir}")))
        .map(str::to_string)
}

/// Rust: `use crate::a::b` names `src/a/b.rs` (or `src/a/b/mod.rs`) when the
/// importer is in the same crate. Grouped and glob uses are read by
/// `use_tree_names`. Parent `mod` declarations are a separate check
/// (`mod_declarations`).
fn rust_import(target: &str, importer: &str, text: &str) -> Option<String> {
    let root = index::rust_crate_src_root(target)?;
    if index::rust_crate_src_root(importer) != Some(root) {
        return None;
    }
    let module = rust_module(target)?.join("::");
    // One budget for every `crate::` occurrence of this importer's text.
    let mut scanned = 0usize;
    for (at, _) in text.match_indices("crate::") {
        if text[..at].chars().next_back().is_some_and(is_word_char) {
            continue;
        }
        if use_tree_names(&text[at + "crate::".len()..], &module, &mut scanned) {
            return Some(format!("crate::{module}"));
        }
    }
    None
}

/// The module segments of a Rust file under its crate's source root. `None`
/// for the crate roots `lib.rs` and `main.rs`, which are not modules.
fn rust_module(path: &str) -> Option<Vec<&str>> {
    let root = index::rust_crate_src_root(path)?;
    let rel = path.strip_prefix(root)?.strip_prefix('/')?;
    let stem = rel.strip_suffix(".rs")?;
    let mut segments: Vec<&str> = stem.split('/').collect();
    if segments.last() == Some(&"mod") {
        segments.pop();
    }
    let crate_root = matches!(segments.as_slice(), ["lib"] | ["main"]);
    if segments.is_empty() || crate_root {
        None
    } else {
        Some(segments)
    }
}

/// Bytes of `use` tree text one importer check may scan. A tree past this bound
/// is not read further, so its importer is not claimed. This bounds the work on
/// untrusted sources, since a nested group is rescanned at each level.
// shortcut: a use tree of this size is never a real import; upgrade if one is.
const USE_TREE_SCAN_BYTES: usize = 1 << 20;

/// Whether a `use` tree (the text after `crate::`) names `module` or an item of
/// it. The walk is iterative, so no depth of the tree can exhaust the call stack.
/// A branch whose path can no longer lead to `module` is dropped at once, so a
/// path never grows past the module's own length. `scanned` is the group text
/// read so far for the whole importer, shared by its `crate::` occurrences.
/// Only a group is charged: its closing scan and its items are read once per
/// level, so the charge bounds all the work a nested tree can cause.
fn use_tree_names(tree: &str, module: &str, scanned: &mut usize) -> bool {
    let mut pending: Vec<(String, &str)> = vec![(String::new(), tree)];
    while let Some((prefix, rest)) = pending.pop() {
        let mut prefix = prefix;
        let mut rest = rest.trim_start();
        loop {
            if let Some(inner) = rest.strip_prefix('{') {
                // Checked before the scan, so an exhausted budget costs nothing.
                if *scanned > USE_TREE_SCAN_BYTES {
                    return false;
                }
                let end = group_end(inner);
                *scanned += end;
                if *scanned > USE_TREE_SCAN_BYTES {
                    return false;
                }
                let inner = &inner[..end];
                for item in split_top_level(inner).into_iter().rev() {
                    pending.push((prefix.clone(), item));
                }
                break;
            }
            if rest.starts_with('*') {
                if names_module(&prefix, module) {
                    return true;
                }
                break;
            }
            let end = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
            let segment = &rest[..end];
            if segment.is_empty() {
                break;
            }
            if let Some(next) = rest[end..].trim_start().strip_prefix("::") {
                let joined = join_path(&prefix, segment);
                if names_module(&joined, module) {
                    return true;
                }
                if !is_ancestor(&joined, module) {
                    break;
                }
                prefix = joined;
                rest = next.trim_start();
                continue;
            }
            let full = if segment == "self" {
                prefix.clone()
            } else {
                join_path(&prefix, segment)
            };
            if names_module(&full, module) {
                return true;
            }
            break;
        }
    }
    false
}

/// Whether `path` is `module` or an item or submodule of it.
fn names_module(path: &str, module: &str) -> bool {
    path == module
        || path
            .strip_prefix(module)
            .is_some_and(|rest| rest.starts_with("::"))
}

/// Whether `module` is `path` or lies below it: `path` can still lead to it.
fn is_ancestor(path: &str, module: &str) -> bool {
    path.is_empty()
        || module == path
        || module
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with("::"))
}

fn join_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}::{segment}")
    }
}

/// Byte index of the `}` that closes a group whose `{` is already consumed.
fn group_end(text: &str) -> usize {
    let mut depth = 0usize;
    for (at, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return at,
            '}' => depth -= 1,
            _ => {}
        }
    }
    text.len()
}

/// Items of a use group, split at commas that are not inside a nested group.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (at, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Rust: the parent module files of `path` that declare it with `mod`. The
/// parent of `a/b` is `a.rs` or `a/mod.rs`; the parent of `a` is `lib.rs` or
/// `main.rs`. Only files of the walk are read; a comment line is not a
/// declaration.
fn mod_declarations(canon: &Path, walk: &[String], path: &str) -> Vec<(String, String)> {
    let (Some(root), Some(segments)) = (index::rust_crate_src_root(path), rust_module(path)) else {
        return Vec::new();
    };
    let Some((last, parents)) = segments.split_last() else {
        return Vec::new();
    };
    let parent_files = if parents.is_empty() {
        vec![format!("{root}/lib.rs"), format!("{root}/main.rs")]
    } else {
        let dir = parents.join("/");
        vec![format!("{root}/{dir}.rs"), format!("{root}/{dir}/mod.rs")]
    };
    let mut out = Vec::new();
    for parent_file in parent_files {
        if index::is_test_path(&parent_file) || !walk.contains(&parent_file) {
            continue;
        }
        // A parent over the size limit, or one that cannot be read, is skipped as
        // grep skips such files: it is not evidence, and the search goes on.
        let oversize = std::fs::metadata(canon.join(&parent_file))
            .is_ok_and(|meta| meta.len() > scan::MAX_FILE_BYTES);
        if oversize {
            continue;
        }
        let Ok(bytes) = semantic::confined_read(canon, &parent_file, scan::MAX_FILE_BYTES) else {
            continue;
        };
        if declares_mod(&String::from_utf8_lossy(&bytes), last) {
            out.push((parent_file, format!("mod {last}")));
        }
    }
    out
}

/// `text` with its comments and string literals blanked, so a word inside them
/// is never read as code. Raw strings (`r#"..."#`) and char literals are not
/// recognised: a `"` inside one of them can hide a later declaration.
// shortcut: a `"` in a raw string or char literal hides code up to the next
// quote; upgrade with a lexer when a real file shows it.
fn strip_comments_and_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                while chars.peek().is_some_and(|&n| n != '\n') {
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            '"' => {
                let mut escaped = false;
                for n in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if n == '\\' {
                        escaped = true;
                    } else if n == '"' {
                        break;
                    }
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Whether a `mod <name>;` declaration stands in `text` outside comments and
/// string literals. An inline `mod <name> { ... }` is not a file and is not one.
fn declares_mod(text: &str, name: &str) -> bool {
    let code = strip_comments_and_strings(text).replace(';', " ; ");
    let words: Vec<&str> = code
        .split(|c: char| !is_word_char(c) && c != ';')
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(3)
        .any(|w| w[0] == "mod" && w[1] == name && w[2] == ";")
}

/// Caller companions of the definitions that enclose one file's hits, and the
/// reasons lookups were skipped. Each definition is anchored at its name: its
/// SCIP definition occurrence when the import has one (that position must
/// resolve to the same symbol), else a whole-word occurrence of the name that
/// resolves to the definition itself. A definition with no such position is
/// skipped and named in `gaps`. A storage failure is an `Err`. The caller set is
/// the one the `callers` query gives for the same anchor: the distinct caller
/// files of its edges, read in one query per definition.
pub(crate) fn callers(
    inputs: &index::CallerInputs,
    tx: &rusqlite::Connection,
    canon: &Path,
    snapshot_id: i64,
    path: &str,
    defs: &[store::SymbolSpan],
) -> Result<CallerFinds, String> {
    let mut finds = CallerFinds {
        companions: Vec::new(),
        gaps: Vec::new(),
    };
    if defs.is_empty() {
        return Ok(finds);
    }
    // The edge query reads the latest snapshot of `tx`; it must be the one the
    // grep was answered from.
    if store::latest_snapshot(tx)?.map(|s| s.id) != Some(snapshot_id) {
        return Err("snapshot changed during grep".to_string());
    }
    // The same proof gate as the `callers` query, checked once.
    if !store::graph_proof_current(tx, snapshot_id)? {
        finds.gaps.push(GAP_GRAPH_PROOF);
        return Ok(finds);
    }
    let validated = semantic::validate_doc_path(canon, path)?;
    let source = semantic::confined_read(canon, &validated, scan::MAX_FILE_BYTES)?;
    let facts = store::semantic_facts_at(tx, snapshot_id, &validated)?;
    // Caller path → (0 for CALLS or 1 for POTENTIAL_CALL, the definition's name).
    let mut found: BTreeMap<String, (u8, String)> = BTreeMap::new();
    for def in defs {
        let (Ok(start), Ok(end)) = (
            usize::try_from(def.byte_start),
            usize::try_from(def.byte_end),
        ) else {
            finds.gaps.push(GAP_NOT_RESOLVED);
            continue;
        };
        // The SCIP definition occurrence covers the terminal name only (`run` of
        // `Widget::run`), so the candidates are searched for that name.
        let candidates =
            name_candidates(&source, index::called_name(&def.name), start, end, &facts);
        let anchor = resolve_name(
            tx,
            snapshot_id,
            canon,
            &validated,
            &source,
            &candidates,
            (start, end),
        )?;
        let Some(anchor) = anchor else {
            finds.gaps.push(GAP_NOT_RESOLVED);
            continue;
        };
        let pairs = match index::companion_caller_pairs(tx, snapshot_id, inputs, &anchor) {
            Ok(pairs) => pairs,
            Err(index::GraphQueryError::Anchor(_)) => {
                finds.gaps.push(GAP_NOT_RESOLVED);
                continue;
            }
            Err(index::GraphQueryError::Store(message)) => return Err(message),
        };
        for (caller, kind) in pairs {
            let rank: u8 = match kind.as_str() {
                "CALLS" => 0,
                "POTENTIAL_CALL" => 1,
                _ => continue,
            };
            if caller == path || index::is_test_path(&caller) {
                continue;
            }
            let entry = found.entry(caller).or_insert((rank, def.name.clone()));
            if rank < entry.0 {
                *entry = (rank, def.name.clone());
            }
        }
    }
    let mut ranked: Vec<(u8, String, String)> = found
        .into_iter()
        .map(|(caller, (rank, via))| (rank, caller, via))
        .collect();
    ranked.sort();
    finds.companions = ranked
        .into_iter()
        .take(MAX_PER_KIND)
        .map(|(rank, caller, via)| Companion {
            path: caller,
            why: "caller",
            basis: if rank == 0 { "CALLS" } else { "POTENTIAL_CALL" },
            via,
        })
        .collect();
    Ok(finds)
}

/// The `definition` role bit of a SCIP occurrence.
const DEFINITION_ROLE: u32 = 0x1;
/// Positions tried for one definition's name, in order. Enough for the
/// definition occurrence and the first few words of a name.
const MAX_NAME_CANDIDATES: usize = 16;

/// A position to anchor a definition's name at. `symbol` is the SCIP identity
/// the position must resolve to, or `None` for a syntax position, which must
/// resolve to the definition itself.
struct NameCandidate {
    at: usize,
    symbol: Option<String>,
}

/// Where a definition's name can be anchored, in the order they are tried: its
/// SCIP definition occurrences inside its byte range first, then every
/// whole-word occurrence of the name inside it. A whole-word occurrence is not
/// the name just because it comes first (a receiver type or a return type can
/// precede it), so the caller checks what each position resolves to.
fn name_candidates(
    source: &[u8],
    name: &str,
    start: usize,
    end: usize,
    facts: &[store::SemanticFact],
) -> Vec<NameCandidate> {
    let needle = name.as_bytes();
    let mut out: Vec<NameCandidate> = facts
        .iter()
        .filter(|fact| {
            fact.roles & DEFINITION_ROLE != 0
                && start <= fact.byte_start
                && fact.byte_end <= end
                && source.get(fact.byte_start..fact.byte_end) == Some(needle)
        })
        .filter_map(|fact| {
            fact.symbol_uri.as_ref().map(|symbol| NameCandidate {
                at: fact.byte_start,
                symbol: Some(symbol.clone()),
            })
        })
        .collect();
    out.extend(
        whole_word_offsets(source, needle, start, end)
            .into_iter()
            .map(|at| NameCandidate { at, symbol: None }),
    );
    out.truncate(MAX_NAME_CANDIDATES);
    out
}

/// Byte offsets of every whole-word `needle` inside `source[start..end]`. A word
/// character is a letter, digit, `_` or `$`, as in identifiers of the supported
/// languages.
fn whole_word_offsets(source: &[u8], needle: &[u8], start: usize, end: usize) -> Vec<usize> {
    let Some(body) = source.get(start..end) else {
        return Vec::new();
    };
    if needle.is_empty() || needle.len() > body.len() {
        return Vec::new();
    }
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    (0..=body.len() - needle.len())
        .filter(|&at| {
            body[at..].starts_with(needle)
                && (at == 0 || !word(body[at - 1]))
                && !body.get(at + needle.len()).is_some_and(|&b| word(b))
        })
        .map(|at| start + at)
        .collect()
}

/// The first candidate position whose resolution is the candidate's own: a SCIP
/// identity equal to the candidate's symbol, or, for a syntax position, the
/// definition's own byte range. `Ok(None)` when no candidate qualifies; a
/// storage failure is an `Err`.
fn resolve_name(
    tx: &rusqlite::Connection,
    snapshot_id: i64,
    canon: &Path,
    validated: &str,
    source: &[u8],
    candidates: &[NameCandidate],
    span: (usize, usize),
) -> Result<Option<index::EdgeAnchor>, String> {
    for candidate in candidates {
        let Some((line1, col1)) = position_of(source, candidate.at) else {
            continue;
        };
        let anchor =
            match index::companion_anchor(tx, snapshot_id, canon, validated, line1, col1, source) {
                Ok(anchor) => anchor,
                Err(index::GraphQueryError::Anchor(_)) => continue,
                Err(index::GraphQueryError::Store(message)) => return Err(message),
            };
        let accepted = match &candidate.symbol {
            Some(symbol) => {
                anchor.kind == index::AnchorKind::Semantic
                    && anchor.symbol_uri.as_deref() == Some(symbol.as_str())
            }
            None => {
                anchor.kind == index::AnchorKind::SyntaxFallback && anchor.span_bytes == Some(span)
            }
        };
        if accepted {
            return Ok(Some(anchor));
        }
    }
    Ok(None)
}

/// The 1-based line and byte column of `offset`. The resolver maps them back to
/// `offset`: its column is a byte column from the line start.
fn position_of(source: &[u8], offset: usize) -> Option<(u32, u32)> {
    let before = source.get(..offset)?;
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |at| at + 1);
    let lines = before.iter().filter(|b| **b == b'\n').count();
    Some(((lines + 1) as u32, (offset - line_start + 1) as u32))
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

    /// The importers of `target` in an index of `files`.
    fn importers_of(files: &[(&str, &[u8])], target: &str) -> Vec<Companion> {
        let root = repo_with(files);
        let home = indexed(&root);
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let inputs = index::CallerInputs::load(&tx, snap.id).unwrap();
        let canon = root.canonicalize().unwrap();
        let all: Vec<String> = files.iter().map(|(rel, _)| rel.to_string()).collect();
        importers(&inputs, &canon, &all, target).unwrap()
    }

    #[test]
    fn importers_python_dotted_module() {
        let files: &[(&str, &[u8])] = &[
            ("pkg/core.py", b"def run():\n    return 1\n"),
            ("app.py", b"import pkg.core\n"),
            ("lone.py", b"from pkg import core\n"),
            ("near.py", b"import pkg.corex\n"),
        ];
        let found = importers_of(files, "pkg/core.py");
        assert_eq!(paths_of(&found), ["app.py", "lone.py"]);
        assert_eq!(found[0].basis, "import-text-match");
        assert_eq!(found[0].why, "importer");
        assert_eq!(found[0].via, "pkg.core");
    }

    #[test]
    fn importers_python_claims_only_the_module_position() {
        let files: &[(&str, &[u8])] = &[
            ("signals.py", b"def x():\n    pass\n"),
            ("np.py", b"x = 1\n"),
            ("numpy.py", b"y = 2\n"),
            (
                "app.py",
                b"from django.core import signals\n\ndef go():\n    return signals.x()\n",
            ),
            ("app2.py", b"import numpy as np\n"),
        ];
        // `from django.core import signals` names a submodule name, not the
        // module `signals`; `as np` is an alias, never a module of this file.
        assert!(importers_of(files, "signals.py").is_empty());
        assert!(importers_of(files, "np.py").is_empty());
        assert_eq!(paths_of(&importers_of(files, "numpy.py")), ["app2.py"]);
    }

    #[test]
    fn importers_js_relative_with_index() {
        let files: &[(&str, &[u8])] = &[
            ("lib/ui/index.ts", b"export const x = 1;\n"),
            ("app.ts", b"import { x } from \"./lib/ui\";\n"),
            (
                "deep/page.ts",
                b"import { x } from \"../lib/ui/index.js\";\n",
            ),
            ("bare.ts", b"import { x } from \"lib/ui\";\n"),
            ("near.ts", b"import { x } from \"./lib/uix\";\n"),
        ];
        let found = importers_of(files, "lib/ui/index.ts");
        assert_eq!(paths_of(&found), ["app.ts", "deep/page.ts"]);
    }

    #[test]
    fn importers_c_include() {
        let files: &[(&str, &[u8])] = &[
            ("inc/foo.h", b"int foo(void);\n"),
            (
                "src/main.c",
                b"#include \"inc/foo.h\"\nint main(void) { return foo(); }\n",
            ),
            ("src/other.c", b"#include \"foo.h\"\n"),
            ("src/far.c", b"#include <foo.h>\n"),
        ];
        let found = importers_of(files, "inc/foo.h");
        assert_eq!(paths_of(&found), ["src/main.c", "src/other.c"]);
    }

    #[test]
    fn importers_go_package_dir() {
        let files: &[(&str, &[u8])] = &[
            ("p/q/q.go", b"package q\n\nfunc Q() {}\n"),
            (
                "main.go",
                b"package main\n\nimport \"example.com/m/p/q\"\n\nfunc main() { q.Q() }\n",
            ),
            ("x.go", b"package x\n\nimport \"example.com/m/p/qq\"\n"),
        ];
        assert_eq!(paths_of(&importers_of(files, "p/q/q.go")), ["main.go"]);
    }

    #[test]
    fn importers_go_has_no_bare_name_match() {
        let files: &[(&str, &[u8])] = &[
            ("errors/errors.go", b"package errors\n\nfunc New() {}\n"),
            (
                "main.go",
                b"package main\n\nimport \"errors\"\n\nfunc main() {}\n",
            ),
        ];
        // The stdlib path `errors` is not the local package `errors/`.
        assert!(importers_of(files, "errors/errors.go").is_empty());
    }

    #[test]
    fn importers_rust_use_crate() {
        let files: &[(&str, &[u8])] = &[
            ("src/a/b.rs", b"pub fn thing() {}\n"),
            (
                "src/main.rs",
                b"mod a;\nuse crate::a::b::thing;\nfn main() { thing(); }\n",
            ),
            ("src/other.rs", b"use crate::a::bx;\n"),
        ];
        assert_eq!(
            paths_of(&importers_of(files, "src/a/b.rs")),
            ["src/main.rs"]
        );
    }

    #[test]
    fn importers_rust_requires_the_same_crate() {
        let files: &[(&str, &[u8])] = &[
            ("crates/a/src/index.rs", b"pub fn x() {}\n"),
            (
                "crates/b/src/main.rs",
                b"use crate::index::x;\nfn main() {}\n",
            ),
            (
                "crates/a/src/main.rs",
                b"use crate::index::x;\nfn main() {}\n",
            ),
        ];
        assert_eq!(
            paths_of(&importers_of(files, "crates/a/src/index.rs")),
            ["crates/a/src/main.rs"]
        );
    }

    #[test]
    fn importers_rust_grouped_use_names_the_module() {
        let files: &[(&str, &[u8])] = &[
            ("src/index.rs", b"pub fn x() {}\n"),
            ("src/other.rs", b"pub fn y() {}\n"),
            ("src/main.rs", b"use crate::{index, other};\nfn main() {}\n"),
        ];
        assert_eq!(
            paths_of(&importers_of(files, "src/index.rs")),
            ["src/main.rs"]
        );
    }

    #[test]
    fn importers_rust_mod_declaration_in_the_parent_file() {
        let files: &[(&str, &[u8])] = &[
            ("src/a.rs", b"pub mod b;\n"),
            ("src/a/b.rs", b"pub fn thing() {}\n"),
        ];
        let found = importers_of(files, "src/a/b.rs");
        assert_eq!(paths_of(&found), ["src/a.rs"]);
        assert_eq!(found[0].via, "mod b");
        assert_eq!(found[0].basis, "import-text-match");
    }

    #[test]
    fn importers_rust_mod_declaration_in_lib_rs() {
        let files: &[(&str, &[u8])] = &[
            ("src/lib.rs", b"pub mod a;\n"),
            ("src/a.rs", b"pub fn thing() {}\n"),
        ];
        assert_eq!(paths_of(&importers_of(files, "src/a.rs")), ["src/lib.rs"]);
    }

    #[test]
    fn whole_word_offsets_skip_longer_words() {
        let source = b"MyType helper_t(void)\nMyType helper(void) {}\n";
        let text = std::str::from_utf8(source).unwrap();
        let expected = text.find("helper(void) {").unwrap();
        assert_eq!(
            whole_word_offsets(source, b"helper", 0, source.len()),
            [expected]
        );
        assert!(whole_word_offsets(source, b"help", 0, source.len()).is_empty());
        assert!(whole_word_offsets(source, b"helper", 0, 10).is_empty());
    }

    /// Caller companions of `lib.py` (the `helper` definition) in an index of
    /// three files: `lib.py`, and `a.py` and `b.py` calling it.
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
        // The callers tool's answer for the anchor at the name `helper`.
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
        let inputs = index::CallerInputs::load(&tx, snap.id).unwrap();
        let finds = callers(&inputs, &tx, &canon, snap.id, "lib.py", &defs).unwrap();
        assert!(finds.gaps.is_empty(), "{:?}", finds.gaps);
        let companion_files: BTreeSet<String> =
            finds.companions.iter().map(|c| c.path.clone()).collect();
        assert_eq!(companion_files, tool_files);
        assert!(finds
            .companions
            .iter()
            .all(|c| c.why == "caller" && c.via == "helper"));
        assert!(finds
            .companions
            .iter()
            .all(|c| c.basis == "CALLS" || c.basis == "POTENTIAL_CALL"));
    }

    #[test]
    fn definition_that_cannot_be_named_is_a_gap_not_an_error() {
        let root = caller_fixture();
        let home = indexed(&root);
        let mut conn = store::open_live(&store::db_path(&home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let snap = store::latest_snapshot(&tx).unwrap().unwrap();
        let defs = vec![store::SymbolSpan {
            name: "missing".to_string(),
            kind: "function".to_string(),
            start_line: 1,
            end_line: 2,
            byte_start: 0,
            byte_end: 26,
        }];
        let canon = root.canonicalize().unwrap();
        let inputs = index::CallerInputs::load(&tx, snap.id).unwrap();
        let finds = callers(&inputs, &tx, &canon, snap.id, "lib.py", &defs).unwrap();
        assert!(finds.companions.is_empty());
        assert_eq!(finds.gaps, [GAP_NOT_RESOLVED]);
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
    fn declares_mod_counts_only_a_declaration_outside_comments_and_strings() {
        assert!(declares_mod("pub mod a;\n", "a"));
        assert!(declares_mod("#[cfg(test)]\nmod a ;\n", "a"));
        assert!(!declares_mod("// mod a;\n", "a"));
        assert!(!declares_mod("/* mod a; */\n", "a"));
        assert!(!declares_mod("let s = \"mod a;\";\n", "a"));
        assert!(!declares_mod("mod a { fn f() {} }\n", "a"));
        assert!(!declares_mod("mod ab;\n", "a"));
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

    #[test]
    fn rust_nested_use_scan_budget_is_shared_across_occurrences() {
        // Unbalanced nested groups: every `crate::` occurrence would rescan the
        // rest of the text if each one had its own budget.
        let text = "use crate::{crate::{".repeat(4800);
        let started = std::time::Instant::now();
        assert_eq!(rust_import("src/index.rs", "src/main.rs", &text), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }
}
