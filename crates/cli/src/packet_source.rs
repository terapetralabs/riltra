// SPDX-License-Identifier: Apache-2.0
//! Pure source snippet range assembly for task packets.

use std::collections::BTreeMap;

use crate::packet::{ByteRange, SourceSnippet};

pub(crate) struct SnippetInput<'a> {
    pub path: &'a str,
    pub digest: &'a str,
    pub requested: ByteRange,
    pub source: Result<&'a [u8], &'static str>,
    pub anchor_index: usize,
}

pub(crate) fn assemble(inputs: Vec<SnippetInput<'_>>) -> Result<Vec<SourceSnippet>, String> {
    let mut groups: BTreeMap<(String, String, Option<&'static str>), Vec<SnippetInput<'_>>> =
        BTreeMap::new();
    for mut input in inputs {
        let invalid_reason = if input.requested.start >= input.requested.end {
            Some("invalid-source-range")
        } else if let Ok(source) = input.source {
            range_error(&input.requested, source)
        } else {
            None
        };
        if let Some(reason) = invalid_reason {
            input.source = Err(reason);
        }
        groups
            .entry((
                input.path.to_string(),
                input.digest.to_string(),
                invalid_reason,
            ))
            .or_default()
            .push(input);
    }

    let mut snippets = Vec::new();
    for ((path, digest, _reason), mut group) in groups {
        group.sort_by_key(|input| {
            (
                input.requested.start,
                input.requested.end,
                input.anchor_index,
            )
        });
        let mut merged: Vec<(ByteRange, Vec<usize>)> = Vec::new();
        for input in &group {
            if let Some((range, anchors)) = merged.last_mut() {
                if input.requested.start < range.end {
                    range.end = range.end.max(input.requested.end);
                    anchors.push(input.anchor_index);
                    continue;
                }
            }
            merged.push((input.requested.clone(), vec![input.anchor_index]));
        }

        let source = group.first().expect("groups are non-empty").source;
        for (requested, mut anchor_indices) in merged {
            anchor_indices.sort_unstable();
            anchor_indices.dedup();
            let snippet = match source {
                Ok(bytes) => available_snippet(&path, &digest, requested, bytes, anchor_indices)?,
                Err(reason) => SourceSnippet {
                    path: path.clone(),
                    digest: digest.clone(),
                    requested,
                    returned: None,
                    text: None,
                    status: "unavailable",
                    reason: Some(reason),
                    anchor_indices,
                },
            };
            snippets.push(snippet);
        }
    }
    snippets.sort_by(|a, b| {
        (&a.path, a.requested.start, a.requested.end, &a.digest).cmp(&(
            &b.path,
            b.requested.start,
            b.requested.end,
            &b.digest,
        ))
    });
    Ok(snippets)
}

fn validate_range(range: &ByteRange, bytes: &[u8]) -> Result<(), String> {
    range_error(range, bytes).map_or(Ok(()), |reason| Err(reason.to_string()))
}

fn range_error(range: &ByteRange, bytes: &[u8]) -> Option<&'static str> {
    if range.start >= range.end || range.end > bytes.len() {
        return Some("invalid-source-range");
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return Some("source-non-utf8"),
    };
    if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
        return Some("invalid-source-range");
    }
    None
}

fn available_snippet(
    path: &str,
    digest: &str,
    requested: ByteRange,
    bytes: &[u8],
    anchor_indices: Vec<usize>,
) -> Result<SourceSnippet, String> {
    validate_range(&requested, bytes)?;
    let source =
        std::str::from_utf8(bytes).map_err(|_| "snippet source is not UTF-8".to_string())?;
    let mut end = requested.end;
    let byte_cap = requested.start.saturating_add(4096).min(end);
    let mut byte_end = byte_cap;
    while !source.is_char_boundary(byte_end) {
        byte_end -= 1;
    }
    end = end.min(byte_end);

    let mut lines = 0;
    for (offset, ch) in source[requested.start..end].char_indices() {
        if ch == '\n' {
            lines += 1;
            if lines == 40 {
                let after_line = requested.start + offset + 1;
                end = end.min(after_line);
                break;
            }
        }
    }
    let returned = ByteRange {
        start: requested.start,
        end,
    };
    let text = source[returned.start..returned.end].to_string();
    let truncated = returned.end < requested.end;
    Ok(SourceSnippet {
        path: path.to_string(),
        digest: digest.to_string(),
        requested,
        returned: Some(returned),
        text: Some(text),
        status: if truncated { "truncated" } else { "available" },
        reason: truncated.then_some("snippet-cap"),
        anchor_indices,
    })
}

#[cfg(test)]
mod tests {
    use super::{assemble, SnippetInput};
    use crate::packet::ByteRange;

    fn input<'a>(
        path: &'a str,
        digest: &'a str,
        requested: std::ops::Range<usize>,
        source: Result<&'a [u8], &'static str>,
        anchor_index: usize,
    ) -> SnippetInput<'a> {
        SnippetInput {
            path,
            digest,
            requested: ByteRange {
                start: requested.start,
                end: requested.end,
            },
            source,
            anchor_index,
        }
    }

    #[test]
    fn nested_transitive_overlaps_merge_and_union_anchor_indices() {
        let source = b"0123456789abcdefghijkl";
        let got = assemble(vec![
            input("a.py", "d", 1..8, Ok(source), 2),
            input("a.py", "d", 3..10, Ok(source), 1),
            input("a.py", "d", 9..12, Ok(source), 2),
            input("a.py", "d", 4..6, Ok(source), 1),
        ])
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].requested, ByteRange { start: 1, end: 12 });
        assert_eq!(got[0].text.as_deref(), Some("123456789ab"));
        assert_eq!(got[0].anchor_indices, [1, 2]);
        assert_eq!(got[0].status, "available");
    }

    #[test]
    fn adjacent_ranges_remain_separate_and_results_sort_by_path_and_start() {
        let a = b"abcdef";
        let z = b"uvwxyz";
        let got = assemble(vec![
            input("z.py", "z", 3..6, Ok(z), 4),
            input("a.py", "a", 3..6, Ok(a), 3),
            input("a.py", "a", 0..3, Ok(a), 2),
        ])
        .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(
            got.iter()
                .map(|s| (s.path.as_str(), s.requested.start))
                .collect::<Vec<_>>(),
            [("a.py", 0), ("a.py", 3), ("z.py", 3)]
        );
        assert_eq!(got[0].text.as_deref(), Some("abc"));
        assert_eq!(got[1].text.as_deref(), Some("def"));
    }

    #[test]
    fn utf8_and_crlf_prefix_obeys_line_and_byte_caps() {
        let lines = (0..41).map(|n| format!("line{n}\r\n")).collect::<String>();
        let line_bytes = lines.as_bytes();
        let unicode = "é".repeat(2049);
        let got = assemble(vec![
            input("lines.py", "l", 0..line_bytes.len(), Ok(line_bytes), 0),
            input(
                "unicode.py",
                "u",
                0..unicode.len(),
                Ok(unicode.as_bytes()),
                1,
            ),
        ])
        .unwrap();
        let lines = got.iter().find(|s| s.path == "lines.py").unwrap();
        assert_eq!(lines.status, "truncated");
        assert_eq!(lines.reason, Some("snippet-cap"));
        assert_eq!(lines.text.as_deref(), Some("line0\r\nline1\r\nline2\r\nline3\r\nline4\r\nline5\r\nline6\r\nline7\r\nline8\r\nline9\r\nline10\r\nline11\r\nline12\r\nline13\r\nline14\r\nline15\r\nline16\r\nline17\r\nline18\r\nline19\r\nline20\r\nline21\r\nline22\r\nline23\r\nline24\r\nline25\r\nline26\r\nline27\r\nline28\r\nline29\r\nline30\r\nline31\r\nline32\r\nline33\r\nline34\r\nline35\r\nline36\r\nline37\r\nline38\r\nline39\r\n"));
        let unicode = got.iter().find(|s| s.path == "unicode.py").unwrap();
        assert_eq!(unicode.text.as_ref().unwrap().len(), 4096);
        assert_eq!(unicode.text.as_ref().unwrap(), &"é".repeat(2048));
        assert_eq!(unicode.returned.as_ref().unwrap().end, 4096);
    }

    #[test]
    fn invalid_ranges_are_unavailable_without_dropping_valid_anchor_snippets() {
        let source = "café".as_bytes();
        for range in [2..2, 0..6, 4..5] {
            let got = assemble(vec![
                input("a.py", "d", range, Ok(source), 0),
                input("a.py", "d", 0..2, Ok(source), 1),
            ])
            .unwrap();
            assert_eq!(got.len(), 2);
            let invalid = got.iter().find(|s| s.anchor_indices == [0]).unwrap();
            assert_eq!(invalid.status, "unavailable");
            assert_eq!(invalid.reason, Some("invalid-source-range"));
            assert_eq!(invalid.returned, None);
            assert_eq!(invalid.text, None);
            let valid = got.iter().find(|s| s.anchor_indices == [1]).unwrap();
            assert_eq!(valid.status, "available");
            assert_eq!(valid.text.as_deref(), Some("ca"));
        }
    }

    #[test]
    fn unavailable_source_retains_merged_requested_range_without_body() {
        let got = assemble(vec![
            input("a.py", "d", 2..5, Err("source-changed"), 1),
            input("a.py", "d", 4..8, Err("source-changed"), 0),
        ])
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].requested, ByteRange { start: 2, end: 8 });
        assert_eq!(got[0].returned, None);
        assert_eq!(got[0].text, None);
        assert_eq!(got[0].status, "unavailable");
        assert_eq!(got[0].reason, Some("source-changed"));
        assert_eq!(got[0].anchor_indices, [0, 1]);
    }
}
