// SPDX-License-Identifier: Apache-2.0
//! Old-v4 graph shapes reproduced with the actual pre-Task7 binary are
//! injected here so CI never needs an old compiler checkout or executable.
use super::*;
use rusqlite::{params, Connection};
use serde_json::Value;

fn n(v: &Value, key: &str) -> usize {
    v[key].as_u64().unwrap() as usize
}
fn uri(m: &Value, id: &str) -> String {
    m["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap()["uri"]
        .as_str()
        .unwrap()
        .to_string()
}
fn definition<'a>(m: &'a Value, id: &str) -> &'a Value {
    m["occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == id && o["definition"] == true)
        .unwrap()
}
fn db(h: &Path) -> Connection {
    Connection::open(h.join("index/sem.db")).unwrap()
}
fn nonce(h: &Path) -> String {
    db(h)
        .query_row(
            "SELECT nonce FROM snapshots ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn source_state(h: &Path) -> Vec<String> {
    let conn = db(h);
    let mut state = Vec::new();
    for table in ["snapshots", "files"] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
            .unwrap();
        let columns = stmt.column_count();
        let rows = stmt
            .query_map([], |row| {
                Ok((0..columns)
                    .map(|i| format!("{table}:{i}:{:?}", row.get_ref(i).unwrap()))
                    .collect::<Vec<_>>())
            })
            .unwrap();
        for row in rows {
            state.extend(row.unwrap());
        }
    }
    state
}

fn setup(names: &[&str]) -> (PathBuf, PathBuf, Vec<Value>) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/m6/py");
    let manifests: Vec<Value> = names
        .iter()
        .map(|case| {
            serde_json::from_str(
                &std::fs::read_to_string(fixture.join(format!("{case}.json"))).unwrap(),
            )
            .unwrap()
        })
        .collect();
    let sources: Vec<String> = manifests
        .iter()
        .map(|m| std::fs::read_to_string(fixture.join(m["file"].as_str().unwrap())).unwrap())
        .collect();
    let files: Vec<_> = manifests
        .iter()
        .zip(&sources)
        .map(|(m, s)| (m["file"].as_str().unwrap(), s.as_str()))
        .collect();
    let h = home("proof-upgrade");
    let repo = repo_with(&h, "sem", &files);
    do_index(&h);
    for (i, m) in manifests.iter().enumerate() {
        let occ: Vec<_> = m["occurrences"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                occurrence(
                    &uri(m, o["id"].as_str().unwrap()),
                    u64::from(o["definition"].as_bool().unwrap()),
                    n(o, "line") as i64,
                    n(o, "col") as i64,
                    n(o, "end") as i64,
                )
            })
            .collect();
        let info: Vec<_> = m["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["uri"].as_str().unwrap(), s["name"].as_str().unwrap()))
            .collect();
        import_documents(
            &h,
            &repo,
            &format!("upgrade-{i}"),
            &[test_document(m["file"].as_str().unwrap(), &occ, &info)],
        );
        if names[i] == "direct" {
            continue;
        }
        let id = if names[i] == "nested_anonymous" {
            "t"
        } else {
            "a"
        };
        let reference = m["occurrences"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|o| o["id"] == id && o["definition"] == false)
            .unwrap();
        let base: usize = sources[i]
            .split_inclusive('\n')
            .take(n(reference, "line"))
            .map(str::len)
            .sum();
        let (start, end) = (base + n(reference, "col"), base + n(reference, "end"));
        let conn = db(&h);
        let sql="UPDATE code_edges SET edge_kind='CALLS',precision='Exact',callee_kind='scip',callee_import_id=?1,callee_uri=?2,callee_doc=NULL,callee_path=NULL,callee_bs=NULL,callee_be=NULL,unresolved=NULL,evidence=json_set(evidence,'$.ref',json_array(?3,?4)) WHERE caller_file=?5 AND caller_sbs=?3 AND edge_kind='POTENTIAL_CALL'";
        assert_eq!(
            conn.execute(
                sql,
                params![
                    (i + 1) as i64,
                    uri(m, id),
                    start as i64,
                    end as i64,
                    m["file"].as_str().unwrap()
                ]
            )
            .unwrap(),
            1
        );
        if names[i] != "same_name_binding" {
            conn.execute("UPDATE code_edges SET caller_kind='scip',caller_import_id=?1,caller_uri=?2,caller_doc=NULL,caller_path=NULL,caller_bs=NULL,caller_be=NULL WHERE caller_file=?3 AND caller_sbs=?4 AND edge_kind='CALLS'",params![(i+1) as i64,uri(m,"c"),m["file"].as_str().unwrap(),start as i64]).unwrap();
        }
    }
    db(&h)
        .execute("DELETE FROM meta WHERE key='graph_proof'", [])
        .unwrap();
    (h, repo, manifests)
}

fn query(h: &Path, m: &Value, case: &str, cmd: &str) -> (i32, String, String) {
    let bad = if matches!(case, "alias" | "same_name_binding") {
        "a"
    } else {
        "t"
    };
    let anchor = if matches!(cmd, "callers" | "impact") {
        definition(m, bad)
    } else {
        definition(m, "c")
    };
    let (line, col) = if case == "same_name_binding" && matches!(cmd, "callees" | "trace") {
        let r = m["occurrences"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|o| o["id"] == "a" && o["definition"] == false)
            .unwrap();
        (n(r, "line") + 1, 1)
    } else {
        (n(anchor, "line") + 1, n(anchor, "col") + 1)
    };
    run(
        h,
        &[
            cmd,
            "--repo",
            "sem",
            "--path",
            m["file"].as_str().unwrap(),
            "--line",
            &line.to_string(),
            "--col",
            &col.to_string(),
        ],
    )
}

fn blocked(h: &Path, m: &Value, case: &str) {
    for cmd in ["callers", "callees", "trace", "impact"] {
        let (code, out, err) = query(h, m, case, cmd);
        assert_eq!(
            code, 3,
            "old graph must fail closed: {case} {cmd}: {out} {err}"
        );
        assert!(out.is_empty());
        assert!(err.contains("graph-proof-incompatible"), "{err}");
    }
}
fn restored(h: &Path, m: &Value, case: &str) {
    for cmd in ["callers", "callees", "trace", "impact"] {
        let (code, out, err) = query(h, m, case, cmd);
        assert_eq!(code, 0, "{cmd}: {err}");
        let v: Value = serde_json::from_str(&out).unwrap();
        let edges: Vec<&Value> = if let Some(rows) = v["edges"].as_array() {
            rows.iter().collect()
        } else {
            v["paths"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|p| p["edges"].as_array().unwrap())
                .collect()
        };
        if case == "direct" {
            assert!(
                edges.iter().any(|e| e["kind"] == "CALLS"),
                "valid proof must recover"
            );
        } else {
            assert!(
                !edges.iter().any(|e| e["kind"] == "CALLS"),
                "false proof survived: {v}"
            );
        }
    }
}

macro_rules! upgrade {
    ($name:ident) => {
        #[test]
        fn $name() {
            let case = stringify!($name);
            let (h, _, m) = setup(&[case]);
            blocked(&h, &m[0], case);
        }
    };
}
upgrade!(alias);
upgrade!(same_name_binding);
upgrade!(nested_anonymous);

#[test]
fn reimport_revalidates_every_import_without_changing_snapshot() {
    let (h, _, m) = setup(&["alias", "nested_anonymous", "direct"]);
    let before = nonce(&h);
    let before_source = source_state(&h);
    blocked(&h, &m[0], "alias");
    let (code, _, _) = run(
        &h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &h.join("absent.scip").to_string_lossy(),
        ],
    );
    assert_eq!(code, 3);
    blocked(&h, &m[1], "nested_anonymous");
    let (code, out, err) = run(
        &h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &h.join("upgrade-0.scip").to_string_lossy(),
        ],
    );
    assert_eq!(code, 0, "{out} {err}");
    assert_eq!(nonce(&h), before);
    assert_eq!(
        source_state(&h),
        before_source,
        "reimport must not rewrite indexed source identity"
    );
    // Identical bytes on the same snapshot revalidate the existing import
    // instead of appending a duplicate (owner decision, F2 option a). The
    // re-derived graph answers for alias as it does for the other cases.
    restored(&h, &m[0], "alias");
    restored(&h, &m[1], "nested_anonymous");
    restored(&h, &m[2], "direct");
    assert_eq!(db(&h).query_row("SELECT COUNT(*) FROM code_edges WHERE edge_kind='CALLS' AND callee_uri LIKE '%/a().'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(
        db(&h)
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        6
    );
}

#[test]
fn full_index_revalidates_old_graph_with_new_snapshot() {
    let (h, _, m) = setup(&["alias", "nested_anonymous", "direct"]);
    let before = nonce(&h);
    let (code, out, err) = run(&h, &["index", "--repo", "sem", "--full"]);
    assert_eq!(code, 0, "{out} {err}");
    assert_ne!(nonce(&h), before);
    for (case, m) in ["alias", "nested_anonymous", "direct"].iter().zip(&m) {
        restored(&h, m, case);
    }
}

#[test]
fn stale_or_future_marker_and_old_writer_generation_fail_closed() {
    let (h, _, m) = setup(&["direct"]);
    let (code, out, err) = run(&h, &["index", "--repo", "sem", "--full"]);
    assert_eq!(code, 0, "{out} {err}");
    restored(&h, &m[0], "direct");
    let conn = db(&h);
    let token: String = conn
        .query_row("SELECT value FROM meta WHERE key='graph_proof'", [], |r| {
            r.get(0)
        })
        .unwrap();
    for wrong in ["future-proof-version", "malformed", "v1:wrong-snapshot:0"] {
        conn.execute("UPDATE meta SET value=?1 WHERE key='graph_proof'", [wrong])
            .unwrap();
        blocked(&h, &m[0], "direct");
    }
    conn.execute("UPDATE meta SET value=?1 WHERE key='graph_proof'", [token])
        .unwrap();
    conn.execute(
        "UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE key='semantic_generation'",
        [],
    )
    .unwrap();
    blocked(&h, &m[0], "direct");
}

#[test]
fn failed_revalidation_rolls_back_import_rows_generation_and_marker() {
    let (h, _, m) = setup(&["alias"]);
    let before_nonce = nonce(&h);
    let before:(i64,i64,String)=db(&h).query_row("SELECT (SELECT COUNT(*) FROM semantic_imports),(SELECT COUNT(*) FROM code_edges),(SELECT value FROM meta WHERE key='semantic_generation')",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    db(&h).execute_batch("CREATE TRIGGER reject_proof BEFORE INSERT ON meta WHEN NEW.key='graph_proof' BEGIN SELECT RAISE(ABORT,'injected proof write failure'); END;").unwrap();
    let (code, _, err) = run(
        &h,
        &[
            "semantic",
            "import",
            "--repo",
            "sem",
            "--scip",
            &h.join("upgrade-0.scip").to_string_lossy(),
        ],
    );
    assert_eq!(code, 3);
    assert!(err.contains("injected proof write failure"), "{err}");
    assert_eq!(nonce(&h), before_nonce);
    let after:(i64,i64,String)=db(&h).query_row("SELECT (SELECT COUNT(*) FROM semantic_imports),(SELECT COUNT(*) FROM code_edges),(SELECT value FROM meta WHERE key='semantic_generation')",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(after, before);
    blocked(&h, &m[0], "alias");
}

#[test]
fn failed_promotion_does_not_advance_snapshot_or_proof() {
    let (h, _, m) = setup(&["direct"]);
    let (code, _, err) = run(&h, &["index", "--repo", "sem", "--full"]);
    assert_eq!(code, 0, "{err}");
    let before_nonce = nonce(&h);
    let before: String = db(&h)
        .query_row("SELECT value FROM meta WHERE key='graph_proof'", [], |r| {
            r.get(0)
        })
        .unwrap();
    db(&h).execute_batch("CREATE TRIGGER reject_promotion_proof BEFORE INSERT ON meta WHEN NEW.key='graph_proof' BEGIN SELECT RAISE(ABORT,'injected promotion proof failure'); END;").unwrap();
    let (code, _, err) = run(&h, &["index", "--repo", "sem", "--full"]);
    assert_eq!(code, 3);
    assert!(err.contains("injected promotion proof failure"), "{err}");
    assert_eq!(nonce(&h), before_nonce);
    assert_eq!(
        db(&h)
            .query_row("SELECT value FROM meta WHERE key='graph_proof'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        before
    );
    restored(&h, &m[0], "direct");
}
