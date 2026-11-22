// SPDX-License-Identifier: Apache-2.0
//! Markdown section and link parser for `riltra docs` (Stage 7, Task 1).
//!
//! Pure: reads nothing from disk. The rules are the Stage 7 plan's D2
//! (sections and links) and D3 (suggested record id).

use crate::store;

/// One section of a Markdown document: a heading and the bytes up to the
/// next heading of any level, or the end of the file.
#[allow(dead_code)] // Task 2 consumes this
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Section {
    pub section_id: String,
    pub heading: Option<String>,
    pub heading_path: Vec<String>,
    pub level: u8,
    pub line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    /// `store::content_digest` of the section's bytes, as `capture_evidence` computes it.
    pub digest: String,
    pub links: Vec<Link>,
}

/// One inline link found outside fences and code spans.
#[allow(dead_code)] // Task 2 consumes this
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub line: usize,
    pub text: String,
    pub target: String,
    pub kind: LinkKind,
}

/// What a link target points at. Only `Path` links are resolved further.
#[allow(dead_code)] // Task 2 consumes this
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkKind {
    External,
    SameDocument,
    /// The path leaves the repository root lexically.
    OutsideRepo,
    Path {
        path: String,
        fragment: Option<String>,
        line: Option<usize>,
        end_line: Option<usize>,
    },
}

/// Splits a document into sections and collects its inline links.
#[allow(dead_code)] // Task 2 consumes this
pub(crate) fn parse_document(doc_path: &str, text: &str) -> Vec<Section> {
    struct Head {
        line: usize,
        start: usize,
        level: u8,
        text: String,
    }
    struct Spec {
        line: usize,
        start: usize,
        level: u8,
        heading: Option<String>,
        heading_path: Vec<String>,
    }

    let mut heads: Vec<Head> = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    // (fence character, opening run length) while inside a fence.
    let mut fence: Option<(u8, usize)> = None;
    let mut offset = 0usize;
    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let line_start = offset;
        offset += raw.len();
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if let Some((ch, len)) = fence {
            if closes_fence(content, ch, len) {
                fence = None;
            }
            continue;
        }
        if let Some(opening) = opens_fence(content) {
            fence = Some(opening);
            continue;
        }
        if let Some((level, title)) = parse_heading(content) {
            heads.push(Head {
                line: line_no,
                start: line_start,
                level,
                text: title,
            });
        }
        scan_links(doc_path, content, line_no, &mut links);
    }

    let mut specs: Vec<Spec> = Vec::new();
    let first_start = heads.first().map_or(text.len(), |h| h.start);
    if !text[..first_start].trim().is_empty() {
        specs.push(Spec {
            line: 1,
            start: 0,
            level: 0,
            heading: None,
            heading_path: Vec::new(),
        });
    }
    let mut stack: Vec<(u8, String)> = Vec::new();
    for h in heads {
        while stack.last().is_some_and(|(level, _)| *level >= h.level) {
            stack.pop();
        }
        stack.push((h.level, h.text.clone()));
        specs.push(Spec {
            line: h.line,
            start: h.start,
            level: h.level,
            heading: Some(h.text),
            heading_path: stack.iter().map(|(_, t)| t.clone()).collect(),
        });
    }

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<Section> = Vec::with_capacity(specs.len());
    for (idx, spec) in specs.iter().enumerate() {
        let byte_end = specs.get(idx + 1).map_or(text.len(), |n| n.start);
        let next_line = specs.get(idx + 1).map_or(usize::MAX, |n| n.line);
        let base = if spec.level == 0 {
            "(preamble)".to_string()
        } else {
            spec.heading_path.join(" / ")
        };
        let k = counts.entry(base.clone()).or_insert(0);
        *k += 1;
        out.push(Section {
            section_id: format!("{base}@{k}"),
            heading: spec.heading.clone(),
            heading_path: spec.heading_path.clone(),
            level: spec.level,
            line: spec.line,
            byte_start: spec.start,
            byte_end,
            digest: store::content_digest(&text.as_bytes()[spec.start..byte_end]),
            links: links
                .iter()
                .filter(|l| l.line >= spec.line && l.line < next_line)
                .cloned()
                .collect(),
        });
    }
    out
}

/// The suggested knowledge record id for one section (D3).
#[allow(dead_code)] // Task 2 consumes this
pub(crate) fn suggested_record_id(
    doc_path: &str,
    heading: Option<&str>,
    section_id: &str,
) -> String {
    // Budget: "doc" (3) + "-" + 57 + "-" + 57 + "-" + 8 hex = 128 bytes at most.
    const SLUG_CAP: usize = 57;
    let file_start = doc_path.rfind('/').map_or(0, |slash| slash + 1);
    let stem = match doc_path[file_start..].rfind('.') {
        Some(dot) => &doc_path[..file_start + dot],
        None => doc_path,
    };
    let doc_slug = trim_cut(&slug(stem), SLUG_CAP);
    let heading_slug = trim_cut(&slug(heading.unwrap_or("preamble")), SLUG_CAP);
    let hash = store::content_digest(format!("{doc_path}\0{section_id}").as_bytes());
    let mut id = String::from("doc");
    if !doc_slug.is_empty() {
        id.push('-');
        id.push_str(&doc_slug);
    }
    if !heading_slug.is_empty() {
        id.push('-');
        id.push_str(&heading_slug);
    }
    id.push('-');
    id.push_str(&hash[..8]);
    id
}

/// Lowercase ASCII letters and digits are kept; every other byte run is one `-`.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b.to_ascii_lowercase() as char);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Cuts an ASCII slug to `max` bytes and drops a trailing `-` left by the cut.
fn trim_cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    s[..max].trim_end_matches('-').to_string()
}

/// Strips up to three spaces of indent; `None` for a deeper indent.
fn after_indent(s: &str) -> Option<&str> {
    let spaces = s.len() - s.trim_start_matches(' ').len();
    (spaces <= 3).then(|| &s[spaces..])
}

fn run_len(s: &str, ch: u8) -> usize {
    s.bytes().take_while(|b| *b == ch).count()
}

/// A fence opens on a run of three or more backticks or tildes.
fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let rest = after_indent(line)?;
    let ch = *rest.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let n = run_len(rest, ch);
    (n >= 3).then_some((ch, n))
}

/// A closing fence is a run of the same character, at least as long, then only whitespace.
fn closes_fence(line: &str, ch: u8, len: usize) -> bool {
    let Some(rest) = after_indent(line) else {
        return false;
    };
    let n = run_len(rest, ch);
    n >= len && rest[n..].chars().all(char::is_whitespace)
}

/// ATX headings only: 1 to 6 `#`, then a space, a tab or the end of the line.
fn parse_heading(line: &str) -> Option<(u8, String)> {
    let rest = after_indent(line)?;
    let hashes = run_len(rest, b'#');
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &rest[hashes..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let mut title = after.trim();
    let stripped = title.trim_end_matches('#');
    if stripped.len() < title.len()
        && (stripped.is_empty() || stripped.ends_with(' ') || stripped.ends_with('\t'))
    {
        title = stripped.trim_end();
    }
    Some((hashes as u8, title.to_string()))
}

/// Cuts `s` to at most `max` bytes on a char boundary.
fn cut_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Finds inline links on one non-fence line, skipping code spans and images.
fn scan_links(doc_path: &str, line: &str, line_no: usize, out: &mut Vec<Link>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let run = run_len(&line[i..], b'`');
                let mut j = i + run;
                let mut close = None;
                while j < bytes.len() {
                    if bytes[j] == b'`' {
                        let r = run_len(&line[j..], b'`');
                        if r == run {
                            close = Some(j + r);
                            break;
                        }
                        j += r;
                    } else {
                        j += 1;
                    }
                }
                i = close.unwrap_or(i + run);
            }
            b'[' if i == 0 || bytes[i - 1] != b'!' => {
                match parse_link_at(doc_path, line, i, line_no) {
                    Some((link, end)) => {
                        out.push(link);
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
}

/// Parses `[text](target)` starting at the `[` at byte `i`. Returns the link and the byte after it.
fn parse_link_at(doc_path: &str, line: &str, i: usize, line_no: usize) -> Option<(Link, usize)> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut j = i;
    let close = loop {
        if j >= bytes.len() {
            return None;
        }
        match bytes[j] {
            b'\\' => j += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let open = close + 2;
    let mut k = open;
    let mut parens = 0usize;
    let end_paren = loop {
        if k >= bytes.len() {
            return None;
        }
        match bytes[k] {
            b'(' => parens += 1,
            b')' => {
                if parens == 0 {
                    break k;
                }
                parens -= 1;
            }
            _ => {}
        }
        k += 1;
    };
    let inner = line[open..end_paren].trim_start_matches([' ', '\t']);
    let target = match inner.strip_prefix('<') {
        Some(angled) => &angled[..angled.find('>')?],
        None => inner.split([' ', '\t']).next().unwrap_or(""),
    };
    let kind = classify(doc_path, target)?;
    let link = Link {
        line: line_no,
        text: cut_bytes(&line[i + 1..close], 200),
        target: cut_bytes(target, 512),
        kind,
    };
    Some((link, end_paren + 1))
}

/// Classifies a link target. `None` means the target is not a reference (for example, empty).
fn classify(doc_path: &str, target: &str) -> Option<LinkKind> {
    if target.is_empty() {
        return None;
    }
    if target.starts_with('#') {
        return Some(LinkKind::SameDocument);
    }
    if has_scheme(target) {
        return Some(LinkKind::External);
    }
    let (path_part, frag) = target.split_once('#').unwrap_or((target, ""));
    let Some(path) = resolve(doc_path, path_part) else {
        return Some(LinkKind::OutsideRepo);
    };
    if path.is_empty() {
        return None;
    }
    let (line, end_line, fragment) = parse_fragment(frag);
    Some(LinkKind::Path {
        path,
        fragment,
        line,
        end_line,
    })
}

/// `[A-Za-z][A-Za-z0-9+.-]*:` before the first `/` or `#`.
fn has_scheme(target: &str) -> bool {
    let Some(colon) = target.find(':') else {
        return false;
    };
    let prefix = &target[..colon];
    if prefix.contains('/') || prefix.contains('#') {
        return false;
    }
    let mut chars = prefix.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// Resolves a path lexically. A leading `/` is repository-root relative; otherwise the
/// document's directory is the base. `None` when the path leaves the root.
fn resolve(doc_path: &str, path_part: &str) -> Option<String> {
    let mut stack: Vec<&str> = Vec::new();
    if !path_part.starts_with('/') {
        let dir = doc_path.rsplit_once('/').map_or("", |(d, _)| d);
        for seg in dir.split('/') {
            push_segment(&mut stack, seg)?;
        }
    }
    for seg in path_part.split('/') {
        push_segment(&mut stack, seg)?;
    }
    Some(stack.join("/"))
}

fn push_segment<'a>(stack: &mut Vec<&'a str>, seg: &'a str) -> Option<()> {
    match seg {
        "" | "." => Some(()),
        ".." => stack.pop().map(|_| ()),
        s => {
            stack.push(s);
            Some(())
        }
    }
}

/// `L<n>` or `L<n>-L<m>` with n >= 1 and m >= n. Any other fragment is kept as text.
fn parse_fragment(frag: &str) -> (Option<usize>, Option<usize>, Option<String>) {
    if frag.is_empty() {
        return (None, None, None);
    }
    match parse_lines(frag) {
        Some((n, m)) => (Some(n), m, None),
        None => (None, None, Some(frag.to_string())),
    }
}

fn parse_lines(frag: &str) -> Option<(usize, Option<usize>)> {
    let rest = frag.strip_prefix('L')?;
    let (first, second) = match rest.split_once("-L") {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    let n = parse_line_number(first)?;
    let m = match second {
        Some(b) => {
            let m = parse_line_number(b)?;
            if m < n {
                return None;
            }
            Some(m)
        }
        None => None,
    };
    Some((n, m))
}

fn parse_line_number(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<usize>().ok().filter(|&v| v >= 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge;

    fn path_kind(path: &str) -> LinkKind {
        LinkKind::Path {
            path: path.to_string(),
            fragment: None,
            line: None,
            end_line: None,
        }
    }

    #[test]
    fn sections_split_at_every_heading_level() {
        let text = "# A\nx\n## B\ny\n# C\n";
        let s = parse_document("docs/g.md", text);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].byte_start, s[0].byte_end, s[0].line), (0, 6, 1));
        assert_eq!((s[1].byte_start, s[1].byte_end, s[1].line), (6, 13, 3));
        assert_eq!(s[1].heading_path, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(s[1].level, 2);
        assert_eq!((s[2].byte_start, s[2].byte_end, s[2].line), (13, 17, 5));
        let ids: Vec<&str> = s.iter().map(|x| x.section_id.as_str()).collect();
        assert_eq!(ids, vec!["A@1", "A / B@1", "C@1"]);
    }

    #[test]
    fn preamble_only_when_not_blank() {
        let blank = parse_document("docs/g.md", "\n\n# A\n");
        assert_eq!(blank.len(), 1);
        assert_eq!(blank[0].section_id, "A@1");

        let intro = parse_document("docs/g.md", "intro\n# A\n");
        assert_eq!(intro.len(), 2);
        assert_eq!(intro[0].section_id, "(preamble)@1");
        assert_eq!(intro[0].level, 0);
        assert_eq!(intro[0].heading, None);
        assert_eq!(intro[0].line, 1);
    }

    #[test]
    fn section_ids_are_unique_for_duplicate_headings() {
        let s = parse_document("docs/g.md", "# A\n## Notes\nx\n## Notes\n");
        let ids: Vec<&str> = s.iter().map(|x| x.section_id.as_str()).collect();
        assert_eq!(ids, vec!["A@1", "A / Notes@1", "A / Notes@2"]);
    }

    #[test]
    fn parser_ignores_fenced_code_images_and_prose_names() {
        let text = r#"# Doc
```md
# not
[x](a.rs#L1)
```
~~~
# also not
[y](a.rs#L2)
~~~
Inline `[z](a.rs#L9)` here
![i](a.rs)
[r][1]
https://e.x
prose parse_input
[ok](src/a.rs#L3)
"#;
        let s = parse_document("README.md", text);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].links.len(), 1);
        match &s[0].links[0].kind {
            LinkKind::Path { path, line, .. } => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(*line, Some(3));
            }
            other => panic!("expected a path link, got {other:?}"),
        }
        assert!(s[0].links.iter().all(|l| !l.target.contains("e.x")));
    }

    #[test]
    fn link_targets_resolve_lexically() {
        let text = "# Links\n\
            [a](../../src/a.rs)\n\
            [b](/src/b.rs#L2-L5)\n\
            [c](../../../o.rs)\n\
            [d](#top)\n\
            [e](mailto:x@y)\n\
            [f](<src/c d.rs>)\n\
            [g](src/a.rs#intro)\n\
            [h](src/a.rs#L0)\n";
        let s = parse_document("docs/guide/x.md", text);
        assert_eq!(s.len(), 1);
        let l = &s[0].links;
        assert_eq!(l.len(), 8);
        assert_eq!(l[0].kind, path_kind("src/a.rs"));
        assert_eq!(
            l[1].kind,
            LinkKind::Path {
                path: "src/b.rs".to_string(),
                fragment: None,
                line: Some(2),
                end_line: Some(5),
            }
        );
        assert_eq!(l[1].target, "/src/b.rs#L2-L5");
        assert_eq!(l[2].kind, LinkKind::OutsideRepo);
        assert_eq!(l[3].kind, LinkKind::SameDocument);
        assert_eq!(l[4].kind, LinkKind::External);
        assert_eq!(l[5].target, "src/c d.rs");
        assert_eq!(l[5].kind, path_kind("docs/guide/src/c d.rs"));
        assert_eq!(
            l[6].kind,
            LinkKind::Path {
                path: "docs/guide/src/a.rs".to_string(),
                fragment: Some("intro".to_string()),
                line: None,
                end_line: None,
            }
        );
        assert_eq!(
            l[7].kind,
            LinkKind::Path {
                path: "docs/guide/src/a.rs".to_string(),
                fragment: Some("L0".to_string()),
                line: None,
                end_line: None,
            }
        );
    }

    #[test]
    fn crlf_spans_exclude_carriage_returns_from_text() {
        let text = "# A\r\nx\r\n## B\r\n";
        let s = parse_document("docs/g.md", text);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].heading.as_deref(), Some("A"));
        assert_eq!(s[0].byte_end, 8);
        assert_eq!(text.as_bytes()[8], b'#');
        assert_eq!(s[0].line, 1);
        assert_eq!(s[1].line, 3);
    }

    #[test]
    fn record_id_is_valid_for_non_ascii_heading() {
        let id = suggested_record_id("docs/adr/ä.md", Some("Überblick"), "Überblick@1");
        assert!(knowledge::validate_id(&id).is_ok(), "{id}");
        let long = "x".repeat(600);
        let id = suggested_record_id("docs/adr/a.md", Some(&long), &format!("{long}@1"));
        assert!(id.len() <= 128, "{}", id.len());
        assert!(knowledge::validate_id(&id).is_ok(), "{id}");
        let a = suggested_record_id("docs/a.md", Some("H"), "H@1");
        let b = suggested_record_id("docs/a.md", Some("H"), "H@2");
        assert_ne!(a, b);
    }

    #[test]
    fn record_id_hashes_full_identity_before_cutting() {
        let a = suggested_record_id("docs/a-b.md", Some("Storage"), "Storage@1");
        let b = suggested_record_id("docs/a_b.md", Some("Storage"), "Storage@1");
        assert_ne!(a, b);

        let p1 = format!("docs/{}x.md", "a".repeat(200));
        let p2 = format!("docs/{}y.md", "a".repeat(200));
        let i1 = suggested_record_id(&p1, Some("S"), "S@1");
        let i2 = suggested_record_id(&p2, Some("S"), "S@1");
        assert_ne!(i1, i2);
        for id in [&i1, &i2] {
            assert!(id.len() <= 128, "{}", id.len());
            assert!(knowledge::validate_id(id).is_ok(), "{id}");
            assert!(id[id.len() - 8..].chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}
