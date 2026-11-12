// SPDX-License-Identifier: Apache-2.0
//! M3 durable store: SQLite canonical storage with versioned schema,
//! FTS5 lexical search, and snapshot-scoped facts. Symbol rowids are
//! per-snapshot only — never permanent cross-revision identities.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

/// Current schema version (`PRAGMA user_version`).
/// 5: the `reference_occurrences` table (E7 Slice 2).
pub const SCHEMA_VERSION: u32 = 5;
/// Parser/extraction version. Bump whenever `syntax.rs` extraction or the
/// syntax edges derived from it change; freshness treats a mismatch as stale
/// (never silently current).
/// 2: C++ data members are extracted as `field` symbols (E7 Slice 1).
/// 3: the terminal span of a C++ call excludes template arguments.
/// 4: C++ name occurrences are extracted (E7 Slice 2).
/// 5: the argument of a C++ factory call (`make_unique<T>`) is a candidate edge.
/// 6: `.inl` files are extracted as C++ (G-SYN coverage gap, M9).
/// 7: a multi-line import ends on its last line (G-SYN import span fix, M9).
/// 8: JS/TS/TSX `function*` declarations are extracted as function definitions.
/// 9: Go files (`.go`) are extracted: functions, methods, struct/interface/type
/// declarations, imports and calls (syntax facts only).
/// 10: Java files (`.java`) are extracted: classes, records, enums, interfaces,
/// methods, constructors, imports and method invocations (syntax facts only).
pub const PARSER_VERSION: u32 = 10;

/// Grammar + runtime versions pinned in `Cargo.lock`. A test pins these
/// against the lockfile so they cannot drift silently.
pub fn grammar_versions() -> String {
    "tree-sitter:0.25.10,python:0.25.0,javascript:0.25.0,typescript:0.23.2,tsx:0.23.2,rust:0.24.2,c:0.24.2,cpp:0.23.4,go:0.25.0,java:0.23.5"
        .to_string()
}

/// FNV-1a 64-bit hex. Non-crypto change-detection digest only.
/// shortcut: upgrade to SHA-256 if digests ever back security decisions.
pub fn content_digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Per-repo database file under the local Riltra home (never inside repos).
pub fn db_path(home: &Path, repo: &str) -> PathBuf {
    home.join("index").join(format!("{repo}.db"))
}

/// Staging file used during rebuilds; atomically promoted on success.
pub fn staging_path(home: &Path, repo: &str) -> PathBuf {
    let mut path = db_path(home, repo);
    path.set_extension("db.staging");
    path
}

fn open_inner(path: Option<&Path>) -> Result<Connection, String> {
    let conn = match path {
        Some(p) => Connection::open(p).map_err(|e| format!("cannot open database: {e}"))?,
        None => Connection::open_in_memory().map_err(|e| format!("cannot open memory db: {e}"))?,
    };
    // Read probe: rejects garbage files here, not at first query.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|e| format!("not a readable database: {e}"))?;
    // Every open ends at the current schema: empty files are built here, so
    // no caller can ever observe missing tables. Forward versions still fail
    // loudly inside `migrate`.
    migrate(&conn)?;
    // WAL lets readers hold a stable snapshot while a rebuild commits.
    // Best-effort: exotic filesystems fall back to rollback mode (still
    // correct, less concurrent).
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
    Ok(conn)
}

pub fn open_live(path: &Path) -> Result<Connection, String> {
    open_inner(Some(path))
}

/// Read probe with a busy timeout: distinguishes corrupt/unreadable files
/// from transiently locked ones. Used before destructive recovery.
pub fn is_readable_db(path: &Path) -> bool {
    // Never create files from a read probe.
    if !path.is_file() {
        return false;
    }
    let Ok(conn) = Connection::open(path) else {
        return false;
    };
    let _ = conn.execute_batch("PRAGMA busy_timeout = 3000;");
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })
    .is_ok()
}

/// Classification of a code-index open through the MCP read path.
#[derive(Debug)]
pub enum IndexRead {
    /// No database file exists (per-tool `no-index` behavior; no creation).
    Missing,
    /// Existing database at the current schema, open read-only.
    Ready(Connection),
    /// Existing database older than `SCHEMA_VERSION` (must not migrate here).
    Outdated(u32),
    /// Existing database newer than `SCHEMA_VERSION` (refused, never guessed).
    Ahead(u32),
    /// Corrupt, unreadable, or unsafe (symlink/non-file) database path.
    Error(String),
}

/// Open an existing code-index database read-only for the MCP tools. Never
/// creates, never migrates: a missing file is `Missing`, and `user_version`
/// is validated on the same connection the caller reads through, so a
/// version check cannot race a check-then-open gap into a writable reopen.
/// `immutable=1` is deliberately not used: live databases may change.
/// `SQLITE_OPEN_NOFOLLOW` is deliberately not used either: SQLite rejects any
/// path whose resolved chain contains a symlinked component (macOS `/var` is
/// a symlink), so it would fail every macOS read; the `symlink_metadata`
/// gate above is the fail-closed control for the database path itself.
pub fn open_read(path: &Path) -> IndexRead {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return IndexRead::Missing,
        Err(e) => return IndexRead::Error(format!("cannot stat database: {e}")),
        Ok(meta) if meta.file_type().is_symlink() => {
            return IndexRead::Error(format!("database path is a symlink: {}", path.display()))
        }
        Ok(meta) if !meta.is_file() => {
            return IndexRead::Error(format!(
                "database path is not a regular file: {}",
                path.display()
            ))
        }
        Ok(_) => {}
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let conn = match Connection::open_with_flags(path, flags) {
        Ok(conn) => conn,
        Err(e) => return IndexRead::Error(format!("cannot open database read-only: {e}")),
    };
    // Bounded wait while a concurrent rebuild's promotion finishes.
    let _ = conn.execute_batch("PRAGMA busy_timeout = 3000;");
    // Read probe: rejects garbage files here, not at first query.
    if let Err(e) = conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    }) {
        return IndexRead::Error(format!("not a readable database: {e}"));
    }
    let version: u32 = match conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)) {
        Ok(v) => v.max(0) as u32,
        Err(e) => return IndexRead::Error(format!("cannot read schema version: {e}")),
    };
    match version {
        v if v == SCHEMA_VERSION => IndexRead::Ready(conn),
        v if v < SCHEMA_VERSION => IndexRead::Outdated(v),
        v => IndexRead::Ahead(v),
    }
}

#[cfg(test)]
pub fn open_memory() -> Result<Connection, String> {
    open_inner(None)
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo TEXT NOT NULL, root TEXT NOT NULL, created_unix INTEGER NOT NULL,
  head TEXT, branch TEXT, dirty INTEGER NOT NULL, untracked INTEGER NOT NULL,
  parser_version INTEGER NOT NULL, grammar_versions TEXT NOT NULL,
  config_fingerprint TEXT NOT NULL, pre_fingerprint TEXT NOT NULL,
  post_fingerprint TEXT NOT NULL, status_at_write TEXT NOT NULL,
  nonce TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS files (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  path TEXT NOT NULL, language TEXT NOT NULL, bytes INTEGER NOT NULL,
  digest TEXT NOT NULL, errors INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_files_snapshot ON files(snapshot_id);
CREATE TABLE IF NOT EXISTS symbols (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  file_id INTEGER NOT NULL, kind TEXT NOT NULL, name TEXT,
  start_line INTEGER NOT NULL, start_col INTEGER NOT NULL,
  end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
  byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
  provider TEXT NOT NULL, grammar TEXT NOT NULL, language TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_symbols_snapshot ON symbols(snapshot_id);
CREATE TABLE IF NOT EXISTS imports (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  file_id INTEGER NOT NULL, text TEXT, start_line INTEGER NOT NULL,
  start_col INTEGER NOT NULL, end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
  byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
  provider TEXT NOT NULL, grammar TEXT NOT NULL, language TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  file_id INTEGER NOT NULL, name TEXT, start_line INTEGER NOT NULL,
  start_col INTEGER NOT NULL, end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
  byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
  callee_start_line INT NULL, callee_start_col INT NULL,
  callee_end_line INT NULL, callee_end_col INT NULL,
  callee_byte_start INT NULL, callee_byte_end INT NULL,
  tcallee_start_line INT NULL, tcallee_start_col INT NULL,
  tcallee_end_line INT NULL, tcallee_end_col INT NULL,
  tcallee_byte_start INT NULL, tcallee_byte_end INT NULL,
  unresolved INTEGER NOT NULL, provider TEXT NOT NULL, grammar TEXT NOT NULL,
  language TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS reference_occurrences (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  file_id INTEGER NOT NULL, name TEXT NOT NULL, role TEXT NOT NULL,
  start_line INTEGER NOT NULL, start_col INTEGER NOT NULL,
  end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
  byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
  provider TEXT NOT NULL, grammar TEXT NOT NULL, language TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ref_occ_name ON reference_occurrences(snapshot_id, name);
CREATE TABLE IF NOT EXISTS code_edges (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  file_id INTEGER NOT NULL,
  caller_kind TEXT NOT NULL, caller_import_id INT NULL, caller_uri TEXT NULL,
  caller_doc TEXT NULL, caller_path TEXT NULL, caller_bs INT NULL, caller_be INT NULL,
  caller_file TEXT NOT NULL, caller_sbs INTEGER NOT NULL, caller_sbe INTEGER NOT NULL,
  callee_kind TEXT NOT NULL, callee_import_id INT NULL, callee_uri TEXT NULL,
  callee_doc TEXT NULL, callee_path TEXT NULL, callee_bs INT NULL, callee_be INT NULL,
  callee_name TEXT NOT NULL,
  edge_kind TEXT NOT NULL, precision TEXT NOT NULL, provider TEXT NOT NULL,
  evidence TEXT NOT NULL, unresolved TEXT NULL
);
CREATE INDEX IF NOT EXISTS idx_edges_out ON code_edges(snapshot_id, caller_kind, caller_import_id, caller_uri, caller_path, caller_bs);
CREATE INDEX IF NOT EXISTS idx_edges_in ON code_edges(snapshot_id, callee_kind, callee_import_id, callee_uri, callee_path, callee_bs);
CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
  name, content='symbols', content_rowid='id', tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
  INSERT INTO symbols_fts(rowid, name) VALUES (new.id, new.name);
END;
CREATE TRIGGER IF NOT EXISTS symbols_ad AFTER DELETE ON symbols BEGIN
  INSERT INTO symbols_fts(symbols_fts, rowid, name) VALUES ('delete', old.id, old.name);
END;
CREATE TABLE IF NOT EXISTS semantic_imports (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  provider_name TEXT NOT NULL, provider_version TEXT NOT NULL,
  capability TEXT NOT NULL, project_root TEXT NOT NULL,
  text_encoding TEXT NOT NULL, artifact_id TEXT NOT NULL,
  source_revision TEXT NOT NULL, claimed_source_digest TEXT NOT NULL DEFAULT '',
  content_identity TEXT NOT NULL DEFAULT '',
  external_symbols TEXT NOT NULL DEFAULT '[]', verification TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS semantic_facts (
  id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
  import_id INTEGER NOT NULL, file_id INT NULL, rel_path TEXT NOT NULL,
  file_digest TEXT NOT NULL, symbol_uri TEXT NULL, display_name TEXT NULL,
  roles INTEGER NOT NULL, start_line INTEGER NOT NULL,
  start_col INTEGER NOT NULL, end_line INTEGER NOT NULL,
  end_col INTEGER NOT NULL, byte_start INTEGER NOT NULL,
  byte_end INTEGER NOT NULL, precision TEXT NOT NULL,
  relationships TEXT NOT NULL DEFAULT '[]', external INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_semfacts_lookup ON semantic_facts(snapshot_id, symbol_uri);
CREATE INDEX IF NOT EXISTS idx_semfacts_span ON semantic_facts(snapshot_id, rel_path, start_line);
";

/// Indexes added without a schema-version bump. Older binaries ignore them, so
/// the change is compatible in both directions. Fresh and migrated databases
/// get them inside their migration transaction; databases already at the
/// current version get them from `ensure_additive_indexes` on live open.
const ADDITIVE_INDEXES_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_files_snapshot_path ON files(snapshot_id, path);";

fn ensure_additive_indexes(conn: &Connection) -> Result<(), String> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_files_snapshot_path'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if present == 0 {
        conn.execute_batch(ADDITIVE_INDEXES_SQL)
            .map_err(|e| format!("cannot create file path index: {e}"))?;
    }
    Ok(())
}

/// Bring any database to the current schema in ONE explicit transaction:
/// empty files are built at current, v1 gains nonce backfill, v2 gains the
/// semantic tables, v3 gains the callee columns + the edges table, v4 gains
/// the occurrence table. Any failure rolls back: version marker and readable
/// data stay exactly as before. Newer versions are rejected, never guessed.
pub fn migrate(conn: &Connection) -> Result<(), String> {
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "database schema v{version} is newer than supported v{SCHEMA_VERSION}"
        ));
    }
    if version == SCHEMA_VERSION {
        return ensure_additive_indexes(conn);
    }
    conn.execute_batch("BEGIN IMMEDIATE;")
        .map_err(|e| e.to_string())?;
    let applied = (|| -> Result<(), String> {
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("schema migration failed: {e}"))?;
        conn.execute_batch(ADDITIVE_INDEXES_SQL)
            .map_err(|e| format!("schema migration failed: {e}"))?;
        if version == 1 {
            conn.execute_batch(
                "ALTER TABLE snapshots ADD COLUMN nonce TEXT NOT NULL DEFAULT '';
                 UPDATE snapshots SET nonce = 'v1-legacy-' || id WHERE nonce = '';",
            )
            .map_err(|e| format!("schema migration v1->v2 failed: {e}"))?;
        }
        // v3 databases have the old `calls` shape: add each callee column
        // only when missing, so fresh databases (built at v4 by the DDL
        // above) never hit a double-ALTER error.
        for col in [
            "callee_start_line",
            "callee_start_col",
            "callee_end_line",
            "callee_end_col",
            "callee_byte_start",
            "callee_byte_end",
            "tcallee_start_line",
            "tcallee_start_col",
            "tcallee_end_line",
            "tcallee_end_col",
            "tcallee_byte_start",
            "tcallee_byte_end",
        ] {
            if !column_exists(conn, "calls", col)? {
                conn.execute_batch(&format!("ALTER TABLE calls ADD COLUMN {col} INT NULL;"))
                    .map_err(|e| format!("schema migration v3->v4 failed: {e}"))?;
            }
        }
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    match applied {
        Ok(()) => conn.execute_batch("COMMIT;").map_err(|e| e.to_string()),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

fn column_exists(conn: &Connection, table: &str, col: &str) -> Result<bool, String> {
    let mut names = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    let found = names
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|n| n == col);
    Ok(found)
}

/// True when the bundled SQLite was compiled with FTS5. Pinned by test on
/// every platform; lexical search depends on it.
pub fn fts_available(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v == 1)
    .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: i64,
    pub repo: String,
    pub root: String,
    pub created_unix: u64,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
    pub untracked: u32,
    pub parser_version: u32,
    pub grammar_versions: String,
    pub config_fingerprint: String,
    pub pre_fingerprint: String,
    pub post_fingerprint: String,
    pub status_at_write: String,
    /// Unique revision identity: never reused across rebuilds.
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
    pub path: String,
    pub language: String,
    pub bytes: u64,
    pub digest: String,
    pub errors: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolRow {
    pub file_id: i64,
    pub kind: String,
    pub name: Option<String>,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub provider: String,
    pub grammar: String,
    pub language: String,
}

/// One C++ name occurrence (E7 Slice 2): the identifier, its role and its
/// span. The confidence is derived from the role, never stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceOccurrenceRow {
    pub file_id: i64,
    pub name: String,
    pub role: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub provider: String,
    pub grammar: String,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportRow {
    pub file_id: i64,
    pub text: Option<String>,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub provider: String,
    pub grammar: String,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallRow {
    pub file_id: i64,
    pub name: Option<String>,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    /// Callee expression range (`function` field); `None` for legacy rows.
    pub callee: Option<SpanCols>,
    /// Terminal identifier/property within `callee`; `None` if inseparable.
    pub tcallee: Option<SpanCols>,
    pub unresolved: u8,
    pub provider: String,
    pub grammar: String,
    pub language: String,
}

/// One 1-based span + byte range, shared by call callee columns.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanCols {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// One graph endpoint. There is deliberately NO `Unknown` variant: unknown
/// caller/target endpoints are stored as `None` with callsite/import
/// location evidence, never a fabricated key, and BFS never expands them.
#[derive(Debug, Clone, PartialEq)]
pub enum EdgeEndpoint {
    /// SCIP identity: producing import + symbol URI (+ document for locals).
    Scip {
        import_id: i64,
        symbol_uri: String,
        doc: Option<String>,
    },
    /// Snapshot-local syntax identity: file + byte range of the definition.
    SyntaxDef {
        path: String,
        byte_start: usize,
        byte_end: usize,
    },
}

/// One stored graph edge. `caller`/`callee` are `None` exactly when that
/// endpoint is unresolved; `evidence` always carries the source location.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRow {
    pub snapshot_id: i64,
    pub file_id: i64,
    pub caller: Option<EdgeEndpoint>,
    pub caller_path: String,
    pub caller_span_bytes: (usize, usize),
    pub callee: Option<EdgeEndpoint>,
    pub callee_name: String,
    pub edge_kind: String,
    pub precision: String,
    pub provider: String,
    pub evidence: String,
    pub unresolved: Option<String>,
}

/// One semantic provider import bound to a single snapshot. `artifact_id`
/// identifies the producer bytes; `source_revision` + `content_identity`
/// record which source state it was proven against; `claimed_source_digest`
/// is a user-supplied claim, never proof. See `semantic::verify_live`.
// Tasks 4/7 construct this; allow until then (staged plan, not dead design).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticImport {
    pub snapshot_id: i64,
    pub provider_name: String,
    pub provider_version: String,
    pub capability: String,
    pub project_root: String,
    pub text_encoding: String,
    pub artifact_id: String,
    pub source_revision: String,
    pub claimed_source_digest: String,
    pub content_identity: String,
    pub external_symbols: String,
    pub verification: String,
}

/// One stored semantic occurrence. `roles` is the full provider role bitset
/// verbatim (SCIP `0x1`..`0x40`); definition/reference filtering is a
/// query-time bitmask. `symbol_uri` is `None` for LSP results (no identity
/// is ever fabricated). `file_id` is `None` when the document has no row in
/// the syntax snapshot; `rel_path` + `file_digest` always locate the fact.
// Tasks 4/7 construct this; allow until then (staged plan, not dead design).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticFact {
    pub snapshot_id: i64,
    pub import_id: i64,
    pub file_id: Option<i64>,
    pub rel_path: String,
    pub file_digest: String,
    pub symbol_uri: Option<String>,
    pub display_name: Option<String>,
    pub roles: u32,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub precision: String,
    pub relationships: String,
    pub external: bool,
}

#[allow(dead_code)]
pub fn insert_semantic_import(conn: &Connection, import: &SemanticImport) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO semantic_imports (snapshot_id, provider_name, provider_version, capability, project_root, text_encoding, artifact_id, source_revision, claimed_source_digest, content_identity, external_symbols, verification) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            import.snapshot_id,
            import.provider_name,
            import.provider_version,
            import.capability,
            import.project_root,
            import.text_encoding,
            import.artifact_id,
            import.source_revision,
            import.claimed_source_digest,
            import.content_identity,
            import.external_symbols,
            import.verification,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

#[allow(dead_code)]
pub fn insert_semantic_facts(conn: &Connection, facts: &[SemanticFact]) -> Result<(), String> {
    for fact in facts {
        conn.execute(
            "INSERT INTO semantic_facts (snapshot_id, import_id, file_id, rel_path, file_digest, symbol_uri, display_name, roles, start_line, start_col, end_line, end_col, byte_start, byte_end, precision, relationships, external) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                fact.snapshot_id,
                fact.import_id,
                fact.file_id,
                fact.rel_path,
                fact.file_digest,
                fact.symbol_uri,
                fact.display_name,
                fact.roles as i64,
                fact.start_line as i64,
                fact.start_col as i64,
                fact.end_line as i64,
                fact.end_col as i64,
                fact.byte_start as i64,
                fact.byte_end as i64,
                fact.precision,
                fact.relationships,
                i64::from(fact.external),
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[allow(dead_code)]
fn semantic_fact_from_row(snapshot_id: i64, row: &rusqlite::Row) -> rusqlite::Result<SemanticFact> {
    Ok(SemanticFact {
        snapshot_id,
        import_id: row.get(0)?,
        file_id: row.get::<_, Option<i64>>(1)?,
        rel_path: row.get(2)?,
        file_digest: row.get(3)?,
        symbol_uri: row.get(4)?,
        display_name: row.get(5)?,
        roles: row.get::<_, i64>(6)? as u32,
        start_line: row.get::<_, i64>(7)? as u32,
        start_col: row.get::<_, i64>(8)? as u32,
        end_line: row.get::<_, i64>(9)? as u32,
        end_col: row.get::<_, i64>(10)? as u32,
        byte_start: row.get::<_, i64>(11)? as usize,
        byte_end: row.get::<_, i64>(12)? as usize,
        precision: row.get(13)?,
        relationships: row.get(14)?,
        external: row.get::<_, i64>(15)? != 0,
    })
}

/// All symbols of one snapshot, ordered by file then start byte.
/// Derivation input for M6 graph edges.
pub fn symbols_for(conn: &Connection, snapshot_id: i64) -> Result<Vec<SymbolRow>, String> {
    let mut statement = conn
        .prepare("SELECT file_id, kind, name, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language FROM symbols WHERE snapshot_id = ?1 ORDER BY file_id, byte_start")
        .map_err(|e| e.to_string())?;
    let mapped = statement
        .query_map([snapshot_id], |row| {
            Ok(SymbolRow {
                file_id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as u32,
                start_col: row.get::<_, i64>(4)? as u32,
                end_line: row.get::<_, i64>(5)? as u32,
                end_col: row.get::<_, i64>(6)? as u32,
                byte_start: row.get::<_, i64>(7)? as usize,
                byte_end: row.get::<_, i64>(8)? as usize,
                provider: row.get(9)?,
                grammar: row.get(10)?,
                language: row.get(11)?,
            })
        })
        .map_err(|e| e.to_string())?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// All imports of one snapshot, ordered by file then start byte.
pub fn imports_for(conn: &Connection, snapshot_id: i64) -> Result<Vec<ImportRow>, String> {
    let mut statement = conn
        .prepare("SELECT file_id, text, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language FROM imports WHERE snapshot_id = ?1 ORDER BY file_id, byte_start")
        .map_err(|e| e.to_string())?;
    let mapped = statement
        .query_map([snapshot_id], |row| {
            Ok(ImportRow {
                file_id: row.get(0)?,
                text: row.get(1)?,
                start_line: row.get::<_, i64>(2)? as u32,
                start_col: row.get::<_, i64>(3)? as u32,
                end_line: row.get::<_, i64>(4)? as u32,
                end_col: row.get::<_, i64>(5)? as u32,
                byte_start: row.get::<_, i64>(6)? as usize,
                byte_end: row.get::<_, i64>(7)? as usize,
                provider: row.get(8)?,
                grammar: row.get(9)?,
                language: row.get(10)?,
            })
        })
        .map_err(|e| e.to_string())?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// All semantic facts of one import in one snapshot, ordered by path/line.
/// Derivation input for M6 semantic-backed edges.
pub fn semantic_facts_of_import(
    conn: &Connection,
    snapshot_id: i64,
    import_id: i64,
) -> Result<Vec<SemanticFact>, String> {
    let mut statement = conn
        .prepare("SELECT import_id, file_id, rel_path, file_digest, symbol_uri, display_name, roles, start_line, start_col, end_line, end_col, byte_start, byte_end, precision, relationships, external FROM semantic_facts WHERE snapshot_id = ?1 AND import_id = ?2 ORDER BY rel_path, start_line")
        .map_err(|e| e.to_string())?;
    let mapped = statement
        .query_map(params![snapshot_id, import_id], |row| {
            semantic_fact_from_row(snapshot_id, row)
        })
        .map_err(|e| e.to_string())?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// All symbols of one file in one snapshot, ordered by start byte.
/// Positional anchor resolution for M6 edge queries.
pub fn symbols_at(
    conn: &Connection,
    snapshot_id: i64,
    rel_path: &str,
) -> Result<Vec<SymbolRow>, String> {
    let mut statement = conn
        .prepare("SELECT s.file_id, s.kind, s.name, s.start_line, s.start_col, s.end_line, s.end_col, s.byte_start, s.byte_end, s.provider, s.grammar, s.language FROM symbols AS s JOIN files AS f ON f.id = s.file_id AND f.snapshot_id = s.snapshot_id WHERE s.snapshot_id = ?1 AND f.path = ?2 ORDER BY s.byte_start, s.byte_end")
        .map_err(|e| e.to_string())?;
    let mapped = statement
        .query_map(params![snapshot_id, rel_path], |row| {
            Ok(SymbolRow {
                file_id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as u32,
                start_col: row.get::<_, i64>(4)? as u32,
                end_line: row.get::<_, i64>(5)? as u32,
                end_col: row.get::<_, i64>(6)? as u32,
                byte_start: row.get::<_, i64>(7)? as usize,
                byte_end: row.get::<_, i64>(8)? as usize,
                provider: row.get(9)?,
                grammar: row.get(10)?,
                language: row.get(11)?,
            })
        })
        .map_err(|e| e.to_string())?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// All stored facts for one symbol in one snapshot, ordered by path/line.
#[allow(dead_code)]
pub fn semantic_facts_for(
    conn: &Connection,
    snapshot_id: i64,
    symbol_uri: &str,
) -> Result<Vec<SemanticFact>, String> {
    let mut statement = conn
        .prepare("SELECT import_id, file_id, rel_path, file_digest, symbol_uri, display_name, roles, start_line, start_col, end_line, end_col, byte_start, byte_end, precision, relationships, external FROM semantic_facts WHERE snapshot_id = ?1 AND symbol_uri = ?2 ORDER BY rel_path, start_line")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![snapshot_id, symbol_uri], |row| {
            semantic_fact_from_row(snapshot_id, row)
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// At most two identities: sufficient to prove uniqueness without loading an
/// import's reference corpus. Source binding predicates match owner bridging.
pub fn semantic_owner_uris(
    conn: &Connection,
    snapshot_id: i64,
    import_id: i64,
    owner: &SymbolRow,
    path: &str,
    digest: &str,
) -> Result<Vec<String>, String> {
    // Restrict owner lookup to this file instead of scanning the snapshot.
    let mut stmt=conn.prepare("SELECT DISTINCT symbol_uri FROM semantic_facts INDEXED BY idx_semfacts_span WHERE snapshot_id=?1 AND import_id=?2 AND rel_path=?3 AND display_name=?4 AND (roles & 1)!=0 AND symbol_uri IS NOT NULL AND file_digest=?5 AND (file_id IS NULL OR file_id=?6) AND byte_start>=?7 AND byte_end<=?8 ORDER BY symbol_uri LIMIT 2").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map(
            params![
                snapshot_id,
                import_id,
                path,
                owner.name,
                digest,
                owner.file_id,
                owner.byte_start as i64,
                owner.byte_end as i64
            ],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

pub fn semantic_definitions_for(
    conn: &Connection,
    snapshot_id: i64,
    import_id: i64,
    symbol_uri: &str,
    doc: Option<&str>,
    limit: i64,
) -> Result<Vec<SemanticFact>, String> {
    let mut stmt=conn.prepare("SELECT import_id, file_id, rel_path, file_digest, symbol_uri, display_name, roles, start_line, start_col, end_line, end_col, byte_start, byte_end, precision, relationships, external FROM semantic_facts WHERE snapshot_id=?1 AND import_id=?2 AND symbol_uri=?3 AND (roles & 1)!=0 AND (?4 IS NULL OR rel_path=?4) ORDER BY rel_path,byte_start,byte_end,id LIMIT ?5").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map(
            params![snapshot_id, import_id, symbol_uri, doc, limit],
            |r| semantic_fact_from_row(snapshot_id, r),
        )
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

pub fn file_identity(
    conn: &Connection,
    snapshot_id: i64,
    path: &str,
) -> Result<Option<(i64, String)>, String> {
    let mut stmt = conn
        .prepare("SELECT id,digest FROM files WHERE snapshot_id=?1 AND path=?2 ORDER BY id LIMIT 2")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![snapshot_id, path], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?;
    let mut rows = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    if rows.len() > 1 {
        return Err("ambiguous file identity".to_string());
    }
    Ok(rows.pop())
}

/// All stored facts for one path in one snapshot, ordered by start position.
/// The query-time span lookup for Go to Definition / Find References.
pub fn semantic_facts_at(
    conn: &Connection,
    snapshot_id: i64,
    rel_path: &str,
) -> Result<Vec<SemanticFact>, String> {
    let mut statement = conn
        .prepare("SELECT import_id, file_id, rel_path, file_digest, symbol_uri, display_name, roles, start_line, start_col, end_line, end_col, byte_start, byte_end, precision, relationships, external FROM semantic_facts WHERE snapshot_id = ?1 AND rel_path = ?2 ORDER BY start_line, start_col")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![snapshot_id, rel_path], |row| {
            semantic_fact_from_row(snapshot_id, row)
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn semantic_import_from_row(
    id: i64,
    snapshot_id: i64,
    row: &rusqlite::Row,
    base: usize,
) -> rusqlite::Result<(i64, SemanticImport)> {
    Ok((
        id,
        SemanticImport {
            snapshot_id,
            provider_name: row.get(base)?,
            provider_version: row.get(base + 1)?,
            capability: row.get(base + 2)?,
            project_root: row.get(base + 3)?,
            text_encoding: row.get(base + 4)?,
            artifact_id: row.get(base + 5)?,
            source_revision: row.get(base + 6)?,
            claimed_source_digest: row.get(base + 7)?,
            content_identity: row.get(base + 8)?,
            external_symbols: row.get(base + 9)?,
            verification: row.get(base + 10)?,
        },
    ))
}

const SEMANTIC_IMPORT_COLS: &str = "id, provider_name, provider_version, capability, project_root, text_encoding, artifact_id, source_revision, claimed_source_digest, content_identity, external_symbols, verification";

/// All imports bound to one snapshot, ordered by id.
pub fn semantic_imports_for(
    conn: &Connection,
    snapshot_id: i64,
) -> Result<Vec<(i64, SemanticImport)>, String> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SEMANTIC_IMPORT_COLS} FROM semantic_imports WHERE snapshot_id = ?1 ORDER BY id"
        ))
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([snapshot_id], |row| {
            let id: i64 = row.get(0)?;
            semantic_import_from_row(id, snapshot_id, row, 1)
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Earlier imports on `snapshot_id` that carry the same artifact digest as
/// `import_id`, oldest first. Identical bytes on one snapshot are one import.
pub fn semantic_import_duplicates(
    conn: &Connection,
    snapshot_id: i64,
    import_id: i64,
) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT older.id FROM semantic_imports AS newer
             JOIN semantic_imports AS older
               ON older.snapshot_id = newer.snapshot_id
              AND older.artifact_id = newer.artifact_id
              AND older.id < newer.id
             WHERE newer.id = ?1 AND newer.snapshot_id = ?2 AND newer.artifact_id <> ''
             ORDER BY older.id",
        )
        .map_err(|e| e.to_string())?;
    let ids = stmt
        .query_map(params![import_id, snapshot_id], |row| row.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(ids)
}

/// Remove one import row and its facts from `snapshot_id`. Edges are not
/// touched: every caller re-derives them afterwards.
pub fn delete_semantic_import(
    conn: &Connection,
    snapshot_id: i64,
    import_id: i64,
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM semantic_facts WHERE snapshot_id = ?1 AND import_id = ?2",
        params![snapshot_id, import_id],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM semantic_imports WHERE snapshot_id = ?1 AND id = ?2",
        params![snapshot_id, import_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Revalidate `keep` with `fresh`, an identical import just written on the same
/// snapshot: `keep` keeps its id and takes `fresh`'s facts. A claim that `keep`
/// carries survives when `fresh` brings none.
pub fn replace_semantic_import(
    conn: &Connection,
    snapshot_id: i64,
    keep: i64,
    fresh: i64,
) -> Result<(), String> {
    conn.execute(
        "UPDATE semantic_imports SET claimed_source_digest =
           (SELECT claimed_source_digest FROM semantic_imports WHERE id = ?1)
         WHERE id = ?2 AND claimed_source_digest = ''",
        params![keep, fresh],
    )
    .map_err(|e| e.to_string())?;
    delete_semantic_import(conn, snapshot_id, keep)?;
    conn.execute(
        "UPDATE semantic_facts SET import_id = ?1 WHERE snapshot_id = ?2 AND import_id = ?3",
        params![keep, snapshot_id, fresh],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE semantic_imports SET id = ?1 WHERE snapshot_id = ?2 AND id = ?3",
        params![keep, snapshot_id, fresh],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// One import row by id, any snapshot.
pub fn semantic_import_by_id(
    conn: &Connection,
    import_id: i64,
) -> Result<Option<(i64, SemanticImport)>, String> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT snapshot_id, {SEMANTIC_IMPORT_COLS} FROM semantic_imports WHERE id = ?1"
        ))
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([import_id]).map_err(|e| e.to_string())?;
    match rows.next().map_err(|e| e.to_string())? {
        Some(row) => {
            let snapshot_id: i64 = row.get(0).map_err(|e: rusqlite::Error| e.to_string())?;
            let id: i64 = row.get(1).map_err(|e: rusqlite::Error| e.to_string())?;
            let pair = semantic_import_from_row(id, snapshot_id, row, 2)
                .map_err(|e: rusqlite::Error| e.to_string())?;
            Ok(Some(pair))
        }
        None => Ok(None),
    }
}

/// Raw `meta` access. Absent keys read as `None`, never as invented values.
#[allow(dead_code)]
pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    let mut statement = conn
        .prepare("SELECT value FROM meta WHERE key = ?1")
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([key]).map_err(|e| e.to_string())?;
    match rows.next().map_err(|e| e.to_string())? {
        Some(row) => Ok(Some(
            row.get(0).map_err(|e: rusqlite::Error| e.to_string())?,
        )),
        None => Ok(None),
    }
}

#[allow(dead_code)]
pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Writer-coordination counter: bumped in-txn on every committed semantic
/// import. Builds record it at staging start; promote aborts when it moved.
/// Missing key means zero imports ever committed.
#[allow(dead_code)]
pub fn semantic_generation(conn: &Connection) -> Result<i64, String> {
    match meta_get(conn, "semantic_generation")? {
        Some(v) => v
            .parse::<i64>()
            .map_err(|e| format!("bad semantic_generation: {e}")),
        None => Ok(0),
    }
}

// Bump when persisted relationship proof rules change. This is derivation
// compatibility metadata, independent of the SQLite schema version.
const GRAPH_PROOF_REVISION: &str = "m6-task7-source-proof-2";

fn graph_proof_token(conn: &Connection, snapshot_id: i64) -> Result<String, String> {
    let nonce: String = conn
        .query_row(
            "SELECT nonce FROM snapshots WHERE id=?1",
            [snapshot_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "{GRAPH_PROOF_REVISION}:{nonce}:{}",
        semantic_generation(conn)?
    ))
}

pub fn graph_proof_current(conn: &Connection, snapshot_id: i64) -> Result<bool, String> {
    Ok(meta_get(conn, "graph_proof")?.as_deref()
        == Some(graph_proof_token(conn, snapshot_id)?.as_str()))
}

/// Call only after the entire graph is derived, or after updating an import
/// whose preexisting graph was already certified, inside the writer txn.
pub fn mark_graph_proof(conn: &Connection, snapshot_id: i64) -> Result<(), String> {
    meta_set(conn, "graph_proof", &graph_proof_token(conn, snapshot_id)?)
}

pub fn require_graph_proof(conn: &Connection, snapshot_id: i64) -> Result<(), String> {
    if graph_proof_current(conn, snapshot_id)? {
        Ok(())
    } else {
        Err("graph-proof-incompatible: validate stored relationships with semantic import or index --full before graph queries".to_string())
    }
}

/// Content identity of one snapshot: digest over sorted `(path, digest)`
/// pairs. Snapshot nonces prove rebuild identity; this proves the bytes.
/// Compared independently of git HEAD/dirty/untracked (Task 6 rule).
#[allow(dead_code)]
pub fn content_identity(conn: &Connection, snapshot_id: i64) -> Result<String, String> {
    let mut statement = conn
        .prepare("SELECT path, digest FROM files WHERE snapshot_id = ?1 ORDER BY path")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([snapshot_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut material = Vec::new();
    for row in rows {
        let (path, digest) = row.map_err(|e| e.to_string())?;
        material.extend_from_slice(path.as_bytes());
        material.push(0);
        material.extend_from_slice(digest.as_bytes());
        material.push(b'\n');
    }
    Ok(content_digest(&material))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub snapshot_id: i64,
    pub path: String,
    pub kind: String,
    pub name: Option<String>,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub provider: String,
    pub grammar: String,
    pub language: String,
}

pub fn insert_snapshot(conn: &Connection, snapshot: &Snapshot) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO snapshots (repo, root, created_unix, head, branch, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write, nonce) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            snapshot.repo,
            snapshot.root,
            snapshot.created_unix as i64,
            snapshot.head,
            snapshot.branch,
            i64::from(snapshot.dirty),
            snapshot.untracked as i64,
            snapshot.parser_version as i64,
            snapshot.grammar_versions,
            snapshot.config_fingerprint,
            snapshot.pre_fingerprint,
            snapshot.post_fingerprint,
            snapshot.status_at_write,
            snapshot.nonce,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

/// Fresh revision identities: process id, nanosecond clock, and a counter.
/// Uniqueness is what lets tests and envelopes distinguish revisions.
pub fn new_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

/// Record the post-build fingerprint and verdict on a snapshot row.
pub fn finish_snapshot(conn: &Connection, id: i64, post: &str, status: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE snapshots SET post_fingerprint = ?1, status_at_write = ?2 WHERE id = ?3",
            params![post, status, id],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("snapshot row missing for finish".to_string());
    }
    Ok(())
}

/// Build-time fingerprint of the live database: latest snapshot nonce plus
/// the semantic writer-generation counter. Recorded when staging starts;
/// promotion aborts unless both still match, so a `semantic import` that
/// commits mid-build is never silently wiped by a stale staging copy.
/// Extends the M4 nonce-guard pattern — no lock files, no new framework.
#[derive(Debug, Clone, PartialEq)]
pub struct PromoteBase {
    pub nonce: String,
    pub generation: i64,
}

/// Read the current live base: latest snapshot nonce (empty when no
/// snapshot exists yet) plus the semantic generation counter.
pub fn live_base(conn: &Connection) -> Result<PromoteBase, String> {
    let nonce: Option<String> = conn
        .query_row(
            "SELECT nonce FROM snapshots ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(PromoteBase {
        nonce: nonce.unwrap_or_default(),
        generation: semantic_generation(conn)?,
    })
}

/// Copy one staging snapshot into the live database inside a single
/// transaction: readers see the old snapshot or the new one, never a mix.
/// FTS rows follow automatically through the insert/delete triggers. The
/// staging build must have been based on `base`: any live advance (new
/// snapshot or committed semantic import) aborts with a `semantic-changed`
/// error and the live database untouched. On any other failure the live
/// snapshot is rolled back intact.
pub fn promote_staging(
    live: &Connection,
    staging: &Path,
    base: &PromoteBase,
) -> Result<(), String> {
    let escaped = staging.to_string_lossy().replace('\'', "''");
    live.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS src"))
        .map_err(|e| format!("cannot attach staging database: {e}"))?;
    let copied = (|| -> Result<(), String> {
        live.execute_batch("BEGIN IMMEDIATE;")
            .map_err(|e| e.to_string())?;
        let current = live_base(live)?;
        if current != *base {
            live.execute_batch("ROLLBACK;").map_err(|e| e.to_string())?;
            return Err("semantic-changed: live database advanced during build".to_string());
        }
        live.execute_batch(
            &format!("DELETE FROM symbols;
             DELETE FROM imports;
             DELETE FROM calls;
             DELETE FROM reference_occurrences;
             DELETE FROM files;
             DELETE FROM snapshots;
             DELETE FROM semantic_facts;
             DELETE FROM semantic_imports;
             DELETE FROM code_edges;
             INSERT INTO snapshots SELECT * FROM src.snapshots;
             INSERT INTO files SELECT * FROM src.files;
             INSERT INTO symbols SELECT * FROM src.symbols;
             INSERT INTO imports SELECT * FROM src.imports;
             INSERT INTO calls SELECT * FROM src.calls;
             INSERT INTO reference_occurrences SELECT * FROM src.reference_occurrences;
             INSERT INTO semantic_imports SELECT * FROM src.semantic_imports;
             INSERT INTO semantic_facts SELECT * FROM src.semantic_facts;
             INSERT INTO code_edges SELECT * FROM src.code_edges;
             DELETE FROM meta WHERE key='graph_proof';
             INSERT INTO meta(key,value)
             SELECT 'graph_proof', '{GRAPH_PROOF_REVISION}:' || s.nonce || ':' || COALESCE((SELECT value FROM meta WHERE key='semantic_generation'),'0')
             FROM src.snapshots s WHERE s.id=(SELECT MAX(id) FROM src.snapshots)
             AND (SELECT value FROM src.meta WHERE key='graph_proof') =
                 '{GRAPH_PROOF_REVISION}:' || s.nonce || ':' || COALESCE((SELECT value FROM src.meta WHERE key='semantic_generation'),'0');
             COMMIT;"),
        )
        .map_err(|e| format!("promote failed, live snapshot intact: {e}"))?;
        Ok(())
    })();
    if copied.is_err() {
        let _ = live.execute_batch("ROLLBACK;");
        let _ = live.execute_batch("DETACH DATABASE src;");
        return copied;
    }
    live.execute_batch("DETACH DATABASE src")
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Attach another database file as `src` for set-based copies. Same
/// single-quote escaping as `promote_staging`.
pub fn attach_src(conn: &Connection, path: &Path) -> Result<(), String> {
    let escaped = path.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS src"))
        .map_err(|e| format!("cannot attach source database: {e}"))?;
    Ok(())
}

pub fn detach_src(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("DETACH DATABASE src")
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Totals over bulk-copied facts, so callers keep definition/import/call/
/// error counters without extra aggregation queries.
pub struct CopiedTotals {
    pub symbols: u64,
    pub imports: u64,
    pub calls: u64,
    pub errors: u32,
}

/// Copy one files row from the attached `src` database into this one.
/// Returns the new rowid, or `None` when no source row matches id, path,
/// AND digest (vanished row or a concurrent rebuild reusing rowids for
/// different content) — callers then ingest the file from current source
/// instead of reusing. The triple predicate is what makes reuse unable to
/// mix two revisions: a rowid alone is recycled by every rebuild. Single
/// `INSERT..SELECT` plus `changes()`: atomic, no TOCTOU.
pub fn copy_file_row(
    staging: &Connection,
    new_snapshot_id: i64,
    old_snapshot_id: i64,
    old_file_id: i64,
    expected_path: &str,
    expected_digest: &str,
) -> Result<Option<i64>, String> {
    staging
        .execute(
            "INSERT INTO files (snapshot_id, path, language, bytes, digest, errors)
             SELECT ?1, path, language, bytes, digest, errors FROM src.files
             WHERE snapshot_id = ?2 AND id = ?3 AND path = ?4 AND digest = ?5",
            params![
                new_snapshot_id,
                old_snapshot_id,
                old_file_id,
                expected_path,
                expected_digest
            ],
        )
        .map_err(|e| e.to_string())?;
    if staging.changes() != 1 {
        return Ok(None);
    }
    Ok(Some(staging.last_insert_rowid()))
}

/// Session-local old→new file id map for bulk fact copies. A `TEMP` table:
/// invisible to other connections, gone when this one closes, no schema
/// migration. Per-file child `SELECT`s cannot use the `file_id` column
/// efficiently (no index by design), so the map turns thousands of filtered
/// copies into three joined bulk statements.
pub fn create_file_map(staging: &Connection) -> Result<(), String> {
    staging
        .execute_batch(
            "CREATE TEMP TABLE fmap (old_id INTEGER PRIMARY KEY, new_id INTEGER NOT NULL)",
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn record_file_map(staging: &Connection, old_id: i64, new_id: i64) -> Result<(), String> {
    staging
        .execute(
            "INSERT INTO fmap (old_id, new_id) VALUES (?1, ?2)",
            params![old_id, new_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Bulk-copy every mapped file's symbols/imports/calls/occurrences in four
/// joined statements. Column lists mirror the matching `insert_*` functions.
/// Rejected alternative: per-file filtered child copies — one full snapshot
/// scan per file without a `file_id` index (quadratic at MMSP scale).
pub fn copy_mapped_facts(
    staging: &Connection,
    new_snapshot_id: i64,
    old_snapshot_id: i64,
) -> Result<CopiedTotals, String> {
    // The occurrence copy has no counter of its own: no document reports it.
    let mut counts = [0u64; 4];
    for (i, (table, extra)) in [
        ("symbols", "kind, name, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language"),
        ("imports", "text, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language"),
        ("calls", "name, start_line, start_col, end_line, end_col, byte_start, byte_end, callee_start_line, callee_start_col, callee_end_line, callee_end_col, callee_byte_start, callee_byte_end, tcallee_start_line, tcallee_start_col, tcallee_end_line, tcallee_end_col, tcallee_byte_start, tcallee_byte_end, unresolved, provider, grammar, language"),
        ("reference_occurrences", "name, role, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language"),
    ]
    .iter()
    .enumerate()
    {
        staging
            .execute(
                &format!(
                    "INSERT INTO {table} (snapshot_id, file_id, {extra})
                     SELECT ?1, m.new_id, {extra} FROM src.{table} AS s
                     JOIN fmap AS m ON m.old_id = s.file_id
                     WHERE s.snapshot_id = ?2"
                ),
                params![new_snapshot_id, old_snapshot_id],
            )
            .map_err(|e| e.to_string())?;
        counts[i] = staging.changes();
    }
    let errors: i64 = staging
        .query_row(
            "SELECT COALESCE(SUM(f.errors), 0) FROM src.files AS f
             JOIN fmap AS m ON m.old_id = f.id
             WHERE f.snapshot_id = ?1",
            params![old_snapshot_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(CopiedTotals {
        symbols: counts[0],
        imports: counts[1],
        calls: counts[2],
        errors: errors as u32,
    })
}

/// Totals over carried semantic facts, mirroring `CopiedTotals`.
/// Reported to Tier B validation; the frozen M4 index JSON is untouched.
#[allow(dead_code)]
pub struct CarriedTotals {
    pub imports: u64,
    pub facts: u64,
}

/// Carry semantic imports + per-file-verified facts from the attached `src`
/// (live) database into the staging snapshot. A fact is copied only when
/// the new snapshot holds a `files` row with the SAME path AND digest —
/// the triple-predicate discipline of `copy_file_row`, applied by join.
/// Facts with `file_id IS NULL` (unindexed documents) have no row to join:
/// they carry only when `disk_digest` reproduces their stored digest from
/// raw source bytes, and stay NULL (never fabricated, never linked).
/// Anything else drops explicitly. Import rows are always rebound to the
/// new snapshot (new `snapshot_id` and fresh `source_revision`) with the
/// ORIGINAL `content_identity` kept: carry preserves locatability only —
/// `verify_live` still reports `Stale` at query time whenever the bytes
/// moved. Never upgrades verification.
pub fn carry_semantic_facts(
    staging: &Connection,
    new_snapshot_id: i64,
    old_snapshot_id: i64,
    new_source_revision: &str,
    disk_digest: &dyn Fn(&str) -> Option<String>,
) -> Result<CarriedTotals, String> {
    let mut statement = staging
        .prepare("SELECT id, provider_name, provider_version, capability, project_root, text_encoding, artifact_id, source_revision, claimed_source_digest, content_identity, external_symbols, verification FROM src.semantic_imports WHERE snapshot_id = ?1 ORDER BY id")
        .map_err(|e| e.to_string())?;
    let old_imports = statement
        .query_map([old_snapshot_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);
    staging
        .execute_batch("CREATE TEMP TABLE sem_import_map (old_id INTEGER PRIMARY KEY, new_id INTEGER NOT NULL)")
        .map_err(|e| e.to_string())?;
    let mut imports = 0u64;
    for (
        old_id,
        provider_name,
        provider_version,
        capability,
        project_root,
        text_encoding,
        artifact_id,
        _,
        claimed,
        content_identity,
        external_symbols,
        verification,
    ) in &old_imports
    {
        staging
            .execute(
                "INSERT INTO semantic_imports (snapshot_id, provider_name, provider_version, capability, project_root, text_encoding, artifact_id, source_revision, claimed_source_digest, content_identity, external_symbols, verification) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![new_snapshot_id, provider_name, provider_version, capability, project_root, text_encoding, artifact_id, new_source_revision, claimed, content_identity, external_symbols, verification],
            )
            .map_err(|e| e.to_string())?;
        let new_id = staging.last_insert_rowid();
        staging
            .execute(
                "INSERT INTO sem_import_map (old_id, new_id) VALUES (?1, ?2)",
                params![old_id, new_id],
            )
            .map_err(|e| e.to_string())?;
        imports += 1;
    }
    staging
        .execute(
            "INSERT INTO semantic_facts (snapshot_id, import_id, file_id, rel_path, file_digest, symbol_uri, display_name, roles, start_line, start_col, end_line, end_col, byte_start, byte_end, precision, relationships, external)
             SELECT ?1, map.new_id, nf.id, f.rel_path, f.file_digest, f.symbol_uri, f.display_name, f.roles, f.start_line, f.start_col, f.end_line, f.end_col, f.byte_start, f.byte_end, f.precision, f.relationships, f.external
             FROM src.semantic_facts AS f
             JOIN sem_import_map AS map ON map.old_id = f.import_id
             JOIN files AS nf ON nf.snapshot_id = ?1 AND nf.path = f.rel_path AND nf.digest = f.file_digest
             WHERE f.snapshot_id = ?2",
            params![new_snapshot_id, old_snapshot_id],
        )
        .map_err(|e| e.to_string())?;
    let facts = staging.changes();
    // Phase 2: NULL facts the join could not reach (no syntax row on either
    // side). Each must re-prove its stored digest against raw source bytes;
    // unmatched or unreadable files drop with the fact, explicitly.
    let mut null_facts: Vec<(i64, String, String)> = Vec::new();
    {
        let mut statement = staging
            .prepare("SELECT f.import_id, f.rel_path, f.file_digest FROM src.semantic_facts AS f WHERE f.snapshot_id = ?1 AND f.file_id IS NULL AND NOT EXISTS (SELECT 1 FROM files AS nf WHERE nf.snapshot_id = ?2 AND nf.path = f.rel_path AND nf.digest = f.file_digest)")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![old_snapshot_id, new_snapshot_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            null_facts.push(row.map_err(|e| e.to_string())?);
        }
    }
    let mut null_carried = 0u64;
    let mut seen_keys = std::collections::BTreeSet::new();
    for (old_import_id, rel_path, file_digest) in &null_facts {
        // One copy per (import, path): the INSERT below copies every NULL
        // fact of that key at once, so repeated keys must not re-run it.
        if !seen_keys.insert((*old_import_id, rel_path.clone())) {
            continue;
        }
        match disk_digest(rel_path) {
            Some(digest) if digest == *file_digest => {
                let new_import_id: i64 = staging
                    .query_row(
                        "SELECT new_id FROM sem_import_map WHERE old_id = ?1",
                        [old_import_id],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                staging
                    .execute(
                        "INSERT INTO semantic_facts (snapshot_id, import_id, file_id, rel_path, file_digest, symbol_uri, display_name, roles, start_line, start_col, end_line, end_col, byte_start, byte_end, precision, relationships, external)
                         SELECT ?1, ?2, NULL, f.rel_path, f.file_digest, f.symbol_uri, f.display_name, f.roles, f.start_line, f.start_col, f.end_line, f.end_col, f.byte_start, f.byte_end, f.precision, f.relationships, f.external
                         FROM src.semantic_facts AS f WHERE f.snapshot_id = ?3 AND f.import_id = ?4 AND f.rel_path = ?5 AND f.file_id IS NULL",
                        params![new_snapshot_id, new_import_id, old_snapshot_id, old_import_id, rel_path],
                    )
                    .map_err(|e| e.to_string())?;
                null_carried += staging.changes();
            }
            _ => {}
        }
    }
    staging
        .execute_batch("DROP TABLE sem_import_map")
        .map_err(|e| e.to_string())?;
    Ok(CarriedTotals {
        imports,
        facts: facts + null_carried,
    })
}

pub fn insert_file(conn: &Connection, snapshot_id: i64, file: &FileRow) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO files (snapshot_id, path, language, bytes, digest, errors) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            snapshot_id,
            file.path,
            file.language,
            file.bytes as i64,
            file.digest,
            file.errors as i64,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub fn insert_symbol(
    conn: &Connection,
    snapshot_id: i64,
    symbol: &SymbolRow,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO symbols (snapshot_id, file_id, kind, name, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            snapshot_id,
            symbol.file_id,
            symbol.kind,
            symbol.name,
            symbol.start_line as i64,
            symbol.start_col as i64,
            symbol.end_line as i64,
            symbol.end_col as i64,
            symbol.byte_start as i64,
            symbol.byte_end as i64,
            symbol.provider,
            symbol.grammar,
            symbol.language,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn insert_reference_occurrence(
    conn: &Connection,
    snapshot_id: i64,
    occurrence: &ReferenceOccurrenceRow,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO reference_occurrences (snapshot_id, file_id, name, role, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            snapshot_id,
            occurrence.file_id,
            occurrence.name,
            occurrence.role,
            occurrence.start_line as i64,
            occurrence.start_col as i64,
            occurrence.end_line as i64,
            occurrence.end_col as i64,
            occurrence.byte_start as i64,
            occurrence.byte_end as i64,
            occurrence.provider,
            occurrence.grammar,
            occurrence.language,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn insert_import(
    conn: &Connection,
    snapshot_id: i64,
    import: &ImportRow,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO imports (snapshot_id, file_id, text, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            snapshot_id,
            import.file_id,
            import.text,
            import.start_line as i64,
            import.start_col as i64,
            import.end_line as i64,
            import.end_col as i64,
            import.byte_start as i64,
            import.byte_end as i64,
            import.provider,
            import.grammar,
            import.language,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn insert_call(conn: &Connection, snapshot_id: i64, call: &CallRow) -> Result<(), String> {
    let c = span_opt6(&call.callee);
    let t = span_opt6(&call.tcallee);
    conn.execute(
        "INSERT INTO calls (snapshot_id, file_id, name, start_line, start_col, end_line, end_col, byte_start, byte_end, callee_start_line, callee_start_col, callee_end_line, callee_end_col, callee_byte_start, callee_byte_end, tcallee_start_line, tcallee_start_col, tcallee_end_line, tcallee_end_col, tcallee_byte_start, tcallee_byte_end, unresolved, provider, grammar, language) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
        params![
            snapshot_id,
            call.file_id,
            call.name,
            call.start_line as i64,
            call.start_col as i64,
            call.end_line as i64,
            call.end_col as i64,
            call.byte_start as i64,
            call.byte_end as i64,
            c[0], c[1], c[2], c[3], c[4], c[5],
            t[0], t[1], t[2], t[3], t[4], t[5],
            i64::from(call.unresolved),
            call.provider,
            call.grammar,
            call.language,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn span_opt6(span: &Option<SpanCols>) -> [Option<i64>; 6] {
    match span {
        Some(s) => [
            Some(s.start_line as i64),
            Some(s.start_col as i64),
            Some(s.end_line as i64),
            Some(s.end_col as i64),
            Some(s.byte_start as i64),
            Some(s.byte_end as i64),
        ],
        None => [None, None, None, None, None, None],
    }
}

fn read_span(row: &rusqlite::Row, base: usize) -> Result<Option<SpanCols>, rusqlite::Error> {
    let sl: Option<i64> = row.get(base)?;
    let sc: Option<i64> = row.get(base + 1)?;
    let el: Option<i64> = row.get(base + 2)?;
    let ec: Option<i64> = row.get(base + 3)?;
    let bs: Option<i64> = row.get(base + 4)?;
    let be: Option<i64> = row.get(base + 5)?;
    match (sl, sc, el, ec, bs, be) {
        (Some(sl), Some(sc), Some(el), Some(ec), Some(bs), Some(be)) => Ok(Some(SpanCols {
            start_line: sl as u32,
            start_col: sc as u32,
            end_line: el as u32,
            end_col: ec as u32,
            byte_start: bs as usize,
            byte_end: be as usize,
        })),
        _ => Ok(None),
    }
}

/// All calls of one snapshot, including callee spans (`None` for legacy).
pub fn calls_for(conn: &Connection, snapshot_id: i64) -> Result<Vec<CallRow>, String> {
    let mut statement = conn
        .prepare("SELECT file_id, name, start_line, start_col, end_line, end_col, byte_start, byte_end, callee_start_line, callee_start_col, callee_end_line, callee_end_col, callee_byte_start, callee_byte_end, tcallee_start_line, tcallee_start_col, tcallee_end_line, tcallee_end_col, tcallee_byte_start, tcallee_byte_end, unresolved, provider, grammar, language FROM calls WHERE snapshot_id = ?1 ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([snapshot_id], |row| {
            Ok(CallRow {
                file_id: row.get(0)?,
                name: row.get(1)?,
                start_line: row.get::<_, i64>(2)? as u32,
                start_col: row.get::<_, i64>(3)? as u32,
                end_line: row.get::<_, i64>(4)? as u32,
                end_col: row.get::<_, i64>(5)? as u32,
                byte_start: row.get::<_, i64>(6)? as usize,
                byte_end: row.get::<_, i64>(7)? as usize,
                callee: read_span(row, 8)?,
                tcallee: read_span(row, 14)?,
                unresolved: row.get::<_, i64>(20)? as u8,
                provider: row.get(21)?,
                grammar: row.get(22)?,
                language: row.get(23)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

struct EndpointParts<'a> {
    kind: &'static str,
    import_id: Option<i64>,
    uri: Option<&'a str>,
    doc: Option<&'a str>,
    path: Option<&'a str>,
    bs: Option<i64>,
    be: Option<i64>,
}

fn endpoint_parts(endpoint: &Option<EdgeEndpoint>) -> EndpointParts<'_> {
    match endpoint {
        Some(EdgeEndpoint::Scip {
            import_id,
            symbol_uri,
            doc,
        }) => EndpointParts {
            kind: "scip",
            import_id: Some(*import_id),
            uri: Some(symbol_uri.as_str()),
            doc: doc.as_deref(),
            path: None,
            bs: None,
            be: None,
        },
        Some(EdgeEndpoint::SyntaxDef {
            path,
            byte_start,
            byte_end,
        }) => EndpointParts {
            kind: "syntax",
            import_id: None,
            uri: None,
            doc: None,
            path: Some(path.as_str()),
            bs: Some(*byte_start as i64),
            be: Some(*byte_end as i64),
        },
        None => EndpointParts {
            kind: "unknown",
            import_id: None,
            uri: None,
            doc: None,
            path: None,
            bs: None,
            be: None,
        },
    }
}

fn read_endpoint(
    kind: &str,
    import_id: Option<i64>,
    uri: Option<String>,
    doc: Option<String>,
    path: Option<String>,
    bs: Option<i64>,
    be: Option<i64>,
) -> Option<EdgeEndpoint> {
    match kind {
        "scip" => Some(EdgeEndpoint::Scip {
            import_id: import_id?,
            symbol_uri: uri?,
            doc,
        }),
        "syntax" => Some(EdgeEndpoint::SyntaxDef {
            path: path?,
            byte_start: bs? as usize,
            byte_end: be? as usize,
        }),
        _ => None,
    }
}

/// Store one graph edge. Unknown endpoints stay `None` — never fabricated.
pub fn insert_edge(conn: &Connection, edge: &EdgeRow) -> Result<i64, String> {
    let from = endpoint_parts(&edge.caller);
    let to = endpoint_parts(&edge.callee);
    conn.execute(
        "INSERT INTO code_edges (snapshot_id, file_id, caller_kind, caller_import_id, caller_uri, caller_doc, caller_path, caller_bs, caller_be, caller_file, caller_sbs, caller_sbe, callee_kind, callee_import_id, callee_uri, callee_doc, callee_path, callee_bs, callee_be, callee_name, edge_kind, precision, provider, evidence, unresolved) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
        params![
            edge.snapshot_id,
            edge.file_id,
            from.kind,
            from.import_id,
            from.uri,
            from.doc,
            from.path,
            from.bs,
            from.be,
            edge.caller_path,
            edge.caller_span_bytes.0 as i64,
            edge.caller_span_bytes.1 as i64,
            to.kind,
            to.import_id,
            to.uri,
            to.doc,
            to.path,
            to.bs,
            to.be,
            edge.callee_name,
            edge.edge_kind,
            edge.precision,
            edge.provider,
            edge.evidence,
            edge.unresolved,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

fn read_edge(row: &rusqlite::Row) -> Result<EdgeRow, rusqlite::Error> {
    let caller_kind: String = row.get(3)?;
    let callee_kind: String = row.get(13)?;
    Ok(EdgeRow {
        snapshot_id: row.get(1)?,
        file_id: row.get(2)?,
        caller: read_endpoint(
            &caller_kind,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
        ),
        caller_path: row.get(10)?,
        caller_span_bytes: (
            row.get::<_, i64>(11)? as usize,
            row.get::<_, i64>(12)? as usize,
        ),
        callee: read_endpoint(
            &callee_kind,
            row.get(14)?,
            row.get(15)?,
            row.get(16)?,
            row.get(17)?,
            row.get(18)?,
            row.get(19)?,
        ),
        callee_name: row.get(20)?,
        edge_kind: row.get(21)?,
        precision: row.get(22)?,
        provider: row.get(23)?,
        evidence: row.get(24)?,
        unresolved: row.get(25)?,
    })
}

const EDGE_COLS: &str = "id, snapshot_id, file_id, caller_kind, caller_import_id, caller_uri, caller_doc, caller_path, caller_bs, caller_be, caller_file, caller_sbs, caller_sbe, callee_kind, callee_import_id, callee_uri, callee_doc, callee_path, callee_bs, callee_be, callee_name, edge_kind, precision, provider, evidence, unresolved";

// Total order within a pinned snapshot, shared by exact and candidate queries.
const EDGE_ORDER: &str = "caller_file, caller_sbs, callee_kind, callee_import_id, callee_uri, callee_doc, callee_path, callee_bs, callee_be, caller_sbe, edge_kind, caller_kind, caller_import_id, caller_uri, caller_doc, caller_path, caller_bs, caller_be, provider, id";

pub struct EdgeSelection<'a> {
    pub snapshot_id: i64,
    pub endpoint: &'a EdgeEndpoint,
    pub inbound: bool,
    pub candidate_name: Option<&'a str>,
    pub candidate_paths: &'a [String],
    pub candidate_aliases: &'a [(String, String)],
    pub syntax_aliases: &'a [EdgeEndpoint],
}

impl EdgeSelection<'_> {
    fn sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let p = if self.inbound { "callee" } else { "caller" };
        let owned = Some(self.endpoint.clone());
        let part = endpoint_parts(&owned);
        let text = |v: Option<&str>| v.map(|s| Value::Text(s.to_string())).unwrap_or(Value::Null);
        let num = |v: Option<i64>| v.map(Value::Integer).unwrap_or(Value::Null);
        let values = vec![
            Value::Integer(self.snapshot_id),
            text(Some(part.kind)),
            num(part.import_id),
            text(part.uri),
            text(part.doc),
            text(part.path),
            num(part.bs),
            num(part.be),
            text(self.candidate_name),
            Value::Text(serde_json::to_string(self.candidate_paths).expect("string paths")),
            Value::Text(
                serde_json::to_string(
                    &self
                        .syntax_aliases
                        .iter()
                        .filter_map(|alias| match alias {
                            EdgeEndpoint::SyntaxDef {
                                path,
                                byte_start,
                                byte_end,
                            } => Some(serde_json::json!([path, byte_start, byte_end])),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                )
                .expect("syntax aliases"),
            ),
            Value::Text(serde_json::to_string(self.candidate_aliases).expect("candidate aliases")),
        ];
        let predicate = format!("snapshot_id = ?1 AND (({p}_kind = ?2 AND {p}_import_id IS ?3 AND {p}_uri IS ?4 AND {p}_doc IS ?5 AND {p}_path IS ?6 AND {p}_bs IS ?7 AND {p}_be IS ?8) OR (edge_kind = 'POTENTIAL_CALL' AND callee_kind = 'unknown' AND (callee_name = ?9 OR EXISTS (SELECT 1 FROM json_each(?12) a WHERE caller_file = json_extract(a.value, '$[0]') AND callee_name = json_extract(a.value, '$[1]'))) AND (provider = 'syntax' OR CASE WHEN json_valid(evidence) THEN json_extract(evidence, '$.import') END = ?3) AND caller_file IN (SELECT value FROM json_each(?10))) OR (caller_kind = 'syntax' AND (provider = 'syntax' OR callee_import_id = ?3 OR CASE WHEN json_valid(evidence) THEN json_extract(evidence, '$.import') END = ?3) AND EXISTS (SELECT 1 FROM json_each(?11) a WHERE caller_path = json_extract(a.value, '$[0]') AND caller_bs = json_extract(a.value, '$[1]') AND caller_be = json_extract(a.value, '$[2]'))))");
        (predicate, values)
    }
}

pub struct EdgeCounts {
    pub total: usize,
    pub unresolved_owner: usize,
    pub unresolved_target: usize,
    pub dynamic_calls: usize,
    pub ambiguous_calls: usize,
    pub unresolved: usize,
}

pub fn selected_edge_counts(
    conn: &Connection,
    selection: &EdgeSelection<'_>,
) -> Result<EdgeCounts, String> {
    let (predicate, values) = selection.sql();
    edge_counts(conn, &predicate, values)
}

pub fn snapshot_edge_counts(conn: &Connection, snapshot_id: i64) -> Result<EdgeCounts, String> {
    edge_counts(
        conn,
        "snapshot_id = ?1 AND edge_kind != 'IMPORTS'",
        vec![rusqlite::types::Value::Integer(snapshot_id)],
    )
}

fn edge_counts(
    conn: &Connection,
    predicate: &str,
    values: Vec<rusqlite::types::Value>,
) -> Result<EdgeCounts, String> {
    conn.query_row(&format!("SELECT COUNT(*), COALESCE(SUM(caller_kind = 'unknown'),0), COALESCE(SUM(callee_kind = 'unknown'),0), COALESCE(SUM(unresolved = 'dynamic'),0), COALESCE(SUM(unresolved LIKE 'ambiguous:%'),0), COALESCE(SUM(unresolved IS NOT NULL),0) FROM code_edges WHERE {predicate}"), rusqlite::params_from_iter(values), |r| Ok(EdgeCounts { total: r.get::<_, i64>(0)? as usize, unresolved_owner: r.get::<_, i64>(1)? as usize, unresolved_target: r.get::<_, i64>(2)? as usize, dynamic_calls: r.get::<_, i64>(3)? as usize, ambiguous_calls: r.get::<_, i64>(4)? as usize, unresolved: r.get::<_, i64>(5)? as usize })).map_err(|e| e.to_string())
}

pub fn selected_edge_page(
    conn: &Connection,
    selection: &EdgeSelection<'_>,
    limit: i64,
    offset: i64,
) -> Result<Vec<EdgeRow>, String> {
    let (predicate, mut values) = selection.sql();
    values.push(rusqlite::types::Value::Integer(limit.clamp(1, 1000)));
    values.push(rusqlite::types::Value::Integer(offset.max(0)));
    let mut stmt = conn.prepare(&format!("SELECT {EDGE_COLS} FROM code_edges WHERE {predicate} ORDER BY {EDGE_ORDER} LIMIT ?13 OFFSET ?14")).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(values), read_edge)
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Outbound edges of one endpoint (callees direction), deterministic order.
pub fn edges_outbound(
    conn: &Connection,
    snapshot_id: i64,
    from: &EdgeEndpoint,
    limit: i64,
    offset: i64,
) -> Result<Vec<EdgeRow>, String> {
    query_edges(conn, snapshot_id, "caller", from, limit, offset)
}

/// Inbound edges of one endpoint (callers direction), deterministic order.
pub fn edges_inbound(
    conn: &Connection,
    snapshot_id: i64,
    to: &EdgeEndpoint,
    limit: i64,
    offset: i64,
) -> Result<Vec<EdgeRow>, String> {
    query_edges(conn, snapshot_id, "callee", to, limit, offset)
}

fn query_edges(
    conn: &Connection,
    snapshot_id: i64,
    prefix: &str,
    endpoint: &EdgeEndpoint,
    limit: i64,
    offset: i64,
) -> Result<Vec<EdgeRow>, String> {
    selected_edge_page(
        conn,
        &EdgeSelection {
            snapshot_id,
            endpoint,
            inbound: prefix == "callee",
            candidate_name: None,
            candidate_paths: &[],
            candidate_aliases: &[],
            syntax_aliases: &[],
        },
        limit,
        offset,
    )
}

fn snapshot_from_row(row: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        id: row.get(0)?,
        repo: row.get(1)?,
        root: row.get(2)?,
        created_unix: row.get::<_, i64>(3)? as u64,
        head: row.get(4)?,
        branch: row.get(5)?,
        dirty: row.get::<_, i64>(6)? != 0,
        untracked: row.get::<_, i64>(7)? as u32,
        parser_version: row.get::<_, i64>(8)? as u32,
        grammar_versions: row.get(9)?,
        config_fingerprint: row.get(10)?,
        pre_fingerprint: row.get(11)?,
        post_fingerprint: row.get(12)?,
        status_at_write: row.get(13)?,
        nonce: row.get(14)?,
    })
}

const SNAPSHOT_COLS: &str = "id, repo, root, created_unix, head, branch, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write, nonce";

pub fn latest_snapshot(conn: &Connection) -> Result<Option<Snapshot>, String> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT {SNAPSHOT_COLS} FROM snapshots ORDER BY id DESC LIMIT 1"
        ))
        .map_err(|e| e.to_string())?;
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    match rows.next().map_err(|e| e.to_string())? {
        Some(row) => Ok(Some(snapshot_from_row(row).map_err(|e| e.to_string())?)),
        None => Ok(None),
    }
}

pub fn files_of(conn: &Connection, snapshot_id: i64) -> Result<Vec<FileRow>, String> {
    let mut statement = conn
        .prepare("SELECT path, language, bytes, digest, errors FROM files WHERE snapshot_id = ?1 ORDER BY path")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([snapshot_id], |row| {
            Ok(FileRow {
                path: row.get(0)?,
                language: row.get(1)?,
                bytes: row.get::<_, i64>(2)? as u64,
                digest: row.get(3)?,
                errors: row.get::<_, i64>(4)? as u32,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Path → (file rowid, content digest) for one snapshot. The id map lets
/// incremental reuse locate old facts without touching `FileRow`.
pub fn file_id_map(
    conn: &Connection,
    snapshot_id: i64,
) -> Result<BTreeMap<String, (i64, String)>, String> {
    let mut statement = conn
        .prepare("SELECT id, path, digest FROM files WHERE snapshot_id = ?1 ORDER BY path")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([snapshot_id], |row| {
            Ok((
                row.get::<_, String>(1)?,
                (row.get::<_, i64>(0)?, row.get::<_, String>(2)?),
            ))
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(|e| e.to_string())
}

fn hit_from_row(snapshot_id: i64, row: &rusqlite::Row) -> rusqlite::Result<Hit> {
    Ok(Hit {
        snapshot_id,
        path: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        start_line: row.get::<_, i64>(3)? as u32,
        start_col: row.get::<_, i64>(4)? as u32,
        end_line: row.get::<_, i64>(5)? as u32,
        end_col: row.get::<_, i64>(6)? as u32,
        byte_start: row.get::<_, i64>(7)? as usize,
        byte_end: row.get::<_, i64>(8)? as usize,
        provider: row.get(9)?,
        grammar: row.get(10)?,
        language: row.get(11)?,
    })
}

fn like_pattern(term: &str) -> String {
    let mut pattern = String::with_capacity(term.len() + 2);
    pattern.push('%');
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

/// Search definitions by name: FTS5 prefix matches first, then LIKE substring
/// matches, both ordered by (path, byte_start). Returns the total count plus
/// the requested page. Calls and imports are never symbol hits.
pub fn search_symbols(
    conn: &Connection,
    snapshot_id: i64,
    term: &str,
    limit: usize,
    offset: usize,
) -> Result<(usize, Vec<Hit>), String> {
    if term.is_empty() {
        return Ok((0, Vec::new()));
    }
    let mut ordered_ids: Vec<i64> = Vec::new();
    // FTS5 prefix search on the whole phrase. Any FTS error (e.g. a term made
    // only of separators) falls back to LIKE alone — never a failure.
    let phrase = format!("\"{}\"*", term.replace('"', "\"\""));
    let mut fts_ids: Vec<i64> = Vec::new();
    if let Ok(mut statement) = conn.prepare(
        "SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id JOIN symbols_fts ON symbols_fts.rowid = s.id WHERE s.snapshot_id = ?1 AND symbols_fts MATCH ?2 ORDER BY f.path, s.byte_start",
    ) {
        if let Ok(mut rows) = statement.query(params![snapshot_id, phrase]) {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(id) = row.get::<_, i64>(0) {
                    fts_ids.push(id);
                }
            }
        }
    }
    // LIKE pass in (path, byte_start) order.
    let like_pattern_value = like_pattern(term);
    let mut like_ids: Vec<i64> = Vec::new();
    {
        let mut statement = conn
            .prepare(
                "SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.snapshot_id = ?1 AND s.name LIKE ?2 ESCAPE '\\' ORDER BY f.path, s.byte_start",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = statement
            .query(params![snapshot_id, like_pattern_value])
            .map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            like_ids.push(row.get(0).map_err(|e| e.to_string())?);
        }
    }
    for id in fts_ids.into_iter().chain(like_ids) {
        if !ordered_ids.contains(&id) {
            ordered_ids.push(id);
        }
    }
    let total = ordered_ids.len();
    let page: Vec<i64> = ordered_ids.into_iter().skip(offset).take(limit).collect();
    let mut hits = Vec::with_capacity(page.len());
    for id in page {
        let hit = conn
            .query_row(
                "SELECT f.path, s.kind, s.name, s.start_line, s.start_col, s.end_line, s.end_col, s.byte_start, s.byte_end, s.provider, s.grammar, s.language FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.id = ?1",
                [id],
                |row| hit_from_row(snapshot_id, row),
            )
            .map_err(|e| e.to_string())?;
        hits.push(hit);
    }
    Ok((total, hits))
}

/// One stored name occurrence joined with its file path.
#[derive(Debug, Clone, PartialEq)]
pub struct OccurrenceHit {
    pub path: String,
    pub role: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub provider: String,
    pub grammar: String,
    pub language: String,
}

/// The occurrence census of one exact name in one snapshot, with the number
/// of declarations of that name so that ambiguity is never hidden.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OccurrenceList {
    /// `field` symbols with this exact name.
    pub field_declarations: usize,
    /// All symbols with this exact name.
    pub symbol_declarations: usize,
    /// Occurrence rows for this exact name (all roles).
    pub total: usize,
    pub items: Vec<OccurrenceHit>,
}

/// The occurrence census of one EXACT name: the declaration counts, the total
/// and one `limit`/`offset` page ordered by (path, byte offset, role). An empty
/// name answers an empty list without touching the table. The name is matched
/// by equality (the symbol search keeps its prefix semantics), and every
/// query is scoped to the one snapshot the caller already pinned.
pub fn occurrences_for(
    conn: &Connection,
    snapshot_id: i64,
    name: &str,
    limit: usize,
    offset: usize,
) -> Result<OccurrenceList, String> {
    if name.is_empty() {
        return Ok(OccurrenceList::default());
    }
    let (symbol_declarations, field_declarations): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN kind = 'field' THEN 1 ELSE 0 END), 0) FROM symbols WHERE snapshot_id = ?1 AND name = ?2",
            params![snapshot_id, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM reference_occurrences WHERE snapshot_id = ?1 AND name = ?2",
            params![snapshot_id, name],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let mut statement = conn
        .prepare(
            "SELECT f.path, r.role, r.start_line, r.start_col, r.end_line, r.end_col, r.byte_start, r.byte_end, r.provider, r.grammar, r.language FROM reference_occurrences r JOIN files f ON f.id = r.file_id WHERE r.snapshot_id = ?1 AND r.name = ?2 ORDER BY f.path, r.byte_start, r.role, r.id LIMIT ?3 OFFSET ?4",
        )
        .map_err(|e| e.to_string())?;
    let items = statement
        .query_map(
            params![snapshot_id, name, limit as i64, offset as i64],
            |row| {
                Ok(OccurrenceHit {
                    path: row.get(0)?,
                    role: row.get(1)?,
                    start_line: row.get::<_, i64>(2)? as u32,
                    start_col: row.get::<_, i64>(3)? as u32,
                    end_line: row.get::<_, i64>(4)? as u32,
                    end_col: row.get::<_, i64>(5)? as u32,
                    byte_start: row.get::<_, i64>(6)? as usize,
                    byte_end: row.get::<_, i64>(7)? as usize,
                    provider: row.get(8)?,
                    grammar: row.get(9)?,
                    language: row.get(10)?,
                })
            },
        )
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(OccurrenceList {
        field_declarations: field_declarations as usize,
        symbol_declarations: symbol_declarations as usize,
        total: total as usize,
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        attach_src, carry_semantic_facts, content_digest, content_identity, db_path, detach_src,
        files_of, finish_snapshot, fts_available, grammar_versions, insert_call, insert_file,
        insert_import, insert_semantic_facts, insert_semantic_import, insert_snapshot,
        insert_symbol, is_readable_db, latest_snapshot, live_base, meta_get, meta_set, migrate,
        open_live, open_memory, open_read, promote_staging, search_symbols, semantic_facts_for,
        semantic_generation, semantic_imports_for, staging_path, FileRow, IndexRead, SemanticFact,
        SemanticImport, Snapshot, SymbolRow, SCHEMA_VERSION,
    };
    use crate::testutil::tmpdir;
    use std::path::Path;

    fn sample_snapshot() -> Snapshot {
        Snapshot {
            id: 0,
            repo: "demo".to_string(),
            root: "C:\\demo".to_string(),
            created_unix: 1000,
            head: Some("abc".to_string()),
            branch: Some("main".to_string()),
            dirty: false,
            untracked: 0,
            parser_version: super::PARSER_VERSION,
            grammar_versions: grammar_versions(),
            config_fingerprint: "cfg1".to_string(),
            pre_fingerprint: "pre1".to_string(),
            post_fingerprint: "pre1".to_string(),
            status_at_write: "fresh".to_string(),
            nonce: "test-nonce-1".to_string(),
        }
    }

    #[test]
    fn semantic_owner_lookup_avoids_scanning_other_files() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let conn = open_memory().unwrap();
        conn.execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<20000)
             INSERT INTO semantic_facts
               (snapshot_id, import_id, file_id, rel_path, file_digest, symbol_uri,
                display_name, roles, start_line, start_col, end_line, end_col,
                byte_start, byte_end, precision)
             SELECT 1, 1, 2, 'other.rs', 'digest', printf('symbol-%08d', x),
                    'other', 1, 1, 1, 1, 2, 0, 1, 'Heuristic' FROM n;",
        )
        .unwrap();
        let steps = AtomicUsize::new(0);
        unsafe extern "C" fn limit_steps(ctx: *mut std::ffi::c_void) -> std::ffi::c_int {
            // The handler is removed before its borrowed counter is dropped.
            let steps = unsafe { &*(ctx as *const AtomicUsize) };
            i32::from(steps.fetch_add(1, Ordering::Relaxed) >= 200)
        }
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                conn.handle(),
                100,
                Some(limit_steps),
                &steps as *const AtomicUsize as *mut std::ffi::c_void,
            );
        }
        let result = super::semantic_owner_uris(
            &conn,
            1,
            1,
            &sample_symbol(1, "owner"),
            "target.rs",
            "digest",
        );
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(conn.handle(), 0, None, std::ptr::null_mut())
        };
        assert_eq!(result.unwrap(), Vec::<String>::new());
    }

    #[test]
    fn file_identity_lookup_uses_the_snapshot_path_index() {
        let conn = open_memory().unwrap();
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id,digest FROM files WHERE snapshot_id=?1 AND path=?2 ORDER BY id LIMIT 2",
            )
            .unwrap()
            .query_map(rusqlite::params![1_i64, "a.rs"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("idx_files_snapshot_path")),
            "{plan:?}"
        );
    }

    #[test]
    fn live_open_restores_the_path_index_on_a_current_schema_database() {
        let dir = tmpdir("store-path-index");
        let path = dir.join("index.db");
        drop(open_live(&path).unwrap());
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP INDEX idx_files_snapshot_path;")
            .unwrap();
        let conn = open_live(&path).unwrap();
        let present: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_files_snapshot_path'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1);
    }

    #[test]
    fn task7_promotion_only_transfers_matching_staging_proof() {
        for state in ["missing", "mismatch", "current"] {
            let dir = tmpdir(&format!("proof-transfer-{state}"));
            let live = open_live(&dir.join("live.db")).unwrap();
            let live_id = insert_snapshot(&live, &sample_snapshot()).unwrap();
            meta_set(&live, "semantic_generation", "7").unwrap();
            super::mark_graph_proof(&live, live_id).unwrap();
            let base = live_base(&live).unwrap();
            let path = dir.join("stage.db");
            let staging = open_live(&path).unwrap();
            let mut snap = sample_snapshot();
            snap.nonce = "staged-proof-snapshot".to_string();
            let id = insert_snapshot(&staging, &snap).unwrap();
            meta_set(&staging, "semantic_generation", "3").unwrap();
            match state {
                "current" => super::mark_graph_proof(&staging, id).unwrap(),
                "mismatch" => meta_set(&staging, "graph_proof", "future:wrong-snapshot:3").unwrap(),
                _ => {}
            }
            drop(staging);
            promote_staging(&live, &path, &base).unwrap();
            assert_eq!(semantic_generation(&live).unwrap(), 7);
            assert_eq!(latest_snapshot(&live).unwrap().unwrap().nonce, snap.nonce);
            assert_eq!(
                super::graph_proof_current(&live, id).unwrap(),
                state == "current"
            );
            if state != "current" {
                assert!(meta_get(&live, "graph_proof").unwrap().is_none());
            }
        }
    }

    #[test]
    fn task7_failed_proof_promotion_rolls_back_retained_connection() {
        let dir = tmpdir("proof-rollback");
        let live = open_live(&dir.join("live.db")).unwrap();
        let id = insert_snapshot(&live, &sample_snapshot()).unwrap();
        super::mark_graph_proof(&live, id).unwrap();
        let base = live_base(&live).unwrap();
        let before = meta_get(&live, "graph_proof").unwrap();
        let path = dir.join("stage.db");
        let staging = open_live(&path).unwrap();
        let mut next = sample_snapshot();
        next.nonce = "new-proof-snapshot".to_string();
        let id = insert_snapshot(&staging, &next).unwrap();
        super::mark_graph_proof(&staging, id).unwrap();
        drop(staging);
        live.execute_batch("CREATE TRIGGER reject_proof BEFORE INSERT ON meta WHEN NEW.key='graph_proof' BEGIN SELECT RAISE(ABORT,'proof fail'); END;").unwrap();
        assert!(promote_staging(&live, &path, &base)
            .unwrap_err()
            .contains("proof fail"));
        assert!(
            live.is_autocommit(),
            "failed proof promotion must finish rollback before returning"
        );
        assert_eq!(live_base(&live).unwrap(), base);
        assert_eq!(meta_get(&live, "graph_proof").unwrap(), before);
    }

    fn sample_symbol(file_id: i64, name: &str) -> SymbolRow {
        SymbolRow {
            file_id,
            kind: "function".to_string(),
            name: Some(name.to_string()),
            start_line: 1,
            start_col: 1,
            end_line: 2,
            end_col: 3,
            byte_start: 0,
            byte_end: 20,
            provider: "syntax".to_string(),
            grammar: "python:0.25.0".to_string(),
            language: "python".to_string(),
        }
    }

    fn sample_semantic_import(snapshot_id: i64) -> SemanticImport {
        SemanticImport {
            snapshot_id,
            provider_name: "scip-import".to_string(),
            provider_version: "0.1-test".to_string(),
            capability: "definitions,references".to_string(),
            project_root: "C:\\demo".to_string(),
            text_encoding: "UTF8".to_string(),
            artifact_id: "artifact-1".to_string(),
            source_revision: "head=abc".to_string(),
            claimed_source_digest: "".to_string(),
            content_identity: "ci-1".to_string(),
            external_symbols: "[]".to_string(),
            verification: "Unverified".to_string(),
        }
    }

    fn sample_semantic_fact(
        snapshot_id: i64,
        import_id: i64,
        symbol: Option<&str>,
    ) -> SemanticFact {
        SemanticFact {
            snapshot_id,
            import_id,
            file_id: None,
            rel_path: "a.py".to_string(),
            file_digest: "d1".to_string(),
            symbol_uri: symbol.map(str::to_string),
            display_name: Some("a".to_string()),
            roles: 0x1,
            start_line: 1,
            start_col: 0,
            end_line: 1,
            end_col: 5,
            byte_start: 0,
            byte_end: 5,
            precision: "Exact".to_string(),
            relationships: "[]".to_string(),
            external: false,
        }
    }

    /// Minimal v2 DDL (snapshots carry nonce, no semantic tables) for a
    /// frozen-shape migration fixture. Deliberately handwritten, never
    /// `SCHEMA_SQL`, so schema drift breaks this loudly.
    const V2_TABLES: &str = "
        CREATE TABLE snapshots (id INTEGER PRIMARY KEY AUTOINCREMENT,
          repo TEXT NOT NULL, root TEXT NOT NULL, created_unix INTEGER NOT NULL,
          head TEXT, branch TEXT, dirty INTEGER NOT NULL, untracked INTEGER NOT NULL,
          parser_version INTEGER NOT NULL, grammar_versions TEXT NOT NULL,
          config_fingerprint TEXT NOT NULL, pre_fingerprint TEXT NOT NULL,
          post_fingerprint TEXT NOT NULL, status_at_write TEXT NOT NULL,
          nonce TEXT NOT NULL DEFAULT '');
        CREATE TABLE files (id INTEGER PRIMARY KEY AUTOINCREMENT,
          snapshot_id INTEGER NOT NULL, path TEXT NOT NULL, language TEXT NOT NULL,
          bytes INTEGER NOT NULL, digest TEXT NOT NULL, errors INTEGER NOT NULL);
        CREATE TABLE symbols (id INTEGER PRIMARY KEY AUTOINCREMENT,
          snapshot_id INTEGER NOT NULL, file_id INTEGER NOT NULL, kind TEXT NOT NULL,
          name TEXT, start_line INTEGER NOT NULL, start_col INTEGER NOT NULL,
          end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
          byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
          provider TEXT NOT NULL, grammar TEXT NOT NULL, language TEXT NOT NULL);
        CREATE TABLE imports (id INTEGER PRIMARY KEY AUTOINCREMENT,
          snapshot_id INTEGER NOT NULL, file_id INTEGER NOT NULL, text TEXT,
          start_line INTEGER NOT NULL, start_col INTEGER NOT NULL,
          end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
          byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
          provider TEXT NOT NULL, grammar TEXT NOT NULL, language TEXT NOT NULL);
        CREATE TABLE calls (id INTEGER PRIMARY KEY AUTOINCREMENT,
          snapshot_id INTEGER NOT NULL, file_id INTEGER NOT NULL, name TEXT,
          start_line INTEGER NOT NULL, start_col INTEGER NOT NULL,
          end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
          byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
          unresolved INTEGER NOT NULL, provider TEXT NOT NULL,
          grammar TEXT NOT NULL, language TEXT NOT NULL);
        CREATE VIRTUAL TABLE symbols_fts USING fts5(
          name, content='symbols', content_rowid='id', tokenize='unicode61');
        CREATE TRIGGER symbols_ai AFTER INSERT ON symbols BEGIN
          INSERT INTO symbols_fts(rowid, name) VALUES (new.id, new.name);
        END;
        CREATE TRIGGER symbols_ad AFTER DELETE ON symbols BEGIN
          INSERT INTO symbols_fts(symbols_fts, rowid, name)
          VALUES ('delete', old.id, old.name);
        END;";

    #[test]
    fn v2_db_migrates_to_current_with_data_intact() {
        let dir = tmpdir("store-v2-m3");
        let path = dir.join("v2.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                &format!("{V2_TABLES} PRAGMA user_version = 2;
                INSERT INTO snapshots (repo, root, created_unix, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write, nonce)
                VALUES ('r', 'C:\\r', 1, 0, 0, 1, 'g', 'c', 'p', 'p', 'fresh', 'nonce-v2');
                INSERT INTO files (snapshot_id, path, language, bytes, digest, errors)
                VALUES (1, 'a.py', 'python', 7, 'd', 0);
                INSERT INTO symbols (snapshot_id, file_id, kind, name, start_line, start_col, end_line, end_col, byte_start, byte_end, provider, grammar, language)
                VALUES (1, 1, 'function', 'keep_me', 1, 0, 1, 7, 0, 7, 'syntax', 'python:0.25.0', 'python');"),
            )
            .unwrap();
        }
        let conn = open_live(&path).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let snap = latest_snapshot(&conn).unwrap().unwrap();
        assert_eq!(snap.nonce, "nonce-v2");
        let (_, hits) = search_symbols(&conn, snap.id, "keep_me", 10, 0).unwrap();
        assert_eq!(hits.len(), 1, "v2 facts must survive migration");
        for t in ["semantic_imports", "semantic_facts", "code_edges"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing v4 table {t}");
        }
    }

    #[test]
    fn v1_db_migrates_to_current() {
        let dir = tmpdir("store-v1-m3");
        let path = dir.join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshots (id INTEGER PRIMARY KEY AUTOINCREMENT,
                 repo TEXT NOT NULL, root TEXT NOT NULL, created_unix INTEGER NOT NULL,
                 head TEXT, branch TEXT, dirty INTEGER NOT NULL, untracked INTEGER NOT NULL,
                 parser_version INTEGER NOT NULL, grammar_versions TEXT NOT NULL,
                 config_fingerprint TEXT NOT NULL, pre_fingerprint TEXT NOT NULL,
                 post_fingerprint TEXT NOT NULL, status_at_write TEXT NOT NULL);
                 INSERT INTO snapshots (repo, root, created_unix, head, branch, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write)
                 VALUES ('r', 'C:\\r', 1, 'abc', 'main', 0, 0, 1, 'g', 'c', 'p', 'p', 'fresh');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let conn = open_live(&path).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let snap = latest_snapshot(&conn).unwrap().unwrap();
        assert_eq!(snap.nonce, "v1-legacy-1");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='semantic_facts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn migration_failure_rolls_back() {
        // A VIEW squatting a v3 table name makes the migration fail midway;
        // the explicit transaction must leave user_version and v2 reads intact.
        let dir = tmpdir("store-migfail");
        let path = dir.join("f.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                &format!("{V2_TABLES}
                CREATE VIEW semantic_facts AS SELECT 1 AS x;
                INSERT INTO snapshots (repo, root, created_unix, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write, nonce)
                VALUES ('r', 'C:\\r', 1, 0, 0, 1, 'g', 'c', 'p', 'p', 'fresh', 'n');
                PRAGMA user_version = 2;"),
            )
            .unwrap();
        }
        assert!(open_live(&path).is_err(), "migration must fail");
        let conn = rusqlite::Connection::open(&path).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2, "failed migration must not bump version");
        let snap = latest_snapshot(&conn).unwrap().unwrap();
        assert_eq!(snap.nonce, "n", "v2 data must stay readable");
    }

    #[test]
    fn semantic_roundtrip_is_snapshot_scoped() {
        let conn = open_memory().unwrap();
        let s1 = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        let mut other = sample_snapshot();
        other.nonce = "test-nonce-2".to_string();
        let s2 = insert_snapshot(&conn, &other).unwrap();
        let imp = insert_semantic_import(&conn, &sample_semantic_import(s1)).unwrap();
        let mut fact = sample_semantic_fact(s1, imp, Some("scip-test . . a#"));
        insert_semantic_facts(&conn, &[fact.clone()]).unwrap();
        fact.snapshot_id = s2;
        fact.import_id = imp;
        insert_semantic_facts(&conn, &[fact]).unwrap();
        let got = semantic_facts_for(&conn, s1, "scip-test . . a#").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].snapshot_id, s1);
        assert_eq!(got[0].roles, 0x1);
        assert!(semantic_facts_for(&conn, s1, "scip-test . . missing#")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn semantic_generation_bumps_in_txn_and_content_identity_is_ordered() {
        let conn = open_memory().unwrap();
        assert_eq!(semantic_generation(&conn).unwrap(), 0);
        meta_set(&conn, "semantic_generation", "41").unwrap();
        assert_eq!(
            meta_get(&conn, "semantic_generation").unwrap(),
            Some("41".to_string())
        );
        assert_eq!(meta_get(&conn, "nope").unwrap(), None);
        let s = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        assert_eq!(content_identity(&conn, s).unwrap(), content_digest(b""));
        for (path, digest) in [("b.py", "d2"), ("a.py", "d1")] {
            insert_file(
                &conn,
                s,
                &FileRow {
                    path: path.to_string(),
                    language: "python".to_string(),
                    bytes: 3,
                    digest: digest.to_string(),
                    errors: 0,
                },
            )
            .unwrap();
        }
        // Sorted by path regardless of insert order: stable identity.
        assert_eq!(
            content_identity(&conn, s).unwrap(),
            content_digest(b"a.py\0d1\nb.py\0d2\n")
        );
    }

    #[test]
    fn fts5_is_available_in_bundled_sqlite() {
        let conn = open_memory().unwrap();
        assert!(fts_available(&conn), "bundled SQLite must enable FTS5");
    }

    #[test]
    fn schema_creates_all_tables_at_v1() {
        let conn = open_memory().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for expected in [
            "meta",
            "snapshots",
            "files",
            "symbols",
            "imports",
            "calls",
            "symbols_fts",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing {expected}: {tables:?}"
            );
        }
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_builds_v1_from_empty_and_rejects_future() {
        let dir = tmpdir("store-migrate");
        let path = dir.join("m.db");
        std::fs::write(&path, b"").unwrap();
        let conn = open_live(&path).unwrap();
        migrate(&conn).unwrap();
        assert!(fts_available(&conn));
        conn.execute("PRAGMA user_version = 99", []).unwrap();
        assert!(migrate(&conn).is_err(), "future schema must be rejected");
    }

    #[test]
    fn corrupt_file_fails_to_open() {
        let dir = tmpdir("store-corrupt");
        let path = dir.join("c.db");
        std::fs::write(&path, b"not a database at all").unwrap();
        assert!(open_live(&path).is_err());
        assert!(!is_readable_db(&path));
    }

    #[test]
    fn readable_check_passes_for_healthy_db() {
        let dir = tmpdir("store-readable");
        let path = dir.join("ok.db");
        {
            let conn = open_live(&path).unwrap();
            migrate(&conn).unwrap();
        }
        assert!(is_readable_db(&path));
        assert!(!is_readable_db(&dir.join("missing.db")));
    }

    #[test]
    fn wal_mode_is_enabled() {
        let dir = tmpdir("store-wal");
        let path = dir.join("w.db");
        let conn = open_live(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal", "concurrent readers need WAL");
    }

    #[test]
    fn migrate_upgrades_v1_snapshots_with_nonce_backfill() {
        let dir = tmpdir("store-v1");
        let path = dir.join("old.db");
        {
            // Raw connection: open_live would auto-migrate, defeating the
            // v1 simulation.
            let conn = rusqlite::Connection::open(&path).unwrap();
            // Simulate a v1 database: v1-shaped table, version marker 1.
            conn.execute_batch(
                "CREATE TABLE snapshots (id INTEGER PRIMARY KEY AUTOINCREMENT,
                 repo TEXT NOT NULL, root TEXT NOT NULL, created_unix INTEGER NOT NULL,
                 head TEXT, branch TEXT, dirty INTEGER NOT NULL, untracked INTEGER NOT NULL,
                 parser_version INTEGER NOT NULL, grammar_versions TEXT NOT NULL,
                 config_fingerprint TEXT NOT NULL, pre_fingerprint TEXT NOT NULL,
                 post_fingerprint TEXT NOT NULL, status_at_write TEXT NOT NULL);
                 INSERT INTO snapshots (repo, root, created_unix, head, branch, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write)
                 VALUES ('r', 'C:\\r', 1, 'abc', 'main', 0, 0, 1, 'g', 'c', 'p', 'p', 'fresh');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let conn = open_live(&path).unwrap();
        migrate(&conn).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, super::SCHEMA_VERSION);
        let snap = latest_snapshot(&conn).unwrap().unwrap();
        assert!(!snap.nonce.is_empty(), "migrated rows need identities");
        assert_eq!(
            snap.head.as_deref(),
            Some("abc"),
            "data must survive migration"
        );
    }

    #[test]
    fn read_transaction_sees_stable_snapshot_during_writer_commit() {
        // Deterministic concurrency proof without threads: reader opens a
        // read transaction, writer commits a new snapshot row, reader must
        // still see the old one. Fails on autocommit reads (sees new row)
        // and deadlocks-or-errors without WAL.
        let dir = tmpdir("store-isolation");
        let path = dir.join("iso.db");
        let writer = open_live(&path).unwrap();
        let mut first = sample_snapshot();
        first.created_unix = 1;
        insert_snapshot(&writer, &first).unwrap();
        let mut reader = open_live(&path).unwrap();
        let tx = reader.transaction().unwrap();
        let before = latest_snapshot(&tx).unwrap().unwrap();
        let mut second = sample_snapshot();
        second.created_unix = 2;
        insert_snapshot(&writer, &second).unwrap();
        let during = latest_snapshot(&tx).unwrap().unwrap();
        assert_eq!(before.created_unix, 1);
        assert_eq!(
            during.created_unix, 1,
            "read txn must not see the concurrent commit"
        );
        tx.commit().unwrap();
        let after = latest_snapshot(&reader).unwrap().unwrap();
        assert_eq!(after.created_unix, 2);
    }

    #[test]
    fn content_digest_is_deterministic_fnv() {
        assert_eq!(content_digest(b""), "cbf29ce484222325");
        assert_eq!(content_digest(b"a"), content_digest(b"a"));
        assert_ne!(content_digest(b"a"), content_digest(b"b"));
    }

    #[test]
    fn insert_and_search_roundtrip_with_pagination() {
        let conn = open_memory().unwrap();
        let snap = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        let f1 = insert_file(
            &conn,
            snap,
            &FileRow {
                path: "b.py".to_string(),
                language: "python".to_string(),
                bytes: 10,
                digest: "d1".to_string(),
                errors: 0,
            },
        )
        .unwrap();
        let f2 = insert_file(
            &conn,
            snap,
            &FileRow {
                path: "a.py".to_string(),
                language: "python".to_string(),
                bytes: 10,
                digest: "d2".to_string(),
                errors: 0,
            },
        )
        .unwrap();
        insert_symbol(&conn, snap, &sample_symbol(f1, "PatchMatch::Run")).unwrap();
        insert_symbol(&conn, snap, &sample_symbol(f2, "Batch_atch")).unwrap();
        insert_symbol(&conn, snap, &sample_symbol(f2, "üñî_atch")).unwrap();
        insert_symbol(&conn, snap, &sample_symbol(f2, "unrelated")).unwrap();

        let (total, hits) = search_symbols(&conn, snap, "atch", 10, 0).unwrap();
        assert_eq!(total, 3, "substring must match all three names: {hits:?}");
        assert!(
            hits.iter().any(|h| h.name.as_deref() == Some("üñî_atch")),
            "unicode names must survive storage: {hits:?}"
        );
    }

    #[test]
    fn search_orders_by_path_and_paginates() {
        let conn = open_memory().unwrap();
        let snap = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        for (path, name) in [
            ("b.py", "zeta_one"),
            ("a.py", "alpha_one"),
            ("c.py", "mid_one"),
        ] {
            let fid = insert_file(
                &conn,
                snap,
                &FileRow {
                    path: path.to_string(),
                    language: "python".to_string(),
                    bytes: 10,
                    digest: "d".to_string(),
                    errors: 0,
                },
            )
            .unwrap();
            insert_symbol(&conn, snap, &sample_symbol(fid, name)).unwrap();
        }
        let (total, page1) = search_symbols(&conn, snap, "one", 2, 0).unwrap();
        assert_eq!(total, 3);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].path, "a.py");
        assert_eq!(page1[1].path, "b.py");
        let (_, page2) = search_symbols(&conn, snap, "one", 2, 2).unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].path, "c.py");
    }

    #[test]
    fn latest_snapshot_returns_newest_or_none() {
        let conn = open_memory().unwrap();
        assert!(latest_snapshot(&conn).unwrap().is_none());
        let mut first = sample_snapshot();
        first.created_unix = 1;
        insert_snapshot(&conn, &first).unwrap();
        let mut second = sample_snapshot();
        second.created_unix = 2;
        let id2 = insert_snapshot(&conn, &second).unwrap();
        assert_eq!(latest_snapshot(&conn).unwrap().unwrap().id, id2);
    }

    #[test]
    fn files_list_is_ordered_by_path() {
        let conn = open_memory().unwrap();
        let snap = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        for path in ["z.py", "a.py", "m.py"] {
            insert_file(
                &conn,
                snap,
                &FileRow {
                    path: path.to_string(),
                    language: "python".to_string(),
                    bytes: 1,
                    digest: "d".to_string(),
                    errors: 0,
                },
            )
            .unwrap();
        }
        let files = files_of(&conn, snap).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a.py", "m.py", "z.py"]);
    }

    #[test]
    fn imports_and_calls_roundtrip() {
        use super::{CallRow, ImportRow};
        let conn = open_memory().unwrap();
        let snap = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        let fid = insert_file(
            &conn,
            snap,
            &FileRow {
                path: "a.py".to_string(),
                language: "python".to_string(),
                bytes: 30,
                digest: "d".to_string(),
                errors: 0,
            },
        )
        .unwrap();
        insert_import(
            &conn,
            snap,
            &ImportRow {
                file_id: fid,
                text: Some("import os".to_string()),
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: 10,
                byte_start: 0,
                byte_end: 9,
                provider: "syntax".to_string(),
                grammar: "python:0.25.0".to_string(),
                language: "python".to_string(),
            },
        )
        .unwrap();
        insert_call(
            &conn,
            snap,
            &CallRow {
                file_id: fid,
                name: Some("w.process".to_string()),
                start_line: 2,
                start_col: 5,
                end_line: 2,
                end_col: 20,
                byte_start: 15,
                byte_end: 30,
                callee: None,
                tcallee: None,
                unresolved: 1,
                provider: "syntax".to_string(),
                grammar: "python:0.25.0".to_string(),
                language: "python".to_string(),
            },
        )
        .unwrap();
        let (total, hits) = search_symbols(&conn, snap, "w.process", 10, 0).unwrap();
        assert_eq!(total, 0, "calls are not symbol definitions: {hits:?}");
    }

    #[test]
    fn file_map_bulk_copy_roundtrip() {
        use super::{
            attach_src, copy_file_row, copy_mapped_facts, create_file_map, detach_src, file_id_map,
            record_file_map, CallRow, ImportRow,
        };
        let dir = tmpdir("store-bulk");
        let live_path = dir.join("live.db");
        let old_snap;
        {
            let live = open_live(&live_path).unwrap();
            old_snap = insert_snapshot(&live, &sample_snapshot()).unwrap();
            for (path, errors) in [("a.py", 2), ("b.py", 0)] {
                let fid = insert_file(
                    &live,
                    old_snap,
                    &FileRow {
                        path: path.to_string(),
                        language: "python".to_string(),
                        bytes: 30,
                        digest: "d1".to_string(),
                        errors,
                    },
                )
                .unwrap();
                insert_symbol(&live, old_snap, &sample_symbol(fid, &format!("f_{path}"))).unwrap();
            }
            insert_import(
                &live,
                old_snap,
                &ImportRow {
                    file_id: 1,
                    text: Some("import os".to_string()),
                    start_line: 1,
                    start_col: 1,
                    end_line: 1,
                    end_col: 10,
                    byte_start: 0,
                    byte_end: 9,
                    provider: "syntax".to_string(),
                    grammar: "python:0.25.0".to_string(),
                    language: "python".to_string(),
                },
            )
            .unwrap();
            insert_call(
                &live,
                old_snap,
                &CallRow {
                    file_id: 2,
                    name: Some("g()".to_string()),
                    start_line: 2,
                    start_col: 5,
                    end_line: 2,
                    end_col: 20,
                    byte_start: 15,
                    byte_end: 30,
                    callee: None,
                    tcallee: None,
                    unresolved: 1,
                    provider: "syntax".to_string(),
                    grammar: "python:0.25.0".to_string(),
                    language: "python".to_string(),
                },
            )
            .unwrap();
            let map = file_id_map(&live, old_snap).unwrap();
            assert_eq!(map.len(), 2);
        }
        let staging = open_live(&dir.join("staging.db")).unwrap();
        attach_src(&staging, &live_path).unwrap();
        create_file_map(&staging).unwrap();
        let new_snap = insert_snapshot(&staging, &sample_snapshot()).unwrap();
        let live = open_live(&live_path).unwrap();
        let map = file_id_map(&live, old_snap).unwrap();
        for (path, (old_fid, digest)) in &map {
            let new_fid = copy_file_row(&staging, new_snap, old_snap, *old_fid, path, digest)
                .unwrap()
                .expect("source row present");
            record_file_map(&staging, *old_fid, new_fid).unwrap();
        }
        // Vanished source rows copy nothing instead of corrupting.
        assert!(
            copy_file_row(&staging, new_snap, old_snap, 9999, "x.py", "dx")
                .unwrap()
                .is_none()
        );
        // Same id but different path or digest (concurrent replacement
        // reusing rowids) must also copy nothing: never mix revisions.
        assert!(
            copy_file_row(&staging, new_snap, old_snap, 1, "other.py", "d1")
                .unwrap()
                .is_none()
        );
        assert!(copy_file_row(&staging, new_snap, old_snap, 1, "a.py", "dx")
            .unwrap()
            .is_none());
        let totals = copy_mapped_facts(&staging, new_snap, old_snap).unwrap();
        assert_eq!(totals.symbols, 2);
        assert_eq!(totals.imports, 1);
        assert_eq!(totals.calls, 1);
        assert_eq!(totals.errors, 2);
        detach_src(&staging).unwrap();
        let files = files_of(&staging, new_snap).unwrap();
        assert_eq!(files.len(), 2);
        let (total, hits) = search_symbols(&staging, new_snap, "f_a", 10, 0).unwrap();
        assert_eq!(total, 1, "copied symbols must be searchable: {hits:?}");
    }

    #[test]
    fn grammar_versions_match_cargo_lock() {
        // Workspace lockfile lives two levels above this crate.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock = std::fs::read_to_string(manifest.join("../../Cargo.lock")).unwrap();
        for (name, version) in pinned_grammars() {
            assert!(
                lock.contains(&format!("name = \"{name}\"\nversion = \"{version}\"")),
                "Cargo.lock must pin {name} {version}"
            );
        }
        let versions = grammar_versions();
        for (name, version) in pinned_grammars() {
            assert!(
                versions.contains(version),
                "grammar_versions must list {name} {version}"
            );
        }
    }

    fn pinned_grammars() -> Vec<(&'static str, &'static str)> {
        vec![
            ("tree-sitter", "0.25.10"),
            ("tree-sitter-python", "0.25.0"),
            ("tree-sitter-javascript", "0.25.0"),
            ("tree-sitter-typescript", "0.23.2"),
            ("tree-sitter-rust", "0.24.2"),
            ("tree-sitter-c", "0.24.2"),
            ("tree-sitter-cpp", "0.23.4"),
            ("tree-sitter-go", "0.25.0"),
            ("tree-sitter-java", "0.23.5"),
        ]
    }

    #[test]
    fn finish_snapshot_records_post_and_status() {
        let conn = open_memory().unwrap();
        let id = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        finish_snapshot(&conn, id, "post", "fresh").unwrap();
        let snap = latest_snapshot(&conn).unwrap().unwrap();
        assert_eq!(snap.post_fingerprint, "post");
        assert_eq!(snap.status_at_write, "fresh");
        assert!(finish_snapshot(&conn, 9999, "x", "y").is_err());
    }

    #[test]
    fn wal_reader_stable_across_semantic_commit() {
        // Deterministic concurrency proof without threads: a reader inside a
        // read transaction keeps seeing the old semantic generation while a
        // writer commits an import. Fails without WAL snapshot reads.
        let dir = tmpdir("store-sem-iso");
        let path = dir.join("iso.db");
        let writer = open_live(&path).unwrap();
        let snap = insert_snapshot(&writer, &sample_snapshot()).unwrap();
        insert_semantic_import(&writer, &sample_semantic_import(snap)).unwrap();
        let mut reader = open_live(&path).unwrap();
        let tx = reader.transaction().unwrap();
        let before = semantic_generation(&tx).unwrap();
        meta_set(&writer, "semantic_generation", "7").unwrap();
        insert_semantic_import(&writer, &sample_semantic_import(snap)).unwrap();
        assert_eq!(semantic_generation(&tx).unwrap(), before);
        assert_eq!(
            semantic_imports_for(&tx, snap).unwrap().len(),
            1,
            "reader keeps its snapshot"
        );
        // The read-transaction property query paths rely on: snapshot row,
        // facts, imports, and generation are mutually consistent mid-commit.
        let seen = latest_snapshot(&tx).unwrap().unwrap();
        assert_eq!(seen.nonce, "test-nonce-1");
        assert_eq!(
            semantic_facts_for(&tx, snap, "scip-test . . a#")
                .unwrap()
                .len(),
            0
        );
        drop(tx);
        assert_eq!(semantic_generation(&writer).unwrap(), 7);
    }

    #[test]
    fn promote_staging_copies_snapshot_atomically() {
        let dir = tmpdir("store-promote");
        let staging = dir.join("s.db");
        let live_path = dir.join("live.db");
        let staging_conn = open_live(&staging).unwrap();
        let id = insert_snapshot(&staging_conn, &sample_snapshot()).unwrap();
        finish_snapshot(&staging_conn, id, "post", "fresh").unwrap();
        let live = open_live(&live_path).unwrap();
        let base = live_base(&live).unwrap();
        promote_staging(&live, &staging, &base).unwrap();
        let snap = latest_snapshot(&live).unwrap().unwrap();
        assert_eq!(snap.post_fingerprint, "post");
        assert!(
            staging.exists(),
            "staging file itself is left for the caller to clean"
        );
    }

    #[test]
    fn m6_v4_migrate_chain_and_fresh_db() {
        use super::{edges_outbound, insert_edge, EdgeEndpoint, EdgeRow};
        // v1-shaped database migrates all the way to v4 with data intact.
        let dir = tmpdir("store-m6v1");
        let path = dir.join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshots (id INTEGER PRIMARY KEY AUTOINCREMENT,
                 repo TEXT NOT NULL, root TEXT NOT NULL, created_unix INTEGER NOT NULL,
                 head TEXT, branch TEXT, dirty INTEGER NOT NULL, untracked INTEGER NOT NULL,
                 parser_version INTEGER NOT NULL, grammar_versions TEXT NOT NULL,
                 config_fingerprint TEXT NOT NULL, pre_fingerprint TEXT NOT NULL,
                 post_fingerprint TEXT NOT NULL, status_at_write TEXT NOT NULL);
                 INSERT INTO snapshots (repo, root, created_unix, head, branch, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write)
                 VALUES ('r', 'C:\\r', 1, 'abc', 'main', 0, 0, 1, 'g', 'c', 'p', 'p', 'fresh');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let conn = open_live(&path).unwrap();
        migrate(&conn).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, SCHEMA_VERSION,
            "v1 must migrate to the current schema"
        );
        assert!(table_exists(&conn, "code_edges"), "edges table created");
        // v3-shaped database: old calls shape without callee columns.
        let dir = tmpdir("store-m6v3");
        let path = dir.join("old3.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshots (id INTEGER PRIMARY KEY AUTOINCREMENT,
                 repo TEXT NOT NULL, root TEXT NOT NULL, created_unix INTEGER NOT NULL,
                 head TEXT, branch TEXT, dirty INTEGER NOT NULL, untracked INTEGER NOT NULL,
                 parser_version INTEGER NOT NULL, grammar_versions TEXT NOT NULL,
                 config_fingerprint TEXT NOT NULL, pre_fingerprint TEXT NOT NULL,
                 post_fingerprint TEXT NOT NULL, status_at_write TEXT NOT NULL,
                 nonce TEXT NOT NULL DEFAULT '');
                 CREATE TABLE calls (id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot_id INTEGER NOT NULL,
                 file_id INTEGER NOT NULL, name TEXT, start_line INTEGER NOT NULL,
                 start_col INTEGER NOT NULL, end_line INTEGER NOT NULL, end_col INTEGER NOT NULL,
                 byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
                 unresolved INTEGER NOT NULL, provider TEXT NOT NULL, grammar TEXT NOT NULL,
                 language TEXT NOT NULL);
                 INSERT INTO snapshots (repo, root, created_unix, dirty, untracked, parser_version, grammar_versions, config_fingerprint, pre_fingerprint, post_fingerprint, status_at_write, nonce)
                 VALUES ('r', 'C:\\r', 1, 0, 0, 1, 'g', 'c', 'p', 'p', 'fresh', 'n1');
                 INSERT INTO calls (snapshot_id, file_id, name, start_line, start_col, end_line, end_col, byte_start, byte_end, unresolved, provider, grammar, language)
                 VALUES (1, 1, 'foo', 1, 1, 1, 9, 0, 8, 1, 'syntax', 'python:0.25.0', 'python');
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let conn = open_live(&path).unwrap();
        migrate(&conn).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, SCHEMA_VERSION,
            "v3 must migrate to the current schema"
        );
        let name: Option<String> = conn
            .query_row("SELECT name FROM calls WHERE snapshot_id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name.as_deref(), Some("foo"), "old call rows survive");
        let callee_null: Option<i64> = conn
            .query_row(
                "SELECT callee_byte_start FROM calls WHERE snapshot_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(callee_null, None, "legacy rows keep NULL callee cols");
        // Fresh database builds at the current schema; a second migrate is a
        // no-op (no double-ALTER failure).
        let dir = tmpdir("store-m6fresh");
        let fresh = open_live(&dir.join("f.db")).unwrap();
        migrate(&fresh).unwrap();
        migrate(&fresh).unwrap();
        let version: u32 = fresh
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        // Edges round-trip through promote: hand-inserted row survives.
        let staging = dir.join("s.db");
        let live_path = dir.join("live.db");
        let staging_conn = open_live(&staging).unwrap();
        let id = insert_snapshot(&staging_conn, &sample_snapshot()).unwrap();
        insert_edge(
            &staging_conn,
            &EdgeRow {
                snapshot_id: id,
                file_id: 1,
                caller: Some(EdgeEndpoint::SyntaxDef {
                    path: "a.py".to_string(),
                    byte_start: 0,
                    byte_end: 20,
                }),
                caller_path: "a.py".to_string(),
                caller_span_bytes: (10, 18),
                callee: None,
                callee_name: "foo".to_string(),
                edge_kind: "POTENTIAL_CALL".to_string(),
                precision: "Heuristic".to_string(),
                provider: "syntax".to_string(),
                evidence: "{\"path\":\"a.py\"}".to_string(),
                unresolved: Some("no-overlap".to_string()),
            },
        )
        .unwrap();
        let live = open_live(&live_path).unwrap();
        let base = live_base(&live).unwrap();
        promote_staging(&live, &staging, &base).unwrap();
        let live_snap = latest_snapshot(&live).unwrap().unwrap();
        let key = EdgeEndpoint::SyntaxDef {
            path: "a.py".to_string(),
            byte_start: 0,
            byte_end: 20,
        };
        let out = edges_outbound(&live, live_snap.id, &key, 50, 0).unwrap();
        assert_eq!(out.len(), 1, "promoted edge round-trips");
        assert!(out[0].callee.is_none(), "unknown target stays unknown");
        assert_eq!(out[0].unresolved.as_deref(), Some("no-overlap"));
    }

    fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n == 1)
        .unwrap_or(false)
    }

    #[test]
    fn m6_call_callee_roundtrip_with_legacy_null() {
        use super::{calls_for, CallRow};
        let conn = open_memory().unwrap();
        let snap = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        let fid = insert_file(
            &conn,
            snap,
            &FileRow {
                path: "a.py".to_string(),
                language: "python".to_string(),
                bytes: 30,
                digest: "d".to_string(),
                errors: 0,
            },
        )
        .unwrap();
        insert_call(
            &conn,
            snap,
            &CallRow {
                file_id: fid,
                name: Some("w.process".to_string()),
                start_line: 2,
                start_col: 5,
                end_line: 2,
                end_col: 20,
                byte_start: 15,
                byte_end: 30,
                callee: Some(super::SpanCols {
                    start_line: 2,
                    start_col: 5,
                    end_line: 2,
                    end_col: 14,
                    byte_start: 15,
                    byte_end: 24,
                }),
                tcallee: Some(super::SpanCols {
                    start_line: 2,
                    start_col: 7,
                    end_line: 2,
                    end_col: 14,
                    byte_start: 17,
                    byte_end: 24,
                }),
                unresolved: 1,
                provider: "syntax".to_string(),
                grammar: "python:0.25.0".to_string(),
                language: "python".to_string(),
            },
        )
        .unwrap();
        // Legacy row without callee data reads back as None.
        conn.execute(
            "INSERT INTO calls (snapshot_id, file_id, name, start_line, start_col, end_line, end_col, byte_start, byte_end, unresolved, provider, grammar, language) VALUES (?1,?2,'old',1,1,1,4,0,3,1,'syntax','python:0.25.0','python')",
            [snap, fid],
        )
        .unwrap();
        let rows = calls_for(&conn, snap).unwrap();
        assert_eq!(rows.len(), 2);
        let precise = rows
            .iter()
            .find(|r| r.name.as_deref() == Some("w.process"))
            .unwrap();
        assert_eq!(precise.callee.as_ref().unwrap().byte_start, 15);
        assert_eq!(precise.tcallee.as_ref().unwrap().byte_start, 17);
        let legacy = rows
            .iter()
            .find(|r| r.name.as_deref() == Some("old"))
            .unwrap();
        assert!(legacy.callee.is_none() && legacy.tcallee.is_none());
    }

    #[test]
    fn promote_aborts_when_live_advanced_semantic_changed() {
        // Deterministic writer race, both orders, no threads: a staging copy
        // based on an old generation must abort with the committed import
        // intact; a staging copy based after the import must succeed.
        let dir = tmpdir("store-race");
        let live_path = dir.join("live.db");
        let live = open_live(&live_path).unwrap();
        let s1 = insert_snapshot(&live, &sample_snapshot()).unwrap();
        let base = live_base(&live).unwrap();
        // Order 1: stage from base, then a semantic import commits on live.
        let staging_path = dir.join("s1.db");
        {
            let staging = open_live(&staging_path).unwrap();
            let mut moved = sample_snapshot();
            moved.nonce = "nonce-new".to_string();
            insert_snapshot(&staging, &moved).unwrap();
        }
        let imp = sample_semantic_import(s1);
        insert_semantic_import(&live, &imp).unwrap();
        meta_set(&live, "semantic_generation", "1").unwrap();
        let err = promote_staging(&live, &staging_path, &base).unwrap_err();
        assert!(err.contains("semantic-changed"), "unexpected: {err}");
        assert_eq!(semantic_generation(&live).unwrap(), 1);
        assert_eq!(
            latest_snapshot(&live).unwrap().unwrap().nonce,
            "test-nonce-1"
        );
        // Order 2: stage from the new base (import included) → succeeds.
        let staging2_path = dir.join("s2.db");
        {
            let staging = open_live(&staging2_path).unwrap();
            let mut moved = sample_snapshot();
            moved.nonce = "nonce-new".to_string();
            let ns = insert_snapshot(&staging, &moved).unwrap();
            insert_semantic_import(&staging, &sample_semantic_import(ns)).unwrap();
        }
        let base2 = live_base(&live).unwrap();
        promote_staging(&live, &staging2_path, &base2).unwrap();
        assert_eq!(latest_snapshot(&live).unwrap().unwrap().nonce, "nonce-new");
    }

    #[test]
    fn carry_semantic_copies_verified_facts_only() {
        let dir = tmpdir("store-carry");
        let live_path = dir.join("live.db");
        let live = open_live(&live_path).unwrap();
        let old = insert_snapshot(&live, &sample_snapshot()).unwrap();
        for (path, digest) in [("keep.py", "d1"), ("drop.py", "d2")] {
            insert_file(
                &live,
                old,
                &FileRow {
                    path: path.to_string(),
                    language: "python".to_string(),
                    bytes: 1,
                    digest: digest.to_string(),
                    errors: 0,
                },
            )
            .unwrap();
        }
        let imp = insert_semantic_import(&live, &sample_semantic_import(old)).unwrap();
        for rel in ["keep.py", "drop.py"] {
            let mut fact = sample_semantic_fact(old, imp, Some("s"));
            fact.rel_path = rel.to_string();
            insert_semantic_facts(&live, &[fact]).unwrap();
        }
        // Staging: new snapshot where keep.py verifies, drop.py changed.
        let staging_path = dir.join("staging.db");
        let staging = open_live(&staging_path).unwrap();
        attach_src(&staging, &live_path).unwrap();
        let mut moved = sample_snapshot();
        moved.nonce = "nonce-new".to_string();
        let ns = insert_snapshot(&staging, &moved).unwrap();
        for (path, digest) in [("keep.py", "d1"), ("drop.py", "dX")] {
            insert_file(
                &staging,
                ns,
                &FileRow {
                    path: path.to_string(),
                    language: "python".to_string(),
                    bytes: 1,
                    digest: digest.to_string(),
                    errors: 0,
                },
            )
            .unwrap();
        }
        let carried = carry_semantic_facts(&staging, ns, old, "rev-new", &|_| None).unwrap();
        assert_eq!(carried.imports, 1);
        assert_eq!(carried.facts, 1, "only the digest-verified file carries");
        let kept = semantic_facts_for(&staging, ns, "s").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rel_path, "keep.py");
        assert!(kept[0].file_id.is_some());
        // Import rebound to the new snapshot, original identity preserved.
        let row: (i64, String, String) = staging
            .query_row(
                "SELECT snapshot_id, source_revision, content_identity FROM semantic_imports",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, ns);
        assert_eq!(row.1, "rev-new");
        assert_eq!(row.2, "ci-1");
        detach_src(&staging).unwrap();
    }

    #[test]
    fn carry_null_facts_verify_against_disk_or_drop() {
        // file_id NULL facts have no syntax row to join: the carry-over
        // verifies them against raw source digests (stubbed here, wired to
        // disk by the build). Matching bytes carry with file_id still NULL;
        // changed bytes drop explicitly — never silently, never linked.
        let dir = tmpdir("store-carry-null");
        let live_path = dir.join("live.db");
        let live = open_live(&live_path).unwrap();
        let old = insert_snapshot(&live, &sample_snapshot()).unwrap();
        let imp = insert_semantic_import(&live, &sample_semantic_import(old)).unwrap();
        for (rel, digest) in [("keep.other", "dk"), ("drop.other", "dd")] {
            let mut fact = sample_semantic_fact(old, imp, Some("s-null"));
            fact.rel_path = rel.to_string();
            fact.file_digest = digest.to_string();
            fact.file_id = None;
            insert_semantic_facts(&live, &[fact]).unwrap();
        }
        // Second occurrence in the same unindexed file: the copy must run
        // once per (import, path), never once per fact row.
        let mut extra = sample_semantic_fact(old, imp, Some("s-null-2"));
        extra.rel_path = "keep.other".to_string();
        extra.file_digest = "dk".to_string();
        extra.file_id = None;
        insert_semantic_facts(&live, &[extra]).unwrap();
        let staging_path = dir.join("staging.db");
        let staging = open_live(&staging_path).unwrap();
        attach_src(&staging, &live_path).unwrap();
        let mut moved = sample_snapshot();
        moved.nonce = "nonce-new".to_string();
        let ns = insert_snapshot(&staging, &moved).unwrap();
        // No syntax rows at all: only the disk verifier can save keep.other.
        let carried = carry_semantic_facts(&staging, ns, old, "rev-new", &|rel| {
            (rel == "keep.other").then(|| "dk".to_string())
        })
        .unwrap();
        assert_eq!(carried.imports, 1);
        assert_eq!(carried.facts, 2, "disk-verified NULL facts carry, no dupes");
        let kept = semantic_facts_for(&staging, ns, "s-null").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rel_path, "keep.other");
        assert_eq!(kept[0].file_id, None, "no fabricated syntax identity");
        assert_eq!(kept[0].file_digest, "dk");
        let kept2 = semantic_facts_for(&staging, ns, "s-null-2").unwrap();
        assert_eq!(kept2.len(), 1);
        detach_src(&staging).unwrap();
    }

    #[test]
    fn db_and_staging_paths_live_under_home_index() {
        let home = tmpdir("store-paths");
        let db = db_path(&home, "myrepo");
        let staging = staging_path(&home, "myrepo");
        assert_eq!(db.file_name().unwrap().to_str().unwrap(), "myrepo.db");
        assert!(staging.to_string_lossy().ends_with("myrepo.db.staging"));
        assert_eq!(db.parent().unwrap(), staging.parent().unwrap());
    }

    #[test]
    fn read_open_classifies_missing_and_v4_and_never_creates() {
        let dir = tmpdir("store-read-open");
        let path = dir.join("x.db");
        assert!(
            matches!(open_read(&path), IndexRead::Missing),
            "missing database must classify as Missing"
        );
        assert!(!path.exists(), "open_read must never create the database");
        {
            let conn = open_live(&path).unwrap();
            insert_snapshot(&conn, &sample_snapshot()).unwrap();
        }
        match open_read(&path) {
            IndexRead::Ready(conn) => {
                assert_eq!(
                    latest_snapshot(&conn).unwrap().unwrap().nonce,
                    "test-nonce-1"
                );
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn read_open_refuses_old_and_new_versions_without_migrating() {
        let dir = tmpdir("store-read-open-ver");
        for version in [1u32, 2, 3, 4, 99] {
            let path = dir.join(format!("v{version}.db"));
            {
                let conn = open_live(&path).unwrap();
                insert_snapshot(&conn, &sample_snapshot()).unwrap();
                conn.execute(&format!("PRAGMA user_version = {version}"), [])
                    .unwrap();
            }
            match (version, open_read(&path)) {
                (v, IndexRead::Outdated(got)) if v < SCHEMA_VERSION => assert_eq!(got, v),
                (v, IndexRead::Ahead(got)) if v > SCHEMA_VERSION => assert_eq!(got, v),
                (v, other) => panic!("v{v}: unexpected {other:?}"),
            }
            // The refusal changed nothing: version marker and row survive.
            let conn = rusqlite::Connection::open(&path).unwrap();
            let got: u32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(got, version);
            assert_eq!(
                latest_snapshot(&conn).unwrap().unwrap().nonce,
                "test-nonce-1"
            );
        }
    }

    #[test]
    fn read_open_refuses_garbage_and_non_regular_paths() {
        let dir = tmpdir("store-read-open-bad");
        let path = dir.join("garbage.db");
        std::fs::write(&path, b"not a database at all").unwrap();
        match open_read(&path) {
            IndexRead::Error(message) => {
                assert!(message.contains("not a readable database"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"not a database at all");
        let as_dir = dir.join("dir.db");
        std::fs::create_dir(&as_dir).unwrap();
        match open_read(&as_dir) {
            IndexRead::Error(message) => {
                assert!(message.contains("not a regular file"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(as_dir.is_dir(), "fail-closed must not replace the path");
    }

    fn sample_occurrence(file_id: i64, name: &str) -> super::ReferenceOccurrenceRow {
        super::ReferenceOccurrenceRow {
            file_id,
            name: name.to_string(),
            role: "member-access".to_string(),
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 2,
            byte_start: 0,
            byte_end: 1,
            provider: "syntax".to_string(),
            grammar: "cpp:0.23.4".to_string(),
            language: "cpp".to_string(),
        }
    }

    fn occurrence_names(conn: &rusqlite::Connection) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT name FROM reference_occurrences ORDER BY name")
            .unwrap();
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        names
    }

    #[test]
    fn a_v4_database_gains_the_empty_occurrence_table_when_it_migrates() {
        let dir = tmpdir("store-occ-migrate");
        let path = dir.join("v4.db");
        {
            // A v4 database is the current schema without the occurrence table.
            let conn = open_live(&path).unwrap();
            conn.execute_batch("DROP TABLE reference_occurrences; PRAGMA user_version = 4;")
                .unwrap();
        }
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert!(!table_exists(&conn, "reference_occurrences"));
        migrate(&conn).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 5, "the occurrence table is schema v5");
        assert!(table_exists(&conn, "reference_occurrences"));
        assert!(occurrence_names(&conn).is_empty());
    }

    #[test]
    fn the_occurrence_census_is_exact_ordered_and_paged() {
        use super::{insert_file, insert_reference_occurrence, insert_symbol, occurrences_for};
        let conn = open_memory().unwrap();
        let snap = insert_snapshot(&conn, &sample_snapshot()).unwrap();
        let file = |path: &str| {
            insert_file(
                &conn,
                snap,
                &FileRow {
                    path: path.to_string(),
                    language: "cpp".to_string(),
                    bytes: 10,
                    digest: "d".to_string(),
                    errors: 0,
                },
            )
            .unwrap()
        };
        let (b, a) = (file("b.cc"), file("a.cc"));
        let symbol = |file_id: i64, kind: &str, name: &str| {
            insert_symbol(
                &conn,
                snap,
                &SymbolRow {
                    file_id,
                    kind: kind.to_string(),
                    name: Some(name.to_string()),
                    start_line: 1,
                    start_col: 1,
                    end_line: 1,
                    end_col: 2,
                    byte_start: 0,
                    byte_end: 1,
                    provider: "syntax".to_string(),
                    grammar: "cpp:0.23.4".to_string(),
                    language: "cpp".to_string(),
                },
            )
            .unwrap();
        };
        // `value` is declared twice (a field and a function); `value_rad` is
        // a different name and must never be listed for `value`.
        symbol(a, "field", "value");
        symbol(b, "function", "value");
        symbol(a, "field", "value_rad");
        let occurrence = |file_id: i64, name: &str, role: &str, byte_start: usize| {
            let mut row = sample_occurrence(file_id, name);
            row.role = role.to_string();
            row.byte_start = byte_start;
            row.byte_end = byte_start + 5;
            insert_reference_occurrence(&conn, snap, &row).unwrap();
        };
        occurrence(b, "value", "member-access", 40);
        occurrence(a, "value", "member-access", 90);
        occurrence(a, "value", "address-of-binding", 10);
        occurrence(a, "value_rad", "member-access", 20);

        let all = occurrences_for(&conn, snap, "value", 50, 0).unwrap();
        assert_eq!(
            (all.field_declarations, all.symbol_declarations, all.total),
            (1, 2, 3)
        );
        let order: Vec<(String, usize)> = all
            .items
            .iter()
            .map(|o| (o.path.clone(), o.byte_start))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a.cc".to_string(), 10),
                ("a.cc".to_string(), 90),
                ("b.cc".to_string(), 40)
            ],
            "ordered by path, then byte offset"
        );
        assert_eq!(all.items[0].role, "address-of-binding");

        let page = occurrences_for(&conn, snap, "value", 1, 1).unwrap();
        assert_eq!((page.total, page.items.len()), (3, 1));
        assert_eq!(page.items[0].byte_start, 90);

        let absent = occurrences_for(&conn, snap, "absent", 50, 0).unwrap();
        assert_eq!(
            (
                absent.field_declarations,
                absent.symbol_declarations,
                absent.total
            ),
            (0, 0, 0)
        );
        let empty = occurrences_for(&conn, snap, "", 50, 0).unwrap();
        assert_eq!((empty.total, empty.items.len()), (0, 0));
    }

    #[test]
    fn promotion_carries_the_new_occurrences_and_drops_the_previous_ones() {
        use super::insert_reference_occurrence;
        let dir = tmpdir("store-occ-promote");
        let live_path = dir.join("live.db");
        let live = open_live(&live_path).unwrap();
        let old = insert_snapshot(&live, &sample_snapshot()).unwrap();
        insert_reference_occurrence(&live, old, &sample_occurrence(1, "leftover")).unwrap();
        let staging_path = dir.join("staging.db");
        let staging = open_live(&staging_path).unwrap();
        let new = insert_snapshot(&staging, &sample_snapshot()).unwrap();
        insert_reference_occurrence(&staging, new, &sample_occurrence(1, "kept_a")).unwrap();
        insert_reference_occurrence(&staging, new, &sample_occurrence(1, "kept_b")).unwrap();
        let base = live_base(&live).unwrap();
        promote_staging(&live, &staging_path, &base).unwrap();
        assert_eq!(
            occurrence_names(&live),
            vec!["kept_a".to_string(), "kept_b".to_string()],
            "exactly the staging rows, none of the previous snapshot's"
        );
    }
}
