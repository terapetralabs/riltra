// SPDX-License-Identifier: Apache-2.0
//! `context` sections read from the stored syntax facts: containment, heritage,
//! imports and callees. Every expectation is written from the fixture source,
//! never from Riltra's own output.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn home(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("riltra-ctxfacts-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn run(home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_riltra"))
        .args(args)
        .env("RILTRA_HOME", home)
        .output()
        .expect("spawn riltra");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A repository named `name` holding `files`, initialised and indexed.
fn repo(h: &Path, name: &str, files: &[(&str, &str)]) {
    let dir = h.join(format!("{name}-src"));
    std::fs::create_dir_all(&dir).unwrap();
    for (rel, text) in files {
        std::fs::write(dir.join(rel), text).unwrap();
    }
    let (code, _o, err) = run(
        h,
        &["init", "--repo", &dir.to_string_lossy(), "--name", name],
    );
    assert_eq!(code, 0, "init: {err}");
    let (code, _o, err) = run(h, &["index", "--repo", name]);
    assert_eq!(code, 0, "index: {err}");
}

fn json_of(out: &str) -> Value {
    serde_json::from_str(out.trim_end()).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{out}"))
}

/// `context --repo name --target t [extra...]`, exit 0, parsed.
fn ctx(h: &Path, name: &str, target: &str, extra: &[&str]) -> Value {
    let mut args = vec!["context", "--repo", name, "--target", target];
    args.extend_from_slice(extra);
    let (code, out, err) = run(h, &args);
    assert_eq!(code, 0, "context {target}: {err}\n{out}");
    json_of(&out)
}

fn rows<'a>(section: &'a Value, list: &str) -> &'a Vec<Value> {
    section[list]["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("no rows in {list}: {section}"))
}

/// Rows as `(kind, name)` pairs, in order.
fn kind_names(section: &Value, list: &str) -> Vec<(String, Option<String>)> {
    rows(section, list)
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().to_string(),
                r["name"].as_str().map(str::to_string),
            )
        })
        .collect()
}

fn pair(kind: &str, name: &str) -> (String, Option<String>) {
    (kind.to_string(), Some(name.to_string()))
}

fn list_counts(section: &Value, list: &str) -> (u64, u64, u64, u64) {
    let l = &section[list];
    (
        l["total"].as_u64().unwrap(),
        l["returned"].as_u64().unwrap(),
        l["omitted_by_limit"].as_u64().unwrap(),
        l["omitted_by_budget"].as_u64().unwrap(),
    )
}

const PY_CLASSES: &str = "class A :\n    pass\n\n\nclass B(A):\n    def m(self):\n        return 1\n\n\ndef top():\n    return 2\n";

#[test]
fn python_heritage_and_containment_equal_the_source() {
    let h = home("py");
    repo(&h, "p", &[("a.py", PY_CLASSES)]);

    let b = ctx(&h, "p", "B", &[]);
    let her = &b["heritage"];
    assert_eq!(her["label"], "Heuristic");
    let sup = rows(her, "supertypes");
    assert_eq!(sup.len(), 1);
    assert_eq!(sup[0]["basis"], "declared-at-anchor");
    assert_eq!(sup[0]["relation"], "extends");
    assert_eq!(sup[0]["supertype_text"], "A");
    assert_eq!(sup[0]["supertype_name"], "A");
    assert!(rows(her, "subtypes").is_empty());
    assert_eq!(
        kind_names(&b["containment"], "members"),
        vec![pair("function", "m")]
    );
    // A top-level class has no recorded container.
    assert_eq!(list_counts(&b["containment"], "ancestors"), (0, 0, 0, 0));

    let a = ctx(&h, "p", "A", &[]);
    let subs = rows(&a["heritage"], "subtypes");
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["basis"], "name-match");
    assert_eq!(subs[0]["subtype"], "B");
    assert_eq!(subs[0]["path"], "a.py");
    assert_eq!(subs[0]["same_name_definitions"], 1);
    assert!(rows(&a["heritage"], "supertypes").is_empty());
}

#[test]
fn a_member_lists_its_class_as_ancestor_and_has_no_heritage() {
    let h = home("py-member");
    repo(&h, "p", &[("a.py", PY_CLASSES)]);
    let m = ctx(&h, "p", "m", &[]);
    assert_eq!(
        kind_names(&m["containment"], "ancestors"),
        vec![pair("class", "B")]
    );
    assert_eq!(m["containment"]["ancestors"]["total"], 1);
    assert_eq!(m["heritage"]["status"], "none-recorded");
    let top = ctx(&h, "p", "top", &[]);
    assert_eq!(top["containment"]["status"], "none-recorded");
    assert_eq!(top["containment"]["reason"], "syntax-facts-none-recorded");
}

#[test]
fn reverse_heritage_runs_only_for_a_type_anchor() {
    let h = home("same-name");
    repo(
        &h,
        "s",
        &[(
            "a.py",
            "class Foo :\n    pass\n\n\ndef Foo2():\n    return 1\n\n\nclass Sub(Foo):\n    pass\n",
        ), (
            "b.py",
            "def Foo():\n    return 1\n",
        )],
    );
    let class = ctx(&h, "s", "Foo", &["--kind", "class"]);
    assert_eq!(rows(&class["heritage"], "subtypes").len(), 1);
    // Two definitions are named Foo: the count says so, nothing is merged.
    assert_eq!(
        rows(&class["heritage"], "subtypes")[0]["same_name_definitions"],
        2
    );
    let func = ctx(&h, "s", "Foo", &["--kind", "function"]);
    assert!(rows(&func["heritage"], "subtypes").is_empty());
    assert!(rows(&func["heritage"], "implementations").is_empty());
    assert_eq!(func["heritage"]["reason"], "anchor-not-a-type");
}

#[test]
fn bases_that_are_not_names_have_no_lookup_name_and_never_match() {
    let h = home("no-name");
    repo(
        &h,
        "n",
        &[(
            "a.py",
            "class Bases :\n    pass\n\n\nclass C(bases[0]):\n    pass\n",
        )],
    );
    let c = ctx(&h, "n", "C", &[]);
    let sup = rows(&c["heritage"], "supertypes");
    assert_eq!(sup.len(), 1);
    assert_eq!(sup[0]["supertype_text"], "bases[0]");
    // An index whose content is a number is not a type argument list: the
    // base has text but no lookup name (a Python `bases[i]` with an identifier
    // index is indistinguishable from `Generic[T]` and keeps the name `bases`).
    assert!(sup[0]["supertype_name"].is_null());
    let bases = ctx(&h, "n", "Bases", &[]);
    assert!(rows(&bases["heritage"], "subtypes").is_empty());
}

#[test]
fn a_literal_base_has_text_but_no_lookup_name() {
    let h = home("null-base");
    repo(
        &h,
        "j",
        &[(
            "a.js",
            "class N extends null {
}

class M extends N {
}
",
        )],
    );
    let n = ctx(&h, "j", "N", &[]);
    let sup = &rows(&n["heritage"], "supertypes")[0];
    assert_eq!(sup["supertype_text"], "null");
    assert!(sup["supertype_name"].is_null());
    // The literal gives no name to look up; the real subclass is still listed.
    let subs = rows(&n["heritage"], "subtypes");
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["subtype"], "M");
}

#[test]
fn typescript_extends_implements_and_namespace_chain() {
    let h = home("ts");
    repo(
        &h,
        "t",
        &[(
            "a.ts",
            "interface I {
}

class Base {
}

namespace ns {
  export class D extends Base implements I {
    run() {}
  }
}
",
        )],
    );
    let d = ctx(&h, "t", "D", &[]);
    let sup: Vec<(String, String)> = rows(&d["heritage"], "supertypes")
        .iter()
        .map(|r| {
            (
                r["relation"].as_str().unwrap().to_string(),
                r["supertype_name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        sup,
        vec![
            ("extends".to_string(), "Base".to_string()),
            ("implements".to_string(), "I".to_string())
        ]
    );
    assert_eq!(
        kind_names(&d["containment"], "ancestors"),
        vec![pair("namespace", "ns")]
    );
    let run = ctx(&h, "t", "run", &[]);
    assert_eq!(
        kind_names(&run["containment"], "ancestors"),
        vec![pair("class", "D"), pair("namespace", "ns")]
    );
}

const RUST_SRC: &str = "trait Show {\n    fn show(&self);\n}\n\nmod outer {\n    mod inner {\n        struct Foo;\n\n        impl Foo {\n            fn m(&self) {}\n        }\n\n        impl super::super::Show for Foo {\n            fn show(&self) {}\n        }\n    }\n}\n";

#[test]
fn rust_impl_is_found_by_name_and_the_chain_keeps_stored_names() {
    let h = home("rust");
    repo(&h, "r", &[("lib.rs", RUST_SRC)]);

    let m = ctx(&h, "r", "m", &[]);
    assert_eq!(
        kind_names(&m["containment"], "ancestors"),
        vec![
            pair("impl", "Foo"),
            pair("module", "inner"),
            pair("module", "outer")
        ]
    );
    for row in rows(&m["containment"], "ancestors") {
        assert_eq!(row["symbol"], false);
    }
    assert_eq!(m["containment"]["ancestors"]["total"], 3);
    assert_eq!(m["heritage"]["status"], "none-recorded");

    let foo = ctx(&h, "r", "Foo", &["--kind", "struct"]);
    let imp = rows(&foo["heritage"], "implementations");
    assert_eq!(imp.len(), 1, "{}", foo["heritage"]);
    assert_eq!(imp[0]["basis"], "name-match");
    assert_eq!(imp[0]["relation"], "implements");
    assert_eq!(imp[0]["path"], "lib.rs");
    assert!(rows(&foo["heritage"], "supertypes").is_empty());
    assert_eq!(
        kind_names(&foo["containment"], "ancestors"),
        vec![pair("module", "inner"), pair("module", "outer")]
    );
}

#[test]
fn a_chain_of_more_than_sixteen_containers_reports_the_cut_honestly() {
    let h = home("deep");
    let mut src = String::new();
    for i in 0..18 {
        src.push_str(&format!("mod m{i} {{\n"));
    }
    src.push_str("fn deep() {}\n");
    for _ in 0..18 {
        src.push_str("}\n");
    }
    repo(&h, "d", &[("lib.rs", &src)]);
    let d = ctx(&h, "d", "deep", &[]);
    let anc = &d["containment"]["ancestors"];
    assert_eq!(anc["returned"], 16);
    assert!(anc["total"].is_null());
    assert_eq!(anc["truncated_depth"], true);
    assert_eq!(anc["omitted_by_limit"], 0);
    assert_eq!(anc["omitted_by_budget"], 0);
    assert_eq!(d["containment"]["status"], "truncated");
    let names: Vec<_> = kind_names(&d["containment"], "ancestors");
    assert_eq!(names[0], pair("module", "m17"));
    assert_eq!(names[15], pair("module", "m2"));
    // Exactly sixteen containers: the count is exact, nothing is cut.
    let h2 = home("deep16");
    let mut src = String::new();
    for i in 0..16 {
        src.push_str(&format!("mod m{i} {{\n"));
    }
    src.push_str("fn deep() {}\n");
    for _ in 0..16 {
        src.push_str("}\n");
    }
    repo(&h2, "d", &[("lib.rs", &src)]);
    let d = ctx(&h2, "d", "deep", &[]);
    let anc = &d["containment"]["ancestors"];
    assert_eq!(anc["total"], 16);
    assert_eq!(anc["truncated_depth"], false);
    assert_ne!(d["containment"]["status"], "truncated");
}

#[test]
fn cpp_namespace_and_class_chain() {
    let h = home("cpp");
    repo(
        &h,
        "c",
        &[(
            "a.cpp",
            "namespace ns {\nclass K {\npublic:\n    int get() { return 1; }\n};\n}\n",
        )],
    );
    let g = ctx(&h, "c", "get", &[]);
    assert_eq!(
        kind_names(&g["containment"], "ancestors"),
        vec![pair("class", "K"), pair("namespace", "ns")]
    );
    assert_eq!(rows(&g["containment"], "ancestors")[1]["symbol"], false);
}

#[test]
fn imports_list_both_directions_and_count_unresolved() {
    let h = home("imports");
    repo(
        &h,
        "i",
        &[
            (
                "x.py",
                "import os\nfrom y import f\n\n\ndef g():\n    return f()\n",
            ),
            ("y.py", "def f():\n    return 1\n"),
        ],
    );
    let g = ctx(&h, "i", "g", &[]);
    let imports = rows(&g["imports"], "imports");
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0]["path"], "y.py");
    assert_eq!(imports[0]["line"], 2);
    assert_eq!(g["imports"]["unresolved"], 1);
    assert!(rows(&g["imports"], "imported_by").is_empty());
    let f = ctx(&h, "i", "f", &[]);
    let by = rows(&f["imports"], "imported_by");
    assert_eq!(by.len(), 1);
    assert_eq!(by[0]["path"], "x.py");
    assert_eq!(f["imports"]["label"], "Syntax");
}

#[test]
fn callees_equal_the_callees_command() {
    let h = home("callees");
    repo(
        &h,
        "k",
        &[(
            "a.py",
            "def leaf():\n    return 1\n\n\ndef mid():\n    return leaf()\n\n\ndef top():\n    mid()\n    leaf()\n",
        )],
    );
    let c = ctx(&h, "k", "top", &[]);
    let (code, out, err) = run(&h, &["callees", "--repo", "k", "--target", "top"]);
    assert_eq!(code, 0, "{err}");
    let expected = json_of(&out)["edges"].clone();
    assert_eq!(c["callees"]["status"], "ok");
    assert_eq!(Value::Array(rows(&c["callees"], "edges").clone()), expected);
    assert_eq!(c["callees"]["edges"]["total"], 2);
    assert_eq!(c["callees"]["edges"]["omitted_by_limit"], 0);
    assert_eq!(c["callees"]["edges"]["omitted_by_budget"], 0);
    for edge in rows(&c["callees"], "edges") {
        assert_eq!(edge["kind"], "POTENTIAL_CALL");
        assert!(edge.get("candidates").is_some());
        assert!(edge.get("candidate_scope").is_some());
    }
}

#[test]
fn a_zero_in_a_graph_section_is_not_established_not_none() {
    let h = home("zero");
    repo(&h, "z", &[("a.py", "def lone():\n    return 1\n")]);
    let l = ctx(&h, "z", "lone", &[]);
    assert_eq!(l["callees"]["status"], "none-recorded");
    assert_eq!(l["callees"]["reason"], "empty-result-not-coverage-proof");
    assert_eq!(l["imports"]["reason"], "empty-result-not-coverage-proof");
}

#[test]
fn limit_applies_to_each_list_and_says_so() {
    let h = home("limit");
    repo(
        &h,
        "l",
        &[(
            "a.py",
            "class K :\n    def a(self):\n        pass\n\n    def b(self):\n        pass\n\n    def c(self):\n        pass\n",
        )],
    );
    let k = ctx(&h, "l", "K", &["--limit", "1"]);
    assert_eq!(list_counts(&k["containment"], "members"), (3, 1, 2, 0));
    assert_eq!(k["containment"]["status"], "truncated");
    assert_eq!(
        kind_names(&k["containment"], "members"),
        vec![pair("function", "a")]
    );
}

#[test]
fn the_byte_budget_cuts_new_rows_before_the_limit_does() {
    let h = home("budget");
    let mut src = String::from("class Big :\n");
    for i in 0..120 {
        src.push_str(&format!(
            "    def method_number_{i:03}_with_a_deliberately_long_name_for_padding(self):\n        pass\n\n"
        ));
    }
    repo(&h, "b", &[("a.py", &src)]);
    let (code, out, err) = run(
        &h,
        &[
            "context",
            "--repo",
            "b",
            "--target",
            "Big",
            "--limit",
            "100",
            "--max-bytes",
            "8192",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(
        out.trim_end().len() <= 8192,
        "{} bytes",
        out.trim_end().len()
    );
    let d = json_of(&out);
    let (total, returned, by_limit, by_budget) = list_counts(&d["containment"], "members");
    assert_eq!(total, 120);
    assert_eq!(by_limit, 20);
    assert!(by_budget > 0, "budget must cut first: {by_budget}");
    assert_eq!(returned + by_limit + by_budget, 120);
    assert_eq!(returned as usize, rows(&d["containment"], "members").len());
    assert_eq!(d["containment"]["status"], "truncated");
    assert_eq!(d["truncation"]["truncated"], true);
}

#[test]
fn a_long_supertype_text_is_capped() {
    let h = home("longtext");
    let long = "A".repeat(2000);
    let src = format!("class {long} :\n    pass\n\n\nclass B({long}):\n    pass\n");
    repo(&h, "t", &[("a.py", &src)]);
    let b = ctx(&h, "t", "B", &["--max-bytes", "16384"]);
    let sup = &rows(&b["heritage"], "supertypes")[0];
    assert_eq!(sup["supertype_text"].as_str().unwrap().len(), 512);
    assert_eq!(sup["text_truncated"], true);
}

#[test]
fn ambiguous_and_missing_answers_carry_no_sections() {
    let h = home("amb");
    repo(
        &h,
        "m",
        &[
            ("a.py", "def save(x):\n    return x\n"),
            ("b.py", "def save(x):\n    return x\n"),
        ],
    );
    let amb = ctx(&h, "m", "save", &[]);
    assert_eq!(amb["status"], "ambiguous");
    for key in ["containment", "heritage", "imports", "callees"] {
        assert!(amb.get(key).is_none(), "{key} on an ambiguous answer");
    }
    let none = ctx(&h, "m", "nosuchname", &[]);
    assert_eq!(none["status"], "not-found");
    assert!(none.get("containment").is_none());
}

#[test]
fn no_index_keeps_its_answer_and_has_no_sections() {
    let h = home("noindex");
    let dir = h.join("u-src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.py"), "def f():\n    return 1\n").unwrap();
    let (code, _o, err) = run(
        &h,
        &["init", "--repo", &dir.to_string_lossy(), "--name", "u"],
    );
    assert_eq!(code, 0, "{err}");
    let (_code, out, _err) = run(&h, &["context", "--repo", "u", "--target", "f"]);
    let d = json_of(&out);
    assert!(d.get("containment").is_none(), "{d}");
    assert!(d.get("heritage").is_none());
}

#[test]
fn context_bytes_are_stable_across_identical_calls() {
    let h = home("stable");
    repo(&h, "p", &[("a.py", PY_CLASSES)]);
    let a = run(&h, &["context", "--repo", "p", "--target", "B"]).1;
    let b = run(&h, &["context", "--repo", "p", "--target", "B"]).1;
    assert_eq!(a, b);
}

#[test]
fn the_budget_drops_lists_in_the_stated_order_at_every_size() {
    let h = home("order");
    let mut a =
        String::from("from c import helper\n\n\nclass Base :\n    pass\n\n\nclass Hub(Base):\n");
    for i in 0..80 {
        a.push_str(&format!(
            "    def method_number_{i:03}_with_a_deliberately_long_name_for_padding(self):\n        pass\n\n"
        ));
    }
    let mut b = String::from("from a import Hub\n\n\n");
    for i in 0..60 {
        b.push_str(&format!("class Sub{i:03}Padding(Hub):\n    pass\n\n\n"));
    }
    repo(
        &h,
        "o",
        &[
            ("a.py", &a),
            ("b.py", &b),
            (
                "c.py",
                "def helper():
    return 1
",
            ),
        ],
    );
    // Earlier in this list is cut first.
    let order = [
        ("imports", "imported_by"),
        ("imports", "imports"),
        ("heritage", "subtypes"),
        ("heritage", "implementations"),
        ("heritage", "supertypes"),
        ("containment", "members"),
        ("callees", "edges"),
    ];
    let mut saw_cut = false;
    for max in [8192usize, 8800, 9600, 10500, 12000, 16000, 24000, 131072] {
        let (code, out, err) = run(
            &h,
            &[
                "context",
                "--repo",
                "o",
                "--target",
                "Hub",
                "--limit",
                "100",
                "--max-bytes",
                &max.to_string(),
            ],
        );
        assert_eq!(code, 0, "{err}");
        assert!(
            out.trim_end().len() <= max,
            "{max}: {} bytes",
            out.trim_end().len()
        );
        let d = json_of(&out);
        let cut = |(sec, list): (&str, &str)| list_counts(&d[sec], list);
        for j in 1..order.len() {
            let (_, _, _, budget_later) = cut(order[j]);
            if budget_later == 0 {
                continue;
            }
            saw_cut = true;
            for earlier in &order[..j] {
                let (_, returned, _, _) = cut(*earlier);
                assert_eq!(
                    returned, 0,
                    "{max}: {earlier:?} must be empty before {:?} is cut",
                    order[j]
                );
            }
        }
        for (sec, list) in order {
            let (total, returned, by_limit, by_budget) = list_counts(&d[sec], list);
            assert_eq!(returned + by_limit + by_budget, total, "{max} {sec}.{list}");
        }
    }
    assert!(
        saw_cut,
        "the sweep never reached a cut after the first list"
    );
    let full = json_of(
        &run(
            &h,
            &[
                "context",
                "--repo",
                "o",
                "--target",
                "Hub",
                "--limit",
                "100",
                "--max-bytes",
                "131072",
            ],
        )
        .1,
    );
    // The fixture makes these lists non-empty, so the order check is not vacuous.
    for (sec, list) in [
        ("imports", "imports"),
        ("imports", "imported_by"),
        ("heritage", "supertypes"),
        ("heritage", "subtypes"),
        ("containment", "members"),
    ] {
        assert!(
            list_counts(&full[sec], list).0 > 0,
            "{sec}.{list} is empty in the fixture"
        );
    }
}

#[test]
fn a_c_include_is_never_matched_to_a_typescript_file() {
    let h = home("c-include");
    repo(
        &h,
        "c",
        &[
            (
                "main.c",
                "#include \"helper\"\nint target() { return 1; }\n",
            ),
            ("helper.ts", "export function helper() {}\n"),
        ],
    );
    let t = ctx(&h, "c", "target", &[]);
    assert_eq!(
        list_counts(&t["imports"], "imports").0,
        0,
        "{}",
        t["imports"]
    );
    assert_eq!(t["imports"]["unresolved"], 1);
    // The reverse direction: the TypeScript file is not imported by main.c.
    let helper = ctx(&h, "c", "helper", &[]);
    assert_eq!(list_counts(&helper["imports"], "imported_by").0, 0);
}
