// SPDX-License-Identifier: Apache-2.0
//! `riltra grep`: literal or regex search over the repository's readable text
//! files, each hit mapped to its innermost indexed definition.
//!
//! Files come from the walk policy shared with `scan` and `index`. Content is
//! read from disk when the search runs, not from the snapshot. A hit gets an
//! enclosing definition only when the file's current digest equals its indexed
//! digest; `in_reason` says why it has none otherwise.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;

use crate::companions::{self, Companion};
use crate::{index, scan, store};

pub(crate) const DEFAULT_LIMIT: usize = 20;
pub(crate) const MAX_LIMIT: usize = 200;
/// Companions are named for the first files of the answer only.
const COMPANION_FILES: usize = 5;
/// Matching lines kept per file; the true count is still reported.
const MAX_HITS_PER_FILE: usize = 5;
/// Hits kept across one response.
const MAX_TOTAL_HITS: usize = 200;
const MAX_LINE_BYTES: usize = 200;
/// Bytes read across all files of one search.
const MAX_BYTES_READ: u64 = 256 * 1024 * 1024;
/// Size limit of the compiled pattern (`RegexBuilder::size_limit`).
const REGEX_SIZE_LIMIT: usize = 1024 * 1024;
/// A NUL byte in this prefix marks a file as binary.
const BINARY_PROBE_BYTES: usize = 8192;
/// Byte budget of one grep document; files are dropped to fit it.
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const INVALID_INPUT: &str = "invalid_input: ";

pub(crate) struct GrepRequest<'a> {
    pub pattern: &'a str,
    pub regex: bool,
    pub ignore_case: bool,
    pub path_prefix: Option<&'a str>,
    pub limit: usize,
}

pub(crate) struct GrepHit {
    pub line: u32,
    /// 1-based byte column of the first match on the line.
    pub col: u32,
    pub text: String,
    pub enclosing: Option<store::SymbolSpan>,
    pub in_reason: &'static str,
}

pub(crate) struct GrepFile {
    pub path: String,
    pub hit_count: usize,
    pub hits: Vec<GrepHit>,
    /// The file's digest matches its indexed digest. Only fresh files get
    /// caller companions; the document does not show this flag.
    pub fresh: bool,
    pub companions: Vec<Companion>,
}

/// Counts for the whole search, not only for the files in the response.
#[derive(Default)]
pub(crate) struct GrepTotals {
    /// Candidate files visited, including the skipped ones below.
    pub files_scanned: usize,
    pub files_skipped_binary: usize,
    pub files_skipped_large: usize,
    pub files_skipped_unreadable: usize,
    pub dirs_unreadable: usize,
    pub dirs_escaped: usize,
    pub files_matched: usize,
    pub hits: usize,
    pub bytes_read: u64,
}

pub(crate) struct GrepResult {
    pub files: Vec<GrepFile>,
    pub totals: GrepTotals,
    pub truncated: Option<&'static str>,
    pub snapshot: Option<store::Snapshot>,
    pub freshness: String,
    pub reasons: Vec<String>,
}

struct RawHit {
    line: u32,
    col: u32,
    text: String,
}

struct Matched {
    path: String,
    hit_count: usize,
    hits: Vec<RawHit>,
    in_reason: &'static str,
    fresh: bool,
}

/// Search `root`. `tx` is a read transaction on the repository's index, or
/// `None` when there is no index. An error that starts with `invalid_input: `
/// refuses the request; any other error is a walk or storage failure.
pub(crate) fn grep_repo(
    root: &Path,
    tx: Option<&rusqlite::Transaction<'_>>,
    req: &GrepRequest<'_>,
) -> Result<GrepResult, String> {
    grep_repo_with_budget(root, tx, req, MAX_BYTES_READ)
}

/// `grep_repo` with the read budget passed in, so a test can reach the limit
/// without reading 256 MiB.
fn grep_repo_with_budget(
    root: &Path,
    tx: Option<&rusqlite::Transaction<'_>>,
    req: &GrepRequest<'_>,
    max_bytes_read: u64,
) -> Result<GrepResult, String> {
    let pattern = compile(req)?;
    let prefix = normalize_prefix(req.path_prefix)?;
    let (plan, skips) = scan::plan_walk(root)?;
    let (snapshot, digests, freshness, reasons) = snapshot_state(root, tx)?;
    // The walk's candidate files, reused for name pairs (no second walk).
    let walk: Vec<String> = plan
        .iter()
        .filter(|p| {
            matches!(
                p.action,
                scan::PlanAction::Parse(_) | scan::PlanAction::Unsupported(_)
            )
        })
        .map(|p| p.rel.clone())
        .collect();
    let mut totals = GrepTotals {
        dirs_unreadable: skips.unreadable,
        dirs_escaped: skips.escape,
        ..GrepTotals::default()
    };
    let mut truncated: Option<&'static str> = None;
    let mut matched: Vec<Matched> = Vec::new();
    for planned in &plan {
        if !matches!(
            planned.action,
            scan::PlanAction::Parse(_) | scan::PlanAction::Unsupported(_)
        ) || !under_prefix(&planned.rel, prefix.as_deref())
        {
            continue;
        }
        let len = match std::fs::metadata(&planned.full) {
            Ok(meta) => meta.len(),
            Err(_) => {
                totals.files_scanned += 1;
                totals.files_skipped_unreadable += 1;
                continue;
            }
        };
        if len > scan::MAX_FILE_BYTES {
            totals.files_scanned += 1;
            totals.files_skipped_large += 1;
            continue;
        }
        if totals.bytes_read + len > max_bytes_read {
            truncated = Some("bytes-read");
            break;
        }
        let bytes = match std::fs::read(&planned.full) {
            Ok(bytes) => bytes,
            Err(_) => {
                totals.files_scanned += 1;
                totals.files_skipped_unreadable += 1;
                continue;
            }
        };
        totals.files_scanned += 1;
        totals.bytes_read += bytes.len() as u64;
        if bytes.len() as u64 > scan::MAX_FILE_BYTES {
            totals.files_skipped_large += 1;
            continue;
        }
        let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
        if probe.contains(&0) {
            totals.files_skipped_binary += 1;
            continue;
        }
        let (in_reason, fresh) = index_state(digests.as_ref(), &planned.rel, &bytes);
        let (hit_count, hits) = search_lines(&pattern, &bytes);
        if hit_count > 0 {
            matched.push(Matched {
                path: planned.rel.clone(),
                hit_count,
                hits,
                in_reason,
                fresh,
            });
        }
    }
    totals.files_matched = matched.len();
    totals.hits = matched.iter().map(|m| m.hit_count).sum();
    matched.sort_by_key(|m| {
        (
            index::is_test_path(&m.path),
            Reverse(m.hit_count),
            m.path.clone(),
        )
    });
    if matched.len() > req.limit {
        matched.truncate(req.limit);
        truncated = truncated.or(Some("limit"));
    }
    let mut shown = 0;
    for file in &mut matched {
        let room = MAX_TOTAL_HITS - shown;
        if file.hits.len() > room {
            file.hits.truncate(room);
            truncated = truncated.or(Some("total-hits"));
        }
        shown += file.hits.len();
    }
    // One symbols query for the files that are still shown with hits.
    let spans_by_path = match (tx, snapshot.as_ref()) {
        (Some(tx), Some(snap)) => {
            let paths: Vec<&str> = matched
                .iter()
                .filter(|m| m.fresh && !m.hits.is_empty())
                .map(|m| m.path.as_str())
                .collect();
            store::symbols_in_files(tx, snap.id, &paths)?
        }
        _ => HashMap::new(),
    };
    let mut files = Vec::with_capacity(matched.len());
    for m in matched {
        let spans: &[store::SymbolSpan] = spans_by_path.get(&m.path).map_or(&[], Vec::as_slice);
        let hits = m
            .hits
            .into_iter()
            .map(|h| GrepHit {
                line: h.line,
                col: h.col,
                enclosing: innermost(spans, h.line),
                text: h.text,
                in_reason: m.in_reason,
            })
            .collect();
        files.push(GrepFile {
            path: m.path,
            hit_count: m.hit_count,
            hits,
            fresh: m.fresh,
            companions: Vec::new(),
        });
    }
    attach_companions(
        root,
        tx,
        snapshot.as_ref(),
        (freshness.as_str(), reasons.as_slice()),
        &walk,
        &mut files,
    )?;
    Ok(GrepResult {
        files,
        totals,
        truncated,
        snapshot,
        freshness,
        reasons,
    })
}

/// Companions for the first `COMPANION_FILES` files. Name pairs need no
/// snapshot; importers and callers need one, and callers need a fresh file.
/// Storage errors propagate: a failed query is never an empty list.
fn attach_companions(
    root: &Path,
    tx: Option<&rusqlite::Transaction<'_>>,
    snapshot: Option<&store::Snapshot>,
    freshness: (&str, &[String]),
    walk: &[String],
    files: &mut [GrepFile],
) -> Result<(), String> {
    for file in files.iter_mut().take(COMPANION_FILES) {
        let mut found = companions::name_pairs(&file.path, walk);
        if let (Some(tx), Some(snap)) = (tx, snapshot) {
            found.extend(companions::importers(tx, snap.id, &file.path)?);
            if file.fresh {
                let mut defs: Vec<store::SymbolSpan> = Vec::new();
                for span in file.hits.iter().filter_map(|h| h.enclosing.as_ref()) {
                    if !defs.contains(span) {
                        defs.push(span.clone());
                    }
                }
                let canon = root
                    .canonicalize()
                    .map_err(|e| format!("cannot resolve root: {e}"))?;
                // The repository name only labels the edge document, which
                // the companion does not read.
                found.extend(companions::callers(
                    tx, "", &canon, snap.id, &file.path, &defs, freshness,
                )?);
            }
        }
        file.companions = found;
    }
    Ok(())
}

fn refusal(message: &str) -> String {
    format!("{INVALID_INPUT}{message}")
}

/// The message of a refused request, or `None` for a walk or storage failure.
pub(crate) fn refusal_message(error: &str) -> Option<&str> {
    error.strip_prefix(INVALID_INPUT)
}

fn compile(req: &GrepRequest<'_>) -> Result<regex::bytes::Regex, String> {
    if req.pattern.is_empty() {
        return Err(refusal("pattern must not be empty"));
    }
    let source = if req.regex {
        req.pattern.to_string()
    } else {
        regex::escape(req.pattern)
    };
    regex::bytes::RegexBuilder::new(&source)
        .case_insensitive(req.ignore_case)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| refusal(&format!("pattern does not compile: {e}")))
}

/// The path prefix as a repository-relative directory or file, without
/// leading `./` or trailing slashes.
fn normalize_prefix(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let slashed = raw.replace('\\', "/");
    let trimmed = slashed.trim_start_matches("./").trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." || Path::new(trimmed).is_absolute() {
        return Err(refusal(&format!(
            "path must be a relative path inside the repository, got '{raw}'"
        )));
    }
    Ok(Some(trimmed.to_string()))
}

fn under_prefix(rel: &str, prefix: Option<&str>) -> bool {
    match prefix {
        None => true,
        Some(p) => {
            rel == p
                || rel
                    .strip_prefix(p)
                    .is_some_and(|rest| rest.starts_with('/'))
        }
    }
}

/// The snapshot, its indexed digests, and its freshness verdict. Without a
/// transaction or a snapshot the verdict is `unknown` with `no-index`.
#[allow(clippy::type_complexity)]
fn snapshot_state(
    root: &Path,
    tx: Option<&rusqlite::Transaction<'_>>,
) -> Result<
    (
        Option<store::Snapshot>,
        Option<HashMap<String, String>>,
        String,
        Vec<String>,
    ),
    String,
> {
    let no_index = || ("unknown".to_string(), vec!["no-index".to_string()]);
    let Some(tx) = tx else {
        let (freshness, reasons) = no_index();
        return Ok((None, None, freshness, reasons));
    };
    let Some(snapshot) = store::latest_snapshot(tx)? else {
        let (freshness, reasons) = no_index();
        return Ok((None, None, freshness, reasons));
    };
    let digests = store::indexed_file_digests(tx, snapshot.id)?;
    let (freshness, reasons) = match root.canonicalize() {
        Ok(canon) => index::evaluate_snapshot(tx, "", &canon, &snapshot),
        Err(_) => ("unknown".to_string(), vec!["root-unreadable".to_string()]),
    };
    Ok((Some(snapshot), Some(digests), freshness, reasons))
}

/// `(in_reason, fresh)` for one file's bytes against the indexed digests.
fn index_state(
    digests: Option<&HashMap<String, String>>,
    rel: &str,
    bytes: &[u8],
) -> (&'static str, bool) {
    match digests {
        None => ("no-index", false),
        Some(map) => match map.get(rel) {
            None => ("not-indexed", false),
            Some(indexed) if *indexed == store::content_digest(bytes) => ("indexed", true),
            Some(_) => ("file-changed-since-index", false),
        },
    }
}

/// The true number of matching lines, and the first few of them as hits.
fn search_lines(pattern: &regex::bytes::Regex, bytes: &[u8]) -> (usize, Vec<RawHit>) {
    let mut count = 0;
    let mut hits = Vec::new();
    // A final newline ends the last line; it does not start another one, and
    // an empty file has no lines at all.
    if bytes.is_empty() {
        return (count, hits);
    }
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    for (index, raw) in body.split(|b| *b == b'\n').enumerate() {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        let Some(found) = pattern.find(line) else {
            continue;
        };
        count += 1;
        if hits.len() < MAX_HITS_PER_FILE {
            hits.push(RawHit {
                line: index as u32 + 1,
                col: found.start() as u32 + 1,
                text: line_text(line),
            });
        }
    }
    (count, hits)
}

/// The line without its `\r`, decoded lossily and cut to `MAX_LINE_BYTES` at
/// a character boundary.
fn line_text(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = String::from_utf8_lossy(line);
    let mut end = text.len().min(MAX_LINE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The smallest definition whose line range holds `line`; ties go to the
/// later start, which is the inner one.
fn innermost(spans: &[store::SymbolSpan], line: u32) -> Option<store::SymbolSpan> {
    spans
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| {
            (
                s.end_line.saturating_sub(s.start_line),
                Reverse(s.start_line),
            )
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{index, store, testutil::tmpdir};
    use std::path::{Path, PathBuf};

    /// A repository directory with the given files (relative paths).
    fn repo_with(files: &[(&str, &[u8])]) -> PathBuf {
        let root = tmpdir("grep-repo");
        for (rel, bytes) in files {
            let full = root.join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, bytes).unwrap();
        }
        root
    }

    /// Index `root` under a fresh home as repository `r`; returns the home.
    fn indexed(root: &Path) -> PathBuf {
        let home = tmpdir("grep-home");
        index::build(&home, "r", root, false).expect("index build");
        home
    }

    fn request(pattern: &str) -> GrepRequest<'_> {
        GrepRequest {
            pattern,
            regex: false,
            ignore_case: false,
            path_prefix: None,
            limit: DEFAULT_LIMIT,
        }
    }

    /// Grep with the snapshot of `home` as the transaction, as the CLI does.
    fn grep_indexed(home: &Path, root: &Path, req: &GrepRequest<'_>) -> GrepResult {
        let mut conn = store::open_live(&store::db_path(home, "r")).unwrap();
        let tx = conn.transaction().unwrap();
        let result = grep_repo(root, Some(&tx), req).expect("grep");
        tx.commit().unwrap();
        result
    }

    #[test]
    fn grep_literal_finds_text_in_string_literal() {
        let root = repo_with(&[(
            "mod.py",
            b"def check(n):\n    if n != 2:\n        raise ValueError(\"wrong argument count\")\n",
        )]);
        let result = grep_repo(&root, None, &request("wrong argument count")).expect("grep");
        assert_eq!(result.files.len(), 1);
        let file = &result.files[0];
        assert_eq!(file.path, "mod.py");
        assert_eq!(file.hit_count, 1);
        let hit = &file.hits[0];
        assert_eq!(hit.line, 3);
        let line = "        raise ValueError(\"wrong argument count\")";
        assert_eq!(hit.col as usize, line.find("wrong").unwrap() + 1);
        assert_eq!(hit.text, line);
        assert_eq!(hit.in_reason, "no-index");
        assert!(hit.enclosing.is_none());
    }

    #[test]
    fn grep_maps_hit_to_innermost_definition() {
        let root = repo_with(&[(
            "box.py",
            b"class Box:\n    def fill(self, value):\n        self.value = value  # MARKER\n\n",
        )]);
        let home = indexed(&root);
        let result = grep_indexed(&home, &root, &request("MARKER"));
        let hit = &result.files[0].hits[0];
        assert_eq!(hit.in_reason, "indexed");
        let enclosing = hit.enclosing.as_ref().expect("enclosing definition");
        assert_eq!(enclosing.name, "fill");
        assert_eq!(enclosing.start_line, 2);
    }

    #[test]
    fn grep_maps_hits_only_for_unchanged_files() {
        let root = repo_with(&[
            ("a.py", b"def fa():\n    return \"needle\"\n"),
            ("b.py", b"def fb():\n    return \"needle\"\n"),
        ]);
        let home = indexed(&root);
        std::fs::write(
            root.join("a.py"),
            b"def fa():\n    return \"needle changed\"\n",
        )
        .unwrap();
        let result = grep_indexed(&home, &root, &request("needle"));
        let changed = result.files.iter().find(|f| f.path == "a.py").unwrap();
        assert_eq!(changed.hits[0].in_reason, "file-changed-since-index");
        assert!(changed.hits[0].enclosing.is_none());
        assert!(!changed.fresh);
        let same = result.files.iter().find(|f| f.path == "b.py").unwrap();
        assert_eq!(same.hits[0].in_reason, "indexed");
        assert_eq!(same.hits[0].enclosing.as_ref().unwrap().name, "fb");
        assert!(same.fresh);
    }

    #[test]
    fn grep_searches_unsupported_text_files() {
        let root = repo_with(&[("parser.y", b"%token NEEDLE\n%%\n")]);
        let home = indexed(&root);
        let result = grep_indexed(&home, &root, &request("NEEDLE"));
        assert_eq!(result.files.len(), 1);
        let hit = &result.files[0].hits[0];
        assert_eq!(result.files[0].path, "parser.y");
        assert_eq!(hit.in_reason, "not-indexed");
        assert!(hit.enclosing.is_none());
    }

    #[test]
    fn grep_skips_binary_and_large_files() {
        let mut big = b"needle".to_vec();
        big.resize(scan::MAX_FILE_BYTES as usize + 1, b'a');
        let root = repo_with(&[("blob.bin", b"needle\0\x01\x02"), ("big.txt", &big)]);
        let result = grep_repo(&root, None, &request("needle")).expect("grep");
        assert!(result.files.is_empty());
        assert_eq!(result.totals.files_scanned, 2);
        assert_eq!(result.totals.files_skipped_binary, 1);
        assert_eq!(result.totals.files_skipped_large, 1);
        assert_eq!(result.totals.files_matched, 0);
        assert_eq!(result.totals.hits, 0);
    }

    #[test]
    fn grep_truncates_line_on_char_boundary() {
        // "é" occupies bytes 199 and 200, so byte 200 is not a boundary.
        let line = format!("{}é needle\n", "x".repeat(199));
        let root = repo_with(&[("notes.txt", line.as_bytes())]);
        let result = grep_repo(&root, None, &request("needle")).expect("grep");
        assert_eq!(result.files[0].hits[0].text, "x".repeat(199));
    }

    #[test]
    fn grep_ranks_non_test_files_first() {
        let root = repo_with(&[
            ("tests/test_core.py", b"needle\nneedle\nneedle\n"),
            ("src/core.py", b"needle\n"),
        ]);
        let result = grep_repo(&root, None, &request("needle")).expect("grep");
        let paths: Vec<&str> = result.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/core.py", "tests/test_core.py"]);
    }

    fn regex_request(pattern: &str) -> GrepRequest<'_> {
        GrepRequest {
            regex: true,
            ..request(pattern)
        }
    }

    fn hit_lines(file: &GrepFile) -> Vec<u32> {
        file.hits.iter().map(|h| h.line).collect()
    }

    #[test]
    fn grep_crlf_lines_match_without_the_carriage_return() {
        let root = repo_with(&[("win.txt", b"a;\r\n\r\nb\r\n")]);
        let result = grep_repo(&root, None, &regex_request(";$")).expect("grep");
        assert_eq!(
            hit_lines(&result.files[0]),
            [1],
            "the \\r is not part of the line"
        );
        let result = grep_repo(&root, None, &regex_request("^$")).expect("grep");
        assert_eq!(
            hit_lines(&result.files[0]),
            [2],
            "a blank CRLF line is blank"
        );
        assert_eq!(result.files[0].hits[0].text, "");
    }

    #[test]
    fn grep_final_newline_does_not_start_a_phantom_line() {
        let root = repo_with(&[("end.txt", b"x\n")]);
        let result = grep_repo(&root, None, &regex_request("^$")).expect("grep");
        assert!(result.files.is_empty(), "no empty line 2 exists");
        let result = grep_repo(&root, None, &regex_request("x*")).expect("grep");
        assert_eq!(result.files[0].hit_count, 1, "one line, one hit");
    }

    #[test]
    fn grep_empty_file_has_no_lines() {
        let root = repo_with(&[("empty.txt", b"")]);
        let result = grep_repo(&root, None, &regex_request("^")).expect("grep");
        assert!(result.files.is_empty());
        assert_eq!(result.totals.hits, 0);
        assert_eq!(result.totals.files_scanned, 1);
    }

    #[test]
    fn grep_maps_each_file_to_its_own_definitions() {
        let root = repo_with(&[
            ("one.py", b"def first():\n    return \"needle\"\n"),
            ("two.py", b"def second():\n    return \"needle\"\n"),
        ]);
        let home = indexed(&root);
        let result = grep_indexed(&home, &root, &request("needle"));
        let mapped: Vec<(String, String)> = result
            .files
            .iter()
            .map(|f| {
                (
                    f.path.clone(),
                    f.hits[0].enclosing.as_ref().unwrap().name.clone(),
                )
            })
            .collect();
        assert_eq!(
            mapped,
            [
                ("one.py".to_string(), "first".to_string()),
                ("two.py".to_string(), "second".to_string()),
            ]
        );
    }

    #[test]
    fn grep_rejects_empty_and_invalid_patterns() {
        let root = repo_with(&[("a.txt", b"x\n")]);
        let regex = |pattern: &'static str| GrepRequest {
            pattern,
            regex: true,
            ..request(pattern)
        };
        for req in [request(""), regex("("), regex(r"\pL{1000}")] {
            let err = grep_repo(&root, None, &req).err().expect("must be refused");
            assert!(err.starts_with("invalid_input: "), "{err}");
        }
        assert!(grep_repo(&root, None, &regex("ne+dle")).is_ok());
    }

    #[test]
    fn grep_ignore_case_and_path_prefix() {
        let root = repo_with(&[
            ("a/x.txt", b"Needle\n"),
            ("ab/z.txt", b"needle\n"),
            ("b/y.txt", b"needle\n"),
        ]);
        let exact = grep_repo(&root, None, &request("needle")).expect("grep");
        assert_eq!(exact.files.len(), 2, "case-sensitive: Needle is not a hit");
        let req = GrepRequest {
            ignore_case: true,
            path_prefix: Some("a/"),
            ..request("needle")
        };
        let result = grep_repo(&root, None, &req).expect("grep");
        let paths: Vec<&str> = result.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["a/x.txt"],
            "prefix is a directory, not a text prefix"
        );
    }

    #[test]
    fn grep_reports_truncation_reason() {
        let root = repo_with(&[
            ("f0.txt", b"needle\n"),
            ("f1.txt", b"needle\n"),
            ("f2.txt", b"needle\n"),
        ]);
        let req = GrepRequest {
            limit: 2,
            ..request("needle")
        };
        let result = grep_repo(&root, None, &req).expect("grep");
        assert_eq!(result.truncated, Some("limit"));
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.totals.files_matched, 3);

        let many: Vec<(String, Vec<u8>)> = (0..50)
            .map(|i| (format!("m{i:02}.txt"), b"needle\n".repeat(5)))
            .collect();
        let refs: Vec<(&str, &[u8])> = many
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        let root = repo_with(&refs);
        let req = GrepRequest {
            limit: 50,
            ..request("needle")
        };
        let result = grep_repo(&root, None, &req).expect("grep");
        assert_eq!(result.truncated, Some("total-hits"));
        assert_eq!(result.totals.hits, 250, "the real count is still reported");
        let shown: usize = result.files.iter().map(|f| f.hits.len()).sum();
        assert_eq!(shown, MAX_TOTAL_HITS);

        let root = repo_with(&[("g0.txt", b"needle\n"), ("g1.txt", b"needle\n")]);
        let result = grep_repo_with_budget(&root, None, &request("needle"), 10).expect("grep");
        assert_eq!(result.truncated, Some("bytes-read"));
        assert_eq!(result.totals.files_scanned, 1);
    }

    #[test]
    fn grep_response_budget_drops_files_and_reports_it() {
        let line = format!("needle {}\n", "x".repeat(190));
        // Long names push 200 one-hit files past the budget.
        let many: Vec<(String, Vec<u8>)> = (0..200)
            .map(|i| (format!("{i:0>100}.txt"), line.clone().into_bytes()))
            .collect();
        let refs: Vec<(&str, &[u8])> = many
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        let root = repo_with(&refs);
        let req = GrepRequest {
            limit: MAX_LIMIT,
            ..request("needle")
        };
        let result = grep_repo(&root, None, &req).expect("grep");
        let json = crate::response::grep_response_json("r", &req, &result);
        assert!(json.len() <= MAX_RESPONSE_BYTES, "{} bytes", json.len());
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["truncated"]["reason"], "response-bytes");
        assert_eq!(
            value["totals"]["files_matched"], 200,
            "totals stay truthful"
        );
        assert!(value["files"].as_array().unwrap().len() < 200);
    }

    #[test]
    fn grep_response_budget_counts_companion_bytes() {
        let files: Vec<GrepFile> = (0..30)
            .map(|i| GrepFile {
                path: format!("{i:02}.txt"),
                hit_count: 1,
                hits: vec![GrepHit {
                    line: 1,
                    col: 1,
                    text: "needle".to_string(),
                    enclosing: None,
                    in_reason: "no-index",
                }],
                fresh: false,
                companions: (0..3)
                    .map(|k| Companion {
                        path: format!("{}{k}", "c".repeat(900)),
                        why: "name-pair",
                        basis: "path-convention",
                        via: "x".to_string(),
                    })
                    .collect(),
            })
            .collect();
        let result = GrepResult {
            files,
            totals: GrepTotals {
                files_matched: 30,
                hits: 30,
                ..GrepTotals::default()
            },
            truncated: None,
            snapshot: None,
            freshness: "unknown".to_string(),
            reasons: vec!["no-index".to_string()],
        };
        let req = request("needle");
        let json = crate::response::grep_response_json("r", &req, &result);
        assert!(json.len() <= MAX_RESPONSE_BYTES, "{} bytes", json.len());
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["truncated"]["reason"], "response-bytes");
        assert!(value["files"].as_array().unwrap().len() < 30);
        assert_eq!(value["files"][0]["companions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn grep_response_key_order_and_empty_hint() {
        let root = repo_with(&[("a.txt", b"x\n")]);
        let req = request("zzz");
        let result = grep_repo(&root, None, &req).expect("grep");
        let json = crate::response::grep_response_json("r", &req, &result);
        assert!(
            json.starts_with(
                "{\"repo\":\"r\",\"pattern\":\"zzz\",\"mode\":\"literal\",\"files\":[],\"totals\":{\"files_scanned\":1,"
            ),
            "{json}"
        );
        assert!(json.ends_with(
            "\"truncated\":null,\"snapshot\":null,\"empty\":{\"searched\":\"text-lines\",\"mode\":\"literal\",\"next\":[\"code_search\",\"code_query\"]}}"
        ), "{json}");
    }

    #[test]
    fn grep_with_index_reports_snapshot_freshness() {
        let root = repo_with(&[("a.py", b"def fa():\n    return 1\n")]);
        let home = indexed(&root);
        let result = grep_indexed(&home, &root, &request("return"));
        assert_eq!(result.freshness, "fresh");
        assert!(result.reasons.is_empty());
        assert!(result.snapshot.is_some());
    }
}
