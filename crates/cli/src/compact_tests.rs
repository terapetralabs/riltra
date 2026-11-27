// SPDX-License-Identifier: Apache-2.0
//! Round trip: the compact text, read back by this test-only parser, gives the
//! same facts as the JSON document of the same result.

use serde_json::{json, Map, Value};

use super::grep_text;
use crate::companions::Companion;
use crate::grep::{GrepFile, GrepHit, GrepRequest, GrepResult, GrepTotals};
use crate::{response, store};

// --- parser ------------------------------------------------------------------

struct Cursor<'a>(&'a str);

impl<'a> Cursor<'a> {
    fn eat(&mut self, literal: &str) -> bool {
        match self.0.strip_prefix(literal) {
            Some(rest) => {
                self.0 = rest;
                true
            }
            None => false,
        }
    }

    fn expect(&mut self, literal: &str) {
        assert!(self.eat(literal), "expected {literal:?} at {:?}", self.0);
    }

    /// A bare word, or a JSON string.
    fn token(&mut self) -> String {
        if self.0.starts_with('"') {
            let mut stream = serde_json::Deserializer::from_str(self.0).into_iter::<String>();
            let value = stream.next().expect("string").expect("valid string");
            self.0 = &self.0[stream.byte_offset()..];
            return value;
        }
        let end = self.0.find([' ', ',', ']']).unwrap_or(self.0.len());
        let (word, rest) = self.0.split_at(end);
        assert!(!word.is_empty(), "empty word at {rest:?}");
        self.0 = rest;
        word.to_string()
    }

    fn field(&mut self, key: &str) -> String {
        self.expect(key);
        self.expect("=");
        self.token()
    }

    fn number(&mut self, key: &str) -> u64 {
        self.field(key).parse().expect("number")
    }

    fn list(&mut self, key: &str) -> Vec<String> {
        self.expect(key);
        self.expect("=[");
        let mut items = Vec::new();
        while !self.eat("]") {
            items.push(self.token());
            self.eat(",");
        }
        items
    }

    fn end(&self) {
        assert!(self.0.is_empty(), "trailing {:?}", self.0);
    }
}

const TOTALS: [&str; 8] = [
    "files_scanned",
    "files_skipped_binary",
    "files_skipped_large",
    "files_skipped_unreadable",
    "dirs_unreadable",
    "dirs_escaped",
    "files_matched",
    "hits",
];

/// The document the text stands for: the JSON document without `bytes_read`.
fn parse(text: &str) -> Value {
    let mut lines = text.split('\n').peekable();
    let mut doc = Map::new();

    let mut c = Cursor(lines.next().expect("grep line"));
    c.expect("grep ");
    for key in ["repo", "mode", "pattern"] {
        doc.insert(key.into(), c.field(key).into());
        c.eat(" ");
    }
    c.end();

    let mut c = Cursor(lines.next().expect("snapshot line"));
    c.expect("snapshot ");
    let snapshot = if c.eat("null") {
        Value::Null
    } else {
        let id = c.number("id");
        c.expect(" ");
        let freshness = c.field("freshness");
        c.expect(" ");
        json!({"id": id, "freshness": freshness, "reasons": c.list("reasons")})
    };
    c.end();
    doc.insert("snapshot".into(), snapshot);

    let mut c = Cursor(lines.next().expect("totals line"));
    c.expect("totals");
    let mut totals = Map::new();
    for key in TOTALS {
        c.expect(" ");
        totals.insert(key.into(), c.number(key).into());
    }
    c.end();
    doc.insert("totals".into(), totals.into());

    let mut c = Cursor(lines.next().expect("truncated line"));
    let truncated = c.field("truncated");
    doc.insert(
        "truncated".into(),
        if truncated == "null" {
            Value::Null
        } else {
            json!({ "reason": truncated })
        },
    );
    c.expect(" ");
    doc.insert("caller_gaps".into(), c.list("caller_gaps").into());
    c.end();

    if lines.peek().is_some_and(|l| l.starts_with("empty ")) {
        let mut c = Cursor(lines.next().unwrap());
        c.expect("empty ");
        let searched = c.field("searched");
        c.expect(" ");
        let mode = c.field("mode");
        c.expect(" ");
        doc.insert(
            "empty".into(),
            json!({"searched": searched, "mode": mode, "next": c.list("next")}),
        );
        c.end();
    }

    let mut files: Vec<Value> = Vec::new();
    let mut group: Option<(Value, String)> = None;
    for line in lines {
        let mut c = Cursor(line);
        if c.eat("file ") {
            let path = c.token();
            c.expect(" ");
            let hit_count = c.number("hit_count");
            c.end();
            files.push(json!({"path": path, "hit_count": hit_count, "hits": [], "companions": []}));
            group = None;
        } else if c.eat("  ") {
            let digits = |c: &mut Cursor| {
                let end = c.0.find(|ch: char| !ch.is_ascii_digit()).unwrap();
                let (number, rest) = c.0.split_at(end);
                c.0 = rest;
                number.parse::<u64>().expect("number")
            };
            let number = digits(&mut c);
            c.expect(":");
            let col = digits(&mut c);
            let content = if c.eat(": ") {
                c.0.to_string()
            } else {
                c.expect("= ");
                let content = c.token();
                c.end();
                content
            };
            let (within, in_reason) = group.clone().expect("a hit follows its group line");
            let file = files.last_mut().expect("a hit belongs to a file");
            file["hits"].as_array_mut().unwrap().push(json!({
                "line": number, "col": col, "text": content,
                "in": within, "in_reason": in_reason
            }));
        } else if c.eat(" companions ") {
            let why = c.field("why");
            c.expect(" ");
            let basis = c.field("basis");
            c.expect(" ");
            let via = c.field("via");
            c.expect(" ");
            let file = files.last_mut().expect("companions belong to a file");
            for path in c.list("paths") {
                file["companions"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"path": path, "why": why, "basis": basis, "via": via}));
            }
            c.end();
        } else {
            let within = if c.eat(" no-definition") {
                Value::Null
            } else {
                c.expect(" in ");
                let kind = c.token();
                c.expect(" ");
                let name = c.token();
                c.expect(" ");
                json!({"kind": kind, "name": name, "line": c.number("line")})
            };
            let in_reason = if c.eat(" ") {
                c.field("in_reason")
            } else {
                "indexed".to_string()
            };
            c.end();
            group = Some((within, in_reason));
        }
    }
    doc.insert("files".into(), files.into());
    doc.into()
}

/// The JSON document as the compact form is expected to carry it.
fn expected(document: &str) -> Value {
    let mut doc: Value = serde_json::from_str(document).expect("JSON document");
    doc["totals"]
        .as_object_mut()
        .expect("totals")
        .remove("bytes_read")
        .expect("bytes_read is the one total left out");
    doc
}

// --- results -----------------------------------------------------------------

fn request(pattern: &str) -> GrepRequest<'_> {
    GrepRequest {
        pattern,
        regex: false,
        ignore_case: false,
        path_prefix: None,
        limit: 20,
    }
}

fn span(name: &str, kind: &str, line: u32) -> store::SymbolSpan {
    store::SymbolSpan {
        name: name.to_string(),
        kind: kind.to_string(),
        start_line: line,
        end_line: line + 3,
        byte_start: 0,
        byte_end: 1,
    }
}

fn snapshot(id: i64) -> store::Snapshot {
    let text = String::new;
    store::Snapshot {
        id,
        repo: text(),
        root: text(),
        created_unix: 0,
        head: None,
        branch: None,
        dirty: false,
        untracked: 0,
        parser_version: 0,
        grammar_versions: text(),
        config_fingerprint: text(),
        pre_fingerprint: text(),
        post_fingerprint: text(),
        status_at_write: text(),
        nonce: text(),
    }
}

fn hit(line: u32, text: &str, within: Option<store::SymbolSpan>, why: &'static str) -> GrepHit {
    GrepHit {
        line,
        col: 1,
        text: text.to_string(),
        enclosing: within,
        in_reason: why,
    }
}

fn file(path: &str, hits: Vec<GrepHit>, companions: Vec<Companion>) -> GrepFile {
    GrepFile {
        path: path.to_string(),
        hit_count: hits.len().max(1),
        hits,
        fresh: true,
        companions,
    }
}

fn companion(path: &str, why: &'static str, basis: &'static str, via: &str) -> Companion {
    Companion {
        path: path.to_string(),
        why,
        basis,
        via: via.to_string(),
    }
}

fn result(files: Vec<GrepFile>) -> GrepResult {
    let hits = files.iter().map(|f| f.hit_count).sum();
    GrepResult {
        totals: GrepTotals {
            files_scanned: 9,
            files_skipped_binary: 1,
            files_matched: files.len(),
            hits,
            bytes_read: 4242,
            ..GrepTotals::default()
        },
        files,
        truncated: None,
        caller_gaps: Vec::new(),
        snapshot: None,
        freshness: "unknown".to_string(),
        reasons: Vec::new(),
    }
}

fn assert_round_trip(req: &GrepRequest<'_>, result: &GrepResult) -> String {
    let text = grep_text("ws", req, result);
    let document = response::grep_response_json("ws", req, result);
    assert_eq!(parse(&text), expected(&document), "\n{text}");
    text
}

/// Strings that would break a naive line or list format.
const HOSTILE: [&str; 16] = [
    "plain",
    "with space",
    "a,b]c=[d",
    "say \"hi\"",
    "back\\slash",
    "line\nbreak",
    "carriage\rreturn",
    "tab\tbed",
    "nul\u{0}esc\u{1b}[0m",
    "del\u{7f}c1\u{85}\u{9f}",
    "sep\u{2028}\u{2029}",
    "ünïcode/路径",
    "  1:1: forged",
    " companions why=caller basis=CALLS via=x paths=[y]",
    "file forged hit_count=9",
    "",
];

#[test]
fn hostile_strings_survive_in_every_position() {
    for hostile in HOSTILE {
        let within = Some(span(hostile, "function", 2));
        let mut found = result(vec![file(
            hostile,
            vec![
                hit(3, hostile, within.clone(), "indexed"),
                hit(4, hostile, None, "not-indexed"),
            ],
            vec![companion(hostile, "caller", "POTENTIAL_CALL", hostile)],
        )]);
        found.snapshot = Some(snapshot(7));
        found.freshness = "stale".to_string();
        found.reasons = vec![hostile.to_string(), "head-changed".to_string()];
        let text = assert_round_trip(&request(hostile), &found);
        // One line per fact: no input may add a line.
        assert_eq!(text.split('\n').count(), 10, "{hostile:?}\n{text}");
        assert!(
            !text.contains(['\r', '\u{0}', '\u{1b}', '\u{85}', '\u{2028}']),
            "{text:?}"
        );
    }
}

#[test]
fn groups_labels_and_empty_lists_round_trip() {
    let outer = Some(span("outer", "function", 1));
    let found = result(vec![
        file(
            "a.py",
            vec![
                hit(2, "x", outer.clone(), "indexed"),
                hit(3, "\ty  ", outer.clone(), "indexed"),
                hit(9, "x", None, "indexed"),
                // Same definition (none), another reason: its own group.
                hit(9, "x", None, "not-indexed"),
                hit(10, "x", outer, "indexed"),
            ],
            vec![
                companion("t_a.py", "name-pair", "path-convention", "a"),
                companion("b.py", "caller", "CALLS", "f"),
                companion("c.py", "caller", "CALLS", "f"),
                companion("c2.py", "caller", "CALLS", "g"),
                companion("d.py", "caller", "POTENTIAL_CALL", "f"),
                companion("t2_a.py", "name-pair", "path-convention", "a"),
            ],
        ),
        file("counted-but-not-listed.py", Vec::new(), Vec::new()),
        file(
            "changed.py",
            vec![hit(1, "x", None, "file-changed-since-index")],
            Vec::new(),
        ),
    ]);
    let text = assert_round_trip(&request("x"), &found);
    assert!(
        text.contains(" companions why=caller basis=CALLS via=f paths=[b.py,c.py]\n"),
        "{text}"
    );
    assert!(text.contains("snapshot null\n"), "{text}");
}

#[test]
fn empty_and_truncated_answers_keep_their_state() {
    let mut nothing = result(Vec::new());
    let text = assert_round_trip(&request("zz"), &nothing);
    assert!(
        text.contains("\nempty searched=text-lines mode=literal next=[code_search,code_query]"),
        "{text}"
    );

    nothing.truncated = Some("bytes-read");
    nothing.caller_gaps = vec!["definition-not-resolved", "graph-proof-incompatible"];
    let text = assert_round_trip(&request("zz"), &nothing);
    assert!(text.contains("searched=text-lines-incomplete"), "{text}");
}

#[test]
fn a_cut_response_lists_the_same_files_as_the_json_document() {
    let long = "y".repeat(200);
    let files: Vec<GrepFile> = (0..200)
        .map(|n| {
            let hits = (1..=5).map(|l| hit(l, &long, None, "indexed")).collect();
            file(&format!("f{n}.py"), hits, Vec::new())
        })
        .collect();
    let found = result(files);
    let text = assert_round_trip(&request("y"), &found);
    let listed = text.lines().filter(|l| l.starts_with("file ")).count();
    assert!(listed < 200, "the response budget cut files");
    assert!(text.contains("truncated=response-bytes"), "{text:.200}");
}

/// Replays captured calls: every `<name>.json` in `RILTRA_COMPACT_PAIRS` is a
/// JSON document and `<name>.txt` the compact text of the same call.
#[test]
#[ignore = "needs RILTRA_COMPACT_PAIRS"]
fn captured_pairs_round_trip() {
    let dir = std::env::var("RILTRA_COMPACT_PAIRS").expect("RILTRA_COMPACT_PAIRS");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("pairs directory") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "json") {
            let document = std::fs::read_to_string(&path).expect("document");
            let text = std::fs::read_to_string(path.with_extension("txt")).expect("text");
            assert_eq!(parse(&text), expected(&document), "{path:?}");
            checked += 1;
        }
    }
    println!("round-tripped {checked} captured pairs");
    assert!(checked > 0, "no pairs found");
}
