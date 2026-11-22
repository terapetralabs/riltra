// SPDX-License-Identifier: Apache-2.0
//! M7 knowledge records: typed model and input validation. Storage lives in
//! `knowledge_store.rs`. Statuses are `active`/`archived` plus the
//! Full-stage lifecycle statuses `superseded`/`retracted`; a create may only
//! use the Foundation statuses, because a new record has no history to
//! supersede or retract.
//!
//! Provenance trust boundary (G-PROV): `confirmed` is an explicit confirmation
//! claim, not an authenticated human identity; `candidate` is the default.
//! Confirmation never implies evidence verification, and evidence freshness
//! is orthogonal to `confirm_state`.

// Task 3 leaves the JSON entry points unwired (CLI dispatch is Task 7).
// Revisit this allow when the CLI is wired.
#![allow(dead_code)]

/// An exact record revision that supersedes another: the id plus the
/// revision pinned when the supersede transition was written.
#[derive(Debug, Clone, PartialEq)]
pub struct SupersededRef {
    pub id: String,
    pub revision: u64,
}

/// A single immutable revision of a knowledge record.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordVersion {
    pub id: String,
    pub revision: u64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub author: String,
    pub created_unix: i64,
    /// Present exactly when the revision carries the `superseded` status.
    pub superseded_by: Option<SupersededRef>,
    /// Retraction event fields, present exactly when the revision was
    /// written by a retract transition.
    pub retracted_reason: Option<String>,
    pub retracted_author: Option<String>,
    /// Provenance-confirmation state of this exact revision: `Some` on v2
    /// stores (`"candidate"` by default, `"confirmed"` after an explicit
    /// confirmation), `None` on v1 stores. The JSON output omits the field
    /// when it is `None`, so Foundation stores stay byte-identical and the
    /// field appears only once Full-stage data exists. `confirmed` is an
    /// explicit confirmation claim, not an authenticated human identity, and
    /// is never promoted automatically from evidence, freshness, search, or
    /// indexing.
    pub confirm_state: Option<String>,
    /// Present exactly when the revision carries benchmark measurement.
    pub measurement: Option<BenchmarkData>,
    /// Stored source bindings captured with this revision (immutable).
    pub evidence: Vec<CapturedEvidence>,
}

/// Input for creating a record. There is deliberately no way to express a
/// starting revision: creation always yields revision 1.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRecord {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub author: String,
}

/// Input for updating a record. ID and kind are absent by construction: an
/// update can never rename them. Every update carries the new status
/// explicitly; status changes are ordinary revisions, not side channels.
/// A `superseded` update must name the replacement revision and a reason; a
/// `retracted` update must carry a reason and no target; both fields are
/// rejected on any other status. `confirm` is an explicit, opt-in request:
/// no other field and no prior confirmation state ever implies it.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateRecord {
    pub title: String,
    pub body: String,
    pub status: String,
    pub author: String,
    pub superseded_by: Option<SupersededRef>,
    pub reason: Option<String>,
    pub confirm: bool,
}

/// Caller-supplied repository-scoped IDs: 1–128 ASCII chars,
/// `[a-z0-9][a-z0-9._-]*`. SQLite row IDs and titles are never identity.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || !id.is_ascii() {
        return Err(format!("invalid record id: {id:?}"));
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return Err(format!("invalid record id: {id:?}")),
    }
    let rest_ok = chars
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-');
    if !rest_ok {
        return Err(format!("invalid record id: {id:?}"));
    }
    Ok(())
}

/// A search path is matched exactly against the stored evidence string. It is
/// never normalized: separators, case, and `.`/`..` stay authored text.
pub fn validate_search_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("search path must not be empty".to_string());
    }
    if path.len() > MAX_SEARCH_PATH_BYTES {
        return Err(format!(
            "search path is {} bytes, at most {} allowed",
            path.len(),
            MAX_SEARCH_PATH_BYTES
        ));
    }
    Ok(())
}

/// Foundation kinds: `decision`, `constraint`, `benchmark`.
pub fn validate_kind(kind: &str) -> Result<(), String> {
    match kind {
        "decision" | "constraint" | "benchmark" => Ok(()),
        _ => Err(format!("invalid record kind: {kind:?}")),
    }
}

/// Update-time statuses: `active`, `archived`, and the Full-stage lifecycle
/// statuses `superseded` and `retracted`. Create uses
/// [`validate_new_status`]; status changes are always explicit updates.
pub fn validate_status(status: &str) -> Result<(), String> {
    match status {
        "active" | "archived" | "superseded" | "retracted" => Ok(()),
        _ => Err(format!("invalid record status: {status:?}")),
    }
}

/// Create-time statuses: `active`, `archived` only. A new record has no
/// history to supersede or retract, so the lifecycle statuses are rejected
/// before any mutation is attempted.
pub fn validate_new_status(status: &str) -> Result<(), String> {
    match status {
        "active" | "archived" => Ok(()),
        _ => Err(format!("invalid record status: {status:?}")),
    }
}

/// Authored relationship kinds: `relates_to`, `supports`, `contradicts`.
/// `supports` certifies nothing and `contradicts` judges nothing; both stay
/// inert assertions.
pub fn validate_relation_kind(kind: &str) -> Result<(), String> {
    match kind {
        "relates_to" | "supports" | "contradicts" => Ok(()),
        _ => Err(format!("invalid relation kind: {kind:?}")),
    }
}

/// Opaque repository-local relation IDs: non-empty, at most 128 bytes, and
/// NUL-free (SQLite text cannot store NUL; reject it with a clean error).
pub fn validate_relation_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || id.contains('\0') {
        return Err(format!("invalid relation id: {id:?}"));
    }
    Ok(())
}

/// A relation-id reference for retraction. Caller-supplied ids keep the
/// strict cap of [`validate_relation_id`]; a store-derived `supersedes`
/// lineage id is additionally accepted at any length when it parses exactly
/// as `supersedes:<record-id>:<revision>:<record-id>:<revision>` with valid
/// record ids and canonical decimal revisions. Only that exact shape is exempt:
/// malformed or unrelated over-limit ids are rejected like any other invalid
/// caller id, and creation (`link`) stays capped by [`validate_relation_id`].
pub fn validate_relation_reference(id: &str) -> Result<(), String> {
    if validate_relation_id(id).is_ok() {
        return Ok(());
    }
    fn invalid(id: &str) -> String {
        format!("invalid relation id: {id:?}")
    }
    let mut parts = id.split(':');
    let head = parts.next();
    let target_id = parts.next();
    let target_rev = parts.next();
    let src_id = parts.next();
    let src_rev = parts.next();
    let (Some("supersedes"), Some(target_id), Some(target_rev), Some(src_id), Some(src_rev)) =
        (head, target_id, target_rev, src_id, src_rev)
    else {
        return Err(invalid(id));
    };
    if parts.next().is_some() {
        return Err(invalid(id));
    }
    if validate_id(target_id).is_err()
        || validate_id(src_id).is_err()
        || !decimal_revision(target_rev)
        || !decimal_revision(src_rev)
    {
        return Err(invalid(id));
    }
    Ok(())
}

/// A revision as the derived format renders it: a plain decimal in i64 range.
/// `+1`, leading zeros and values the store could never have rendered are
/// non-canonical and rejected.
fn decimal_revision(part: &str) -> bool {
    part.parse::<u64>()
        .is_ok_and(|n| n <= i64::MAX as u64 && n.to_string() == part)
}

pub fn validate_new(input: &NewRecord) -> Result<(), String> {
    validate_id(&input.id)?;
    validate_kind(&input.kind)?;
    validate_new_status(&input.status)?;
    if input.title.is_empty() || input.body.is_empty() || input.author.is_empty() {
        return Err("title, body and author are required".to_string());
    }
    Ok(())
}

pub fn validate_update(input: &UpdateRecord) -> Result<(), String> {
    validate_status(&input.status)?;
    if input.title.is_empty() || input.body.is_empty() || input.author.is_empty() {
        return Err("title, body and author are required".to_string());
    }
    // Confirmation is about provenance, not lifecycle: a transition event is
    // never a confirmation and both cannot be expressed in one request.
    if input.confirm && matches!(input.status.as_str(), "superseded" | "retracted") {
        return Err("`confirm_state` is not valid with a lifecycle status transition".to_string());
    }
    // Lifecycle-field coherence: a supersede names its replacement revision
    // and a reason; a retract carries only a reason; a plain update carries
    // neither. The store re-checks targets against its own state.
    match input.status.as_str() {
        "superseded" => {
            if input.superseded_by.is_none() {
                return Err("status 'superseded' requires `superseded_by`".to_string());
            }
            if input.reason.as_deref().map(str::is_empty).unwrap_or(true) {
                return Err("status 'superseded' requires a non-empty `reason`".to_string());
            }
        }
        "retracted" => {
            if input.superseded_by.is_some() {
                return Err("`superseded_by` is only valid with status 'superseded'".to_string());
            }
            if input.reason.as_deref().map(str::is_empty).unwrap_or(true) {
                return Err("status 'retracted' requires a non-empty `reason`".to_string());
            }
        }
        _ => {
            if input.superseded_by.is_some() {
                return Err("`superseded_by` is only valid with status 'superseded'".to_string());
            }
            if input.reason.is_some() {
                return Err("`reason` is only valid with a lifecycle status".to_string());
            }
        }
    }
    Ok(())
}

use serde::de::{self, Deserializer as _, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// Byte limits count bytes, never Unicode characters.
pub const MAX_MUTATION_JSON_BYTES: usize = 1024 * 1024;
pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_AUTHOR_BYTES: usize = 256;
pub const MAX_EVIDENCE_REFS: usize = 64;
/// Pinned for the Task 5 search path; mutation inputs carry no query field.
pub const MAX_QUERY_TEXT_BYTES: usize = 4096;
/// Search accepts one exact stored evidence path. The bound mirrors
/// [`MAX_QUERY_TEXT_BYTES`] so no unbounded input is bound into SQL.
pub const MAX_SEARCH_PATH_BYTES: usize = 4096;
/// Search paging: Task 7 applies the default; the store enforces the cap.
pub const SEARCH_LIMIT_DEFAULT: usize = 50;
pub const SEARCH_LIMIT_MAX: usize = 1000;
/// Relation rationales and retraction reasons cap at 64 KiB (bytes).
pub const MAX_RATIONALE_BYTES: usize = 64 * 1024;
const MAX_JSON_DEPTH: usize = 64;

/// Parsed JSON that preserves every key occurrence so duplicates can be
/// rejected explicitly. Never parse mutations into `serde_json::Value`: its
/// last-wins objects silently discard duplicate-key evidence.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JVal {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

struct DupCheckVisitor {
    depth: usize,
    path: String,
}

struct Recurse {
    depth: usize,
    path: String,
}

impl<'de> de::DeserializeSeed<'de> for Recurse {
    type Value = JVal;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<JVal, D::Error> {
        deserializer.deserialize_any(DupCheckVisitor {
            depth: self.depth,
            path: self.path,
        })
    }
}

impl<'de> Visitor<'de> for DupCheckVisitor {
    type Value = JVal;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<JVal, E> {
        Ok(JVal::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<JVal, E> {
        Ok(JVal::Int(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<JVal, E> {
        Ok(JVal::UInt(v))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<JVal, E> {
        Ok(JVal::Float(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<JVal, E> {
        Ok(JVal::Str(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<JVal, E> {
        Ok(JVal::Str(v))
    }

    fn visit_none<E: de::Error>(self) -> Result<JVal, E> {
        Ok(JVal::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<JVal, E> {
        Ok(JVal::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JVal, A::Error> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(de::Error::custom("input too deeply nested"));
        }
        let mut items = Vec::new();
        let mut index = 0;
        while let Some(value) = seq.next_element_seed(Recurse {
            depth: self.depth + 1,
            path: format!("{}[{index}]", self.path),
        })? {
            items.push(value);
            index += 1;
        }
        Ok(JVal::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JVal, A::Error> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(de::Error::custom("input too deeply nested"));
        }
        // Keys arrive decoded (escapes resolved), so `{"a":1,"a":2}` and
        // `{"a":1,"\\u0061":2}` both report the same duplicate.
        let mut fields: Vec<(String, JVal)> = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if fields.iter().any(|(known, _)| known == &key) {
                return Err(de::Error::custom(format!(
                    "duplicate key `{}.{}`",
                    self.path, key
                )));
            }
            let child = format!("{}.{}", self.path, key);
            let value = map.next_value_seed(Recurse {
                depth: self.depth + 1,
                path: child,
            })?;
            fields.push((key, value));
        }
        Ok(JVal::Obj(fields))
    }
}

pub(crate) fn parse_dupless(text: &str) -> Result<JVal, String> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = deserializer
        .deserialize_any(DupCheckVisitor {
            depth: 0,
            path: "$".to_string(),
        })
        .map_err(|e| e.to_string())?;
    deserializer.end().map_err(|e| e.to_string())?;
    Ok(value)
}

pub(crate) fn as_obj<'a>(value: &'a JVal, path: &str) -> Result<&'a [(String, JVal)], String> {
    match value {
        JVal::Obj(fields) => Ok(fields),
        _ => Err(format!("type mismatch: field `{path}` must be an object")),
    }
}

pub(crate) fn find<'a>(obj: &'a [(String, JVal)], key: &str) -> Option<&'a JVal> {
    obj.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

pub(crate) fn check_known(
    obj: &[(String, JVal)],
    allowed: &[&str],
    path: &str,
) -> Result<(), String> {
    for (key, _) in obj {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown field `{path}.{key}`"));
        }
    }
    Ok(())
}

pub(crate) fn req_str(obj: &[(String, JVal)], key: &str, path: &str) -> Result<String, String> {
    match find(obj, key) {
        None => Err(format!("missing required field `{path}.{key}`")),
        Some(JVal::Str(s)) => Ok(s.clone()),
        Some(_) => Err(format!(
            "type mismatch: field `{path}.{key}` must be a string"
        )),
    }
}

pub(crate) fn req_int_non_negative(value: &JVal, path: &str) -> Result<i64, String> {
    match value {
        JVal::Int(i) if *i >= 0 => Ok(*i),
        JVal::UInt(u) => i64::try_from(*u).map_err(|_| format!("field `{path}` is out of range")),
        _ => Err(format!("field `{path}` must be a non-negative integer")),
    }
}

/// A validated evidence reference: repository-relative path plus an optional
/// half-open raw byte span. Shape, spans and count are validated at parse
/// time; the store captures digests and Git metadata at write time.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceRef {
    pub path: String,
    pub byte_start: Option<i64>,
    pub byte_end: Option<i64>,
    /// Digest the reviewed bytes had (docs ingest). `capture_evidence` refuses a
    /// different digest. Parsers leave it `None`.
    pub expected_digest: Option<String>,
}

/// An observed source binding captured at write time: repository-relative
/// path, optional half-open raw byte span, content digest of the cited bytes
/// (span slice, or the whole file when no span), and Git revision metadata
/// when available. This is an observation, never proof that the source
/// supports the prose: every claim stays [`VERIFICATION_UNVERIFIED`].
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedEvidence {
    pub path: String,
    pub byte_start: Option<i64>,
    pub byte_end: Option<i64>,
    pub digest: String,
    pub git_head: Option<String>,
    pub git_branch: Option<String>,
}

/// Freshness of one stored binding against a live repository root.
/// Filesystem observations are sampled separately from the knowledge-row read
/// transaction: they never establish an atomic snapshot of the working tree.
#[derive(Debug, Clone, PartialEq)]
pub enum EvidenceFreshness {
    /// Live bytes match the recorded digest.
    Fresh,
    /// Live bytes differ (or the cited span no longer fits): the original
    /// path/span/digest are preserved, `observed_digest` names what is there.
    Stale { observed_digest: String },
    /// The binding cannot be sampled (missing, escaping, unreadable,
    /// over-cap): `reason` names why. Never a replacement target.
    Unresolved { reason: String },
}

impl EvidenceFreshness {
    /// Inspectable one-word reason for logs and future JSON output.
    pub fn label(&self) -> &'static str {
        match self {
            EvidenceFreshness::Fresh => "fresh",
            EvidenceFreshness::Stale { .. } => "stale",
            EvidenceFreshness::Unresolved { .. } => "unresolved",
        }
    }
}

/// Knowledge claims are authored, never automatically verified — not even
/// when every cited source is fresh. No stored digest certifies prose.
pub const VERIFICATION_UNVERIFIED: &str = "Unverified";

/// Support summary for a revision's evidence count: no evidence means
/// unknown support, never a completeness or safety claim. Non-empty evidence
/// is still [`VERIFICATION_UNVERIFIED`], never verified.
pub fn evidence_support(evidence_count: usize) -> &'static str {
    if evidence_count == 0 {
        "unknown"
    } else {
        "unverified"
    }
}

/// One named benchmark metric with an explicit unit. Values must be finite;
/// no command executes the method or certifies the numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricData {
    pub name: String,
    pub value: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BenchmarkData {
    pub method: String,
    pub environment: String,
    pub sample_count: u64,
    pub metrics: Vec<MetricData>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedNew {
    pub record: NewRecord,
    // Task 4 owns evidence capture; refs are validated here and carried.
    #[allow(dead_code)]
    pub evidence: Vec<EvidenceRef>,
    pub measurement: Option<BenchmarkData>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedUpdate {
    pub record: UpdateRecord,
    // Task 4 owns evidence capture; refs are validated here and carried.
    #[allow(dead_code)]
    pub evidence: Vec<EvidenceRef>,
    pub measurement: Option<BenchmarkData>,
}

pub fn validate_metric_value(value: f64) -> Result<(), String> {
    if value.is_finite() {
        Ok(())
    } else {
        Err("metric value must be finite".to_string())
    }
}

fn parse_evidence(value: &JVal, path: &str) -> Result<Vec<EvidenceRef>, String> {
    let items = match value {
        JVal::Arr(items) => items,
        _ => return Err(format!("type mismatch: field `{path}` must be an array")),
    };
    if items.len() > MAX_EVIDENCE_REFS {
        return Err(format!(
            "field `{path}` holds {} references, at most {} allowed",
            items.len(),
            MAX_EVIDENCE_REFS
        ));
    }
    let mut refs = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let obj = as_obj(item, &item_path)?;
        check_known(obj, &["path", "byte_start", "byte_end"], &item_path)?;
        let rel = match find(obj, "path") {
            None => return Err(format!("missing required field `{item_path}.path`")),
            Some(JVal::Str(s)) if !s.is_empty() => s.clone(),
            Some(_) => {
                return Err(format!(
                    "field `{item_path}.path` must be a non-empty string"
                ))
            }
        };
        let mut byte_start = None;
        let mut byte_end = None;
        if let Some(v) = find(obj, "byte_start") {
            byte_start = Some(req_int_non_negative(v, &format!("{item_path}.byte_start"))?);
        }
        if let Some(v) = find(obj, "byte_end") {
            byte_end = Some(req_int_non_negative(v, &format!("{item_path}.byte_end"))?);
        }
        if let (Some(start), Some(end)) = (byte_start, byte_end) {
            if end <= start {
                return Err(format!(
                    "field `{item_path}` span must satisfy byte_end > byte_start"
                ));
            }
        }
        refs.push(EvidenceRef {
            path: rel,
            byte_start,
            byte_end,
            expected_digest: None,
        });
    }
    Ok(refs)
}

fn parse_metric_value(value: &JVal, path: &str) -> Result<f64, String> {
    match value {
        JVal::Int(i) => Ok(*i as f64),
        JVal::UInt(u) => Ok(*u as f64),
        JVal::Float(f) => {
            validate_metric_value(*f)?;
            Ok(*f)
        }
        _ => Err(format!("field `{path}` must be a number")),
    }
}

fn parse_measurement(value: &JVal, path: &str) -> Result<BenchmarkData, String> {
    let obj = as_obj(value, path)?;
    check_known(
        obj,
        &["method", "environment", "sample_count", "metrics"],
        path,
    )?;
    let method = req_str(obj, "method", path)?;
    let environment = req_str(obj, "environment", path)?;
    if method.is_empty() || environment.is_empty() {
        return Err(format!(
            "field `{path}` method and environment are required"
        ));
    }
    let sample_count = match find(obj, "sample_count") {
        None => return Err(format!("missing required field `{path}.sample_count`")),
        Some(JVal::Int(i)) if *i > 0 => *i as u64,
        // SQLite stores integers signed: cap the accepted range before any
        // write transaction starts rather than failing mid-commit.
        Some(JVal::UInt(u)) if *u > 0 && *u <= i64::MAX as u64 => *u,
        Some(_) => {
            return Err(format!(
                "field `{path}.sample_count` must be a positive integer in range"
            ));
        }
    };
    let metrics_obj = match find(obj, "metrics") {
        None => return Err(format!("missing required field `{path}.metrics`")),
        Some(v) => as_obj(v, &format!("{path}.metrics"))?,
    };
    if metrics_obj.is_empty() {
        return Err(format!(
            "field `{path}.metrics` must name at least one metric"
        ));
    }
    let mut metrics = Vec::with_capacity(metrics_obj.len());
    for (name, metric) in metrics_obj {
        if name.is_empty() {
            return Err(format!("field `{path}.metrics` names must be non-empty"));
        }
        let metric_path = format!("{path}.metrics.{name}");
        let metric_obj = as_obj(metric, &metric_path)?;
        check_known(metric_obj, &["value", "unit"], &metric_path)?;
        let value = match find(metric_obj, "value") {
            None => return Err(format!("missing required field `{metric_path}.value`")),
            Some(v) => parse_metric_value(v, &format!("{metric_path}.value"))?,
        };
        let unit = req_str(metric_obj, "unit", &metric_path)?;
        if unit.is_empty() {
            return Err(format!("field `{metric_path}.unit` must be explicit"));
        }
        metrics.push(MetricData {
            name: name.clone(),
            value,
            unit,
        });
    }
    Ok(BenchmarkData {
        method,
        environment,
        sample_count,
        metrics,
    })
}

fn check_byte_len(text: &str, max: usize, field: &str) -> Result<(), String> {
    if text.len() > max {
        return Err(format!(
            "field `{field}` is {} bytes, at most {max} allowed",
            text.len()
        ));
    }
    Ok(())
}

/// Strict create-input validation: duplicate keys (any depth), unknown or
/// missing fields, wrong types, byte limits and benchmark rules fail before
/// any mutation is attempted.
pub fn validate_create_json(bytes: &[u8]) -> Result<ValidatedNew, String> {
    if bytes.len() > MAX_MUTATION_JSON_BYTES {
        return Err(format!(
            "mutation input too large: {} bytes, at most {} allowed",
            bytes.len(),
            MAX_MUTATION_JSON_BYTES
        ));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| "mutation input is not valid UTF-8".to_string())?;
    let root = parse_dupless(text)?;
    let obj = as_obj(&root, "$")?;
    check_known(
        obj,
        &[
            "id",
            "kind",
            "title",
            "body",
            "status",
            "author",
            "evidence",
            "measurement",
        ],
        "$",
    )?;
    let record = NewRecord {
        id: req_str(obj, "id", "$")?,
        kind: req_str(obj, "kind", "$")?,
        title: req_str(obj, "title", "$")?,
        body: req_str(obj, "body", "$")?,
        status: req_str(obj, "status", "$")?,
        author: req_str(obj, "author", "$")?,
    };
    validate_new(&record)?;
    check_byte_len(&record.title, MAX_TITLE_BYTES, "$.title")?;
    check_byte_len(&record.body, MAX_BODY_BYTES, "$.body")?;
    check_byte_len(&record.author, MAX_AUTHOR_BYTES, "$.author")?;
    let evidence = match find(obj, "evidence") {
        None => Vec::new(),
        Some(v) => parse_evidence(v, "$.evidence")?,
    };
    let measurement = match find(obj, "measurement") {
        None => None,
        Some(v) => Some(parse_measurement(v, "$.measurement")?),
    };
    if record.kind == "benchmark" && measurement.is_none() {
        return Err("benchmark records require measurement".to_string());
    }
    if record.kind != "benchmark" && measurement.is_some() {
        return Err("measurement is only valid for benchmark records".to_string());
    }
    Ok(ValidatedNew {
        record,
        evidence,
        measurement,
    })
}

/// A `superseded_by` target: an exact existing record revision.
fn parse_superseded_by(value: &JVal, path: &str) -> Result<SupersededRef, String> {
    let obj = as_obj(value, path)?;
    check_known(obj, &["id", "revision"], path)?;
    let id = req_str(obj, "id", path)?;
    validate_id(&id)?;
    let revision = match find(obj, "revision") {
        None => return Err(format!("missing required field `{path}.revision`")),
        Some(v) => {
            let n = req_int_non_negative(v, &format!("{path}.revision"))?;
            u64::try_from(n).map_err(|_| format!("field `{path}.revision` is out of range"))?
        }
    };
    Ok(SupersededRef { id, revision })
}

/// Strict update-input validation. `id`/`kind` are rejected explicitly: an
/// update can never rename them.
pub fn validate_update_json(bytes: &[u8]) -> Result<ValidatedUpdate, String> {
    if bytes.len() > MAX_MUTATION_JSON_BYTES {
        return Err(format!(
            "mutation input too large: {} bytes, at most {} allowed",
            bytes.len(),
            MAX_MUTATION_JSON_BYTES
        ));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| "mutation input is not valid UTF-8".to_string())?;
    let root = parse_dupless(text)?;
    let obj = as_obj(&root, "$")?;
    if find(obj, "id").is_some() || find(obj, "kind").is_some() {
        return Err("record id and kind are immutable and cannot be set on update".to_string());
    }
    check_known(
        obj,
        &[
            "title",
            "body",
            "status",
            "author",
            "evidence",
            "measurement",
            "superseded_by",
            "reason",
            "confirm_state",
        ],
        "$",
    )?;
    let record = UpdateRecord {
        title: req_str(obj, "title", "$")?,
        body: req_str(obj, "body", "$")?,
        status: req_str(obj, "status", "$")?,
        author: req_str(obj, "author", "$")?,
        superseded_by: match find(obj, "superseded_by") {
            None => None,
            Some(v) => Some(parse_superseded_by(v, "$.superseded_by")?),
        },
        reason: match find(obj, "reason") {
            None => None,
            Some(JVal::Str(s)) => Some(s.clone()),
            Some(_) => return Err("type mismatch: field `$.reason` must be a string".to_string()),
        },
        // The only expressible request is `"confirmed"`: `"candidate"` is the
        // default state, not something a caller asks for, so it is rejected
        // exactly like any other unknown value.
        confirm: match find(obj, "confirm_state") {
            None => false,
            Some(JVal::Str(s)) if s == "confirmed" => true,
            Some(_) => {
                return Err("field `$.confirm_state` must be the string \"confirmed\"".to_string())
            }
        },
    };
    validate_update(&record)?;
    check_byte_len(&record.title, MAX_TITLE_BYTES, "$.title")?;
    check_byte_len(&record.body, MAX_BODY_BYTES, "$.body")?;
    check_byte_len(&record.author, MAX_AUTHOR_BYTES, "$.author")?;
    if let Some(reason) = &record.reason {
        check_byte_len(reason, MAX_RATIONALE_BYTES, "$.reason")?;
    }
    let evidence = match find(obj, "evidence") {
        None => Vec::new(),
        Some(v) => parse_evidence(v, "$.evidence")?,
    };
    let measurement = match find(obj, "measurement") {
        None => None,
        Some(v) => Some(parse_measurement(v, "$.measurement")?),
    };
    Ok(ValidatedUpdate {
        record,
        evidence,
        measurement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knowledge_id_rules() {
        assert!(validate_id("decision-001").is_ok());
        assert!(validate_id("a").is_ok());
        assert!(validate_id("z9._-x").is_ok());
        for bad in ["", "A", "-abc", ".abc", "_abc", "has space", "UPPER", "ábc"] {
            assert!(validate_id(bad).is_err(), "must reject {bad:?}");
        }
        assert!(validate_id(&"a".repeat(129)).is_err());
        assert!(validate_id(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn knowledge_relation_reference_rules() {
        // Caller-supplied references keep the strict 128-byte cap.
        assert!(validate_relation_reference("relation-1").is_ok());
        assert!(validate_relation_reference(&"x".repeat(128)).is_ok());
        assert!(validate_relation_reference(&"x".repeat(129)).is_err());
        // Full-length store-derived supersede ids are accepted when exactly
        // well formed, however long.
        let derived = format!("supersedes:{}:2:{}:1", "a".repeat(128), "b".repeat(128));
        assert!(derived.len() > 128);
        assert!(validate_relation_reference(&derived).is_ok());
        // Over-limit ids without that exact shape stay rejected: malformed
        // derived syntax, non-canonical revisions (`+1`, leading zeros,
        // beyond-i64 values) and unrelated prefixes are input errors.
        for bad in [
            format!("supersedes:{}:2:{}", "a".repeat(128), "b".repeat(128)),
            format!("supersedes:{}:x:{}:1", "a".repeat(128), "b".repeat(128)),
            format!("supersedes:{}:+1:{}:2", "a".repeat(128), "b".repeat(128)),
            format!("supersedes:{}:02:{}:1", "a".repeat(128), "b".repeat(128)),
            format!(
                "supersedes:{}:{}:{}:1",
                "a".repeat(128),
                u64::MAX,
                "b".repeat(128)
            ),
            format!("supersedes:{}:2:{}:1:2", "a".repeat(128), "b".repeat(128)),
            format!("superseded:{}:2:{}:1", "a".repeat(128), "b".repeat(128)),
            format!("supersedes:{}:2:{}:1", "A".repeat(128), "b".repeat(128)),
            format!(
                "{}:{}:2:{}:1",
                "supersedes".repeat(20),
                "a".repeat(128),
                "b".repeat(128)
            ),
        ] {
            assert!(
                validate_relation_reference(&bad).is_err(),
                "must reject {:?}",
                &bad[..40]
            );
        }
        // NUL bytes reject any length.
        assert!(validate_relation_reference(&"n\0".repeat(60)).is_err());
    }

    #[test]
    fn knowledge_kind_rules() {
        for ok in ["decision", "constraint", "benchmark"] {
            assert!(validate_kind(ok).is_ok());
        }
        for bad in ["", "note", "DECISION", "decision "] {
            assert!(validate_kind(bad).is_err());
        }
    }

    #[test]
    fn knowledge_status_rules() {
        for ok in ["active", "archived", "superseded", "retracted"] {
            assert!(validate_status(ok).is_ok());
        }
        for bad in ["", "Active", "deleted", "SUPERSEDED"] {
            assert!(validate_status(bad).is_err());
        }
        // A new record has no history to supersede or retract: create keeps
        // the Foundation statuses only.
        for ok in ["active", "archived"] {
            assert!(validate_new_status(ok).is_ok());
        }
        for bad in ["superseded", "retracted"] {
            assert!(validate_new_status(bad).is_err());
        }
    }

    #[test]
    fn knowledge_json_size_cap() {
        let big = format!(
            "{{\"id\":\"d1\",\"kind\":\"decision\",\"title\":\"t\",\"body\":\"{}\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[]}}",
            "b".repeat(1024)
        );
        assert!(validate_create_json(big.as_bytes()).is_ok());
        let over = vec![b'x'; 1024 * 1024 + 1];
        let err = validate_create_json(&over).unwrap_err();
        assert!(err.contains("too large"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_json_duplicate_top_level_valid_fields() {
        // Otherwise-valid input; the duplicated `title` must fail as a
        // duplicate key, not as an unknown field.
        let raw = r#"{"id":"d1","kind":"decision","title":"t","title":"t2","body":"b","status":"active","author":"r","evidence":[]}"#;
        let err = validate_create_json(raw.as_bytes()).unwrap_err();
        assert!(err.contains("duplicate"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_json_duplicate_nested_metric() {
        let raw = r#"{"id":"b1","kind":"benchmark","title":"t","body":"b","status":"active","author":"r","evidence":[],
            "measurement":{"method":"m","environment":"e","sample_count":10,
            "metrics":{"m1":{"value":1.0,"unit":"ms"},"m1":{"value":2.0,"unit":"ms"}}}}"#;
        let err = validate_create_json(raw.as_bytes()).unwrap_err();
        assert!(err.contains("duplicate"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_json_unknown_missing_type_rejected() {
        let unknown = r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","author":"r","evidence":[],"nope":1}"#;
        assert!(validate_create_json(unknown.as_bytes())
            .unwrap_err()
            .contains("unknown"));
        let nested_unknown = r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","author":"r",
            "evidence":[{"path":"a.rs","bogus":1}]}"#;
        assert!(validate_create_json(nested_unknown.as_bytes())
            .unwrap_err()
            .contains("unknown"));
        let missing = r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","evidence":[]}"#;
        assert!(validate_create_json(missing.as_bytes())
            .unwrap_err()
            .contains("missing"));
        let wrong_type = r#"{"id":"d1","kind":"decision","title":7,"body":"b","status":"active","author":"r","evidence":[]}"#;
        assert!(validate_create_json(wrong_type.as_bytes())
            .unwrap_err()
            .contains("type"));
        let not_object = r#"[1,2]"#;
        assert!(validate_create_json(not_object.as_bytes()).is_err());
    }

    #[test]
    fn knowledge_json_byte_limits_utf8() {
        let over_multibyte = "é".repeat(300);
        assert_eq!(over_multibyte.len(), 600);
        let raw = format!(
            "{{\"id\":\"d1\",\"kind\":\"decision\",\"title\":\"{over_multibyte}\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[]}}"
        );
        let err = validate_create_json(raw.as_bytes()).unwrap_err();
        assert!(err.contains("title"), "unexpected: {err}");
        // Exactly 512 bytes of multibyte text is accepted (byte limit, not chars).
        let exact = "é".repeat(256);
        assert_eq!(exact.len(), 512);
        let raw = format!(
            "{{\"id\":\"d1\",\"kind\":\"decision\",\"title\":\"{exact}\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[]}}"
        );
        assert!(validate_create_json(raw.as_bytes()).is_ok());
        let big_body = "b".repeat(64 * 1024 + 1);
        let raw = format!(
            "{{\"id\":\"d1\",\"kind\":\"decision\",\"title\":\"t\",\"body\":\"{big_body}\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[]}}"
        );
        assert!(validate_create_json(raw.as_bytes())
            .unwrap_err()
            .contains("body"));
        let big_author = "r".repeat(257);
        let raw = format!(
            "{{\"id\":\"d1\",\"kind\":\"decision\",\"title\":\"t\",\"body\":\"b\",\"status\":\"active\",\"author\":\"{big_author}\",\"evidence\":[]}}"
        );
        assert!(validate_create_json(raw.as_bytes())
            .unwrap_err()
            .contains("author"));
    }

    #[test]
    fn knowledge_json_benchmark_rules() {
        let base = |measurement: &str| {
            format!(
                "{{\"id\":\"b1\",\"kind\":\"benchmark\",\"title\":\"t\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[],\"measurement\":{measurement}}}"
            )
        };
        // Measurement required for benchmarks, forbidden otherwise.
        let no_measurement = r#"{"id":"b1","kind":"benchmark","title":"t","body":"b","status":"active","author":"r","evidence":[]}"#;
        assert!(validate_create_json(no_measurement.as_bytes())
            .unwrap_err()
            .contains("measurement"));
        let with_measurement = r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","author":"r","evidence":[],"measurement":{}}"#;
        assert!(validate_create_json(with_measurement.as_bytes()).is_err());
        // Missing method / bad sample count / empty metrics.
        assert!(validate_create_json(
            base(
                r#"{"environment":"e","sample_count":10,"metrics":{"m":{"value":1.0,"unit":"ms"}}}"#
            )
            .as_bytes()
        )
        .unwrap_err()
        .contains("method"));
        for bad_sample in ["0", "-3", "1.5", "\"10\""] {
            let raw = base(&format!(
                r#"{{"method":"m","environment":"e","sample_count":{bad_sample},"metrics":{{"m":{{"value":1.0,"unit":"ms"}}}}}}"#
            ));
            assert!(
                validate_create_json(raw.as_bytes()).is_err(),
                "sample_count {bad_sample} must fail"
            );
        }
        let empty_metrics =
            base(r#"{"method":"m","environment":"e","sample_count":10,"metrics":{}}"#);
        assert!(validate_create_json(empty_metrics.as_bytes()).is_err());
        // Non-finite metric value rejected (typed layer, not parser luck).
        assert!(validate_metric_value(f64::INFINITY).is_err());
        assert!(validate_metric_value(f64::NAN).is_err());
        assert!(validate_metric_value(1.5).is_ok());
        // Metric without unit rejected.
        let no_unit = base(
            r#"{"method":"m","environment":"e","sample_count":10,"metrics":{"m":{"value":1.0}}}"#,
        );
        assert!(validate_create_json(no_unit.as_bytes())
            .unwrap_err()
            .contains("unit"));
        // A valid benchmark passes.
        let good = base(
            r#"{"method":"m","environment":"e","sample_count":10,"metrics":{"m":{"value":1.0,"unit":"ms"}}}"#,
        );
        assert!(validate_create_json(good.as_bytes()).is_ok());
    }

    #[test]
    fn knowledge_json_sample_count_range() {
        let base = |sample: &str| {
            format!(
                "{{\"id\":\"b1\",\"kind\":\"benchmark\",\"title\":\"t\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[],\"measurement\":{{\"method\":\"m\",\"environment\":\"e\",\"sample_count\":{sample},\"metrics\":{{\"m\":{{\"value\":1.0,\"unit\":\"ms\"}}}}}}}}"
            )
        };
        // i64::MAX is representable as a SQLite signed integer: accepted.
        assert!(validate_create_json(base("9223372036854775807").as_bytes()).is_ok());
        // i64::MAX + 1 is not: rejected before any write transaction starts.
        let err = validate_create_json(base("9223372036854775808").as_bytes()).unwrap_err();
        assert!(err.contains("range"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_json_evidence_rules() {
        // 65 references exceed the per-revision bound.
        let refs: Vec<String> = (0..65)
            .map(|i| format!("{{\"path\":\"f{i}.rs\"}}"))
            .collect();
        let raw = format!(
            "{{\"id\":\"d1\",\"kind\":\"decision\",\"title\":\"t\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[{}]}}",
            refs.join(",")
        );
        assert!(validate_create_json(raw.as_bytes())
            .unwrap_err()
            .contains("evidence"));
        // Non-object item and bad span rejected.
        let item = r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","author":"r","evidence":["x.rs"]}"#;
        assert!(validate_create_json(item.as_bytes())
            .unwrap_err()
            .contains("type"));
        let span = r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","author":"r",
            "evidence":[{"path":"a.rs","byte_start":10,"byte_end":5}]}"#;
        assert!(validate_create_json(span.as_bytes())
            .unwrap_err()
            .contains("span"));
        // Well-formed refs (one with span, one bare) validate.
        let good = r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","author":"r",
            "evidence":[{"path":"a.rs","byte_start":0,"byte_end":8},{"path":"b.rs"}]}"#;
        let parsed = validate_create_json(good.as_bytes()).unwrap();
        assert_eq!(parsed.evidence.len(), 2);
    }

    #[test]
    fn knowledge_json_update_rules() {
        // id/kind present in update input: immutable, rejected explicitly.
        let with_id =
            r#"{"id":"d1","title":"t","body":"b","status":"active","author":"r","evidence":[]}"#;
        let err = validate_update_json(with_id.as_bytes()).unwrap_err();
        assert!(
            err.contains("immutable") || err.contains("unknown"),
            "unexpected: {err}"
        );
        let with_kind = r#"{"kind":"decision","title":"t","body":"b","status":"active","author":"r","evidence":[]}"#;
        let err = validate_update_json(with_kind.as_bytes()).unwrap_err();
        assert!(
            err.contains("immutable") || err.contains("unknown"),
            "unexpected: {err}"
        );
        let bad_status =
            r#"{"title":"t","body":"b","status":"superseded","author":"r","evidence":[]}"#;
        assert!(validate_update_json(bad_status.as_bytes()).is_err());
        let good = r#"{"title":"t","body":"b","status":"archived","author":"r","evidence":[]}"#;
        assert!(validate_update_json(good.as_bytes()).is_ok());
    }

    fn superseded_ref(id: &str, revision: u64) -> SupersededRef {
        SupersededRef {
            id: id.to_string(),
            revision,
        }
    }

    #[test]
    fn knowledge_json_fullstage_update_rules() {
        let update = |extra: &str| {
            format!(
                "{{\"title\":\"t\",\"body\":\"b\",\"status\":\"superseded\",\"author\":\"r\",\"evidence\":[]{extra}}}"
            )
        };
        // superseded requires both superseded_by and reason.
        let err = validate_update_json(update("").as_bytes()).unwrap_err();
        assert!(err.contains("superseded_by"), "unexpected: {err}");
        let err = validate_update_json(
            update(",\"superseded_by\":{\"id\":\"d2\",\"revision\":1}").as_bytes(),
        )
        .unwrap_err();
        assert!(err.contains("reason"), "unexpected: {err}");
        let ok = update(
            ",\"superseded_by\":{\"id\":\"d2\",\"revision\":1},\"reason\":\"replaced by d2\"",
        );
        let parsed = validate_update_json(ok.as_bytes()).unwrap();
        assert_eq!(parsed.record.superseded_by, Some(superseded_ref("d2", 1)));
        assert_eq!(parsed.record.reason.as_deref(), Some("replaced by d2"));
        // Unknown nested fields, invalid ids, missing/non-integer revisions.
        for bad in [
            ",\"superseded_by\":{\"id\":\"d2\",\"revision\":1,\"x\":2},\"reason\":\"r\"",
            ",\"superseded_by\":{\"id\":\"D2\",\"revision\":1},\"reason\":\"r\"",
            ",\"superseded_by\":{\"id\":\"d2\"},\"reason\":\"r\"",
            ",\"superseded_by\":{\"id\":\"d2\",\"revision\":\"1\"},\"reason\":\"r\"",
            ",\"superseded_by\":{\"id\":\"d2\",\"revision\":-1},\"reason\":\"r\"",
        ] {
            let err = validate_update_json(update(bad).as_bytes()).unwrap_err();
            assert!(!err.is_empty(), "case must be rejected: {bad}");
        }
        let retract = |extra: &str| {
            format!(
                "{{\"title\":\"t\",\"body\":\"b\",\"status\":\"retracted\",\"author\":\"r\",\"evidence\":[]{extra}}}"
            )
        };
        // retracted requires reason and must not carry a target.
        let err = validate_update_json(retract("").as_bytes()).unwrap_err();
        assert!(err.contains("reason"), "unexpected: {err}");
        assert!(validate_update_json(retract(",\"reason\":\"withdrawn\"").as_bytes()).is_ok());
        assert!(validate_update_json(
            retract(",\"reason\":\"w\",\"superseded_by\":{\"id\":\"d2\",\"revision\":1}")
                .as_bytes()
        )
        .is_err());
        // Lifecycle fields are invalid on a plain update.
        let plain = r#"{"title":"t","body":"b","status":"active","author":"r","evidence":[],"reason":"why"}"#;
        assert!(validate_update_json(plain.as_bytes()).is_err());
        let plain = r#"{"title":"t","body":"b","status":"active","author":"r","evidence":[],"superseded_by":{"id":"d2","revision":1}}"#;
        assert!(validate_update_json(plain.as_bytes()).is_err());
        // Reason byte cap.
        let big = "r".repeat(MAX_RATIONALE_BYTES + 1);
        let err = validate_update_json(retract(&format!(",\"reason\":\"{big}\"")).as_bytes())
            .unwrap_err();
        assert!(err.contains("reason"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_json_confirm_rules() {
        let update = |extra: &str| {
            format!(
                "{{\"title\":\"t\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[]{extra}}}"
            )
        };
        // Withheld by default; the request parses only as "confirmed".
        let parsed = validate_update_json(update("").as_bytes()).unwrap();
        assert!(!parsed.record.confirm);
        let parsed =
            validate_update_json(update(",\"confirm_state\":\"confirmed\"").as_bytes()).unwrap();
        assert!(parsed.record.confirm);
        for bad in [
            ",\"confirm_state\":\"candidate\"",
            ",\"confirm_state\":\"Confirmed\"",
            ",\"confirm_state\":true",
            ",\"confirm_state\":1",
            ",\"confirm_state\":null",
        ] {
            let err = validate_update_json(update(bad).as_bytes()).unwrap_err();
            assert!(
                err.contains("confirm_state"),
                "case must be rejected: {bad}"
            );
        }
        // Confirmation never combines with a lifecycle transition.
        let combined = update(
            ",\"confirm_state\":\"confirmed\",\"superseded_by\":{\"id\":\"d2\",\"revision\":1},\"reason\":\"r\"",
        )
        .replace("\"status\":\"active\"", "\"status\":\"superseded\"");
        let err = validate_update_json(combined.as_bytes()).unwrap_err();
        assert!(err.contains("confirm_state"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_new_and_update_rules() {
        let good = NewRecord {
            id: "decision-001".to_string(),
            kind: "decision".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            status: "active".to_string(),
            author: "r".to_string(),
        };
        assert!(validate_new(&good).is_ok());
        let mut bad_kind = good.clone();
        bad_kind.kind = "note".to_string();
        assert!(validate_new(&bad_kind).is_err());
        let mut bad_status = good.clone();
        bad_status.status = "retracted".to_string();
        assert!(validate_new(&bad_status).is_err());
        let mut empty_title = good.clone();
        empty_title.title = String::new();
        assert!(validate_new(&empty_title).is_err());
        let update = UpdateRecord {
            title: "t2".to_string(),
            body: "b2".to_string(),
            status: "archived".to_string(),
            author: "r".to_string(),
            superseded_by: None,
            reason: None,
            confirm: false,
        };
        assert!(validate_update(&update).is_ok());
        // Confirmation is opt-in and valid on a content revision...
        let mut confirmed = update.clone();
        confirmed.confirm = true;
        assert!(validate_update(&confirmed).is_ok());
        // ... but never jointly with a lifecycle transition event.
        let mut confirmed_supersede = confirmed.clone();
        confirmed_supersede.status = "superseded".to_string();
        confirmed_supersede.superseded_by = Some(superseded_ref("d2", 1));
        confirmed_supersede.reason = Some("replaced".to_string());
        let err = validate_update(&confirmed_supersede).unwrap_err();
        assert!(err.contains("confirm_state"), "unexpected: {err}");
        let mut confirmed_retract = confirmed.clone();
        confirmed_retract.status = "retracted".to_string();
        confirmed_retract.reason = Some("withdrawn".to_string());
        assert!(validate_update(&confirmed_retract).is_err());
        let mut bad_update = update.clone();
        bad_update.status = "superseded".to_string();
        // Supersede without a target and reason is rejected, then completed.
        assert!(validate_update(&bad_update).is_err());
        bad_update.superseded_by = Some(superseded_ref("d2", 1));
        assert!(validate_update(&bad_update).is_err());
        bad_update.reason = Some("replaced".to_string());
        assert!(validate_update(&bad_update).is_ok());
        // Retraction requires a reason and no target.
        let mut retract = update.clone();
        retract.status = "retracted".to_string();
        assert!(validate_update(&retract).is_err());
        retract.reason = Some("withdrawn".to_string());
        assert!(validate_update(&retract).is_ok());
        retract.superseded_by = Some(superseded_ref("d2", 1));
        assert!(validate_update(&retract).is_err());
    }
}
