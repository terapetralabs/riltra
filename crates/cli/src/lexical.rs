// SPDX-License-Identifier: Apache-2.0
//! Lexical concept search (PR 9, `query`): a deterministic BM25 ranker over the
//! `concept_fts` table, shared by the CLI and the MCP tool. Lexical by default:
//! `results` is the ranked list alone. With `expand` (PR 9c), one hop along
//! proven edges from those results is added under `expansion` (see `expand`).
//! No stemming, no stoplist, no score in the output.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, Transaction};

use crate::{concept, expand, index, response, store};

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_TEXT_BYTES: usize = 16384;
pub const MAX_TERMS: usize = 64;
pub const BASIS: &str = "lexical-bm25-concept-fts";
// shortcut: the candidate list keeps at most this many rows per list (non-test, test) before `--limit`, so a broad term over a very large index holds bounded memory; upgrade (SQL-side demotion or a larger cap) when a real repository's match count reaches it.
pub const CANDIDATE_CAP: usize = 5000;

const TEST_WORDS: [&str; 5] = ["test", "tests", "testing", "spec", "specs"];
const UNREADABLE: &str = "index database unreadable; rebuild with `index`";

/// The query terms of a text (rule 1): the lowercase words of every
/// whitespace-separated run, distinct, at least 3 characters, never made of numeric
/// characters only (any script), the first `MAX_TERMS`.
pub fn extract_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for run in text.split_whitespace() {
        for word in concept::split_identifier(run) {
            if word.chars().count() < 3 || word.chars().all(char::is_numeric) {
                continue;
            }
            if !terms.contains(&word) {
                terms.push(word);
            }
            if terms.len() == MAX_TERMS {
                return terms;
            }
        }
    }
    terms
}

/// The refusals of rule 6, shared by the CLI and the MCP tool.
pub fn validate(text: &str, limit: usize) -> Result<(), String> {
    if text.is_empty() {
        return Err("--text must not be empty".to_string());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!("--text exceeds {MAX_TEXT_BYTES} bytes"));
    }
    if !(1..=index::MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(format!(
            "--limit must be between 1 and {}",
            index::MAX_SEARCH_LIMIT
        ));
    }
    Ok(())
}

/// A refusal of a readable index: the MCP error kind and the message both
/// surfaces print.
pub struct Refusal {
    pub kind: &'static str,
    pub message: String,
}

impl Refusal {
    fn store(message: impl ToString) -> Self {
        Refusal {
            kind: "store_error",
            message: message.to_string(),
        }
    }
}

/// The refusal for a database whose schema this binary does not read.
pub fn ahead_message(version: u32) -> String {
    format!(
        "database schema v{version} is newer than supported v{}; use a riltra binary that supports this database",
        store::SCHEMA_VERSION
    )
}

/// The refusal for an older schema. Unlike the other tools, `query` needs the
/// rebuilt index: a migration alone leaves `concept_fts` empty, so every query
/// would answer `no-match`.
pub fn outdated_message(repo: &str, version: u32) -> String {
    format!(
        "index schema v{version} is outdated; run `riltra index --repo {repo}` to rebuild it (concept search reads tables only a rebuild fills), then retry"
    )
}

/// Rule 5: demotion of test paths applies unless a term names tests.
pub fn demotion_applies(terms: &[String]) -> bool {
    !terms.iter().any(|t| TEST_WORDS.contains(&t.as_str()))
}

/// One candidate symbol, in rule-4 order.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: i64,
    pub path: String,
    pub name: Option<String>,
    pub kind: String,
    pub line: u32,
    pub byte_start: usize,
}

/// Rules 5 and 6: rows arrive in rule-4 order and are split into a non-test
/// and a test list, each keeping at most `cap` rows. With demotion the result
/// is the non-test list then the test list; without it, the first `cap` rows
/// of the single stream.
pub fn select_candidates(
    rows: impl IntoIterator<Item = Result<Candidate, String>>,
    demote: bool,
    cap: usize,
) -> Result<Vec<Candidate>, String> {
    let mut plain: Vec<Candidate> = Vec::new();
    let mut tests: Vec<Candidate> = Vec::new();
    for row in rows {
        let row = row?;
        let list = if demote && index::is_test_path(&row.path) {
            &mut tests
        } else {
            &mut plain
        };
        if list.len() < cap {
            list.push(row);
        }
    }
    plain.extend(tests);
    Ok(plain)
}

/// One ranked result: a symbol and the query terms FTS matched on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub line: u32,
    pub name: Option<String>,
    pub kind: String,
    pub why: Vec<String>,
    /// Internal, never serialised: the stored span start (expansion identity).
    pub byte_start: usize,
}

/// The ranked answer of one query on one snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub total: usize,
    pub truncated: bool,
    pub hits: Vec<Hit>,
}

const COUNT_MATCHES: &str = "SELECT COUNT(*) FROM concept_fts JOIN symbols s ON s.id = concept_fts.rowid WHERE concept_fts MATCH ?1 AND s.snapshot_id = ?2";
const RANKED_MATCHES: &str = "SELECT concept_fts.rowid, f.path, s.name, s.kind, s.start_line, s.byte_start FROM concept_fts JOIN symbols s ON s.id = concept_fts.rowid JOIN files f ON f.id = s.file_id WHERE concept_fts MATCH ?1 AND s.snapshot_id = ?2 ORDER BY bm25(concept_fts, 1.0, 1.0, 1.0), f.path, s.start_line, s.byte_start";

/// The query on one caller-provided read transaction, so the snapshot, the
/// verdict, `total` and the rows come from one pinned state.
pub fn query_tx(
    tx: &Transaction<'_>,
    snapshot_id: i64,
    terms: &[String],
    limit: usize,
    cap: usize,
) -> Result<Answer, String> {
    if terms.is_empty() {
        return Ok(Answer {
            total: 0,
            truncated: false,
            hits: Vec::new(),
        });
    }
    let matcher = terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    let total: i64 = tx
        .query_row(COUNT_MATCHES, params![matcher, snapshot_id], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let mut statement = tx.prepare(RANKED_MATCHES).map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![matcher, snapshot_id], |r| {
            Ok(Candidate {
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                kind: r.get(3)?,
                line: r.get::<_, i64>(4)? as u32,
                byte_start: r.get::<_, i64>(5)? as usize,
            })
        })
        .map_err(|e| e.to_string())?
        .map(|row| row.map_err(|e| e.to_string()));
    let candidates = select_candidates(rows, demotion_applies(terms), cap)?;
    let page: Vec<Candidate> = candidates.into_iter().take(limit).collect();
    let why = why_for(tx, terms, &page)?;
    let hits: Vec<Hit> = page
        .into_iter()
        .zip(why)
        .map(|(c, why)| Hit {
            path: c.path,
            line: c.line,
            name: c.name,
            kind: c.kind,
            why,
            byte_start: c.byte_start,
        })
        .collect();
    Ok(Answer {
        total: total as usize,
        truncated: (total as usize) > hits.len(),
        hits,
    })
}

/// For each row, the terms (in query order) that FTS itself matched on it.
fn why_for(
    tx: &Transaction<'_>,
    terms: &[String],
    page: &[Candidate],
) -> Result<Vec<Vec<String>>, String> {
    let mut why: Vec<Vec<String>> = vec![Vec::new(); page.len()];
    if page.is_empty() {
        return Ok(why);
    }
    let ids: Vec<i64> = page.iter().map(|c| c.id).collect();
    let marks = vec!["?"; ids.len()].join(",");
    let sql =
        format!("SELECT rowid FROM concept_fts WHERE concept_fts MATCH ?1 AND rowid IN ({marks})");
    for term in terms {
        let mut args = vec![Value::Text(format!("\"{term}\""))];
        args.extend(ids.iter().map(|&id| Value::Integer(id)));
        let mut statement = tx.prepare(&sql).map_err(|e| e.to_string())?;
        let matched: HashSet<i64> = statement
            .query_map(params_from_iter(args), |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        for (slot, id) in why.iter_mut().zip(&ids) {
            if matched.contains(id) {
                slot.push(term.clone());
            }
        }
    }
    Ok(why)
}

/// The document of a missing index: `no-index`, created nothing. With `expand`
/// the document carries an expansion with status `none-imported`: there is no
/// index, so no import and no seed.
pub fn no_index_document(repo: &str, recorded: &str, text: &str, expand: bool) -> String {
    let answer = Answer {
        total: 0,
        truncated: false,
        hits: Vec::new(),
    };
    let expansion = expand.then(expand::none_imported);
    response::query_response_json(&response::QueryResponse {
        repo,
        root: recorded,
        snapshot: None,
        freshness: "unknown",
        reasons: &["no-index".to_string()],
        terms: &extract_terms(text),
        answer: &answer,
        reason: Some("no-index"),
        expansion: expansion.as_ref(),
    })
}

/// The document of a readable index. Freshness, snapshot and rows come from
/// one read transaction. A root that cannot be canonicalized is reported in
/// the freshness reasons; the match still runs, since it does not need the root.
pub fn ready_document(
    conn: &mut Connection,
    repo: &str,
    recorded: &str,
    text: &str,
    limit: usize,
    expand: bool,
) -> Result<String, Refusal> {
    let terms = extract_terms(text);
    let tx = conn.transaction().map_err(Refusal::store)?;
    let snapshot = match store::latest_snapshot(&tx) {
        Ok(Some(snapshot)) => snapshot,
        _ => return Err(Refusal::store(UNREADABLE)),
    };
    // `concept_fts` has one row per symbol. A schema-5 index migrated by another
    // command (`search` opens it live) is schema 6 with no concept rows, and would
    // answer `no-match` to everything.
    let symbols: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE snapshot_id = ?1",
            [snapshot.id],
            |r| r.get(0),
        )
        .map_err(Refusal::store)?;
    let concept_rows: i64 = tx
        .query_row("SELECT COUNT(*) FROM concept_fts", [], |r| r.get(0))
        .map_err(Refusal::store)?;
    if symbols != concept_rows {
        return Err(Refusal {
            kind: "concept_not_built",
            message: format!(
                "the concept search table of '{repo}' was never built for this snapshot (migrated, not rebuilt); run `riltra index --repo {repo}` to build it, then retry"
            ),
        });
    }
    let canonical = Path::new(recorded).canonicalize();
    let (freshness, reasons, root) = match &canonical {
        Ok(canon) => {
            let (status, reasons) = index::evaluate_snapshot(&tx, repo, canon, &snapshot);
            (status, reasons, snapshot.root.clone())
        }
        Err(_) => (
            "unknown".to_string(),
            vec!["root-unreadable".to_string()],
            recorded.to_string(),
        ),
    };
    let answer =
        query_tx(&tx, snapshot.id, &terms, limit, CANDIDATE_CAP).map_err(Refusal::store)?;
    // Expansion reads the same transaction, so its verdicts and rows come from
    // the pinned state. Its read errors refuse the whole document (R5).
    let source_fresh = freshness == "fresh";
    let expansion = if expand {
        let mut verdict = |id: i64| -> Result<bool, String> {
            match &canonical {
                Ok(canon) => index::import_is_stale(&tx, &snapshot, canon, source_fresh, id),
                Err(e) => Err(format!(
                    "import verdict requested without a readable root: {e}"
                )),
            }
        };
        let found = expand::expand(
            &tx,
            &snapshot,
            source_fresh,
            &answer.hits,
            &expand::Limits::DEFAULT,
            &mut verdict,
        )
        .map_err(Refusal::store)?;
        Some(found)
    } else {
        None
    };
    tx.commit().map_err(Refusal::store)?;
    let reason = if terms.is_empty() {
        Some("no-extractable-term")
    } else if answer.hits.is_empty() {
        Some("no-match")
    } else {
        None
    };
    Ok(response::query_response_json(&response::QueryResponse {
        repo,
        root: &root,
        snapshot: Some(&snapshot),
        freshness: &freshness,
        reasons: &reasons,
        terms: &terms,
        answer: &answer,
        reason,
        expansion: expansion.as_ref(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        demotion_applies, extract_terms, select_candidates, validate, Candidate, MAX_TERMS,
        MAX_TEXT_BYTES,
    };

    fn row(id: i64, path: &str, line: u32) -> Candidate {
        Candidate {
            id,
            path: path.to_string(),
            name: None,
            kind: "function".to_string(),
            line,
            byte_start: 0,
        }
    }

    fn ids(rows: &[Candidate]) -> Vec<i64> {
        rows.iter().map(|c| c.id).collect()
    }

    fn select(rows: Vec<Candidate>, demote: bool, cap: usize) -> Vec<Candidate> {
        select_candidates(rows.into_iter().map(Ok), demote, cap).unwrap()
    }

    #[test]
    fn extraction_follows_the_frozen_rules() {
        // Expectations are written from each text's construction.
        let cases: [(&str, &[&str]); 13] = [
            (
                "parseHttpRequest handler",
                &["parse", "http", "request", "handler"],
            ),
            ("snake_case_name", &["snake", "case", "name"]),
            (
                "`parse_source` and `Widget::run`",
                &["parse", "source", "and", "widget", "run"],
            ),
            ("src/a_b.rs:42:7", &["src"]),
            ("Alpha alpha ALPHA beta Beta", &["alpha", "beta"]),
            ("x1 ab 123 ok go 99 xyz", &["xyz"]),
            // Digits of any script are digits: nothing is left of these texts.
            ("123 ١٢٣ １２３ ४५६", &[]),
            ("²³⁴ ⅫⅫⅫ ½½½", &[]),
            ("abc١٢٣", &["abc١٢٣"]),
            ("Café naïveBayes état", &["café", "naïve", "bayes", "état"]),
            (
                "first line\nsecond\tthird",
                &["first", "line", "second", "third"],
            ),
            ("--text", &["text"]),
            ("   \t\n  ", &[]),
        ];
        for (text, expected) in cases {
            assert_eq!(extract_terms(text), expected, "{text:?}");
        }
    }

    #[test]
    fn a_long_text_keeps_the_first_sixty_four_distinct_terms() {
        // Seventy distinct four-letter words "wXYx"; the first 64 are kept.
        let words: Vec<String> = (0..70)
            .map(|i| {
                format!(
                    "w{}{}x",
                    (b'a' + (i / 26) as u8) as char,
                    (b'a' + (i % 26) as u8) as char
                )
            })
            .collect();
        let text = words.join(" ");
        assert_eq!(MAX_TERMS, 64);
        assert_eq!(extract_terms(&text), words[..64].to_vec());
    }

    #[test]
    fn demotion_is_lifted_only_by_a_test_word_term() {
        let terms = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(demotion_applies(&terms(&["widget"])));
        assert!(demotion_applies(&terms(&["latest", "widget"])));
        for word in ["test", "tests", "testing", "spec", "specs"] {
            assert!(!demotion_applies(&terms(&["widget", word])), "{word}");
        }
    }

    #[test]
    fn validation_names_the_refused_input() {
        assert!(validate("", 20).unwrap_err().contains("--text"));
        let too_long = "a".repeat(MAX_TEXT_BYTES + 1);
        assert!(validate(&too_long, 20).unwrap_err().contains("16384"));
        assert!(validate(&"a".repeat(MAX_TEXT_BYTES), 20).is_ok());
        assert!(validate("ok", 0).unwrap_err().contains("--limit"));
        assert!(validate("ok", 1001).is_err());
        assert!(validate("ok", 1).is_ok());
        assert!(validate("ok", 1000).is_ok());
    }

    #[test]
    fn production_row_survives_the_cap_behind_test_rows() {
        // Rule-4 order: three test rows score better than one production row.
        let rows = vec![
            row(1, "tests/test_a.py", 1),
            row(2, "tests/test_b.py", 1),
            row(3, "tests/test_c.py", 1),
            row(4, "src/prod.rs", 1),
        ];
        assert_eq!(ids(&select(rows.clone(), true, 3)), [4, 1, 2, 3]);
        assert_eq!(ids(&select(rows, false, 3)), [1, 2, 3]);
    }

    #[test]
    fn the_cap_keeps_the_first_rows_of_one_list() {
        let rows: Vec<Candidate> = (1..=5).map(|i| row(i, "src/prod.rs", i as u32)).collect();
        assert_eq!(ids(&select(rows, true, 3)), [1, 2, 3]);
    }

    #[test]
    fn hit_carries_the_stored_byte_start_and_never_serialises_it() {
        use crate::{concept, response, store};
        let mut conn = store::open_memory().unwrap();
        let snap = store::insert_snapshot(
            &conn,
            &store::Snapshot {
                id: 0,
                repo: "r".to_string(),
                root: "repo-root".to_string(),
                created_unix: 1,
                head: None,
                branch: None,
                dirty: false,
                untracked: 0,
                parser_version: store::PARSER_VERSION,
                grammar_versions: store::grammar_versions(),
                config_fingerprint: "c".to_string(),
                pre_fingerprint: "p".to_string(),
                post_fingerprint: "p".to_string(),
                status_at_write: "fresh".to_string(),
                nonce: store::new_nonce(),
            },
        )
        .unwrap();
        let fid = store::insert_file(
            &conn,
            snap,
            &store::FileRow {
                path: "lib.py".to_string(),
                language: "python".to_string(),
                bytes: 40,
                digest: "d".to_string(),
                errors: 0,
            },
        )
        .unwrap();
        store::insert_symbol(
            &conn,
            snap,
            &store::SymbolRow {
                file_id: fid,
                kind: "function".to_string(),
                name: Some("widget_run".to_string()),
                start_line: 1,
                start_col: 1,
                end_line: 2,
                end_col: 9,
                byte_start: 24,
                byte_end: 40,
                provider: "syntax".to_string(),
                grammar: "python:0.25.0".to_string(),
                language: "python".to_string(),
            },
        )
        .unwrap();
        concept::build_concept_index(&conn, snap).unwrap();
        let terms = vec!["widget".to_string()];
        let answer = {
            let tx = conn.transaction().unwrap();
            super::query_tx(&tx, snap, &terms, 20, 100).unwrap()
        };
        assert_eq!(answer.hits.len(), 1);
        assert_eq!(answer.hits[0].byte_start, 24, "the stored span start");
        let render = |answer: &super::Answer| {
            response::query_response_json(&response::QueryResponse {
                repo: "demo",
                root: "repo-root",
                snapshot: None,
                freshness: "fresh",
                reasons: &[],
                terms: &terms,
                answer,
                reason: None,
                expansion: None,
            })
        };
        let mut moved = answer.clone();
        moved.hits[0].byte_start = 999;
        let json = render(&answer);
        assert_eq!(
            json,
            render(&moved),
            "byte_start is internal, never serialised"
        );
        assert!(!json.contains("byte_start"), "{json}");
    }
}
