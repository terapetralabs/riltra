// SPDX-License-Identifier: Apache-2.0
//! Tier A contract corpus. Producer occurrences and expected relationships are
//! authored independently of tree-sitter/CLI output; no external indexer runs.
use super::*;
use serde_json::{json, Value};

fn number(v: &Value, key: &str) -> usize {
    v[key].as_u64().unwrap() as usize
}

fn raw_span(source: &str, occurrence: &Value) -> (usize, usize) {
    let line = number(occurrence, "line");
    let base: usize = source.split_inclusive('\n').take(line).map(str::len).sum();
    let span = (
        base + number(occurrence, "col"),
        base + number(occurrence, "end"),
    );
    assert_eq!(
        &source[span.0..span.1],
        occurrence["text"].as_str().unwrap()
    );
    span
}

fn query(h: &Path, command: &str, file: &str, anchor: &Value) -> Value {
    let line = (number(anchor, "line") + 1).to_string();
    let col = (number(anchor, "col") + 1).to_string();
    let (code, out, err) = run(
        h,
        &[
            command, "--repo", "sem", "--path", file, "--line", &line, "--col", &col,
        ],
    );
    assert_eq!(code, 0, "{command} {file}: {err}");
    let args = [
        command, "--repo", "sem", "--path", file, "--line", &line, "--col", &col,
    ];
    let (again, repeated, err) = run(h, &args);
    assert_eq!(again, 0, "repeat: {err}");
    assert_eq!(out, repeated, "raw JSON must repeat byte for byte");
    let value: Value = serde_json::from_str(&out).unwrap();
    fn artifacts(value: &Value, nonce: &str) {
        match value {
            Value::Object(fields) => {
                for (key, v) in fields {
                    if key == "artifact_id" {
                        assert!(!v.as_str().unwrap().is_empty());
                    }
                    if key == "source_revision" {
                        assert!(v.as_str().unwrap().contains(nonce));
                    }
                    artifacts(v, nonce);
                }
            }
            Value::Array(rows) => {
                for row in rows {
                    artifacts(row, nonce);
                }
            }
            _ => {}
        }
    }
    artifacts(&value, value["snapshot"]["nonce"].as_str().unwrap());
    normalized_edge_json(value)
}

fn check_evidence(value: &Value, source: &str, manifest: &Value, semantic: bool) {
    let symbols = manifest["symbols"].as_array().unwrap();
    let occurrences = manifest["occurrences"].as_array().unwrap();
    let mut digest = 0xcbf29ce484222325_u64;
    for b in source.bytes() {
        digest = (digest ^ b as u64).wrapping_mul(0x100000001b3);
    }
    fn edges(value: &Value) -> Vec<&Value> {
        if let Some(rows) = value["edges"].as_array() {
            return rows.iter().collect();
        }
        value["paths"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|p| p["edges"].as_array().unwrap())
            .collect()
    }
    for edge in edges(value) {
        let evidence = &edge["evidence"];
        assert_eq!(evidence["path"], manifest["file"]);
        assert_eq!(evidence["digest"], format!("{digest:016x}"));
        let span = evidence["span"].as_array().unwrap();
        let (a, b) = (
            span[0].as_u64().unwrap() as usize,
            span[1].as_u64().unwrap() as usize,
        );
        assert!(
            a < b && b <= source.len(),
            "invalid source evidence: {edge}"
        );
        assert_ne!(edge["verification"], "Current");
        if edge["kind"] != "CALLS" {
            continue;
        }
        assert!(semantic, "syntax must never prove CALLS");
        assert_eq!(edge["precision"], "Exact");
        assert_eq!(edge["caller"]["import_id"], 1);
        assert_eq!(edge["callee"]["import_id"], 1);
        assert_eq!(evidence["imports"][0]["provider"], "corpus");
        let terminal = evidence["terminal_span"].as_array().unwrap();
        let (ta, tb) = (
            terminal[0].as_u64().unwrap() as usize,
            terminal[1].as_u64().unwrap() as usize,
        );
        assert!(a <= ta && ta < tb && tb <= b);
        let uri = edge["callee"]["symbol_uri"].as_str().unwrap();
        let symbol = symbols
            .iter()
            .find(|s| s["uri"] == uri)
            .expect("callee must be fixture identity");
        assert!(
            occurrences
                .iter()
                .any(|o| o["id"] == symbol["id"] && o["definition"] == true),
            "callee definition proof"
        );
        assert!(
            occurrences.iter().any(|o| {
                !o["definition"].as_bool().unwrap() && o["id"] == symbol["id"] && {
                    let (ra, rb) = raw_span(source, o);
                    ra < tb && ta < rb && evidence["reference_span"] == json!([ra, rb])
                }
            }),
            "matching terminal producer proof: {edge}"
        );
        assert_eq!(&source[ta..tb], edge["callee_name"].as_str().unwrap());
        if uri.starts_with("local ") {
            assert_eq!(edge["callee"]["doc"], manifest["file"]);
        }
    }
    if let Some(verdict) = value.get("verdict") {
        assert!(matches!(
            verdict.as_str().unwrap(),
            "HIGH" | "MEDIUM" | "UNKNOWN"
        ));
        assert_eq!(value["completeness"]["status"], "partial");
    }
}

/// Whether one edge of `path` has a caller span in `file` covering bytes a..b.
fn covers(path: &Value, file: &Value, a: usize, b: usize) -> bool {
    path["edges"].as_array().unwrap().iter().any(|e| {
        let span = &e["caller_span"];
        e["caller_path"] == *file
            && span[0].as_u64().unwrap() as usize <= a
            && b <= span[1].as_u64().unwrap() as usize
    })
}

/// Syntax-only cases pinned to a documented outcome, not a coverage exemption.
/// In c/competing_targets the only same-name call is `obj->target()`, a call
/// through a struct member pointer. The syntax layer records no name-matched
/// edge for it, so the SYNTAX-ONLY `callers` and impact documents list no
/// candidate. The SCIP-anchored impact golden for this case may list one
/// candidate-name path for that call. The manifest is ambiguous (inbound_calls 0,
/// inbound_references 0), so the case asserts the syntax outcome, not a coverage.
const MEMBER_POINTER_CALLS_WITHOUT_NAME_CANDIDATES: &[&str] = &["c/competing_targets"];

/// Candidate truth from the manifest alone (never from a golden): a syntax-only
/// impact answer proves no path, every manifest call site is a candidate, and
/// extra candidates may come only from the decoy case kinds.
fn candidate_truth(
    lang: &str,
    case: &str,
    manifest: &Value,
    source: &str,
    v: &Value,
    (h, file, anchor): (&Path, &str, &Value),
) {
    if MEMBER_POINTER_CALLS_WITHOUT_NAME_CANDIDATES.contains(&format!("{lang}/{case}").as_str()) {
        let callers = query(h, "callers", file, anchor);
        assert!(
            callers["edges"].as_array().unwrap().is_empty(),
            "{lang}/{case}: callers must also be empty: {callers}"
        );
        assert!(
            v["paths"].as_array().unwrap().is_empty(),
            "{lang}/{case}: no candidate path: {v}"
        );
        assert_eq!(v["candidates"]["returned"], 0, "{lang}/{case}: {v}");
        return;
    }
    const DECOYS: [&str; 8] = [
        "same_name",
        "same_name_binding",
        "competing_targets",
        "dynamic_dispatch",
        "nested_anonymous",
        "unresolved",
        "unsupported",
        "impact_chain",
    ];
    let paths = v["paths"].as_array().unwrap();
    assert!(
        paths.iter().all(|p| p["basis"] != "proven"),
        "{lang}/{case}: syntax-only impact proved a path: {v}"
    );
    let candidates: Vec<&Value> = paths
        .iter()
        .filter(|p| p["basis"] == "candidate-name")
        .collect();
    let calls = number(manifest, "inbound_calls");
    assert!(candidates.len() >= calls, "{lang}/{case}: {v}");
    let target = manifest["target"].as_str().unwrap();
    let sites: Vec<&Value> = manifest["occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["id"] == target && o["definition"] == false)
        .collect();
    // Manifest fields read: occurrences (id, definition), target, inbound_calls,
    // inbound_references. The sites are the non-definition target occurrences.
    assert!(
        sites.len() >= calls,
        "{lang}/{case}: manifest sites < inbound_calls"
    );
    let references = number(manifest, "inbound_references");
    // Name matching finds calls, not value references: with references in the
    // manifest there is no inbound call to cover, and none may be listed.
    let call_sites: Vec<&Value> = if references > 0 {
        assert_eq!(
            calls, 0,
            "{lang}/{case}: references in the manifest but inbound calls"
        );
        Vec::new()
    } else if sites.len() == calls {
        // Every non-definition occurrence is an inbound call: each is covered.
        sites.clone()
    } else {
        // Decoy extras: the manifest does not say which occurrences are calls.
        Vec::new()
    };
    for site in call_sites {
        let (a, b) = raw_span(source, site);
        assert!(
            candidates
                .iter()
                .any(|p| covers(p, &manifest["file"], a, b)),
            "{lang}/{case}: call site {a}..{b} has no candidate: {v}"
        );
    }
    if !DECOYS.contains(&case) {
        if references == 0 {
            assert_eq!(
                sites.len(),
                calls,
                "{lang}/{case}: plain case sites must all be inbound calls"
            );
        }
        assert_eq!(
            candidates.len(),
            calls,
            "{lang}/{case}: extra candidate on a plain case: {v}"
        );
    }
}

/// Non-callable definition occurrences of the target name that appear in the
/// manifests and change the expected count. The manifests carry no kind field, so
/// each entry is read from the fixture source (`<lang>/<case>`). The table does not
/// claim to list every non-callable item in the fixtures: an item the manifests do
/// not list is irrelevant to the arithmetic. A reviewer checks each entry against
/// its fixture.
const NON_CALLABLE_DEFINITION_OCCURRENCES: &[(&str, usize)] = &[
    // Struct fields holding a function pointer, `struct A { int (*target)(void); }`.
    ("c/competing_targets", 2),
    ("c/same_name", 2),
    // A parameter named `target` inside the function `target`.
    ("c/same_name_binding", 1),
    ("cpp/same_name_binding", 1),
    ("py/same_name_binding", 1),
    ("rs/same_name_binding", 1),
    ("ts/same_name_binding", 1),
];

/// The `same_name` cases with no POTENTIAL_CALL edge named after the target, so the
/// `>= 2` check has nothing to bite there. `c/same_name` defines `target` only as
/// two struct fields holding a function pointer (`struct A { int (*target)(void); }`),
/// which the index does not count as a callable definition, and no call in the
/// case is named `target`. Every other `same_name` case must produce at least one
/// checked edge; `corpus` asserts both directions.
const SAME_NAME_WITHOUT_EDGE: &[&str] = &["c/same_name"];

/// Copies of the case document that `corpus` writes into the repository. The
/// `local_scope` case adds one mirror copy beside the file (see `files` in `corpus`).
fn written_copies(case: &str) -> usize {
    if case == "local_scope" {
        2
    } else {
        1
    }
}

/// Every POTENTIAL_CALL edge named `name` carries the number of function and
/// method definitions the repository holds with that name: the manifest's
/// definitions with that text, minus the table's non-callable occurrences, times
/// the written copies. Never read from a golden or CLI output.
fn check_candidate_labels(
    lang: &str,
    case: &str,
    manifest: &Value,
    name: &str,
    copies: usize,
    edges: &[&Value],
) -> usize {
    let defined = manifest["occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["definition"] == true && o["text"] == name)
        .count();
    let key = format!("{lang}/{case}");
    let non_callable = NON_CALLABLE_DEFINITION_OCCURRENCES
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(0, |(_, n)| *n);
    assert!(
        defined >= non_callable,
        "{key}: table exceeds the definitions"
    );
    let expected = (defined - non_callable) * copies;
    let mut checked = 0;
    for edge in edges {
        if edge["kind"] == "POTENTIAL_CALL" && edge["callee_name"] == name {
            assert_eq!(edge["candidates"], expected, "{key}: {edge}");
            assert!(edge["candidate_scope"].is_string(), "{key}: {edge}");
            if case == "same_name" {
                assert!(edge["candidates"].as_u64().unwrap() >= 2, "{key}: {edge}");
            }
            checked += 1;
        }
    }
    checked
}

fn corpus(lang: &str, case: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let fixture = root.join("fixtures/m6").join(lang);
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.join(format!("{case}.json"))).unwrap(),
    )
    .unwrap();
    let file = manifest["file"].as_str().unwrap();
    let target = manifest["target"].as_str().unwrap();
    let source = std::fs::read_to_string(fixture.join(file)).unwrap();
    let occurrences = manifest["occurrences"].as_array().unwrap();
    for o in occurrences {
        raw_span(&source, o);
    }
    let definition = |id| {
        occurrences
            .iter()
            .find(|o| o["id"] == id && o["definition"] == true)
            .expect("fixture anchor")
    };
    let h = home(&format!("corpus-{lang}-{case}"));
    let mirror = format!("other_{file}");
    // The same case rule that builds `files` below; the expected count reads it too.
    let copies = written_copies(case);
    let files = if case == "local_scope" {
        vec![(file, source.as_str()), (mirror.as_str(), source.as_str())]
    } else {
        vec![(file, source.as_str())]
    };
    let dir = repo_with(&h, "sem", &files);
    do_index(&h);
    let mut bundle = json!({});
    for (key, cmd, id) in [
        ("syntax_callees", "callees", "c"),
        ("syntax_impact", "impact", target),
    ] {
        let v = query(&h, cmd, file, definition(id));
        check_evidence(&v, &source, &manifest, false);
        if cmd == "impact" {
            assert_eq!(v["verdict"], "UNKNOWN");
            candidate_truth(
                lang,
                case,
                &manifest,
                &source,
                &v,
                (&h, file, definition(id)),
            );
        }
        bundle[key] = v;
    }
    let symbols = manifest["symbols"].as_array().unwrap();
    // Syntax-only callers, read before the import; not stored in the bundle.
    let syntax_callers = query(&h, "callers", file, definition(target));
    check_evidence(&syntax_callers, &source, &manifest, false);
    let target_name = symbols.iter().find(|s| s["id"] == target).unwrap()["name"]
        .as_str()
        .unwrap();
    let caller_edges: Vec<&Value> = syntax_callers["edges"].as_array().unwrap().iter().collect();
    let mut checked =
        check_candidate_labels(lang, case, &manifest, target_name, copies, &caller_edges);
    let impact_edges: Vec<&Value> = bundle["syntax_impact"]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["edges"].as_array().unwrap().iter())
        .collect();
    checked += check_candidate_labels(lang, case, &manifest, target_name, copies, &impact_edges);
    if case == "same_name" {
        let key = format!("{lang}/{case}");
        if SAME_NAME_WITHOUT_EDGE.contains(&key.as_str()) {
            assert_eq!(
                checked, 0,
                "{key}: an edge named target now exists; update SAME_NAME_WITHOUT_EDGE"
            );
        } else {
            assert!(
                checked > 0,
                "{key}: no POTENTIAL_CALL edge named target, so the >= 2 check is a no-op"
            );
        }
    }
    let occ: Vec<_> = occurrences
        .iter()
        .map(|o| {
            let symbol = symbols.iter().find(|s| s["id"] == o["id"]).unwrap();
            occurrence(
                symbol["uri"].as_str().unwrap(),
                u64::from(o["definition"].as_bool().unwrap()),
                number(o, "line") as i64,
                number(o, "col") as i64,
                number(o, "end") as i64,
            )
        })
        .collect();
    let info: Vec<_> = symbols
        .iter()
        .map(|s| (s["uri"].as_str().unwrap(), s["name"].as_str().unwrap()))
        .collect();
    let mut documents = vec![test_document(file, &occ, &info)];
    if case == "local_scope" {
        // Same local URI in two documents: only the document is identity
        // evidence. Global caller symbols belong to their own documents.
        let mirrored: Vec<_> = symbols
            .iter()
            .map(|s| {
                let uri = s["uri"].as_str().unwrap();
                (
                    s["id"].as_str().unwrap(),
                    if uri.starts_with("local ") {
                        uri.to_string()
                    } else {
                        uri.replace(file, &mirror)
                    },
                    s["name"].as_str().unwrap(),
                )
            })
            .collect();
        let other_occ: Vec<_> = occurrences
            .iter()
            .map(|o| {
                let (_, uri, _) = mirrored
                    .iter()
                    .find(|(id, _, _)| *id == o["id"].as_str().unwrap())
                    .unwrap();
                occurrence(
                    uri,
                    u64::from(o["definition"].as_bool().unwrap()),
                    number(o, "line") as i64,
                    number(o, "col") as i64,
                    number(o, "end") as i64,
                )
            })
            .collect();
        let other_info: Vec<_> = mirrored
            .iter()
            .map(|(_, uri, name)| (uri.as_str(), *name))
            .collect();
        documents.push(test_document(&mirror, &other_occ, &other_info));
    }
    import_documents(&h, &dir, "corpus", &documents);
    for (cmd, id) in [
        ("callers", target),
        ("callees", "c"),
        ("trace", "c"),
        ("impact", target),
    ] {
        let v = query(&h, cmd, file, definition(id));
        check_evidence(&v, &source, &manifest, true);
        assert_eq!(
            v["snapshot"], bundle["syntax_impact"]["snapshot"],
            "SCIP import must work without reindex"
        );
        if cmd == "callees" || cmd == "callers" {
            let rows = v["edges"].as_array().unwrap();
            let calls = rows.iter().filter(|e| e["kind"] == "CALLS").count();
            let expected = if cmd == "callees" {
                "callees_calls"
            } else {
                "inbound_calls"
            };
            assert_eq!(
                calls,
                number(&manifest, expected),
                "{lang}/{case} {cmd}: {v}"
            );
            if cmd == "callers" {
                assert_eq!(
                    rows.iter().filter(|e| e["kind"] == "REFERENCES").count(),
                    number(&manifest, "inbound_references")
                );
            }
        }
        if cmd == "impact" {
            assert_eq!(v["verdict"], manifest["impact"], "{lang}/{case}: {v}");
            if lang == "rs" && case == "dynamic_dispatch" {
                // A dynamic call by name is a candidate, never a proven path.
                let paths = v["paths"].as_array().unwrap();
                assert!(!paths.is_empty(), "{v}");
                assert!(paths.iter().all(|p| p["basis"] == "candidate-name"), "{v}");
            }
        }
        if cmd == "trace" && matches!(case, "recursion" | "cycle") {
            assert_eq!(v["cycle"], true);
        }
        bundle[cmd] = v;
    }
    if manifest["stale"] == true {
        let comment = if lang == "py" {
            "# edited\n"
        } else {
            "// edited\n"
        };
        std::fs::write(dir.join(file), format!("{source}{comment}")).unwrap();
        for (cmd, id) in [
            ("callers", "t"),
            ("callees", "c"),
            ("trace", "c"),
            ("impact", "t"),
        ] {
            let v = query(&h, cmd, file, definition(id));
            check_evidence(&v, &source, &manifest, true);
            assert_eq!(v["verification"], "Stale");
            if cmd == "impact" {
                assert_eq!(v["verdict"], "UNKNOWN");
            }
            bundle[format!("stale_{cmd}")] = v;
        }
    }
    let golden = root
        .join("golden/m6-corpus")
        .join(lang)
        .join(format!("{case}.json"));
    if std::env::var_os("RILTRA_UPDATE_M6_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        // Explicit maintainer action, after every independent truth assertion.
        std::fs::write(
            &golden,
            serde_json::to_string_pretty(&bundle).unwrap() + "\n",
        )
        .unwrap();
    }
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(golden).unwrap()).unwrap();
    assert!(bundle==expected,"{lang}/{case} full JSON golden differs (inspect the checked-in JSON and source-truth assertions)");
}

macro_rules! language {
    ($lang:ident) => {
        mod $lang {
            macro_rules! case {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        super::corpus(stringify!($lang), stringify!($name));
                    }
                };
            }
            case!(direct);
            case!(reference);
            case!(nested_calls);
            case!(local_scope);
            case!(same_name);
            case!(alias);
            case!(recursion);
            case!(cycle);
            case!(missing_definition);
            case!(dynamic_dispatch);
            case!(unresolved);
            case!(stale_after_edit);
            case!(unsupported);
            case!(competing_targets);
            case!(same_name_binding);
        }
    };
}
language!(py);
language!(ts);
language!(rs);
language!(c);
language!(cpp);

#[test]
fn rs_impact_scip_chain_depth_and_stale_match_source_oracle() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/m6/rs/impact_chain");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.with_extension("json")).unwrap())
            .unwrap();
    let file = manifest["file"].as_str().unwrap();
    let source = std::fs::read_to_string(fixture.with_extension("rs")).unwrap();
    let occurrences = manifest["occurrences"].as_array().unwrap();
    for item in occurrences {
        raw_span(&source, item);
    }

    let symbols = manifest["symbols"].as_array().unwrap();
    let occ: Vec<_> = occurrences
        .iter()
        .map(|item| {
            let symbol = symbols.iter().find(|s| s["id"] == item["id"]).unwrap();
            occurrence(
                symbol["uri"].as_str().unwrap(),
                u64::from(item["definition"].as_bool().unwrap()),
                number(item, "line") as i64,
                number(item, "col") as i64,
                number(item, "end") as i64,
            )
        })
        .collect();
    let info: Vec<_> = symbols
        .iter()
        .map(|s| (s["uri"].as_str().unwrap(), s["name"].as_str().unwrap()))
        .collect();
    let h = home("corpus-rs-impact-chain");
    let dir = repo_with(&h, "sem", &[(file, source.as_str())]);
    do_index(&h);
    import_documents(&h, &dir, "corpus", &[test_document(file, &occ, &info)]);

    let target = occurrences
        .iter()
        .find(|item| item["id"] == manifest["target"] && item["definition"] == true)
        .unwrap();
    let line = (number(target, "line") + 1).to_string();
    let col = (number(target, "col") + 1).to_string();
    let impact = |extra: &[&str]| {
        let mut args = vec![
            "impact", "--repo", "sem", "--path", file, "--line", &line, "--col", &col,
        ];
        args.extend_from_slice(extra);
        let (code, out, err) = run(&h, &args);
        assert_eq!(code, 0, "Rust impact oracle query: {err}");
        serde_json::from_str::<Value>(&out).unwrap()
    };
    let full = impact(&[]);
    assert_eq!(full["verdict"], manifest["expected"]["verdict"], "{full}");
    assert_eq!(full["paths"].as_array().unwrap().len(), 2, "{full}");
    for (path, expected) in full["paths"]
        .as_array()
        .unwrap()
        .iter()
        .zip(manifest["expected"]["paths"].as_array().unwrap())
    {
        assert_eq!(path["scope"], expected["scope"]);
        assert_eq!(path["distance"], expected["distance"]);
        assert!(path["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|edge| edge["kind"] == "CALLS" && edge["precision"] == "Exact"));
    }

    let bounded = impact(&["--depth", "1"]);
    assert_eq!(bounded["verdict"], "UNKNOWN", "{bounded}");
    assert_eq!(bounded["truncated"], true, "{bounded}");

    std::fs::write(dir.join(file), format!("{source}// edited\n")).unwrap();
    let stale = impact(&[]);
    assert_eq!(stale["verification"], "Stale", "{stale}");
    assert_eq!(stale["verdict"], "UNKNOWN", "{stale}");
}

#[test]
fn rs_impact_missing_provider_caller_and_unrelated_unsupported_file_are_unknown() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/m6/rs/impact_missing_caller");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.with_extension("json")).unwrap())
            .unwrap();
    let file = manifest["file"].as_str().unwrap();
    let source = std::fs::read_to_string(fixture.with_extension("rs")).unwrap();
    let oracle_occurrence = &manifest["occurrences"][0];
    raw_span(&source, oracle_occurrence);

    let h = home("corpus-rs-impact-missing-caller");
    let caller =
        std::fs::read_to_string(fixture.with_file_name("impact_missing_caller_unprovided.rs"))
            .unwrap();
    let dir = repo_with(
        &h,
        "sem",
        &[
            (file, source.as_str()),
            ("src/unprovided.rs", caller.as_str()),
        ],
    );
    std::fs::write(dir.join("notes.xyz"), "unrelated unsupported input").unwrap();
    do_index(&h);

    let symbol = manifest["symbols"][0]["uri"].as_str().unwrap();
    let name = manifest["symbols"][0]["name"].as_str().unwrap();
    let occ = occurrence(
        symbol,
        1,
        number(oracle_occurrence, "line") as i64,
        number(oracle_occurrence, "col") as i64,
        number(oracle_occurrence, "end") as i64,
    );
    import_documents(
        &h,
        &dir,
        "corpus",
        &[test_document(file, &[occ], &[(symbol, name)])],
    );

    let line = (number(oracle_occurrence, "line") + 1).to_string();
    let col = (number(oracle_occurrence, "col") + 1).to_string();
    let (code, out, err) = run(
        &h,
        &[
            "impact", "--repo", "sem", "--path", file, "--line", &line, "--col", &col,
        ],
    );
    assert_eq!(code, 0, "Rust impact with provider gap: {err}");
    let impact: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        impact["verdict"], manifest["expected"]["verdict"],
        "{impact}"
    );
    // The missing caller is listed as a candidate, never as a proven path.
    let paths = impact["paths"].as_array().unwrap();
    assert!(!paths.is_empty(), "missing caller is listed: {impact}");
    assert!(
        paths.iter().all(|p| p["basis"] == "candidate-name"),
        "missing caller is unknown, not safe: {impact}"
    );
    assert_eq!(
        impact["completeness"]["denominator"]["missing_provider_files"],
        manifest["expected"]["missing_provider_files"],
        "{impact}"
    );
    assert_eq!(
        impact["completeness"]["denominator"]["unsupported_files"],
        manifest["expected"]["unsupported_files"],
        "{impact}"
    );
}

#[test]
fn rs_impact_keeps_scip_path_when_another_caller_lacks_provider_coverage() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/m6/rs/impact_partial_provider");
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.join("case.json")).unwrap()).unwrap();
    let file = manifest["file"].as_str().unwrap();
    let covered_file = manifest["expected"]["path"]["file"].as_str().unwrap();
    let unprovided_file = manifest["files"][2].as_str().unwrap();
    let lib_source = std::fs::read_to_string(fixture.join(file)).unwrap();
    let covered_source = std::fs::read_to_string(fixture.join(covered_file)).unwrap();
    let unprovided_source = std::fs::read_to_string(fixture.join(unprovided_file)).unwrap();
    let occurrences = manifest["occurrences"].as_array().unwrap();
    for item in occurrences {
        let source = match item["file"].as_str().unwrap() {
            path if path == file => &lib_source,
            path if path == covered_file => &covered_source,
            path if path == unprovided_file => &unprovided_source,
            path => panic!("unexpected fixture file: {path}"),
        };
        raw_span(source, item);
    }

    let symbols = manifest["symbols"].as_array().unwrap();
    let target = &symbols[0];
    let caller = &symbols[1];
    let target_uri = target["uri"].as_str().unwrap();
    let caller_uri = caller["uri"].as_str().unwrap();
    let to_occurrence = |item: &Value| {
        let symbol = symbols.iter().find(|s| s["id"] == item["id"]).unwrap();
        occurrence(
            symbol["uri"].as_str().unwrap(),
            u64::from(item["definition"].as_bool().unwrap()),
            number(item, "line") as i64,
            number(item, "col") as i64,
            number(item, "end") as i64,
        )
    };
    let lib_occ = occurrences
        .iter()
        .filter(|item| item["file"] == file)
        .map(to_occurrence)
        .collect::<Vec<_>>();
    let covered_occ = occurrences
        .iter()
        .filter(|item| item["file"] == covered_file)
        .map(to_occurrence)
        .collect::<Vec<_>>();
    let h = home("corpus-rs-impact-partial-provider");
    let dir = repo_with(
        &h,
        "sem",
        &[
            (file, &lib_source),
            (covered_file, &covered_source),
            (unprovided_file, &unprovided_source),
        ],
    );
    do_index(&h);
    import_documents(
        &h,
        &dir,
        "corpus",
        &[
            test_document(file, &lib_occ, &[(target_uri, "target")]),
            test_document(
                covered_file,
                &covered_occ,
                &[(target_uri, "target"), (caller_uri, "covered")],
            ),
        ],
    );

    let anchor = occurrences
        .iter()
        .find(|item| item["file"] == file && item["id"] == target["id"])
        .unwrap();
    let line = (number(anchor, "line") + 1).to_string();
    let col = (number(anchor, "col") + 1).to_string();
    let (code, out, err) = run(
        &h,
        &[
            "impact", "--repo", "sem", "--path", file, "--line", &line, "--col", &col,
        ],
    );
    assert_eq!(code, 0, "Rust impact with partial provider coverage: {err}");
    let impact: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        impact["verdict"], manifest["expected"]["verdict"],
        "{impact}"
    );
    assert_eq!(impact["completeness"]["status"], "partial", "{impact}");
    assert_eq!(
        impact["completeness"]["denominator"]["missing_provider_files"],
        manifest["expected"]["missing_provider_files"],
        "{impact}"
    );
    assert_eq!(
        impact["completeness"]["denominator"]["unsupported_files"],
        manifest["expected"]["unsupported_files"],
        "{impact}"
    );
    // Candidates from the uncovered caller may follow the one proven path.
    let proven: Vec<&Value> = impact["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["basis"] == "proven")
        .collect();
    assert_eq!(proven.len(), 1, "{impact}");
    let path = proven[0];
    assert_eq!(path["scope"], manifest["expected"]["path"]["scope"]);
    assert_eq!(path["distance"], manifest["expected"]["path"]["distance"]);
    assert_eq!(path["edges"].as_array().unwrap().len(), 1, "{path}");
    let edge = &path["edges"][0];
    let reference = occurrences
        .iter()
        .find(|item| {
            item["file"] == covered_file
                && item["id"] == target["id"]
                && !item["definition"].as_bool().unwrap()
        })
        .unwrap();
    let reference_span = raw_span(&covered_source, reference);
    assert_eq!(edge["kind"], "CALLS", "{edge}");
    assert_eq!(edge["precision"], "Exact", "{edge}");
    assert_eq!(edge["verification"], "Unverified", "{edge}");
    assert_eq!(edge["unresolved"], Value::Null, "{edge}");
    assert_eq!(edge["provider"], "scip-import", "{edge}");
    assert_eq!(edge["caller"]["symbol_uri"], caller_uri, "{edge}");
    assert_eq!(edge["callee"]["symbol_uri"], target_uri, "{edge}");
    assert_eq!(edge["caller"]["import_id"], edge["callee"]["import_id"]);
    assert_eq!(edge["callee_name"], "target", "{edge}");
    let evidence = &edge["evidence"];
    let digest = covered_source
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, b| {
            (hash ^ b as u64).wrapping_mul(0x100000001b3)
        });
    assert_eq!(evidence["path"], covered_file, "{evidence}");
    assert_eq!(evidence["digest"], format!("{digest:016x}"), "{evidence}");
    assert_eq!(evidence["span"], edge["caller_span"]);
    assert_eq!(evidence["callee_span"], serde_json::json!(reference_span));
    assert_eq!(evidence["terminal_span"], serde_json::json!(reference_span));
    assert_eq!(
        evidence["reference_span"],
        serde_json::json!(reference_span)
    );
    assert_eq!(
        evidence["imports"].as_array().unwrap().len(),
        1,
        "{evidence}"
    );
    assert_eq!(evidence["imports"][0]["provider"], "corpus", "{evidence}");
}

#[test]
fn py_nested_anonymous() {
    corpus("py", "nested_anonymous");
}
#[test]
fn ts_nested_anonymous() {
    corpus("ts", "nested_anonymous");
}
#[test]
fn rs_nested_anonymous() {
    corpus("rs", "nested_anonymous");
}
#[test]
fn cpp_nested_anonymous() {
    corpus("cpp", "nested_anonymous");
}
