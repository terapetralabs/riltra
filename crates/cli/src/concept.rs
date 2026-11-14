// SPDX-License-Identifier: Apache-2.0
//! Concept search text (PR 6): the identifier tokens, path tokens and doc text
//! of every symbol of a snapshot, kept in `concept_fts` so a later query can
//! find a definition by the words in its name, its path or its comment.

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharKind {
    Separator,
    Upper,
    Digit,
    Lower,
}

fn char_kind(c: char) -> CharKind {
    // shortcut: only ASCII uppercase starts a word, so a non-ASCII uppercase letter never does (upgrade when a real corpus has identifiers in a cased non-Latin script).
    if c.is_ascii_uppercase() {
        CharKind::Upper
    } else if c.is_ascii_digit() {
        CharKind::Digit
    } else if c.is_alphanumeric() {
        CharKind::Lower
    } else {
        CharKind::Separator
    }
}

/// True when the char at `i` starts a new word (a letter-to-digit change, a
/// lower-to-upper boundary, or the last upper of an upper run before a lower).
fn starts_word(kinds: &[CharKind], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let (prev, kind) = (kinds[i - 1], kinds[i]);
    let next_is_lower = kinds.get(i + 1) == Some(&CharKind::Lower);
    (prev == CharKind::Lower && kind == CharKind::Upper)
        || ((prev == CharKind::Digit) != (kind == CharKind::Digit))
        || (prev == CharKind::Upper && kind == CharKind::Upper && next_is_lower)
}

fn push_word(words: &mut Vec<String>, word: &mut String) {
    if !word.is_empty() {
        words.push(std::mem::take(word));
    }
}

/// Lowercase words of an identifier or path segment (PR 6).
pub fn split_identifier(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let kinds: Vec<CharKind> = chars.iter().map(|&c| char_kind(c)).collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if kinds[i] == CharKind::Separator {
            push_word(&mut words, &mut word);
            continue;
        }
        if starts_word(&kinds, i) {
            push_word(&mut words, &mut word);
        }
        word.extend(c.to_lowercase());
    }
    push_word(&mut words, &mut word);
    words
}

/// The `path` column of a `concept_fts` row: the relative path as stored, then
/// the words of every `/`-separated segment (extension included).
fn path_tokens(path: &str) -> String {
    let mut parts = vec![path.to_string()];
    for segment in path.split('/') {
        parts.extend(split_identifier(segment));
    }
    parts.join(" ")
}

/// Rebuild every `concept_fts` row of `snapshot_id` from its symbols.
pub fn build_concept_index(conn: &rusqlite::Connection, snapshot_id: i64) -> Result<(), String> {
    conn.execute("DELETE FROM concept_fts", [])
        .map_err(|e| e.to_string())?;
    let symbols: Vec<(i64, Option<String>, String, i64, i64, i64)> = {
        let mut st = conn
            .prepare(
                "SELECT s.id, s.name, f.path, s.file_id, s.byte_start, s.byte_end FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.snapshot_id = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([snapshot_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
    };
    let mut docs = conn
        .prepare(
            "SELECT text FROM doc_comments WHERE snapshot_id = ?1 AND file_id = ?2 AND def_byte_start = ?3 AND def_byte_end = ?4 ORDER BY form",
        )
        .map_err(|e| e.to_string())?;
    let mut insert = conn
        .prepare("INSERT INTO concept_fts(rowid, tokens, path, doc) VALUES (?1, ?2, ?3, ?4)")
        .map_err(|e| e.to_string())?;
    for (id, name, path, file_id, byte_start, byte_end) in &symbols {
        let texts: Vec<String> = docs
            .query_map(
                rusqlite::params![snapshot_id, file_id, byte_start, byte_end],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let tokens = name
            .as_deref()
            .map(split_identifier)
            .unwrap_or_default()
            .join(" ");
        insert
            .execute(rusqlite::params![
                id,
                tokens,
                path_tokens(path),
                texts.join("\n")
            ])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_identifier;

    #[test]
    fn identifiers_split_on_separators_case_boundaries_and_digits() {
        // Hand-written expectations; the last block are names from this repository.
        let cases: [(&str, &[&str]); 27] = [
            ("parseHttp", &["parse", "http"]),
            ("parseHTTPResponse", &["parse", "http", "response"]),
            ("XMLHttp", &["xml", "http"]),
            ("HTTPServer2", &["http", "server", "2"]),
            ("HTML5Parser", &["html", "5", "parser"]),
            ("v2Api", &["v", "2", "api"]),
            ("getHTTP", &["get", "http"]),
            ("ABC", &["abc"]),
            ("a", &["a"]),
            ("x1y2", &["x", "1", "y", "2"]),
            ("__init__", &["init"]),
            ("", &[]),
            ("___", &[]),
            ("snake_case_name", &["snake", "case", "name"]),
            ("kebab-case.name here", &["kebab", "case", "name", "here"]),
            ("Widget::run", &["widget", "run"]),
            ("operator<<", &["operator"]),
            ("~Foo", &["foo"]),
            ("naïveBayes", &["naïve", "bayes"]),
            ("ÉtatCivil", &["état", "civil"]),
            ("split_identifier", &["split", "identifier"]),
            ("parse_source", &["parse", "source"]),
            ("FileFacts", &["file", "facts"]),
            ("RILTRA_HOME", &["riltra", "home"]),
            ("copy_mapped_facts", &["copy", "mapped", "facts"]),
            (
                "ReferenceOccurrenceRow",
                &["reference", "occurrence", "row"],
            ),
            (
                "a_v5_database_is_outdated_for_reads",
                &["a", "v", "5", "database", "is", "outdated", "for", "reads"],
            ),
        ];
        for (name, expected) in cases {
            assert_eq!(split_identifier(name), expected, "{name:?}");
        }
    }
}
