// SPDX-License-Identifier: Apache-2.0
//! Which symbol URIs of one SCIP import name more than one definition.
//!
//! The importer records the global ones in `meta`, counted over the whole
//! artifact, because a later reindex drops the facts of changed files and the
//! stored facts alone could make an ambiguous symbol look unique again. This
//! evidence only lowers a verdict: a URI that is not listed still needs every
//! other proof.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::store::{self, SemanticFact, SemanticImport};

/// `meta` key prefix; the rest of the key is the artifact digest.
pub const KEY_PREFIX: &str = "semantic_identity:";
const VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    Unique,
    Ambiguous(usize),
    Unchecked,
}

impl Identity {
    /// The `unresolved` reason of an edge; `None` when nothing speaks against it.
    pub fn reason(self) -> Option<String> {
        match self {
            Identity::Unique => None,
            Identity::Ambiguous(n) => Some(format!("ambiguous:{n}")),
            Identity::Unchecked => Some("identity-unchecked".to_string()),
        }
    }

    /// Why a query cannot answer for `uri`, in user words.
    pub fn refusal(self, uri: &str) -> Option<String> {
        match self {
            Identity::Unique => None,
            Identity::Ambiguous(n) => Some(format!("identity {uri} names {n} definitions")),
            Identity::Unchecked => Some(format!(
                "identity-unchecked: {uri}; run riltra semantic import again"
            )),
        }
    }
}

/// The validated duplicate list of one import; `None` is identity-unchecked.
pub struct ImportIdentity(Option<BTreeMap<String, usize>>);

impl ImportIdentity {
    /// A missing, unreadable, other-version or other-artifact value is
    /// unchecked, never an empty list.
    pub fn decode(value: Option<&str>, artifact_id: &str) -> Self {
        let list = || {
            let json: serde_json::Value = serde_json::from_str(value?).ok()?;
            if artifact_id.is_empty()
                || json.get("version")?.as_u64()? != VERSION
                || json.get("artifact")?.as_str()? != artifact_id
            {
                return None;
            }
            json.get("ambiguous")?
                .as_object()?
                .iter()
                .map(|(uri, n)| Some((uri.clone(), usize::try_from(n.as_u64()?).ok()?)))
                .collect()
        };
        ImportIdentity(list())
    }

    pub fn load(conn: &Connection, import: &SemanticImport) -> Result<Self, String> {
        let value = store::meta_get(conn, &format!("{KEY_PREFIX}{}", import.artifact_id))?;
        Ok(Self::decode(value.as_deref(), &import.artifact_id))
    }

    /// `places` is the number of distinct definition spans the stored facts
    /// show in the queried document: the only count a local symbol has, and a
    /// floor for a global one.
    pub fn of(&self, uri: &str, places: usize) -> Identity {
        let Some(list) = &self.0 else {
            return Identity::Unchecked;
        };
        let recorded = list.get(uri).copied().unwrap_or(0).max(places);
        if recorded > 1 {
            Identity::Ambiguous(recorded)
        } else {
            Identity::Unique
        }
    }
}

/// The identity of `uri` as one query sees it: `facts` are those of the
/// queried document. Reads the import's list once.
pub fn at_document(
    conn: &Connection,
    import_id: i64,
    uri: &str,
    facts: &[SemanticFact],
) -> Result<Identity, String> {
    let (_, import) = store::semantic_import_by_id(conn, import_id)?
        .ok_or_else(|| "import vanished".to_string())?;
    let places: BTreeSet<(usize, usize)> = facts
        .iter()
        .filter(|f| {
            f.import_id == import_id && f.roles & 1 != 0 && f.symbol_uri.as_deref() == Some(uri)
        })
        .map(|f| (f.byte_start, f.byte_end))
        .collect();
    Ok(ImportIdentity::load(conn, &import)?.of(uri, places.len()))
}

/// Record the checked list for one artifact. Call inside the import's own
/// transaction: an import that fails must leave no list behind.
pub fn record(
    conn: &Connection,
    artifact_id: &str,
    ambiguous: &BTreeMap<String, usize>,
) -> Result<(), String> {
    let value = serde_json::json!({
        "version": VERSION,
        "artifact": artifact_id,
        "ambiguous": ambiguous,
    });
    store::meta_set(
        conn,
        &format!("{KEY_PREFIX}{artifact_id}"),
        &value.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"version":1,"artifact":"a1","ambiguous":{"dup":3}}"#;

    fn of(value: Option<&str>, uri: &str, places: usize) -> Identity {
        ImportIdentity::decode(value, "a1").of(uri, places)
    }

    #[test]
    fn a_checked_list_names_only_what_it_lists() {
        assert_eq!(of(Some(GOOD), "dup", 1), Identity::Ambiguous(3));
        assert_eq!(of(Some(GOOD), "other", 1), Identity::Unique);
        // Local symbols are counted in the document, not listed.
        assert_eq!(of(Some(GOOD), "local 0", 2), Identity::Ambiguous(2));
        assert_eq!(of(Some(GOOD), "local 0", 1), Identity::Unique);
    }

    #[test]
    fn anything_not_validated_for_this_artifact_is_unchecked() {
        for value in [
            None,
            Some(""),
            Some("not json"),
            Some("{}"),
            Some(r#"{"version":2,"artifact":"a1","ambiguous":{}}"#),
            Some(r#"{"version":1,"artifact":"a2","ambiguous":{}}"#),
            Some(r#"{"version":1,"artifact":"a1"}"#),
            Some(r#"{"version":1,"artifact":"a1","ambiguous":[]}"#),
            Some(r#"{"version":1,"artifact":"a1","ambiguous":{"x":"2"}}"#),
        ] {
            assert_eq!(of(value, "any", 1), Identity::Unchecked, "{value:?}");
        }
        assert_eq!(
            ImportIdentity::decode(Some(r#"{"version":1,"artifact":"","ambiguous":{}}"#), "")
                .of("any", 1),
            Identity::Unchecked
        );
    }

    #[test]
    fn record_round_trips_through_meta() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .unwrap();
        let list = BTreeMap::from([("a \"quoted\" uri".to_string(), 2)]);
        record(&conn, "a1", &list).unwrap();
        let value = store::meta_get(&conn, "semantic_identity:a1").unwrap();
        assert_eq!(
            of(value.as_deref(), "a \"quoted\" uri", 0),
            Identity::Ambiguous(2)
        );
    }
}
