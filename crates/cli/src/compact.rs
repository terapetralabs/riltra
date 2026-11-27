// SPDX-License-Identifier: Apache-2.0
//! The compact text form of a `grep` answer, served only by
//! `riltra mcp --compact`. It is built from the same `GrepResult`, with the same
//! file selection, as the JSON document.
//!
//! One exception: escaping can make the text longer than the JSON (a C1 control
//! is 2 bytes in JSON and 6 here, a line separator 3 and 6). When the text would
//! exceed the response budget, there is no compact form and the server answers
//! with the JSON document, so no file or hit is dropped to make room.
//!
//! ```text
//! grep repo=<word> mode=<word> pattern=<word>
//! snapshot id=<n> freshness=<word> reasons=[<word>,...]      | snapshot null
//! totals files_scanned=<n> ... files_matched=<n> hits=<n>
//! truncated=<null|word> caller_gaps=[...]
//! empty searched=<word> mode=<word> next=[...]               (no hits only)
//! file <word> hit_count=<n>
//!  in <kind> <name> line=<n> [in_reason=<word>]              | no-definition [...]
//!   <line>:<col>: <the source line, verbatim>                | <line>:<col>= "<escaped>"
//!  companions why=<word> basis=<word> via=<word> paths=[...]
//! ```
//!
//! A word is bare when it is made of letters, digits and `_-./+@~`, and a JSON
//! string otherwise. A source line is verbatim after `: ` unless it holds a
//! control or line-separator character other than a tab, when it is a JSON
//! string after `= `. No value can start a new line or pose as another field.
//!
//! Left out on purpose: `totals.bytes_read` (a cost figure, not a fact about
//! the answer), and `in_reason` when it is `indexed`, the default.

use crate::companions::Companion;
use crate::grep::{GrepFile, GrepHit, GrepRequest, GrepResult, MAX_RESPONSE_BYTES};
use crate::{json, response};

/// What a hit sits in (the definition's kind, name and line) and why.
type Within<'a> = (Option<(&'a str, &'a str, u32)>, &'a str);

fn breaks(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    // `json::escape` covers C0 and DEL; C1 and U+2028/9 are escaped here.
    for c in json::escape(value).chars() {
        if breaks(c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

fn word(value: &str) -> String {
    let bare = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || "_-./+@~".contains(c));
    if bare {
        value.to_string()
    } else {
        quoted(value)
    }
}

fn list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let words: Vec<String> = items.into_iter().map(word).collect();
    format!("[{}]", words.join(","))
}

fn within(hit: &GrepHit) -> Within<'_> {
    let definition = hit
        .enclosing
        .as_ref()
        .map(|s| (s.kind.as_str(), s.name.as_str(), s.start_line));
    (definition, hit.in_reason)
}

fn within_line((definition, in_reason): Within<'_>) -> String {
    let mut line = match definition {
        Some((kind, name, at)) => format!(" in {} {} line={at}", word(kind), word(name)),
        None => " no-definition".to_string(),
    };
    if in_reason != "indexed" {
        line.push_str(&format!(" in_reason={}", word(in_reason)));
    }
    line
}

fn hit_line(hit: &GrepHit) -> String {
    if hit.text.chars().any(|c| c != '\t' && breaks(c)) {
        format!("  {}:{}= {}", hit.line, hit.col, quoted(&hit.text))
    } else {
        format!("  {}:{}: {}", hit.line, hit.col, hit.text)
    }
}

fn same_kind(a: &Companion, b: &Companion) -> bool {
    (a.why, a.basis, &a.via) == (b.why, b.basis, &b.via)
}

/// One file: hits grouped under what holds them, then companions, with
/// consecutive companions of one kind on one line. Order is the result's.
fn file_lines(file: &GrepFile, lines: &mut Vec<String>) {
    lines.push(format!(
        "file {} hit_count={}",
        word(&file.path),
        file.hit_count
    ));
    for group in file.hits.chunk_by(|a, b| within(a) == within(b)) {
        lines.push(within_line(within(&group[0])));
        lines.extend(group.iter().map(hit_line));
    }
    for group in file.companions.chunk_by(same_kind) {
        lines.push(format!(
            " companions why={} basis={} via={} paths={}",
            word(group[0].why),
            word(group[0].basis),
            word(&group[0].via),
            list(group.iter().map(|c| c.path.as_str()))
        ));
    }
}

/// `None` when the text would exceed `MAX_RESPONSE_BYTES`: the caller answers
/// with the JSON document instead.
pub(crate) fn grep_text(repo: &str, req: &GrepRequest<'_>, result: &GrepResult) -> Option<String> {
    // The same files as the JSON document, so the same evidence.
    let (shown, truncated, _) = response::grep_fit(repo, req, result);
    let mode = response::grep_mode(req);
    let totals = &result.totals;
    let mut lines = vec![
        format!(
            "grep repo={} mode={mode} pattern={}",
            word(repo),
            word(req.pattern)
        ),
        match &result.snapshot {
            Some(snapshot) => format!(
                "snapshot id={} freshness={} reasons={}",
                snapshot.id,
                word(&result.freshness),
                list(result.reasons.iter().map(String::as_str))
            ),
            None => "snapshot null".to_string(),
        },
        format!(
            "totals files_scanned={} files_skipped_binary={} files_skipped_large={} \
             files_skipped_unreadable={} dirs_unreadable={} dirs_escaped={} files_matched={} hits={}",
            totals.files_scanned,
            totals.files_skipped_binary,
            totals.files_skipped_large,
            totals.files_skipped_unreadable,
            totals.dirs_unreadable,
            totals.dirs_escaped,
            totals.files_matched,
            totals.hits
        ),
        format!(
            "truncated={} caller_gaps={}",
            truncated.unwrap_or("null"),
            list(result.caller_gaps.iter().copied())
        ),
    ];
    if let Some((searched, next)) = response::grep_empty(totals.hits, truncated) {
        lines.push(format!(
            "empty searched={searched} mode={mode} next={}",
            list(next)
        ));
    }
    for file in &result.files[..shown] {
        file_lines(file, &mut lines);
    }
    let text = lines.join("\n");
    (text.len() <= MAX_RESPONSE_BYTES).then_some(text)
}

#[cfg(test)]
#[path = "compact_tests.rs"]
mod tests;
