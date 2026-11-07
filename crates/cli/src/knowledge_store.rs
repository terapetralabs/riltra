// SPDX-License-Identifier: Apache-2.0
//! M7 knowledge store: separate authoritative SQLite database per registered
//! repository (`$RILTRA_HOME/knowledge/<name>.db`). Schema v3 with guarded,
//! lazy migration from v1 on the first Full-stage write; Foundation (v1)
//! stores stay readable. Never touches the M6 code index.
//!
//! Contracts implemented here (ratified by the M7 spec delta):
//!
//! - **Canonical JSONL port (G-PORT):** lifecycle audit metadata rides as
//!   small structured event objects (`events` arrays mirroring the
//!   append-only `record_lifecycle`/`relation_lifecycle`/`retractions` tables),
//!   never as derived export fields. Events carry their original author,
//!   reason, timestamp and target/revision binding, and round-trip
//!   byte-exactly through export/import.
//! - **Provenance trust boundary (G-PROV):** every record revision and
//!   relation carries `confirm_state` ∈ {`candidate`, `confirmed`}.
//!   `candidate` is the default; `confirmed` is an explicit confirmation
//!   claim, not an authenticated human identity. `confirm_state` is derived
//!   from stored `confirm` lifecycle events — never promoted automatically
//!   from evidence, freshness, search, or indexing. Import preserves
//!   historical `confirm` events from a valid payload for a lossless
//!   round-trip; a confirmation restored that way is historical data, not a
//!   fresh verification by Riltra. Evidence freshness and verification are
//!   independent of `confirm_state`.

// Task 1 leaves this module unwired (CLI dispatch is Task 7); Tasks 2-7
// consume these items. Revisit this allow when the CLI is wired.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};

use crate::knowledge::{NewRecord, RecordVersion, UpdateRecord};
use crate::paths;
use crate::registry::RepoEntry;

/// Independent knowledge schema version (`PRAGMA user_version`). Lifecycle
/// writes migrate v1 to v2; code-anchor writes migrate through v3. Fresh stores
/// remain v1 until an explicit write needs one of those features.
pub const KNOWLEDGE_SCHEMA_VERSION: u32 = 3;

/// Per-repository knowledge database file (never inside repositories).
pub fn knowledge_db_path(home: &Path, repo: &str) -> PathBuf {
    home.join("knowledge").join(format!("{repo}.db"))
}

/// Open the authoritative knowledge store. `create=false` never creates a
/// database or companion files and uses open flags without CREATE.
pub fn open_knowledge(repo: &RepoEntry, create: bool) -> Result<Connection, String> {
    open_knowledge_at(&paths::config_dir(), repo, create)
}

/// Testable form of [`open_knowledge`] with an explicit home directory.
pub fn open_knowledge_at(
    home: &Path,
    repo: &RepoEntry,
    create: bool,
) -> Result<Connection, String> {
    open_inner(home, repo, create, false)
}

/// Open an existing canonical store read-write without creating it or
/// changing its schema. Vector publication runs structural verification and
/// its final freshness check inside `BEGIN IMMEDIATE` on this connection, so
/// a canonical writer cannot commit between that snapshot and manifest
/// publication, including in WAL mode.
pub(crate) fn open_vector_build_guard_at(
    home: &Path,
    repo: &RepoEntry,
) -> Result<Connection, String> {
    if !paths::validate_name(&repo.name) {
        return Err(format!("invalid repository name: '{}'", repo.name));
    }
    let chome = canonical_home(home, false)?;
    let croot = paths::canonical_root(Path::new(&repo.root))
        .map_err(|e| format!("cannot resolve repository root '{}': {e}", repo.root))?;
    let cdir = ensure_knowledge_dir(&chome, false)?;
    let db = cdir.join(format!("{}.db", repo.name));
    let companions = companion_paths(&db);
    for path in std::iter::once(&db).chain(companions.iter()) {
        reject_link(path)?;
    }
    if !db.is_file() {
        return Err(format!("no knowledge store for repository '{}'", repo.name));
    }
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| format!("cannot open knowledge store for '{}': {e}", repo.name))?;
    let cdb = db
        .canonicalize()
        .map_err(|e| format!("knowledge store path escapes for '{}': {e}", repo.name))?;
    if cdb != cdir.join(format!("{}.db", repo.name)) {
        drop(conn);
        return Err(format!(
            "knowledge store path escapes via link for repository '{}'",
            repo.name
        ));
    }
    for path in companions.iter() {
        reject_link(path)?;
    }
    conn.busy_timeout(Duration::from_millis(3000))
        .map_err(|e| format!("cannot set knowledge store timeout: {e}"))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|e| format!("not a readable knowledge database for '{}': {e}", repo.name))?;
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| format!("cannot read knowledge schema version: {e}"))?;
    if !matches!(version, 1..=KNOWLEDGE_SCHEMA_VERSION) {
        return Err(format!(
            "corrupt knowledge store for '{}': unexpected schema version {version}",
            repo.name
        ));
    }
    check_schema_complete(&conn, repo, version)?;
    check_meta(&conn, repo, &croot, version)?;
    Ok(conn)
}

/// Test-only init-failure injector: fails schema initialization after the DDL
/// batch to prove the whole init rolls back atomically.
#[cfg(test)]
pub(crate) fn open_knowledge_at_inject_init_failure(
    home: &Path,
    repo: &RepoEntry,
) -> Result<Connection, String> {
    open_inner(home, repo, true, true)
}

fn open_inner(
    home: &Path,
    repo: &RepoEntry,
    create: bool,
    inject_init_failure: bool,
) -> Result<Connection, String> {
    if !paths::validate_name(&repo.name) {
        return Err(format!("invalid repository name: '{}'", repo.name));
    }
    let chome = canonical_home(home, create)?;
    let croot = paths::canonical_root(Path::new(&repo.root))
        .map_err(|e| format!("cannot resolve repository root '{}': {e}", repo.root))?;
    let cdir = ensure_knowledge_dir(&chome, create)?;
    let db = cdir.join(format!("{}.db", repo.name));
    let companions = companion_paths(&db);
    // Pre-open scan: reject hostile links before SQLite touches the target.
    for p in std::iter::once(&db).chain(companions.iter()) {
        reject_link(p)?;
    }
    let existed = db.is_file();
    if !existed && !create {
        return Err(format!("no knowledge store for repository '{}'", repo.name));
    }
    if !existed && create {
        // Refuse binding a new store to a moved or retargeted root before
        // SQLite creates anything, so a refusal leaves no file behind.
        check_fresh_root_binding(repo, &croot)?;
    }
    if !existed && create && db.exists() {
        // Present but not a file (directory, fifo, …). Links were rejected
        // above, so this is never a dangling link.
        return Err(format!(
            "knowledge store path is not a file for repository '{}'",
            repo.name
        ));
    }
    let mut conn = if create {
        Connection::open(&db)
    } else {
        Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
    }
    .map_err(|e| format!("cannot open knowledge store for '{}': {e}", repo.name))?;
    // Post-open re-check: SQLite follows links, so canonical comparison after
    // open is the normative guard for DB and companions.
    let cdb = db
        .canonicalize()
        .map_err(|e| format!("knowledge store path escapes for '{}': {e}", repo.name))?;
    if cdb != cdir.join(format!("{}.db", repo.name)) {
        drop(conn);
        return Err(format!(
            "knowledge store path escapes via link for repository '{}'",
            repo.name
        ));
    }
    for p in companions.iter() {
        reject_link(p)?;
    }
    conn.busy_timeout(Duration::from_millis(3000))
        .map_err(|e| format!("cannot set knowledge store timeout: {e}"))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|e| format!("not a readable knowledge database for '{}': {e}", repo.name))?;
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| format!("cannot read knowledge schema version: {e}"))?;
    if version > KNOWLEDGE_SCHEMA_VERSION {
        return Err(format!(
            "forward knowledge schema version {version} for '{}'",
            repo.name
        ));
    }
    if !existed {
        init_schema(&mut conn, repo, &croot, inject_init_failure)?;
    } else {
        // Accept Foundation v1, lifecycle v2 and code-binding v3. A
        // version-0/garbage stamp and any disagreement between the two
        // version stamps fail closed; the store is never deleted or rebuilt.
        if !matches!(version, 1..=KNOWLEDGE_SCHEMA_VERSION) {
            return Err(format!(
                "corrupt knowledge store for '{}': unexpected schema version {version}",
                repo.name
            ));
        }
        check_schema_complete(&conn, repo, version)?;
        check_meta(&conn, repo, &croot, version)?;
    }
    // Best-effort like the code index: exotic filesystems fall back to
    // rollback mode (still correct, less concurrent). Read-only opens ignore
    // the result as well.
    //
    // NOTE (Task 2): WAL mode activates here, after the scans above. Later
    // SQLite accesses can create or touch `-wal`/`-shm` files, so every write
    // transaction must re-validate companions before committing; the checks in
    // this function cover the open instant only. No TOCTOU protection beyond
    // SQLite transactions and bounded writer waits is claimed.
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
    Ok(conn)
}

fn companion_paths(db: &Path) -> [PathBuf; 3] {
    fn with_suffix(db: &Path, suffix: &str) -> PathBuf {
        let mut os = db.as_os_str().to_os_string();
        os.push(suffix);
        PathBuf::from(os)
    }
    [
        with_suffix(db, "-wal"),
        with_suffix(db, "-shm"),
        with_suffix(db, "-journal"),
    ]
}

/// Total metadata inspections allowed at a store-controlled path. Windows
/// retries a transient `PermissionDenied` (os error 5) that antivirus or
/// filter drivers can raise while holding `-wal`/`-shm` during SQLite cleanup
/// (see `docs/audits/2026-10-19-windows-wal-concurrency.md`); every other
/// platform keeps the original single inspection.
const INSPECT_ATTEMPTS: u32 = if cfg!(windows) { 3 } else { 1 };

/// Fixed pause between retried inspections, mirroring SQLite's documented
/// 25 ms Windows antivirus-retry interval.
const INSPECT_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Any symlink at a store-controlled path is an escape, including dangling
/// links (`symlink_metadata` succeeds on those). A missing path is absent;
/// any other I/O error fails closed instead of being treated as absent.
fn reject_link(p: &Path) -> Result<(), String> {
    reject_link_with(p, |q| std::fs::symlink_metadata(q))
}

/// Testable form of [`reject_link`]: `inspect` is the raw metadata call
/// (production passes `std::fs::symlink_metadata`), so tests can script
/// inspection outcomes without changing the retry or classification rules.
fn reject_link_with<F>(p: &Path, inspect: F) -> Result<(), String>
where
    F: Fn(&Path) -> std::io::Result<std::fs::Metadata>,
{
    let mut attempts = INSPECT_ATTEMPTS;
    loop {
        match inspect(p) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempts > 1 => {
                attempts -= 1;
                std::thread::sleep(INSPECT_RETRY_DELAY);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(format!(
                    "cannot inspect knowledge store path '{}': {e}",
                    p.display()
                ))
            }
            Ok(md) if md.file_type().is_symlink() => {
                return Err(format!(
                    "knowledge store path escapes via link: '{}'",
                    p.display()
                ))
            }
            Ok(_) => return Ok(()),
        }
    }
}

fn canonical_home(home: &Path, create: bool) -> Result<PathBuf, String> {
    match home.canonicalize() {
        Ok(c) => Ok(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            std::fs::create_dir_all(home)
                .map_err(|e| format!("cannot create home '{}': {e}", home.display()))?;
            home.canonicalize()
                .map_err(|e| format!("cannot resolve home '{}': {e}", home.display()))
        }
        Err(e) => Err(format!("cannot resolve home '{}': {e}", home.display())),
    }
}

/// Validate the knowledge directory before SQLite open: a symlink is an
/// escape/retarget, a non-directory is an error, a missing directory is
/// created only when `create` is set.
fn ensure_knowledge_dir(chome: &Path, create: bool) -> Result<PathBuf, String> {
    let kdir = chome.join("knowledge");
    match std::fs::symlink_metadata(&kdir) {
        Ok(md) if md.file_type().is_symlink() => Err(format!(
            "knowledge directory escapes via link: '{}'",
            kdir.display()
        )),
        Ok(md) if md.is_dir() => {
            let c = kdir.canonicalize().map_err(|e| {
                format!(
                    "cannot resolve knowledge directory '{}': {e}",
                    kdir.display()
                )
            })?;
            if c != chome.join("knowledge") {
                return Err(format!(
                    "knowledge directory retargeted: '{}'",
                    kdir.display()
                ));
            }
            Ok(c)
        }
        Ok(_) => Err(format!(
            "knowledge path is not a directory: '{}'",
            kdir.display()
        )),
        Err(_) => {
            // Missing and unusable paths (e.g. ENOTDIR under a file on Unix)
            // share one outcome: without `create` the store is absent; with
            // `create` the attempt fails here with a uniform message on every
            // platform instead of leaking OS-specific error kinds.
            if !create {
                return Err("no knowledge store: knowledge directory is absent".to_string());
            }
            std::fs::create_dir_all(&kdir).map_err(|e| {
                format!(
                    "cannot create knowledge directory '{}': {e}",
                    kdir.display()
                )
            })?;
            let c = kdir.canonicalize().map_err(|e| {
                format!(
                    "cannot resolve knowledge directory '{}': {e}",
                    kdir.display()
                )
            })?;
            if c != chome.join("knowledge") {
                return Err(format!(
                    "knowledge directory retargeted: '{}'",
                    kdir.display()
                ));
            }
            Ok(c)
        }
    }
}

/// Resolve the restore target without opening or creating a store: canonical
/// home, ensured knowledge directory, and the store path with its companion
/// paths, rejecting links at every store-controlled path. Restore resolves
/// paths this way so a refused restore never creates an empty store first.
pub fn restore_target_paths(
    home: &Path,
    repo: &RepoEntry,
) -> Result<(PathBuf, [PathBuf; 3]), String> {
    let chome = canonical_home(home, true)?;
    let cdir = ensure_knowledge_dir(&chome, true)?;
    let db = cdir.join(format!("{}.db", repo.name));
    let companions = companion_paths(&db);
    reject_link(&db)?;
    for p in companions.iter() {
        reject_link(p)?;
    }
    Ok((db, companions))
}

const SCHEMA_SQL: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE records (
  id TEXT NOT NULL, revision INTEGER NOT NULL,
  kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
  status TEXT NOT NULL, author TEXT NOT NULL, created_unix INTEGER NOT NULL,
  PRIMARY KEY (id, revision)
);
CREATE TABLE heads (id TEXT PRIMARY KEY, current_revision INTEGER NOT NULL);
CREATE TABLE evidence (
  id TEXT NOT NULL, revision INTEGER NOT NULL, seq INTEGER NOT NULL,
  path TEXT NOT NULL, byte_start INTEGER, byte_end INTEGER,
  digest TEXT NOT NULL, git_head TEXT, git_branch TEXT,
  PRIMARY KEY (id, revision, seq)
);
CREATE TABLE relations (
  relation_id TEXT PRIMARY KEY,
  src_id TEXT NOT NULL, src_revision INTEGER NOT NULL,
  dst_id TEXT NOT NULL, dst_revision INTEGER NOT NULL,
  kind TEXT NOT NULL, author TEXT NOT NULL,
  rationale TEXT NOT NULL, created_unix INTEGER NOT NULL
);
CREATE TABLE retractions (
  relation_id TEXT PRIMARY KEY,
  author TEXT NOT NULL, reason TEXT NOT NULL, created_unix INTEGER NOT NULL
);
CREATE TABLE measurements (
  id TEXT NOT NULL, revision INTEGER NOT NULL,
  method TEXT NOT NULL, environment TEXT NOT NULL, sample_count INTEGER NOT NULL,
  PRIMARY KEY (id, revision)
);
CREATE TABLE metric_values (
  id TEXT NOT NULL, revision INTEGER NOT NULL, name TEXT NOT NULL,
  value REAL NOT NULL, unit TEXT NOT NULL,
  PRIMARY KEY (id, revision, name)
);
CREATE VIRTUAL TABLE knowledge_fts USING fts5(id UNINDEXED, title, body);
-- FTS holds exactly the current head revision per record id. Only head moves
-- (inserts and updates of `heads.current_revision`) maintain the index, in
-- the same transaction as the move, so rolling back a head move rolls back
-- the index with it. Inserting a revision without moving the head never
-- touches the index, and immutable historical rows are never removed by it.
CREATE TRIGGER heads_fts_sync_insert AFTER INSERT ON heads BEGIN
  DELETE FROM knowledge_fts WHERE id = new.id;
  INSERT INTO knowledge_fts(id, title, body)
  SELECT id, title, body FROM records
  WHERE id = new.id AND revision = new.current_revision;
END;
CREATE TRIGGER heads_fts_sync_update AFTER UPDATE OF current_revision ON heads BEGIN
  DELETE FROM knowledge_fts WHERE id = new.id;
  INSERT INTO knowledge_fts(id, title, body)
  SELECT id, title, body FROM records
  WHERE id = new.id AND revision = new.current_revision;
END;
";

/// Build schema v1 on a file that did not exist before open, inside one
/// SQLite transaction: DDL, metadata binding, generation init and the version
/// stamp commit together or roll back together. Existing files are never
/// initialized here: garbage or version-0 files fail closed.
fn init_schema(
    conn: &mut Connection,
    repo: &RepoEntry,
    croot: &Path,
    inject_init_failure: bool,
) -> Result<(), String> {
    let tx = conn
        .transaction()
        .map_err(|e| format!("cannot begin knowledge schema transaction: {e}"))?;
    tx.execute_batch(SCHEMA_SQL)
        .map_err(|e| format!("cannot create knowledge schema: {e}"))?;
    if inject_init_failure {
        // Test hook: drop the transaction without committing to prove no
        // partial schema or metadata survives initialization failure.
        return Err("injected init failure".to_string());
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('repo_name', ?1)",
        [repo.name.as_str()],
    )
    .map_err(|e| format!("cannot bind knowledge metadata: {e}"))?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('canonical_root', ?1)",
        [croot.to_string_lossy().as_ref()],
    )
    .map_err(|e| format!("cannot bind knowledge metadata: {e}"))?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('knowledge_schema_version', '1')",
        [],
    )
    .map_err(|e| format!("cannot bind knowledge metadata: {e}"))?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('knowledge_generation', '0')",
        [],
    )
    .map_err(|e| format!("cannot bind knowledge metadata: {e}"))?;
    tx.execute_batch("PRAGMA user_version = 1")
        .map_err(|e| format!("cannot stamp knowledge schema version: {e}"))?;
    tx.commit()
        .map_err(|e| format!("cannot commit knowledge schema: {e}"))?;
    Ok(())
}

/// Objects that must exist for a schema-v1 store to be accepted. A matching
/// `user_version` and readable `meta` alone are not enough: an existing
/// database missing any required object fails explicitly here.
const REQUIRED_TABLES: [&str; 9] = [
    "meta",
    "records",
    "heads",
    "evidence",
    "relations",
    "retractions",
    "measurements",
    "metric_values",
    "knowledge_fts",
];
const REQUIRED_TRIGGERS: [&str; 2] = ["heads_fts_sync_insert", "heads_fts_sync_update"];
/// Additional tables a schema-v2 store must contain. A v2 store without them
/// is corrupt and fails closed at open.
const LIFECYCLE_TABLES: [&str; 2] = ["record_lifecycle", "relation_lifecycle"];
const CODE_BINDING_TABLES: [&str; 2] = ["code_bindings", "code_binding_retractions"];

/// Schema v2 additions, created only by the guarded v1→v2 migration inside a
/// Full-stage write transaction. `record_lifecycle` is append-only, keyed by
/// the revision it describes; `relation_lifecycle` is append-only, keyed by
/// the confirmed relation. Neither table has an update or delete path.
const MIGRATION_V2_SQL: &str = "
CREATE TABLE IF NOT EXISTS record_lifecycle (
  id TEXT NOT NULL, revision INTEGER NOT NULL, event TEXT NOT NULL,
  target_id TEXT, target_revision INTEGER, reason TEXT,
  author TEXT NOT NULL, created_unix INTEGER NOT NULL,
  PRIMARY KEY (id, revision)
);
CREATE TABLE IF NOT EXISTS relation_lifecycle (
  relation_id TEXT PRIMARY KEY, event TEXT NOT NULL,
  author TEXT NOT NULL, created_unix INTEGER NOT NULL
);
";

/// Guarded v1→v2 migration, run as the first step of a Full-stage write
/// transaction. Reads never call this; a v2 store is a no-op. The migration
/// only creates two empty tables and moves the version metadata — existing
/// rows stay byte-identical and the generation is untouched. Because it
/// shares the caller's transaction, any later failure (or a failure here)
/// rolls back the tables and both version stamps together; a crash after
/// commit leaves a consistent v2 store and never a partial migration.
fn ensure_lifecycle_schema(tx: &Transaction) -> Result<(), String> {
    ensure_lifecycle_schema_inner(tx, false)
}

fn ensure_lifecycle_schema_inner(tx: &Transaction, inject_failure: bool) -> Result<(), String> {
    let version: u32 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| format!("cannot read knowledge schema version: {e}"))?;
    match version {
        2 | 3 => Ok(()),
        1 => {
            tx.execute_batch(MIGRATION_V2_SQL)
                .map_err(|e| format!("cannot migrate knowledge schema: {e}"))?;
            if inject_failure {
                // Test hook: fail after table creation and before either
                // version stamp, proving the whole migration rolls back.
                return Err("injected migration failure".to_string());
            }
            let stamped = tx
                .execute(
                    "UPDATE meta SET value = '2' WHERE key = 'knowledge_schema_version'",
                    [],
                )
                .map_err(|e| format!("cannot migrate knowledge schema: {e}"))?;
            if stamped != 1 {
                return Err("corrupt knowledge store: schema marker missing".to_string());
            }
            tx.execute_batch("PRAGMA user_version = 2")
                .map_err(|e| format!("cannot migrate knowledge schema: {e}"))?;
            Ok(())
        }
        _ => Err(format!(
            "corrupt knowledge store: unexpected schema version {version}"
        )),
    }
}

const MIGRATION_V3_SQL: &str = "
CREATE TABLE IF NOT EXISTS code_bindings (
  binding_id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo TEXT NOT NULL,
  repo_root_digest TEXT NOT NULL CHECK(length(repo_root_digest)=16 AND repo_root_digest NOT GLOB '*[^0-9a-f]*'),
  snapshot_id INTEGER NOT NULL, snapshot_nonce TEXT NOT NULL,
  snapshot_digest TEXT NOT NULL CHECK(length(snapshot_digest)=16 AND snapshot_digest NOT GLOB '*[^0-9a-f]*'),
  source_digest TEXT NOT NULL CHECK(length(source_digest)=16 AND source_digest NOT GLOB '*[^0-9a-f]*'),
  path TEXT NOT NULL, byte_start INTEGER NOT NULL CHECK(byte_start >= 0),
  byte_end INTEGER NOT NULL CHECK(byte_end > byte_start),
  cursor_line INTEGER NOT NULL CHECK(cursor_line > 0),
  cursor_col INTEGER NOT NULL CHECK(cursor_col > 0),
  symbol_uri TEXT, display_name TEXT,
  anchor_kind TEXT NOT NULL CHECK(anchor_kind IN ('semantic','syntax-fallback')),
  precision TEXT NOT NULL CHECK(precision IN ('Exact','Heuristic')),
  record_id TEXT NOT NULL, record_revision INTEGER NOT NULL CHECK(record_revision > 0),
  relation TEXT NOT NULL CHECK(relation IN ('documents','constrains','implements','tests','motivates','observes')),
  author TEXT NOT NULL CHECK(author <> ''),
  rationale TEXT NOT NULL CHECK(rationale <> ''),
  provenance TEXT NOT NULL CHECK(provenance <> ''),
  created_unix INTEGER NOT NULL CHECK(created_unix >= 0)
);
CREATE INDEX IF NOT EXISTS code_bindings_anchor ON code_bindings(repo, path, byte_start);
CREATE TABLE IF NOT EXISTS code_binding_retractions (
  binding_id INTEGER PRIMARY KEY,
  author TEXT NOT NULL CHECK(author <> ''),
  reason TEXT NOT NULL CHECK(reason <> ''),
  created_unix INTEGER NOT NULL CHECK(created_unix >= 0)
);
";

/// Create v3's explicit code-anchor links inside the caller's write transaction.
/// Read paths never call this; v1/v2 remain readable until an explicit write.
pub(crate) fn ensure_code_binding_schema(tx: &Transaction) -> Result<(), String> {
    ensure_lifecycle_schema(tx)?;
    let version: u32 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| format!("cannot read knowledge schema version: {e}"))?;
    match version {
        3 => Ok(()),
        2 => {
            tx.execute_batch(MIGRATION_V3_SQL)
                .map_err(|e| format!("cannot migrate knowledge schema: {e}"))?;
            let stamped = tx
                .execute(
                    "UPDATE meta SET value = '3' WHERE key = 'knowledge_schema_version'",
                    [],
                )
                .map_err(|e| format!("cannot migrate knowledge schema: {e}"))?;
            if stamped != 1 {
                return Err("corrupt knowledge store: schema marker missing".to_string());
            }
            tx.execute_batch("PRAGMA user_version = 3")
                .map_err(|e| format!("cannot migrate knowledge schema: {e}"))?;
            Ok(())
        }
        _ => Err(format!(
            "corrupt knowledge store: unexpected schema version {version}"
        )),
    }
}

/// Test-only entry that exercises the migration exactly as a Full-stage write
/// would: one immediate transaction, migration first, commit. Later tasks
/// wire `ensure_lifecycle_schema` into the real write paths.
#[cfg(test)]
fn migrate_to_v2_for_test(conn: &mut Connection, inject_failure: bool) -> Result<(), String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    ensure_lifecycle_schema_inner(&tx, inject_failure)?;
    tx.commit()
        .map_err(|e| format!("cannot commit knowledge migration: {e}"))?;
    Ok(())
}

fn check_schema_complete(conn: &Connection, repo: &RepoEntry, version: u32) -> Result<(), String> {
    let tables = REQUIRED_TABLES
        .iter()
        .copied()
        .chain(LIFECYCLE_TABLES.iter().copied().filter(|_| version >= 2))
        .chain(CODE_BINDING_TABLES.iter().copied().filter(|_| version >= 3));
    for table in tables {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .map_err(|_| "corrupt knowledge store: schema unreadable".to_string())?;
        if n != 1 {
            return Err(format!(
                "incomplete knowledge schema for '{}': missing table '{table}'",
                repo.name
            ));
        }
    }
    for trigger in REQUIRED_TRIGGERS {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
                [trigger],
                |row| row.get(0),
            )
            .map_err(|_| "corrupt knowledge store: schema unreadable".to_string())?;
        if n != 1 {
            return Err(format!(
                "incomplete knowledge schema for '{}': missing trigger '{trigger}'",
                repo.name
            ));
        }
    }
    Ok(())
}

fn meta_value(conn: &Connection, key: &str) -> Result<String, String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .map_err(|_| "corrupt knowledge store: metadata unreadable".to_string())
}

/// Fresh-create guard: registration records the canonical root string, and
/// every read command re-resolves the entry and compares (see `scan`/`status`
/// handling). A live canonical form that differs means the tree moved or the
/// entry was retargeted before the first knowledge store was created, so the
/// new store is refused instead of silently binding another tree.
fn check_fresh_root_binding(repo: &RepoEntry, croot: &Path) -> Result<(), String> {
    if croot.to_string_lossy().as_ref() != repo.root {
        return Err(format!(
            "knowledge store creation refused for '{}': repository root retargeted",
            repo.name
        ));
    }
    Ok(())
}

/// The store belongs to exactly one registered repository: name, canonical
/// root and schema marker must all match on every open. The marker must
/// agree with the store's own `user_version` (v1 stores carry '1', migrated
/// stores '2'), so a store whose version stamps disagree fails closed. The
/// stored root must also still resolve to itself, so a moved or deleted root
/// fails instead of being accepted on a stale string comparison.
fn check_meta(
    conn: &Connection,
    repo: &RepoEntry,
    croot: &Path,
    version: u32,
) -> Result<(), String> {
    let name = meta_value(conn, "repo_name")?;
    let root = meta_value(conn, "canonical_root")?;
    let marker = meta_value(conn, "knowledge_schema_version")?;
    let root_alive = paths::canonical_root(Path::new(&root))
        .map(|live| live.to_string_lossy().into_owned() == root)
        .unwrap_or(false);
    if name != repo.name
        || root != croot.to_string_lossy().as_ref()
        || marker != version.to_string()
        || !root_alive
    {
        return Err(format!(
            "knowledge metadata mismatch for repository '{}'",
            repo.name
        ));
    }
    Ok(())
}

/// Outcome of a structural verification of one knowledge database.
#[derive(Debug, PartialEq)]
pub struct StoreCheck {
    pub schema_version: u32,
    pub records: u64,
    pub relations: u64,
    pub code_bindings: u64,
}

/// Structural verification of an already-open knowledge database, used by
/// `knowledge verify` on the live store and by restore on the staged
/// snapshot before promotion: `PRAGMA integrity_check` must pass, both
/// version stamps must agree on an accepted schema version, the required
/// tables for that version must exist, the meta binding must name `repo` at
/// `croot`, and counts are reported for manifest comparison. Structural
/// only: no digest and no authenticity claim (spec D6).
pub fn verify_store(
    conn: &Connection,
    repo: &RepoEntry,
    croot: &Path,
) -> Result<StoreCheck, String> {
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| format!("corrupt knowledge store: integrity check failed: {e}"))?;
    if verdict != "ok" {
        return Err(format!(
            "corrupt knowledge store: integrity check reported: {verdict}"
        ));
    }
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| format!("corrupt knowledge store: cannot read schema version: {e}"))?;
    if version > KNOWLEDGE_SCHEMA_VERSION {
        return Err(format!(
            "forward knowledge schema version {version} for '{}'",
            repo.name
        ));
    }
    if !matches!(version, 1..=KNOWLEDGE_SCHEMA_VERSION) {
        return Err(format!(
            "corrupt knowledge store for '{}': unexpected schema version {version}",
            repo.name
        ));
    }
    check_schema_complete(conn, repo, version)?;
    check_meta(conn, repo, croot, version)?;
    let records: i64 = conn
        .query_row("SELECT count(DISTINCT id) FROM records", [], |row| {
            row.get(0)
        })
        .map_err(|e| format!("corrupt knowledge store: cannot count records: {e}"))?;
    let relations: i64 = conn
        .query_row("SELECT count(*) FROM relations", [], |row| row.get(0))
        .map_err(|e| format!("corrupt knowledge store: cannot count relations: {e}"))?;
    let code_bindings = if version >= 3 {
        crate::knowledge_binding::verify_store_rows(conn, repo, croot)?
    } else {
        0
    };
    Ok(StoreCheck {
        schema_version: version,
        records: u64::try_from(records)
            .map_err(|_| "corrupt knowledge store: negative record count".to_string())?,
        relations: u64::try_from(relations)
            .map_err(|_| "corrupt knowledge store: negative relation count".to_string())?,
        code_bindings,
    })
}

/// One page of immutable revision history with its generation guard.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPage {
    pub revisions: Vec<RecordVersion>,
    pub total: usize,
    pub truncated: bool,
    pub generation: u64,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Generation guard for a genuinely absent store: the empty store is
/// generation 0, so only an omitted or zero expectation may succeed.
pub fn check_missing_store_generation(generation: Option<u64>) -> Result<(), String> {
    match generation {
        Some(expected) if expected != 0 => Err(format!(
            "knowledge generation conflict: expected {expected} but store is at 0"
        )),
        _ => Ok(()),
    }
}

/// Current monotonic knowledge generation (bumped by every mutation).
pub fn read_generation(conn: &Connection) -> Result<u64, String> {
    let raw: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'knowledge_generation'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "corrupt knowledge store: generation unreadable".to_string())?;
    raw.parse::<u64>()
        .map_err(|_| "corrupt knowledge store: generation unreadable".to_string())
}

pub(crate) fn bump_generation(tx: &Transaction) -> Result<u64, String> {
    let current = read_generation(tx)?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| "knowledge generation overflow".to_string())?;
    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'knowledge_generation'",
        [next.to_string()],
    )
    .map_err(|e| format!("cannot bump knowledge generation: {e}"))?;
    Ok(next)
}

/// Re-check the database and companion paths immediately before a write
/// transaction starts: a link planted between open and write refuses the
/// mutation with nothing committed. This covers the check instant only;
/// races between this scan and SQLite's own access are not claimed away
/// (TOCTOU), only narrowed to that window on top of SQLite's locking.
pub(crate) fn revalidate_open_paths(conn: &Connection) -> Result<(), String> {
    if let Some(path) = conn.path() {
        let db = Path::new(path);
        reject_link(db)?;
        for companion in companion_paths(db) {
            reject_link(&companion)?;
        }
    }
    Ok(())
}

fn record_from_row(row: &rusqlite::Row) -> rusqlite::Result<RecordVersion> {
    let revision_i64: i64 = row.get(1)?;
    let revision = u64::try_from(revision_i64).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("negative revision {revision_i64}"),
            )),
        )
    })?;
    Ok(RecordVersion {
        id: row.get(0)?,
        revision,
        kind: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        status: row.get(5)?,
        author: row.get(6)?,
        created_unix: row.get(7)?,
        superseded_by: None,
        retracted_reason: None,
        retracted_author: None,
        confirm_state: None,
        measurement: None,
        evidence: Vec::new(),
    })
}

const RECORD_COLS: &str = "id, revision, kind, title, body, status, author, created_unix";

/// Stored binding root: the canonical root this store is bound to (verified
/// against the registry entry at open), resolved from metadata so evidence
/// capture needs no extra caller plumbing. Fails closed when the root is
/// gone: no binding is fabricated.
fn capture_root(conn: &Connection) -> Result<PathBuf, String> {
    let path = PathBuf::from(meta_value(conn, "canonical_root")?);
    if !path.is_dir() {
        return Err("cannot capture evidence: repository root unavailable".to_string());
    }
    Ok(path)
}

/// Capture evidence bindings before any mutation: confined bounded reads
/// (confinement, symlink escape, readability, 1 MiB cap) plus a span-bounds
/// check against the bytes actually read. Any failure rejects the whole
/// mutation before a write transaction starts. Digests cover the cited span
/// slice, or the whole file when no span is given. Git HEAD/branch is a
/// best-effort observation, never a failure.
fn capture_evidence(
    conn: &Connection,
    refs: &[crate::knowledge::EvidenceRef],
) -> Result<Vec<crate::knowledge::CapturedEvidence>, String> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let root = capture_root(conn)?;
    let git = crate::git::fingerprint(&root);
    let mut out = Vec::with_capacity(refs.len());
    for item in refs {
        let bytes = crate::semantic::confined_read(&root, &item.path, crate::scan::MAX_FILE_BYTES)
            .map_err(|e| format!("cannot capture evidence '{}': {e}", item.path))?;
        let len = bytes.len() as u64;
        // Parser guarantees non-negative bounds and byte_end > byte_start
        // when both are present; the file decides whether they still bind.
        let start = item.byte_start.map(|v| v as u64).unwrap_or(0);
        let end = item.byte_end.map(|v| v as u64).unwrap_or(len);
        let explicit_span = item.byte_start.is_some() || item.byte_end.is_some();
        if end > len || start > len || (explicit_span && start >= end) {
            return Err(format!(
                "cannot capture evidence '{}': span [{start},{end}) binds no bytes \
                 in {} source bytes",
                item.path,
                bytes.len()
            ));
        }
        let cited = &bytes[start as usize..end as usize];
        out.push(crate::knowledge::CapturedEvidence {
            path: item.path.clone(),
            byte_start: item.byte_start,
            byte_end: item.byte_end,
            digest: crate::store::content_digest(cited),
            git_head: git.head.clone(),
            git_branch: git.branch.clone(),
        });
    }
    Ok(out)
}

fn write_evidence(
    tx: &Transaction,
    id: &str,
    revision: i64,
    items: &[crate::knowledge::CapturedEvidence],
) -> Result<(), String> {
    for (seq, item) in items.iter().enumerate() {
        tx.execute(
            "INSERT INTO evidence (id, revision, seq, path, byte_start, byte_end,
              digest, git_head, git_branch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                id,
                revision,
                seq as i64,
                item.path,
                item.byte_start,
                item.byte_end,
                item.digest,
                item.git_head,
                item.git_branch,
            ],
        )
        .map_err(|e| format!("cannot store evidence: {e}"))?;
    }
    Ok(())
}

fn read_evidence(
    conn: &Connection,
    id: &str,
    revision: i64,
) -> Result<Vec<crate::knowledge::CapturedEvidence>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT path, byte_start, byte_end, digest, git_head, git_branch
             FROM evidence WHERE id = ?1 AND revision = ?2 ORDER BY seq",
        )
        .map_err(|e| format!("cannot read evidence: {e}"))?;
    let items = stmt
        .query_map(rusqlite::params![id, revision], |row| {
            Ok(crate::knowledge::CapturedEvidence {
                path: row.get(0)?,
                byte_start: row.get(1)?,
                byte_end: row.get(2)?,
                digest: row.get(3)?,
                git_head: row.get(4)?,
                git_branch: row.get(5)?,
            })
        })
        .map_err(|e| format!("cannot read evidence: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot read evidence: {e}"))?;
    Ok(items)
}

/// Sample one stored binding against a live root. Knowledge rows come from
/// the caller's read; this sampling is separate and reported honestly:
/// neither the row read nor an unchanged Git HEAD snapshots the tree, and a
/// matching digest never verifies the prose.
pub fn evidence_freshness(
    recorded: &crate::knowledge::CapturedEvidence,
    repo_root: &Path,
) -> crate::knowledge::EvidenceFreshness {
    use crate::knowledge::EvidenceFreshness as Fresh;
    let bytes = match crate::semantic::confined_read(
        repo_root,
        &recorded.path,
        crate::scan::MAX_FILE_BYTES,
    ) {
        Ok(found) => found,
        Err(e) => return Fresh::Unresolved { reason: e },
    };
    let len = bytes.len() as u64;
    let start = recorded.byte_start.map(|v| v as u64).unwrap_or(0);
    let end = recorded.byte_end.map(|v| v as u64).unwrap_or(len);
    // A span that no longer fits means the cited bytes changed (truncated
    // away): stale, with the current full-file digest as observed state. A
    // bare path on an empty file still binds (start == end == 0), matching
    // capture; only an explicit span can go stale-empty.
    let explicit_span = recorded.byte_start.is_some() || recorded.byte_end.is_some();
    if end > len || start > len || (explicit_span && start >= end) {
        return Fresh::Stale {
            observed_digest: crate::store::content_digest(&bytes),
        };
    }
    let live = crate::store::content_digest(&bytes[start as usize..end as usize]);
    if live == recorded.digest {
        Fresh::Fresh
    } else {
        Fresh::Stale {
            observed_digest: live,
        }
    }
}

/// Create revision 1 of a new record. Fails on duplicates, invalid input, or
/// path revalidation; failures commit nothing and bump no generation.
pub fn create_record(conn: &mut Connection, input: &NewRecord) -> Result<(u64, u64), String> {
    create_inner(conn, input, None, &[], false)
}

/// Create revision 1 from strictly validated JSON input, persisting benchmark
/// measurement and captured evidence atomically with the revision.
pub fn create_validated(
    conn: &mut Connection,
    input: &crate::knowledge::ValidatedNew,
) -> Result<(u64, u64), String> {
    create_inner(
        conn,
        &input.record,
        input.measurement.as_ref(),
        &input.evidence,
        false,
    )
}

/// Test-only fault injector: fails after the records and evidence inserts to
/// prove the whole creation rolls back.
#[cfg(test)]
pub(crate) fn create_validated_with_fault(
    conn: &mut Connection,
    input: &crate::knowledge::ValidatedNew,
) -> Result<(u64, u64), String> {
    create_inner(
        conn,
        &input.record,
        input.measurement.as_ref(),
        &input.evidence,
        true,
    )
}

fn write_measurement(
    tx: &Transaction,
    id: &str,
    revision: i64,
    measurement: &crate::knowledge::BenchmarkData,
) -> Result<(), String> {
    tx.execute(
        "INSERT INTO measurements (id, revision, method, environment, sample_count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            id,
            revision,
            measurement.method,
            measurement.environment,
            i64::try_from(measurement.sample_count)
                .map_err(|_| "sample count out of range".to_string())?,
        ],
    )
    .map_err(|e| format!("cannot store measurement: {e}"))?;
    for metric in &measurement.metrics {
        tx.execute(
            "INSERT INTO metric_values (id, revision, name, value, unit)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, revision, metric.name, metric.value, metric.unit],
        )
        .map_err(|e| format!("cannot store metric: {e}"))?;
    }
    Ok(())
}

fn create_inner(
    conn: &mut Connection,
    input: &NewRecord,
    measurement: Option<&crate::knowledge::BenchmarkData>,
    evidence: &[crate::knowledge::EvidenceRef],
    inject_fault: bool,
) -> Result<(u64, u64), String> {
    crate::knowledge::validate_new(input)?;
    if input.kind == "benchmark" && measurement.is_none() {
        return Err("benchmark records require measurement".to_string());
    }
    revalidate_open_paths(conn)?;
    // Source bindings are sampled before the write transaction starts: any
    // confinement, readability, cap or span failure rejects the mutation
    // with nothing committed.
    let captured = capture_evidence(conn, evidence)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    let exists: i64 = tx
        .query_row(
            "SELECT count(*) FROM heads WHERE id = ?1",
            [&input.id],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot check record existence: {e}"))?;
    if exists != 0 {
        return Err(format!("duplicate record id: '{}'", input.id));
    }
    tx.execute(
        "INSERT INTO records (id, revision, kind, title, body, status, author, created_unix)
         VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            input.id,
            input.kind,
            input.title,
            input.body,
            input.status,
            input.author,
            now_unix()
        ],
    )
    .map_err(|e| format!("cannot insert record revision: {e}"))?;
    // Evidence rows precede the fault hook so the injector proves they roll
    // back with everything else; heads, FTS, measurement and generation join
    // the same transaction below.
    write_evidence(&tx, &input.id, 1, &captured)?;
    if inject_fault {
        // Test hook: drop the transaction here to prove partial writes roll
        // back fully (no head, FTS, evidence, measurement or generation survives).
        return Err("injected write failure".to_string());
    }
    tx.execute(
        "INSERT INTO heads (id, current_revision) VALUES (?1, 1)",
        [&input.id],
    )
    .map_err(|e| format!("cannot advance record head: {e}"))?;
    if let Some(data) = measurement {
        write_measurement(&tx, &input.id, 1, data)?;
    }
    let generation = bump_generation(&tx)?;
    tx.commit()
        .map_err(|e| format!("cannot commit record creation: {e}"))?;
    Ok((1, generation))
}

/// Append a new immutable revision. The expected head is verified inside the
/// write transaction, so concurrent writers serialize: exactly one wins and
/// the loser fails with a conflict, committing nothing.
pub fn update_record(
    conn: &mut Connection,
    id: &str,
    expected_revision: u64,
    input: &UpdateRecord,
) -> Result<(u64, u64), String> {
    update_inner(conn, id, expected_revision, input, None, &[], false)
}

/// Update from strictly validated JSON input. A supplied measurement replaces
/// the claim on the new revision; when absent on a benchmark revision, the
/// previous revision's measurement is carried forward unchanged so reads stay
/// complete. Evidence refs are captured fresh for the new revision; older
/// revisions keep their original bindings.
pub fn update_validated(
    conn: &mut Connection,
    id: &str,
    expected_revision: u64,
    input: &crate::knowledge::ValidatedUpdate,
) -> Result<(u64, u64), String> {
    update_inner(
        conn,
        id,
        expected_revision,
        &input.record,
        input.measurement.as_ref(),
        &input.evidence,
        false,
    )
}

/// Test-only fault injector: fails after the records and evidence inserts to
/// prove the whole update rolls back.
#[cfg(test)]
pub(crate) fn update_validated_with_fault(
    conn: &mut Connection,
    id: &str,
    expected_revision: u64,
    input: &crate::knowledge::ValidatedUpdate,
) -> Result<(u64, u64), String> {
    update_inner(
        conn,
        id,
        expected_revision,
        &input.record,
        input.measurement.as_ref(),
        &input.evidence,
        true,
    )
}

/// Carry a benchmark claim forward to a new revision unchanged.
fn carry_measurement(tx: &Transaction, id: &str, from_rev: i64, to_rev: i64) -> Result<(), String> {
    let copied = tx
        .execute(
            "INSERT INTO measurements (id, revision, method, environment, sample_count)
             SELECT ?1, ?2, method, environment, sample_count FROM measurements
             WHERE id = ?1 AND revision = ?3",
            rusqlite::params![id, to_rev, from_rev],
        )
        .map_err(|e| format!("cannot carry measurement: {e}"))?;
    if copied != 1 {
        return Err(format!("benchmark revision without measurement: '{id}'"));
    }
    tx.execute(
        "INSERT INTO metric_values (id, revision, name, value, unit)
         SELECT ?1, ?2, name, value, unit FROM metric_values
         WHERE id = ?1 AND revision = ?3",
        rusqlite::params![id, to_rev, from_rev],
    )
    .map_err(|e| format!("cannot carry metrics: {e}"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_inner(
    conn: &mut Connection,
    id: &str,
    expected_revision: u64,
    input: &UpdateRecord,
    measurement: Option<&crate::knowledge::BenchmarkData>,
    evidence: &[crate::knowledge::EvidenceRef],
    inject_fault: bool,
) -> Result<(u64, u64), String> {
    crate::knowledge::validate_update(input)?;
    revalidate_open_paths(conn)?;
    // Same pre-transaction capture as creation: bad bindings reject the
    // update with nothing committed.
    let captured = capture_evidence(conn, evidence)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    // A Full-stage transition or an explicit confirmation migrates the store
    // as the very first step of its own write transaction; every later
    // rejection rolls the migration back with it, so a failed write leaves a
    // v1 store a v1 store.
    if input.status == "superseded" || input.status == "retracted" || input.confirm {
        ensure_lifecycle_schema(&tx)?;
    }
    let current: Option<i64> = tx
        .query_row(
            "SELECT current_revision FROM heads WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("cannot read record head: {e}"))?;
    let current = match current {
        Some(c) => c,
        None => return Err(format!("unknown record id: '{id}'")),
    };
    // An unrepresentable expectation can never match; a corrupt negative head
    // fails here too, before any mutation is attempted.
    let expected_matches = i64::try_from(expected_revision)
        .map(|expected| expected == current)
        .unwrap_or(false);
    if !expected_matches {
        return Err(format!(
            "revision conflict for '{id}': expected {expected_revision} but head is {current}"
        ));
    }
    let kind: String = tx
        .query_row(
            "SELECT kind FROM records WHERE id = ?1 AND revision = ?2",
            rusqlite::params![id, current],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot read record kind: {e}"))?;
    // The immutable kind decides measurement eligibility inside the
    // transaction, before any mutation: only benchmark revisions may carry
    // measurement, whether supplied or carried forward.
    if kind != "benchmark" && measurement.is_some() {
        return Err("measurement is only valid for benchmark records".to_string());
    }
    // Lifecycle transitions are validated against current state inside the
    // transaction, before any mutation. Rejection classes surface as
    // "invalid lifecycle transition" (exit 2); unknown targets stay in the
    // unknown-endpoint class (exit 3).
    let mut supersede_target: Option<(String, i64)> = None;
    if input.status == "superseded" || input.status == "retracted" {
        let head_status: String = tx
            .query_row(
                "SELECT status FROM records WHERE id = ?1 AND revision = ?2",
                rusqlite::params![id, current],
                |row| row.get(0),
            )
            .map_err(|e| format!("cannot read record status: {e}"))?;
        if input.status == "retracted" {
            // Withdrawal is a one-way event: repeating it is rejected.
            if head_status == "retracted" {
                return Err(format!(
                    "invalid lifecycle transition: record is already retracted: '{id}'"
                ));
            }
        } else {
            // A withdrawn claim must not be silently revived into a
            // replacement pointer.
            if head_status == "retracted" {
                return Err(format!(
                    "invalid lifecycle transition: record is retracted: '{id}'"
                ));
            }
            let target = input
                .superseded_by
                .as_ref()
                .ok_or_else(|| "status 'superseded' requires `superseded_by`".to_string())?;
            if target.id == id {
                return Err(
                    "invalid lifecycle transition: a record cannot supersede itself".to_string(),
                );
            }
            let target_rev = i64::try_from(target.revision)
                .map_err(|_| "supersede target revision out of range".to_string())?;
            if !endpoint_exists(&tx, &target.id, target_rev)? {
                return Err(format!(
                    "unknown supersede target: '{}' rev {}",
                    target.id, target.revision
                ));
            }
            // The target record must not be retracted either.
            let target_head_status: String = tx
                .query_row(
                    "SELECT r.status FROM heads h JOIN records r
                     ON r.id = h.id AND r.revision = h.current_revision
                     WHERE h.id = ?1",
                    [&target.id],
                    |row| row.get(0),
                )
                .map_err(|e| format!("cannot read supersede target status: {e}"))?;
            if target_head_status == "retracted" {
                return Err(format!(
                    "invalid lifecycle transition: supersede target is retracted: '{}'",
                    target.id
                ));
            }
            // Repeating the current head's target is rejected; a different
            // valid target is a new transition.
            let head_target: Option<(String, i64)> = tx
                .query_row(
                    "SELECT target_id, target_revision FROM record_lifecycle
                     WHERE id = ?1 AND revision = ?2 AND event = 'supersede'",
                    rusqlite::params![id, current],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| format!("cannot read supersede history: {e}"))?;
            if let Some((target_id, target_revision)) = head_target {
                if target_id == target.id && target_revision == target_rev {
                    return Err(format!(
                        "invalid lifecycle transition: record already supersedes '{}' rev {}",
                        target.id, target.revision
                    ));
                }
            }
            // The lineage stays acyclic: walking supersedes edges from the
            // superseded record must never reach the superseding record.
            // Retracted relations are walked too — the lineage is history,
            // and unlinking a relation must not authorize a cycle.
            let cycle: i64 = tx
                .query_row(
                    "WITH RECURSIVE reach(id) AS (
                       SELECT dst_id FROM relations WHERE kind = 'supersedes' AND src_id = ?1
                       UNION
                       SELECT r.dst_id FROM relations r JOIN reach ON r.src_id = reach.id
                       WHERE r.kind = 'supersedes'
                     )
                     SELECT count(*) FROM reach WHERE id = ?2",
                    rusqlite::params![id, target.id],
                    |row| row.get(0),
                )
                .map_err(|e| format!("cannot check supersedes lineage: {e}"))?;
            if cycle != 0 {
                return Err(format!(
                    "invalid lifecycle transition: supersedes cycle involving '{}'",
                    target.id
                ));
            }
            supersede_target = Some((target.id.clone(), target_rev));
        }
    }
    // Explicit confirmation is idempotent for the head revision: an identical
    // payload — same title, body, status and author, no new evidence or
    // measurement — re-confirming an already-confirmed head succeeds without
    // writing. Everything else writes a new confirming revision; there is no
    // in-place mutation and no implicit confirmation.
    if input.confirm {
        let head: (String, String, String, String) = tx
            .query_row(
                "SELECT title, body, status, author FROM records WHERE id = ?1 AND revision = ?2",
                rusqlite::params![id, current],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| format!("cannot read record: {e}"))?;
        let already: i64 = tx
            .query_row(
                "SELECT count(*) FROM record_lifecycle
                 WHERE id = ?1 AND revision = ?2 AND event = 'confirm'",
                rusqlite::params![id, current],
                |row| row.get(0),
            )
            .map_err(|e| format!("cannot read confirmation state: {e}"))?;
        let unchanged = head
            == (
                input.title.clone(),
                input.body.clone(),
                input.status.clone(),
                input.author.clone(),
            );
        if already != 0 && unchanged && evidence.is_empty() && measurement.is_none() {
            let head_revision = u64::try_from(current)
                .map_err(|_| "corrupt knowledge store: stored revision out of range".to_string())?;
            let generation = read_generation(&tx)?;
            tx.commit()
                .map_err(|e| format!("cannot commit knowledge write: {e}"))?;
            return Ok((head_revision, generation));
        }
    }
    let next_i64 = current
        .checked_add(1)
        .ok_or_else(|| "knowledge revision counter overflow".to_string())?;
    let next =
        u64::try_from(next_i64).map_err(|_| "knowledge revision counter overflow".to_string())?;
    tx.execute(
        "INSERT INTO records (id, revision, kind, title, body, status, author, created_unix)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            id,
            next_i64,
            kind,
            input.title,
            input.body,
            input.status,
            input.author,
            now_unix()
        ],
    )
    .map_err(|e| format!("cannot insert record revision: {e}"))?;
    // Evidence rows precede the fault hook so the injector proves they roll
    // back with everything else.
    write_evidence(&tx, id, next_i64, &captured)?;
    if inject_fault {
        // Test hook: drop the transaction here to prove partial writes roll
        // back fully (no head, FTS, evidence, measurement or generation survives).
        return Err("injected write failure".to_string());
    }
    tx.execute(
        "UPDATE heads SET current_revision = ?1 WHERE id = ?2",
        rusqlite::params![next_i64, id],
    )
    .map_err(|e| format!("cannot advance record head: {e}"))?;
    match measurement {
        Some(data) => write_measurement(&tx, id, next_i64, data)?,
        None if kind == "benchmark" => carry_measurement(&tx, id, current, next_i64)?,
        None => {}
    }
    // Lifecycle bookkeeping joins the same transaction: the immutable event,
    // and for a supersede the implicit lineage relation with its
    // deterministic identity. Any failure rolls all of it back.
    match input.status.as_str() {
        "superseded" => {
            let (target_id, target_rev) =
                supersede_target.ok_or_else(|| "supersede target was not validated".to_string())?;
            let reason = input
                .reason
                .as_deref()
                .ok_or_else(|| "status 'superseded' requires a `reason`".to_string())?;
            tx.execute(
                "INSERT INTO record_lifecycle
                 (id, revision, event, target_id, target_revision, reason, author, created_unix)
                 VALUES (?1, ?2, 'supersede', ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    id,
                    next_i64,
                    target_id,
                    target_rev,
                    reason,
                    input.author,
                    now_unix()
                ],
            )
            .map_err(|e| format!("cannot store lifecycle event: {e}"))?;
            // Store-derived identity: deterministic, complete, and referenceable
            // for retraction through `validate_relation_reference` even when it
            // exceeds the caller cap at maximum-length record ids. Creation
            // (link) stays capped; only this exact derived shape is exempt.
            let relation_id = format!("supersedes:{target_id}:{target_rev}:{id}:{next_i64}");
            tx.execute(
                "INSERT INTO relations
                 (relation_id, src_id, src_revision, dst_id, dst_revision,
                  kind, author, rationale, created_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'supersedes', ?6, ?7, ?8)",
                rusqlite::params![
                    relation_id,
                    target_id,
                    target_rev,
                    id,
                    next_i64,
                    input.author,
                    reason,
                    now_unix()
                ],
            )
            .map_err(|e| format!("cannot store supersedes relation: {e}"))?;
        }
        "retracted" => {
            let reason = input
                .reason
                .as_deref()
                .ok_or_else(|| "status 'retracted' requires a `reason`".to_string())?;
            tx.execute(
                "INSERT INTO record_lifecycle
                 (id, revision, event, reason, author, created_unix)
                 VALUES (?1, ?2, 'retract', ?3, ?4, ?5)",
                rusqlite::params![id, next_i64, reason, input.author, now_unix()],
            )
            .map_err(|e| format!("cannot store lifecycle event: {e}"))?;
        }
        _ => {}
    }
    // A confirmation event is written alongside the revision it confirms:
    // the revision carries the state, the event makes it auditable and
    // immutable. Confirmation never implies evidence or verification.
    if input.confirm {
        tx.execute(
            "INSERT INTO record_lifecycle (id, revision, event, author, created_unix)
             VALUES (?1, ?2, 'confirm', ?3, ?4)",
            rusqlite::params![id, next_i64, input.author, now_unix()],
        )
        .map_err(|e| format!("cannot store confirmation event: {e}"))?;
    }
    let generation = bump_generation(&tx)?;
    tx.commit()
        .map_err(|e| format!("cannot commit record update: {e}"))?;
    Ok((next, generation))
}

/// Measurement attached to one revision, if any. Metrics order by insertion.
fn read_measurement(
    conn: &Connection,
    id: &str,
    revision: i64,
) -> Result<Option<crate::knowledge::BenchmarkData>, String> {
    let row: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT method, environment, sample_count FROM measurements
             WHERE id = ?1 AND revision = ?2",
            rusqlite::params![id, revision],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| format!("cannot read measurement: {e}"))?;
    let Some((method, environment, sample)) = row else {
        return Ok(None);
    };
    let sample_count = u64::try_from(sample)
        .map_err(|_| "corrupt knowledge store: sample count out of range".to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT name, value, unit FROM metric_values
             WHERE id = ?1 AND revision = ?2 ORDER BY rowid",
        )
        .map_err(|e| format!("cannot read measurement: {e}"))?;
    let metrics = stmt
        .query_map(rusqlite::params![id, revision], |r| {
            Ok(crate::knowledge::MetricData {
                name: r.get(0)?,
                value: r.get(1)?,
                unit: r.get(2)?,
            })
        })
        .map_err(|e| format!("cannot read measurement: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("cannot read measurement: {e}"))?;
    Ok(Some(crate::knowledge::BenchmarkData {
        method,
        environment,
        sample_count,
        metrics,
    }))
}

/// Read one revision (`None` = current head). Never creates anything.
pub fn show_record(
    conn: &Connection,
    id: &str,
    revision: Option<u64>,
) -> Result<RecordVersion, String> {
    let rev = match revision {
        Some(r) => r,
        None => {
            let current: i64 = conn
                .query_row(
                    "SELECT current_revision FROM heads WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .map_err(|_| format!("unknown record id: '{id}'"))?;
            u64::try_from(current)
                .map_err(|_| "corrupt knowledge store: stored revision out of range".to_string())?
        }
    };
    let rev_i64 = i64::try_from(rev).map_err(|_| format!("record revision out of range: {rev}"))?;
    let mut record = conn
        .query_row(
            &format!("SELECT {RECORD_COLS} FROM records WHERE id = ?1 AND revision = ?2"),
            rusqlite::params![id, rev_i64],
            record_from_row,
        )
        .map_err(|e| match e {
            rusqlite::Error::FromSqlConversionFailure(..) => {
                "corrupt knowledge store: stored revision out of range".to_string()
            }
            _ => format!("unknown record revision: '{id}' rev {rev}"),
        })?;
    record.measurement = read_measurement(conn, id, rev_i64)?;
    record.evidence = read_evidence(conn, id, rev_i64)?;
    load_record_lifecycle(conn, lifecycle_present(conn)?, &mut record)?;
    Ok(record)
}

/// Read one revision and the store generation from one SQLite read
/// transaction: both describe the same committed snapshot even while another
/// connection commits. `probe` is a test-only seam between the two reads;
/// production callers use [`show_snapshot`], which passes none.
fn show_snapshot_inner(
    conn: &Connection,
    id: &str,
    revision: Option<u64>,
    probe: Option<&dyn Fn()>,
) -> Result<(RecordVersion, u64), String> {
    conn.execute_batch("BEGIN")
        .map_err(|e| format!("cannot begin knowledge read: {e}"))?;
    let pair = (|| {
        let record = show_record(conn, id, revision)?;
        if let Some(run) = probe {
            run();
        }
        let generation = read_generation(conn)?;
        Ok((record, generation))
    })();
    match pair {
        Ok(pair) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| format!("cannot commit knowledge read: {e}"))?;
            Ok(pair)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Read one revision (`None` = current head) and the current generation from
/// one read snapshot. Never creates or mutates anything.
pub fn show_snapshot(
    conn: &Connection,
    id: &str,
    revision: Option<u64>,
) -> Result<(RecordVersion, u64), String> {
    show_snapshot_inner(conn, id, revision, None)
}

/// Test-only probe invoked between the record read and the generation read,
/// so a test can commit from another connection exactly on that boundary.
#[cfg(test)]
pub(crate) fn show_snapshot_with_probe(
    conn: &Connection,
    id: &str,
    revision: Option<u64>,
    probe: Option<&dyn Fn()>,
) -> Result<(RecordVersion, u64), String> {
    show_snapshot_inner(conn, id, revision, probe)
}

/// Whether this store carries the v2 lifecycle tables. Reads check the table
/// itself, not a version stamp, so a v1 store (or a store whose tables are
/// missing) yields Foundation defaults instead of a schema error.
fn lifecycle_present(conn: &Connection) -> Result<bool, String> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'record_lifecycle'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot inspect knowledge schema: {e}"))?;
    Ok(n != 0)
}

/// One `record_lifecycle` row: (event, target_id, target_revision, reason,
/// author).
type LifecycleRow = (String, Option<String>, Option<i64>, Option<String>, String);

/// Overlay one revision's lifecycle state from the append-only event table.
/// Revisions without an event keep Foundation defaults: no target, no
/// retraction fields, and the candidate confirmation state (on a v2 store).
/// The event is keyed by (id, revision) and is written exactly once per
/// revision, so this never needs to merge events.
fn load_record_lifecycle(
    conn: &Connection,
    has_lifecycle: bool,
    record: &mut RecordVersion,
) -> Result<(), String> {
    if !has_lifecycle {
        return Ok(());
    }
    let revision = i64::try_from(record.revision)
        .map_err(|_| "corrupt knowledge store: stored revision out of range".to_string())?;
    let event: Option<LifecycleRow> = conn
        .query_row(
            "SELECT event, target_id, target_revision, reason, author
             FROM record_lifecycle WHERE id = ?1 AND revision = ?2",
            rusqlite::params![record.id, revision],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|e| format!("cannot read record lifecycle: {e}"))?;
    // On a v2 store every revision carries a state: candidate by default,
    // confirmed only when this exact revision has its own confirm event.
    record.confirm_state = Some("candidate".to_string());
    if let Some((event, target_id, target_revision, reason, author)) = event {
        match event.as_str() {
            "supersede" => {
                if let (Some(target_id), Some(target_revision)) = (target_id, target_revision) {
                    let revision = u64::try_from(target_revision).map_err(|_| {
                        "corrupt knowledge store: supersede target out of range".to_string()
                    })?;
                    record.superseded_by = Some(crate::knowledge::SupersededRef {
                        id: target_id,
                        revision,
                    });
                }
            }
            "retract" => {
                record.retracted_reason = reason;
                record.retracted_author = Some(author);
            }
            "confirm" => {
                record.confirm_state = Some("confirmed".to_string());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Deterministic history by revision with a generation guard: a supplied
/// generation that no longer matches fails instead of serving shifted pages.
///
/// Generation, total and rows are read inside one SQLite read transaction, so
/// a concurrent commit landing mid-read can neither mix generations with rows
/// nor shift pages; the whole page reflects a single snapshot.
pub fn history(
    conn: &Connection,
    id: &str,
    limit: usize,
    offset: usize,
    generation: Option<u64>,
) -> Result<HistoryPage, String> {
    history_inner(conn, id, limit, offset, generation, None)
}

/// Test-only probe invoked after generation validation but before row reads,
/// so a test can commit from another connection exactly on that boundary.
#[cfg(test)]
pub(crate) fn history_with_probe(
    conn: &Connection,
    id: &str,
    limit: usize,
    offset: usize,
    generation: Option<u64>,
    probe: Option<&dyn Fn()>,
) -> Result<HistoryPage, String> {
    history_inner(conn, id, limit, offset, generation, probe)
}

fn history_inner(
    conn: &Connection,
    id: &str,
    limit: usize,
    offset: usize,
    generation: Option<u64>,
    probe: Option<&dyn Fn()>,
) -> Result<HistoryPage, String> {
    // Same self-loop guard as search/related: history carries no next
    // offset, but a zero limit is still a validation error, not an empty
    // page that callers might re-request forever.
    if limit == 0 {
        return Err("history limit must be at least 1".to_string());
    }
    let limit_i64 =
        i64::try_from(limit).map_err(|_| format!("history limit out of range: {limit}"))?;
    let offset_i64 =
        i64::try_from(offset).map_err(|_| format!("history offset out of range: {offset}"))?;
    conn.execute_batch("BEGIN")
        .map_err(|e| format!("cannot begin knowledge read: {e}"))?;
    let page = (|| {
        let current = read_generation(conn)?;
        if let Some(expected) = generation {
            if expected != current {
                return Err(format!(
                    "knowledge generation conflict: expected {expected} but store is at {current}"
                ));
            }
        }
        if let Some(run) = probe {
            run();
        }
        let total: i64 = conn
            .query_row("SELECT count(*) FROM records WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .map_err(|e| format!("cannot read record history: {e}"))?;
        if total == 0 {
            return Err(format!("unknown record id: '{id}'"));
        }
        let total_usize = usize::try_from(total)
            .map_err(|_| "corrupt knowledge store: history total out of range".to_string())?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {RECORD_COLS} FROM records WHERE id = ?1
                 ORDER BY revision ASC LIMIT ?2 OFFSET ?3"
            ))
            .map_err(|e| format!("cannot read record history: {e}"))?;
        let mut revisions = stmt
            .query_map(
                rusqlite::params![id, limit_i64, offset_i64],
                record_from_row,
            )
            .map_err(|e| map_row_error(e, "cannot read record history"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| map_row_error(e, "cannot read record history"))?;
        let has_lifecycle = lifecycle_present(conn)?;
        for record in &mut revisions {
            let rev_i64 = i64::try_from(record.revision)
                .map_err(|_| "corrupt knowledge store: stored revision out of range".to_string())?;
            record.measurement = read_measurement(conn, id, rev_i64)?;
            record.evidence = read_evidence(conn, id, rev_i64)?;
            load_record_lifecycle(conn, has_lifecycle, record)?;
        }
        let end = offset.saturating_add(revisions.len());
        Ok(HistoryPage {
            truncated: end < total_usize,
            total: total_usize,
            generation: current,
            revisions,
        })
    })();
    match page {
        Ok(built) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| format!("cannot commit knowledge read: {e}"))?;
            Ok(built)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// A stored revision outside `u64` range surfaces as store corruption, never
/// as a panic and never disguised as a missing row.
fn map_row_error(e: rusqlite::Error, otherwise: &str) -> String {
    match e {
        rusqlite::Error::FromSqlConversionFailure(..) => {
            "corrupt knowledge store: stored revision out of range".to_string()
        }
        _ => otherwise.to_string(),
    }
}

/// One page of lexical search hits with its generation guard.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<RecordVersion>,
    pub total: usize,
    pub truncated: bool,
    pub next_offset: Option<usize>,
    pub generation: u64,
}

/// Quote user tokens as FTS5 phrases: internal quotes double, each token
/// stays one literal, and phrases join with spaces (implicit AND). Quoting
/// neutralizes operators, column filters and SQL-looking text, but FTS5 still
/// treats `*` as a prefix operator inside phrases and folds token separators —
/// so the expression is only a coarse pre-filter. Every candidate is
/// re-checked in Rust for literal containment of each raw token before it
/// counts as a hit.
fn fts_literal_match(query: &str) -> Result<(Option<String>, Vec<String>), String> {
    if query.len() > crate::knowledge::MAX_QUERY_TEXT_BYTES {
        return Err(format!(
            "search query is {} bytes, at most {} allowed",
            query.len(),
            crate::knowledge::MAX_QUERY_TEXT_BYTES
        ));
    }
    let tokens: Vec<String> = query.split_whitespace().map(str::to_string).collect();
    if tokens.is_empty() {
        return Ok((None, tokens));
    }
    let mut expr = String::new();
    for token in &tokens {
        if !expr.is_empty() {
            expr.push(' ');
        }
        expr.push('"');
        expr.push_str(&token.replace('"', "\"\""));
        expr.push('"');
    }
    Ok((Some(expr), tokens))
}

/// Every raw token must appear literally (case-insensitively, mirroring FTS
/// recall) in the title or body. This is what makes `*`, quotes and
/// separators inert text rather than query syntax.
fn contains_all_terms(record: &RecordVersion, tokens: &[String]) -> bool {
    let haystack = format!("{}\n{}", record.title, record.body).to_lowercase();
    tokens
        .iter()
        .all(|token| haystack.contains(&token.to_lowercase()))
}

/// Deterministic lexical search over current head revisions only: title and
/// body match literal user terms, results order by record ID (binary
/// collation), and filters apply before counting. Generation, total and rows
/// are read inside one SQLite read transaction; a supplied generation that
/// no longer matches fails instead of serving shifted pages.
#[allow(clippy::too_many_arguments)]
pub fn search(
    conn: &Connection,
    query: &str,
    path: Option<&str>,
    kind: Option<&str>,
    include_archived: bool,
    include_superseded: bool,
    include_retracted: bool,
    limit: usize,
    offset: usize,
    generation: Option<u64>,
) -> Result<SearchPage, String> {
    if let Some(p) = path {
        crate::knowledge::validate_search_path(p)?;
    }
    if let Some(k) = kind {
        crate::knowledge::validate_kind(k)?;
    }
    if limit > crate::knowledge::SEARCH_LIMIT_MAX {
        return Err(format!(
            "search limit {limit} exceeds maximum {}",
            crate::knowledge::SEARCH_LIMIT_MAX
        ));
    }
    // A zero limit can never advance next_offset: the page would point at
    // itself forever. Reject it instead of serving a self-loop.
    if limit == 0 {
        return Err("search limit must be at least 1".to_string());
    }
    // Usize page bounds always fit the SQLite range on 64-bit targets, but
    // the conversion stays explicit: absurd values fail here, never in SQL.
    i64::try_from(limit).map_err(|_| format!("search limit out of range: {limit}"))?;
    i64::try_from(offset).map_err(|_| format!("search offset out of range: {offset}"))?;
    let (matcher, tokens) = fts_literal_match(query)?;
    conn.execute_batch("BEGIN")
        .map_err(|e| format!("cannot begin knowledge read: {e}"))?;
    let page = (|| {
        let current = read_generation(conn)?;
        if let Some(expected) = generation {
            if expected != current {
                return Err(format!(
                    "knowledge generation conflict: expected {expected} but store is at {current}"
                ));
            }
        }
        // Text uses FTS as the driver; a path-only request streams current
        // heads directly. Every value below is bound, including the
        // pre-quoted MATCH expression, kind, and evidence path.
        let (mut sql, mut args) = match matcher {
            Some(match_expr) => (
                String::from(
                    "SELECT r.id, r.revision, r.kind, r.title, r.body, r.status, r.author, r.created_unix
                     FROM knowledge_fts
                     JOIN heads h ON h.id = knowledge_fts.id
                     JOIN records r ON r.id = knowledge_fts.id
                       AND r.revision = h.current_revision
                     WHERE knowledge_fts MATCH ?1",
                ),
                vec![rusqlite::types::Value::Text(match_expr)],
            ),
            None if path.is_some() => (
                String::from(
                    "SELECT r.id, r.revision, r.kind, r.title, r.body, r.status, r.author, r.created_unix
                     FROM records r
                     JOIN heads h ON h.id = r.id AND r.revision = h.current_revision
                     WHERE 1",
                ),
                Vec::new(),
            ),
            None => {
                return Ok(SearchPage {
                    hits: Vec::new(),
                    total: 0,
                    truncated: false,
                    next_offset: None,
                    generation: current,
                });
            }
        };
        if let Some(k) = kind {
            sql.push_str(&format!(" AND r.kind = ?{}", args.len() + 1));
            args.push(rusqlite::types::Value::Text(k.to_string()));
        }
        // Visibility is explicit: active always, each historical status only
        // on request. The status literals below are fixed constants, never
        // interpolated input, and the clause applies before counting.
        let mut visible = String::from("'active'");
        if include_archived {
            visible.push_str(", 'archived'");
        }
        if include_superseded {
            visible.push_str(", 'superseded'");
        }
        if include_retracted {
            visible.push_str(", 'retracted'");
        }
        sql.push_str(&format!(" AND r.status IN ({visible})"));
        if let Some(p) = path {
            // The correlated probe is restricted to this record revision and
            // uses the evidence primary-key prefix. EXISTS yields one row no
            // matter how many captured refs name the same path.
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM evidence e WHERE e.id = r.id
                   AND e.revision = r.revision AND e.path = ?{})",
                args.len() + 1
            ));
            args.push(rusqlite::types::Value::Text(p.to_string()));
        }
        // FTS is a coarse pre-filter: stream candidates in ID order, keep
        // the literal-term check in Rust, count every verified row, and
        // retain only the requested page — memory stays bounded by the page
        // (at most 1000 rows), never by the corpus. Measurement and evidence
        // load afterwards, only for returned rows.
        sql.push_str(" ORDER BY r.id COLLATE BINARY ASC");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("cannot search knowledge: {e}"))?;
        let mut streamed = stmt
            .query(rusqlite::params_from_iter(args.iter()))
            .map_err(|e| map_row_error(e, "cannot search knowledge"))?;
        let mut total_usize: usize = 0;
        let mut hits: Vec<RecordVersion> = Vec::new();
        while let Some(row) = streamed
            .next()
            .map_err(|e| map_row_error(e, "cannot search knowledge"))?
        {
            let record =
                record_from_row(row).map_err(|e| map_row_error(e, "cannot search knowledge"))?;
            if !contains_all_terms(&record, &tokens) {
                continue;
            }
            if total_usize >= offset && hits.len() < limit {
                hits.push(record);
            }
            total_usize += 1;
        }
        drop(streamed);
        let has_lifecycle = lifecycle_present(conn)?;
        for record in &mut hits {
            let rev_i64 = i64::try_from(record.revision)
                .map_err(|_| "corrupt knowledge store: stored revision out of range".to_string())?;
            record.measurement = read_measurement(conn, &record.id, rev_i64)?;
            record.evidence = read_evidence(conn, &record.id, rev_i64)?;
            load_record_lifecycle(conn, has_lifecycle, record)?;
        }
        let end = offset.saturating_add(hits.len());
        Ok(SearchPage {
            truncated: end < total_usize,
            next_offset: if end < total_usize { Some(end) } else { None },
            total: total_usize,
            generation: current,
            hits,
        })
    })();
    match page {
        Ok(built) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| format!("cannot commit knowledge read: {e}"))?;
            Ok(built)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// One-hop direction for [`related`]. Outbound follows authored source
/// links; inbound follows links pointing at the record. No transitive walk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RelationDirection {
    Outbound,
    Inbound,
}

/// One authored relationship with its retraction state. Endpoints stay pinned
/// to the exact revisions named at link time; updating a record never moves
/// them. Retraction details are present only when `retracted` is true and the
/// caller asked for retracted rows.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationView {
    pub relation_id: String,
    pub src_id: String,
    pub src_revision: u64,
    pub dst_id: String,
    pub dst_revision: u64,
    pub kind: String,
    pub author: String,
    pub rationale: String,
    pub created_unix: i64,
    pub retracted: bool,
    pub retraction_author: Option<String>,
    pub retraction_reason: Option<String>,
    /// Confirmation state of this exact relation: `Some("candidate")`/
    /// `Some("confirmed")` on v2 stores, `None` on v1 stores (the JSON output
    /// then omits the field).
    pub confirm_state: Option<String>,
}

/// One page of one-hop relations with its generation guard.
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedPage {
    pub relations: Vec<RelationView>,
    pub total: usize,
    pub truncated: bool,
    pub next_offset: Option<usize>,
    pub generation: u64,
}

fn check_actor(text: &str, field: &str) -> Result<(), String> {
    if text.is_empty() || text.len() > crate::knowledge::MAX_AUTHOR_BYTES {
        return Err(format!(
            "field `{field}` must be 1..={} bytes",
            crate::knowledge::MAX_AUTHOR_BYTES
        ));
    }
    Ok(())
}

fn check_rationale(text: &str, field: &str) -> Result<(), String> {
    if text.is_empty() || text.len() > crate::knowledge::MAX_RATIONALE_BYTES {
        return Err(format!(
            "field `{field}` must be 1..={} bytes",
            crate::knowledge::MAX_RATIONALE_BYTES
        ));
    }
    Ok(())
}

fn endpoint_exists(tx: &Transaction, id: &str, revision: i64) -> Result<bool, String> {
    tx.query_row(
        "SELECT count(*) FROM records WHERE id = ?1 AND revision = ?2",
        rusqlite::params![id, revision],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count == 1)
    .map_err(|e| format!("cannot read relation endpoint: {e}"))
}

/// Author one directed relationship between two exact record revisions in
/// this database. Null, foreign, guessed or name-matched endpoints cannot be
/// expressed: both revisions must exist before commit, and an identical
/// active endpoint/kind tuple is rejected. `confirm` explicitly confirms the
/// authored row: on a fresh link it records the confirmation event with the
/// write, and re-linking the byte-identical live row with `confirm` promotes
/// it (idempotently — a repeat is a no-op, not an error). The authored shape
/// is never rewritten, and a retracted relation is never revived. One write
/// transaction covers the insert/promotion and the generation bump.
#[allow(clippy::too_many_arguments)]
pub fn link(
    conn: &mut Connection,
    relation_id: &str,
    src_id: &str,
    src_revision: u64,
    dst_id: &str,
    dst_revision: u64,
    kind: &str,
    author: &str,
    rationale: &str,
    confirm: bool,
) -> Result<u64, String> {
    crate::knowledge::validate_relation_id(relation_id)?;
    crate::knowledge::validate_relation_kind(kind)?;
    check_actor(author, "author")?;
    check_rationale(rationale, "rationale")?;
    let src_i64 = i64::try_from(src_revision)
        .map_err(|_| format!("relation revision out of range: {src_revision}"))?;
    let dst_i64 = i64::try_from(dst_revision)
        .map_err(|_| format!("relation revision out of range: {dst_revision}"))?;
    revalidate_open_paths(conn)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    // A confirmation is Full-stage data: the store migrates as the first
    // step of the same transaction.
    if confirm {
        ensure_lifecycle_schema(&tx)?;
    }
    // A taken id is either the byte-identical live row being confirmed, or a
    // conflict. Reading the authored shape here is what makes promotion safe:
    // only an exact match of every stored field may ever be confirmed.
    let existing: Option<(String, i64, String, i64, String, String, String)> = tx
        .query_row(
            "SELECT src_id, src_revision, dst_id, dst_revision, kind, author, rationale
             FROM relations WHERE relation_id = ?1",
            [relation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(|e| format!("cannot check relation id: {e}"))?;
    if let Some((x_src, x_src_rev, x_dst, x_dst_rev, x_kind, x_author, x_rationale)) = existing {
        let identical = x_src == src_id
            && x_src_rev == src_i64
            && x_dst == dst_id
            && x_dst_rev == dst_i64
            && x_kind == kind
            && x_author == author
            && x_rationale == rationale;
        let retracted: i64 = tx
            .query_row(
                "SELECT count(*) FROM retractions WHERE relation_id = ?1",
                [relation_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("cannot read relation retraction: {e}"))?;
        if !confirm || !identical || retracted != 0 {
            return Err(format!("duplicate relation id: '{relation_id}'"));
        }
        let already: i64 = tx
            .query_row(
                "SELECT count(*) FROM relation_lifecycle
                 WHERE relation_id = ?1 AND event = 'confirm'",
                [relation_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("cannot read relation confirmation: {e}"))?;
        if already != 0 {
            // Repeating the confirmation succeeds without writing.
            let generation = read_generation(&tx)?;
            tx.commit()
                .map_err(|e| format!("cannot commit relation link: {e}"))?;
            return Ok(generation);
        }
        tx.execute(
            "INSERT INTO relation_lifecycle (relation_id, event, author, created_unix)
             VALUES (?1, 'confirm', ?2, ?3)",
            rusqlite::params![relation_id, author, now_unix()],
        )
        .map_err(|e| format!("cannot store relation confirmation: {e}"))?;
        let generation = bump_generation(&tx)?;
        tx.commit()
            .map_err(|e| format!("cannot commit relation link: {e}"))?;
        return Ok(generation);
    }
    if !endpoint_exists(&tx, src_id, src_i64)? {
        return Err(format!(
            "unknown relation endpoint: '{src_id}' rev {src_revision}"
        ));
    }
    if !endpoint_exists(&tx, dst_id, dst_i64)? {
        return Err(format!(
            "unknown relation endpoint: '{dst_id}' rev {dst_revision}"
        ));
    }
    // Retracted rows are history, not active assertions: only a live tuple
    // with the same endpoints and kind blocks the link.
    let live_dup: i64 = tx
        .query_row(
            "SELECT count(*) FROM relations r
             LEFT JOIN retractions t ON t.relation_id = r.relation_id
             WHERE r.src_id = ?1 AND r.src_revision = ?2
               AND r.dst_id = ?3 AND r.dst_revision = ?4
               AND r.kind = ?5 AND t.relation_id IS NULL",
            rusqlite::params![src_id, src_i64, dst_id, dst_i64, kind],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot check duplicate relation: {e}"))?;
    if live_dup != 0 {
        return Err(format!(
            "duplicate relation: '{src_id}' rev {src_revision} '{kind}' '{dst_id}' rev {dst_revision}"
        ));
    }
    tx.execute(
        "INSERT INTO relations (relation_id, src_id, src_revision, dst_id, dst_revision,
          kind, author, rationale, created_unix)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            relation_id,
            src_id,
            src_i64,
            dst_id,
            dst_i64,
            kind,
            author,
            rationale,
            now_unix()
        ],
    )
    .map_err(|e| format!("cannot store relation: {e}"))?;
    if confirm {
        tx.execute(
            "INSERT INTO relation_lifecycle (relation_id, event, author, created_unix)
             VALUES (?1, 'confirm', ?2, ?3)",
            rusqlite::params![relation_id, author, now_unix()],
        )
        .map_err(|e| format!("cannot store relation confirmation: {e}"))?;
    }
    let generation = bump_generation(&tx)?;
    tx.commit()
        .map_err(|e| format!("cannot commit relation link: {e}"))?;
    Ok(generation)
}

/// Retract a relationship: an append-only event with author and reason, never
/// a deletion. Normal queries omit the relation afterwards; audit reads with
/// `include_retracted` retain it. Repeating the retraction fails. The id may
/// be a caller-supplied id or a full-length store-derived `supersedes`
/// identity, accepted by `validate_relation_reference` only in its exact
/// derived shape.
pub fn unlink(
    conn: &mut Connection,
    relation_id: &str,
    author: &str,
    reason: &str,
) -> Result<u64, String> {
    crate::knowledge::validate_relation_reference(relation_id)?;
    check_actor(author, "author")?;
    check_rationale(reason, "reason")?;
    revalidate_open_paths(conn)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    let known: i64 = tx
        .query_row(
            "SELECT count(*) FROM relations WHERE relation_id = ?1",
            [relation_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot read relation: {e}"))?;
    if known != 1 {
        return Err(format!("unknown relation id: '{relation_id}'"));
    }
    let already: i64 = tx
        .query_row(
            "SELECT count(*) FROM retractions WHERE relation_id = ?1",
            [relation_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot read relation retraction: {e}"))?;
    if already != 0 {
        return Err(format!("relation already retracted: '{relation_id}'"));
    }
    tx.execute(
        "INSERT INTO retractions (relation_id, author, reason, created_unix)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![relation_id, author, reason, now_unix()],
    )
    .map_err(|e| format!("cannot store relation retraction: {e}"))?;
    let generation = bump_generation(&tx)?;
    tx.commit()
        .map_err(|e| format!("cannot commit relation retraction: {e}"))?;
    Ok(generation)
}

/// Confirmation state lives in `relation_lifecycle`, which exists only on v2
/// stores: reads join it when present and report `None` (field omitted from
/// JSON) when the store has no lifecycle tables at all.
fn relation_confirm_join(has_lifecycle: bool) -> (&'static str, &'static str) {
    if has_lifecycle {
        (
            "LEFT JOIN relation_lifecycle l ON l.relation_id = r.relation_id",
            "l.event",
        )
    } else {
        ("", "NULL")
    }
}

fn relation_from_row(row: &rusqlite::Row, has_lifecycle: bool) -> rusqlite::Result<RelationView> {
    let bad = |rev: i64| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("negative relation revision {rev}"),
            )),
        )
    };
    let src_i64: i64 = row.get(2)?;
    let dst_i64: i64 = row.get(4)?;
    Ok(RelationView {
        relation_id: row.get(0)?,
        src_id: row.get(1)?,
        src_revision: u64::try_from(src_i64).map_err(|_| bad(src_i64))?,
        dst_id: row.get(3)?,
        dst_revision: u64::try_from(dst_i64).map_err(|_| bad(dst_i64))?,
        kind: row.get(5)?,
        author: row.get(6)?,
        rationale: row.get(7)?,
        created_unix: row.get(8)?,
        retracted: row.get::<_, Option<String>>(9)?.is_some(),
        retraction_author: row.get(10)?,
        retraction_reason: row.get(11)?,
        // Column 12 is the optional lifecycle join: on v2 stores a present
        // event means confirmed, its absence candidate; on v1 the join is
        // an explicit NULL and there is no state to report.
        confirm_state: if has_lifecycle {
            Some(match row.get::<_, Option<String>>(12)? {
                Some(_) => "confirmed".to_string(),
                None => "candidate".to_string(),
            })
        } else {
            None
        },
    })
}

/// One-hop relations for one record revision (`None` = current head).
/// Current and historical revisions read separately by construction: an empty
/// current result never implies no historical relation existed — pass the
/// explicit revision to see it. Generation, total and rows are read inside
/// one SQLite read transaction.
#[allow(clippy::too_many_arguments)]
pub fn related(
    conn: &Connection,
    id: &str,
    revision: Option<u64>,
    direction: RelationDirection,
    include_retracted: bool,
    limit: usize,
    offset: usize,
    generation: Option<u64>,
) -> Result<RelatedPage, String> {
    if limit > crate::knowledge::SEARCH_LIMIT_MAX {
        return Err(format!(
            "related limit {limit} exceeds maximum {}",
            crate::knowledge::SEARCH_LIMIT_MAX
        ));
    }
    // Same self-loop guard as search: a zero limit never advances.
    if limit == 0 {
        return Err("related limit must be at least 1".to_string());
    }
    let limit_i64 =
        i64::try_from(limit).map_err(|_| format!("related limit out of range: {limit}"))?;
    let offset_i64 =
        i64::try_from(offset).map_err(|_| format!("related offset out of range: {offset}"))?;
    conn.execute_batch("BEGIN")
        .map_err(|e| format!("cannot begin knowledge read: {e}"))?;
    let page = (|| {
        let current = read_generation(conn)?;
        if let Some(expected) = generation {
            if expected != current {
                return Err(format!(
                    "knowledge generation conflict: expected {expected} but store is at {current}"
                ));
            }
        }
        // Resolve the queried revision first: heads for the default, an
        // existence check for explicit history. Either failure names the
        // record instead of returning a misleading empty page.
        let rev_i64 = match revision {
            None => conn
                .query_row(
                    "SELECT current_revision FROM heads WHERE id = ?1",
                    [id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|_| format!("unknown record id: '{id}'"))?,
            Some(rev) => {
                let wanted = i64::try_from(rev)
                    .map_err(|_| format!("record revision out of range: {rev}"))?;
                let found: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM records WHERE id = ?1 AND revision = ?2",
                        rusqlite::params![id, wanted],
                        |row| row.get(0),
                    )
                    .map_err(|e| format!("cannot read record revision: {e}"))?;
                if found != 1 {
                    return Err(format!("unknown record revision: '{id}' rev {rev}"));
                }
                wanted
            }
        };
        let endpoint = match direction {
            RelationDirection::Outbound => "r.src_id = ?1 AND r.src_revision = ?2",
            RelationDirection::Inbound => "r.dst_id = ?1 AND r.dst_revision = ?2",
        };
        let has_lifecycle = lifecycle_present(conn)?;
        let (confirm_join, confirm_col) = relation_confirm_join(has_lifecycle);
        let retracted_filter = if include_retracted {
            ""
        } else {
            " AND t.relation_id IS NULL"
        };
        let from_where = format!(
            "FROM relations r
             LEFT JOIN retractions t ON t.relation_id = r.relation_id
             {confirm_join}
             WHERE {endpoint}{retracted_filter}"
        );
        let total: i64 = conn
            .query_row(
                &format!("SELECT count(*) {from_where}"),
                rusqlite::params![id, rev_i64],
                |row| row.get(0),
            )
            .map_err(|e| format!("cannot read relations: {e}"))?;
        let total_usize = usize::try_from(total)
            .map_err(|_| "corrupt knowledge store: relation total out of range".to_string())?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT r.relation_id, r.src_id, r.src_revision, r.dst_id, r.dst_revision,
                   r.kind, r.author, r.rationale, r.created_unix,
                   t.relation_id, t.author, t.reason, {confirm_col}
                 {from_where}
                 ORDER BY r.src_id COLLATE BINARY ASC, r.src_revision ASC,
                   r.dst_id COLLATE BINARY ASC, r.dst_revision ASC,
                   r.kind COLLATE BINARY ASC, r.relation_id COLLATE BINARY ASC
                 LIMIT ?3 OFFSET ?4"
            ))
            .map_err(|e| format!("cannot read relations: {e}"))?;
        let items = stmt
            .query_map(
                rusqlite::params![id, rev_i64, limit_i64, offset_i64],
                |row| relation_from_row(row, has_lifecycle),
            )
            .map_err(|e| map_row_error(e, "cannot read relations"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| map_row_error(e, "cannot read relations"))?;
        let end = offset.saturating_add(items.len());
        Ok(RelatedPage {
            truncated: end < total_usize,
            next_offset: if end < total_usize { Some(end) } else { None },
            total: total_usize,
            generation: current,
            relations: items,
        })
    })();
    match page {
        Ok(built) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| format!("cannot commit knowledge read: {e}"))?;
            Ok(built)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Exact lookup of one relation by its opaque id, with retraction state. The
/// link response uses this instead of scanning a bounded `related` page, so a
/// relation that sorts past the page limit is still reported correctly.
pub fn relation_by_id(conn: &Connection, relation_id: &str) -> Result<RelationView, String> {
    let has_lifecycle = lifecycle_present(conn)?;
    let (confirm_join, confirm_col) = relation_confirm_join(has_lifecycle);
    conn.query_row(
        &format!(
            "SELECT r.relation_id, r.src_id, r.src_revision, r.dst_id, r.dst_revision,
               r.kind, r.author, r.rationale, r.created_unix,
               t.relation_id, t.author, t.reason, {confirm_col}
             FROM relations r
             LEFT JOIN retractions t ON t.relation_id = r.relation_id
             {confirm_join}
             WHERE r.relation_id = ?1"
        ),
        [relation_id],
        |row| relation_from_row(row, has_lifecycle),
    )
    .optional()
    .map_err(|e| map_row_error(e, "cannot read relation"))?
    .ok_or_else(|| format!("unknown relation id: '{relation_id}'"))
}

// --- G-PORT: canonical JSONL/Markdown port ---------------------------------
//
// Canonical export is deterministic from the store contents alone: lifecycle
// audit metadata rides as structured event objects mirroring the append-only
// tables, and no derived field, no clock reading, and no store identity is
// emitted. Import is a two-phase operation: a pure parse pass that never
// touches a store, then one `Immediate` transaction that re-validates
// referential integrity across the file and the store and merges under an
// explicit policy. Malformed, oversized, or unparseable input therefore never
// reaches a store; a payload that parses but fails in-transaction validation
// (unresolved endpoints, merge conflicts) rolls back cleanly, though opening
// an absent store for that attempt may have created it empty first.

use crate::knowledge::JVal;

/// Export formats: raw canonical JSONL, or a Markdown wrapper carrying the
/// same payload in one fenced block plus presentational prose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportFormat {
    Jsonl,
    Markdown,
}

/// A rendered export: the text, payload counts, and the generation the
/// payload was read from, for envelopes and tests.
#[derive(Debug, PartialEq)]
pub struct ExportDocument {
    pub text: String,
    pub records: usize,
    pub relations: usize,
    pub code_bindings: usize,
    pub generation: u64,
}

/// Import report for the CLI envelope.
#[derive(Debug, PartialEq)]
pub struct ImportReport {
    pub records_inserted: usize,
    pub revisions_appended: usize,
    pub relations_inserted: usize,
    pub code_bindings_inserted: usize,
    pub records_unchanged: usize,
    pub relations_unchanged: usize,
    pub code_bindings_unchanged: usize,
    pub generation: u64,
}

/// Resource bounds enforced before any write. The file bound is also applied
/// by the CLI before reading.
pub const MAX_IMPORT_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IMPORT_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMPORT_RECORD_LINES: usize = 1_000_000;
const MAX_IMPORT_RELATION_LINES: usize = 2_000_000;
const MAX_EVIDENCE_DIGEST_BYTES: usize = 4096;

type EventRow = (
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    String,
    i64,
);
type RelationRow = (String, i64, String, i64, String, String, String, i64);

/// Field-for-field port of one lifecycle audit event: a `record_lifecycle`
/// row, a `relation_lifecycle` row, or a `retractions` row.
#[derive(Debug, Clone, PartialEq)]
struct PortEvent {
    event: String,
    target_id: Option<String>,
    target_revision: Option<u64>,
    reason: Option<String>,
    author: String,
    created_unix: i64,
}

#[derive(Debug, Clone, PartialEq)]
struct PortEvidence {
    path: String,
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    digest: String,
    git_head: Option<String>,
    git_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct PortMetric {
    name: String,
    value: f64,
    unit: String,
}

#[derive(Debug, Clone, PartialEq)]
struct PortMeasurement {
    method: String,
    environment: String,
    sample_count: u64,
    metrics: Vec<PortMetric>,
}

#[derive(Debug, Clone, PartialEq)]
struct PortRevision {
    revision: u64,
    title: String,
    body: String,
    status: String,
    author: String,
    created_unix: i64,
    evidence: Vec<PortEvidence>,
    measurement: Option<PortMeasurement>,
    events: Vec<PortEvent>,
}

#[derive(Debug, Clone, PartialEq)]
struct PortRecord {
    id: String,
    kind: String,
    head_revision: u64,
    status: String,
    revisions: Vec<PortRevision>,
}

#[derive(Debug, Clone, PartialEq)]
struct PortRelation {
    relation_id: String,
    src_id: String,
    src_revision: u64,
    dst_id: String,
    dst_revision: u64,
    kind: String,
    author: String,
    rationale: String,
    created_unix: i64,
    events: Vec<PortEvent>,
}

/// One presentational record block in the Markdown wrapper.
struct PortProse {
    id: String,
    kind: String,
    head_revision: u64,
    status: String,
    title: String,
    body: String,
}

/// A parsed, fully validated import payload. Fields stay private: callers
/// only hand it back to [`import_payload`].
#[derive(Debug, PartialEq)]
pub struct ImportPayload {
    repo_name: String,
    records: Vec<PortRecord>,
    relations: Vec<PortRelation>,
    code_bindings: Vec<crate::knowledge_binding::PortCodeBinding>,
    needs_lifecycle: bool,
}

fn invalid_msg(msg: impl std::fmt::Display) -> String {
    format!("invalid import payload: {msg}")
}

fn corrupt_msg(msg: impl std::fmt::Display) -> String {
    format!("corrupt import file: {msg}")
}

fn conflict_msg(msg: impl std::fmt::Display) -> String {
    format!("import conflict: {msg}")
}

// --- emission ---------------------------------------------------------------

fn event_json(e: &PortEvent) -> String {
    let mut s = format!("{{\"event\":\"{}\"", crate::json::escape(&e.event));
    if let Some(t) = &e.target_id {
        s.push_str(&format!(",\"target_id\":\"{}\"", crate::json::escape(t)));
    }
    if let Some(r) = e.target_revision {
        s.push_str(&format!(",\"target_revision\":{r}"));
    }
    if let Some(r) = &e.reason {
        s.push_str(&format!(",\"reason\":\"{}\"", crate::json::escape(r)));
    }
    s.push_str(&format!(
        ",\"author\":\"{}\",\"created_unix\":{}}}",
        crate::json::escape(&e.author),
        e.created_unix
    ));
    s
}

fn evidence_json(e: &PortEvidence) -> String {
    let mut s = format!("{{\"path\":\"{}\"", crate::json::escape(&e.path));
    if let Some(b) = e.byte_start {
        s.push_str(&format!(",\"byte_start\":{b}"));
    }
    if let Some(b) = e.byte_end {
        s.push_str(&format!(",\"byte_end\":{b}"));
    }
    s.push_str(&format!(
        ",\"digest\":\"{}\"",
        crate::json::escape(&e.digest)
    ));
    if let Some(g) = &e.git_head {
        s.push_str(&format!(",\"git_head\":\"{}\"", crate::json::escape(g)));
    }
    if let Some(g) = &e.git_branch {
        s.push_str(&format!(",\"git_branch\":\"{}\"", crate::json::escape(g)));
    }
    s.push('}');
    s
}

/// Shortest round-trip float text (serde_json/ryu). Store reads reject
/// non-finite values before emission can see them.
fn metric_json(m: &PortMetric) -> String {
    let value = serde_json::to_string(&m.value).unwrap_or_else(|_| "null".to_string());
    format!(
        "{{\"name\":\"{}\",\"value\":{value},\"unit\":\"{}\"}}",
        crate::json::escape(&m.name),
        crate::json::escape(&m.unit)
    )
}

fn measurement_json(m: &PortMeasurement) -> String {
    let metrics: Vec<String> = m.metrics.iter().map(metric_json).collect();
    format!(
        "{{\"method\":\"{}\",\"environment\":\"{}\",\"sample_count\":{},\"metrics\":[{}]}}",
        crate::json::escape(&m.method),
        crate::json::escape(&m.environment),
        m.sample_count,
        metrics.join(",")
    )
}

fn revision_json(r: &PortRevision) -> String {
    let evidence: Vec<String> = r.evidence.iter().map(evidence_json).collect();
    let events: Vec<String> = r.events.iter().map(event_json).collect();
    let mut s = format!(
        "{{\"revision\":{},\"title\":\"{}\",\"body\":\"{}\",\"status\":\"{}\",\"author\":\"{}\",\"created_unix\":{},\"evidence\":[{}]",
        r.revision,
        crate::json::escape(&r.title),
        crate::json::escape(&r.body),
        crate::json::escape(&r.status),
        crate::json::escape(&r.author),
        r.created_unix,
        evidence.join(",")
    );
    if let Some(m) = &r.measurement {
        s.push_str(&format!(",\"measurement\":{}", measurement_json(m)));
    }
    s.push_str(&format!(",\"events\":[{}]}}", events.join(",")));
    s
}

fn record_json_line(rec: &PortRecord) -> String {
    let revisions: Vec<String> = rec.revisions.iter().map(revision_json).collect();
    format!(
        "{{\"type\":\"record\",\"id\":\"{}\",\"kind\":\"{}\",\"head_revision\":{},\"status\":\"{}\",\"revisions\":[{}]}}",
        crate::json::escape(&rec.id),
        crate::json::escape(&rec.kind),
        rec.head_revision,
        crate::json::escape(&rec.status),
        revisions.join(",")
    )
}

fn relation_json_line(rel: &PortRelation) -> String {
    let events: Vec<String> = rel.events.iter().map(event_json).collect();
    format!(
        "{{\"type\":\"relation\",\"relation_id\":\"{}\",\"src_id\":\"{}\",\"src_revision\":{},\"dst_id\":\"{}\",\"dst_revision\":{},\"kind\":\"{}\",\"author\":\"{}\",\"rationale\":\"{}\",\"created_unix\":{},\"events\":[{}]}}",
        crate::json::escape(&rel.relation_id),
        crate::json::escape(&rel.src_id),
        rel.src_revision,
        crate::json::escape(&rel.dst_id),
        rel.dst_revision,
        crate::json::escape(&rel.kind),
        crate::json::escape(&rel.author),
        crate::json::escape(&rel.rationale),
        rel.created_unix,
        events.join(",")
    )
}

fn meta_json_line(repo: &str, records: usize, relations: usize, code_bindings: usize) -> String {
    if code_bindings == 0 {
        format!(
            "{{\"type\":\"meta\",\"format\":\"riltra-knowledge\",\"version\":1,\"repo\":\"{}\",\"records\":{records},\"relations\":{relations}}}",
            crate::json::escape(repo)
        )
    } else {
        format!(
            "{{\"type\":\"meta\",\"format\":\"riltra-knowledge\",\"version\":2,\"repo\":\"{}\",\"records\":{records},\"relations\":{relations},\"code_bindings\":{code_bindings}}}",
            crate::json::escape(repo)
        )
    }
}

fn flush_record(
    current: &mut Option<PortRecord>,
    record_lines: &mut Vec<String>,
    prose: &mut Vec<PortProse>,
) {
    if let Some(rec) = current.take() {
        record_lines.push(record_json_line(&rec));
        if let Some(last) = rec.revisions.last() {
            prose.push(PortProse {
                id: rec.id,
                kind: rec.kind,
                head_revision: rec.head_revision,
                status: rec.status,
                title: last.title.clone(),
                body: last.body.clone(),
            });
        }
    }
}

// --- store readers ----------------------------------------------------------

fn store_evidence(conn: &Connection, id: &str, revision: i64) -> Result<Vec<PortEvidence>, String> {
    let items = read_evidence(conn, id, revision)?;
    Ok(items
        .into_iter()
        .map(|e| PortEvidence {
            path: e.path,
            byte_start: e.byte_start,
            byte_end: e.byte_end,
            digest: e.digest,
            git_head: e.git_head,
            git_branch: e.git_branch,
        })
        .collect())
}

fn store_measurement(
    conn: &Connection,
    id: &str,
    revision: i64,
) -> Result<Option<PortMeasurement>, String> {
    let Some(m) = read_measurement(conn, id, revision)? else {
        return Ok(None);
    };
    let mut metrics = Vec::with_capacity(m.metrics.len());
    for metric in m.metrics {
        if !metric.value.is_finite() {
            return Err(format!(
                "corrupt knowledge store: metric value of '{id}' revision {revision} is not finite"
            ));
        }
        metrics.push(PortMetric {
            name: metric.name,
            value: metric.value,
            unit: metric.unit,
        });
    }
    Ok(Some(PortMeasurement {
        method: m.method,
        environment: m.environment,
        sample_count: m.sample_count,
        metrics,
    }))
}

fn store_record_events(
    conn: &Connection,
    id: &str,
    revision: i64,
    has_lifecycle: bool,
) -> Result<Vec<PortEvent>, String> {
    if !has_lifecycle {
        return Ok(Vec::new());
    }
    let row: Option<EventRow> = conn
        .query_row(
            "SELECT event, target_id, target_revision, reason, author, created_unix
             FROM record_lifecycle WHERE id = ?1 AND revision = ?2",
            rusqlite::params![id, revision],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(|e| format!("cannot read lifecycle event: {e}"))?;
    let Some((event, target_id, target_revision, reason, author, created_unix)) = row else {
        return Ok(Vec::new());
    };
    let target_revision = match target_revision {
        None => None,
        Some(v) => Some(
            u64::try_from(v)
                .map_err(|_| "corrupt knowledge store: negative target revision".to_string())?,
        ),
    };
    if !matches!(event.as_str(), "supersede" | "retract" | "confirm") {
        return Err(format!(
            "corrupt knowledge store: unknown record lifecycle event '{event}'"
        ));
    }
    Ok(vec![PortEvent {
        event,
        target_id,
        target_revision,
        reason,
        author,
        created_unix,
    }])
}

fn store_relation_events(
    conn: &Connection,
    relation_id: &str,
    has_lifecycle: bool,
) -> Result<Vec<PortEvent>, String> {
    let mut events = Vec::new();
    if has_lifecycle {
        let confirm: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT event, author, created_unix FROM relation_lifecycle WHERE relation_id = ?1",
                [relation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| format!("cannot read relation lifecycle: {e}"))?;
        if let Some((event, author, created_unix)) = confirm {
            if event != "confirm" {
                return Err(format!(
                    "corrupt knowledge store: unknown relation lifecycle event '{event}'"
                ));
            }
            events.push(PortEvent {
                event,
                target_id: None,
                target_revision: None,
                reason: None,
                author,
                created_unix,
            });
        }
    }
    let retract: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT author, reason, created_unix FROM retractions WHERE relation_id = ?1",
            [relation_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| format!("cannot read retraction: {e}"))?;
    if let Some((author, reason, created_unix)) = retract {
        events.push(PortEvent {
            event: "retract".to_string(),
            target_id: None,
            target_revision: None,
            reason: Some(reason),
            author,
            created_unix,
        });
    }
    events.sort_by(|a, b| {
        (a.created_unix, a.event.as_str()).cmp(&(b.created_unix, b.event.as_str()))
    });
    Ok(events)
}

fn store_revision(
    conn: &Connection,
    id: &str,
    revision: u64,
    has_lifecycle: bool,
) -> Result<PortRevision, String> {
    let rev_i64 = i64::try_from(revision)
        .map_err(|_| "corrupt knowledge store: revision out of range".to_string())?;
    let row: Option<(String, String, String, String, i64)> = conn
        .query_row(
            "SELECT title, body, status, author, created_unix FROM records
             WHERE id = ?1 AND revision = ?2",
            rusqlite::params![id, rev_i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|e| format!("cannot read record revision: {e}"))?;
    let Some((title, body, status, author, created_unix)) = row else {
        return Err(format!(
            "corrupt knowledge store: record '{id}' has no revision {revision}"
        ));
    };
    Ok(PortRevision {
        revision,
        title,
        body,
        status,
        author,
        created_unix,
        evidence: store_evidence(conn, id, rev_i64)?,
        measurement: store_measurement(conn, id, rev_i64)?,
        events: store_record_events(conn, id, rev_i64, has_lifecycle)?,
    })
}

fn store_relation(
    conn: &Connection,
    relation_id: &str,
    has_lifecycle: bool,
) -> Result<Option<PortRelation>, String> {
    let row: Option<RelationRow> = conn
        .query_row(
            "SELECT src_id, src_revision, dst_id, dst_revision, kind, author, rationale, created_unix
             FROM relations WHERE relation_id = ?1",
            [relation_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(|e| format!("cannot read relation: {e}"))?;
    let Some((src_id, src_rev, dst_id, dst_rev, kind, author, rationale, created_unix)) = row
    else {
        return Ok(None);
    };
    let src_revision = u64::try_from(src_rev)
        .map_err(|_| "corrupt knowledge store: relation revision out of range".to_string())?;
    let dst_revision = u64::try_from(dst_rev)
        .map_err(|_| "corrupt knowledge store: relation revision out of range".to_string())?;
    Ok(Some(PortRelation {
        relation_id: relation_id.to_string(),
        src_id,
        src_revision,
        dst_id,
        dst_revision,
        kind,
        author,
        rationale,
        created_unix,
        events: store_relation_events(conn, relation_id, has_lifecycle)?,
    }))
}

// --- export -----------------------------------------------------------------

/// Render the canonical export of one store. Read-only and deterministic from
/// the store contents alone; record lines sort by id and revision, relation
/// lines by endpoint tuple, kind, then relation id (all binary collation).
pub fn export_document(
    conn: &mut Connection,
    format: ExportFormat,
) -> Result<ExportDocument, String> {
    // One deferred read transaction: every read below (repo meta, lifecycle
    // presence, record revisions with their evidence, measurements and
    // lifecycle events, relation lines with their events, and the reported
    // generation) sees the same committed snapshot, so the payload and the
    // generation reported alongside it can never straddle a concurrent
    // commit. WAL lets the snapshot proceed without blocking writers.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|e| format!("cannot begin knowledge export: {e}"))?;
    let repo = meta_value(&tx, "repo_name")?;
    let has_lifecycle = lifecycle_present(&tx)?;

    let mut record_lines: Vec<String> = Vec::new();
    let mut prose: Vec<PortProse> = Vec::new();
    {
        let mut stmt = tx
            .prepare("SELECT id, revision, kind FROM records ORDER BY id COLLATE BINARY, revision")
            .map_err(|e| format!("cannot read records: {e}"))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| format!("cannot read records: {e}"))?;
        let mut current: Option<PortRecord> = None;
        while let Some(row) = rows
            .next()
            .map_err(|e| format!("cannot read records: {e}"))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| format!("cannot read records: {e}"))?;
            let revision: i64 = row
                .get(1)
                .map_err(|e| format!("cannot read records: {e}"))?;
            let kind: String = row
                .get(2)
                .map_err(|e| format!("cannot read records: {e}"))?;
            let revision = u64::try_from(revision)
                .map_err(|_| "corrupt knowledge store: negative revision".to_string())?;
            let rev = store_revision(&tx, &id, revision, has_lifecycle)?;
            let flush = match &current {
                Some(rec) => rec.id != id,
                None => false,
            };
            if flush {
                flush_record(&mut current, &mut record_lines, &mut prose);
            }
            match &mut current {
                Some(rec) => {
                    rec.head_revision = revision;
                    rec.status = rev.status.clone();
                    rec.revisions.push(rev);
                }
                None => {
                    current = Some(PortRecord {
                        id,
                        kind,
                        head_revision: revision,
                        status: rev.status.clone(),
                        revisions: vec![rev],
                    });
                }
            }
        }
        flush_record(&mut current, &mut record_lines, &mut prose);
    }

    let mut relation_lines: Vec<String> = Vec::new();
    {
        let mut stmt = tx
            .prepare(
                "SELECT relation_id, src_id, src_revision, dst_id, dst_revision, kind,
                        author, rationale, created_unix
                 FROM relations
                 ORDER BY src_id COLLATE BINARY, src_revision, dst_id COLLATE BINARY,
                          dst_revision, kind COLLATE BINARY, relation_id COLLATE BINARY",
            )
            .map_err(|e| format!("cannot read relations: {e}"))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| format!("cannot read relations: {e}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| format!("cannot read relations: {e}"))?
        {
            let relation_id: String = row
                .get(0)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let src_id: String = row
                .get(1)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let src_revision: i64 = row
                .get(2)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let dst_id: String = row
                .get(3)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let dst_revision: i64 = row
                .get(4)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let kind: String = row
                .get(5)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let author: String = row
                .get(6)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let rationale: String = row
                .get(7)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let created_unix: i64 = row
                .get(8)
                .map_err(|e| format!("cannot read relations: {e}"))?;
            let rel = PortRelation {
                src_revision: u64::try_from(src_revision)
                    .map_err(|_| "corrupt knowledge store: relation revision out of range")?,
                dst_revision: u64::try_from(dst_revision)
                    .map_err(|_| "corrupt knowledge store: relation revision out of range")?,
                events: store_relation_events(&tx, &relation_id, has_lifecycle)?,
                relation_id,
                src_id,
                dst_id,
                kind,
                author,
                rationale,
                created_unix,
            };
            relation_lines.push(relation_json_line(&rel));
        }
    }

    let records = record_lines.len();
    let relations = relation_lines.len();
    let code_binding_lines = crate::knowledge_binding::export_lines(&tx)?;
    let code_bindings = code_binding_lines.len();
    let mut payload = meta_json_line(&repo, records, relations, code_bindings);
    payload.push('\n');
    for line in &record_lines {
        payload.push_str(line);
        payload.push('\n');
    }
    for line in &relation_lines {
        payload.push_str(line);
        payload.push('\n');
    }
    for line in &code_binding_lines {
        payload.push_str(line);
        payload.push('\n');
    }
    let text = match format {
        ExportFormat::Jsonl => payload,
        ExportFormat::Markdown => markdown_wrapper(&repo, &payload, &prose),
    };
    // The generation is read inside the same snapshot as the payload, so the
    // envelope's generation always describes the exported data.
    let generation = read_generation(&tx)?;
    Ok(ExportDocument {
        text,
        records,
        relations,
        code_bindings,
        generation,
    })
}

/// The export of a repository whose store does not exist yet: a metadata-only
/// payload with zero counts. Reads answer empty and create nothing.
pub fn empty_export(repo: &str, format: ExportFormat) -> ExportDocument {
    let payload = format!("{}\n", meta_json_line(repo, 0, 0, 0));
    let text = match format {
        ExportFormat::Jsonl => payload,
        ExportFormat::Markdown => markdown_wrapper(repo, &payload, &[]),
    };
    ExportDocument {
        text,
        records: 0,
        relations: 0,
        code_bindings: 0,
        generation: 0,
    }
}

/// A prose line that would open or close a fence gets its first backtick
/// backslash-escaped, so imported prose can never alter what extraction sees.
fn escape_prose_line(line: &str) -> String {
    match line.find("```") {
        Some(idx) => format!("{}\\{}", &line[..idx], &line[idx..]),
        None => line.to_string(),
    }
}

fn markdown_wrapper(repo: &str, payload: &str, prose: &[PortProse]) -> String {
    let mut md = format!(
        "# Riltra knowledge export — {repo}\n\n\
         Lossless canonical payload in the fenced block below; the prose is\n\
         presentational and import reads fenced `json` blocks only.\n\n\
         ```json\n{payload}```\n"
    );
    for p in prose {
        md.push_str(&format!(
            "\n## {} — {} (current revision {}, {})\n\n",
            p.id, p.kind, p.head_revision, p.status
        ));
        let block = format!("{}\n\n{}", p.title, p.body);
        for line in block.lines() {
            md.push_str(&escape_prose_line(line));
            md.push('\n');
        }
    }
    md
}

/// Normalize an import file to its canonical JSONL payload. A file whose
/// first non-empty line starts with `{` passes through unchanged; anything
/// else is treated as a Markdown wrapper and every fenced ```json block is
/// concatenated in order.
pub fn extract_import_payload(text: &str) -> Result<String, String> {
    // A UTF-8 BOM (common from Windows editors) is stripped before format
    // detection so a BOM-prefixed JSONL file is not mistaken for Markdown
    // and does not fail the JSON parser.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut saw_content = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        saw_content = true;
        if line.trim_start().starts_with('{') {
            return Ok(text.to_string());
        }
        break;
    }
    if !saw_content {
        return Err(invalid_msg("file is empty"));
    }
    let mut payload = String::new();
    let mut blocks = 0usize;
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if inside {
            if trimmed == "```" {
                inside = false;
            } else {
                payload.push_str(line);
                payload.push('\n');
            }
        } else if trimmed == "```json" {
            inside = true;
            blocks += 1;
        }
    }
    if inside {
        return Err(invalid_msg("unterminated fenced payload block"));
    }
    if blocks == 0 {
        return Err(invalid_msg("no fenced payload blocks found"));
    }
    Ok(payload)
}

// --- parsing ----------------------------------------------------------------

fn port_uint(value: &JVal, path: &str) -> Result<u64, String> {
    match value {
        JVal::Int(i) if *i >= 0 => Ok(*i as u64),
        JVal::UInt(u) if *u <= i64::MAX as u64 => Ok(*u),
        _ => Err(invalid_msg(format!(
            "field `{path}` must be a non-negative integer in range"
        ))),
    }
}

fn port_positive(value: &JVal, path: &str) -> Result<u64, String> {
    let n = port_uint(value, path)?;
    if n == 0 {
        return Err(invalid_msg(format!("field `{path}` must be at least 1")));
    }
    Ok(n)
}

fn req_val<'a>(obj: &'a [(String, JVal)], key: &str, path: &str) -> Result<&'a JVal, String> {
    match crate::knowledge::find(obj, key) {
        Some(v) => Ok(v),
        None => Err(invalid_msg(format!(
            "missing required field `{path}.{key}`"
        ))),
    }
}

fn port_text(text: &str, path: &str, max: usize) -> Result<(), String> {
    if text.is_empty() {
        return Err(invalid_msg(format!("field `{path}` must be non-empty")));
    }
    if text.len() > max {
        return Err(invalid_msg(format!(
            "field `{path}` is {} bytes, at most {max} allowed",
            text.len()
        )));
    }
    Ok(())
}

fn opt_str_field(obj: &[(String, JVal)], key: &str, path: &str) -> Result<Option<String>, String> {
    match crate::knowledge::find(obj, key) {
        None | Some(JVal::Null) => Ok(None),
        Some(JVal::Str(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid_msg(format!(
            "type mismatch: field `{path}.{key}` must be a string"
        ))),
    }
}

fn parse_port_events(value: &JVal, path: &str, max: usize) -> Result<Vec<PortEvent>, String> {
    let items = match value {
        JVal::Arr(items) => items,
        _ => return Err(invalid_msg(format!("field `{path}` must be an array"))),
    };
    if items.len() > max {
        return Err(invalid_msg(format!(
            "field `{path}` carries {} events, at most {max} allowed",
            items.len()
        )));
    }
    let mut events = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let p = format!("{path}[{i}]");
        let obj = crate::knowledge::as_obj(item, &p).map_err(invalid_msg)?;
        crate::knowledge::check_known(
            obj,
            &[
                "event",
                "target_id",
                "target_revision",
                "reason",
                "author",
                "created_unix",
            ],
            &p,
        )
        .map_err(invalid_msg)?;
        let event = crate::knowledge::req_str(obj, "event", &p).map_err(invalid_msg)?;
        if !matches!(event.as_str(), "supersede" | "retract" | "confirm") {
            return Err(invalid_msg(format!(
                "field `{p}.event` must be supersede, retract or confirm"
            )));
        }
        let author = crate::knowledge::req_str(obj, "author", &p).map_err(invalid_msg)?;
        check_actor(&author, &format!("{p}.author")).map_err(invalid_msg)?;
        let created_unix = crate::knowledge::req_int_non_negative(
            req_val(obj, "created_unix", &p)?,
            &format!("{p}.created_unix"),
        )
        .map_err(invalid_msg)?;
        let target_id = match crate::knowledge::find(obj, "target_id") {
            Some(_) => Some(crate::knowledge::req_str(obj, "target_id", &p).map_err(invalid_msg)?),
            None => None,
        };
        let target_revision = match crate::knowledge::find(obj, "target_revision") {
            Some(v) => Some(port_positive(v, &format!("{p}.target_revision"))?),
            None => None,
        };
        let reason = match crate::knowledge::find(obj, "reason") {
            Some(_) => Some(crate::knowledge::req_str(obj, "reason", &p).map_err(invalid_msg)?),
            None => None,
        };
        let mut out = PortEvent {
            event: event.clone(),
            target_id: None,
            target_revision: None,
            reason: None,
            author,
            created_unix,
        };
        if event == "supersede" {
            let target = target_id
                .ok_or_else(|| invalid_msg(format!("missing required field `{p}.target_id`")))?;
            crate::knowledge::validate_id(&target).map_err(invalid_msg)?;
            out.target_id = Some(target);
            out.target_revision = Some(target_revision.ok_or_else(|| {
                invalid_msg(format!("missing required field `{p}.target_revision`"))
            })?);
            let reason = reason
                .ok_or_else(|| invalid_msg(format!("missing required field `{p}.reason`")))?;
            port_text(
                &reason,
                &format!("{p}.reason"),
                crate::knowledge::MAX_RATIONALE_BYTES,
            )?;
            out.reason = Some(reason);
        } else {
            if target_id.is_some() || target_revision.is_some() {
                return Err(invalid_msg(format!(
                    "field `{p}` may not carry target fields on a `{event}` event"
                )));
            }
            if event == "retract" {
                let reason = reason
                    .ok_or_else(|| invalid_msg(format!("missing required field `{p}.reason`")))?;
                port_text(
                    &reason,
                    &format!("{p}.reason"),
                    crate::knowledge::MAX_RATIONALE_BYTES,
                )?;
                out.reason = Some(reason);
            } else if reason.is_some() {
                return Err(invalid_msg(format!(
                    "field `{p}` may not carry a reason on a confirm event"
                )));
            }
        }
        events.push(out);
    }
    Ok(events)
}

fn check_relation_events(events: &[PortEvent]) -> Result<(), String> {
    for e in events {
        if e.event == "supersede" {
            return Err(invalid_msg("a relation event must be confirm or retract"));
        }
    }
    if let [a, b] = events {
        if a.event == b.event {
            return Err(invalid_msg(
                "a relation carries at most one confirm and one retract event",
            ));
        }
        if (a.created_unix, a.event.as_str()) >= (b.created_unix, b.event.as_str()) {
            return Err(corrupt_msg("relation events are not in canonical order"));
        }
    }
    Ok(())
}

/// Semantic coherence of a parsed payload. The store's write paths can only
/// produce revisions whose lifecycle status agrees with their recorded event,
/// whose kind agrees with measurement presence, and whose supersede events are
/// mirrored by their revision-pinned implicit relations, and the supersedes
/// lineage is acyclic. A payload that violates any of these is not a faithful
/// export of any coherent store and is refused before a store is opened.
/// Cross-store cycles cannot form without a merge conflict: the merge plan
/// already refuses any payload revision that differs from the stored one, so
/// every stored supersede edge is immutable under import and a cycle would
/// have to be payload-internal, which the lineage walk below rejects.
fn check_semantic_coherence(
    records: &[PortRecord],
    relations: &[PortRelation],
) -> Result<(), String> {
    for rec in records {
        for rev in &rec.revisions {
            // Native creation is the only author of revision 1 and writes only
            // active or archived with no event; a first revision carrying a
            // transition or a confirmation is not a faithful export of any
            // coherent store.
            if rev.revision == 1 {
                if !matches!(rev.status.as_str(), "active" | "archived") {
                    return Err(invalid_msg(format!(
                        "record '{}' revision 1 status '{}' cannot be produced by record creation",
                        rec.id, rev.status
                    )));
                }
                if !rev.events.is_empty() {
                    return Err(invalid_msg(format!(
                        "record '{}' revision 1 carries a lifecycle event that record creation cannot produce",
                        rec.id
                    )));
                }
            }
            // The lifecycle status must agree with the recorded event.
            match (
                rev.status.as_str(),
                rev.events.first().map(|e| e.event.as_str()),
            ) {
                ("superseded", Some("supersede")) => {}
                ("retracted", Some("retract")) => {}
                ("active" | "archived", None) => {}
                ("active" | "archived", Some("confirm")) => {}
                (status, event) => {
                    return Err(invalid_msg(format!(
                        "record '{}' revision {} status '{status}' disagrees with its lifecycle event {}",
                        rec.id,
                        rev.revision,
                        event.unwrap_or("none")
                    )));
                }
            }
            // The immutable kind decides measurement eligibility.
            if (rec.kind == "benchmark") != rev.measurement.is_some() {
                return Err(invalid_msg(format!(
                    "record '{}' revision {} kind '{}' disagrees with its measurement presence",
                    rec.id, rev.revision, rec.kind
                )));
            }
        }
    }
    // Every supersede event must be mirrored by its revision-pinned implicit
    // relation; the relation id itself is already pinned to the store-derived
    // formula per line.
    let relation_ids: std::collections::HashSet<&str> =
        relations.iter().map(|r| r.relation_id.as_str()).collect();
    for rec in records {
        for rev in &rec.revisions {
            if let Some(e) = rev.events.first() {
                if e.event == "supersede" {
                    let target_id = e.target_id.as_deref().unwrap_or_default();
                    let target_revision = e.target_revision.unwrap_or(0);
                    let expected = format!(
                        "supersedes:{target_id}:{target_revision}:{}:{}",
                        rec.id, rev.revision
                    );
                    if !relation_ids.contains(expected.as_str()) {
                        return Err(invalid_msg(format!(
                            "supersede of '{}:{}' has no implicit relation '{expected}'",
                            rec.id, rev.revision
                        )));
                    }
                }
            }
        }
    }
    // Every supersedes relation must be mirrored by the event on the revision
    // it pins as superseded.
    for rel in relations {
        if rel.kind == "supersedes" {
            let rec = records.iter().find(|r| r.id == rel.dst_id).ok_or_else(|| {
                invalid_msg(format!(
                    "supersedes relation '{}' names unknown revision '{}'",
                    rel.relation_id, rel.dst_id
                ))
            })?;
            let rev = rec
                .revisions
                .iter()
                .find(|v| v.revision == rel.dst_revision)
                .ok_or_else(|| {
                    invalid_msg(format!(
                        "supersedes relation '{}' names unknown revision {}:{}",
                        rel.relation_id, rel.dst_id, rel.dst_revision
                    ))
                })?;
            let agrees = rev.events.first().map(|e| {
                e.event == "supersede"
                    && e.target_id.as_deref() == Some(rel.src_id.as_str())
                    && e.target_revision == Some(rel.src_revision)
            });
            if !agrees.unwrap_or(false) {
                return Err(invalid_msg(format!(
                    "supersedes relation '{}' has no agreeing supersede event on {}:{}",
                    rel.relation_id, rel.dst_id, rel.dst_revision
                )));
            }
        }
    }
    supersedes_lineage_is_acyclic(records, &[])
}

/// The supersedes lineage must be acyclic at record level. Edges use the
/// store's own orientation: `a -> b` means `a` is the replacement revision's
/// record and `b` the superseded record. The store refuses any supersede
/// transition whose lineage walk — over every stored supersedes relation,
/// retracted ones included, because the lineage is history — can reach the
/// record about to be superseded, so a payload that closes such a cycle,
/// alone or against stored history, is not a faithful export of any coherent
/// store and is refused. A record may supersede different targets over its
/// revisions, so a node can carry several outgoing edges.
fn supersedes_lineage_is_acyclic(
    records: &[PortRecord],
    stored_edges: &[(String, String)],
) -> Result<(), String> {
    let mut edges: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (src, dst) in stored_edges {
        edges.entry(src.as_str()).or_default().push(dst.as_str());
    }
    for rec in records {
        for rev in &rec.revisions {
            if let Some(e) = rev.events.first() {
                if e.event == "supersede" {
                    edges
                        .entry(e.target_id.as_deref().unwrap_or_default())
                        .or_default()
                        .push(rec.id.as_str());
                }
            }
        }
    }
    // Iterative tri-color DFS: 1 = on the current path, 2 = fully explored.
    let mut state: std::collections::HashMap<&str, u8> = std::collections::HashMap::new();
    let mut stack: Vec<(&str, usize)> = Vec::new();
    for start in edges.keys() {
        if state.contains_key(*start) {
            continue;
        }
        state.insert(*start, 1);
        stack.push((*start, 0));
        while let Some(&(node, mut at)) = stack.last() {
            let children = edges.get(node).map(Vec::as_slice).unwrap_or_default();
            match children.get(at) {
                Some(&child) => {
                    at += 1;
                    if let Some(last) = stack.last_mut() {
                        last.1 = at;
                    }
                    match state.get(child).copied().unwrap_or(0) {
                        1 => {
                            return Err(invalid_msg(format!(
                                "supersedes lineage forms a cycle through '{child}'"
                            )));
                        }
                        2 => {}
                        _ => {
                            state.insert(child, 1);
                            stack.push((child, 0));
                        }
                    }
                }
                None => {
                    state.insert(node, 2);
                    stack.pop();
                }
            }
        }
    }
    Ok(())
}

fn parse_port_evidence(value: &JVal, path: &str) -> Result<Vec<PortEvidence>, String> {
    let items = match value {
        JVal::Arr(items) => items,
        _ => return Err(invalid_msg(format!("field `{path}` must be an array"))),
    };
    if items.len() > crate::knowledge::MAX_EVIDENCE_REFS {
        return Err(invalid_msg(format!(
            "field `{path}` holds {} references, at most {} allowed",
            items.len(),
            crate::knowledge::MAX_EVIDENCE_REFS
        )));
    }
    let mut refs = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let p = format!("{path}[{i}]");
        let obj = crate::knowledge::as_obj(item, &p).map_err(invalid_msg)?;
        crate::knowledge::check_known(
            obj,
            &[
                "path",
                "byte_start",
                "byte_end",
                "digest",
                "git_head",
                "git_branch",
            ],
            &p,
        )
        .map_err(invalid_msg)?;
        let ref_path = crate::knowledge::req_str(obj, "path", &p).map_err(invalid_msg)?;
        port_text(
            &ref_path,
            &format!("{p}.path"),
            crate::knowledge::MAX_RATIONALE_BYTES,
        )?;
        let mut byte_start = None;
        let mut byte_end = None;
        if let Some(v) = crate::knowledge::find(obj, "byte_start") {
            byte_start = Some(
                crate::knowledge::req_int_non_negative(v, &format!("{p}.byte_start"))
                    .map_err(invalid_msg)?,
            );
        }
        if let Some(v) = crate::knowledge::find(obj, "byte_end") {
            byte_end = Some(
                crate::knowledge::req_int_non_negative(v, &format!("{p}.byte_end"))
                    .map_err(invalid_msg)?,
            );
        }
        if let (Some(start), Some(end)) = (byte_start, byte_end) {
            if end <= start {
                return Err(invalid_msg(format!(
                    "field `{p}` span must satisfy byte_end > byte_start"
                )));
            }
        }
        let digest = crate::knowledge::req_str(obj, "digest", &p).map_err(invalid_msg)?;
        port_text(&digest, &format!("{p}.digest"), MAX_EVIDENCE_DIGEST_BYTES)?;
        refs.push(PortEvidence {
            path: ref_path,
            byte_start,
            byte_end,
            digest,
            git_head: opt_str_field(obj, "git_head", &p)?,
            git_branch: opt_str_field(obj, "git_branch", &p)?,
        });
    }
    Ok(refs)
}

fn parse_port_measurement(value: &JVal, path: &str) -> Result<PortMeasurement, String> {
    let obj = crate::knowledge::as_obj(value, path).map_err(invalid_msg)?;
    crate::knowledge::check_known(
        obj,
        &["method", "environment", "sample_count", "metrics"],
        path,
    )
    .map_err(invalid_msg)?;
    let method = crate::knowledge::req_str(obj, "method", path).map_err(invalid_msg)?;
    let environment = crate::knowledge::req_str(obj, "environment", path).map_err(invalid_msg)?;
    if method.is_empty() || environment.is_empty() {
        return Err(invalid_msg(format!(
            "field `{path}` method and environment are required"
        )));
    }
    let sample_count = match req_val(obj, "sample_count", path)? {
        JVal::Int(i) if *i > 0 => *i as u64,
        JVal::UInt(u) if *u > 0 && *u <= i64::MAX as u64 => *u,
        _ => {
            return Err(invalid_msg(format!(
                "field `{path}.sample_count` must be a positive integer in range"
            )));
        }
    };
    let metric_items = match req_val(obj, "metrics", path)? {
        JVal::Arr(items) => items,
        _ => {
            return Err(invalid_msg(format!(
                "field `{path}.metrics` must be an array"
            )))
        }
    };
    if metric_items.is_empty() {
        return Err(invalid_msg(format!(
            "field `{path}.metrics` must name at least one metric"
        )));
    }
    let mut metrics: Vec<PortMetric> = Vec::with_capacity(metric_items.len());
    for (i, item) in metric_items.iter().enumerate() {
        let p = format!("{path}.metrics[{i}]");
        let mobj = crate::knowledge::as_obj(item, &p).map_err(invalid_msg)?;
        crate::knowledge::check_known(mobj, &["name", "value", "unit"], &p).map_err(invalid_msg)?;
        let name = crate::knowledge::req_str(mobj, "name", &p).map_err(invalid_msg)?;
        if name.is_empty() {
            return Err(invalid_msg(format!("field `{p}.name` must be non-empty")));
        }
        if metrics.iter().any(|m| m.name == name) {
            return Err(invalid_msg(format!(
                "field `{path}.metrics` repeats the metric name '{name}'"
            )));
        }
        let value = match req_val(mobj, "value", &p)? {
            JVal::Int(i) => *i as f64,
            JVal::UInt(u) => *u as f64,
            JVal::Float(f) => {
                crate::knowledge::validate_metric_value(*f).map_err(invalid_msg)?;
                *f
            }
            _ => return Err(invalid_msg(format!("field `{p}.value` must be a number"))),
        };
        let unit = crate::knowledge::req_str(mobj, "unit", &p).map_err(invalid_msg)?;
        if unit.is_empty() {
            return Err(invalid_msg(format!("field `{p}.unit` must be explicit")));
        }
        metrics.push(PortMetric { name, value, unit });
    }
    Ok(PortMeasurement {
        method,
        environment,
        sample_count,
        metrics,
    })
}

fn parse_port_record(obj: &[(String, JVal)], line_no: usize) -> Result<PortRecord, String> {
    let p = format!("line {line_no}");
    crate::knowledge::check_known(
        obj,
        &["type", "id", "kind", "head_revision", "status", "revisions"],
        &p,
    )
    .map_err(invalid_msg)?;
    let id = crate::knowledge::req_str(obj, "id", &p).map_err(invalid_msg)?;
    crate::knowledge::validate_id(&id).map_err(invalid_msg)?;
    let kind = crate::knowledge::req_str(obj, "kind", &p).map_err(invalid_msg)?;
    crate::knowledge::validate_kind(&kind).map_err(invalid_msg)?;
    let head_revision = port_positive(
        req_val(obj, "head_revision", &p)?,
        &format!("{p}.head_revision"),
    )?;
    let status = crate::knowledge::req_str(obj, "status", &p).map_err(invalid_msg)?;
    crate::knowledge::validate_status(&status).map_err(invalid_msg)?;
    let rev_items = match req_val(obj, "revisions", &p)? {
        JVal::Arr(items) if !items.is_empty() => items,
        JVal::Arr(_) => {
            return Err(invalid_msg(format!(
                "field `{p}.revisions` must hold at least one revision"
            )));
        }
        _ => {
            return Err(invalid_msg(format!(
                "field `{p}.revisions` must be an array"
            )))
        }
    };
    let mut revisions = Vec::with_capacity(rev_items.len());
    for (i, item) in rev_items.iter().enumerate() {
        let rp = format!("{p}.revisions[{i}]");
        let robj = crate::knowledge::as_obj(item, &rp).map_err(invalid_msg)?;
        crate::knowledge::check_known(
            robj,
            &[
                "revision",
                "title",
                "body",
                "status",
                "author",
                "created_unix",
                "evidence",
                "measurement",
                "events",
            ],
            &rp,
        )
        .map_err(invalid_msg)?;
        let revision = port_positive(req_val(robj, "revision", &rp)?, &format!("{rp}.revision"))?;
        if revision != (i as u64) + 1 {
            return Err(corrupt_msg(format!(
                "revisions of '{id}' are not contiguous from 1"
            )));
        }
        let title = crate::knowledge::req_str(robj, "title", &rp).map_err(invalid_msg)?;
        port_text(
            &title,
            &format!("{rp}.title"),
            crate::knowledge::MAX_TITLE_BYTES,
        )?;
        let body = crate::knowledge::req_str(robj, "body", &rp).map_err(invalid_msg)?;
        port_text(
            &body,
            &format!("{rp}.body"),
            crate::knowledge::MAX_BODY_BYTES,
        )?;
        let rev_status = crate::knowledge::req_str(robj, "status", &rp).map_err(invalid_msg)?;
        crate::knowledge::validate_status(&rev_status).map_err(invalid_msg)?;
        let author = crate::knowledge::req_str(robj, "author", &rp).map_err(invalid_msg)?;
        check_actor(&author, &format!("{rp}.author")).map_err(invalid_msg)?;
        let created_unix = crate::knowledge::req_int_non_negative(
            req_val(robj, "created_unix", &rp)?,
            &format!("{rp}.created_unix"),
        )
        .map_err(invalid_msg)?;
        let evidence =
            parse_port_evidence(req_val(robj, "evidence", &rp)?, &format!("{rp}.evidence"))?;
        let measurement = match crate::knowledge::find(robj, "measurement") {
            None => None,
            Some(v) => Some(parse_port_measurement(v, &format!("{rp}.measurement"))?),
        };
        let events = parse_port_events(req_val(robj, "events", &rp)?, &format!("{rp}.events"), 1)?;
        revisions.push(PortRevision {
            revision,
            title,
            body,
            status: rev_status,
            author,
            created_unix,
            evidence,
            measurement,
            events,
        });
    }
    let last = revisions
        .last()
        .expect("revisions is non-empty by construction");
    if head_revision != last.revision {
        return Err(corrupt_msg(format!(
            "head_revision of '{id}' does not match its last revision"
        )));
    }
    if status != last.status {
        return Err(corrupt_msg(format!(
            "status of '{id}' does not match its last revision's status"
        )));
    }
    Ok(PortRecord {
        id,
        kind,
        head_revision,
        status,
        revisions,
    })
}

fn parse_port_relation(obj: &[(String, JVal)], line_no: usize) -> Result<PortRelation, String> {
    let p = format!("line {line_no}");
    crate::knowledge::check_known(
        obj,
        &[
            "type",
            "relation_id",
            "src_id",
            "src_revision",
            "dst_id",
            "dst_revision",
            "kind",
            "author",
            "rationale",
            "created_unix",
            "events",
        ],
        &p,
    )
    .map_err(invalid_msg)?;
    let relation_id = crate::knowledge::req_str(obj, "relation_id", &p).map_err(invalid_msg)?;
    crate::knowledge::validate_relation_reference(&relation_id).map_err(invalid_msg)?;
    let src_id = crate::knowledge::req_str(obj, "src_id", &p).map_err(invalid_msg)?;
    crate::knowledge::validate_id(&src_id).map_err(invalid_msg)?;
    let src_revision = port_positive(
        req_val(obj, "src_revision", &p)?,
        &format!("{p}.src_revision"),
    )?;
    let dst_id = crate::knowledge::req_str(obj, "dst_id", &p).map_err(invalid_msg)?;
    crate::knowledge::validate_id(&dst_id).map_err(invalid_msg)?;
    let dst_revision = port_positive(
        req_val(obj, "dst_revision", &p)?,
        &format!("{p}.dst_revision"),
    )?;
    let kind = crate::knowledge::req_str(obj, "kind", &p).map_err(invalid_msg)?;
    if kind == "supersedes" {
        // Store-derived identity: the exact deterministic formula the
        // supersede transition writes, never a caller-shaped id. The
        // relation runs replacement (src) -> superseded revision (dst).
        let expected = format!("supersedes:{src_id}:{src_revision}:{dst_id}:{dst_revision}");
        if relation_id != expected {
            return Err(invalid_msg(format!(
                "field `{p}.relation_id` must be '{expected}' for a supersedes relation"
            )));
        }
    } else {
        crate::knowledge::validate_relation_kind(&kind).map_err(invalid_msg)?;
    }
    let author = crate::knowledge::req_str(obj, "author", &p).map_err(invalid_msg)?;
    check_actor(&author, &format!("{p}.author")).map_err(invalid_msg)?;
    let rationale = crate::knowledge::req_str(obj, "rationale", &p).map_err(invalid_msg)?;
    check_rationale(&rationale, &format!("{p}.rationale")).map_err(invalid_msg)?;
    let created_unix = crate::knowledge::req_int_non_negative(
        req_val(obj, "created_unix", &p)?,
        &format!("{p}.created_unix"),
    )
    .map_err(invalid_msg)?;
    let events = parse_port_events(req_val(obj, "events", &p)?, &format!("{p}.events"), 2)?;
    check_relation_events(&events)?;
    Ok(PortRelation {
        relation_id,
        src_id,
        src_revision,
        dst_id,
        dst_revision,
        kind,
        author,
        rationale,
        created_unix,
        events,
    })
}

/// Parse and fully validate an import payload without touching any store.
/// Field-level, enumeration, and line-limit violations are
/// `invalid import payload` (CLI exit 2); whole-file coherence violations
/// (duplicates, ordering, counts, prefix consistency, event order) are
/// `corrupt import file` (CLI exit 3).
pub fn parse_import_payload(text: &str) -> Result<ImportPayload, String> {
    let mut meta: Option<(String, usize, usize, u64, Option<usize>)> = None;
    let mut records: Vec<PortRecord> = Vec::new();
    let mut relations: Vec<PortRelation> = Vec::new();
    let mut code_bindings: Vec<crate::knowledge_binding::PortCodeBinding> = Vec::new();
    let mut prev_id: Option<String> = None;
    let mut prev_rel_key: Option<(String, u64, String, u64, String, String)> = None;
    let mut prev_binding_key = None;
    let mut seen_relation_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_relation = false;
    let mut content_index = 0usize;
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_no = idx + 1;
        if line.len() > MAX_IMPORT_LINE_BYTES {
            return Err(invalid_msg(format!(
                "line {line_no} is {} bytes, at most {MAX_IMPORT_LINE_BYTES} allowed",
                line.len()
            )));
        }
        let value = crate::knowledge::parse_dupless(line)
            .map_err(|e| invalid_msg(format!("line {line_no}: {e}")))?;
        let obj =
            crate::knowledge::as_obj(&value, &format!("line {line_no}")).map_err(invalid_msg)?;
        let ty = match crate::knowledge::find(obj, "type") {
            Some(JVal::Str(s)) => s.clone(),
            Some(_) => {
                return Err(invalid_msg(format!(
                    "line {line_no}: `type` must be a string"
                )));
            }
            None => {
                return Err(invalid_msg(format!(
                    "line {line_no}: missing required field `type`"
                )));
            }
        };
        match ty.as_str() {
            "meta" => {
                if content_index != 0 {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: meta line is not first"
                    )));
                }
                crate::knowledge::check_known(
                    obj,
                    &[
                        "type",
                        "format",
                        "version",
                        "repo",
                        "records",
                        "relations",
                        "code_bindings",
                    ],
                    &format!("line {line_no}"),
                )
                .map_err(invalid_msg)?;
                let format = crate::knowledge::req_str(obj, "format", &format!("line {line_no}"))
                    .map_err(invalid_msg)?;
                if format != "riltra-knowledge" {
                    return Err(invalid_msg(format!(
                        "line {line_no}: unsupported format '{format}'"
                    )));
                }
                let version = port_positive(
                    req_val(obj, "version", &format!("line {line_no}"))?,
                    &format!("line {line_no}.version"),
                )?;
                if !matches!(version, 1 | 2) {
                    return Err(invalid_msg(format!(
                        "line {line_no}: unsupported format version {version}"
                    )));
                }
                let declared_code_bindings =
                    match (version, crate::knowledge::find(obj, "code_bindings")) {
                        (1, None) => None,
                        (1, Some(_)) => {
                            return Err(invalid_msg(format!(
                                "line {line_no}: format version 1 cannot carry code_bindings"
                            )));
                        }
                        (2, Some(value)) => {
                            let count = port_uint(value, &format!("line {line_no}.code_bindings"))?;
                            Some(
                                usize::try_from(count)
                                    .map_err(|_| invalid_msg("code binding count out of range"))?,
                            )
                        }
                        (2, None) => {
                            return Err(invalid_msg(format!(
                                "line {line_no}: format version 2 requires `code_bindings`"
                            )));
                        }
                        _ => unreachable!(),
                    };
                let repo = crate::knowledge::req_str(obj, "repo", &format!("line {line_no}"))
                    .map_err(invalid_msg)?;
                if repo.is_empty() {
                    return Err(invalid_msg(format!(
                        "line {line_no}: `repo` must be non-empty"
                    )));
                }
                let declared_records = port_uint(
                    req_val(obj, "records", &format!("line {line_no}"))?,
                    &format!("line {line_no}.records"),
                )?;
                let declared_relations = port_uint(
                    req_val(obj, "relations", &format!("line {line_no}"))?,
                    &format!("line {line_no}.relations"),
                )?;
                let declared_records = usize::try_from(declared_records)
                    .map_err(|_| invalid_msg("record count out of range"))?;
                let declared_relations = usize::try_from(declared_relations)
                    .map_err(|_| invalid_msg("relation count out of range"))?;
                meta = Some((
                    repo,
                    declared_records,
                    declared_relations,
                    version,
                    declared_code_bindings,
                ));
            }
            "record" => {
                if meta.is_none() {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: record line before the meta line"
                    )));
                }
                if seen_relation || !code_bindings.is_empty() {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: record line after relation lines"
                    )));
                }
                if records.len() >= MAX_IMPORT_RECORD_LINES {
                    return Err(invalid_msg(format!(
                        "more than {MAX_IMPORT_RECORD_LINES} record lines"
                    )));
                }
                let rec = parse_port_record(obj, line_no)?;
                if let Some(prev) = &prev_id {
                    if *prev == rec.id {
                        return Err(corrupt_msg(format!(
                            "line {line_no}: duplicate record line '{}'",
                            rec.id
                        )));
                    }
                    if *prev > rec.id {
                        return Err(corrupt_msg(format!(
                            "line {line_no}: record lines are not in canonical order"
                        )));
                    }
                }
                prev_id = Some(rec.id.clone());
                records.push(rec);
            }
            "relation" => {
                if meta.is_none() {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: relation line before the meta line"
                    )));
                }
                if !code_bindings.is_empty() {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: relation line after code binding lines"
                    )));
                }
                seen_relation = true;
                if relations.len() >= MAX_IMPORT_RELATION_LINES {
                    return Err(invalid_msg(format!(
                        "more than {MAX_IMPORT_RELATION_LINES} relation lines"
                    )));
                }
                let rel = parse_port_relation(obj, line_no)?;
                if !seen_relation_ids.insert(rel.relation_id.clone()) {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: duplicate relation line '{}'",
                        rel.relation_id
                    )));
                }
                let key = (
                    rel.src_id.clone(),
                    rel.src_revision,
                    rel.dst_id.clone(),
                    rel.dst_revision,
                    rel.kind.clone(),
                    rel.relation_id.clone(),
                );
                if let Some(prev) = &prev_rel_key {
                    if *prev >= key {
                        return Err(corrupt_msg(format!(
                            "line {line_no}: relation lines are not in canonical order"
                        )));
                    }
                }
                prev_rel_key = Some(key);
                relations.push(rel);
            }
            "code_binding" => {
                let Some((_, _, _, version, _)) = &meta else {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: code binding line before the meta line"
                    )));
                };
                if *version != 2 {
                    return Err(invalid_msg(format!(
                        "line {line_no}: code binding lines require format version 2"
                    )));
                }
                if code_bindings.len() >= MAX_IMPORT_RELATION_LINES {
                    return Err(invalid_msg(format!(
                        "more than {} code binding lines",
                        MAX_IMPORT_RELATION_LINES
                    )));
                }
                let binding = crate::knowledge_binding::parse_export_line(line)
                    .map_err(|e| invalid_msg(format!("line {line_no}: {e}")))?;
                let key = crate::knowledge_binding::binding_sort_key(&binding);
                if prev_binding_key.as_ref().is_some_and(|prev| prev >= &key) {
                    return Err(corrupt_msg(format!(
                        "line {line_no}: code binding lines are duplicated or not in canonical order"
                    )));
                }
                prev_binding_key = Some(key);
                code_bindings.push(binding);
            }
            other => {
                return Err(invalid_msg(format!(
                    "line {line_no}: unknown line type '{other}'"
                )));
            }
        }
        content_index += 1;
    }
    let Some((repo_name, declared_records, declared_relations, _version, declared_code_bindings)) =
        meta
    else {
        return Err(invalid_msg("missing meta line"));
    };
    if records.len() != declared_records {
        return Err(corrupt_msg(format!(
            "declared record count {declared_records} does not match {} record lines",
            records.len()
        )));
    }
    if relations.len() != declared_relations {
        return Err(corrupt_msg(format!(
            "declared relation count {declared_relations} does not match {} relation lines",
            relations.len()
        )));
    }
    if let Some(declared) = declared_code_bindings {
        if code_bindings.len() != declared {
            return Err(corrupt_msg(format!(
                "declared code binding count {declared} does not match {} code binding lines",
                code_bindings.len()
            )));
        }
    }
    check_semantic_coherence(&records, &relations)?;
    let needs_lifecycle = records
        .iter()
        .any(|r| r.revisions.iter().any(|v| !v.events.is_empty()))
        || relations.iter().any(|r| !r.events.is_empty());
    Ok(ImportPayload {
        repo_name,
        records,
        relations,
        code_bindings,
        needs_lifecycle,
    })
}

// --- import -----------------------------------------------------------------

/// Merge a parsed payload into one store under a single `Immediate`
/// transaction. Referential integrity is validated across the file and the
/// store before the first write, merge conflicts abort before any write, and
/// the whole import is atomic: a failure rolls back every insert and any
/// schema migration. Repeat imports of identical content are no-ops and leave
/// the generation untouched. No clock value is ever minted: every timestamp
/// comes from the payload.
pub fn import_payload(
    conn: &mut Connection,
    payload: &ImportPayload,
) -> Result<ImportReport, String> {
    revalidate_open_paths(conn)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot begin knowledge write: {e}"))?;
    // Stored supersedes history combines with the payload's edges: the store
    // refuses any transition that would close a record-level supersede cycle
    // over every stored relation, retracted ones included, so an import that
    // would close one across the store/payload boundary is refused before the
    // first write. The payload alone was checked at parse time; this walk
    // adds the stored historical edges.
    let mut stored_edges: Vec<(String, String)> = Vec::new();
    {
        let mut stmt = tx
            .prepare("SELECT src_id, dst_id FROM relations WHERE kind = 'supersedes'")
            .map_err(|e| format!("cannot read supersedes lineage: {e}"))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| format!("cannot read supersedes lineage: {e}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| format!("cannot read supersedes lineage: {e}"))?
        {
            let src: String = row
                .get(0)
                .map_err(|e| format!("cannot read supersedes lineage: {e}"))?;
            let dst: String = row
                .get(1)
                .map_err(|e| format!("cannot read supersedes lineage: {e}"))?;
            stored_edges.push((src, dst));
        }
    }
    supersedes_lineage_is_acyclic(&payload.records, &stored_edges)?;
    if payload.needs_lifecycle {
        ensure_lifecycle_schema(&tx)?;
    }
    if !payload.code_bindings.is_empty() {
        crate::knowledge_binding::validate_import_bindings(
            &payload.code_bindings,
            &payload.repo_name,
            &meta_value(&tx, "repo_name")?,
            &crate::store::content_digest(meta_value(&tx, "canonical_root")?.as_bytes()),
        )?;
        ensure_code_binding_schema(&tx)?;
    }
    let has_lifecycle = lifecycle_present(&tx)?;

    // Phase A: referential integrity across the file and the store.
    let file_revs: std::collections::HashSet<(String, u64)> = payload
        .records
        .iter()
        .flat_map(|r| r.revisions.iter().map(move |v| (r.id.clone(), v.revision)))
        .collect();
    let endpoint = |id: &str, rev: u64| -> Result<bool, String> {
        if file_revs.contains(&(id.to_string(), rev)) {
            return Ok(true);
        }
        let rev_i64 = i64::try_from(rev)
            .map_err(|_| "corrupt import file: revision out of range".to_string())?;
        endpoint_exists(&tx, id, rev_i64)
    };
    for rel in &payload.relations {
        if !endpoint(&rel.src_id, rel.src_revision)? {
            return Err(invalid_msg(format!(
                "relation '{}' references unknown revision {}:{}",
                rel.relation_id, rel.src_id, rel.src_revision
            )));
        }
        if !endpoint(&rel.dst_id, rel.dst_revision)? {
            return Err(invalid_msg(format!(
                "relation '{}' references unknown revision {}:{}",
                rel.relation_id, rel.dst_id, rel.dst_revision
            )));
        }
    }
    for rec in &payload.records {
        for v in &rec.revisions {
            for e in &v.events {
                if e.event == "supersede" {
                    let target = e
                        .target_id
                        .as_deref()
                        .expect("supersede events always carry a target");
                    let revision = e
                        .target_revision
                        .expect("supersede events always carry a target revision");
                    if !endpoint(target, revision)? {
                        return Err(invalid_msg(format!(
                            "supersede event of '{}.{}' references unknown revision {target}:{revision}",
                            rec.id, v.revision
                        )));
                    }
                }
            }
        }
    }
    for binding in &payload.code_bindings {
        if !endpoint(&binding.record_id, binding.record_revision)? {
            return Err(invalid_msg(format!(
                "code binding references unknown revision {}:{}",
                binding.record_id, binding.record_revision
            )));
        }
    }

    // Phase B: merge plan. Identical overlap is a silent no-op; any overlap
    // with different content, or any duplicate live relation tuple, is a
    // conflict detected before the first write.
    let mut record_writes: Vec<(&PortRecord, u64)> = Vec::new();
    let mut records_unchanged = 0usize;
    for rec in &payload.records {
        let store_max: Option<i64> = tx
            .query_row(
                "SELECT max(revision) FROM records WHERE id = ?1",
                [rec.id.as_str()],
                |r| r.get(0),
            )
            .map_err(|e| format!("cannot read record head: {e}"))?;
        match store_max {
            None => record_writes.push((rec, 0)),
            Some(max) => {
                let max = u64::try_from(max)
                    .map_err(|_| "corrupt knowledge store: negative revision".to_string())?;
                // The record kind is record-level and stored on every
                // revision: a payload that renames it is divergence, not a
                // compatible subset or superset.
                let stored_kind: Option<String> = tx
                    .query_row(
                        "SELECT kind FROM records WHERE id = ?1 AND revision = 1",
                        [rec.id.as_str()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| format!("cannot read record kind: {e}"))?;
                if stored_kind.as_deref() != Some(rec.kind.as_str()) {
                    return Err(conflict_msg(format!(
                        "record '{}' kind differs from the stored record",
                        rec.id
                    )));
                }
                for v in rec.revisions.iter().filter(|v| v.revision <= max) {
                    let stored = store_revision(&tx, &rec.id, v.revision, has_lifecycle)?;
                    if &stored != v {
                        return Err(conflict_msg(format!(
                            "record '{}' revision {} differs from the stored revision",
                            rec.id, v.revision
                        )));
                    }
                }
                if rec.head_revision > max {
                    record_writes.push((rec, max + 1));
                } else {
                    records_unchanged += 1;
                }
            }
        }
    }

    let mut relation_writes: Vec<&PortRelation> = Vec::new();
    let mut relations_unchanged = 0usize;
    let mut planned_live: std::collections::HashMap<(String, u64, String, u64, String), String> =
        std::collections::HashMap::new();
    for rel in &payload.relations {
        match store_relation(&tx, &rel.relation_id, has_lifecycle)? {
            Some(stored) => {
                if &stored == rel {
                    relations_unchanged += 1;
                } else {
                    return Err(conflict_msg(format!(
                        "relation '{}' differs from the stored relation",
                        rel.relation_id
                    )));
                }
            }
            None => {
                let retracted = rel.events.iter().any(|e| e.event == "retract");
                if !retracted {
                    let tuple = (
                        rel.src_id.clone(),
                        rel.src_revision,
                        rel.dst_id.clone(),
                        rel.dst_revision,
                        rel.kind.clone(),
                    );
                    if let Some(other) = planned_live.get(&tuple) {
                        return Err(conflict_msg(format!(
                            "relation '{}' duplicates the live relation '{other}' \
                             (same endpoints and kind)",
                            rel.relation_id
                        )));
                    }
                    let src_i64 = i64::try_from(rel.src_revision)
                        .map_err(|_| "corrupt import file: revision out of range".to_string())?;
                    let dst_i64 = i64::try_from(rel.dst_revision)
                        .map_err(|_| "corrupt import file: revision out of range".to_string())?;
                    let duplicate: Option<String> = tx
                        .query_row(
                            "SELECT r.relation_id FROM relations r
                             LEFT JOIN retractions t ON t.relation_id = r.relation_id
                             WHERE r.src_id = ?1 AND r.src_revision = ?2
                               AND r.dst_id = ?3 AND r.dst_revision = ?4
                               AND r.kind = ?5 AND t.relation_id IS NULL
                               AND r.relation_id != ?6
                             LIMIT 1",
                            rusqlite::params![
                                rel.src_id,
                                src_i64,
                                rel.dst_id,
                                dst_i64,
                                rel.kind,
                                rel.relation_id
                            ],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|e| format!("cannot read relations: {e}"))?;
                    if let Some(other) = duplicate {
                        return Err(conflict_msg(format!(
                            "relation '{}' duplicates the live relation '{other}' \
                             (same endpoints and kind)",
                            rel.relation_id
                        )));
                    }
                    planned_live.insert(tuple, rel.relation_id.clone());
                }
                relation_writes.push(rel);
            }
        }
    }

    // Phase C: apply. Everything conflicting has already been refused.
    let mut records_inserted = 0usize;
    let mut revisions_appended = 0usize;
    let mut relations_inserted = 0usize;
    let mut code_bindings_inserted = 0usize;
    let mut code_bindings_unchanged = 0usize;
    let mut changed = false;
    for (rec, first_new) in record_writes {
        if first_new == 0 {
            records_inserted += 1;
        } else {
            revisions_appended += (rec.head_revision - first_new + 1) as usize;
        }
        for rev in rec
            .revisions
            .iter()
            .filter(|v| first_new == 0 || v.revision >= first_new)
        {
            let rev_i64 = i64::try_from(rev.revision)
                .map_err(|_| "corrupt import file: revision out of range".to_string())?;
            tx.execute(
                "INSERT INTO records (id, revision, kind, title, body, status, author, created_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    rec.id,
                    rev_i64,
                    rec.kind,
                    rev.title,
                    rev.body,
                    rev.status,
                    rev.author,
                    rev.created_unix
                ],
            )
            .map_err(|e| format!("cannot store record revision: {e}"))?;
            let captured: Vec<crate::knowledge::CapturedEvidence> = rev
                .evidence
                .iter()
                .map(|e| crate::knowledge::CapturedEvidence {
                    path: e.path.clone(),
                    byte_start: e.byte_start,
                    byte_end: e.byte_end,
                    digest: e.digest.clone(),
                    git_head: e.git_head.clone(),
                    git_branch: e.git_branch.clone(),
                })
                .collect();
            write_evidence(&tx, &rec.id, rev_i64, &captured)?;
            if let Some(m) = &rev.measurement {
                let measurement = crate::knowledge::BenchmarkData {
                    method: m.method.clone(),
                    environment: m.environment.clone(),
                    sample_count: m.sample_count,
                    metrics: m
                        .metrics
                        .iter()
                        .map(|x| crate::knowledge::MetricData {
                            name: x.name.clone(),
                            value: x.value,
                            unit: x.unit.clone(),
                        })
                        .collect(),
                };
                write_measurement(&tx, &rec.id, rev_i64, &measurement)?;
            }
            if let Some(e) = rev.events.first() {
                let target_i64 = match e.target_revision {
                    None => None,
                    Some(r) => Some(i64::try_from(r).map_err(|_| {
                        "corrupt import file: target revision out of range".to_string()
                    })?),
                };
                tx.execute(
                    "INSERT INTO record_lifecycle
                     (id, revision, event, target_id, target_revision, reason, author, created_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        rec.id,
                        rev_i64,
                        e.event,
                        e.target_id,
                        target_i64,
                        e.reason,
                        e.author,
                        e.created_unix
                    ],
                )
                .map_err(|e| format!("cannot store lifecycle event: {e}"))?;
            }
        }
        let head_i64 = i64::try_from(rec.head_revision)
            .map_err(|_| "corrupt import file: revision out of range".to_string())?;
        tx.execute(
            "INSERT INTO heads (id, current_revision) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET current_revision = excluded.current_revision",
            rusqlite::params![rec.id, head_i64],
        )
        .map_err(|e| format!("cannot store record head: {e}"))?;
        changed = true;
    }
    for rel in relation_writes {
        let src_i64 = i64::try_from(rel.src_revision)
            .map_err(|_| "corrupt import file: revision out of range".to_string())?;
        let dst_i64 = i64::try_from(rel.dst_revision)
            .map_err(|_| "corrupt import file: revision out of range".to_string())?;
        tx.execute(
            "INSERT INTO relations
             (relation_id, src_id, src_revision, dst_id, dst_revision, kind, author, rationale, created_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                rel.relation_id,
                rel.src_id,
                src_i64,
                rel.dst_id,
                dst_i64,
                rel.kind,
                rel.author,
                rel.rationale,
                rel.created_unix
            ],
        )
        .map_err(|e| format!("cannot store relation: {e}"))?;
        for e in &rel.events {
            match e.event.as_str() {
                "confirm" => {
                    tx.execute(
                        "INSERT INTO relation_lifecycle (relation_id, event, author, created_unix)
                         VALUES (?1, 'confirm', ?2, ?3)",
                        rusqlite::params![rel.relation_id, e.author, e.created_unix],
                    )
                    .map_err(|e| format!("cannot store relation confirmation: {e}"))?;
                }
                "retract" => {
                    tx.execute(
                        "INSERT INTO retractions (relation_id, author, reason, created_unix)
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![rel.relation_id, e.author, e.reason, e.created_unix],
                    )
                    .map_err(|e| format!("cannot store relation retraction: {e}"))?;
                }
                _ => {
                    return Err(invalid_msg("a relation event must be confirm or retract"));
                }
            }
        }
        relations_inserted += 1;
        changed = true;
    }
    for binding in &payload.code_bindings {
        if crate::knowledge_binding::import_binding(&tx, binding)? {
            code_bindings_inserted += 1;
            changed = true;
        } else {
            code_bindings_unchanged += 1;
        }
    }

    let generation = if changed {
        bump_generation(&tx)?
    } else {
        read_generation(&tx)?
    };
    tx.commit()
        .map_err(|e| format!("cannot commit import: {e}"))?;
    Ok(ImportReport {
        records_inserted,
        revisions_appended,
        relations_inserted,
        code_bindings_inserted,
        records_unchanged,
        relations_unchanged,
        code_bindings_unchanged,
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tmpdir;

    fn setup(name: &str) -> (PathBuf, RepoEntry) {
        let home = tmpdir("m7-home");
        let root = tmpdir("m7-root");
        // Fixtures mirror production registry entries, which record the
        // canonical root string at registration time.
        let canonical = crate::paths::canonical_root(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let repo = RepoEntry {
            name: name.to_string(),
            root: canonical,
            added_unix: 1,
        };
        (home, repo)
    }

    fn companion(db: &Path, suffix: &str) -> PathBuf {
        PathBuf::from(format!("{}{suffix}", db.display()))
    }

    #[test]
    fn knowledge_store_open_db_path_layout() {
        let home = PathBuf::from("/tmp/h");
        assert_eq!(
            knowledge_db_path(&home, "myrepo"),
            PathBuf::from("/tmp/h/knowledge/myrepo.db")
        );
    }

    #[test]
    fn knowledge_store_open_missing_store_read_creates_nothing() {
        let (home, repo) = setup("missing");
        let db = knowledge_db_path(&home, "missing");
        let err = open_knowledge_at(&home, &repo, false).unwrap_err();
        assert!(err.contains("no knowledge store"), "unexpected: {err}");
        assert!(!db.exists(), "read must not create the database");
        for s in ["-wal", "-shm", "-journal"] {
            assert!(
                !companion(&db, s).exists(),
                "read must not create companion {s}"
            );
        }
    }

    #[test]
    fn knowledge_store_open_readonly_never_creates_and_reads() {
        let (home, repo) = setup("ro");
        open_knowledge_at(&home, &repo, true).unwrap();
        let conn = open_knowledge_at(&home, &repo, false).unwrap();
        assert!(conn
            .execute_batch("CREATE TABLE probe_x (a INTEGER)")
            .is_err());
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_store_open_fresh_create_builds_schema_v1() {
        let (home, repo) = setup("fresh");
        let conn = open_knowledge_at(&home, &repo, true).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        // Fresh stores are created at v1; schema v2 arrives only through the
        // guarded Full-stage migration, so a store that never uses Full-stage
        // features stays readable by a Foundation binary.
        assert_eq!(version, 1);
        for table in [
            "meta",
            "records",
            "heads",
            "evidence",
            "relations",
            "retractions",
            "knowledge_fts",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "schema v1 must contain {table}");
        }
        let name: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'repo_name'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(name, "fresh");
        conn.close().unwrap();
        // The M6 code index is never touched by knowledge operations.
        assert!(!home.join("index").join("fresh.db").exists());
    }

    #[test]
    fn knowledge_store_open_corrupt_file_fails_closed() {
        let (home, repo) = setup("corrupt");
        let db = knowledge_db_path(&home, "corrupt");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"not-a-database").unwrap();
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(
            err.contains("readable") || err.contains("corrupt"),
            "unexpected: {err}"
        );
        assert_eq!(std::fs::read(&db).unwrap(), b"not-a-database");
    }

    #[test]
    fn knowledge_store_open_forward_version_fails() {
        let (home, repo) = setup("fwd");
        open_knowledge_at(&home, &repo, true)
            .unwrap()
            .close()
            .unwrap();
        let raw = rusqlite::Connection::open(knowledge_db_path(&home, "fwd")).unwrap();
        raw.execute_batch("PRAGMA user_version = 99").unwrap();
        raw.close().unwrap();
        let err = open_knowledge_at(&home, &repo, false).unwrap_err();
        assert!(err.contains("forward"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_store_open_meta_mismatch_fails() {
        for (key, bad) in [("repo_name", "other"), ("knowledge_schema_version", "2")] {
            let (home, repo) = setup("mismatch");
            open_knowledge_at(&home, &repo, true)
                .unwrap()
                .close()
                .unwrap();
            let raw = rusqlite::Connection::open(knowledge_db_path(&home, "mismatch")).unwrap();
            raw.execute("UPDATE meta SET value = ?1 WHERE key = ?2", [bad, key])
                .unwrap();
            raw.close().unwrap();
            let err = open_knowledge_at(&home, &repo, false).unwrap_err();
            assert!(err.contains("mismatch"), "key {key}: unexpected: {err}");
        }
        // Wrong canonical root: point the entry at another existing dir.
        let (home, repo) = setup("mismatch-root");
        open_knowledge_at(&home, &repo, true)
            .unwrap()
            .close()
            .unwrap();
        let other_root = tmpdir("m7-other-root");
        let moved = RepoEntry {
            name: repo.name.clone(),
            root: other_root.to_string_lossy().into_owned(),
            added_unix: 1,
        };
        let err = open_knowledge_at(&home, &moved, false).unwrap_err();
        assert!(err.contains("mismatch"), "root: unexpected: {err}");
    }

    #[test]
    fn knowledge_store_open_fresh_create_leaves_lifecycle_tables_absent() {
        let (home, repo) = setup("fresh-nolife");
        let conn = open_knowledge_at(&home, &repo, true).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('record_lifecycle','relation_lifecycle')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 0,
            "lifecycle tables are created only by the v1→v2 migration"
        );
    }

    #[test]
    fn knowledge_schema_migration_stamps_v2_and_creates_lifecycle_tables() {
        let (home, repo) = setup("migrate");
        let mut conn = open_knowledge_at(&home, &repo, true).unwrap();
        create_record(&mut conn, &new_record("d1", "migrate me")).unwrap();
        let before_gen = read_generation(&conn).unwrap();
        let before_rows: Vec<(i64, String, String)> = {
            let mut stmt = conn
                .prepare("SELECT revision, title, status FROM records ORDER BY id, revision")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        migrate_to_v2_for_test(&mut conn, false).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let marker: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'knowledge_schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker, "2");
        for table in ["record_lifecycle", "relation_lifecycle"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "migration must create {table}");
        }
        assert_eq!(
            read_generation(&conn).unwrap(),
            before_gen,
            "migration itself must not bump the generation"
        );
        let after_rows: Vec<(i64, String, String)> = {
            let mut stmt = conn
                .prepare("SELECT revision, title, status FROM records ORDER BY id, revision")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert_eq!(before_rows, after_rows, "existing rows must be untouched");
        conn.close().unwrap();
        // The migrated store reopens through the guarded open path.
        let mut conn = open_knowledge_at(&home, &repo, true).unwrap();
        // A second migration attempt is a no-op on an already-migrated store.
        migrate_to_v2_for_test(&mut conn, false).unwrap();
        let marker: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'knowledge_schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker, "2");
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_schema_v3_migration_from_v1_preserves_existing_records() {
        let (home, repo) = setup("migrate-v3-v1");
        let mut conn = open_knowledge_at(&home, &repo, true).unwrap();
        create_record(&mut conn, &new_record("d1", "preserve through v3")).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let before_generation = read_generation(&conn).unwrap();
        let before_record = show_record(&conn, "d1", Some(1)).unwrap();

        // A pre-v2 store can hold ordinary revisions without lifecycle tables.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        ensure_code_binding_schema(&tx).unwrap();
        tx.commit().unwrap();

        let check = verify_store(&conn, &repo, Path::new(&repo.root)).unwrap();
        assert_eq!(check.schema_version, 3);
        assert_eq!(check.code_bindings, 0);
        assert_eq!(read_generation(&conn).unwrap(), before_generation);
        let after_record = show_record(&conn, "d1", Some(1)).unwrap();
        assert_eq!(after_record.title, before_record.title);
        assert_eq!(after_record.body, before_record.body);
        assert_eq!(after_record.revision, before_record.revision);
    }

    #[test]
    fn knowledge_schema_migration_failure_rolls_back_to_v1() {
        let (home, repo) = setup("migrate-fail");
        let mut conn = open_knowledge_at(&home, &repo, true).unwrap();
        create_record(&mut conn, &new_record("d1", "stay v1")).unwrap();
        let gen = read_generation(&conn).unwrap();
        let err = migrate_to_v2_for_test(&mut conn, true).unwrap_err();
        assert!(err.contains("injected"), "unexpected: {err}");
        // The failed migration left no trace: still v1, no tables, no marker
        // change, no generation bump.
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('record_lifecycle','relation_lifecycle')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            tables, 0,
            "no lifecycle table may survive a failed migration"
        );
        let marker: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'knowledge_schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker, "1");
        assert_eq!(read_generation(&conn).unwrap(), gen);
        conn.close().unwrap();
        // The store still opens as v1 and serves Foundation reads.
        let conn = open_knowledge_at(&home, &repo, false).unwrap();
        let record = show_record(&conn, "d1", None).unwrap();
        assert_eq!(record.status, "active");
    }

    #[test]
    fn knowledge_store_open_corrupt_v2_missing_lifecycle_tables_rejected() {
        let (home, repo) = setup("v2-corrupt");
        open_knowledge_at(&home, &repo, true)
            .unwrap()
            .close()
            .unwrap();
        let raw = rusqlite::Connection::open(knowledge_db_path(&home, "v2-corrupt")).unwrap();
        raw.execute_batch("PRAGMA user_version = 2").unwrap();
        raw.execute(
            "UPDATE meta SET value = '2' WHERE key = 'knowledge_schema_version'",
            [],
        )
        .unwrap();
        raw.close().unwrap();
        let err = open_knowledge_at(&home, &repo, false).unwrap_err();
        assert!(err.contains("record_lifecycle"), "unexpected: {err}");
        // Fail closed: the store is left in place, never deleted or rebuilt.
        assert!(knowledge_db_path(&home, "v2-corrupt").is_file());
    }

    #[test]
    fn knowledge_store_open_v2_meta_disagreement_rejected() {
        let (home, repo) = setup("v2-mismatch");
        let mut conn = open_knowledge_at(&home, &repo, true).unwrap();
        migrate_to_v2_for_test(&mut conn, false).unwrap();
        conn.close().unwrap();
        let raw = rusqlite::Connection::open(knowledge_db_path(&home, "v2-mismatch")).unwrap();
        raw.execute(
            "UPDATE meta SET value = '1' WHERE key = 'knowledge_schema_version'",
            [],
        )
        .unwrap();
        raw.close().unwrap();
        let err = open_knowledge_at(&home, &repo, false).unwrap_err();
        assert!(err.contains("mismatch"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_store_open_v1_reads_never_migrate() {
        let (home, repo) = setup("v1-stays");
        let mut conn = open_knowledge_at(&home, &repo, true).unwrap();
        create_record(&mut conn, &new_record("d1", "zqxv1 stays v1")).unwrap();
        conn.close().unwrap();
        let conn = open_knowledge_at(&home, &repo, false).unwrap();
        let _ = search(&conn, "zqxv1", None, None, false, false, false, 10, 0, None).unwrap();
        let _ = history(&conn, "d1", 10, 0, None).unwrap();
        let _ = show_record(&conn, "d1", None).unwrap();
        conn.close().unwrap();
        let conn = open_knowledge_at(&home, &repo, false).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "reads must never migrate a v1 store");
    }

    #[test]
    fn knowledge_store_open_fts_follows_head() {
        let (home, repo) = setup("ftshead");
        let conn = open_knowledge_at(&home, &repo, true).unwrap();
        for (rev, title) in [
            (1, "zqxalpha first decision"),
            (2, "zqxbeta revised decision"),
        ] {
            conn.execute(
                "INSERT INTO records (id, revision, kind, title, body, status, author, created_unix)
                 VALUES ('r1', ?1, 'decision', ?2, 'body', 'active', 't', 1)",
                rusqlite::params![rev, title],
            )
            .unwrap();
        }
        // No head yet: inserting revisions alone must not index anything.
        let unheaded: i64 = conn
            .query_row("SELECT count(*) FROM knowledge_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unheaded, 0, "revisions without a head must not be indexed");
        // Head at rev1: FTS is rev1 only, even though rev2 exists.
        conn.execute(
            "INSERT INTO heads (id, current_revision) VALUES ('r1', 1)",
            [],
        )
        .unwrap();
        let stale: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxalpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 1, "head revision must be indexed");
        let future: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxbeta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(future, 0, "non-head revision must not be indexed");
        // Moving the head moves the index.
        conn.execute("UPDATE heads SET current_revision = 2 WHERE id = 'r1'", [])
            .unwrap();
        let stale_after: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxalpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale_after, 0, "old head must leave the index");
        let head_after: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxbeta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(head_after, 1, "new head must be indexed");
        // Immutable history is preserved regardless of the index.
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM records WHERE id = 'r1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2, "historical revisions are never removed");
        // Rolling back a head move rolls back the index with it.
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.execute("UPDATE heads SET current_revision = 1 WHERE id = 'r1'", [])
            .unwrap();
        let in_txn: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxalpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_txn, 1, "index follows the head inside the transaction");
        conn.execute_batch("ROLLBACK").unwrap();
        let head_rev: i64 = conn
            .query_row(
                "SELECT current_revision FROM heads WHERE id = 'r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(head_rev, 2);
        let rolled_back: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxbeta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            rolled_back, 1,
            "rolled-back head move must not leak index changes"
        );
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_store_open_init_failure_rolls_back() {
        let (home, repo) = setup("initfault");
        let err = open_knowledge_at_inject_init_failure(&home, &repo).unwrap_err();
        assert!(err.contains("injected"), "unexpected: {err}");
        // No partial schema or metadata survived the failed initialization.
        let raw = rusqlite::Connection::open(knowledge_db_path(&home, "initfault")).unwrap();
        let objects: i64 = raw
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type IN ('table', 'trigger')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(objects, 0, "failed init must leave no tables or triggers");
        let version: u32 = raw
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 0, "failed init must not stamp a version");
        raw.close().unwrap();
        // The leftover file is not repaired or deleted automatically: the next
        // open fails explicitly.
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(err.contains("corrupt"), "unexpected: {err}");
        assert!(knowledge_db_path(&home, "initfault").exists());
    }

    #[test]
    fn knowledge_store_open_incomplete_schema_rejected() {
        for drop_sql in ["DROP TABLE heads", "DROP TABLE knowledge_fts"] {
            let (home, repo) = setup("incomplete");
            open_knowledge_at(&home, &repo, true)
                .unwrap()
                .close()
                .unwrap();
            let raw = rusqlite::Connection::open(knowledge_db_path(&home, "incomplete")).unwrap();
            raw.execute_batch(drop_sql).unwrap();
            raw.close().unwrap();
            let err = open_knowledge_at(&home, &repo, false).unwrap_err();
            assert!(err.contains("incomplete"), "{drop_sql}: unexpected: {err}");
        }
    }

    #[test]
    fn knowledge_store_open_moved_root_mismatch() {
        let (home, repo) = setup("movedroot");
        open_knowledge_at(&home, &repo, true)
            .unwrap()
            .close()
            .unwrap();
        std::fs::remove_dir_all(Path::new(&repo.root)).unwrap();
        let err = open_knowledge_at(&home, &repo, false).unwrap_err();
        assert!(
            err.contains("resolve") || err.contains("mismatch"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn knowledge_store_open_fresh_create_refuses_retarget() {
        // The recorded path is rebound to another tree before the first
        // knowledge store is created: the live canonical root no longer
        // matches the recorded string, so creation must fail. Rebinding the
        // path itself (rather than swapping the entry string) detects the
        // retarget on every platform, including filesystems where a raw path
        // already equals its canonical form.
        let (home, repo) = setup("retarget");
        let other = tmpdir("m7-retarget-target");
        if !repoint_dir_to(Path::new(&repo.root), &other) {
            eprintln!("NOT TESTED: cannot rebind directories in this environment");
            return;
        }
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(err.contains("retarget"), "unexpected: {err}");
        assert!(
            !knowledge_db_path(&home, "retarget").exists(),
            "refused creation must leave no store file behind"
        );
    }

    #[test]
    fn knowledge_store_open_fresh_create_compatible_root() {
        // The normal compatible path still works: the recorded canonical
        // root creates fine, as does the no-index case (other tests).
        let (home, repo) = setup("retarget-ok");
        open_knowledge_at(&home, &repo, true)
            .unwrap()
            .close()
            .unwrap();
    }

    /// Replace the directory at `link` with a link to `target`, so the
    /// recorded path resolves to another tree. Returns false when the
    /// environment cannot create the link (caller reports NOT TESTED).
    fn repoint_dir_to(link: &Path, target: &Path) -> bool {
        if std::fs::remove_dir_all(link).is_err() {
            return false;
        }
        #[cfg(unix)]
        {
            if std::os::unix::fs::symlink(target, link).is_err() {
                return false;
            }
        }
        #[cfg(windows)]
        {
            if plant_link(link, target).is_none() {
                return false;
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (link, target);
            return false;
        }
        // The link primitive reported success, but some environments leave a
        // reparse point the OS itself cannot resolve (observed as Access
        // denied on canonicalize). Only a resolvable link exercises the
        // retarget path; otherwise report NOT TESTED instead of failing an
        // environment limitation as a code defect.
        if link.canonicalize().is_err() {
            let _ = std::fs::remove_file(link);
            let _ = std::fs::remove_dir(link);
            return false;
        }
        true
    }

    #[test]
    fn knowledge_store_open_db_path_is_directory() {
        let (home, repo) = setup("isdir");
        let db = knowledge_db_path(&home, "isdir");
        std::fs::create_dir_all(&db).unwrap();
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(err.contains("not a file"), "unexpected: {err}");
    }

    #[test]
    fn knowledge_store_open_unusable_home_fails() {
        let (home, repo) = setup("badhome");
        let file = home.join("file-home");
        std::fs::write(&file, b"x").unwrap();
        let err = open_knowledge_at(&file, &repo, true).unwrap_err();
        assert!(
            err.contains("cannot create") || err.contains("cannot resolve"),
            "unexpected: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn knowledge_store_open_non_utf8_home_supported() {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = std::env::temp_dir().into_os_string().into_vec();
        bytes.extend_from_slice(format!("/riltra-m7-nonutf-{}-", std::process::id()).as_bytes());
        bytes.extend_from_slice(&[0x66u8, 0x6f, 0x80, 0x6f]);
        let home = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        let _ = std::fs::remove_dir_all(&home);
        // Some filesystems (e.g. APFS) cannot represent non-UTF-8 names at
        // all: probe creation first and report NOT TESTED instead of failing
        // an environment limitation as a code defect.
        if std::fs::create_dir_all(&home).is_err() {
            eprintln!("NOT TESTED: filesystem rejects non-UTF-8 names");
            return;
        }
        let root = tmpdir("m7-root");
        let repo = RepoEntry {
            name: "nonutf".to_string(),
            root: crate::paths::canonical_root(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            added_unix: 1,
        };
        let conn = open_knowledge_at(&home, &repo, true).unwrap();
        conn.close().unwrap();
        assert!(knowledge_db_path(&home, "nonutf").is_file());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn knowledge_store_open_hostile_db_symlink_rejected() {
        let (home, repo) = setup("dblink");
        let db = knowledge_db_path(&home, "dblink");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let outside = tmpdir("m7-outside");
        let target = outside.join("evil.db");
        std::fs::write(&target, b"SENTINEL").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &db).unwrap();
        #[cfg(windows)]
        match plant_link(&db, &target) {
            Some(mech) => eprintln!("hostile db link planted via {mech}"),
            None => {
                eprintln!("NOT TESTED: cannot plant links in this environment");
                return;
            }
        }
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(
            err.contains("link") || err.contains("escape"),
            "unexpected: {err}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"SENTINEL");
    }

    #[test]
    fn knowledge_store_open_hostile_companion_symlink_rejected() {
        let (home, repo) = setup("complink");
        open_knowledge_at(&home, &repo, true)
            .unwrap()
            .close()
            .unwrap();
        let db = knowledge_db_path(&home, "complink");
        let wal = companion(&db, "-wal");
        let _ = std::fs::remove_file(&wal);
        let outside = tmpdir("m7-outside-wal");
        let target = outside.join("evil-wal");
        std::fs::write(&target, b"SENTINEL").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &wal).unwrap();
        #[cfg(windows)]
        match plant_link(&wal, &target) {
            Some(mech) => eprintln!("hostile companion link planted via {mech}"),
            None => {
                eprintln!("NOT TESTED: cannot plant links in this environment");
                return;
            }
        }
        let err = open_knowledge_at(&home, &repo, false).unwrap_err();
        assert!(
            err.contains("link") || err.contains("escape"),
            "unexpected: {err}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"SENTINEL");
    }

    #[test]
    fn knowledge_store_open_dangling_companion_symlink_rejected() {
        let (home, repo) = setup("dangling");
        open_knowledge_at(&home, &repo, true)
            .unwrap()
            .close()
            .unwrap();
        let db = knowledge_db_path(&home, "dangling");
        let shm = companion(&db, "-shm");
        let _ = std::fs::remove_file(&shm);
        let ghost = tmpdir("m7-ghost").join("nope-shm");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ghost, &shm).unwrap();
        #[cfg(windows)]
        match plant_link(&shm, &ghost) {
            Some(mech) => eprintln!("dangling companion link planted via {mech}"),
            None => {
                eprintln!("NOT TESTED: cannot plant links in this environment");
                return;
            }
        }
        let err = open_knowledge_at(&home, &repo, false).unwrap_err();
        assert!(
            err.contains("link") || err.contains("escape") || err.contains("dangling"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn knowledge_store_open_post_open_recheck_rejects_link() {
        // The DB path itself is a link: the post-open canonical re-check is
        // the normative guard because SQLite follows links on open.
        let (home, repo) = setup("recheck");
        let db = knowledge_db_path(&home, "recheck");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let outside = tmpdir("m7-outside-recheck");
        let target = outside.join("real.db");
        std::fs::write(&target, b"SENTINEL").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &db).unwrap();
        #[cfg(windows)]
        match plant_link(&db, &target) {
            Some(mech) => eprintln!("recheck link planted via {mech}"),
            None => {
                eprintln!("NOT TESTED: cannot plant links in this environment");
                return;
            }
        }
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(
            err.contains("link") || err.contains("escape"),
            "unexpected: {err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn knowledge_store_open_windows_junction_knowledge_dir_rejected() {
        let (home, repo) = setup("junction");
        let kdir = home.join("knowledge");
        std::fs::create_dir_all(&home).unwrap();
        let outside = tmpdir("m7-outside-junction");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                kdir.to_str().unwrap(),
                outside.to_str().unwrap(),
            ])
            .output();
        let created = status.map(|o| o.status.success()).unwrap_or(false) && is_junction(&kdir);
        if !created {
            eprintln!("NOT TESTED: cannot create junctions in this environment");
            return;
        }
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(
            err.contains("link") || err.contains("escape") || err.contains("retarget"),
            "unexpected: {err}"
        );
    }

    #[cfg(windows)]
    fn is_junction(p: &Path) -> bool {
        std::fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
            || p.canonicalize().map(|c| c != *p).unwrap_or(false)
    }

    /// Plant a hostile link for rejection tests. Returns the mechanism used,
    /// or `None` when the environment allows neither (caller reports
    /// NOT TESTED). The junction fallback names a directory junction like a
    /// file; rejection runs before any type check, so it exercises the same
    /// pre/post-open scan path as a file symlink.
    #[cfg(windows)]
    fn plant_link(link: &Path, target: &Path) -> Option<&'static str> {
        if std::os::windows::fs::symlink_file(target, link).is_ok() {
            return Some("symlink");
        }
        let anchor = if target.is_dir() {
            target.to_path_buf()
        } else {
            match target.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => return None,
            }
        };
        // mklink cannot consume verbatim (`\\?\`) paths: it reports success
        // yet leaves a junction Windows cannot resolve (later surfacing as
        // Access denied). Strip the prefix for the link tool only.
        let ok = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &for_mklink(link),
                &for_mklink(&anchor),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok && std::fs::symlink_metadata(link).is_ok() {
            Some("junction-fallback")
        } else {
            None
        }
    }

    #[cfg(windows)]
    fn for_mklink(path: &Path) -> String {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
        text.into_owned()
    }

    #[cfg(not(windows))]
    #[test]
    fn knowledge_store_open_unix_dir_symlink_rejected() {
        let (home, repo) = setup("dirlink");
        let kdir = home.join("knowledge");
        let outside = tmpdir("m7-outside-dir");
        std::os::unix::fs::symlink(&outside, &kdir).unwrap();
        let err = open_knowledge_at(&home, &repo, true).unwrap_err();
        assert!(
            err.contains("link") || err.contains("escape") || err.contains("retarget"),
            "unexpected: {err}"
        );
    }

    /// Scripted outcomes for the `reject_link_with` seam: replaces only the
    /// sequence of raw metadata results, never the classification rules.
    enum ScriptedInspect {
        Denied,
        Invalid,
        Delegate,
    }

    fn scripted_inspector(
        script: Vec<ScriptedInspect>,
    ) -> (
        impl Fn(&Path) -> std::io::Result<std::fs::Metadata>,
        std::rc::Rc<std::cell::Cell<u32>>,
    ) {
        let queue = std::cell::RefCell::new(script.into_iter());
        let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let counted = std::rc::Rc::clone(&calls);
        let inspect = move |p: &Path| {
            counted.set(counted.get() + 1);
            match queue.borrow_mut().next() {
                Some(ScriptedInspect::Denied) => Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "scripted permission denied",
                )),
                Some(ScriptedInspect::Invalid) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "scripted other error",
                )),
                Some(ScriptedInspect::Delegate) | None => std::fs::symlink_metadata(p),
            }
        };
        (inspect, calls)
    }

    #[test]
    fn reject_link_immediate_success_inspects_once() {
        let dir = tmpdir("m8-retry-ok");
        let file = dir.join("krepo.db");
        std::fs::write(&file, b"db").unwrap();
        let (inspect, calls) = scripted_inspector(vec![ScriptedInspect::Delegate]);
        reject_link_with(&file, inspect).unwrap();
        assert_eq!(calls.get(), 1, "immediate success must not re-inspect");
    }

    #[test]
    fn reject_link_other_errors_fail_closed_without_retry() {
        let dir = tmpdir("m8-retry-other");
        let file = dir.join("krepo.db");
        std::fs::write(&file, b"db").unwrap();
        let (inspect, calls) = scripted_inspector(vec![ScriptedInspect::Invalid]);
        let err = reject_link_with(&file, inspect).unwrap_err();
        assert!(
            err.starts_with("cannot inspect knowledge store path"),
            "unexpected: {err}"
        );
        assert!(err.contains("scripted other error"), "unexpected: {err}");
        assert_eq!(calls.get(), 1, "non-permission errors must not retry");
    }

    #[cfg(windows)]
    #[test]
    fn reject_link_retries_transient_permission_denied() {
        let dir = tmpdir("m8-retry-transient");
        let file = dir.join("krepo.db");
        std::fs::write(&file, b"db").unwrap();
        let (inspect, calls) =
            scripted_inspector(vec![ScriptedInspect::Denied, ScriptedInspect::Delegate]);
        reject_link_with(&file, inspect).unwrap();
        assert_eq!(calls.get(), 2, "transient denial must be re-inspected");
    }

    #[cfg(windows)]
    #[test]
    fn reject_link_retry_budget_is_three_inspections() {
        let dir = tmpdir("m8-retry-budget");
        let file = dir.join("krepo.db");
        std::fs::write(&file, b"db").unwrap();
        let (inspect, calls) = scripted_inspector(vec![
            ScriptedInspect::Denied,
            ScriptedInspect::Denied,
            ScriptedInspect::Delegate,
        ]);
        reject_link_with(&file, inspect).unwrap();
        assert_eq!(calls.get(), INSPECT_ATTEMPTS);
    }

    #[cfg(windows)]
    #[test]
    fn reject_link_transient_denied_then_missing_path_is_absent() {
        let dir = tmpdir("m8-retry-missing");
        let missing = dir.join("krepo.db-wal");
        let (inspect, calls) =
            scripted_inspector(vec![ScriptedInspect::Denied, ScriptedInspect::Delegate]);
        reject_link_with(&missing, inspect).unwrap();
        assert_eq!(calls.get(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn reject_link_persistent_denial_fails_closed_after_budget() {
        let dir = tmpdir("m8-retry-persistent");
        let file = dir.join("krepo.db");
        std::fs::write(&file, b"db").unwrap();
        let (inspect, calls) = scripted_inspector(vec![
            ScriptedInspect::Denied,
            ScriptedInspect::Denied,
            ScriptedInspect::Denied,
            ScriptedInspect::Delegate, // must never be reached
        ]);
        let err = reject_link_with(&file, inspect).unwrap_err();
        assert!(
            err.starts_with("cannot inspect knowledge store path"),
            "unexpected: {err}"
        );
        assert!(
            err.contains("scripted permission denied"),
            "unexpected: {err}"
        );
        assert_eq!(calls.get(), INSPECT_ATTEMPTS, "budget must be exact");
    }

    #[cfg(windows)]
    #[test]
    fn reject_link_transient_denied_then_link_still_rejected() {
        let dir = tmpdir("m8-retry-link");
        let target = tmpdir("m8-retry-link-target").join("evil.db");
        std::fs::write(&target, b"SENTINEL").unwrap();
        let link = dir.join("krepo.db");
        match plant_link(&link, &target) {
            Some(mech) => eprintln!("hostile link planted via {mech}"),
            None => {
                eprintln!("NOT TESTED: cannot plant links in this environment");
                return;
            }
        }
        let (inspect, calls) =
            scripted_inspector(vec![ScriptedInspect::Denied, ScriptedInspect::Delegate]);
        let err = reject_link_with(&link, inspect).unwrap_err();
        assert!(
            err.starts_with("knowledge store path escapes via link"),
            "unexpected: {err}"
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read(&target).unwrap(), b"SENTINEL");
    }

    #[cfg(not(windows))]
    #[test]
    fn reject_link_permission_denied_not_retried_off_windows() {
        let dir = tmpdir("m8-retry-unix");
        let file = dir.join("krepo.db");
        std::fs::write(&file, b"db").unwrap();
        let (inspect, calls) =
            scripted_inspector(vec![ScriptedInspect::Denied, ScriptedInspect::Delegate]);
        let err = reject_link_with(&file, inspect).unwrap_err();
        assert!(
            err.starts_with("cannot inspect knowledge store path"),
            "unexpected: {err}"
        );
        assert_eq!(calls.get(), 1, "unix behavior must stay single-inspection");
    }

    use crate::knowledge::{
        validate_create_json, validate_update_json, NewRecord, SupersededRef, UpdateRecord,
    };

    fn new_record(id: &str, title: &str) -> NewRecord {
        NewRecord {
            id: id.to_string(),
            kind: "decision".to_string(),
            title: title.to_string(),
            body: format!("body of {title}"),
            status: "active".to_string(),
            author: "tester".to_string(),
        }
    }

    fn update_to(title: &str, status: &str) -> UpdateRecord {
        UpdateRecord {
            title: title.to_string(),
            body: format!("body of {title}"),
            status: status.to_string(),
            author: "tester".to_string(),
            superseded_by: None,
            reason: None,
            confirm: false,
        }
    }

    /// Reconfirm the exact revision written for `title` (same title/body/
    /// status/author as `new_record`/`update_to`) with an explicit confirm.
    fn confirm_to(title: &str) -> UpdateRecord {
        UpdateRecord {
            confirm: true,
            ..update_to(title, "active")
        }
    }

    fn supersede_to(target: &str, revision: u64, reason: &str) -> UpdateRecord {
        UpdateRecord {
            title: "superseded title".to_string(),
            body: "superseded body".to_string(),
            status: "superseded".to_string(),
            author: "tester".to_string(),
            superseded_by: Some(crate::knowledge::SupersededRef {
                id: target.to_string(),
                revision,
            }),
            reason: Some(reason.to_string()),
            confirm: false,
        }
    }

    fn retract_to(reason: &str) -> UpdateRecord {
        UpdateRecord {
            title: "retracted title".to_string(),
            body: "retracted body".to_string(),
            status: "retracted".to_string(),
            author: "tester".to_string(),
            superseded_by: None,
            reason: Some(reason.to_string()),
            confirm: false,
        }
    }

    fn open_rw(home: &Path, repo: &RepoEntry) -> rusqlite::Connection {
        open_knowledge_at(home, repo, true).unwrap()
    }

    #[test]
    fn knowledge_records_create_show_update_history() {
        let (home, repo) = setup("revs");
        let mut conn = open_rw(&home, &repo);
        let (rev, gen) = create_record(&mut conn, &new_record("d1", "first zqxtitle")).unwrap();
        assert_eq!((rev, gen), (1, 1));
        let shown = show_record(&conn, "d1", None).unwrap();
        assert_eq!(shown.revision, 1);
        assert_eq!(shown.title, "first zqxtitle");
        let (rev2, gen2) =
            update_record(&mut conn, "d1", 1, &update_to("second zqxtitle", "active")).unwrap();
        assert_eq!((rev2, gen2), (2, 2));
        let old = show_record(&conn, "d1", Some(1)).unwrap();
        assert_eq!(old.title, "first zqxtitle");
        let page = history(&conn, "d1", 50, 0, None).unwrap();
        assert_eq!(page.total, 2);
        assert!(!page.truncated);
        assert_eq!(page.generation, 2);
        assert_eq!(
            page.revisions
                .iter()
                .map(|r| r.revision)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_stale_expected_revision_conflict() {
        let (home, repo) = setup("stale");
        let mut conn = open_rw(&home, &repo);
        update_record(&mut conn, "missing-never", 1, &update_to("x", "active")).unwrap_err();
        create_record(&mut conn, &new_record("d1", "stable zqxone")).unwrap();
        update_record(&mut conn, "d1", 1, &update_to("moved zqxtwo", "active")).unwrap();
        let gen_before = read_generation(&conn).unwrap();
        let err =
            update_record(&mut conn, "d1", 1, &update_to("loser zqxthree", "active")).unwrap_err();
        assert!(err.contains("conflict"), "unexpected: {err}");
        // No write and no generation bump happened.
        assert_eq!(show_record(&conn, "d1", None).unwrap().revision, 2);
        assert_eq!(read_generation(&conn).unwrap(), gen_before);
        let page = history(&conn, "d1", 50, 0, None).unwrap();
        assert_eq!(page.total, 2);
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxthree'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0, "failed update must not leak into the index");
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_duplicate_id_fails() {
        let (home, repo) = setup("dup");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        let err = create_record(&mut conn, &new_record("d1", "t2")).unwrap_err();
        assert!(err.contains("duplicate"), "unexpected: {err}");
        // Same id with a different kind is still a duplicate, not a rename.
        let mut other_kind = new_record("d1", "t3");
        other_kind.kind = "constraint".to_string();
        let err = create_record(&mut conn, &other_kind).unwrap_err();
        assert!(err.contains("duplicate"), "unexpected: {err}");
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 1);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_id_and_kind_immutable() {
        let (home, repo) = setup("immutable");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        update_record(&mut conn, "d1", 1, &update_to("t2", "active")).unwrap();
        let shown = show_record(&conn, "d1", None).unwrap();
        assert_eq!(shown.id, "d1");
        assert_eq!(shown.kind, "decision");
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_invalid_kind_and_status_rejected() {
        let (home, repo) = setup("invalid");
        let mut conn = open_rw(&home, &repo);
        let mut bad_kind = new_record("d1", "t");
        bad_kind.kind = "note".to_string();
        assert!(create_record(&mut conn, &bad_kind).is_err());
        for status in ["superseded", "retracted", "bogus"] {
            let mut bad_status = new_record("d1", "t");
            bad_status.status = status.to_string();
            let err = create_record(&mut conn, &bad_status).unwrap_err();
            assert!(err.contains("status"), "{status}: unexpected: {err}");
        }
        // Nothing was written by the rejected inputs.
        assert!(show_record(&conn, "d1", None).is_err());
        assert_eq!(read_generation(&conn).unwrap(), 0);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_archive_restore_new_revisions() {
        let (home, repo) = setup("lifecycle");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        let (rev2, _) = update_record(&mut conn, "d1", 1, &update_to("t1", "archived")).unwrap();
        assert_eq!(rev2, 2);
        assert_eq!(show_record(&conn, "d1", None).unwrap().status, "archived");
        let (rev3, _) = update_record(&mut conn, "d1", 2, &update_to("t1", "active")).unwrap();
        assert_eq!(rev3, 3);
        assert_eq!(show_record(&conn, "d1", None).unwrap().status, "active");
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 3);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_concurrent_writers_one_winner() {
        let (home, repo) = setup("race");
        let mut first = open_rw(&home, &repo);
        create_record(&mut first, &new_record("d1", "base zqxbase")).unwrap();
        let gen_before = read_generation(&first).unwrap();
        drop(first);
        // Two read-write connections observe the same head revision.
        let mut a = open_knowledge_at(&home, &repo, true).unwrap();
        let mut b = open_knowledge_at(&home, &repo, true).unwrap();
        assert_eq!(show_record(&a, "d1", None).unwrap().revision, 1);
        assert_eq!(show_record(&b, "d1", None).unwrap().revision, 1);
        let ra = update_record(&mut a, "d1", 1, &update_to("winner zqxwin", "active"));
        let rb = update_record(&mut b, "d1", 1, &update_to("loser zqxlose", "active"));
        // Exactly one writer wins; the loser reports a conflict.
        match (ra, rb) {
            (Ok(_), Err(e)) | (Err(e), Ok(_)) => {
                assert!(e.contains("conflict"), "loser must conflict: {e}")
            }
            (Ok(_), Ok(_)) => panic!("both writers won: lost update"),
            (Err(a), Err(b)) => panic!("both writers lost: {a} vs {b}"),
        }
        drop(a);
        drop(b);
        let check = open_knowledge_at(&home, &repo, false).unwrap();
        let head = show_record(&check, "d1", None).unwrap();
        assert_eq!(
            head.revision, 2,
            "no lost update: head advanced exactly once"
        );
        assert_eq!(history(&check, "d1", 50, 0, None).unwrap().total, 2);
        assert_eq!(
            read_generation(&check).unwrap(),
            gen_before + 1,
            "loser caused no generation bump"
        );
        check.close().unwrap();
    }

    #[test]
    fn knowledge_records_history_generation_guard_and_pagination() {
        let (home, repo) = setup("paging");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        update_record(&mut conn, "d1", 1, &update_to("t2", "active")).unwrap();
        update_record(&mut conn, "d1", 2, &update_to("t3", "active")).unwrap();
        let gen = read_generation(&conn).unwrap();
        let err = history(&conn, "d1", 50, 0, Some(gen + 1)).unwrap_err();
        assert!(err.contains("generation"), "unexpected: {err}");
        let p1 = history(&conn, "d1", 2, 0, Some(gen)).unwrap();
        assert_eq!(p1.total, 3);
        assert!(p1.truncated);
        assert_eq!(
            p1.revisions.iter().map(|r| r.revision).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let p2 = history(&conn, "d1", 2, 2, Some(gen)).unwrap();
        assert!(!p2.truncated);
        assert_eq!(
            p2.revisions.iter().map(|r| r.revision).collect::<Vec<_>>(),
            vec![3]
        );
        let empty = history(&conn, "d1", 2, 3, Some(gen)).unwrap();
        assert_eq!(empty.total, 3);
        assert!(empty.revisions.is_empty());
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_missing_id_errors() {
        let (home, repo) = setup("noid");
        let mut conn = open_rw(&home, &repo);
        assert!(show_record(&conn, "nope", None).is_err());
        assert!(show_record(&conn, "nope", Some(1)).is_err());
        assert!(history(&conn, "nope", 50, 0, None).is_err());
        assert!(update_record(&mut conn, "nope", 1, &update_to("t", "active")).is_err());
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_write_revalidates_companions() {
        let (home, repo) = setup("revalidate");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        let gen_before = read_generation(&conn).unwrap();
        conn.close().unwrap();
        let db = knowledge_db_path(&home, "revalidate");
        // Open the writer on clean paths, then clear any companions SQLite
        // maintains itself: companion lifecycle across open is
        // platform-dependent, so planting must start from a verified-absent
        // state with no SQLite call between clearing and planting.
        // The -journal path is used because SQLite never holds it open while
        // this connection is idle (WAL mode never uses it; a rollback write
        // never starts since revalidation runs first).
        let mut conn = open_knowledge_at(&home, &repo, true).unwrap();
        for suffix in ["-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(companion(&db, suffix));
        }
        let journal = companion(&db, "-journal");
        assert!(
            !journal.exists(),
            "companion must be absent before planting"
        );
        let outside = tmpdir("m7-outside-reval");
        let target = outside.join("evil-wal");
        std::fs::write(&target, b"SENTINEL").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &journal).unwrap();
        #[cfg(windows)]
        match plant_link(&journal, &target) {
            Some(mech) => eprintln!("revalidation link planted via {mech}"),
            None => {
                eprintln!("NOT TESTED: cannot plant links in this environment");
                return;
            }
        }
        let err = update_record(&mut conn, "d1", 1, &update_to("t2", "active")).unwrap_err();
        assert!(
            err.contains("link") || err.contains("escape"),
            "unexpected: {err}"
        );
        // Drop the poisoned connection without insisting on a clean close,
        // remove the planted link, and verify nothing changed through a fresh
        // connection: reads through the hostile companion are not trusted.
        // Junctions plant as directories on Windows, so removal falls back
        // from file to directory removal.
        let _ = conn.close();
        std::fs::remove_file(&journal)
            .or_else(|_| std::fs::remove_dir(&journal))
            .unwrap();
        let check = open_knowledge_at(&home, &repo, false).unwrap();
        assert_eq!(show_record(&check, "d1", None).unwrap().revision, 1);
        assert_eq!(read_generation(&check).unwrap(), gen_before);
        let hits: i64 = check
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 't2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0, "refused update must not leak into the index");
        check.close().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"SENTINEL");
    }

    #[test]
    fn knowledge_history_reads_single_snapshot() {
        use std::sync::{Arc, Barrier};
        let (home, repo) = setup("snapshot");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "snap zqxone")).unwrap();
        conn.close().unwrap();
        // Writer commits exactly on the boundary between the reader's
        // generation validation and its row reads. No sleeps: barriers order
        // the two threads deterministically.
        // Both connections are opened sequentially up front: the barrier
        // orders the commit boundary deterministically, and no concurrent
        // SQLite open ever races WAL initialization.
        let barrier = Arc::new(Barrier::new(2));
        let mut writer_conn = open_knowledge_at(&home, &repo, true).unwrap();
        let parked = barrier.clone();
        let released = barrier.clone();
        let writer = std::thread::spawn(move || {
            parked.wait();
            update_record(
                &mut writer_conn,
                "d1",
                1,
                &update_to("writer zqxwtwo", "active"),
            )
            .unwrap();
            writer_conn.close().unwrap();
            released.wait();
        });
        let conn = open_knowledge_at(&home, &repo, false).unwrap();
        let page = history_with_probe(
            &conn,
            "d1",
            50,
            0,
            Some(1),
            Some(&|| {
                barrier.wait();
                barrier.wait();
            }),
        )
        .unwrap();
        writer.join().unwrap();
        // Generation, total and rows all reflect the pre-commit snapshot.
        assert_eq!(page.generation, 1);
        assert_eq!(page.total, 1);
        assert_eq!(
            page.revisions
                .iter()
                .map(|r| r.revision)
                .collect::<Vec<_>>(),
            vec![1]
        );
        // The concurrent commit itself is intact afterwards.
        assert_eq!(show_record(&conn, "d1", None).unwrap().revision, 2);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_show_snapshot_reads_record_and_generation_together() {
        use std::sync::{Arc, Barrier};
        let (home, repo) = setup("show-snapshot");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "snap show")).unwrap();
        conn.close().unwrap();

        // Why the helper exists: two separate implicit reads can straddle a
        // commit, pairing revision 1 with generation 2.
        let conn = open_knowledge_at(&home, &repo, false).unwrap();
        let record = show_record(&conn, "d1", None).unwrap();
        let mut writer_conn = open_knowledge_at(&home, &repo, true).unwrap();
        update_record(
            &mut writer_conn,
            "d1",
            1,
            &update_to("writer one", "active"),
        )
        .unwrap();
        let generation = read_generation(&conn).unwrap();
        assert_eq!(
            (record.revision, generation),
            (1, 2),
            "separate reads can describe different commits"
        );

        // The helper: a writer commits exactly between the record read and the
        // generation read; both still come from the pre-commit snapshot.
        // Barriers order the threads deterministically, no sleeps.
        let barrier = Arc::new(Barrier::new(2));
        let mut writer_conn = open_knowledge_at(&home, &repo, true).unwrap();
        let parked = barrier.clone();
        let released = barrier.clone();
        let writer = std::thread::spawn(move || {
            parked.wait();
            update_record(
                &mut writer_conn,
                "d1",
                2,
                &update_to("writer two", "active"),
            )
            .unwrap();
            writer_conn.close().unwrap();
            released.wait();
        });
        let (snapshot_record, snapshot_generation) = show_snapshot_with_probe(
            &conn,
            "d1",
            None,
            Some(&|| {
                barrier.wait();
                barrier.wait();
            }),
        )
        .unwrap();
        writer.join().unwrap();

        assert_eq!(snapshot_record.revision, 2);
        assert_eq!(
            snapshot_generation, 2,
            "record and envelope must describe one snapshot"
        );
        // The concurrent commit landed intact after the read transaction.
        assert_eq!(show_record(&conn, "d1", None).unwrap().revision, 3);
        assert_eq!(read_generation(&conn).unwrap(), 3);

        // Historical selection still works, and quiescent reads change nothing.
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let (old, generation) = show_snapshot(&conn, "d1", Some(1)).unwrap();
        assert_eq!((old.revision, generation), (1, 3));
        let after_version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after_version, version);
        assert_eq!(read_generation(&conn).unwrap(), 3);

        conn.close().unwrap();
    }

    #[test]
    fn knowledge_integers_checked_no_panic_no_mutation() {
        let (home, repo) = setup("intbounds");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        // Unrepresentable revision request fails explicitly.
        let err = show_record(&conn, "d1", Some(u64::MAX)).unwrap_err();
        assert!(err.contains("range"), "unexpected: {err}");
        // Pagination beyond i64 fails explicitly.
        let err = history(&conn, "d1", usize::MAX, 0, None).unwrap_err();
        assert!(err.contains("limit"), "unexpected: {err}");
        let err = history(&conn, "d1", 50, usize::MAX, None).unwrap_err();
        assert!(err.contains("offset"), "unexpected: {err}");
        // A corrupt negative stored revision fails explicitly on reads.
        conn.execute("UPDATE heads SET current_revision = -1 WHERE id = 'd1'", [])
            .unwrap();
        let err = show_record(&conn, "d1", None).unwrap_err();
        assert!(err.contains("corrupt"), "unexpected: {err}");
        conn.execute(
            "UPDATE records SET revision = -1 WHERE id = 'd1' AND revision = 1",
            [],
        )
        .unwrap();
        let err = history(&conn, "d1", 50, 0, None).unwrap_err();
        assert!(err.contains("corrupt"), "unexpected: {err}");
        conn.close().unwrap();
        // Generation overflow fails explicitly with no mutation.
        let (home, repo) = setup("genoverflow");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        conn.execute(
            "UPDATE meta SET value = '18446744073709551615' WHERE key = 'knowledge_generation'",
            [],
        )
        .unwrap();
        let err = update_record(&mut conn, "d1", 1, &update_to("t2", "active")).unwrap_err();
        assert!(err.contains("overflow"), "unexpected: {err}");
        assert_eq!(show_record(&conn, "d1", None).unwrap().revision, 1);
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 1);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_parallel_writers_overlap() {
        use std::sync::{Arc, Barrier};
        let (home, repo) = setup("parrace");
        let mut first = open_rw(&home, &repo);
        create_record(&mut first, &new_record("d1", "base zqxpbase")).unwrap();
        let gen_before = read_generation(&first).unwrap();
        drop(first);
        // Both writers read head rev 1 first then rendezvous, so the two
        // expected-1 updates genuinely overlap instead of merely following
        // each other. Connections open sequentially up front so no concurrent
        // SQLite open ever races WAL initialization; only the updates overlap.
        let barrier = Arc::new(Barrier::new(3));
        let mut writers = Vec::new();
        for _ in ["zqxpwina", "zqxpwinb"] {
            writers.push(open_knowledge_at(&home, &repo, true).unwrap());
        }
        let mut handles = Vec::new();
        for (term, mut w) in ["zqxpwina", "zqxpwinb"].into_iter().zip(writers) {
            let b = barrier.clone();
            handles.push(std::thread::spawn(move || {
                assert_eq!(show_record(&w, "d1", None).unwrap().revision, 1);
                b.wait();
                let out = update_record(&mut w, "d1", 1, &update_to(term, "active"));
                w.close().unwrap();
                (term, out.map(|_| term))
            }));
        }
        barrier.wait();
        let mut winners = Vec::new();
        for h in handles {
            let (term, res) = h.join().unwrap();
            match res {
                Ok(_) => winners.push(term),
                Err(e) => assert!(e.contains("conflict"), "loser must conflict: {e}"),
            }
        }
        assert_eq!(winners.len(), 1, "exactly one overlapping writer wins");
        let check = open_knowledge_at(&home, &repo, false).unwrap();
        assert_eq!(show_record(&check, "d1", None).unwrap().revision, 2);
        assert_eq!(history(&check, "d1", 50, 0, None).unwrap().total, 2);
        assert_eq!(read_generation(&check).unwrap(), gen_before + 1);
        // FTS reflects the winner only.
        let winner_hits: i64 = check
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqxpwina' OR knowledge_fts MATCH 'zqxpwinb'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(winner_hits, 1, "index must reflect exactly the winner");
        check.close().unwrap();
    }

    #[test]
    fn knowledge_records_invalid_json_fails_atomically() {
        let (home, repo) = setup("jsonatomic");
        let conn = open_rw(&home, &repo);
        let bad_title = "t".repeat(513);
        let bad_payloads = [
            // Duplicate top-level key.
            r#"{"id":"d1","kind":"decision","title":"t","title":"t2","body":"b","status":"active","author":"r","evidence":[]}"#.to_string(),
            // Unknown field.
            r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","author":"r","evidence":[],"zzz":1}"#.to_string(),
            // Missing author.
            r#"{"id":"d1","kind":"decision","title":"t","body":"b","status":"active","evidence":[]}"#.to_string(),
            // Oversize title (513 bytes).
            format!(
                "{{\"id\":\"d1\",\"kind\":\"decision\",\"title\":\"{bad_title}\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[]}}"
            ),
            // Benchmark without measurement.
            r#"{"id":"b1","kind":"benchmark","title":"t","body":"b","status":"active","author":"r","evidence":[]}"#.to_string(),
        ];
        for raw in &bad_payloads {
            assert!(
                validate_create_json(raw.as_bytes()).is_err(),
                "must fail: {raw}"
            );
        }
        // Nothing was written by any rejected input.
        assert!(show_record(&conn, "d1", None).is_err());
        assert!(show_record(&conn, "b1", None).is_err());
        assert_eq!(read_generation(&conn).unwrap(), 0);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_benchmark_roundtrip() {
        let (home, repo) = setup("benchrt");
        let mut conn = open_rw(&home, &repo);
        let raw = r#"{"id":"b1","kind":"benchmark","title":"speed zqxb","body":"faster","status":"active","author":"r","evidence":[],
            "measurement":{"method":"criterion","environment":"lab-pc","sample_count":100,
            "metrics":{"latency":{"value":1.25,"unit":"ms"},"throughput":{"value":800,"unit":"ops"}}}}"#;
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let (rev, _) = create_validated(&mut conn, &input).unwrap();
        assert_eq!(rev, 1);
        let shown = show_record(&conn, "b1", None).unwrap();
        let measurement = shown
            .measurement
            .expect("benchmark show must carry measurement");
        assert_eq!(measurement.method, "criterion");
        assert_eq!(measurement.sample_count, 100);
        assert_eq!(measurement.metrics.len(), 2);
        assert!(measurement
            .metrics
            .iter()
            .any(|m| m.name == "latency" && m.unit == "ms"));
        // Update with a new measurement replaces it on the new revision only.
        let raw2 = r#"{"title":"speed zqxb2","body":"faster","status":"active","author":"r","evidence":[],
            "measurement":{"method":"criterion","environment":"lab-pc","sample_count":200,
            "metrics":{"latency":{"value":1.0,"unit":"ms"}}}}"#;
        let update = validate_update_json(raw2.as_bytes()).unwrap();
        update_validated(&mut conn, "b1", 1, &update).unwrap();
        let shown2 = show_record(&conn, "b1", None).unwrap();
        assert_eq!(shown2.revision, 2);
        assert_eq!(shown2.measurement.as_ref().unwrap().sample_count, 200);
        let shown1 = show_record(&conn, "b1", Some(1)).unwrap();
        assert_eq!(shown1.measurement.as_ref().unwrap().sample_count, 100);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_benchmark_update_carries_forward() {
        let (home, repo) = setup("benchcarry");
        let mut conn = open_rw(&home, &repo);
        let raw = r#"{"id":"b1","kind":"benchmark","title":"t","body":"b","status":"active","author":"r","evidence":[],
            "measurement":{"method":"m","environment":"e","sample_count":10,
            "metrics":{"m":{"value":1.0,"unit":"ms"}}}}"#;
        let input = validate_create_json(raw.as_bytes()).unwrap();
        create_validated(&mut conn, &input).unwrap();
        // Title-only update without measurement keeps the claim intact.
        let raw2 = r#"{"title":"t2","body":"b","status":"active","author":"r","evidence":[]}"#;
        let update = validate_update_json(raw2.as_bytes()).unwrap();
        update_validated(&mut conn, "b1", 1, &update).unwrap();
        let shown = show_record(&conn, "b1", None).unwrap();
        assert_eq!(shown.measurement.as_ref().unwrap().sample_count, 10);
        conn.close().unwrap();
    }

    #[test]
    fn knowledge_records_measurement_rejected_on_non_benchmark_update() {
        for kind in ["decision", "constraint"] {
            let (home, repo) = setup(&format!("nomeasure-{kind}"));
            let mut conn = open_rw(&home, &repo);
            let raw = format!(
                "{{\"id\":\"d1\",\"kind\":\"{kind}\",\"title\":\"t\",\"body\":\"b\",\"status\":\"active\",\"author\":\"r\",\"evidence\":[]}}"
            );
            let input = validate_create_json(raw.as_bytes()).unwrap();
            create_validated(&mut conn, &input).unwrap();
            let gen_before = read_generation(&conn).unwrap();
            // A fully valid measurement is still refused on non-benchmarks.
            let raw2 = r#"{"title":"t2","body":"b","status":"active","author":"r","evidence":[],
                "measurement":{"method":"m","environment":"e","sample_count":10,
                "metrics":{"m":{"value":1.0,"unit":"ms"}}}}"#;
            let update = validate_update_json(raw2.as_bytes()).unwrap();
            let err = update_validated(&mut conn, "d1", 1, &update).unwrap_err();
            assert!(err.contains("measurement"), "{kind}: unexpected: {err}");
            // No head, revision, FTS or generation change.
            assert_eq!(show_record(&conn, "d1", None).unwrap().revision, 1);
            assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 1);
            assert_eq!(read_generation(&conn).unwrap(), gen_before);
            let hits: i64 = conn
                .query_row(
                    "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 't2'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                hits, 0,
                "{kind}: refused update must not leak into the index"
            );
            conn.close().unwrap();
        }
    }

    #[test]
    fn knowledge_records_mid_write_fault_rolls_back() {
        // Create path: fault after the records insert.
        let (home, repo) = setup("faultcreate");
        let mut conn = open_rw(&home, &repo);
        let raw = r#"{"id":"d1","kind":"decision","title":"fault zqff","body":"b","status":"active","author":"r","evidence":[]}"#;
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let err = create_validated_with_fault(&mut conn, &input).unwrap_err();
        assert!(err.contains("injected"), "unexpected: {err}");
        assert!(show_record(&conn, "d1", None).is_err());
        assert!(history(&conn, "d1", 50, 0, None).is_err());
        assert_eq!(read_generation(&conn).unwrap(), 0);
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqff'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0);
        conn.close().unwrap();
        // Update path: fault after the records insert; head and index intact.
        let (home, repo) = setup("faultupdate");
        let mut conn = open_rw(&home, &repo);
        let raw = r#"{"id":"d1","kind":"decision","title":"base zqfb","body":"b","status":"active","author":"r","evidence":[]}"#;
        let input = validate_create_json(raw.as_bytes()).unwrap();
        create_validated(&mut conn, &input).unwrap();
        let gen_before = read_generation(&conn).unwrap();
        let raw2 =
            r#"{"title":"new zqfn","body":"b","status":"active","author":"r","evidence":[]}"#;
        let update = validate_update_json(raw2.as_bytes()).unwrap();
        let err = update_validated_with_fault(&mut conn, "d1", 1, &update).unwrap_err();
        assert!(err.contains("injected"), "unexpected: {err}");
        assert_eq!(show_record(&conn, "d1", None).unwrap().revision, 1);
        assert_eq!(read_generation(&conn).unwrap(), gen_before);
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM knowledge_fts WHERE knowledge_fts MATCH 'zqfn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0);
        conn.close().unwrap();
    }

    use crate::knowledge::{evidence_support, EvidenceFreshness, VERIFICATION_UNVERIFIED};

    fn repo_root(repo: &RepoEntry) -> PathBuf {
        PathBuf::from(&repo.root)
    }

    fn create_json_with_evidence(id: &str, refs_json: &str) -> String {
        format!(
            "{{\"id\":\"{id}\",\"kind\":\"decision\",\"title\":\"t\",\"body\":\"b\",\
             \"status\":\"active\",\"author\":\"r\",\"evidence\":[{refs_json}]}}"
        )
    }

    fn update_json_with_evidence(refs_json: &str) -> String {
        format!(
            "{{\"title\":\"t2\",\"body\":\"b2\",\"status\":\"active\",\"author\":\"r\",\
             \"evidence\":[{refs_json}]}}"
        )
    }

    #[test]
    fn evidence_write_captures_span_digest_and_reports_fresh() {
        let (home, repo) = setup("evfresh");
        let root = repo_root(&repo);
        std::fs::write(root.join("src.rs"), b"0123456789abcdef").unwrap();
        let raw =
            create_json_with_evidence("d1", r#"{"path":"src.rs","byte_start":0,"byte_end":8}"#);
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let mut conn = open_rw(&home, &repo);
        let (rev, _) = create_validated(&mut conn, &input).unwrap();
        assert_eq!(rev, 1);
        let shown = show_record(&conn, "d1", None).unwrap();
        assert_eq!(shown.evidence.len(), 1);
        let ev = &shown.evidence[0];
        assert_eq!(ev.path, "src.rs");
        assert_eq!((ev.byte_start, ev.byte_end), (Some(0), Some(8)));
        assert_eq!(
            ev.digest,
            crate::store::content_digest(b"01234567"),
            "digest covers the cited span bytes"
        );
        // Tmp fixtures are never git repositories: metadata stays absent
        // rather than fabricated.
        assert!(ev.git_head.is_none() && ev.git_branch.is_none());
        assert!(matches!(
            evidence_freshness(ev, &root),
            EvidenceFreshness::Fresh
        ));
        // Fresh source bytes never verify the prose.
        assert_eq!(VERIFICATION_UNVERIFIED, "Unverified");
        conn.close().unwrap();
    }

    #[test]
    fn evidence_changed_bytes_report_stale_preserving_original() {
        let (home, repo) = setup("evstale");
        let root = repo_root(&repo);
        std::fs::write(root.join("a.rs"), b"AAAAAAAA").unwrap();
        let raw = create_json_with_evidence("d1", r#"{"path":"a.rs"}"#);
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let mut conn = open_rw(&home, &repo);
        create_validated(&mut conn, &input).unwrap();
        std::fs::write(root.join("a.rs"), b"BBBBBBBB").unwrap();
        let shown = show_record(&conn, "d1", None).unwrap();
        let ev = &shown.evidence[0];
        assert_eq!(
            ev.digest,
            crate::store::content_digest(b"AAAAAAAA"),
            "stored binding keeps the original digest"
        );
        match evidence_freshness(ev, &root) {
            EvidenceFreshness::Stale { observed_digest } => {
                assert_eq!(observed_digest, crate::store::content_digest(b"BBBBBBBB"))
            }
            other => panic!("expected stale, got {other:?}"),
        }
        // Truncation below the cited span is drift, not an empty binding.
        std::fs::write(root.join("a.rs"), b"AB").unwrap();
        assert!(matches!(
            evidence_freshness(&shown.evidence[0], &root),
            EvidenceFreshness::Stale { .. }
        ));
        conn.close().unwrap();
    }

    #[test]
    fn evidence_missing_overcap_unreadable_resolve_unresolved() {
        let (home, repo) = setup("evunres");
        let root = repo_root(&repo);
        std::fs::write(root.join("f.rs"), b"hello").unwrap();
        let raw = create_json_with_evidence("d1", r#"{"path":"f.rs"}"#);
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let mut conn = open_rw(&home, &repo);
        create_validated(&mut conn, &input).unwrap();
        let recorded = show_record(&conn, "d1", None).unwrap().evidence;
        assert_eq!(recorded.len(), 1);
        // Missing file: unresolved with a reason, never a replacement.
        std::fs::remove_file(root.join("f.rs")).unwrap();
        match evidence_freshness(&recorded[0], &root) {
            EvidenceFreshness::Unresolved { reason } => assert!(!reason.is_empty()),
            other => panic!("missing file must be unresolved, got {other:?}"),
        }
        // Grown past the source cap: unreadable for binding purposes.
        let big = vec![b'x'; crate::scan::MAX_FILE_BYTES as usize + 1];
        std::fs::write(root.join("f.rs"), &big).unwrap();
        assert!(matches!(
            evidence_freshness(&recorded[0], &root),
            EvidenceFreshness::Unresolved { .. }
        ));
        // Path replaced by a directory: not readable as source bytes.
        std::fs::remove_file(root.join("f.rs")).unwrap();
        std::fs::create_dir(root.join("f.rs")).unwrap();
        assert!(matches!(
            evidence_freshness(&recorded[0], &root),
            EvidenceFreshness::Unresolved { .. }
        ));
        conn.close().unwrap();
    }

    #[test]
    fn evidence_outside_root_and_escape_reject_mutation() {
        let (home, repo) = setup("evreject");
        let root = repo_root(&repo);
        let outside_dir = tmpdir("m7-ev-outside");
        std::fs::write(outside_dir.join("o.rs"), b"outside").unwrap();
        std::fs::write(root.join("inside.rs"), b"inside").unwrap();
        let mut conn = open_rw(&home, &repo);
        for refs in [
            r#"{"path":"../escape.rs"}"#.to_string(),
            r#"{"path":"/absolute.rs"}"#.to_string(),
            r#"{"path":"missing-never.rs"}"#.to_string(),
        ] {
            let raw = create_json_with_evidence("dx", &refs);
            let input = validate_create_json(raw.as_bytes()).unwrap();
            assert!(
                create_validated(&mut conn, &input).is_err(),
                "refs {refs} must reject the whole mutation"
            );
        }
        // Symlink escape: a link inside the root resolving outside.
        let link = root.join("link.rs");
        let target = outside_dir.join("o.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        match plant_link(&link, &target) {
            Some(_) => {}
            None => {
                eprintln!("NOT TESTED: cannot plant links in this environment");
                return;
            }
        }
        let raw = create_json_with_evidence("dx", r#"{"path":"link.rs"}"#);
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let err = create_validated(&mut conn, &input).unwrap_err();
        assert!(
            err.contains("escape") || err.contains("evidence"),
            "unexpected: {err}"
        );
        // Nothing committed, generation untouched.
        assert!(show_record(&conn, "dx", None).is_err());
        assert_eq!(read_generation(&conn).unwrap(), 0);
        conn.close().unwrap();
    }

    #[test]
    fn evidence_span_beyond_file_rejects_mutation() {
        let (home, repo) = setup("evspan");
        let root = repo_root(&repo);
        std::fs::write(root.join("s.rs"), b"12345").unwrap();
        let mut conn = open_rw(&home, &repo);
        // Span end past EOF cannot bind: whole mutation fails atomically.
        let raw =
            create_json_with_evidence("d1", r#"{"path":"s.rs","byte_start":0,"byte_end":100}"#);
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let err = create_validated(&mut conn, &input).unwrap_err();
        assert!(err.contains("span"), "unexpected: {err}");
        assert!(show_record(&conn, "d1", None).is_err());
        assert_eq!(read_generation(&conn).unwrap(), 0);
        // Same guard on update: head and generation unchanged.
        let raw = create_json_with_evidence("d1", r#"{"path":"s.rs"}"#);
        let input = validate_create_json(raw.as_bytes()).unwrap();
        create_validated(&mut conn, &input).unwrap();
        let gen_before = read_generation(&conn).unwrap();
        let bad = validate_update_json(
            update_json_with_evidence(r#"{"path":"s.rs","byte_start":4,"byte_end":50}"#).as_bytes(),
        )
        .unwrap();
        let err = update_validated(&mut conn, "d1", 1, &bad).unwrap_err();
        assert!(err.contains("span"), "unexpected: {err}");
        assert_eq!(show_record(&conn, "d1", None).unwrap().revision, 1);
        assert_eq!(read_generation(&conn).unwrap(), gen_before);
        conn.close().unwrap();
    }

    #[test]
    fn evidence_history_preserves_original_bindings() {
        let (home, repo) = setup("evhist");
        let root = repo_root(&repo);
        std::fs::write(root.join("v1.rs"), b"AAAA").unwrap();
        std::fs::write(root.join("v2.rs"), b"BBBB").unwrap();
        let mut conn = open_rw(&home, &repo);
        let raw = create_json_with_evidence("d1", r#"{"path":"v1.rs"}"#);
        create_validated(&mut conn, &validate_create_json(raw.as_bytes()).unwrap()).unwrap();
        let upd = validate_update_json(update_json_with_evidence(r#"{"path":"v2.rs"}"#).as_bytes())
            .unwrap();
        update_validated(&mut conn, "d1", 1, &upd).unwrap();
        // A revision may also drop evidence: history keeps every binding.
        let bare = validate_update_json(
            r#"{"title":"t3","body":"b3","status":"active","author":"r","evidence":[]}"#.as_bytes(),
        )
        .unwrap();
        update_validated(&mut conn, "d1", 2, &bare).unwrap();
        let page = history(&conn, "d1", 50, 0, None).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.revisions[0].evidence[0].path, "v1.rs");
        assert_eq!(
            page.revisions[0].evidence[0].digest,
            crate::store::content_digest(b"AAAA")
        );
        assert_eq!(page.revisions[1].evidence[0].path, "v2.rs");
        assert_eq!(
            page.revisions[1].evidence[0].digest,
            crate::store::content_digest(b"BBBB")
        );
        assert!(page.revisions[2].evidence.is_empty());
        conn.close().unwrap();
    }

    #[test]
    fn evidence_fault_injection_leaves_no_partial_rows() {
        let (home, repo) = setup("evfault");
        let root = repo_root(&repo);
        std::fs::write(root.join("e.rs"), b"data").unwrap();
        let raw = create_json_with_evidence("d1", r#"{"path":"e.rs"}"#);
        let input = validate_create_json(raw.as_bytes()).unwrap();
        let mut conn = open_rw(&home, &repo);
        let err = create_validated_with_fault(&mut conn, &input).unwrap_err();
        assert!(err.contains("injected"), "unexpected: {err}");
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM evidence", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "fault must roll back evidence rows too");
        assert_eq!(read_generation(&conn).unwrap(), 0);
        // Update path: original binding and generation survive.
        create_validated(&mut conn, &input).unwrap();
        let gen_before = read_generation(&conn).unwrap();
        std::fs::write(root.join("e2.rs"), b"data2").unwrap();
        let upd = validate_update_json(update_json_with_evidence(r#"{"path":"e2.rs"}"#).as_bytes())
            .unwrap();
        let err = update_validated_with_fault(&mut conn, "d1", 1, &upd).unwrap_err();
        assert!(err.contains("injected"), "unexpected: {err}");
        let shown = show_record(&conn, "d1", None).unwrap();
        assert_eq!(shown.revision, 1);
        assert_eq!(shown.evidence[0].path, "e.rs");
        assert_eq!(read_generation(&conn).unwrap(), gen_before);
        conn.close().unwrap();
    }

    #[test]
    fn evidence_truth_boundary_empty_is_unknown() {
        // No evidence: unknown support, never a completeness claim.
        assert_eq!(evidence_support(0), "unknown");
        assert_eq!(evidence_support(2), "unverified");
        let (home, repo) = setup("evtruth");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "plain")).unwrap();
        let shown = show_record(&conn, "d1", None).unwrap();
        assert!(shown.evidence.is_empty());
        assert_eq!(evidence_support(shown.evidence.len()), "unknown");
        conn.close().unwrap();
    }

    fn kind_record(id: &str, kind: &str, title: &str) -> NewRecord {
        NewRecord {
            id: id.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            body: format!("body of {title}"),
            status: "active".to_string(),
            author: "tester".to_string(),
        }
    }

    fn search_default(conn: &Connection, query: &str) -> Result<SearchPage, String> {
        search(conn, query, None, None, false, false, false, 50, 0, None)
    }

    #[test]
    fn search_literal_terms_treat_injection_as_text() {
        let (home, repo) = setup("searchlit");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "alpha zqxone")).unwrap();
        // Every FTS operator / SQL fragment below is inert literal text:
        // bound as quoted phrases, never parsed as query language or SQL.
        for hostile in [
            "\" OR 1=1 --",
            "OR",
            "*",
            "title:alpha",
            "alpha*",
            "\"alpha\"",
            "NEAR(alpha zqxone)",
            "alpha OR zqxone",
            "'; DROP TABLE records; --",
        ] {
            let page = search_default(&conn, hostile).unwrap();
            assert!(
                !page.hits.iter().any(|h| h.id == "d1"),
                "hostile query {hostile:?} must not match via syntax"
            );
        }
        // The plain term still matches title text.
        let page = search_default(&conn, "alpha").unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.hits[0].id, "d1");
        // Multi-term queries require every term (AND).
        create_record(&mut conn, &new_record("d2", "alpha zqxtwo")).unwrap();
        let page = search_default(&conn, "alpha zqxone").unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.hits[0].id, "d1");
        // Body text is indexed too.
        let page = search_default(&conn, "body").unwrap();
        assert_eq!(page.total, 2);
        conn.close().unwrap();
    }

    #[test]
    fn search_indexes_current_revisions_only() {
        let (home, repo) = setup("searchhist");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "histonly-zqxtext")).unwrap();
        update_record(&mut conn, "d1", 1, &update_to("newtext-zqxhead", "active")).unwrap();
        // Superseded revision text is not searchable.
        let gone = search_default(&conn, "histonly-zqxtext").unwrap();
        assert_eq!(gone.total, 0);
        assert!(gone.hits.is_empty());
        assert!(!gone.truncated);
        let head = search_default(&conn, "newtext-zqxhead").unwrap();
        assert_eq!(head.total, 1);
        assert_eq!(head.hits[0].revision, 2);
        // Hits carry the full revision payload for consistency.
        assert_eq!(head.hits[0].title, "newtext-zqxhead");
        conn.close().unwrap();
    }

    #[test]
    fn search_orders_by_id_with_stable_pages() {
        let (home, repo) = setup("searchpage");
        let mut conn = open_rw(&home, &repo);
        for id in ["d3", "d1", "d2"] {
            create_record(
                &mut conn,
                &kind_record(id, "decision", &format!("common zqx-{id}")),
            )
            .unwrap();
        }
        let full = search(
            &conn, "common", None, None, false, false, false, 50, 0, None,
        )
        .unwrap();
        assert_eq!(full.total, 3);
        assert_eq!(
            full.hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["d1", "d2", "d3"]
        );
        assert!(!full.truncated);
        assert_eq!(full.next_offset, None);
        // Paginated reads return the same order in slices.
        let p1 = search(&conn, "common", None, None, false, false, false, 2, 0, None).unwrap();
        assert_eq!(p1.total, 3);
        assert!(p1.truncated);
        assert_eq!(p1.next_offset, Some(2));
        assert_eq!(
            p1.hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["d1", "d2"]
        );
        let p2 = search(
            &conn,
            "common",
            None,
            None,
            false,
            false,
            false,
            2,
            2,
            Some(p1.generation),
        )
        .unwrap();
        assert!(!p2.truncated);
        assert_eq!(p2.next_offset, None);
        assert_eq!(
            p2.hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["d3"]
        );
        conn.close().unwrap();
    }

    #[test]
    fn search_empty_result_is_success() {
        let (home, repo) = setup("searchempty");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "something")).unwrap();
        // No match and whitespace-only queries succeed with empty results.
        for query in ["no-such-term-zqx", "   "] {
            let page = search_default(&conn, query).unwrap();
            assert_eq!(page.total, 0);
            assert!(page.hits.is_empty());
            assert!(!page.truncated);
            assert_eq!(page.next_offset, None);
        }
        // An empty store also searches cleanly.
        let (home2, repo2) = setup("searchempty2");
        let conn2 = open_rw(&home2, &repo2);
        let page = search_default(&conn2, "anything").unwrap();
        assert_eq!(page.total, 0);
        conn.close().unwrap();
        conn2.close().unwrap();
    }

    #[test]
    fn search_archived_and_kind_filters() {
        let (home, repo) = setup("searchfilter");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &kind_record("d1", "decision", "filter zqxme")).unwrap();
        create_record(&mut conn, &kind_record("c1", "constraint", "filter zqxme")).unwrap();
        update_record(&mut conn, "c1", 1, &update_to("filter zqxme", "archived")).unwrap();
        // Archived heads are hidden unless explicitly included.
        let hidden = search_default(&conn, "filter").unwrap();
        assert_eq!(hidden.total, 1);
        assert_eq!(hidden.hits[0].id, "d1");
        let shown = search(&conn, "filter", None, None, true, false, false, 50, 0, None).unwrap();
        assert_eq!(shown.total, 2);
        // Kind narrows before counting: archived constraint still filtered.
        let kinds = search(
            &conn,
            "filter",
            None,
            Some("constraint"),
            false,
            false,
            false,
            50,
            0,
            None,
        )
        .unwrap();
        assert_eq!(kinds.total, 0);
        let kinds = search(
            &conn,
            "filter",
            None,
            Some("constraint"),
            true,
            false,
            false,
            50,
            0,
            None,
        )
        .unwrap();
        assert_eq!(kinds.total, 1);
        assert_eq!(kinds.hits[0].id, "c1");
        // Invalid kind is a validation error, not an empty page.
        assert!(search(
            &conn,
            "filter",
            None,
            Some("note"),
            false,
            false,
            false,
            50,
            0,
            None
        )
        .is_err());
        conn.close().unwrap();
    }

    #[test]
    fn search_limits_and_generation_guard() {
        let (home, repo) = setup("searchgen");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "guarded zqxterm")).unwrap();
        let gen = search_default(&conn, "guarded").unwrap().generation;
        // A concurrent mutation bumps generation: old pages fail explicitly.
        create_record(&mut conn, &new_record("d2", "guarded zqxterm")).unwrap();
        let err = search(
            &conn,
            "guarded",
            None,
            None,
            false,
            false,
            false,
            50,
            0,
            Some(gen),
        )
        .unwrap_err();
        assert!(err.contains("generation"), "unexpected: {err}");
        // Bounds: over-limit pages and over-long queries fail before reading.
        assert!(search(&conn, "guarded", None, None, false, false, false, 1001, 0, None).is_err());
        let long = "q".repeat(4096 + 1);
        assert!(search_default(&conn, &long).is_err());
        // Exactly 4096 bytes is accepted.
        let exact = format!("guarded {}", "q".repeat(4088));
        assert_eq!(exact.len(), 4096);
        assert!(search_default(&conn, &exact).is_ok());
        // Failed writes change neither results nor generation.
        let before = search_default(&conn, "guarded").unwrap();
        assert!(create_record(&mut conn, &new_record("d1", "dup")).is_err());
        let after = search(
            &conn,
            "guarded",
            None,
            None,
            false,
            false,
            false,
            50,
            0,
            Some(before.generation),
        )
        .unwrap();
        assert_eq!(after.total, before.total);
        conn.close().unwrap();
    }

    fn link_simple(
        conn: &mut Connection,
        relation_id: &str,
        src_id: &str,
        src_rev: u64,
        dst_id: &str,
        dst_rev: u64,
        kind: &str,
    ) -> Result<u64, String> {
        link(
            conn,
            relation_id,
            src_id,
            src_rev,
            dst_id,
            dst_rev,
            kind,
            "tester",
            "related work",
            false,
        )
    }

    fn related_outbound(conn: &Connection, id: &str) -> Result<RelatedPage, String> {
        related(
            conn,
            id,
            None,
            RelationDirection::Outbound,
            false,
            50,
            0,
            None,
        )
    }

    fn two_records(conn: &mut Connection) {
        create_record(conn, &new_record("d1", "first")).unwrap();
        create_record(conn, &new_record("d2", "second")).unwrap();
    }

    #[test]
    fn relation_valid_link_pins_exact_revisions() {
        let (home, repo) = setup("relpin");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        let gen = link_simple(&mut conn, "r1", "d1", 1, "d2", 1, "supports").unwrap();
        assert_eq!(read_generation(&conn).unwrap(), gen);
        let page = related_outbound(&conn, "d1").unwrap();
        assert_eq!(page.total, 1);
        assert!(!page.truncated);
        let rel = &page.relations[0];
        assert_eq!(rel.relation_id, "r1");
        assert_eq!((rel.src_id.as_str(), rel.src_revision), ("d1", 1));
        assert_eq!((rel.dst_id.as_str(), rel.dst_revision), ("d2", 1));
        assert_eq!(rel.kind, "supports");
        assert!(!rel.retracted);
        conn.close().unwrap();
    }

    #[test]
    fn relation_endpoints_must_exist_in_same_store() {
        let (home, repo) = setup("relend");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        let gen_before = read_generation(&conn).unwrap();
        // Missing IDs, missing revisions, and empty/oversize relation IDs all
        // fail with no rows and no generation change. A same-name record in
        // another store is unreachable by construction: endpoints resolve in
        // this database only.
        for (rel, src, sr, dst, dr) in [
            ("r1", "missing", 1, "d2", 1),
            ("r2", "d1", 1, "missing", 1),
            ("r3", "d1", 99, "d2", 1),
            ("r4", "d1", 1, "d2", 99),
        ] {
            let err = link_simple(&mut conn, rel, src, sr, dst, dr, "relates_to").unwrap_err();
            assert!(
                err.contains("endpoint") || err.contains("unknown"),
                "unexpected: {err}"
            );
        }
        assert!(link_simple(&mut conn, "", "d1", 1, "d2", 1, "relates_to").is_err());
        assert!(link_simple(&mut conn, &"r".repeat(129), "d1", 1, "d2", 1, "relates_to").is_err());
        assert!(link_simple(&mut conn, "r-ok", "d1", 1, "d2", 1, "certifies").is_err());
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM relations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(read_generation(&conn).unwrap(), gen_before);
        conn.close().unwrap();
    }

    #[test]
    fn relation_duplicate_active_tuple_rejected() {
        let (home, repo) = setup("reldup");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        link_simple(&mut conn, "r1", "d1", 1, "d2", 1, "supports").unwrap();
        let gen_before = read_generation(&conn).unwrap();
        // Same endpoints and kind under a new relation ID still duplicates.
        let err = link_simple(&mut conn, "r2", "d1", 1, "d2", 1, "supports").unwrap_err();
        assert!(err.contains("duplicate"), "unexpected: {err}");
        // Same relation ID is a duplicate too.
        assert!(link_simple(&mut conn, "r1", "d1", 1, "d2", 1, "contradicts").is_err());
        // A different kind is a distinct assertion.
        link_simple(&mut conn, "r3", "d1", 1, "d2", 1, "contradicts").unwrap();
        // Retraction ends the active tuple: re-linking is a new assertion.
        unlink(&mut conn, "r1", "tester", "mistake").unwrap();
        link_simple(&mut conn, "r4", "d1", 1, "d2", 1, "supports").unwrap();
        let page = related_outbound(&conn, "d1").unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(read_generation(&conn).unwrap(), gen_before + 3);
        conn.close().unwrap();
    }

    #[test]
    fn relation_update_preserves_pins_current_vs_historical() {
        let (home, repo) = setup("relhist");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        link_simple(&mut conn, "r1", "d1", 1, "d2", 1, "supports").unwrap();
        // Updating the source never moves its relations to the new revision.
        update_record(&mut conn, "d1", 1, &update_to("first v2", "active")).unwrap();
        let current = related_outbound(&conn, "d1").unwrap();
        assert_eq!(current.total, 0, "rev 2 carries no relations forward");
        let pinned = related(
            &conn,
            "d1",
            Some(1),
            RelationDirection::Outbound,
            false,
            50,
            0,
            None,
        )
        .unwrap();
        assert_eq!(pinned.total, 1);
        assert_eq!(pinned.relations[0].relation_id, "r1");
        // The untouched target still sees the inbound link at its head.
        let inbound = related(
            &conn,
            "d2",
            None,
            RelationDirection::Inbound,
            false,
            50,
            0,
            None,
        )
        .unwrap();
        assert_eq!(inbound.total, 1);
        // Unknown revisions fail instead of guessing.
        assert!(related(
            &conn,
            "d1",
            Some(99),
            RelationDirection::Outbound,
            false,
            50,
            0,
            None
        )
        .is_err());
        assert!(related(
            &conn,
            "missing",
            None,
            RelationDirection::Outbound,
            false,
            50,
            0,
            None
        )
        .is_err());
        conn.close().unwrap();
    }

    #[test]
    fn relation_retraction_keeps_audit_and_rejects_double_unlink() {
        let (home, repo) = setup("relretract");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        link_simple(&mut conn, "r1", "d1", 1, "d2", 1, "supports").unwrap();
        unlink(&mut conn, "r1", "tester", "no longer holds").unwrap();
        // Normal queries omit retracted relations.
        assert_eq!(related_outbound(&conn, "d1").unwrap().total, 0);
        // Audit output retains them with author and reason.
        let audit = related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            true,
            50,
            0,
            None,
        )
        .unwrap();
        assert_eq!(audit.total, 1);
        assert!(audit.relations[0].retracted);
        assert_eq!(
            audit.relations[0].retraction_author.as_deref(),
            Some("tester")
        );
        assert_eq!(
            audit.relations[0].retraction_reason.as_deref(),
            Some("no longer holds")
        );
        // Retraction is append-only: repeats and unknown IDs fail.
        let gen_before = read_generation(&conn).unwrap();
        assert!(unlink(&mut conn, "r1", "tester", "again").is_err());
        assert!(unlink(&mut conn, "missing", "tester", "reason").is_err());
        assert!(unlink(&mut conn, "r1", "", "reason").is_err());
        assert!(unlink(&mut conn, "r1", "tester", "").is_err());
        assert_eq!(read_generation(&conn).unwrap(), gen_before);
        conn.close().unwrap();
    }

    #[test]
    fn relation_self_link_and_cycle_stay_one_hop() {
        let (home, repo) = setup("relcycle");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        // Authored self-links and cycles are permitted; queries never infer
        // transitive relations from them.
        link_simple(&mut conn, "r-self", "d1", 1, "d1", 1, "relates_to").unwrap();
        link_simple(&mut conn, "r-fwd", "d1", 1, "d2", 1, "supports").unwrap();
        link_simple(&mut conn, "r-back", "d2", 1, "d1", 1, "contradicts").unwrap();
        let out = related_outbound(&conn, "d1").unwrap();
        assert_eq!(out.total, 2);
        // Order is endpoint/revision/kind + relation ID: dst d1 sorts
        // before dst d2, so the self-link comes first.
        assert_eq!(
            out.relations
                .iter()
                .map(|r| r.relation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["r-self", "r-fwd"]
        );
        // r-back is inbound to d1, not a second outbound hop; d2 sees only
        // its own outbound link (no transitive d2 -> d1 -> d1 expansion).
        let d2out = related_outbound(&conn, "d2").unwrap();
        assert_eq!(d2out.total, 1);
        assert_eq!(d2out.relations[0].relation_id, "r-back");
        conn.close().unwrap();
    }

    #[test]
    fn relation_pagination_and_generation_guard() {
        let (home, repo) = setup("relpage");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        create_record(&mut conn, &new_record("d3", "third")).unwrap();
        // Created out of order: reads stay in endpoint/kind/relation order.
        link_simple(&mut conn, "r-c", "d1", 1, "d3", 1, "supports").unwrap();
        link_simple(&mut conn, "r-a", "d1", 1, "d2", 1, "supports").unwrap();
        link_simple(&mut conn, "r-b", "d1", 1, "d2", 1, "contradicts").unwrap();
        let full = related_outbound(&conn, "d1").unwrap();
        assert_eq!(full.total, 3);
        // Endpoint order first (dst d2 before d3), then kind
        // ("contradicts" before "supports"), then relation ID.
        assert_eq!(
            full.relations
                .iter()
                .map(|r| r.relation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["r-b", "r-a", "r-c"]
        );
        let p1 = related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            false,
            2,
            0,
            None,
        )
        .unwrap();
        assert!(p1.truncated);
        assert_eq!(p1.next_offset, Some(2));
        let p2 = related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            false,
            2,
            2,
            Some(p1.generation),
        )
        .unwrap();
        assert!(!p2.truncated);
        assert_eq!(p2.relations[0].relation_id, "r-c");
        // A concurrent link bumps generation: stale pages fail explicitly.
        link_simple(&mut conn, "r-d", "d1", 1, "d3", 1, "contradicts").unwrap();
        let err = related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            false,
            2,
            2,
            Some(p1.generation),
        )
        .unwrap_err();
        assert!(err.contains("generation"), "unexpected: {err}");
        assert!(related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            false,
            1001,
            0,
            None
        )
        .is_err());
        conn.close().unwrap();
    }

    #[test]
    fn relation_foundation_status_guard() {
        // Full-stage record statuses stay invalid even beside retraction
        // events, which live on links — the two lifecycles never mix.
        let (home, repo) = setup("relstatus");
        let mut conn = open_rw(&home, &repo);
        two_records(&mut conn);
        for status in ["retracted", "superseded"] {
            let raw = format!(
                "{{\"id\":\"dx\",\"kind\":\"decision\",\"title\":\"t\",\"body\":\"b\",\
                 \"status\":\"{status}\",\"author\":\"r\",\"evidence\":[]}}"
            );
            assert!(validate_create_json(raw.as_bytes()).is_err());
            // The store path rejects them too, beneath the JSON layer.
            let err = update_record(&mut conn, "d1", 1, &update_to("t", status)).unwrap_err();
            assert!(err.contains("status"), "{status}: unexpected: {err}");
        }
        // Link retraction still succeeds: it is an event, not a status.
        link_simple(&mut conn, "r1", "d1", 1, "d2", 1, "supports").unwrap();
        unlink(&mut conn, "r1", "tester", "reason").unwrap();
        assert_eq!(related_outbound(&conn, "d1").unwrap().total, 0);
        conn.close().unwrap();
    }

    #[test]
    fn search_related_history_reject_zero_limit() {
        let (home, repo) = setup("zerolimit");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "zero zqxlimit")).unwrap();
        // A zero limit can never advance next_offset: reject it instead of
        // returning a page that points at itself.
        let err = search(&conn, "zero", None, None, false, false, false, 0, 0, None).unwrap_err();
        assert!(err.contains("limit"), "unexpected: {err}");
        let err = related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            false,
            0,
            0,
            None,
        )
        .unwrap_err();
        assert!(err.contains("limit"), "unexpected: {err}");
        let err = history(&conn, "d1", 0, 0, None).unwrap_err();
        assert!(err.contains("limit"), "unexpected: {err}");
        // Positive limits still work everywhere.
        assert_eq!(search_default(&conn, "zero").unwrap().total, 1);
        assert_eq!(related_outbound(&conn, "d1").unwrap().total, 0);
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 1);
        conn.close().unwrap();
    }

    #[test]
    fn search_pages_advance_and_terminate() {
        let (home, repo) = setup("pagewalk");
        let mut conn = open_rw(&home, &repo);
        for i in 1..=5 {
            create_record(
                &mut conn,
                &kind_record(&format!("w{i:02}"), "decision", "walk zqxterm"),
            )
            .unwrap();
        }
        // Walk the next_offset chain: strictly advancing, terminating with
        // no next offset on the final page.
        let mut collected = Vec::new();
        let mut offset = 0;
        let mut pages = 0;
        loop {
            let page = search(
                &conn, "walk", None, None, false, false, false, 2, offset, None,
            )
            .unwrap();
            assert!(page.truncated || page.next_offset.is_none());
            collected.extend(page.hits.iter().map(|h| h.id.clone()));
            pages += 1;
            assert!(pages <= 10, "pagination must terminate");
            match page.next_offset {
                Some(next) => {
                    assert!(next > offset, "next offset must advance");
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(pages, 3);
        assert_eq!(collected, vec!["w01", "w02", "w03", "w04", "w05"]);
        // Final and empty pages carry no next offset.
        let last = search(&conn, "walk", None, None, false, false, false, 2, 4, None).unwrap();
        assert!(!last.truncated);
        assert_eq!(last.next_offset, None);
        let past_end = search(&conn, "walk", None, None, false, false, false, 2, 99, None).unwrap();
        assert_eq!(past_end.total, 5);
        assert!(past_end.hits.is_empty());
        assert!(!past_end.truncated);
        assert_eq!(past_end.next_offset, None);
        conn.close().unwrap();
    }

    #[test]
    fn search_bounded_pages_over_large_match_set() {
        let (home, repo) = setup("bigpage");
        let root = repo_root(&repo);
        let mut conn = open_rw(&home, &repo);
        // More matches than any single page, plus archived and other-kind
        // records that filters must exclude before counting.
        for i in 1..=12 {
            create_record(
                &mut conn,
                &kind_record(&format!("k{i:02}"), "decision", "bounded zqxmatch"),
            )
            .unwrap();
        }
        create_record(
            &mut conn,
            &kind_record("c1", "constraint", "bounded zqxmatch"),
        )
        .unwrap();
        create_record(
            &mut conn,
            &kind_record("c2", "constraint", "bounded zqxmatch"),
        )
        .unwrap();
        update_record(
            &mut conn,
            "k12",
            1,
            &update_to("bounded zqxmatch", "archived"),
        )
        .unwrap();
        std::fs::write(root.join("ev.rs"), b"bounded zqxmatch").unwrap();
        let raw = "{\"id\":\"kev\",\"kind\":\"decision\",\"title\":\"bounded zqxmatch\",\
                   \"body\":\"bounded zqxmatch body\",\"status\":\"active\",\"author\":\"r\",\
                   \"evidence\":[{\"path\":\"ev.rs\"}]}";
        create_validated(&mut conn, &validate_create_json(raw.as_bytes()).unwrap()).unwrap();
        // Default view: 11 visible decisions + kev + 2 constraints = 14.
        let p1 = search(
            &conn, "bounded", None, None, false, false, false, 5, 0, None,
        )
        .unwrap();
        assert_eq!(p1.total, 14);
        assert!(p1.truncated);
        assert_eq!(p1.next_offset, Some(5));
        assert_eq!(
            p1.hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            vec!["c1", "c2", "k01", "k02", "k03"]
        );
        // Kind filter narrows before counting: 11 visible decisions + kev.
        let kinds = search(
            &conn,
            "bounded",
            None,
            Some("decision"),
            false,
            false,
            false,
            5,
            10,
            None,
        )
        .unwrap();
        assert_eq!(kinds.total, 12);
        assert!(!kinds.truncated);
        assert_eq!(kinds.next_offset, None);
        assert_eq!(kinds.hits.len(), 2);
        assert_eq!(kinds.hits[1].id, "kev");
        // Returned rows carry their full payload; archived appears on demand.
        assert!(kinds.hits[1].evidence.iter().any(|e| e.path == "ev.rs"));
        let all = search(
            &conn,
            "bounded",
            None,
            Some("decision"),
            true,
            false,
            false,
            50,
            0,
            None,
        )
        .unwrap();
        assert_eq!(all.total, 13);
        assert!(all
            .hits
            .iter()
            .any(|h| h.id == "k12" && h.status == "archived"));
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_supersede_writes_revision_event_and_relation() {
        let (home, repo) = setup("supersede");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "old claim")).unwrap();
        create_record(&mut conn, &new_record("d2", "new claim")).unwrap();
        let (rev, gen) =
            update_record(&mut conn, "d1", 1, &supersede_to("d2", 1, "d2 replaces d1")).unwrap();
        assert_eq!(rev, 2);
        // The new revision carries the superseded status and target.
        let record = show_record(&conn, "d1", None).unwrap();
        assert_eq!(record.status, "superseded");
        assert_eq!(
            record.superseded_by,
            Some(SupersededRef {
                id: "d2".to_string(),
                revision: 1
            })
        );
        assert!(record.retracted_reason.is_none());
        // The immutable lifecycle event pins the exact target and reason.
        let (event, target_id, target_revision, reason, author, created): (
            String,
            String,
            i64,
            String,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT event, target_id, target_revision, reason, author, created_unix
                 FROM record_lifecycle WHERE id = 'd1' AND revision = 2",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(event, "supersede");
        assert_eq!(target_id, "d2");
        assert_eq!(target_revision, 1);
        assert_eq!(reason, "d2 replaces d1");
        assert_eq!(author, "tester");
        assert!(created > 0);
        // The implicit relation runs replacement -> superseded revision.
        let relation_id = "supersedes:d2:1:d1:2";
        let (src_id, src_rev, dst_id, dst_rev, kind, created): (
            String,
            i64,
            String,
            i64,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT src_id, src_revision, dst_id, dst_revision, kind, created_unix
                 FROM relations WHERE relation_id = ?1",
                [relation_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (
                src_id.as_str(),
                src_rev,
                dst_id.as_str(),
                dst_rev,
                kind.as_str()
            ),
            ("d2", 1, "d1", 2, "supersedes")
        );
        assert!(created > 0);
        // Visible through related(): inbound on the old record names the
        // replacement; outbound on the replacement names the old record.
        let inbound = related(
            &conn,
            "d1",
            None,
            RelationDirection::Inbound,
            false,
            10,
            0,
            None,
        )
        .unwrap();
        assert_eq!(inbound.total, 1);
        assert_eq!(inbound.relations[0].relation_id, relation_id);
        assert_eq!(inbound.relations[0].src_id, "d2");
        let outbound = related(
            &conn,
            "d2",
            None,
            RelationDirection::Outbound,
            false,
            10,
            0,
            None,
        )
        .unwrap();
        assert_eq!(outbound.total, 1);
        assert_eq!(outbound.relations[0].dst_id, "d1");
        // One generation bump for the whole transition, and the write
        // migrated the v1 store as its first step.
        assert_eq!(read_generation(&conn).unwrap(), gen);
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        // History keeps the pre-transition revision untouched.
        let page = history(&conn, "d1", 50, 0, None).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.revisions[0].status, "active");
        assert!(page.revisions[0].superseded_by.is_none());
        assert_eq!(page.revisions[1].status, "superseded");
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_resupersede_appends_immutable_history() {
        let (home, repo) = setup("resupersede");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "old claim")).unwrap();
        create_record(&mut conn, &new_record("d2", "first replacement")).unwrap();
        create_record(&mut conn, &new_record("d3", "better replacement")).unwrap();
        let (_, gen1) =
            update_record(&mut conn, "d1", 1, &supersede_to("d2", 1, "first try")).unwrap();
        // Re-superseding to a different valid target is allowed: a new
        // revision, event and relation; nothing is rewritten.
        let (rev, gen2) =
            update_record(&mut conn, "d1", 2, &supersede_to("d3", 1, "better fit")).unwrap();
        assert_eq!(rev, 3);
        assert!(gen2 > gen1);
        let targets: Vec<(i64, String, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT revision, target_id, target_revision FROM record_lifecycle
                     WHERE id = 'd1' ORDER BY revision",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert_eq!(
            targets,
            vec![(2, "d2".to_string(), 1), (3, "d3".to_string(), 1)]
        );
        let relations: i64 = conn
            .query_row(
                "SELECT count(*) FROM relations WHERE kind = 'supersedes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(relations, 2, "the first lineage relation survives");
        let head = show_record(&conn, "d1", None).unwrap();
        assert_eq!(
            head.superseded_by,
            Some(SupersededRef {
                id: "d3".to_string(),
                revision: 1
            })
        );
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_supersede_rejections_leave_store_unchanged() {
        let (home, repo) = setup("supersede-reject");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "old claim")).unwrap();
        create_record(&mut conn, &new_record("d2", "replacement")).unwrap();
        create_record(&mut conn, &new_record("d3", "retracted one")).unwrap();
        let gen = read_generation(&conn).unwrap();
        // Self-supersession.
        let err = update_record(&mut conn, "d1", 1, &supersede_to("d1", 1, "self")).unwrap_err();
        assert!(
            err.contains("invalid lifecycle transition"),
            "unexpected: {err}"
        );
        assert!(err.contains("itself"), "unexpected: {err}");
        // Unknown target revision and unknown target record.
        let err = update_record(&mut conn, "d1", 1, &supersede_to("d2", 99, "ghost")).unwrap_err();
        assert!(
            err.contains("unknown supersede target"),
            "unexpected: {err}"
        );
        let err = update_record(&mut conn, "d1", 1, &supersede_to("nope", 1, "ghost")).unwrap_err();
        assert!(
            err.contains("unknown supersede target"),
            "unexpected: {err}"
        );
        // Nothing was written, and the rejected transitions rolled their
        // migration back: the store is still v1.
        assert_eq!(read_generation(&conn).unwrap(), gen);
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 1);
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 1,
            "a rejected transition must roll its migration back"
        );
        // Retracting d3 is a real transition: it migrates and commits.
        update_record(&mut conn, "d3", 1, &retract_to("withdrawn")).unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2, "the first accepted transition migrates");
        let gen = read_generation(&conn).unwrap();
        // Target record is retracted.
        let err =
            update_record(&mut conn, "d1", 1, &supersede_to("d3", 2, "withdrawn")).unwrap_err();
        assert!(
            err.contains("invalid lifecycle transition"),
            "unexpected: {err}"
        );
        assert!(err.contains("retracted"), "unexpected: {err}");
        assert_eq!(read_generation(&conn).unwrap(), gen);
        // Same-target rejection: first supersede lands, repeating it fails.
        update_record(&mut conn, "d1", 1, &supersede_to("d2", 1, "first")).unwrap();
        let gen = read_generation(&conn).unwrap();
        let err = update_record(&mut conn, "d1", 2, &supersede_to("d2", 1, "again")).unwrap_err();
        assert!(
            err.contains("invalid lifecycle transition"),
            "unexpected: {err}"
        );
        assert!(err.contains("already supersedes"), "unexpected: {err}");
        assert_eq!(read_generation(&conn).unwrap(), gen);
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 2);
        // Superseding an already-retracted record is rejected.
        let err = update_record(&mut conn, "d3", 2, &supersede_to("d1", 2, "revive")).unwrap_err();
        assert!(
            err.contains("invalid lifecycle transition"),
            "unexpected: {err}"
        );
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_supersede_target_revision_overflow_rejected_atomically() {
        let (home, repo) = setup("supersede-overflow");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "claim d1")).unwrap();
        create_record(&mut conn, &new_record("d2", "claim d2")).unwrap();
        let gen = read_generation(&conn).unwrap();
        // A target revision beyond the representable range is rejected by the
        // store itself (defense in depth: the CLI validator rejects earlier),
        // before any mutation, and the migration rolls back with it.
        let err = update_record(
            &mut conn,
            "d1",
            1,
            &supersede_to("d2", u64::MAX, "overflow"),
        )
        .unwrap_err();
        assert!(
            err.contains("supersede target revision out of range"),
            "unexpected: {err}"
        );
        assert_eq!(read_generation(&conn).unwrap(), gen);
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 1);
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 1,
            "a rejected transition must roll its migration back"
        );
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_supersede_cycle_rejected() {
        let (home, repo) = setup("supersede-cycle");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("a", "claim a")).unwrap();
        create_record(&mut conn, &new_record("b", "claim b")).unwrap();
        // A supersedes B (edge a -> b), then B -> A would close a cycle.
        update_record(&mut conn, "b", 1, &supersede_to("a", 1, "a replaces b")).unwrap();
        let gen = read_generation(&conn).unwrap();
        let err =
            update_record(&mut conn, "a", 1, &supersede_to("b", 2, "b replaces a")).unwrap_err();
        assert!(err.contains("cycle"), "unexpected: {err}");
        assert_eq!(
            read_generation(&conn).unwrap(),
            gen,
            "rejected cycle must not bump"
        );
        assert_eq!(history(&conn, "a", 50, 0, None).unwrap().total, 1);
        // Indirect cycle: c supersedes a, then a's replacement chain back to c
        // through existing edges must be detected as well.
        create_record(&mut conn, &new_record("c", "claim c")).unwrap();
        update_record(&mut conn, "c", 1, &supersede_to("b", 2, "b replaces c")).unwrap();
        // Live edges: a -> b, b -> c. Now c superseded by a would need edge
        // a -> c... that is already the direct case; instead close it via c:
        // a's outbound walk a -> b -> c reaches target c.
        let gen = read_generation(&conn).unwrap();
        let err =
            update_record(&mut conn, "a", 1, &supersede_to("c", 2, "c replaces a")).unwrap_err();
        assert!(err.contains("cycle"), "unexpected: {err}");
        assert_eq!(read_generation(&conn).unwrap(), gen);
        // A retracted lineage relation does not open the door to a cycle: the
        // supersedes history keeps the lineage acyclic.
        unlink(&mut conn, "supersedes:a:1:b:2", "tester", "undo for test").unwrap();
        let b = show_record(&conn, "b", None).unwrap();
        assert_eq!(
            b.status, "superseded",
            "unlinking a lineage relation must not change record status"
        );
        assert_eq!(
            b.superseded_by,
            Some(SupersededRef {
                id: "a".to_string(),
                revision: 1
            })
        );
        let err =
            update_record(&mut conn, "a", 1, &supersede_to("b", 2, "b replaces a")).unwrap_err();
        assert!(err.contains("cycle"), "unexpected: {err}");
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_retract_rules() {
        let (home, repo) = setup("retract");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "claim")).unwrap();
        create_record(&mut conn, &new_record("d2", "replacement")).unwrap();
        create_record(&mut conn, &new_record("d3", "also replacement")).unwrap();
        let (rev, _) = update_record(&mut conn, "d1", 1, &retract_to("no longer true")).unwrap();
        assert_eq!(rev, 2);
        let record = show_record(&conn, "d1", None).unwrap();
        assert_eq!(record.status, "retracted");
        assert_eq!(record.retracted_reason.as_deref(), Some("no longer true"));
        assert_eq!(record.retracted_author.as_deref(), Some("tester"));
        assert!(record.superseded_by.is_none());
        // Double retraction is rejected: withdrawal is a one-way event.
        let gen = read_generation(&conn).unwrap();
        let err = update_record(&mut conn, "d1", 2, &retract_to("again")).unwrap_err();
        assert!(err.contains("already retracted"), "unexpected: {err}");
        assert_eq!(read_generation(&conn).unwrap(), gen);
        // Superseding a retracted record is rejected (no silent revival).
        let err = update_record(&mut conn, "d1", 2, &supersede_to("d2", 1, "revive")).unwrap_err();
        assert!(
            err.contains("invalid lifecycle transition"),
            "unexpected: {err}"
        );
        // Retracting a superseded record is allowed and does not touch the
        // lineage: rev2 keeps its supersede event and relation.
        update_record(&mut conn, "d2", 1, &supersede_to("d3", 1, "d3 is better")).unwrap();
        let (rev3, _) = update_record(&mut conn, "d2", 2, &retract_to("bench invalid")).unwrap();
        assert_eq!(rev3, 3);
        let head = show_record(&conn, "d2", None).unwrap();
        assert_eq!(head.status, "retracted");
        assert_eq!(head.retracted_reason.as_deref(), Some("bench invalid"));
        let rev2 = show_record(&conn, "d2", Some(2)).unwrap();
        assert_eq!(
            rev2.superseded_by,
            Some(SupersededRef {
                id: "d3".to_string(),
                revision: 1
            })
        );
        // History still carries the superseded state ...
        let page = history(&conn, "d2", 50, 0, None).unwrap();
        assert_eq!(page.revisions[1].status, "superseded");
        // ... and the derived relation survives the retraction.
        let inbound = related(
            &conn,
            "d2",
            Some(2),
            RelationDirection::Inbound,
            false,
            10,
            0,
            None,
        )
        .unwrap();
        assert_eq!(inbound.total, 1);
        assert_eq!(inbound.relations[0].kind, "supersedes");
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_search_visibility_matrix() {
        let (home, repo) = setup("visibility");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("a1", "zqxvis active")).unwrap();
        let mut archived = new_record("a2", "zqxvis archived");
        archived.status = "archived".to_string();
        create_record(&mut conn, &archived).unwrap();
        create_record(&mut conn, &new_record("a3", "zqxvis replaced")).unwrap();
        create_record(&mut conn, &new_record("a4", "zqxvis withdrawn")).unwrap();
        // The transitions keep the searchable text so visibility, not
        // matching, is what the flags under test decide.
        update_record(
            &mut conn,
            "a3",
            1,
            &UpdateRecord {
                title: "zqxvis replaced".to_string(),
                body: "zqxvis replaced body".to_string(),
                status: "superseded".to_string(),
                author: "tester".to_string(),
                superseded_by: Some(SupersededRef {
                    id: "a1".to_string(),
                    revision: 1,
                }),
                reason: Some("a1 supersedes".to_string()),
                confirm: false,
            },
        )
        .unwrap();
        update_record(
            &mut conn,
            "a4",
            1,
            &UpdateRecord {
                title: "zqxvis withdrawn".to_string(),
                body: "zqxvis withdrawn body".to_string(),
                status: "retracted".to_string(),
                author: "tester".to_string(),
                superseded_by: None,
                reason: Some("bad evidence".to_string()),
                confirm: false,
            },
        )
        .unwrap();
        let ids = |page: &SearchPage| page.hits.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
        // Default view: active only.
        let page = search(
            &conn, "zqxvis", None, None, false, false, false, 50, 0, None,
        )
        .unwrap();
        assert_eq!(ids(&page), vec!["a1"]);
        // Each historical visibility is opt-in and independent.
        let page = search(&conn, "zqxvis", None, None, true, false, false, 50, 0, None).unwrap();
        assert_eq!(ids(&page), vec!["a1", "a2"]);
        let page = search(&conn, "zqxvis", None, None, false, true, false, 50, 0, None).unwrap();
        assert_eq!(ids(&page), vec!["a1", "a3"]);
        let page = search(&conn, "zqxvis", None, None, false, false, true, 50, 0, None).unwrap();
        assert_eq!(ids(&page), vec!["a1", "a4"]);
        // --include-retracted alone must not surface archived records.
        let page = search(&conn, "zqxvis", None, None, false, false, true, 50, 0, None).unwrap();
        assert!(!ids(&page).contains(&"a2".to_string()));
        // Everything on request.
        let page = search(&conn, "zqxvis", None, None, true, true, true, 50, 0, None).unwrap();
        assert_eq!(ids(&page), vec!["a1", "a2", "a3", "a4"]);
        // Historical hits carry their full lifecycle payload.
        let page = search(&conn, "zqxvis", None, None, false, true, true, 50, 0, None).unwrap();
        let superseded = page.hits.iter().find(|h| h.id == "a3").unwrap();
        assert_eq!(
            superseded.superseded_by,
            Some(SupersededRef {
                id: "a1".to_string(),
                revision: 1
            })
        );
        let retracted = page.hits.iter().find(|h| h.id == "a4").unwrap();
        assert_eq!(retracted.retracted_reason.as_deref(), Some("bad evidence"));
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_supersede_max_length_ids_keeps_full_relation_identity() {
        let (home, repo) = setup("supersede-maxid");
        let mut conn = open_rw(&home, &repo);
        let old = "o".repeat(128);
        let new = "n".repeat(128);
        create_record(&mut conn, &new_record(&old, "old max id")).unwrap();
        create_record(&mut conn, &new_record(&new, "new max id")).unwrap();
        update_record(&mut conn, &old, 1, &supersede_to(&new, 1, "max ids")).unwrap();
        let relation_id = format!("supersedes:{new}:1:{old}:2");
        assert!(
            relation_id.len() > 128,
            "the derived identity exceeds the caller-supplied relation id cap"
        );
        // The store-generated identity is preserved in full: nothing is
        // truncated or rejected, and the relation lists normally.
        let listed = related(
            &conn,
            &old,
            None,
            RelationDirection::Inbound,
            false,
            10,
            0,
            None,
        )
        .unwrap();
        assert_eq!(listed.total, 1);
        assert_eq!(listed.relations[0].relation_id, relation_id);
        assert_eq!(listed.relations[0].kind, "supersedes");
        let stored: i64 = conn
            .query_row(
                "SELECT count(*) FROM relations WHERE relation_id = ?1",
                [&relation_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, 1);
        // Full-length derived ids are referenceable for retraction: the
        // reference validator accepts exactly this shape, the retraction is
        // an append-only event with a generation bump, and the relation row
        // keeps its full identity.
        let gen_before = read_generation(&conn).unwrap();
        let generation = unlink(&mut conn, &relation_id, "tester", "would retract").unwrap();
        assert_eq!(
            generation,
            gen_before + 1,
            "a successful retraction bumps the generation once"
        );
        let retractions: i64 = conn
            .query_row(
                "SELECT count(*) FROM retractions WHERE relation_id = ?1",
                [&relation_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retractions, 1);
        let still_stored: i64 = conn
            .query_row(
                "SELECT count(*) FROM relations WHERE relation_id = ?1",
                [&relation_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_stored, 1, "retraction is an event, never a deletion");
        let audited = related(
            &conn,
            &old,
            None,
            RelationDirection::Inbound,
            true,
            10,
            0,
            None,
        )
        .unwrap();
        assert_eq!(audited.total, 1);
        assert!(audited.relations[0].retracted);
        assert_eq!(audited.relations[0].relation_id, relation_id);
        // Repeating the retraction fails and changes nothing.
        let err = unlink(&mut conn, &relation_id, "tester", "again").unwrap_err();
        assert!(err.contains("already retracted"), "unexpected: {err}");
        assert_eq!(read_generation(&conn).unwrap(), generation);
        conn.close().unwrap();
    }

    #[test]
    fn lifecycle_plain_writes_stay_v1_with_default_reads() {
        let (home, repo) = setup("stillv1");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "plain zqxtitle")).unwrap();
        update_record(
            &mut conn,
            "d1",
            1,
            &update_to("plain zqxtitle v2", "archived"),
        )
        .unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "Foundation writes must not migrate");
        // Reads against v1 return documented defaults and never migrate.
        let shown = show_record(&conn, "d1", None).unwrap();
        assert!(shown.superseded_by.is_none());
        assert!(shown.retracted_reason.is_none());
        assert!(shown.retracted_author.is_none());
        assert!(shown.confirm_state.is_none());
        let _ = history(&conn, "d1", 50, 0, None).unwrap();
        let _ = search_default(&conn, "plain").unwrap();
        let _ = related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            false,
            10,
            0,
            None,
        )
        .unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "reads never migrate");
        conn.close().unwrap();
    }

    #[test]
    fn provenance_confirm_writes_event_and_new_revision() {
        let (home, repo) = setup("confirm");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        // A v1 store has no confirmation state to report: the field is absent.
        let before = show_record(&conn, "d1", None).unwrap();
        assert!(
            before.confirm_state.is_none(),
            "v1 reads carry no confirm state"
        );
        let gen_before = read_generation(&conn).unwrap();
        let (rev, gen) = update_record(&mut conn, "d1", 1, &confirm_to("t1")).unwrap();
        assert_eq!(rev, 2);
        assert!(gen > gen_before, "writing a confirmation bumps once");
        // The confirming revision reads as confirmed; the revision it
        // confirms reads as the candidate it was.
        let head = show_record(&conn, "d1", None).unwrap();
        assert_eq!(head.confirm_state.as_deref(), Some("confirmed"));
        let first = show_record(&conn, "d1", Some(1)).unwrap();
        assert_eq!(first.confirm_state.as_deref(), Some("candidate"));
        // The immutable event row is authored by the confirming revision.
        let (event, author): (String, String) = conn
            .query_row(
                "SELECT event, author FROM record_lifecycle
                 WHERE id = 'd1' AND revision = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((event.as_str(), author.as_str()), ("confirm", "tester"));
        // The first confirmation migrated the store; search reports the
        // confirmed state on the hit.
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let page = search_default(&conn, "t1").unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.hits[0].confirm_state.as_deref(), Some("confirmed"));
        conn.close().unwrap();
    }

    #[test]
    fn provenance_reconfirm_is_a_noop() {
        let (home, repo) = setup("reconfirm");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        update_record(&mut conn, "d1", 1, &confirm_to("t1")).unwrap();
        // Re-confirming the head revision with identical content and no new
        // evidence succeeds without writing: same revision, same generation.
        let gen = read_generation(&conn).unwrap();
        let (rev, gen_after) = update_record(&mut conn, "d1", 2, &confirm_to("t1")).unwrap();
        assert_eq!((rev, gen_after), (2, gen));
        assert_eq!(history(&conn, "d1", 50, 0, None).unwrap().total, 2);
        let events: i64 = conn
            .query_row(
                "SELECT count(*) FROM record_lifecycle WHERE id = 'd1' AND event = 'confirm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1, "a no-op confirmation writes no second event");
        conn.close().unwrap();
    }

    #[test]
    fn provenance_confirm_content_change_and_unconfirmed_revisions() {
        let (home, repo) = setup("confirm-change");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        update_record(&mut conn, "d1", 1, &confirm_to("t1")).unwrap();
        // Changed content is a new revision: confirmation rides with it.
        let (rev, _) = update_record(&mut conn, "d1", 2, &confirm_to("t1 v2")).unwrap();
        assert_eq!(rev, 3);
        assert_eq!(
            show_record(&conn, "d1", None)
                .unwrap()
                .confirm_state
                .as_deref(),
            Some("confirmed")
        );
        // An ordinary update writes an unconfirmed revision: confirmation is
        // per revision and never inherited.
        let (rev, _) = update_record(&mut conn, "d1", 3, &update_to("t1 v3", "active")).unwrap();
        assert_eq!(rev, 4);
        assert_eq!(
            show_record(&conn, "d1", None)
                .unwrap()
                .confirm_state
                .as_deref(),
            Some("candidate")
        );
        // History keeps each revision's own state.
        let page = history(&conn, "d1", 50, 0, None).unwrap();
        assert_eq!(
            page.revisions[0].confirm_state.as_deref(),
            Some("candidate")
        );
        assert_eq!(
            page.revisions[1].confirm_state.as_deref(),
            Some("confirmed")
        );
        assert_eq!(
            page.revisions[2].confirm_state.as_deref(),
            Some("confirmed")
        );
        assert_eq!(
            page.revisions[3].confirm_state.as_deref(),
            Some("candidate")
        );
        conn.close().unwrap();
    }

    #[test]
    fn provenance_confirm_rejects_lifecycle_combinations() {
        let (home, repo) = setup("confirm-combo");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        create_record(&mut conn, &new_record("d2", "t2")).unwrap();
        let gen = read_generation(&conn).unwrap();
        let mut combined = supersede_to("d2", 1, "replaced");
        combined.confirm = true;
        let err = update_record(&mut conn, "d1", 1, &combined).unwrap_err();
        assert!(err.contains("confirm_state"), "unexpected: {err}");
        let mut combined = retract_to("withdrawn");
        combined.confirm = true;
        assert!(update_record(&mut conn, "d1", 1, &combined).is_err());
        // Rejected requests write nothing and never migrate.
        assert_eq!(read_generation(&conn).unwrap(), gen);
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        conn.close().unwrap();
    }

    #[test]
    fn provenance_relation_confirm_is_idempotent() {
        let (home, repo) = setup("relconfirm");
        let mut conn = open_rw(&home, &repo);
        create_record(&mut conn, &new_record("d1", "t1")).unwrap();
        create_record(&mut conn, &new_record("d2", "t2")).unwrap();
        let gen0 = link_simple(&mut conn, "r1", "d1", 1, "d2", 1, "supports").unwrap();
        // Unconfirmed authored relations on a v1 store carry no state field.
        assert!(relation_by_id(&conn, "r1").unwrap().confirm_state.is_none());
        // Confirmation migrates the store and pins the event to the id.
        let gen1 = link(
            &mut conn,
            "r1",
            "d1",
            1,
            "d2",
            1,
            "supports",
            "tester",
            "related work",
            true,
        )
        .unwrap();
        assert!(gen1 > gen0);
        assert_eq!(
            relation_by_id(&conn, "r1")
                .unwrap()
                .confirm_state
                .as_deref(),
            Some("confirmed")
        );
        let page = related_outbound(&conn, "d1").unwrap();
        assert_eq!(
            page.relations[0].confirm_state.as_deref(),
            Some("confirmed")
        );
        // Repeat confirmation of the identical row: a successful no-op.
        let gen2 = link(
            &mut conn,
            "r1",
            "d1",
            1,
            "d2",
            1,
            "supports",
            "tester",
            "related work",
            true,
        )
        .unwrap();
        assert_eq!(gen2, gen1, "a no-op confirmation must not bump");
        let events: i64 = conn
            .query_row(
                "SELECT count(*) FROM relation_lifecycle WHERE relation_id = 'r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
        // Identical re-link without the request stays the duplicate error,
        // and a different authored shape is never promoted.
        let err = link(
            &mut conn,
            "r1",
            "d1",
            1,
            "d2",
            1,
            "supports",
            "tester",
            "related work",
            false,
        )
        .unwrap_err();
        assert!(err.contains("duplicate relation id"), "unexpected: {err}");
        let err = link(
            &mut conn,
            "r1",
            "d1",
            1,
            "d2",
            1,
            "supports",
            "tester",
            "a different rationale",
            true,
        )
        .unwrap_err();
        assert!(err.contains("duplicate relation id"), "unexpected: {err}");
        // A new relation confirmed on the first write carries the event too.
        link(
            &mut conn,
            "r2",
            "d2",
            1,
            "d1",
            1,
            "relates_to",
            "tester",
            "work",
            true,
        )
        .unwrap();
        assert_eq!(
            relation_by_id(&conn, "r2")
                .unwrap()
                .confirm_state
                .as_deref(),
            Some("confirmed")
        );
        // Retraction does not erase confirmation, and a retracted relation
        // is never revived through re-link confirmation.
        unlink(&mut conn, "r1", "tester", "mistake").unwrap();
        let page = related(
            &conn,
            "d1",
            None,
            RelationDirection::Outbound,
            true,
            10,
            0,
            None,
        )
        .unwrap();
        let r1 = page
            .relations
            .iter()
            .find(|r| r.relation_id == "r1")
            .unwrap();
        assert!(r1.retracted);
        assert_eq!(r1.confirm_state.as_deref(), Some("confirmed"));
        let err = link(
            &mut conn,
            "r1",
            "d1",
            1,
            "d2",
            1,
            "supports",
            "tester",
            "related work",
            true,
        )
        .unwrap_err();
        assert!(err.contains("duplicate relation id"), "unexpected: {err}");
        conn.close().unwrap();
    }
}

// --- G-PORT: canonical JSONL/Markdown port ---------------------------------

#[cfg(test)]
mod port_tests {
    use super::*;
    use crate::knowledge::{
        BenchmarkData, MetricData, NewRecord, SupersededRef, UpdateRecord, ValidatedNew,
        ValidatedUpdate,
    };
    use crate::testutil::tmpdir;

    /// A fresh home with a registered repo entry named `krepo` (both stores in
    /// a round-trip share the name so `meta.repo` compares byte-equal).
    fn setup() -> (PathBuf, RepoEntry) {
        let home = tmpdir("m7-port-home");
        let root = tmpdir("m7-port-root");
        let canonical = crate::paths::canonical_root(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let repo = RepoEntry {
            name: "krepo".to_string(),
            root: canonical,
            added_unix: 1,
        };
        (home, repo)
    }

    fn open(home: &Path, repo: &RepoEntry) -> Connection {
        open_knowledge_at(home, repo, true).unwrap()
    }

    fn user_version_of(conn: &Connection) -> u32 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn new_record(id: &str, kind: &str, title: &str, body: &str) -> ValidatedNew {
        ValidatedNew {
            record: NewRecord {
                id: id.to_string(),
                kind: kind.to_string(),
                title: title.to_string(),
                body: body.to_string(),
                status: "active".to_string(),
                author: "a".to_string(),
            },
            evidence: vec![],
            measurement: None,
        }
    }

    fn update_input(title: &str, body: &str) -> UpdateRecord {
        UpdateRecord {
            title: title.to_string(),
            body: body.to_string(),
            status: "active".to_string(),
            author: "a".to_string(),
            superseded_by: None,
            reason: None,
            confirm: false,
        }
    }

    /// The lifecycle-rich fixture: supersede/confirm/retract record events, a
    /// benchmark measurement, a confirmed and a retracted relation, and the
    /// implicit `supersedes` relation.
    fn rich_store(conn: &mut Connection) {
        create_validated(conn, &new_record("d1", "decision", "First", "body one")).unwrap();
        create_validated(conn, &new_record("d2", "decision", "Second", "body two")).unwrap();
        create_validated(conn, &new_record("d3", "decision", "Third", "body three")).unwrap();
        create_validated(
            conn,
            &ValidatedNew {
                record: NewRecord {
                    id: "b1".to_string(),
                    kind: "benchmark".to_string(),
                    title: "Perf".to_string(),
                    body: "body bench".to_string(),
                    status: "active".to_string(),
                    author: "a".to_string(),
                },
                evidence: vec![],
                measurement: Some(BenchmarkData {
                    method: "m".to_string(),
                    environment: "e".to_string(),
                    sample_count: 3,
                    metrics: vec![
                        MetricData {
                            name: "latency".to_string(),
                            value: 1.5,
                            unit: "ms".to_string(),
                        },
                        MetricData {
                            name: "p99".to_string(),
                            value: 42.0,
                            unit: "ms".to_string(),
                        },
                    ],
                }),
            },
        )
        .unwrap();
        let sup = UpdateRecord {
            title: "First v2".to_string(),
            body: "body one v2".to_string(),
            status: "superseded".to_string(),
            author: "a".to_string(),
            superseded_by: Some(SupersededRef {
                id: "d2".to_string(),
                revision: 1,
            }),
            reason: Some("replaced by d2".to_string()),
            confirm: false,
        };
        update_validated(
            conn,
            "d1",
            1,
            &ValidatedUpdate {
                record: sup,
                evidence: vec![],
                measurement: None,
            },
        )
        .unwrap();
        let mut conf = update_input("Second v2", "body two v2");
        conf.confirm = true;
        update_validated(
            conn,
            "d2",
            1,
            &ValidatedUpdate {
                record: conf,
                evidence: vec![],
                measurement: None,
            },
        )
        .unwrap();
        let ret = UpdateRecord {
            title: "Third v2".to_string(),
            body: "body three v2".to_string(),
            status: "retracted".to_string(),
            author: "a".to_string(),
            superseded_by: None,
            reason: Some("was wrong".to_string()),
            confirm: false,
        };
        update_validated(
            conn,
            "d3",
            1,
            &ValidatedUpdate {
                record: ret,
                evidence: vec![],
                measurement: None,
            },
        )
        .unwrap();
        link(
            conn,
            "r1",
            "d2",
            2,
            "d1",
            2,
            "supports",
            "a",
            "the reason",
            true,
        )
        .unwrap();
        link(
            conn,
            "r2",
            "d3",
            2,
            "d1",
            2,
            "relates_to",
            "a",
            "maybe",
            false,
        )
        .unwrap();
        unlink(conn, "r2", "a", "mistake").unwrap();
    }

    fn dump(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        let cols = stmt.column_count();
        let rows = stmt
            .query_map([], |row| {
                let mut parts = Vec::new();
                for i in 0..cols {
                    let v: rusqlite::types::Value = row.get(i)?;
                    parts.push(format!("{v:?}"));
                }
                Ok(parts.join("\u{1f}"))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn meta_line(records: usize, relations: usize) -> String {
        format!(
            "{{\"type\":\"meta\",\"format\":\"riltra-knowledge\",\"version\":1,\"repo\":\"krepo\",\"records\":{records},\"relations\":{relations}}}"
        )
    }

    fn record_line(id: &str, title: &str, body: &str) -> String {
        format!(
            "{{\"type\":\"record\",\"id\":\"{id}\",\"kind\":\"decision\",\"head_revision\":1,\"status\":\"active\",\
             \"revisions\":[{{\"revision\":1,\"title\":\"{title}\",\"body\":\"{body}\",\"status\":\"active\",\
             \"author\":\"a\",\"created_unix\":10,\"evidence\":[],\"events\":[]}}]}}"
        )
    }

    /// A record whose confirmation rides on the appended revision 2, the only
    /// place a coherent store can hold one.
    fn confirm_event_record() -> String {
        "{\"type\":\"record\",\"id\":\"d9\",\"kind\":\"decision\",\"head_revision\":2,\"status\":\"active\",\
         \"revisions\":[{\"revision\":1,\"title\":\"Nine\",\"body\":\"b9\",\"status\":\"active\",\
         \"author\":\"a\",\"created_unix\":10,\"evidence\":[],\"events\":[]},{\"revision\":2,\"title\":\"Nine\",\
         \"body\":\"b9\",\"status\":\"active\",\"author\":\"a\",\"created_unix\":11,\"evidence\":[],\
         \"events\":[{\"event\":\"confirm\",\"author\":\"a\",\"created_unix\":12}]}]}"
            .to_string()
    }

    #[test]
    fn port_export_is_canonical_and_deterministic() {
        let (home, repo) = setup();
        let mut conn = open(&home, &repo);
        rich_store(&mut conn);
        let a = export_document(&mut conn, ExportFormat::Jsonl).unwrap();
        let b = export_document(&mut conn, ExportFormat::Jsonl).unwrap();
        assert_eq!(
            a.text, b.text,
            "two exports of one store must be byte-identical"
        );
        assert_eq!(a.records, 4);
        assert_eq!(a.relations, 3);
        let mut lines = a.text.split_inclusive('\n');
        assert_eq!(
            lines.next().unwrap(),
            "{\"type\":\"meta\",\"format\":\"riltra-knowledge\",\"version\":1,\"repo\":\"krepo\",\"records\":4,\"relations\":3}\n"
        );
        assert!(!a.text.contains('\r'), "LF newlines only");
        assert!(a.text.ends_with('\n'));
        // Records sort by id, binary: b1 < d1 < d2 < d3.
        let second = a.text.lines().nth(1).unwrap();
        assert!(
            second.starts_with("{\"type\":\"record\",\"id\":\"b1\","),
            "{second}"
        );
        // Lifecycle audit data rides as structured events with original
        // authors, reasons, and target bindings; derived draft fields never do.
        assert!(a
            .text
            .contains("\"target_id\":\"d2\",\"target_revision\":1,\"reason\":\"replaced by d2\""));
        assert!(a.text.contains("\"relation_id\":\"supersedes:d2:1:d1:2\""));
        assert!(a
            .text
            .contains("\"event\":\"retract\",\"reason\":\"was wrong\""));
        assert!(a
            .text
            .contains("\"event\":\"retract\",\"reason\":\"mistake\""));
        assert!(a
            .text
            .contains("\"name\":\"latency\",\"value\":1.5,\"unit\":\"ms\""));
        assert!(
            a.text.find("\"name\":\"latency\"").unwrap() < a.text.find("\"name\":\"p99\"").unwrap(),
            "metrics keep authored order"
        );
        for withdrawn in [
            "superseded_by",
            "confirm_state",
            "retracted_reason",
            "retracted_author",
            "retraction_author",
            "retraction_reason",
        ] {
            assert!(!a.text.contains(withdrawn), "withdrawn field {withdrawn}");
        }
        // Record lines strictly precede relation lines.
        let mut seen_relation = false;
        for line in a.text.lines().skip(1) {
            if line.contains("\"type\":\"relation\"") {
                seen_relation = true;
            } else {
                assert!(!seen_relation, "record line after relation line: {line}");
            }
        }
    }

    #[test]
    fn port_markdown_wrapper_extracts_to_the_same_payload() {
        let (home, repo) = setup();
        let mut conn = open(&home, &repo);
        rich_store(&mut conn);
        let jsonl = export_document(&mut conn, ExportFormat::Jsonl).unwrap();
        let md = export_document(&mut conn, ExportFormat::Markdown).unwrap();
        assert!(md.text.contains("# Riltra knowledge export"));
        assert!(md.text.contains("```json"));
        assert!(md
            .text
            .contains("## d1 — decision (current revision 2, superseded)"));
        assert_eq!(
            extract_import_payload(&md.text).unwrap(),
            jsonl.text,
            "markdown payload extracts back to the canonical JSONL"
        );
        assert_eq!(
            extract_import_payload(&jsonl.text).unwrap(),
            jsonl.text,
            "a bare JSONL file passes through"
        );
        assert_eq!(
            extract_import_payload(&format!("\u{feff}{}", jsonl.text)).unwrap(),
            jsonl.text,
            "a UTF-8 BOM must not derail format detection"
        );

        // A prose body that would open a fence is escaped and cannot leak into
        // the payload region.
        create_validated(
            &mut conn,
            &new_record("e1", "decision", "Esc", "line one\n```json\nline two"),
        )
        .unwrap();
        let md2 = export_document(&mut conn, ExportFormat::Markdown).unwrap();
        assert!(
            md2.text.contains("\\```json"),
            "prose fence must be escaped"
        );
        assert_eq!(
            extract_import_payload(&md2.text).unwrap(),
            export_document(&mut conn, ExportFormat::Jsonl)
                .unwrap()
                .text
        );

        assert!(extract_import_payload("plain prose only\n")
            .unwrap_err()
            .contains("fenced"));
        assert!(extract_import_payload("```json\n{\"a\":1}\n")
            .unwrap_err()
            .contains("unterminated"));
    }

    /// A record line whose single revision carries the given `events` array.
    fn record_line_events(events: &str) -> String {
        format!(
            "{{\"type\":\"record\",\"id\":\"d1\",\"kind\":\"decision\",\"head_revision\":1,\"status\":\"active\",\
             \"revisions\":[{{\"revision\":1,\"title\":\"T\",\"body\":\"B\",\"status\":\"active\",\
             \"author\":\"a\",\"created_unix\":10,\"evidence\":[],\"events\":{events}}}]}}"
        )
    }

    /// A minimal relation line on the fixed d1:1 -> d1:1 tuple.
    fn relation_line(rid: &str, kind: &str, events: &str) -> String {
        format!(
            "{{\"type\":\"relation\",\"relation_id\":\"{rid}\",\"src_id\":\"d1\",\"src_revision\":1,\
             \"dst_id\":\"d1\",\"dst_revision\":1,\"kind\":\"{kind}\",\"author\":\"a\",\"rationale\":\"w\",\
             \"created_unix\":20,\"events\":{events}}}"
        )
    }

    #[test]
    fn port_parse_classifies_malformed_and_corrupt() {
        let meta = meta_line(1, 0);
        let rec = record_line("d1", "T", "B");
        let valid = format!("{meta}\n{rec}\n");
        assert!(parse_import_payload(&valid).is_ok());
        assert!(!parse_import_payload(&valid).unwrap().needs_lifecycle);
        let empty = format!("{}\n", meta_line(0, 0));
        let empty_payload = parse_import_payload(&empty).unwrap();
        assert!(empty_payload.records.is_empty());
        assert!(!empty_payload.needs_lifecycle);
        let with_event = format!("{}\n{}\n{}\n", meta_line(2, 0), rec, confirm_event_record());
        assert!(parse_import_payload(&with_event).unwrap().needs_lifecycle);

        let two_records = format!(
            "{}\n{}\n{}\n",
            meta_line(2, 0),
            record_line("d2", "T2", "B2"),
            rec
        );
        let cases: Vec<(String, &str)> = vec![
            (
                format!("{meta}\n{{\"type\":\"record\",\"id\":\"d1\",\n"),
                "invalid import payload",
            ),
            (
                "{\"type\":\"meta\",\"format\":\"riltra-knowledge\",\"version\":2,\"repo\":\"krepo\",\"records\":0,\"relations\":0}\n".to_string(),
                "invalid import payload",
            ),
            (
                format!("{}\n{{\"type\":\"unknown\"}}\n", meta_line(0, 0)),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"kind\":\"decision\",", "\"kind\":\"decision\",\"extra\":1,")
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"title\":\"T\"", "\"title\":\"T\",\"title\":\"U\"")
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"status\":\"active\"", "\"status\":\"bogus\"")
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"evidence\":[],\"events\":[]", "\"evidence\":[]")
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"evidence\":[],\"events\":[]", "\"events\":[]")
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"kind\":\"decision\"", "\"kind\":\"bogus\"")
                ),
                "invalid import payload",
            ),
            (
                format!("{meta}\n{rec}\n{rec}\n"),
                "corrupt import file",
            ),
            (format!("{}\n{rec}\n", meta_line(2, 0)), "corrupt import file"),
            (
                two_records.replace("\"id\":\"d2\"", "\"id\":\"zz\""),
                "corrupt import file",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"revision\":1,\"title\"", "\"revision\":3,\"title\"")
                ),
                "corrupt import file",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace("\"head_revision\":1", "\"head_revision\":2")
                ),
                "corrupt import file",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace(
                        "\"head_revision\":1,\"status\":\"active\",",
                        "\"head_revision\":1,\"status\":\"archived\","
                    )
                ),
                "corrupt import file",
            ),
            (
                format!("{rec}\n{meta}\n"),
                "corrupt import file",
            ),
            (
                format!(
                    "{}\n",
                    meta.replace("\"format\":\"riltra-knowledge\"", "\"format\":\"other\"")
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    rec.replace(
                        "\"evidence\":[]",
                        "\"evidence\":[{\"path\":\"a.py\",\"byte_start\":5,\"byte_end\":5,\"digest\":\"d\"}]"
                    )
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n{}\n",
                    meta_line(0, 2),
                    relation_line("r1", "relates_to", "[]"),
                    relation_line("r1", "relates_to", "[]")
                ),
                "corrupt import file",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 1),
                    relation_line("r1", "bogus", "[]")
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    "x".repeat(MAX_IMPORT_LINE_BYTES + 1)
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{{\"type\":\"relation\",\"relation_id\":\"r1\",\"src_id\":\"d1\",\"src_revision\":1,\"dst_id\":\"d1\",\"dst_revision\":1,\"kind\":\"relates_to\",\"author\":\"a\",\"rationale\":\"w\",\"created_unix\":20,\"events\":[{{\"event\":\"confirm\",\"author\":\"a\",\"created_unix\":30}},{{\"event\":\"retract\",\"reason\":\"r\",\"author\":\"a\",\"created_unix\":20}}]}}\n",
                    meta_line(1, 1)
                ),
                "corrupt import file",
            ),
            (
                format!(
                    "{}\n{{\"type\":\"relation\",\"relation_id\":\"r1\",\"src_id\":\"d1\",\"src_revision\":1,\"dst_id\":\"d1\",\"dst_revision\":1,\"kind\":\"relates_to\",\"author\":\"a\",\"rationale\":\"w\",\"created_unix\":20,\"events\":[{{\"event\":\"confirm\",\"author\":\"a\",\"created_unix\":20}},{{\"event\":\"confirm\",\"author\":\"a\",\"created_unix\":30}}]}}\n",
                    meta_line(1, 1)
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{{\"type\":\"relation\",\"relation_id\":\"wrong\",\"src_id\":\"d1\",\"src_revision\":1,\"dst_id\":\"d1\",\"dst_revision\":1,\"kind\":\"supersedes\",\"author\":\"a\",\"rationale\":\"w\",\"created_unix\":20,\"events\":[]}}\n",
                    meta_line(1, 1)
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    record_line_events(
                        "[{\"event\":\"retract\",\"reason\":\"r\",\"target_id\":\"d9\",\"target_revision\":1,\"author\":\"a\",\"created_unix\":11}]"
                    )
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    record_line_events(
                        "[{\"event\":\"confirm\",\"reason\":\"r\",\"author\":\"a\",\"created_unix\":11}]"
                    )
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n",
                    meta_line(1, 0),
                    record_line_events(
                        "[{\"event\":\"supersede\",\"target_id\":\"d9\",\"target_revision\":1,\"author\":\"a\",\"created_unix\":11}]"
                    )
                ),
                "invalid import payload",
            ),
            (
                format!(
                    "{}\n{}\n{}\n",
                    meta_line(1, 1),
                    relation_line("r1", "relates_to", "[]"),
                    rec
                ),
                "corrupt import file",
            ),
        ];
        for (text, prefix) in cases {
            let err = parse_import_payload(&text).unwrap_err();
            assert!(err.starts_with(prefix), "expected {prefix}: {err}\n{text}");
        }
    }

    #[test]
    fn port_import_round_trip_preserves_every_audit_row() {
        let (home1, repo1) = setup();
        let mut c1 = open(&home1, &repo1);
        rich_store(&mut c1);
        let doc = export_document(&mut c1, ExportFormat::Jsonl).unwrap();
        let payload = parse_import_payload(&doc.text).unwrap();
        assert!(payload.needs_lifecycle);

        let (home2, repo2) = setup();
        let mut c2 = open(&home2, &repo2);
        let report = import_payload(&mut c2, &payload).unwrap();
        assert_eq!(report.records_inserted, 4);
        assert_eq!(report.revisions_appended, 0);
        assert_eq!(report.relations_inserted, 3);
        assert_eq!(report.records_unchanged, 0);
        assert!(report.generation > 0);
        assert_eq!(report.generation, read_generation(&c2).unwrap());
        let doc2 = export_document(&mut c2, ExportFormat::Jsonl).unwrap();
        assert_eq!(doc2.text, doc.text, "byte-identical re-export");
        for sql in [
            "SELECT id, revision, event, coalesce(target_id,''), coalesce(target_revision,-1), coalesce(reason,''), author, created_unix FROM record_lifecycle ORDER BY id, revision",
            "SELECT relation_id, event, author, created_unix FROM relation_lifecycle ORDER BY relation_id",
            "SELECT relation_id, author, reason, created_unix FROM retractions ORDER BY relation_id",
            "SELECT id, revision, title, status, author, created_unix FROM records ORDER BY id, revision",
            "SELECT relation_id, src_id, src_revision, dst_id, dst_revision, kind, author, rationale, created_unix FROM relations ORDER BY relation_id",
            "SELECT id, revision, method, environment, sample_count FROM measurements ORDER BY id, revision",
            "SELECT id, revision, name, value, unit FROM metric_values ORDER BY id, revision, rowid",
        ] {
            assert_eq!(dump(&c1, sql), dump(&c2, sql), "{sql}");
        }

        let gen = read_generation(&c2).unwrap();
        let again = import_payload(&mut c2, &payload).unwrap();
        assert_eq!(again.records_inserted, 0);
        assert_eq!(again.revisions_appended, 0);
        assert_eq!(again.relations_inserted, 0);
        assert_eq!(again.records_unchanged, 4);
        assert_eq!(again.relations_unchanged, 3);
        assert_eq!(
            again.generation, gen,
            "a no-op import must not bump the generation"
        );
        assert_eq!(read_generation(&c2).unwrap(), gen);
        assert_eq!(
            export_document(&mut c2, ExportFormat::Jsonl).unwrap().text,
            doc.text
        );
    }

    #[test]
    fn port_import_merge_and_conflicts_are_atomic() {
        let (home, repo) = setup();
        let mut conn = open(&home, &repo);
        create_validated(
            &mut conn,
            &new_record("d1", "decision", "First", "body one"),
        )
        .unwrap();
        let v1_text = export_document(&mut conn, ExportFormat::Jsonl)
            .unwrap()
            .text;
        update_validated(
            &mut conn,
            "d1",
            1,
            &ValidatedUpdate {
                record: update_input("First v2", "body two"),
                evidence: vec![],
                measurement: None,
            },
        )
        .unwrap();
        let v2_text = export_document(&mut conn, ExportFormat::Jsonl)
            .unwrap()
            .text;
        let p1 = parse_import_payload(&v1_text).unwrap();
        let p2 = parse_import_payload(&v2_text).unwrap();

        let (home2, repo2) = setup();
        let mut c2 = open(&home2, &repo2);
        import_payload(&mut c2, &p1).unwrap();
        let rep = import_payload(&mut c2, &p2).unwrap();
        assert_eq!(rep.revisions_appended, 1);
        assert_eq!(rep.records_inserted, 0);
        assert_eq!(
            export_document(&mut c2, ExportFormat::Jsonl).unwrap().text,
            v2_text
        );
        let rep2 = import_payload(&mut c2, &p1).unwrap();
        assert_eq!(rep2.records_inserted, 0);
        assert_eq!(rep2.records_unchanged, 1);
        assert_eq!(
            export_document(&mut c2, ExportFormat::Jsonl).unwrap().text,
            v2_text,
            "a subset import must not shrink the store"
        );

        // A mixed payload (new d9 plus a divergent d1) writes neither.
        let div = v1_text.replace("\"title\":\"First\"", "\"title\":\"Changed\"");
        let mixed = format!(
            "{}\n{}\n{}\n",
            div.lines()
                .next()
                .unwrap()
                .replace("\"records\":1", "\"records\":2"),
            div.lines().nth(1).unwrap(),
            record_line("d9", "Nine", "b9")
        );
        let pm = parse_import_payload(&mixed).unwrap();
        let gen = read_generation(&c2).unwrap();
        let err = import_payload(&mut c2, &pm).unwrap_err();
        assert!(err.starts_with("import conflict:"), "{err}");
        assert_eq!(read_generation(&c2).unwrap(), gen);
        assert_eq!(
            export_document(&mut c2, ExportFormat::Jsonl).unwrap().text,
            v2_text
        );
        let n: i64 = c2
            .query_row("SELECT count(*) FROM records WHERE id = 'd9'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "a conflicting payload must not partially write");

        // Two live relations sharing one tuple inside a single payload: the
        // second is refused before either is written.
        let two_live = format!(
            "{}\n{}\n{}\n",
            meta_line(0, 2),
            relation_line("r8", "relates_to", "[]"),
            relation_line("r9", "relates_to", "[]")
        );
        let pt = parse_import_payload(&two_live).unwrap();
        let gen = read_generation(&c2).unwrap();
        let err = import_payload(&mut c2, &pt).unwrap_err();
        assert!(err.starts_with("import conflict:"), "{err}");
        assert_eq!(read_generation(&c2).unwrap(), gen);
        assert_eq!(
            export_document(&mut c2, ExportFormat::Jsonl).unwrap().text,
            v2_text
        );

        // A payload that renames the record kind is divergence even when every
        // revision byte-matches: kind is record-level content.
        let rekind = v1_text.replace("\"kind\":\"decision\"", "\"kind\":\"constraint\"");
        let pk = parse_import_payload(&rekind).unwrap();
        let err = import_payload(&mut c2, &pk).unwrap_err();
        assert!(err.starts_with("import conflict:"), "{err}");

        // Audit-only divergence (a re-minted timestamp) is divergence too.
        let ts: i64 = c2
            .query_row(
                "SELECT created_unix FROM records WHERE id = 'd1' AND revision = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let rescheduled = v1_text.replace(
            &format!("\"created_unix\":{ts}"),
            &format!("\"created_unix\":{}", ts + 1),
        );
        assert_ne!(rescheduled, v1_text, "audit timestamp must appear verbatim");
        let pa = parse_import_payload(&rescheduled).unwrap();
        let err = import_payload(&mut c2, &pa).unwrap_err();
        assert!(err.starts_with("import conflict:"), "{err}");
        assert_eq!(
            export_document(&mut c2, ExportFormat::Jsonl).unwrap().text,
            v2_text,
            "conflicting imports must not write"
        );
    }

    #[test]
    fn port_import_v1_migration_rules() {
        let (home, repo) = setup();
        let mut conn = open(&home, &repo);
        create_validated(
            &mut conn,
            &new_record("d1", "decision", "First", "body one"),
        )
        .unwrap();
        assert_eq!(user_version_of(&conn), 1);
        let base = export_document(&mut conn, ExportFormat::Jsonl)
            .unwrap()
            .text;
        let p = parse_import_payload(&base).unwrap();
        assert!(!p.needs_lifecycle);
        let rep = import_payload(&mut conn, &p).unwrap();
        assert_eq!(rep.records_inserted, 0);
        assert_eq!(rep.records_unchanged, 1);
        assert_eq!(
            user_version_of(&conn),
            1,
            "a Foundation-only payload must not migrate a v1 store"
        );

        let g0 = read_generation(&conn).unwrap();
        let ev = format!("{}\n{}\n", meta_line(1, 0), confirm_event_record());
        let pev = parse_import_payload(&ev).unwrap();
        assert!(pev.needs_lifecycle);
        let rep2 = import_payload(&mut conn, &pev).unwrap();
        assert_eq!(rep2.records_inserted, 1);
        assert_eq!(
            read_generation(&conn).unwrap(),
            g0 + 1,
            "a changed import bumps the generation exactly once"
        );
        assert_eq!(
            user_version_of(&conn),
            2,
            "an event-carrying payload migrates"
        );

        // A conflicting payload against a v1 store rolls the migration back.
        let (home3, repo3) = setup();
        let mut c3 = open(&home3, &repo3);
        create_validated(&mut c3, &new_record("d1", "decision", "First", "body one")).unwrap();
        let bad = format!(
            "{}\n{}\n{}\n",
            base.lines()
                .next()
                .unwrap()
                .replace("\"records\":1", "\"records\":2"),
            base.lines()
                .nth(1)
                .unwrap()
                .replace("\"title\":\"First\"", "\"title\":\"Changed\""),
            confirm_event_record()
        );
        let pbad = parse_import_payload(&bad).unwrap();
        assert!(pbad.needs_lifecycle);
        let err = import_payload(&mut c3, &pbad).unwrap_err();
        assert!(err.starts_with("import conflict:"), "{err}");
        assert_eq!(
            user_version_of(&c3),
            1,
            "a failed import must roll back the migration"
        );
        let n: i64 = c3
            .query_row("SELECT count(*) FROM records WHERE id = 'd9'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    /// A record line with full control over kind, status and revision events.
    fn record_line_full(id: &str, kind: &str, status: &str, events: &str) -> String {
        format!(
            "{{\"type\":\"record\",\"id\":\"{id}\",\"kind\":\"{kind}\",\"head_revision\":1,\
             \"status\":\"{status}\",\"revisions\":[{{\"revision\":1,\"title\":\"T\",\"body\":\"B\",\
             \"status\":\"{status}\",\"author\":\"a\",\"created_unix\":10,\"evidence\":[],\
             \"events\":{events}}}]}}"
        )
    }

    /// A record whose revision 2 carries the lifecycle state; revision 1 is
    /// the plain creation every store starts from.
    fn two_rev_record(id: &str, status: &str, events: &str) -> String {
        format!(
            "{{\"type\":\"record\",\"id\":\"{id}\",\"kind\":\"decision\",\"head_revision\":2,\
             \"status\":\"{status}\",\"revisions\":[{{\"revision\":1,\"title\":\"T\",\"body\":\"B\",\
             \"status\":\"active\",\"author\":\"a\",\"created_unix\":10,\"evidence\":[],\"events\":[]}},\
             {{\"revision\":2,\"title\":\"T\",\"body\":\"B\",\"status\":\"{status}\",\"author\":\"a\",\
             \"created_unix\":11,\"evidence\":[],\"events\":{events}}}]}}"
        )
    }

    /// A relation line between two revision-1 records.
    fn rel_line(rid: &str, src_id: &str, dst_id: &str, kind: &str) -> String {
        rel_line_rev(rid, src_id, 1, dst_id, 1, kind)
    }

    /// A relation line between two explicit revisions.
    fn rel_line_rev(
        rid: &str,
        src_id: &str,
        src_revision: u64,
        dst_id: &str,
        dst_revision: u64,
        kind: &str,
    ) -> String {
        format!(
            "{{\"type\":\"relation\",\"relation_id\":\"{rid}\",\"src_id\":\"{src_id}\",\
             \"src_revision\":{src_revision},\"dst_id\":\"{dst_id}\",\"dst_revision\":{dst_revision},\
             \"kind\":\"{kind}\",\"author\":\"a\",\"rationale\":\"r\",\"created_unix\":12,\"events\":[]}}"
        )
    }

    #[test]
    fn port_export_reports_generation_from_one_snapshot() {
        use std::thread;

        let (home, repo) = setup();
        let mut conn = open(&home, &repo);
        create_validated(&mut conn, &new_record("d1", "decision", "EVEN", "body one")).unwrap();
        let g0 = read_generation(&conn).unwrap();
        assert_eq!(g0 % 2, 1, "the fixture starts on an odd generation");

        // A second connection to the same store plays the concurrent writer:
        // every update appends one revision and bumps the generation by one,
        // so in any single committed state the head title and the generation
        // parity are locked together ("EVEN" iff the generation is odd).
        let writer = open(&home, &repo);
        let handle = thread::spawn(move || {
            let mut w = writer;
            let mut expected = 1u64;
            for i in 0..250 {
                let title = if i % 2 == 0 { "ODD" } else { "EVEN" };
                let input = update_input(title, "body one");
                if update_validated(
                    &mut w,
                    "d1",
                    expected,
                    &ValidatedUpdate {
                        record: input,
                        evidence: vec![],
                        measurement: None,
                    },
                )
                .is_ok()
                {
                    expected += 1;
                }
            }
        });

        let mut exports = 0usize;
        loop {
            let doc = export_document(&mut conn, ExportFormat::Jsonl).unwrap();
            let generation = doc.generation;
            let payload = parse_import_payload(&doc.text).unwrap();
            let head = payload
                .records
                .iter()
                .find(|r| r.id == "d1")
                .and_then(|r| r.revisions.last())
                .expect("the export always carries d1");
            assert_eq!(
                head.title == "EVEN",
                generation % 2 == 1,
                "exported head {:?} is inconsistent with reported generation {generation}",
                head.title
            );
            exports += 1;
            if exports >= 250 {
                break;
            }
        }
        handle.join().unwrap();
        conn.close().unwrap();
    }

    #[test]
    fn port_import_rejects_status_event_disagreement() {
        let meta = meta_line(1, 0);
        let supersede_event = r#"[{"event":"supersede","target_id":"d2","target_revision":1,"reason":"r","author":"a","created_unix":11}]"#;
        let retract_event = r#"[{"event":"retract","reason":"r","author":"a","created_unix":11}]"#;
        let confirm_event = r#"[{"event":"confirm","author":"a","created_unix":11}]"#;
        // Every status/event pairing except the four the store can author;
        // the disagreement sits on an appended revision 2 because revision 1
        // is creation-only.
        let cases = [
            ("superseded", "[]"),
            ("retracted", "[]"),
            ("active", retract_event),
            ("active", supersede_event),
            ("archived", supersede_event),
            ("superseded", confirm_event),
            ("retracted", confirm_event),
            ("retracted", supersede_event),
        ];
        for (status, events) in cases {
            let text = format!("{meta}\n{}\n", two_rev_record("d1", status, events));
            let err = parse_import_payload(&text).unwrap_err();
            assert!(
                err.starts_with("invalid import payload"),
                "status {status} with events {events}: {err}"
            );
        }
    }

    #[test]
    fn port_import_accepts_coherent_status_event_pairs() {
        let meta = meta_line(1, 0);
        // Revision 1 is creation-only: active or archived with no event.
        let cases = [("archived", "[]"), ("active", "[]")];
        for (status, events) in cases {
            let text = format!(
                "{meta}\n{}\n",
                record_line_full("d1", "decision", status, events)
            );
            assert!(
                parse_import_payload(&text).is_ok(),
                "coherent status {status} with events {events} must parse"
            );
        }
        // Lifecycle states live on appended revisions.
        let lifecycle = [
            (
                "retracted",
                r#"[{"event":"retract","reason":"r","author":"a","created_unix":11}]"#,
            ),
            (
                "active",
                r#"[{"event":"confirm","author":"a","created_unix":11}]"#,
            ),
        ];
        for (status, events) in lifecycle {
            let text = format!("{meta}\n{}\n", two_rev_record("d1", status, events));
            assert!(
                parse_import_payload(&text).is_ok(),
                "coherent status {status} on revision 2 must parse"
            );
        }
    }

    #[test]
    fn port_import_rejects_first_revision_lifecycle_states() {
        let meta = meta_line(1, 0);
        // Native creation is the only author of revision 1 and writes only
        // active or archived with no event: every other first revision is
        // not a faithful export of any coherent store.
        let retract = r#"[{"event":"retract","reason":"r","author":"a","created_unix":11}]"#;
        let confirm = r#"[{"event":"confirm","author":"a","created_unix":11}]"#;
        let rejected = [
            ("retracted", retract),
            ("active", confirm),
            ("superseded", "[]"),
            ("retracted", "[]"),
        ];
        for (status, events) in rejected {
            let text = format!(
                "{meta}\n{}\n",
                record_line_full("d1", "decision", status, events)
            );
            let err = parse_import_payload(&text).unwrap_err();
            assert!(
                err.starts_with("invalid import payload"),
                "revision 1 {status} with events {events}: {err}"
            );
            assert!(err.contains("revision 1"), "{err}");
        }
        // The creation-producible first revisions and every appended
        // lifecycle state still parse.
        for status in ["active", "archived"] {
            let text = format!(
                "{meta}\n{}\n",
                record_line_full("d1", "decision", status, "[]")
            );
            assert!(parse_import_payload(&text).is_ok(), "{status}");
        }
        for (status, events) in [("retracted", retract), ("active", confirm)] {
            let text = format!("{meta}\n{}\n", two_rev_record("d1", status, events));
            assert!(
                parse_import_payload(&text).is_ok(),
                "revision 2 {status} with events {events} must parse"
            );
        }
    }

    #[test]
    fn port_import_rejects_kind_measurement_disagreement() {
        let meta = meta_line(1, 0);
        let measurement = r#"{"method":"m","environment":"e","sample_count":3,"metrics":[{"name":"latency","value":1.5,"unit":"ms"}]}"#;
        // A benchmark revision without measurement.
        let bench = record_line_full("b1", "benchmark", "active", "[]");
        let err = parse_import_payload(&format!("{meta}\n{bench}\n")).unwrap_err();
        assert!(err.starts_with("invalid import payload"), "{err}");
        // A non-benchmark revision carrying measurement.
        let mut decision = record_line_full("d1", "decision", "active", "[]");
        decision = decision.replacen(
            "\"events\":[]",
            &format!("\"events\":[],\"measurement\":{measurement}"),
            1,
        );
        let err = parse_import_payload(&format!("{meta}\n{decision}\n")).unwrap_err();
        assert!(err.starts_with("invalid import payload"), "{err}");
        // The coherent pairings parse.
        let bench_full = record_line_full("b1", "benchmark", "active", "[]").replacen(
            "\"events\":[]",
            &format!("\"events\":[],\"measurement\":{measurement}"),
            1,
        );
        assert!(
            parse_import_payload(&format!("{meta}\n{bench_full}\n")).is_ok(),
            "benchmark with measurement must parse"
        );
        assert!(
            parse_import_payload(&format!(
                "{meta}\n{}\n",
                record_line_full("d1", "decision", "active", "[]")
            ))
            .is_ok(),
            "decision without measurement must parse"
        );
    }

    #[test]
    fn port_import_rejects_supersede_event_relation_disagreement() {
        let meta2 = meta_line(2, 1);
        let d2 = record_line_full("d2", "decision", "active", "[]");
        // The superseded state sits on an appended revision 2.
        let d1_sup = two_rev_record(
            "d1",
            "superseded",
            r#"[{"event":"supersede","target_id":"d2","target_revision":1,"reason":"r","author":"a","created_unix":11}]"#,
        );
        // The event without its revision-pinned implicit relation.
        let err =
            parse_import_payload(&format!("{}\n{d1_sup}\n{d2}\n", meta_line(2, 0))).unwrap_err();
        assert!(err.starts_with("invalid import payload"), "{err}");
        // A caller-style relation id is already refused per line by the
        // store-derived formula check (exit 2), before any agreement runs.
        let err = parse_import_payload(&format!(
            "{meta2}\n{d1_sup}\n{d2}\n{}\n",
            rel_line("sr1", "d2", "d1", "supersedes")
        ))
        .unwrap_err();
        assert!(err.starts_with("invalid import payload"), "{err}");
        // The relation without its event.
        let d1_plain = record_line_full("d1", "decision", "active", "[]");
        let err = parse_import_payload(&format!(
            "{meta2}\n{d1_plain}\n{d2}\n{}\n",
            rel_line("supersedes:d2:1:d1:1", "d2", "d1", "supersedes")
        ))
        .unwrap_err();
        assert!(err.starts_with("invalid import payload"), "{err}");
        // The coherent pair parses: d1 revision 2 superseded by d2 revision 1.
        let ok = parse_import_payload(&format!(
            "{meta2}\n{d1_sup}\n{d2}\n{}\n",
            rel_line_rev("supersedes:d2:1:d1:2", "d2", 1, "d1", 2, "supersedes")
        ));
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn port_import_rejects_supersedes_cycles() {
        let supersede_event = |target: &str, target_revision: u64| {
            format!(
                r#"[{{"event":"supersede","target_id":"{target}","target_revision":{target_revision},"reason":"r","author":"a","created_unix":11}}]"#
            )
        };
        // Mutual replacement across revisions: d1 rev2 is replaced by d2 rev1
        // and d2 rev2 by d1 rev2. The store refuses that record-level cycle,
        // so the payload must be refused too (revision 1 stays creation-only,
        // so the transitions ride on the appended revision 2).
        let text = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            meta_line(2, 2),
            two_rev_record("d1", "superseded", &supersede_event("d2", 1)),
            two_rev_record("d2", "superseded", &supersede_event("d1", 2)),
            rel_line_rev("supersedes:d1:2:d2:2", "d1", 2, "d2", 2, "supersedes"),
            rel_line_rev("supersedes:d2:1:d1:2", "d2", 1, "d1", 2, "supersedes")
        );
        let err = parse_import_payload(&text).unwrap_err();
        assert!(err.starts_with("invalid import payload"), "{err}");
        // Self replacement.
        let meta1 = meta_line(1, 1);
        let d1_self = two_rev_record("d1", "superseded", &supersede_event("d1", 2));
        let text = format!(
            "{meta1}\n{d1_self}\n{}\n",
            rel_line_rev("supersedes:d1:2:d1:2", "d1", 2, "d1", 2, "supersedes")
        );
        let err = parse_import_payload(&text).unwrap_err();
        assert!(err.starts_with("invalid import payload"), "{err}");
    }
}
