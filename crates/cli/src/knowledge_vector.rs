// SPDX-License-Identifier: Apache-2.0

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use futures_util::TryStreamExt;
use lancedb::arrow::{
    arrow_array::{
        types::Float32Type, Array, BooleanArray, FixedSizeListArray, Float32Array, RecordBatch,
        StringArray, UInt64Array,
    },
    arrow_schema::{DataType, Field, Schema},
};
use lancedb::query::{ColumnOrdering, ExecutableQuery, QueryBase, Select};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const VECTOR_HOME: &str = "v";
const LOCK_FILE: &str = "LOCK";
const TABLE_NAME: &str = "v";
const VECTOR_DIMENSIONS: usize = 384;
const MANIFEST_VERSION: u32 = 1;
// LanceDB creates 56-character data filenames below <table>.lance/data. Keep
// a conservative budget below the observed failing paths in this backend;
// this is not a claim about the Windows kernel's MAX_PATH setting.
#[cfg(windows)]
const WINDOWS_LANCE_PATH_BUDGET: usize = 260;
#[cfg(windows)]
const LANCE_DATA_FILE_NAME_BUDGET: &str =
    "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    sequence: u64,
    repo: String,
    root: String,
    source_generation: u64,
    corpus_fingerprint: String,
    model_fingerprint: String,
    rows: usize,
    table_digest: String,
}

#[derive(Debug)]
enum ManifestReadError {
    Path(String),
    Io(String),
    Corrupt(String),
}

impl ManifestReadError {
    fn message(&self) -> &str {
        match self {
            Self::Path(message) | Self::Io(message) | Self::Corrupt(message) => message,
        }
    }
}

#[derive(Debug)]
struct CorpusRecord {
    id: String,
    revision: u64,
    kind: String,
    title: String,
    body: String,
    content_digest: String,
}

struct CorpusSnapshot {
    generation: u64,
    fingerprint: String,
    row_count: usize,
    records: Vec<CorpusRecord>,
}

#[derive(Debug)]
struct VectorPaths {
    root: PathBuf,
    repo: PathBuf,
    lock: PathBuf,
}

#[derive(Debug)]
struct BuildSummary {
    sequence: u64,
    source_generation: u64,
    rows: usize,
    rebuilt: bool,
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn content_digest(kind: &str, title: &str, body: &str) -> String {
    let mut hash = Sha256::new();
    for field in [kind.as_bytes(), title.as_bytes(), body.as_bytes()] {
        hash_field(&mut hash, field);
    }
    format!("{:x}", hash.finalize())
}

fn checked_home(home: &Path) -> Result<PathBuf, String> {
    let canonical = home
        .canonicalize()
        .map_err(|e| format!("cannot resolve RILTRA_HOME '{}': {e}", home.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "RILTRA_HOME is not a directory: '{}'",
            home.display()
        ));
    }
    Ok(canonical)
}

fn checked_root(repo: &crate::registry::RepoEntry) -> Result<String, String> {
    if !crate::paths::validate_name(&repo.name) {
        return Err(format!("invalid repository name: '{}'", repo.name));
    }
    let canonical = crate::paths::canonical_root(Path::new(&repo.root))
        .map_err(|e| format!("cannot resolve repository root '{}': {e}", repo.root))?;
    let root = canonical.to_string_lossy().into_owned();
    if root != repo.root {
        return Err(format!(
            "registered root moved or retargeted (record '{}')",
            repo.root
        ));
    }
    Ok(root)
}

fn check_dir(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!(
            "cannot inspect vector path '{}': {e}",
            path.display()
        )),
        Ok(md) if md.file_type().is_symlink() => Err(format!(
            "vector path escapes via link: '{}'",
            path.display()
        )),
        Ok(md) if !md.is_dir() => Err(format!(
            "vector path is not a directory: '{}'",
            path.display()
        )),
        Ok(_) => {
            let canonical = path
                .canonicalize()
                .map_err(|e| format!("cannot resolve vector path '{}': {e}", path.display()))?;
            if canonical != path {
                return Err(format!("vector path retargeted: '{}'", path.display()));
            }
            Ok(true)
        }
    }
}

fn ensure_dir(path: &Path) -> Result<(), String> {
    if check_dir(path)? {
        return Ok(());
    }
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if check_dir(path)? {
                return Ok(());
            }
            return Err(format!(
                "vector directory appeared but is unavailable: '{}'",
                path.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot create vector directory '{}': {error}",
                path.display()
            ))
        }
    }
    if !check_dir(path)? {
        return Err(format!(
            "vector directory disappeared: '{}'",
            path.display()
        ));
    }
    Ok(())
}

fn vector_paths(home: &Path, repo: &crate::registry::RepoEntry) -> Result<VectorPaths, String> {
    let home = checked_home(home)?;
    let root = home.join(VECTOR_HOME);
    check_dir(&root)?;
    let repo_path = root.join(hex_digest(repo.name.as_bytes()));
    if check_dir(&root)? {
        check_dir(&repo_path)?;
    } else if repo_path.exists() {
        return Err(format!(
            "vector path escapes RILTRA_HOME: '{}'",
            repo_path.display()
        ));
    }
    Ok(VectorPaths {
        lock: repo_path.join(LOCK_FILE),
        root,
        repo: repo_path,
    })
}

fn check_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!(
            "cannot inspect vector file '{}': {e}",
            path.display()
        )),
        Ok(md) if md.file_type().is_symlink() => Err(format!(
            "vector file escapes via link: '{}'",
            path.display()
        )),
        Ok(md) if !md.is_file() => Err(format!("vector path is not a file: '{}'", path.display())),
        Ok(_) => {
            let canonical = path
                .canonicalize()
                .map_err(|e| format!("cannot resolve vector file '{}': {e}", path.display()))?;
            if canonical != path {
                return Err(format!("vector file retargeted: '{}'", path.display()));
            }
            Ok(true)
        }
    }
}

fn open_lock(paths: &VectorPaths, exclusive: bool, create: bool) -> Result<Option<File>, String> {
    if !check_dir(&paths.root)? || !check_dir(&paths.repo)? {
        return Ok(None);
    }
    if check_file(&paths.lock)? {
        let file = if exclusive {
            OpenOptions::new().read(true).write(true).open(&paths.lock)
        } else {
            File::open(&paths.lock)
        }
        .map_err(|e| format!("cannot open vector lock '{}': {e}", paths.lock.display()))?;
        check_file(&paths.lock)?;
        let canonical = paths
            .lock
            .canonicalize()
            .map_err(|e| format!("cannot resolve vector lock '{}': {e}", paths.lock.display()))?;
        if canonical != paths.lock {
            return Err(format!(
                "vector lock retargeted: '{}'",
                paths.lock.display()
            ));
        }
        if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        }
        .map_err(|e| format!("cannot lock vector state: {e}"))?;
        if let Err(e) = check_file(&paths.lock) {
            let _ = file.unlock();
            return Err(e);
        }
        return Ok(Some(file));
    }
    if !create {
        return Ok(None);
    }
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&paths.lock)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return open_lock(paths, exclusive, false)
        }
        Err(e) => {
            return Err(format!(
                "cannot create vector lock '{}': {e}",
                paths.lock.display()
            ))
        }
    };
    if let Err(e) = check_file(&paths.lock) {
        drop(file);
        return Err(e);
    }
    if exclusive {
        file.lock()
    } else {
        file.lock_shared()
    }
    .map_err(|e| format!("cannot lock vector state: {e}"))?;
    Ok(Some(file))
}

fn read_corpus(
    home: &Path,
    repo: &crate::registry::RepoEntry,
    collect_records: bool,
) -> Result<Option<CorpusSnapshot>, String> {
    let root = checked_root(repo)?;
    let home = checked_home(home)?;
    let knowledge_dir = home.join("knowledge");
    if !check_dir(&knowledge_dir)? {
        return Ok(None);
    }
    let db_path = crate::knowledge_store::knowledge_db_path(&home, &repo.name);
    if crate::knowledge_store_absent(&db_path)? {
        return Ok(None);
    }
    let mut conn = crate::knowledge_store::open_knowledge_at(&home, repo, false)?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("cannot begin knowledge snapshot: {e}"))?;
    let snapshot = read_corpus_tx(&tx, repo, Path::new(&root), collect_records)?;
    tx.commit()
        .map_err(|e| format!("cannot finish knowledge snapshot: {e}"))?;
    Ok(Some(snapshot))
}

fn read_corpus_tx(
    tx: &rusqlite::Transaction<'_>,
    repo: &crate::registry::RepoEntry,
    root: &Path,
    collect_records: bool,
) -> Result<CorpusSnapshot, String> {
    crate::knowledge_store::verify_store(tx, repo, root)?;
    let generation = crate::knowledge_store::read_generation(tx)?;
    let mut statement = tx
        .prepare(
            "SELECT r.id, r.revision, r.kind, r.title, r.body
             FROM heads h JOIN records r
               ON r.id = h.id AND r.revision = h.current_revision
             WHERE r.status = 'active'
             ORDER BY r.id COLLATE BINARY",
        )
        .map_err(|e| format!("cannot read active knowledge heads: {e}"))?;
    let mut rows = statement
        .query([])
        .map_err(|e| format!("cannot query active knowledge heads: {e}"))?;

    let mut fingerprint = Sha256::new();
    let mut records = Vec::new();
    let mut row_count = 0usize;
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("cannot decode active knowledge heads: {e}"))?
    {
        let id = row
            .get::<_, String>(0)
            .map_err(|e| format!("cannot decode active knowledge heads: {e}"))?;
        let revision = row
            .get::<_, i64>(1)
            .map_err(|e| format!("cannot decode active knowledge heads: {e}"))?;
        let kind = row
            .get::<_, String>(2)
            .map_err(|e| format!("cannot decode active knowledge heads: {e}"))?;
        let title = row
            .get::<_, String>(3)
            .map_err(|e| format!("cannot decode active knowledge heads: {e}"))?;
        let body = row
            .get::<_, String>(4)
            .map_err(|e| format!("cannot decode active knowledge heads: {e}"))?;
        let revision = u64::try_from(revision)
            .map_err(|_| format!("corrupt knowledge store: invalid head revision for '{id}'"))?;
        let digest = content_digest(&kind, &title, &body);
        hash_field(&mut fingerprint, id.as_bytes());
        hash_field(&mut fingerprint, &revision.to_be_bytes());
        hash_field(&mut fingerprint, digest.as_bytes());
        if collect_records {
            records.push(CorpusRecord {
                id,
                revision,
                kind,
                title,
                body,
                content_digest: digest,
            });
        }
        row_count += 1;
    }
    drop(rows);
    drop(statement);
    hash_field(&mut fingerprint, &(row_count as u64).to_be_bytes());
    Ok(CorpusSnapshot {
        generation,
        fingerprint: format!("{:x}", fingerprint.finalize()),
        row_count,
        records,
    })
}

fn generation_name(sequence: u64) -> String {
    format!("gen-{sequence:020}")
}

fn manifest_name(sequence: u64) -> String {
    format!("manifest-{sequence:020}.json")
}

fn sequence_from_name(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let digits = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn latest_sequence(repo_dir: &Path) -> Result<u64, String> {
    let mut latest = 0;
    for entry in fs::read_dir(repo_dir)
        .map_err(|e| format!("cannot list vector state '{}': {e}", repo_dir.display()))?
    {
        let entry = entry.map_err(|e| format!("cannot list vector state: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        latest = latest.max(
            sequence_from_name(name, "manifest-", ".json")
                .or_else(|| sequence_from_name(name, "gen-", ""))
                .unwrap_or(0),
        );
    }
    latest
        .checked_add(1)
        .ok_or_else(|| "vector generation sequence exhausted".to_string())
}

fn read_current_manifest(repo_dir: &Path) -> Result<Option<Manifest>, ManifestReadError> {
    let mut current: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(repo_dir).map_err(|e| {
        ManifestReadError::Io(format!(
            "cannot list vector state '{}': {e}",
            repo_dir.display()
        ))
    })? {
        let entry =
            entry.map_err(|e| ManifestReadError::Io(format!("cannot list vector state: {e}")))?;
        let Some(sequence) = entry
            .file_name()
            .to_str()
            .and_then(|name| sequence_from_name(name, "manifest-", ".json"))
        else {
            continue;
        };
        let path = entry.path();
        if !check_file(&path).map_err(ManifestReadError::Path)? {
            return Err(ManifestReadError::Path(format!(
                "vector manifest is not a file: '{}'",
                path.display()
            )));
        }
        if current
            .as_ref()
            .is_none_or(|(latest, _)| sequence > *latest)
        {
            current = Some((sequence, path));
        }
    }
    let Some((sequence, path)) = current else {
        return Ok(None);
    };
    let bytes = fs::read(&path).map_err(|e| {
        ManifestReadError::Io(format!(
            "cannot read vector manifest '{}': {e}",
            path.display()
        ))
    })?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| ManifestReadError::Corrupt(format!("corrupt vector manifest: {e}")))?;
    if manifest.version != MANIFEST_VERSION || manifest.sequence != sequence {
        return Err(ManifestReadError::Corrupt(
            "vector manifest version or sequence mismatch".into(),
        ));
    }
    Ok(Some(manifest))
}

fn build_current_manifest(repo_dir: &Path) -> Result<Option<Manifest>, String> {
    match read_current_manifest(repo_dir) {
        Ok(current) => Ok(current),
        Err(ManifestReadError::Corrupt(_)) => Ok(None),
        Err(ManifestReadError::Path(message) | ManifestReadError::Io(message)) => Err(message),
    }
}

fn next_stage_name() -> String {
    format!(
        "s{:x}-{:x}",
        std::process::id(),
        NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
    )
}

fn check_lance_path_budget(stage: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let data_file = stage
            .join(format!("{TABLE_NAME}.lance"))
            .join("data")
            .join(LANCE_DATA_FILE_NAME_BUDGET);
        let units = data_file.as_os_str().encode_wide().count();
        if units > WINDOWS_LANCE_PATH_BUDGET {
            return Err(format!(
                "vector storage path exceeds the supported Windows path budget ({} UTF-16 code units; limit {})",
                units, WINDOWS_LANCE_PATH_BUDGET
            ));
        }
    }
    #[cfg(not(windows))]
    let _ = stage;
    Ok(())
}

fn validate_vectors(vectors: &[(Vec<f32>, bool)]) -> Result<(), String> {
    for (vector, _) in vectors {
        if vector.len() != VECTOR_DIMENSIONS || vector.iter().any(|value| !value.is_finite()) {
            return Err("embedding model returned an invalid vector".into());
        }
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if !norm.is_finite() || norm == 0.0 || (norm - 1.0).abs() > 0.001 {
            return Err("embedding model returned a non-normalized vector".into());
        }
    }
    Ok(())
}

struct VectorDigestRow<'a> {
    repo: &'a str,
    root: &'a str,
    id: &'a str,
    revision: u64,
    kind: &'a str,
    content_digest: &'a str,
    model_fingerprint: &'a str,
    truncated: bool,
    vector: &'a [f32],
}

fn hash_vector_row(hash: &mut Sha256, row: VectorDigestRow<'_>) {
    for field in [
        row.repo.as_bytes(),
        row.root.as_bytes(),
        row.id.as_bytes(),
        &row.revision.to_be_bytes(),
        row.kind.as_bytes(),
        row.content_digest.as_bytes(),
        row.model_fingerprint.as_bytes(),
        &[u8::from(row.truncated)],
    ] {
        hash_field(hash, field);
    }
    let mut vector_bytes = Vec::with_capacity(std::mem::size_of_val(row.vector));
    for value in row.vector {
        vector_bytes.extend_from_slice(&value.to_bits().to_be_bytes());
    }
    hash_field(hash, &vector_bytes);
}

fn expected_table_digest(
    repo: &crate::registry::RepoEntry,
    root: &str,
    model_fingerprint: &str,
    records: &[CorpusRecord],
    vectors: &[(Vec<f32>, bool)],
) -> Result<String, String> {
    if records.len() != vectors.len() {
        return Err("vector row count does not match the active corpus".into());
    }
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"riltra-vector-table-v1");
    for (record, (vector, truncated)) in records.iter().zip(vectors) {
        hash_vector_row(
            &mut hash,
            VectorDigestRow {
                repo: &repo.name,
                root,
                id: &record.id,
                revision: record.revision,
                kind: &record.kind,
                content_digest: &record.content_digest,
                model_fingerprint,
                truncated: *truncated,
                vector,
            },
        );
    }
    hash_field(&mut hash, &(records.len() as u64).to_be_bytes());
    Ok(format!("{:x}", hash.finalize()))
}

fn lance_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("repo", DataType::Utf8, false),
        Field::new("root", DataType::Utf8, false),
        Field::new("id", DataType::Utf8, false),
        Field::new("revision", DataType::UInt64, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("content_digest", DataType::Utf8, false),
        Field::new("model_fingerprint", DataType::Utf8, false),
        Field::new("embedding_truncated", DataType::Boolean, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                VECTOR_DIMENSIONS as i32,
            ),
            false,
        ),
    ]))
}

fn write_lance_generation(
    path: &Path,
    repo: &crate::registry::RepoEntry,
    root: &str,
    model_fingerprint: &str,
    records: &[CorpusRecord],
    vectors: &[(Vec<f32>, bool)],
) -> Result<(), String> {
    let uri = path
        .to_str()
        .ok_or_else(|| "vector path is not valid UTF-8".to_string())?;
    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| format!("cannot create vector runtime: {e}"))?;
    runtime.block_on(async {
        let db = lancedb::connect(uri)
            .execute()
            .await
            .map_err(|e| format!("cannot open vector staging database: {e}"))?;
        let schema = lance_schema();
        if records.is_empty() {
            db.create_empty_table(TABLE_NAME, schema)
                .execute()
                .await
                .map_err(|e| format!("cannot create empty vector table: {e}"))?;
        } else {
            let vector_array = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                vectors
                    .iter()
                    .map(|(vector, _)| Some(vector.iter().copied().map(Some).collect::<Vec<_>>())),
                VECTOR_DIMENSIONS as i32,
            );
            let batch = RecordBatch::try_new(
                schema,
                vec![
                    Arc::new(StringArray::from(vec![repo.name.as_str(); records.len()])),
                    Arc::new(StringArray::from(vec![root; records.len()])),
                    Arc::new(StringArray::from(
                        records
                            .iter()
                            .map(|record| record.id.as_str())
                            .collect::<Vec<_>>(),
                    )),
                    Arc::new(UInt64Array::from(
                        records
                            .iter()
                            .map(|record| record.revision)
                            .collect::<Vec<_>>(),
                    )),
                    Arc::new(StringArray::from(
                        records
                            .iter()
                            .map(|record| record.kind.as_str())
                            .collect::<Vec<_>>(),
                    )),
                    Arc::new(StringArray::from(
                        records
                            .iter()
                            .map(|record| record.content_digest.as_str())
                            .collect::<Vec<_>>(),
                    )),
                    Arc::new(StringArray::from(vec![model_fingerprint; records.len()])),
                    Arc::new(BooleanArray::from(
                        vectors
                            .iter()
                            .map(|(_, truncated)| *truncated)
                            .collect::<Vec<_>>(),
                    )),
                    Arc::new(vector_array),
                ],
            )
            .map_err(|e| format!("cannot prepare vector rows: {e}"))?;
            db.create_table(TABLE_NAME, batch)
                .execute()
                .await
                .map_err(|e| format!("cannot write vector rows: {e}"))?;
        }
        Ok(())
    })
}

fn validate_vector_table(path: &Path, manifest: &Manifest) -> Result<(usize, String), String> {
    let uri = path
        .to_str()
        .ok_or_else(|| "vector path is not valid UTF-8".to_string())?;
    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| format!("cannot create vector runtime: {e}"))?;
    runtime.block_on(async {
        let db = lancedb::connect(uri)
            .execute()
            .await
            .map_err(|e| format!("cannot open vector generation: {e}"))?;
        let table = db
            .open_table(TABLE_NAME)
            .execute()
            .await
            .map_err(|e| format!("cannot open vector table: {e}"))?;
        let schema = table
            .schema()
            .await
            .map_err(|e| format!("cannot read vector table schema: {e}"))?;
        if schema.as_ref() != lance_schema().as_ref() {
            return Err("vector table schema does not match the manifest contract".into());
        }
        let mut stream = table
            .query()
            .select(Select::All)
            .order_by(Some(vec![ColumnOrdering::asc_nulls_last("id".into())]))
            .execute()
            .await
            .map_err(|e| format!("cannot scan vector table: {e}"))?;

        let mut hash = Sha256::new();
        hash_field(&mut hash, b"riltra-vector-table-v1");
        let mut rows = 0usize;
        let mut previous_id: Option<String> = None;
        while let Some(batch) = stream
            .try_next()
            .await
            .map_err(|e| format!("cannot read vector table batch: {e}"))?
        {
            let repo = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| "vector repo column has the wrong type".to_string())?;
            let root = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| "vector root column has the wrong type".to_string())?;
            let ids = batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| "vector id column has the wrong type".to_string())?;
            let revisions = batch
                .column(3)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| "vector revision column has the wrong type".to_string())?;
            let kinds = batch
                .column(4)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| "vector kind column has the wrong type".to_string())?;
            let content_digests = batch
                .column(5)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| "vector content digest column has the wrong type".to_string())?;
            let model_fingerprints = batch
                .column(6)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| "vector model fingerprint column has the wrong type".to_string())?;
            let truncated = batch
                .column(7)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(|| "vector truncation column has the wrong type".to_string())?;
            let vectors = batch
                .column(8)
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .ok_or_else(|| "vector column has the wrong type".to_string())?;
            for row in 0..batch.num_rows() {
                if [
                    repo.is_null(row),
                    root.is_null(row),
                    ids.is_null(row),
                    revisions.is_null(row),
                    kinds.is_null(row),
                    content_digests.is_null(row),
                    model_fingerprints.is_null(row),
                    truncated.is_null(row),
                    vectors.is_null(row),
                ]
                .into_iter()
                .any(|is_null| is_null)
                {
                    return Err("vector table contains a null required value".into());
                }
                let id = ids.value(row);
                if previous_id
                    .as_deref()
                    .is_some_and(|previous| previous >= id)
                {
                    return Err("vector table contains duplicate or unordered record IDs".into());
                }
                previous_id = Some(id.to_string());
                let row_repo = repo.value(row);
                let row_root = root.value(row);
                let row_model = model_fingerprints.value(row);
                if row_repo != manifest.repo
                    || row_root != manifest.root
                    || row_model != manifest.model_fingerprint
                {
                    return Err("vector row repository or model identity mismatch".into());
                }
                let content_digest = content_digests.value(row);
                if content_digest.len() != 64
                    || !content_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err("vector row content digest is malformed".into());
                }
                let vector = vectors.value(row);
                let vector = vector
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .ok_or_else(|| "vector row has the wrong element type".to_string())?;
                if vector.len() != VECTOR_DIMENSIONS
                    || (0..vector.len()).any(|index| vector.is_null(index))
                {
                    return Err("vector row has a null or invalid dimension".into());
                }
                let vector_values = (0..vector.len())
                    .map(|index| vector.value(index))
                    .collect::<Vec<_>>();
                validate_vectors(&[(vector_values.clone(), truncated.value(row))])?;
                hash_vector_row(
                    &mut hash,
                    VectorDigestRow {
                        repo: row_repo,
                        root: row_root,
                        id,
                        revision: revisions.value(row),
                        kind: kinds.value(row),
                        content_digest,
                        model_fingerprint: row_model,
                        truncated: truncated.value(row),
                        vector: &vector_values,
                    },
                );
                rows = rows
                    .checked_add(1)
                    .ok_or_else(|| "vector row count overflow".to_string())?;
            }
        }
        hash_field(&mut hash, &(rows as u64).to_be_bytes());
        Ok((rows, format!("{:x}", hash.finalize())))
    })
}

#[cfg(test)]
fn build_with<F>(
    home: &Path,
    repo: &crate::registry::RepoEntry,
    model_fingerprint: &str,
    embed: F,
) -> Result<BuildSummary, String>
where
    F: FnMut(&str, &str) -> Result<(Vec<f32>, bool), String>,
{
    build_with_hook(home, repo, model_fingerprint, embed, |_| Ok(()))
}

#[cfg(test)]
fn build_with_hook<F, H>(
    home: &Path,
    repo: &crate::registry::RepoEntry,
    model_fingerprint: &str,
    embed: F,
    after_generation: H,
) -> Result<BuildSummary, String>
where
    F: FnMut(&str, &str) -> Result<(Vec<f32>, bool), String>,
    H: FnOnce(&Path) -> Result<(), String>,
{
    build_with_model_check(
        home,
        repo,
        model_fingerprint,
        embed,
        || Ok(()),
        after_generation,
    )
}

fn build_with_model_check<F, V, H>(
    home: &Path,
    repo: &crate::registry::RepoEntry,
    model_fingerprint: &str,
    mut embed: F,
    mut verify_model: V,
    after_generation: H,
) -> Result<BuildSummary, String>
where
    F: FnMut(&str, &str) -> Result<(Vec<f32>, bool), String>,
    V: FnMut() -> Result<(), String>,
    H: FnOnce(&Path) -> Result<(), String>,
{
    let root = checked_root(repo)?;
    if model_fingerprint.len() != 64
        || !model_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("invalid model fingerprint".into());
    }
    let home = checked_home(home)?;
    read_corpus(&home, repo, false)?
        .ok_or_else(|| format!("no knowledge store for repository '{}'", repo.name))?;
    let paths = vector_paths(&home, repo)?;
    if check_dir(&paths.repo)? {
        build_current_manifest(&paths.repo)?;
    }
    let mut stage = paths.repo.join(next_stage_name());
    check_lance_path_budget(&stage)?;
    ensure_dir(&paths.root)?;
    ensure_dir(&paths.repo)?;
    let _lock = open_lock(&paths, true, true)?
        .ok_or_else(|| "vector state directory disappeared".to_string())?;
    check_dir(&paths.root)?;
    check_dir(&paths.repo)?;
    let snapshot = read_corpus(&home, repo, true)?
        .ok_or_else(|| format!("no knowledge store for repository '{}'", repo.name))?;

    let current = build_current_manifest(&paths.repo)?;
    if let Some(current) = current {
        if current.repo == repo.name
            && current.root == root
            && current.source_generation == snapshot.generation
            && current.corpus_fingerprint == snapshot.fingerprint
            && current.model_fingerprint == model_fingerprint
        {
            let generation_dir = paths.repo.join(generation_name(current.sequence));
            if check_dir(&generation_dir)? {
                validate_tree(&generation_dir)?;
                if let Ok((rows, digest)) = validate_vector_table(&generation_dir, &current) {
                    if rows == current.rows
                        && rows == snapshot.row_count
                        && digest == current.table_digest
                    {
                        verify_model()?;
                        return Ok(BuildSummary {
                            sequence: current.sequence,
                            source_generation: current.source_generation,
                            rows: current.rows,
                            rebuilt: false,
                        });
                    }
                }
            }
        }
    }

    let mut vectors = Vec::with_capacity(snapshot.records.len());
    for record in &snapshot.records {
        vectors.push(embed(&record.title, &record.body)?);
    }
    validate_vectors(&vectors)?;
    let table_digest =
        expected_table_digest(repo, &root, model_fingerprint, &snapshot.records, &vectors)?;
    let sequence = latest_sequence(&paths.repo)?;
    while stage.exists() {
        stage = paths.repo.join(next_stage_name());
        check_lance_path_budget(&stage)?;
    }
    fs::create_dir(&stage).map_err(|e| {
        format!(
            "cannot create vector staging directory '{}': {e}",
            stage.display()
        )
    })?;
    validate_tree(&stage)?;
    write_lance_generation(
        &stage,
        repo,
        &root,
        model_fingerprint,
        &snapshot.records,
        &vectors,
    )?;
    check_dir(&stage)?;
    validate_tree(&stage)?;
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        sequence,
        repo: repo.name.clone(),
        root: root.clone(),
        source_generation: snapshot.generation,
        corpus_fingerprint: snapshot.fingerprint.clone(),
        model_fingerprint: model_fingerprint.to_ascii_lowercase(),
        rows: snapshot.row_count,
        table_digest,
    };
    let (stage_rows, stage_digest) = validate_vector_table(&stage, &manifest)?;
    if stage_rows != manifest.rows || stage_digest != manifest.table_digest {
        return Err("staged vector data failed its integrity digest".into());
    }
    verify_model()?;

    // BEGIN IMMEDIATE prevents SQLite writers from committing after this last
    // comparison and before manifest publication, including in WAL mode.
    // SQLite may create/touch journal sidecars; this transaction does not
    // write canonical rows or schema. It does not serialize backup restore's
    // filesystem rename: restore_from requires a quiescent store, so restore
    // must not run concurrently with a vector build.
    let mut knowledge_guard = crate::knowledge_store::open_vector_build_guard_at(&home, repo)?;
    let publish_tx = knowledge_guard
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("cannot guard knowledge publication: {e}"))?;
    let before_publish = read_corpus_tx(&publish_tx, repo, Path::new(&root), false)?;
    if before_publish.generation != snapshot.generation
        || before_publish.fingerprint != snapshot.fingerprint
    {
        return Err("knowledge changed during vector build; previous generation retained".into());
    }

    let generation_dir = paths.repo.join(generation_name(sequence));
    if check_dir(&generation_dir)? {
        return Err("vector generation path already exists".into());
    }
    fs::rename(&stage, &generation_dir)
        .map_err(|e| format!("cannot finalize vector generation: {e}"))?;
    validate_tree(&generation_dir)?;
    after_generation(&generation_dir)?;
    validate_tree(&generation_dir)?;
    write_manifest_atomic(&paths.repo, &manifest)?;
    publish_tx
        .commit()
        .map_err(|e| format!("cannot finish knowledge publication guard: {e}"))?;
    Ok(BuildSummary {
        sequence,
        source_generation: manifest.source_generation,
        rows: manifest.rows,
        rebuilt: true,
    })
}

fn write_manifest_atomic(repo_dir: &Path, manifest: &Manifest) -> Result<(), String> {
    let final_path = repo_dir.join(manifest_name(manifest.sequence));
    if check_file(&final_path)? {
        return Err("vector manifest publication target already exists".into());
    }
    let temp_path = repo_dir.join(format!(
        ".manifest-{}-{}.tmp",
        manifest.sequence,
        next_stage_name()
    ));
    let bytes =
        serde_json::to_vec(manifest).map_err(|e| format!("cannot encode vector manifest: {e}"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| format!("cannot create vector manifest stage: {e}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("cannot write vector manifest stage: {e}"))?;
    drop(file);
    if let Err(error) = fs::rename(&temp_path, &final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("cannot publish vector manifest: {error}"));
    }
    Ok(())
}

#[cfg(test)]
fn build_test(home: &Path, repo: &crate::registry::RepoEntry) -> Result<u64, String> {
    let vector = vec![1.0 / (VECTOR_DIMENSIONS as f32).sqrt(); VECTOR_DIMENSIONS];
    build_with(home, repo, &hex_digest(b"test-model"), |_, _| {
        Ok((vector.clone(), false))
    })
    .map(|summary| summary.rows as u64)
}

fn cli_repo(name: &str, err: &mut String) -> Result<crate::registry::RepoEntry, i32> {
    if !crate::paths::validate_name(name) {
        err.push_str(&format!("error: invalid repository name: '{name}'\n"));
        return Err(2);
    }
    let (_, repo) = crate::knowledge_repo(name, err)?;
    checked_root(&repo).map_err(|error| {
        err.push_str(&format!("error: {error}\n"));
        3
    })?;
    Ok(repo)
}

fn fail(err: &mut String, message: impl AsRef<str>) -> i32 {
    err.push_str(&format!("error: {}\n", message.as_ref()));
    3
}

pub fn build_cli(repo_name: &str, model_dir: &str, out: &mut String, err: &mut String) -> i32 {
    let repo = match cli_repo(repo_name, err) {
        Ok(repo) => repo,
        Err(code) => return code,
    };
    let model_dir = Path::new(model_dir);
    let model_fingerprint = match crate::inference::local_model_fingerprint(model_dir) {
        Ok(fingerprint) => fingerprint,
        Err(error) => return fail(err, error),
    };
    let mut embeddings = match crate::inference::LocalEmbeddings::load_vector_model(model_dir) {
        Ok(embeddings) => embeddings,
        Err(error) => return fail(err, error),
    };
    let home = crate::paths::config_dir();
    let expected_fingerprint = model_fingerprint.clone();
    let verified_model_dir = model_dir.to_path_buf();
    let summary = match build_with_model_check(
        &home,
        &repo,
        &model_fingerprint,
        |title, body| {
            let embedded = embeddings.embed_passage_vector(title, body)?;
            Ok((embedded.vector, embedded.embedding_truncated))
        },
        || {
            let current_fingerprint =
                crate::inference::local_model_fingerprint(&verified_model_dir)?;
            if current_fingerprint != expected_fingerprint {
                return Err(
                    "local model or runtime changed during vector build; previous generation retained"
                        .into(),
                );
            }
            Ok(())
        },
        |_| Ok(()),
    ) {
        Ok(summary) => summary,
        Err(error) => return fail(err, error),
    };
    *out = format!(
        "{{\"repo\":\"{}\",\"sequence\":{},\"source_generation\":{},\"rows\":{},\"model_fingerprint\":\"{}\",\"rebuilt\":{}}}\n",
        crate::json::escape(repo_name),
        summary.sequence,
        summary.source_generation,
        summary.rows,
        model_fingerprint,
        summary.rebuilt
    );
    0
}

fn status_output(
    repo: &str,
    status: &str,
    manifest: Option<&Manifest>,
    diagnostic: Option<&str>,
) -> String {
    let (sequence, source_generation, rows, model_fingerprint) = match manifest {
        Some(manifest) => (
            manifest.sequence.to_string(),
            manifest.source_generation.to_string(),
            manifest.rows.to_string(),
            format!("\"{}\"", crate::json::escape(&manifest.model_fingerprint)),
        ),
        None => ("null".into(), "null".into(), "null".into(), "null".into()),
    };
    let diagnostic = diagnostic
        .map(|value| format!("\"{}\"", crate::json::escape(value)))
        .unwrap_or_else(|| "null".into());
    format!(
        "{{\"repo\":\"{}\",\"status\":\"{}\",\"sequence\":{},\"source_generation\":{},\"rows\":{},\"model_fingerprint\":{},\"diagnostic\":{}}}\n",
        crate::json::escape(repo),
        status,
        sequence,
        source_generation,
        rows,
        model_fingerprint,
        diagnostic
    )
}

fn status_at(home: &Path, repo: &crate::registry::RepoEntry) -> Result<String, String> {
    match fs::symlink_metadata(home) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(status_output(&repo.name, "missing", None, None));
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect RILTRA_HOME '{}': {error}",
                home.display()
            ));
        }
        Ok(_) => {}
    }
    let snapshot = read_corpus(home, repo, false)?;
    let paths = vector_paths(home, repo)?;
    if !check_dir(&paths.root)? || !check_dir(&paths.repo)? {
        return Ok(status_output(&repo.name, "missing", None, None));
    }
    let _lock = match open_lock(&paths, false, false)? {
        Some(lock) => lock,
        None => {
            return Ok(status_output(
                &repo.name,
                "unavailable",
                None,
                Some("vector lock file is missing"),
            ));
        }
    };
    let manifest = match read_current_manifest(&paths.repo) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => {
            return Ok(status_output(&repo.name, "missing", None, None));
        }
        Err(error) => {
            return Ok(status_output(
                &repo.name,
                "unavailable",
                None,
                Some(error.message()),
            ));
        }
    };
    let Some(snapshot) = snapshot else {
        return Ok(status_output(
            &repo.name,
            "stale",
            Some(&manifest),
            Some("knowledge store is missing"),
        ));
    };
    if manifest.repo != repo.name || manifest.root != repo.root {
        return Ok(status_output(
            &repo.name,
            "stale",
            Some(&manifest),
            Some("repository identity mismatch"),
        ));
    }
    let generation_dir = paths.repo.join(generation_name(manifest.sequence));
    let table = match check_dir(&generation_dir) {
        Ok(true) => {
            validate_tree(&generation_dir)?;
            validate_vector_table(&generation_dir, &manifest)
        }
        Ok(false) => Err("published vector generation is missing".into()),
        Err(error) => return Err(error),
    };
    match table {
        Err(error) => Ok(status_output(
            &repo.name,
            "unavailable",
            Some(&manifest),
            Some(&error),
        )),
        Ok((rows, _)) if rows != manifest.rows => Ok(status_output(
            &repo.name,
            "unavailable",
            Some(&manifest),
            Some("vector row count does not match manifest"),
        )),
        Ok((_, ref digest)) if digest != &manifest.table_digest => Ok(status_output(
            &repo.name,
            "unavailable",
            Some(&manifest),
            Some("vector table content digest does not match manifest"),
        )),
        Ok(_)
            if manifest.source_generation != snapshot.generation
                || manifest.corpus_fingerprint != snapshot.fingerprint =>
        {
            Ok(status_output(
                &repo.name,
                "stale",
                Some(&manifest),
                Some("active knowledge corpus changed"),
            ))
        }
        Ok(_) => Ok(status_output(&repo.name, "ready", Some(&manifest), None)),
    }
}

pub fn status_cli(repo_name: &str, out: &mut String, err: &mut String) -> i32 {
    let repo = match cli_repo(repo_name, err) {
        Ok(repo) => repo,
        Err(code) => return code,
    };
    match status_at(&crate::paths::config_dir(), &repo) {
        Ok(output) => {
            *out = output;
            0
        }
        Err(error) => fail(err, error),
    }
}

fn stage_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('s') else {
        return false;
    };
    let Some((pid, counter)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !counter.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_hexdigit())
        && counter.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn manifest_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".manifest-") else {
        return false;
    };
    let Some((sequence, stage)) = rest.split_once('-') else {
        return false;
    };
    let Some(stage) = stage.strip_suffix(".tmp") else {
        return false;
    };
    sequence.bytes().all(|byte| byte.is_ascii_digit()) && stage_name(stage)
}

fn validate_tree(path: &Path) -> Result<(), String> {
    if check_dir(path)? {
        for entry in fs::read_dir(path)
            .map_err(|e| format!("cannot inspect vector generation '{}': {e}", path.display()))?
        {
            let child = entry
                .map_err(|e| format!("cannot inspect vector generation: {e}"))?
                .path();
            match fs::symlink_metadata(&child) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "vector generation contains a link: '{}'",
                        child.display()
                    ));
                }
                Ok(metadata) if metadata.is_dir() => validate_tree(&child)?,
                Ok(metadata) if metadata.is_file() => {
                    if child.canonicalize().map_err(|e| {
                        format!("cannot resolve vector file '{}': {e}", child.display())
                    })? != child
                    {
                        return Err(format!("vector file retargeted: '{}'", child.display()));
                    }
                }
                Ok(_) => {
                    return Err(format!(
                        "unsupported entry in vector generation: '{}'",
                        child.display()
                    ))
                }
                Err(error) => {
                    return Err(format!(
                        "cannot inspect vector generation entry '{}': {error}",
                        child.display()
                    ))
                }
            }
        }
    }
    Ok(())
}

fn clear_paths(paths: &VectorPaths) -> Result<bool, String> {
    let root_exists = check_dir(&paths.root)?;
    let repo_exists = check_dir(&paths.repo)?;
    if !root_exists || !repo_exists {
        return Ok(false);
    }
    let _lock = match open_lock(paths, true, true) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let entries = match fs::read_dir(&paths.repo) {
        Ok(entries) => entries,
        Err(error) => return Err(format!("cannot list vector state: {error}")),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Err(format!("cannot list vector state: {error}")),
        };
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            return Err("vector state contains a non-UTF-8 filename".into());
        };
        if name == LOCK_FILE {
            check_file(&path)?;
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(format!(
                    "cannot inspect vector path '{}': {error}",
                    path.display()
                ))
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "vector path escapes via link: '{}'",
                path.display()
            ));
        }
        if sequence_from_name(&name, "gen-", "").is_some() || stage_name(&name) {
            if !metadata.is_dir() {
                return Err(format!(
                    "vector generation is not a directory: '{}'",
                    path.display()
                ));
            }
            validate_tree(&path)?;
            directories.push(path);
        } else if sequence_from_name(&name, "manifest-", ".json").is_some()
            || manifest_temp_name(&name)
        {
            if !metadata.is_file() {
                return Err(format!(
                    "vector manifest is not a file: '{}'",
                    path.display()
                ));
            }
            check_file(&path)?;
            files.push(path);
        } else {
            return Err(format!(
                "unrecognized vector state entry: '{}'",
                path.display()
            ));
        }
    }
    for directory in &directories {
        if let Err(error) = fs::remove_dir_all(directory) {
            return Err(format!(
                "cannot remove vector generation '{}': {error}",
                directory.display()
            ));
        }
    }
    for file in &files {
        if let Err(error) = fs::remove_file(file) {
            return Err(format!(
                "cannot remove vector manifest '{}': {error}",
                file.display()
            ));
        }
    }
    Ok(!directories.is_empty() || !files.is_empty())
}

pub fn clear_cli(repo_name: &str, out: &mut String, err: &mut String) -> i32 {
    let repo = match cli_repo(repo_name, err) {
        Ok(repo) => repo,
        Err(code) => return code,
    };
    let home = crate::paths::config_dir();
    match fs::symlink_metadata(&home) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            *out = format!(
                "{{\"repo\":\"{}\",\"cleared\":false}}\n",
                crate::json::escape(repo_name)
            );
            return 0;
        }
        Err(error) => {
            return fail(
                err,
                format!("cannot inspect RILTRA_HOME '{}': {error}", home.display()),
            )
        }
        Ok(_) => {}
    }
    let paths = match vector_paths(&home, &repo) {
        Ok(paths) => paths,
        Err(error) => return fail(err, error),
    };
    let cleared = match clear_paths(&paths) {
        Ok(cleared) => cleared,
        Err(error) => return fail(err, error),
    };
    *out = format!(
        "{{\"repo\":\"{}\",\"cleared\":{}}}\n",
        crate::json::escape(repo_name),
        cleared
    );
    0
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{mpsc, Arc, Barrier},
        thread,
        time::Duration,
    };

    use lancedb::arrow::{
        arrow_array::{RecordBatch, StringArray},
        arrow_schema::{DataType, Field, Schema},
    };

    use crate::{
        knowledge::{NewRecord, SupersededRef, UpdateRecord},
        knowledge_store,
        registry::RepoEntry,
        testutil::tmpdir,
    };

    fn fixture() -> (std::path::PathBuf, RepoEntry) {
        let home = tmpdir("");
        let root = tmpdir("vector-root").canonicalize().unwrap();
        let repo = RepoEntry {
            name: "repo".into(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        (home, repo)
    }

    fn record(id: &str, status: &str) -> NewRecord {
        NewRecord {
            id: id.into(),
            kind: "decision".into(),
            title: format!("title {id}"),
            body: format!("body {id}"),
            status: status.into(),
            author: "test".into(),
        }
    }

    fn vector() -> Vec<f32> {
        vec![1.0 / (super::VECTOR_DIMENSIONS as f32).sqrt(); super::VECTOR_DIMENSIONS]
    }

    fn build_as(home: &std::path::Path, repo: &RepoEntry, model: &str) -> super::BuildSummary {
        let vector = vector();
        super::build_with(home, repo, model, |_, _| Ok((vector.clone(), false))).unwrap()
    }

    #[test]
    fn builds_only_active_heads_without_changing_canonical_data() {
        let home = tmpdir("");
        let root = tmpdir("vector-root").canonicalize().unwrap();
        let repo = RepoEntry {
            name: "repo".into(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        for (id, status) in [("active", "active"), ("archive", "archived")] {
            knowledge_store::create_record(
                &mut conn,
                &NewRecord {
                    id: id.into(),
                    kind: "decision".into(),
                    title: "title".into(),
                    body: "body".into(),
                    status: status.into(),
                    author: "test".into(),
                },
            )
            .unwrap();
        }
        let before = knowledge_store::read_generation(&conn).unwrap();
        assert_eq!(super::build_test(&home, &repo).unwrap(), 1);
        assert_eq!(knowledge_store::read_generation(&conn).unwrap(), before);
    }

    #[test]
    fn generation_and_corpus_fingerprint_drive_freshness() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();

        let model_a = "a".repeat(64);
        let first = build_as(&home, &repo, &model_a);
        let no_op = build_as(&home, &repo, &model_a);
        assert_eq!(first.sequence, no_op.sequence);
        assert!(!no_op.rebuilt);

        // Restore-like content replacement: keep generation and revision equal
        // while changing bytes, proving that generation alone is insufficient.
        conn.execute(
            "UPDATE records SET body = 'restored body' WHERE id = 'active' AND revision = 1",
            [],
        )
        .unwrap();
        assert_eq!(
            knowledge_store::read_generation(&conn).unwrap(),
            first.source_generation
        );
        let stale = super::status_at(&home, &repo).unwrap();
        assert!(stale.contains("\"status\":\"stale\""), "{stale}");
        let rebuilt = build_as(&home, &repo, &model_a);
        assert!(rebuilt.rebuilt);
        assert!(rebuilt.sequence > first.sequence);

        // Same corpus with a different verified-model identity must also build
        // a new immutable generation.
        let model_b = "b".repeat(64);
        let model_changed = build_as(&home, &repo, &model_b);
        assert!(model_changed.rebuilt);
        assert!(model_changed.sequence > rebuilt.sequence);
    }

    #[test]
    fn model_revalidation_failure_keeps_the_current_generation() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        let model_a = "a".repeat(64);
        let first = build_as(&home, &repo, &model_a);

        let no_op_error = super::build_with_model_check(
            &home,
            &repo,
            &model_a,
            |_, _| Ok((vector(), false)),
            || Err("local model changed".into()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(no_op_error.contains("model changed"), "{no_op_error}");

        let model_b = "b".repeat(64);
        let publish_error = super::build_with_model_check(
            &home,
            &repo,
            &model_b,
            |_, _| Ok((vector(), false)),
            || Err("local model changed".into()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(publish_error.contains("model changed"), "{publish_error}");
        let paths = super::vector_paths(&home, &repo).unwrap();
        assert_eq!(
            super::read_current_manifest(&paths.repo)
                .unwrap()
                .unwrap()
                .sequence,
            first.sequence
        );
        assert!(!paths
            .repo
            .join(super::generation_name(first.sequence + 1))
            .exists());
        assert!(super::status_at(&home, &repo)
            .unwrap()
            .contains("\"status\":\"ready\""));
    }

    #[test]
    fn supersede_and_retract_remove_non_active_heads_from_new_generations() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("old", "active")).unwrap();
        knowledge_store::create_record(&mut conn, &record("new", "active")).unwrap();
        let model = "c".repeat(64);
        assert_eq!(build_as(&home, &repo, &model).rows, 2);

        knowledge_store::update_record(
            &mut conn,
            "old",
            1,
            &UpdateRecord {
                title: "old title".into(),
                body: "old body".into(),
                status: "superseded".into(),
                author: "test".into(),
                superseded_by: Some(SupersededRef {
                    id: "new".into(),
                    revision: 1,
                }),
                reason: Some("replacement".into()),
                confirm: false,
            },
        )
        .unwrap();
        let superseded = build_as(&home, &repo, &model);
        assert_eq!(superseded.rows, 1);

        knowledge_store::update_record(
            &mut conn,
            "new",
            1,
            &UpdateRecord {
                title: "new title".into(),
                body: "new body".into(),
                status: "retracted".into(),
                author: "test".into(),
                superseded_by: None,
                reason: Some("withdrawn".into()),
                confirm: false,
            },
        )
        .unwrap();
        let retracted = build_as(&home, &repo, &model);
        assert_eq!(retracted.rows, 0);
        assert!(retracted.sequence > superseded.sequence);
    }

    #[test]
    fn failed_manifest_publication_keeps_the_previous_generation_current() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        let model = "d".repeat(64);
        let first = build_as(&home, &repo, &model);
        let paths = super::vector_paths(&home, &repo).unwrap();
        let error = super::build_with_hook(
            &home,
            &repo,
            &"e".repeat(64),
            {
                let vector = vector();
                move |_, _| Ok((vector.clone(), false))
            },
            |_| Err("injected manifest publication failure".into()),
        )
        .unwrap_err();
        assert!(error.contains("injected manifest publication failure"));
        assert_eq!(
            super::read_current_manifest(&paths.repo)
                .unwrap()
                .unwrap()
                .sequence,
            first.sequence
        );
        let status = super::status_at(&home, &repo).unwrap();
        assert!(status.contains("\"status\":\"ready\""), "{status}");
    }

    #[test]
    fn rollback_journal_writer_waits_for_immediate_publication_guard() {
        let (home, repo) = fixture();
        let mut writer = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut writer, &record("active", "active")).unwrap();
        writer.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
        let mode: String = writer
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "delete");

        let mut guard = knowledge_store::open_vector_build_guard_at(&home, &repo).unwrap();
        let transaction = guard
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let writer_thread = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let result = knowledge_store::update_record(
                &mut writer,
                "active",
                1,
                &UpdateRecord {
                    title: "after guard".into(),
                    body: "after guard".into(),
                    status: "active".into(),
                    author: "test".into(),
                    superseded_by: None,
                    reason: None,
                    confirm: false,
                },
            );
            done_tx.send(result).unwrap();
        });
        attempted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err());
        transaction.commit().unwrap();
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .0,
            2
        );
        writer_thread.join().unwrap();
    }

    #[test]
    fn wal_writer_cannot_commit_between_final_freshness_check_and_publication() {
        let (home, repo) = fixture();
        let mut setup = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut setup, &record("active", "active")).unwrap();
        setup.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        let mode: String = setup
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        drop(setup);
        let writer = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();

        let (guarded_tx, guarded_rx) = mpsc::channel();
        let (publish_tx, publish_rx) = mpsc::channel();
        let build_home = home.clone();
        let build_repo = repo.clone();
        let builder = thread::spawn(move || {
            let vector = vector();
            super::build_with_hook(
                &build_home,
                &build_repo,
                &"6".repeat(64),
                |_, _| Ok((vector.clone(), false)),
                |_| {
                    guarded_tx.send(()).unwrap();
                    publish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                },
            )
        });
        guarded_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (writer_done_tx, writer_done_rx) = mpsc::channel();
        let writer_thread = thread::spawn(move || {
            let mut writer = writer;
            attempted_tx.send(()).unwrap();
            let result = knowledge_store::update_record(
                &mut writer,
                "active",
                1,
                &UpdateRecord {
                    title: "changed after publish".into(),
                    body: "changed after publish".into(),
                    status: "active".into(),
                    author: "test".into(),
                    superseded_by: None,
                    reason: None,
                    confirm: false,
                },
            );
            writer_done_tx.send(result).unwrap();
        });
        attempted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(writer_done_rx
            .recv_timeout(Duration::from_millis(200))
            .is_err());
        publish_tx.send(()).unwrap();
        let built = builder.join().unwrap().unwrap();
        assert!(built.rebuilt);
        assert_eq!(
            writer_done_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .0,
            2
        );
        writer_thread.join().unwrap();
        let status = super::status_at(&home, &repo).unwrap();
        assert!(status.contains("\"status\":\"stale\""), "{status}");
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_concurrent_restore_can_replace_the_database_path() {
        let (home, repo) = fixture();
        let root = std::path::Path::new(&repo.root);
        let backup_base = tmpdir("vector-restore-backup").join("snapshot");
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        crate::backup::backup_to(&conn, &repo, root, &backup_base).unwrap();
        knowledge_store::update_record(
            &mut conn,
            "active",
            1,
            &UpdateRecord {
                title: "current title".into(),
                body: "current body".into(),
                status: "active".into(),
                author: "test".into(),
                superseded_by: None,
                reason: None,
                confirm: false,
            },
        )
        .unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
        drop(conn);

        let db = knowledge_store::knowledge_db_path(&home, &repo.name);
        let companions = ["-wal", "-shm", "-journal"].map(|suffix| {
            let mut path = db.as_os_str().to_os_string();
            path.push(suffix);
            std::path::PathBuf::from(path)
        });
        assert!(companions.iter().all(|path| !path.exists()));

        let vector = vector();
        let result = super::build_with_hook(
            &home,
            &repo,
            &"9".repeat(64),
            |_, _| Ok((vector.clone(), false)),
            |_| crate::backup::restore_from(&home, &repo, root, &backup_base, true).map(|_| ()),
        );
        assert!(result.is_ok(), "{}", result.unwrap_err());
        let status = super::status_at(&home, &repo).unwrap();
        assert!(status.contains("\"status\":\"stale\""), "{status}");
    }

    #[test]
    fn quiescent_restore_is_detected_and_rebuilt_from_restored_corpus() {
        let (home, repo) = fixture();
        let root = std::path::Path::new(&repo.root);
        let backup_base = tmpdir("vector-restore-quiescent").join("snapshot");
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        crate::backup::backup_to(&conn, &repo, root, &backup_base).unwrap();
        knowledge_store::update_record(
            &mut conn,
            "active",
            1,
            &UpdateRecord {
                title: "changed title".into(),
                body: "changed body".into(),
                status: "active".into(),
                author: "test".into(),
                superseded_by: None,
                reason: None,
                confirm: false,
            },
        )
        .unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
        drop(conn);

        let model = "a".repeat(64);
        let built_changed = build_as(&home, &repo, &model);
        assert_eq!(built_changed.source_generation, 2);
        crate::backup::restore_from(&home, &repo, root, &backup_base, true).unwrap();
        let stale = super::status_at(&home, &repo).unwrap();
        assert!(stale.contains("\"status\":\"stale\""), "{stale}");
        let rebuilt = build_as(&home, &repo, &model);
        assert!(rebuilt.rebuilt);
        assert!(rebuilt.sequence > built_changed.sequence);
        let ready = super::status_at(&home, &repo).unwrap();
        assert!(ready.contains("\"status\":\"ready\""), "{ready}");
    }

    #[test]
    fn competing_builds_publish_one_generation() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let home = home.clone();
            let repo = repo.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                build_as(&home, &repo, &"f".repeat(64))
            }));
        }
        barrier.wait();
        let summaries = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(summaries[0].sequence, summaries[1].sequence);
        assert_eq!(
            summaries.iter().filter(|summary| summary.rebuilt).count(),
            1
        );
        assert_eq!(
            super::read_current_manifest(&super::vector_paths(&home, &repo).unwrap().repo)
                .unwrap()
                .unwrap()
                .sequence,
            summaries[0].sequence
        );
    }

    #[test]
    fn clear_waits_for_shared_reader_lock() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        build_as(&home, &repo, &"1".repeat(64));
        let paths = super::vector_paths(&home, &repo).unwrap();
        let reader = super::open_lock(&paths, false, false).unwrap().unwrap();
        let clear_paths = super::VectorPaths {
            root: paths.root.clone(),
            repo: paths.repo.clone(),
            lock: paths.lock.clone(),
        };
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(super::clear_paths(&clear_paths)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(200)).is_err());
        drop(reader);
        assert!(done_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap());
        worker.join().unwrap();
        assert!(super::read_current_manifest(&paths.repo).unwrap().is_none());
        assert!(
            paths.lock.exists(),
            "keep the lock inode for future readers"
        );
    }

    #[test]
    fn status_is_read_only_and_derived_corruption_is_a_diagnostic() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        let missing = super::status_at(&home, &repo).unwrap();
        assert!(missing.contains("\"status\":\"missing\""), "{missing}");
        assert!(!home.join(super::VECTOR_HOME).exists());
        build_as(&home, &repo, &"2".repeat(64));
        let paths = super::vector_paths(&home, &repo).unwrap();
        let manifest = paths.repo.join(super::manifest_name(1));
        let original = std::fs::read(&manifest).unwrap();
        std::fs::write(&manifest, b"not a manifest").unwrap();
        let unavailable = super::status_at(&home, &repo).unwrap();
        assert!(
            unavailable.contains("\"status\":\"unavailable\""),
            "{unavailable}"
        );
        let rebuilt = build_as(&home, &repo, &"2".repeat(64));
        assert!(rebuilt.rebuilt);
        assert!(rebuilt.sequence > 1);
        std::fs::write(&manifest, original).unwrap();

        drop(conn);
        std::fs::write(
            knowledge_store::knowledge_db_path(&home, &repo.name),
            b"broken sqlite",
        )
        .unwrap();
        let canonical_error = super::status_at(&home, &repo).unwrap_err();
        assert!(
            canonical_error.contains("knowledge database"),
            "{canonical_error}"
        );
    }

    #[test]
    fn status_reports_existing_vector_state_without_its_lock_as_unavailable() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        build_as(&home, &repo, &"2".repeat(64));
        let paths = super::vector_paths(&home, &repo).unwrap();
        assert!(paths.repo.join(super::manifest_name(1)).exists());

        std::fs::remove_file(&paths.lock).unwrap();
        let status = super::status_at(&home, &repo).unwrap();

        assert!(status.contains("\"status\":\"unavailable\""), "{status}");
        assert!(status.contains("lock"), "{status}");
        assert!(!paths.lock.exists(), "status must not recreate the lock");
        assert!(paths.repo.join(super::manifest_name(1)).exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn current_generation_table_link_outside_home_fails_closed() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        let model = "9".repeat(64);
        let first = build_as(&home, &repo, &model);

        let external_home = tmpdir("ext");
        let mut external_conn =
            knowledge_store::open_knowledge_at(&external_home, &repo, true).unwrap();
        knowledge_store::create_record(&mut external_conn, &record("active", "active")).unwrap();
        drop(external_conn);
        build_as(&external_home, &repo, &model);

        let paths = super::vector_paths(&home, &repo).unwrap();
        let generation = paths.repo.join(super::generation_name(first.sequence));
        let table_link = generation.join("v.lance");
        let external_table = super::vector_paths(&external_home, &repo)
            .unwrap()
            .repo
            .join(super::generation_name(1))
            .join("v.lance");
        std::fs::remove_dir_all(&table_link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external_table, &table_link).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&table_link)
                .arg(&external_table)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "could not create junction: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let status_error = super::status_at(&home, &repo).unwrap_err();
        assert!(
            status_error.contains("link") || status_error.contains("retarget"),
            "{status_error}"
        );
        let build_error =
            super::build_with(&home, &repo, &model, |_, _| Ok((vector(), false))).unwrap_err();
        assert!(
            build_error.contains("link") || build_error.contains("retarget"),
            "{build_error}"
        );
        assert!(
            external_table.is_dir(),
            "the external table must remain intact"
        );
        assert_eq!(
            super::read_current_manifest(&paths.repo)
                .unwrap()
                .unwrap()
                .sequence,
            first.sequence
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn manifest_link_fails_closed_without_publishing_a_generation() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        let model = "a".repeat(64);
        let first = build_as(&home, &repo, &model);
        let paths = super::vector_paths(&home, &repo).unwrap();
        let manifest = paths.repo.join(super::manifest_name(first.sequence));
        let external_manifest = tmpdir("ext").join("manifest.json");
        std::fs::copy(&manifest, &external_manifest).unwrap();
        std::fs::remove_file(&manifest).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external_manifest, &manifest).unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&manifest)
                .arg(external_manifest.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "could not create manifest junction: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let status = super::status_at(&home, &repo);
        assert!(
            matches!(
                &status,
                Ok(output)
                    if output.contains("\"status\":\"unavailable\"")
                        && output.contains("link")
            ),
            "status must report the unsafe manifest as unavailable: {status:?}"
        );
        std::fs::remove_file(&paths.lock).unwrap();
        let mut before: Vec<_> = std::fs::read_dir(&paths.repo)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        before.sort();

        let build = super::build_with(&home, &repo, &model, |_, _| Ok((vector(), false)));
        assert!(
            build.is_err(),
            "unsafe manifest link was not rejected: {build:?}"
        );
        let mut after: Vec<_> = std::fs::read_dir(&paths.repo)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        after.sort();
        assert_eq!(
            after, before,
            "rejected builds must not publish or stage data"
        );
        assert!(
            !paths.lock.exists(),
            "rejected builds must not create a lock"
        );
        assert!(
            external_manifest.is_file(),
            "the external manifest must remain intact"
        );
    }

    #[test]
    fn canonical_non_head_corruption_is_rejected_by_status_and_build() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        let model = "6".repeat(64);
        build_as(&home, &repo, &model);

        let db_path = knowledge_store::knowledge_db_path(&home, &repo.name);
        let mut conn = rusqlite::Connection::open(&db_path).unwrap();
        let migration = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        knowledge_store::ensure_code_binding_schema(&migration).unwrap();
        migration.commit().unwrap();
        drop(conn);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "INSERT INTO code_bindings (
                repo, repo_root_digest, snapshot_id, snapshot_nonce,
                snapshot_digest, source_digest, path, byte_start, byte_end,
                cursor_line, cursor_col, anchor_kind, precision, record_id,
                record_revision, relation, author, rationale, provenance, created_unix
             ) VALUES (
                'repo', 'invalid', 1, '', 'invalid', 'invalid', 'src/lib.rs',
                -1, 0, 0, 0, 'invalid', 'invalid', 'active', 1,
                'invalid', '', '', '', -1
             )",
            [],
        )
        .unwrap();
        drop(conn);

        let status_error = super::status_at(&home, &repo).unwrap_err();
        assert!(
            status_error.contains("corrupt knowledge store"),
            "{status_error}"
        );
        let build_error =
            super::build_with(&home, &repo, &model, |_, _| Ok((vector(), false))).unwrap_err();
        assert!(
            build_error.contains("corrupt knowledge store"),
            "{build_error}"
        );

        let paths = super::vector_paths(&home, &repo).unwrap();
        assert!(super::clear_paths(&paths).unwrap());
        let missing_index_error = super::status_at(&home, &repo).unwrap_err();
        assert!(
            missing_index_error.contains("corrupt knowledge store"),
            "{missing_index_error}"
        );
    }

    #[test]
    fn same_row_count_with_replaced_ids_and_vectors_fails_digest_validation() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        let model = "7".repeat(64);
        let first = build_as(&home, &repo, &model);

        let other_home = tmpdir("");
        let mut other = knowledge_store::open_knowledge_at(&other_home, &repo, true).unwrap();
        knowledge_store::create_record(&mut other, &record("different", "active")).unwrap();
        drop(other);
        let mut alternate = vec![0.0; super::VECTOR_DIMENSIONS];
        alternate[0] = 1.0;
        super::build_with(&other_home, &repo, &model, |_, _| {
            Ok((alternate.clone(), true))
        })
        .unwrap();

        let original_paths = super::vector_paths(&home, &repo).unwrap();
        let replacement_paths = super::vector_paths(&other_home, &repo).unwrap();
        let original_table = original_paths
            .repo
            .join(super::generation_name(first.sequence))
            .join("v.lance");
        let replacement_table = replacement_paths
            .repo
            .join(super::generation_name(1))
            .join("v.lance");
        std::fs::remove_dir_all(&original_table).unwrap();
        std::fs::rename(&replacement_table, &original_table).unwrap();

        let status = super::status_at(&home, &repo).unwrap();
        assert!(status.contains("\"status\":\"unavailable\""), "{status}");
        assert!(status.contains("content digest"), "{status}");
        let rebuilt = build_as(&home, &repo, &model);
        assert!(rebuilt.rebuilt);
        assert!(rebuilt.sequence > first.sequence);
    }

    #[test]
    fn status_rejects_a_same_row_count_table_with_the_wrong_schema() {
        let (home, repo) = fixture();
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        let first = build_as(&home, &repo, &"8".repeat(64));
        let generation = super::vector_paths(&home, &repo)
            .unwrap()
            .repo
            .join(super::generation_name(first.sequence));
        let replacement_db = tmpdir("");
        let uri = replacement_db.to_str().unwrap().to_string();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let db = lancedb::connect(uri.as_str()).execute().await.unwrap();
                let schema = Arc::new(Schema::new(vec![Field::new(
                    "wrong",
                    DataType::Utf8,
                    false,
                )]));
                let batch = RecordBatch::try_new(
                    schema,
                    vec![Arc::new(StringArray::from(vec!["one row"]))],
                )
                .unwrap();
                db.create_table(super::TABLE_NAME, batch)
                    .execute()
                    .await
                    .unwrap();
            });
        let table_path = generation.join("v.lance");
        std::fs::remove_dir_all(&table_path).unwrap();
        std::fs::rename(replacement_db.join("v.lance"), &table_path).unwrap();
        let status = super::status_at(&home, &repo).unwrap();
        assert!(status.contains("\"status\":\"unavailable\""), "{status}");
        assert!(status.contains("schema"), "{status}");
    }

    #[test]
    fn vector_state_is_confined_and_separate_for_each_repository() {
        let (home, repo_a) = fixture();
        let repo_b = RepoEntry {
            name: "repo-two".into(),
            ..repo_a.clone()
        };
        for repo in [&repo_a, &repo_b] {
            let mut conn = knowledge_store::open_knowledge_at(&home, repo, true).unwrap();
            knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
            drop(conn);
            build_as(&home, repo, &"3".repeat(64));
        }
        let paths_a = super::vector_paths(&home, &repo_a).unwrap();
        let paths_b = super::vector_paths(&home, &repo_b).unwrap();
        let canonical_vector_home = home.canonicalize().unwrap().join(super::VECTOR_HOME);
        assert_ne!(paths_a.repo, paths_b.repo);
        assert!(paths_a.repo.starts_with(&canonical_vector_home));
        assert!(paths_b.repo.starts_with(&canonical_vector_home));
        assert_eq!(
            super::read_current_manifest(&paths_a.repo)
                .unwrap()
                .unwrap()
                .repo,
            repo_a.name
        );
        assert_eq!(
            super::read_current_manifest(&paths_b.repo)
                .unwrap()
                .unwrap()
                .repo,
            repo_b.name
        );
    }

    #[cfg(unix)]
    #[test]
    fn vector_root_symlink_is_rejected_before_following_it() {
        let (home, repo) = fixture();
        let outside = tmpdir("vector-outside");
        std::os::unix::fs::symlink(&outside, home.join(super::VECTOR_HOME)).unwrap();
        let error = super::vector_paths(&home, &repo).unwrap_err();
        assert!(error.contains("link"), "{error}");
        assert!(outside.exists());
    }

    #[cfg(windows)]
    #[test]
    fn vector_root_junction_is_rejected_before_following_it() {
        let (home, repo) = fixture();
        let outside = tmpdir("vector-outside");
        let link = home.join(super::VECTOR_HOME);
        let made = std::os::windows::fs::symlink_dir(&outside, &link).is_ok()
            || std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false);
        assert!(
            made,
            "could not create a Windows directory symlink or junction"
        );
        let error = super::vector_paths(&home, &repo).unwrap_err();
        assert!(
            error.contains("link") || error.contains("retarget"),
            "{error}"
        );
        assert!(outside.exists());
    }

    #[cfg(windows)]
    #[test]
    fn path_budget_failure_preserves_an_existing_index() {
        let short_home = tmpdir("");
        let root = tmpdir("vector-root").canonicalize().unwrap();
        let repo = RepoEntry {
            name: "repo".into(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut conn = knowledge_store::open_knowledge_at(&short_home, &repo, true).unwrap();
        knowledge_store::create_record(&mut conn, &record("active", "active")).unwrap();
        drop(conn);
        build_as(&short_home, &repo, &"4".repeat(64));

        let deep_home = std::env::temp_dir()
            .join(format!("riltra-long-{}", std::process::id()))
            .join("a".repeat(50))
            .join("b".repeat(50))
            .join("home");
        std::fs::create_dir_all(deep_home.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&deep_home).unwrap();
        std::fs::create_dir_all(
            knowledge_store::knowledge_db_path(&deep_home, &repo.name)
                .parent()
                .unwrap(),
        )
        .unwrap();
        std::fs::copy(
            knowledge_store::knowledge_db_path(&short_home, &repo.name),
            knowledge_store::knowledge_db_path(&deep_home, &repo.name),
        )
        .unwrap();
        std::fs::rename(
            short_home.join(super::VECTOR_HOME),
            deep_home.join(super::VECTOR_HOME),
        )
        .unwrap();
        let paths = super::vector_paths(&deep_home, &repo).unwrap();
        let manifest_path = paths.repo.join(super::manifest_name(1));
        let before = std::fs::read(&manifest_path).unwrap();
        let error = super::build_with(&deep_home, &repo, &"5".repeat(64), {
            let vector = vector();
            move |_, _| Ok((vector.clone(), false))
        })
        .unwrap_err();
        assert!(error.contains("supported Windows path budget"), "{error}");
        assert_eq!(std::fs::read(&manifest_path).unwrap(), before);
        assert!(super::read_current_manifest(&paths.repo).unwrap().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn refuses_windows_paths_that_cannot_fit_a_lance_data_file() {
        let base = tmpdir("");
        let home = base.join("a".repeat(45)).join("b".repeat(45));
        std::fs::create_dir_all(&home).unwrap();
        let root = tmpdir("vector-root").canonicalize().unwrap();
        let repo = RepoEntry {
            name: "repo".into(),
            root: root.to_string_lossy().into_owned(),
            added_unix: 0,
        };
        let mut conn = knowledge_store::open_knowledge_at(&home, &repo, true).unwrap();
        knowledge_store::create_record(
            &mut conn,
            &NewRecord {
                id: "active".into(),
                kind: "decision".into(),
                title: "title".into(),
                body: "body".into(),
                status: "active".into(),
                author: "test".into(),
            },
        )
        .unwrap();

        let error = super::build_test(&home, &repo).unwrap_err();
        assert!(error.contains("supported Windows path budget"), "{error}");
        assert!(
            !home.join("v").exists(),
            "reject before creating derived state"
        );
    }
}
