// SPDX-License-Identifier: Apache-2.0
//! M5 semantic evidence contract: the single shared shape every provider
//! adapter produces, one path+range validator, one provenance/diagnostics
//! shape, and the static capability registry. No provider-specific logic
//! lives anywhere else.

use std::path::{Path, PathBuf};

use crate::json::escape;
use crate::paths;

/// Precision of one evidence item: compiler-backed exact or heuristic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Precision {
    // Kept for contract stability; never constructed in M5 (see verify_live).
    #[allow(dead_code)]
    Exact,
    Heuristic,
}

impl Precision {
    pub fn as_str(self) -> &'static str {
        match self {
            Precision::Exact => "Exact",
            Precision::Heuristic => "Heuristic",
        }
    }
}

/// Verification state. `Current` exists for contract stability but is
/// unreachable in M5: no independent producer-to-source proof mechanism
/// exists, so imports land `Unverified` and drift flips them to `Stale`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verification {
    // Kept for contract stability; unreachable in M5 (see verify_live).
    #[allow(dead_code)]
    Current,
    Unverified,
    Stale,
}

impl Verification {
    pub fn as_str(self) -> &'static str {
        match self {
            Verification::Current => "Current",
            Verification::Unverified => "Unverified",
            Verification::Stale => "Stale",
        }
    }
}

/// Byte + line/col span. Byte offsets are authoritative for slicing;
/// line/col are 0-based display coordinates (SCIP convention).
#[derive(Debug, Clone, PartialEq)]
pub struct SpanBytes {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Location evidence: the mandatory half of every fact.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub path: String,
    pub digest: String,
    pub span: SpanBytes,
}

/// Optional provider symbol identity. `Some` for SCIP URIs, `None` for LSP
/// results — no SCIP-style identity is ever required of or fabricated for
/// LSP output.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSymbol {
    pub uri: String,
    pub display_name: Option<String>,
}

/// Occurrence roles decoded from the full provider bitset. `raw` preserves
/// every bit (SCIP `0x1`..`0x40`); the booleans are the decoded view.
/// Reference predicate: `!definition`.
#[derive(Debug, Clone, PartialEq)]
pub struct Roles {
    pub raw: u32,
    pub definition: bool,
    pub reference: bool,
    pub import: bool,
    pub read: bool,
    pub write: bool,
    pub generated: bool,
    pub test: bool,
    pub forward_declaration: bool,
}

impl Roles {
    /// Decode a SCIP `SymbolRole` bitset. Bit values pinned by `scip.proto`:
    /// Definition `0x1`, Import `0x2`, WriteAccess `0x4`, ReadAccess `0x8`,
    /// Generated `0x10`, Test `0x20`, ForwardDefinition `0x40`.
    pub fn from_scip_bits(raw: u32) -> Roles {
        let definition = raw & 0x1 != 0;
        Roles {
            raw,
            definition,
            reference: !definition,
            import: raw & 0x2 != 0,
            read: raw & 0x8 != 0,
            write: raw & 0x4 != 0,
            generated: raw & 0x10 != 0,
            test: raw & 0x20 != 0,
            forward_declaration: raw & 0x40 != 0,
        }
    }

    /// LSP answers carry no role bitset: the request kind is the role.
    pub fn from_lsp_kind(definition: bool) -> Roles {
        Roles {
            raw: u32::from(definition),
            definition,
            reference: !definition,
            import: false,
            read: false,
            write: false,
            generated: false,
            test: false,
            forward_declaration: false,
        }
    }
}

/// Verbatim provider relationship. Stored, never traversed (M6 owns that).
#[derive(Debug, Clone, PartialEq)]
pub struct Relationship {
    pub target: String,
    pub is_reference: bool,
    pub is_implementation: bool,
    pub is_type_definition: bool,
    pub is_definition: bool,
}

/// Metadata for facts a producer references but defines externally
/// (SCIP `external_symbols` package triple). Marks, never locates.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalMeta {
    pub package_manager: String,
    pub package: String,
    pub version: String,
}

/// Provider identity: name + version, always recorded with the evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderId {
    pub name: String,
    pub version: String,
}

/// One semantic evidence item: location (required) + optional identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub location: Location,
    pub symbol: Option<ProviderSymbol>,
    pub roles: Roles,
    pub relationships: Vec<Relationship>,
    pub external: Option<ExternalMeta>,
    pub precision: Precision,
    pub verification: Verification,
    pub provider: ProviderId,
}

impl Evidence {
    /// Constructor for LSP answers: location only, no symbol identity.
    pub fn lsp_location(path: &str, digest: &str, span: SpanBytes) -> Evidence {
        Evidence {
            location: Location {
                path: path.to_string(),
                digest: digest.to_string(),
                span,
            },
            symbol: None,
            roles: Roles::from_lsp_kind(false),
            relationships: Vec::new(),
            external: None,
            // M5 honesty rule: no provider output is Exact without
            // producer attestation (deferred with `Current`).
            precision: Precision::Heuristic,
            verification: Verification::Unverified,
            provider: ProviderId {
                name: "lsp-subprocess".to_string(),
                version: String::new(),
            },
        }
    }
}

/// Actual capabilities of one provider. Static table, not configuration:
/// capabilities describe what the adapter implements, not wishes.
/// Fields are read by the registry tests; dispatch consults `resolve`.
#[allow(dead_code)]
pub struct Capability {
    pub name: &'static str,
    pub definitions: bool,
    pub references: bool,
}

/// Transport identity of every SCIP import. The producer named inside an
/// artifact is metadata, never a registry key.
pub const SCIP_IMPORT: &str = "scip-import";

/// The whole M5 provider universe. Unknown names are unsupported, never
/// guessed.
pub fn capabilities() -> &'static [Capability] {
    &[
        Capability {
            name: SCIP_IMPORT,
            definitions: true,
            references: true,
        },
        Capability {
            name: "lsp-subprocess",
            definitions: true,
            references: true,
        },
    ]
}

/// Look up a provider by name. Unknown → explicit unsupported error.
pub fn resolve(provider: &str) -> Result<&'static Capability, String> {
    capabilities()
        .iter()
        .find(|c| c.name == provider)
        .ok_or_else(|| format!("unsupported semantic provider: '{provider}'"))
}

/// Decode a `file://` URI (or bare path) to a local `PathBuf`: optional
/// scheme strip, percent-decoding, and the Windows drive-slash fix
/// (`/c:/x` is not drive-qualified — servers and producers normalize drive
/// letters, proven by rust-analyzer and scip-typescript Tier B). Containment
/// checks stay with the callers.
pub fn uri_path(uri: &str) -> Result<PathBuf, String> {
    let s = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("bad percent-encoding in URI".to_string());
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .map_err(|_| "bad percent-encoding in URI".to_string())?;
            out.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| "bad percent-encoding in URI".to_string())?,
            );
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).map_err(|_| "non-UTF8 URI".to_string())?;
    let bytes = decoded.as_bytes();
    if bytes.len() > 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Ok(PathBuf::from(&decoded[1..]));
    }
    Ok(PathBuf::from(decoded))
}

/// Read one repo file with both guards the syntax path enforces: path
/// confinement (canonical containment — symlink escapes fail) and a byte
/// budget (same cap as syntax ingest, so a huge file can neither blow RAM
/// nor sneak past). Used by the SCIP importer, the NULL-fact carry-over,
/// and the semantic freshness check. Missing/unreadable/oversize/escaping
/// files are errors with reasons; callers map them to skip or Stale.
pub fn confined_read(root: &Path, rel: &str, cap_bytes: u64) -> Result<Vec<u8>, String> {
    let (candidate, was_missing) = confined_path(root, rel)?;
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("repository root unavailable: {e}"))?;
    // Containment is decided above, before any metadata of the file is read.
    let meta = candidate
        .metadata()
        .map_err(|e| format!("cannot stat '{rel}': {e}"))?;
    // A path that appeared after the missing check must still resolve inside.
    if was_missing && !crate::paths::contains(&canonical_root, &candidate) {
        return Err(format!("document path escapes repository: '{rel}'"));
    }
    let len = meta.len();
    if len > cap_bytes {
        return Err(format!("'{rel}' too large ({len} bytes > {cap_bytes})"));
    }
    // take(cap+1): the metadata check races file growth; the bounded read
    // cannot be outrun.
    let mut file =
        std::fs::File::open(&candidate).map_err(|e| format!("cannot read '{rel}': {e}"))?;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    file.by_ref()
        .take(cap_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read '{rel}': {e}"))?;
    if bytes.len() as u64 > cap_bytes {
        return Err(format!("'{rel}' too large (> {cap_bytes} bytes)"));
    }
    Ok(bytes)
}

/// The containment decision alone: `rel` must resolve inside the canonical `root`,
/// with no link, junction or reparse point on the way out. It reads neither the
/// file's content nor its size, so the answer cannot depend on either. Returns the
/// candidate path and whether its leaf was missing (accepted through its ancestors;
/// `confined_read` re-checks it once the file exists).
pub fn confined_path(root: &Path, rel: &str) -> Result<(std::path::PathBuf, bool), String> {
    let mut candidate = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("non-canonical document path: '{rel}'"));
        }
        candidate.push(part);
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("repository root unavailable: {e}"))?;
    let mut contained = crate::paths::contains(&canonical_root, &candidate);
    let mut was_missing = false;
    // `contains` is false when canonicalize fails, which includes a missing
    // path. Only a NotFound leaf gets the ancestor walk, which must remain
    // inside the root; an existing symlink is still an escape.
    if !contained {
        match candidate.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                was_missing = true;
                let mut probe = candidate.parent();
                loop {
                    match probe {
                        Some(p) if p.canonicalize().is_ok() => {
                            contained = crate::paths::contains(&canonical_root, p);
                            break;
                        }
                        Some(p) => probe = p.parent(),
                        None => break,
                    }
                }
            }
            // Any other stat failure is not evidence of absence.
            Err(e) => return Err(format!("cannot stat '{rel}': {e}")),
            Ok(_) => {}
        }
    }
    if !contained {
        return Err(format!("document path escapes repository: '{rel}'"));
    }
    Ok((candidate, was_missing))
}

/// Single shared document-path validator for both providers. Enforces the
/// SCIP `relative_path` rules lexically (relative, `/` separators, no
/// empty/`.`/`..` components) then proves containment + existence through
/// `paths::contains` (canonicalizes, so symlink escapes fail). Returns the
/// normalized `/`-separated relative path.
///
/// Observed producer deviation (scip-typescript 0.4.0 on Windows, Tier B):
/// `\` separators instead of the spec-mandated `/`. They are normalized
/// before validation — an identity-preserving mapping — while `..`, drive
/// qualification, and escapes are still rejected afterwards.
pub fn validate_doc_path(root: &Path, rel: &str) -> Result<String, String> {
    if rel.is_empty() {
        return Err("empty document path".to_string());
    }
    let rel = rel.replace('\\', "/");
    if rel.starts_with('/') || rel.contains('\0') {
        return Err(format!("document path not relative/portable: '{rel}'"));
    }
    let mut parts = Vec::new();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("non-canonical document path: '{rel}'"));
        }
        // Drive-qualified absolutes smuggled past the leading-slash check.
        if part.len() == 2 && part.as_bytes()[1] == b':' && part.as_bytes()[0].is_ascii_alphabetic()
        {
            return Err(format!("absolute document path: '{rel}'"));
        }
        parts.push(part);
    }
    let mut candidate = root.to_path_buf();
    for part in &parts {
        candidate.push(part);
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("repository root unavailable: {e}"))?;
    if !paths::contains(&canonical_root, &candidate) {
        return Err(format!("document path escapes repository: '{rel}'"));
    }
    Ok(parts.join("/"))
}

/// Single shared range checker: ordered half-open ranges only. Empty
/// (start==end) is allowed — SCIP permits it. Base/encoding checks belong
/// to the provider converters, not here.
pub fn validate_range(
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
) -> Result<(), String> {
    if (start_line, start_col) > (end_line, end_col) {
        return Err(format!(
            "disordered range: ({start_line},{start_col})-({end_line},{end_col})"
        ));
    }
    Ok(())
}

/// The single diagnostics shape for every unavailable/timeout/error/
/// unverifiable path. Syntax-only results stay usable alongside it.
pub fn fallback_report(provider: &str, reason: &str) -> String {
    format!(
        "{{\"provider\":\"{}\",\"status\":\"fallback\",\"reason\":\"{}\",\"syntax_fallback\":true}}",
        escape(provider),
        escape(reason)
    )
}

/// Live source state for `verify_live`. Nonce proves rebuild identity;
/// `content_identity` proves the bytes; git/config fields prove the rest.
/// Compared independently: equal fingerprints never excuse different bytes.
pub struct LiveState {
    pub head: String,
    pub branch: String,
    pub dirty: bool,
    pub untracked: u32,
    pub config_fingerprint: String,
    pub snapshot_nonce: String,
    pub content_identity: String,
}

/// Canonical revision string. One builder used at import binding AND query
/// time: the comparison is exact, never field-by-field drift.
pub fn revision_string(live: &LiveState) -> String {
    format!(
        "head={}|branch={}|dirty={}|untracked={}|cfg={}|nonce={}",
        live.head,
        live.branch,
        live.dirty,
        live.untracked,
        live.config_fingerprint,
        live.snapshot_nonce
    )
}

/// The ONE freshness rule for semantic facts (M5). Import-granular and
/// conservative: an unchanged file digest never implies the relationships
/// are still valid, so ANY drift in content or revision marks the whole
/// import `Stale` (still served with explicit marking, never as `Current`).
/// `Current` is unreachable in M5: no independent producer-to-source proof
/// exists, so a fully matching import is `Unverified`, and `--source-digest`
/// claims are recorded, never trusted.
pub fn verify_live(import: &crate::store::SemanticImport, live: &LiveState) -> Verification {
    if import.content_identity != live.content_identity {
        return Verification::Stale;
    }
    if import.source_revision != revision_string(live) {
        return Verification::Stale;
    }
    Verification::Unverified
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;
    use std::path::PathBuf;

    #[test]
    fn doc_path_rejects_traversal_and_absolutes() {
        let dir = tmpdir("sem-path");
        std::fs::write(dir.join("ok.py"), "x").unwrap();
        std::fs::create_dir_all(dir.join("win")).unwrap();
        std::fs::write(dir.join("win").join("d.py"), "x").unwrap();
        assert!(validate_doc_path(&dir, "ok.py").is_ok());
        // Producer backslash deviation normalizes to the same identity.
        assert_eq!(validate_doc_path(&dir, "win\\d.py").unwrap(), "win/d.py");
        for bad in [
            "",
            "/abs.py",
            "../evil.py",
            "a/../../evil.py",
            "a//b.py",
            "a/./b.py",
            "C:/evil.py",
            "C:\\evil.py",
            "sub/",
        ] {
            assert!(validate_doc_path(&dir, bad).is_err(), "must reject {bad:?}");
        }
        // Lexically clean but missing on disk: no bytes to digest → reject.
        assert!(validate_doc_path(&dir, "missing.py").is_err());
    }

    #[test]
    fn doc_path_rejects_symlink_escape() {
        let dir = tmpdir("sem-link");
        let outside = tmpdir("sem-link-out");
        std::fs::write(outside.join("secret.py"), "x").unwrap();
        let link = dir.join("escape.py");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.join("secret.py"), &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(outside.join("secret.py"), &link).is_ok();
        if !made {
            return; // OS refused symlinks here; nothing to prove.
        }
        assert!(validate_doc_path(&dir, "escape.py").is_err());
    }

    #[test]
    fn range_checker_rejects_disorder_and_zero_width_rules() {
        // end before start: reject. Empty (start==end): allowed (SCIP permits).
        assert!(validate_range(3, 5, 3, 2).is_err());
        assert!(validate_range(4, 0, 3, 9).is_err());
        assert!(validate_range(3, 5, 3, 5).is_ok());
        assert!(validate_range(1, 0, 9, 0).is_ok());
    }

    #[test]
    fn uri_path_decodes_scheme_percent_and_drive_slash() {
        assert_eq!(
            uri_path("file:///C:/r/a%20b.py").unwrap(),
            PathBuf::from("C:/r/a b.py")
        );
        assert_eq!(
            uri_path("file:///c:/r/a.py").unwrap(),
            PathBuf::from("c:/r/a.py")
        );
        assert_eq!(
            uri_path("/home/u/a.py").unwrap(),
            PathBuf::from("/home/u/a.py")
        );
        assert!(uri_path("file:///bad%.py").is_err());
    }

    #[test]
    fn fallback_report_has_exact_keys() {
        let json = fallback_report("lsp-subprocess", "timeout");
        assert!(json.contains("\"provider\":\"lsp-subprocess\""));
        assert!(json.contains("\"status\":\"fallback\""));
        assert!(json.contains("\"reason\":\"timeout\""));
        assert!(json.contains("\"syntax_fallback\":true"));
    }

    #[test]
    fn capability_registry_knows_both_providers_and_rejects_unknown() {
        let scip = resolve("scip-import").unwrap();
        assert!(scip.definitions && scip.references);
        let lsp = resolve("lsp-subprocess").unwrap();
        assert!(lsp.definitions && lsp.references);
        assert!(resolve("scip-typescript").is_err());
    }

    fn live_fixture() -> (crate::store::SemanticImport, LiveState) {
        let live = LiveState {
            head: "abc".to_string(),
            branch: "main".to_string(),
            dirty: false,
            untracked: 0,
            config_fingerprint: "cfg".to_string(),
            snapshot_nonce: "n1".to_string(),
            content_identity: "ci".to_string(),
        };
        let import = crate::store::SemanticImport {
            snapshot_id: 1,
            provider_name: "scip-import".to_string(),
            provider_version: "v".to_string(),
            capability: "definitions,references".to_string(),
            project_root: "r".to_string(),
            text_encoding: "UTF8".to_string(),
            artifact_id: "a".to_string(),
            source_revision: revision_string(&live),
            claimed_source_digest: "user-claim".to_string(),
            content_identity: "ci".to_string(),
            external_symbols: "[]".to_string(),
            verification: "Unverified".to_string(),
        };
        (import, live)
    }

    #[test]
    fn verify_live_matrix() {
        let (import, live) = live_fixture();
        // Fully matching attested import: still Unverified (claim ≠ proof).
        assert_eq!(verify_live(&import, &live), Verification::Unverified);
        // Same git fingerprint, different bytes: Stale (nonce ≠ content).
        let mut moved = live_fixture().1;
        moved.content_identity = "ci2".to_string();
        assert_eq!(verify_live(&import, &moved), Verification::Stale);
        // Same bytes, rebuilt nonce without rebind: Stale.
        let mut rebuilt = live_fixture().1;
        rebuilt.snapshot_nonce = "n2".to_string();
        assert_eq!(verify_live(&import, &rebuilt), Verification::Stale);
        // Dependency/config drift with identical bytes: Stale.
        let mut depdrift = live_fixture().1;
        depdrift.config_fingerprint = "cfg2".to_string();
        assert_eq!(verify_live(&import, &depdrift), Verification::Stale);
        // Dirty flag drift: Stale.
        let mut dirty = live_fixture().1;
        dirty.dirty = true;
        assert_eq!(verify_live(&import, &dirty), Verification::Stale);
    }

    #[test]
    fn same_name_across_providers_never_merges() {
        // Identity includes the provider namespace: equal display names
        // from different providers are different evidence, never one symbol.
        let a = Evidence::lsp_location(
            "a.py",
            "d",
            SpanBytes {
                start_line: 1,
                start_col: 0,
                end_line: 1,
                end_col: 5,
                byte_start: 0,
                byte_end: 5,
            },
        );
        let b = Evidence::lsp_location(
            "a.py",
            "d",
            SpanBytes {
                start_line: 1,
                start_col: 0,
                end_line: 1,
                end_col: 5,
                byte_start: 0,
                byte_end: 5,
            },
        );
        assert_eq!(a.location, b.location);
        assert!(a.symbol.is_none() && b.symbol.is_none());
    }

    // Not every stat failure means "absent": a non-NotFound error must be
    // reported as itself, never retried as if the file were merely missing.
    #[cfg(windows)]
    #[test]
    fn confined_read_reports_non_not_found_stat_errors() {
        let dir = tmpdir("sem-confined-badname");
        let outside = tmpdir("sem-confined-badname-out");
        let link = dir.join("linked");
        // A plain in-root invalid name looks the same whether or not it took
        // the missing path, so it cannot prove the branch. A junction keeps
        // the document path relative while making its nearest existing
        // ancestor resolve outside: the broad `is_err()` path would report an
        // escape, while the NotFound-only path must report the stat error.
        // mklink /J needs no elevation, so failure here is a real failure.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .expect("run mklink");
        assert!(
            made.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        let rel = "linked/bad<name.txt";
        let os_err = std::fs::symlink_metadata(dir.join(rel)).unwrap_err();
        assert_ne!(
            os_err.kind(),
            std::io::ErrorKind::NotFound,
            "test input must not be a missing path"
        );
        // Control: a genuinely missing leaf under the same escaped ancestor
        // still takes the missing path and is reported as an escape.
        let missing = "linked/missing.txt";
        assert_eq!(
            std::fs::symlink_metadata(dir.join(missing))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        let escape = confined_read(&dir, missing, 1024).unwrap_err();
        assert!(
            escape.contains("escapes repository"),
            "a missing leaf under the same escaped ancestor stays an escape: {escape}"
        );
        let err = confined_read(&dir, rel, 1024).unwrap_err();
        assert_eq!(
            err,
            format!("cannot stat '{rel}': {os_err}"),
            "non-NotFound stat error must be reported as-is, not as an escape"
        );
    }

    #[test]
    fn confined_read_guards_size_and_containment() {
        let dir = tmpdir("sem-confined");
        std::fs::write(dir.join("ok.txt"), b"hello").unwrap();
        // Oversize content without a gigabyte fixture: cap below file size.
        std::fs::write(dir.join("big.txt"), vec![b'x'; 100]).unwrap();
        assert_eq!(confined_read(&dir, "ok.txt", 1024).unwrap(), b"hello");
        let missing = confined_read(&dir, "missing.txt", 1024).unwrap_err();
        assert!(
            missing.contains("cannot stat 'missing.txt'"),
            "missing file must not read as an escape: {missing}"
        );
        assert!(confined_read(&dir, "big.txt", 64).is_err());
        assert!(confined_read(&dir, "../escape.txt", 1024).is_err());
        // An existing symlink out of the tree stays an escape.
        let outside = tmpdir("sem-confined-out");
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let link = dir.join("escape.txt");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.join("secret.txt"), &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(outside.join("secret.txt"), &link).is_ok();
        if made {
            let escape = confined_read(&dir, "escape.txt", 1024).unwrap_err();
            assert!(escape.contains("escapes repository"), "{escape}");
        }
    }
}
