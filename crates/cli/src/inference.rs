// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use fastembed::{
    AutoDevicePolicy, InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokenizers::{AddedToken, Tokenizer};

const MODEL_MANIFEST: &str = include_str!("model_manifest.json");
const MODEL_NAME: &str = "intfloat/multilingual-e5-small";
const MODEL_REVISION: &str = "ccc66d3bcd826f577e26b9a4072cc5fe3a7ad6a3";
const DIMENSIONS: usize = 384;
const MAX_TOKENS: usize = 512;
const RUNTIME_VERSION: &str = "1.24.4";
const RUNTIME_API: u32 = 24;
const RUNTIME_SOURCE_COMMIT: &str = "2d924974ef147392ced8409d36bd6d2e7fcc8a74";
const RUNTIME_SOURCE_REPOSITORY: &str = "https://github.com/microsoft/onnxruntime";
const KNOWN_TELEMETRY_ENABLED_WINDOWS_RUNTIME_SHA256: &str =
    "b95efb2113b603bbbf3f191061c5516a871ed546893c820e4f3b7b6c358dbf2a";

#[derive(Debug, Deserialize)]
struct ModelManifest {
    model: String,
    revision: String,
    dimensions: usize,
    pooling: String,
    query_prefix: String,
    passage_prefix: String,
    max_tokens: usize,
    cpu_only: bool,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct RuntimeManifest {
    version: String,
    api: u32,
    source_repository: String,
    source_commit: String,
    platform: String,
    cpu_only: bool,
    telemetry_disabled: bool,
    build_options: Vec<String>,
    main_library: ManifestFile,
    companion_libraries: Vec<ManifestFile>,
}

#[derive(Debug)]
enum InferenceError {
    Manifest(String),
    ManifestContractMismatch,
    UnsafeAssetPath(String),
    MissingAsset(String),
    AssetRead(String),
    AssetHashMismatch(String),
    RuntimePathNotAbsolute,
    UnsupportedRuntimePlatform,
    MissingRuntimeManifest,
    RuntimeManifest(String),
    MissingRuntime(String),
    RuntimeHashMismatch(String),
    RuntimeTelemetryEnabled,
    RuntimeAlreadyInitialized,
    RuntimeLoad(String),
    Tokenizer(String),
    ModelLoad(String),
    EmptyEmbedding,
    InvalidEmbeddingDimension(usize),
    InvalidEmbeddingValue,
}

impl fmt::Display for InferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(message) => write!(f, "invalid model manifest: {message}"),
            Self::ManifestContractMismatch => {
                write!(
                    f,
                    "model manifest does not match the pinned inference contract"
                )
            }
            Self::UnsafeAssetPath(path) => write!(f, "asset path escapes its root: {path}"),
            Self::MissingAsset(path) => write!(f, "missing model asset: {path}"),
            Self::AssetRead(message) => write!(f, "cannot read model asset: {message}"),
            Self::AssetHashMismatch(path) => {
                write!(f, "model asset SHA-256 mismatch: {path}")
            }
            Self::RuntimePathNotAbsolute => write!(f, "ONNX Runtime path must be absolute"),
            Self::UnsupportedRuntimePlatform => {
                write!(
                    f,
                    "no verified ONNX Runtime is provisioned for this platform"
                )
            }
            Self::MissingRuntimeManifest => {
                write!(f, "missing local runtime-manifest.json")
            }
            Self::RuntimeManifest(message) => {
                write!(f, "invalid local runtime manifest: {message}")
            }
            Self::MissingRuntime(path) => write!(f, "missing ONNX Runtime library: {path}"),
            Self::RuntimeHashMismatch(path) => {
                write!(f, "ONNX Runtime SHA-256 mismatch: {path}")
            }
            Self::RuntimeTelemetryEnabled => write!(
                f,
                "ONNX Runtime build is not approved as telemetry-disabled"
            ),
            Self::RuntimeAlreadyInitialized => write!(
                f,
                "a different ONNX Runtime library is already initialized in this process"
            ),
            Self::RuntimeLoad(message) => write!(f, "cannot initialize ONNX Runtime: {message}"),
            Self::Tokenizer(message) => write!(f, "tokenizer error: {message}"),
            Self::ModelLoad(message) => write!(f, "cannot load local embedding model: {message}"),
            Self::EmptyEmbedding => write!(f, "embedding model returned no vector"),
            Self::InvalidEmbeddingDimension(actual) => write!(
                f,
                "embedding has {actual} dimensions; expected {DIMENSIONS}"
            ),
            Self::InvalidEmbeddingValue => {
                write!(f, "embedding contains an invalid or non-normalized value")
            }
        }
    }
}

impl std::error::Error for InferenceError {}

#[derive(Debug)]
pub(crate) struct EmbeddedText {
    pub(crate) vector: Vec<f32>,
    pub(crate) embedding_truncated: bool,
}

pub(crate) struct LocalEmbeddings {
    model: TextEmbedding,
    tokenizer: Tokenizer,
    _environment: ort::environment::Environment,
    query_prefix: String,
    passage_prefix: String,
    max_tokens: usize,
}

fn model_manifest() -> Result<ModelManifest, InferenceError> {
    let manifest: ModelManifest = serde_json::from_str(MODEL_MANIFEST)
        .map_err(|e| InferenceError::Manifest(e.to_string()))?;
    if manifest.model != MODEL_NAME
        || manifest.revision != MODEL_REVISION
        || manifest.dimensions != DIMENSIONS
        || manifest.pooling != "mean"
        || manifest.query_prefix != "query: "
        || manifest.passage_prefix != "passage: "
        || manifest.max_tokens != MAX_TOKENS
        || !manifest.cpu_only
    {
        return Err(InferenceError::ManifestContractMismatch);
    }
    Ok(manifest)
}

fn safe_relative_path(path: &str) -> Result<&Path, InferenceError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(InferenceError::UnsafeAssetPath(
            path.to_string_lossy().into_owned(),
        ));
    }
    Ok(path)
}

fn confined_file(root: &Path, relative: &str) -> Result<PathBuf, InferenceError> {
    let relative = safe_relative_path(relative)?;
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(InferenceError::UnsafeAssetPath(
                relative.display().to_string(),
            ));
        }
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(InferenceError::MissingAsset(relative.display().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InferenceError::MissingAsset(relative.display().to_string()));
        }
        Err(error) => {
            return Err(InferenceError::AssetRead(format!(
                "{}: {error}",
                path.display()
            )));
        }
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|e| InferenceError::AssetRead(format!("{}: {e}", root.display())))?;
    let mut current = canonical_root.clone();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        current.push(part.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .map_err(|e| InferenceError::AssetRead(format!("{}: {e}", current.display())))?;
        if metadata.file_type().is_symlink() {
            return Err(InferenceError::UnsafeAssetPath(
                relative.display().to_string(),
            ));
        }
        if parts.peek().is_some() && !metadata.is_dir() {
            return Err(InferenceError::MissingAsset(relative.display().to_string()));
        }
    }
    let canonical_path = current
        .canonicalize()
        .map_err(|e| InferenceError::AssetRead(format!("{}: {e}", current.display())))?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(InferenceError::UnsafeAssetPath(
            relative.display().to_string(),
        ));
    }
    Ok(canonical_path)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

/// Fingerprint the verified local model and the approved runtime files.
/// Asset contents are checked against their manifests before their declared
/// digests contribute to the fingerprint.
pub(crate) fn local_model_fingerprint(model_dir: &Path) -> Result<String, String> {
    let manifest = model_manifest().map_err(|error| error.to_string())?;
    let model_dir = model_dir.canonicalize().map_err(|error| {
        format!(
            "cannot resolve model directory '{}': {error}",
            model_dir.display()
        )
    })?;
    let model_assets =
        read_model_assets(&model_dir, &manifest).map_err(|error| error.to_string())?;
    let runtime_dir = confined_runtime_dir(&model_dir).map_err(|error| error.to_string())?;
    verify_runtime_assets(&runtime_dir).map_err(|error| error.to_string())?;
    let runtime_manifest =
        read_runtime_manifest(&runtime_dir).map_err(|error| error.to_string())?;

    let mut hash = Sha256::new();
    hash_field(&mut hash, b"riltra-local-e5-model-v1");
    hash_field(&mut hash, MODEL_MANIFEST.as_bytes());
    let mut model_files = manifest.files.iter().collect::<Vec<_>>();
    model_files.sort_by(|left, right| left.path.cmp(&right.path));
    for file in model_files {
        if !model_assets.contains_key(&file.path) {
            return Err(format!(
                "model asset missing after verification: {}",
                file.path
            ));
        }
        hash_field(&mut hash, file.path.as_bytes());
        hash_field(&mut hash, file.sha256.to_ascii_lowercase().as_bytes());
    }

    let manifest_path =
        confined_file(&runtime_dir, "runtime-manifest.json").map_err(|error| error.to_string())?;
    let runtime_manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        format!(
            "cannot read runtime manifest '{}': {error}",
            manifest_path.display()
        )
    })?;
    hash_field(&mut hash, &runtime_manifest_bytes);
    let mut runtime_files = std::iter::once(&runtime_manifest.main_library)
        .chain(runtime_manifest.companion_libraries.iter())
        .collect::<Vec<_>>();
    runtime_files.sort_by(|left, right| left.path.cmp(&right.path));
    for file in runtime_files {
        hash_field(&mut hash, file.path.as_bytes());
        hash_field(&mut hash, file.sha256.to_ascii_lowercase().as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn read_model_assets(
    root: &Path,
    manifest: &ModelManifest,
) -> Result<HashMap<String, Vec<u8>>, InferenceError> {
    let mut assets = HashMap::new();
    for file in &manifest.files {
        let path = confined_file(root, &file.path)?;
        let bytes = fs::read(&path)
            .map_err(|e| InferenceError::AssetRead(format!("{}: {e}", path.display())))?;
        if sha256(&bytes) != file.sha256 {
            return Err(InferenceError::AssetHashMismatch(file.path.clone()));
        }
        if assets.insert(file.path.clone(), bytes).is_some() {
            return Err(InferenceError::Manifest(format!(
                "duplicate asset path {}",
                file.path
            )));
        }
    }
    Ok(assets)
}

fn verify_model_assets(root: &Path) -> Result<(), InferenceError> {
    let manifest = model_manifest()?;
    read_model_assets(root, &manifest).map(drop)
}

fn runtime_platform() -> Result<&'static str, InferenceError> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Ok("windows-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("linux-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("macos-x86_64")
    } else {
        Err(InferenceError::UnsupportedRuntimePlatform)
    }
}

fn read_runtime_manifest(runtime_dir: &Path) -> Result<RuntimeManifest, InferenceError> {
    let path =
        confined_file(runtime_dir, "runtime-manifest.json").map_err(|error| match error {
            InferenceError::MissingAsset(_) => InferenceError::MissingRuntimeManifest,
            other => other,
        })?;
    let bytes = fs::read(&path)
        .map_err(|e| InferenceError::RuntimeManifest(format!("{}: {e}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|e| InferenceError::RuntimeManifest(e.to_string()))
}

fn confined_runtime_dir(model_dir: &Path) -> Result<PathBuf, InferenceError> {
    let canonical_model_dir = model_dir
        .canonicalize()
        .map_err(|error| InferenceError::AssetRead(format!("{}: {error}", model_dir.display())))?;
    let runtime_dir = canonical_model_dir.join("runtime");
    let metadata = fs::symlink_metadata(&runtime_dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            InferenceError::MissingRuntimeManifest
        } else {
            InferenceError::AssetRead(format!("{}: {error}", runtime_dir.display()))
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(InferenceError::UnsafeAssetPath("runtime".into()));
    }
    if !metadata.is_dir() {
        return Err(InferenceError::MissingRuntimeManifest);
    }
    let canonical_runtime_dir = runtime_dir.canonicalize().map_err(|error| {
        InferenceError::AssetRead(format!("{}: {error}", runtime_dir.display()))
    })?;
    if !canonical_runtime_dir.starts_with(&canonical_model_dir) {
        return Err(InferenceError::UnsafeAssetPath("runtime".into()));
    }
    Ok(canonical_runtime_dir)
}

fn verify_runtime_policy(manifest: &RuntimeManifest) -> Result<(), InferenceError> {
    let platform = runtime_platform()?;
    if manifest.main_library.sha256 == KNOWN_TELEMETRY_ENABLED_WINDOWS_RUNTIME_SHA256
        || !manifest.telemetry_disabled
        || manifest
            .build_options
            .iter()
            .any(|option| option == "onnxruntime_USE_TELEMETRY=ON")
    {
        return Err(InferenceError::RuntimeTelemetryEnabled);
    }
    if manifest.version != RUNTIME_VERSION
        || manifest.api != RUNTIME_API
        || manifest.source_repository != RUNTIME_SOURCE_REPOSITORY
        || manifest.source_commit != RUNTIME_SOURCE_COMMIT
        || manifest.platform != platform
        || !manifest.cpu_only
        || !manifest
            .build_options
            .iter()
            .any(|option| option == "onnxruntime_USE_TELEMETRY=OFF")
        || !manifest
            .build_options
            .iter()
            .any(|option| option == "onnxruntime_BUILD_SHARED_LIB=ON")
        || manifest
            .build_options
            .iter()
            .any(|option| option == "onnxruntime_BUILD_SHARED_LIB=OFF")
    {
        return Err(InferenceError::RuntimeManifest(
            "version, API, source, platform, CPU, or telemetry-off build metadata is not approved"
                .into(),
        ));
    }
    Ok(())
}

fn verify_runtime_assets(runtime_dir: &Path) -> Result<PathBuf, InferenceError> {
    if !runtime_dir.is_absolute() {
        return Err(InferenceError::RuntimePathNotAbsolute);
    }
    let manifest = read_runtime_manifest(runtime_dir)?;
    verify_runtime_policy(&manifest)?;

    let main_relative = safe_relative_path(&manifest.main_library.path)?;
    if main_relative.components().count() != 1 {
        return Err(InferenceError::UnsafeAssetPath(
            manifest.main_library.path.clone(),
        ));
    }
    let main_path = confined_file(runtime_dir, &manifest.main_library.path)?;
    let bytes = fs::read(&main_path)
        .map_err(|e| InferenceError::MissingRuntime(format!("{}: {e}", main_path.display())))?;
    if sha256(&bytes) != manifest.main_library.sha256 {
        return Err(InferenceError::RuntimeHashMismatch(
            manifest.main_library.path.clone(),
        ));
    }
    for companion in &manifest.companion_libraries {
        let path = confined_file(runtime_dir, &companion.path)?;
        let bytes = fs::read(&path)
            .map_err(|e| InferenceError::MissingRuntime(format!("{}: {e}", path.display())))?;
        if sha256(&bytes) != companion.sha256 {
            return Err(InferenceError::RuntimeHashMismatch(companion.path.clone()));
        }
    }
    Ok(main_path)
}

fn add_fastembed_special_tokens(
    tokenizer: &mut Tokenizer,
    special_tokens_map: &[u8],
) -> Result<(), InferenceError> {
    let special_tokens: serde_json::Value = serde_json::from_slice(special_tokens_map)
        .map_err(|e| InferenceError::Tokenizer(e.to_string()))?;
    if let Some(tokens) = special_tokens.as_object() {
        for value in tokens.values() {
            let token = if let Some(content) = value.as_str() {
                Some(AddedToken {
                    content: content.to_string(),
                    special: true,
                    ..Default::default()
                })
            } else if let Some(value) = value.as_object() {
                match (
                    value.get("content").and_then(serde_json::Value::as_str),
                    value
                        .get("single_word")
                        .and_then(serde_json::Value::as_bool),
                    value.get("lstrip").and_then(serde_json::Value::as_bool),
                    value.get("rstrip").and_then(serde_json::Value::as_bool),
                    value.get("normalized").and_then(serde_json::Value::as_bool),
                ) {
                    (
                        Some(content),
                        Some(single_word),
                        Some(lstrip),
                        Some(rstrip),
                        Some(normalized),
                    ) => Some(AddedToken {
                        content: content.to_string(),
                        special: true,
                        single_word,
                        lstrip,
                        rstrip,
                        normalized,
                    }),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(token) = token {
                tokenizer
                    .add_special_tokens([token])
                    .map_err(|e| InferenceError::Tokenizer(e.to_string()))?;
            }
        }
    }
    Ok(())
}

fn runtime_environment(
    runtime_path: &Path,
) -> Result<ort::environment::Environment, InferenceError> {
    static ENVIRONMENT: OnceLock<Mutex<Option<(PathBuf, ort::environment::Environment)>>> =
        OnceLock::new();
    let mut slot = ENVIRONMENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| InferenceError::RuntimeLoad("environment lock is poisoned".into()))?;
    if let Some((loaded_path, environment)) = slot.as_ref() {
        if loaded_path != runtime_path {
            return Err(InferenceError::RuntimeAlreadyInitialized);
        }
        return Ok(environment.clone());
    }
    let environment = ort::init_from(runtime_path)
        .map_err(|e| InferenceError::RuntimeLoad(e.to_string()))?
        .with_telemetry(false)
        .build()
        .map_err(|e| InferenceError::RuntimeLoad(e.to_string()))?;
    *slot = Some((runtime_path.to_path_buf(), environment.clone()));
    Ok(environment)
}

fn validate_embedding(vector: Vec<f32>) -> Result<Vec<f32>, InferenceError> {
    if vector.len() != DIMENSIONS {
        return Err(InferenceError::InvalidEmbeddingDimension(vector.len()));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(InferenceError::InvalidEmbeddingValue);
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm == 0.0 || (norm - 1.0).abs() > 0.001 {
        return Err(InferenceError::InvalidEmbeddingValue);
    }
    Ok(vector)
}

impl LocalEmbeddings {
    pub(crate) fn load_vector_model(model_dir: &Path) -> Result<Self, String> {
        Self::load(model_dir).map_err(|error| error.to_string())
    }

    fn load(model_dir: &Path) -> Result<Self, InferenceError> {
        let manifest = model_manifest()?;
        let runtime_dir = confined_runtime_dir(model_dir)?;
        let runtime_path = verify_runtime_assets(&runtime_dir)?;
        let mut assets = read_model_assets(model_dir, &manifest)?;
        let take = |assets: &mut HashMap<String, Vec<u8>>, path: &str| {
            assets
                .remove(path)
                .ok_or_else(|| InferenceError::MissingAsset(path.to_string()))
        };
        let tokenizer_file = take(&mut assets, "tokenizer.json")?;
        let config_file = take(&mut assets, "config.json")?;
        let special_tokens_map_file = take(&mut assets, "special_tokens_map.json")?;
        let tokenizer_config_file = take(&mut assets, "tokenizer_config.json")?;
        let model_file = take(&mut assets, "onnx/model.onnx")?;

        let mut tokenizer = Tokenizer::from_bytes(tokenizer_file.as_slice())
            .map_err(|e| InferenceError::Tokenizer(e.to_string()))?;
        tokenizer
            .with_truncation(None)
            .map_err(|e| InferenceError::Tokenizer(e.to_string()))?;
        add_fastembed_special_tokens(&mut tokenizer, &special_tokens_map_file)?;

        let environment = runtime_environment(&runtime_path)?;
        let mut model = UserDefinedEmbeddingModel::new(
            model_file,
            TokenizerFiles {
                tokenizer_file,
                config_file,
                special_tokens_map_file,
                tokenizer_config_file,
            },
        );
        model.pooling = Some(Pooling::Mean);
        let options = InitOptionsUserDefined::new()
            .with_environment(environment.clone())
            .with_max_length(manifest.max_tokens)
            .with_auto_device(AutoDevicePolicy::PreferCPU);
        let model = TextEmbedding::try_new_from_user_defined(model, options)
            .map_err(|e| InferenceError::ModelLoad(e.to_string()))?;
        Ok(Self {
            model,
            tokenizer,
            _environment: environment,
            query_prefix: manifest.query_prefix,
            passage_prefix: manifest.passage_prefix,
            max_tokens: manifest.max_tokens,
        })
    }

    fn embed_query(&mut self, text: &str) -> Result<EmbeddedText, InferenceError> {
        self.embed(&self.query_prefix.clone(), text)
    }

    pub(crate) fn embed_query_vector(&mut self, text: &str) -> Result<EmbeddedText, String> {
        self.embed_query(text).map_err(|error| error.to_string())
    }

    pub(crate) fn embed_passage_vector(
        &mut self,
        title: &str,
        body: &str,
    ) -> Result<EmbeddedText, String> {
        self.embed_passage(title, body)
            .map_err(|error| error.to_string())
    }

    fn embed_passage(&mut self, title: &str, body: &str) -> Result<EmbeddedText, InferenceError> {
        self.embed(&self.passage_prefix.clone(), &format!("{title}\n{body}"))
    }

    fn embed(&mut self, prefix: &str, text: &str) -> Result<EmbeddedText, InferenceError> {
        let input = format!("{prefix}{text}");
        let token_count = self
            .tokenizer
            .encode(input.as_str(), true)
            .map_err(|e| InferenceError::Tokenizer(e.to_string()))?
            .get_ids()
            .len();
        let mut embeddings = self
            .model
            .embed([input.as_str()], None)
            .map_err(|e| InferenceError::ModelLoad(e.to_string()))?;
        let vector = embeddings.pop().ok_or(InferenceError::EmptyEmbedding)?;
        Ok(EmbeddedText {
            vector: validate_embedding(vector)?,
            embedding_truncated: token_count > self.max_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{validate_embedding, verify_model_assets, verify_runtime_assets, InferenceError};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("riltra-inference-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct EnvVarGuard {
        name: &'static str,
        old: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &Path) -> Self {
            let old = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, old }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.old {
                std::env::set_var(self.name, value);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }

    fn runtime_library_name() -> &'static str {
        if cfg!(target_os = "windows") {
            "onnxruntime.dll"
        } else if cfg!(target_os = "macos") {
            "libonnxruntime.dylib"
        } else {
            "libonnxruntime.so.1.24.4"
        }
    }

    fn write_runtime_manifest(
        runtime_dir: &Path,
        bytes: &[u8],
        digest: &str,
        telemetry_disabled: bool,
        source_commit: &str,
        library_path: &str,
    ) {
        std::fs::create_dir_all(runtime_dir).unwrap();
        std::fs::write(runtime_dir.join(runtime_library_name()), bytes).unwrap();
        let manifest = serde_json::json!({
            "version": "1.24.4",
            "api": 24,
            "source_repository": "https://github.com/microsoft/onnxruntime",
            "source_commit": source_commit,
            "platform": super::runtime_platform().unwrap(),
            "cpu_only": true,
            "telemetry_disabled": telemetry_disabled,
            "build_options": [
                "onnxruntime_USE_TELEMETRY=OFF",
                "onnxruntime_BUILD_SHARED_LIB=ON"
            ],
            "main_library": { "path": library_path, "sha256": digest },
            "companion_libraries": []
        });
        std::fs::write(
            runtime_dir.join("runtime-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "''"));
            let command = format!(
                "New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
                quote(link),
                quote(target)
            );
            let output = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &command])
                .output()?;
            if output.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )))
            }
        }
    }

    fn unicode_normalizing_tokenizer() -> tokenizers::Tokenizer {
        let model = tokenizers::models::wordpiece::WordPiece::builder()
            .vocab([
                ("[UNK]".to_string(), 0),
                ("word".to_string(), 1),
                ("<".to_string(), 2),
                ("mask".to_string(), 3),
                (">".to_string(), 4),
            ])
            .unk_token("[UNK]".to_string())
            .build()
            .unwrap();
        let mut tokenizer = tokenizers::Tokenizer::new(model);
        tokenizer
            .with_normalizer(Some(tokenizers::normalizers::NFKC))
            .unwrap();
        tokenizer.with_pre_tokenizer(Some(tokenizers::pre_tokenizers::bert::BertPreTokenizer));
        tokenizer
    }

    #[test]
    fn missing_model_assets_are_classified_without_creating_files() {
        let temp = TempDir::new();
        let missing = temp.0.join("not-provisioned");

        let error = verify_model_assets(&missing).unwrap_err();

        assert!(matches!(error, InferenceError::MissingAsset(path) if path == "config.json"));
        assert!(!missing.exists());
    }

    #[test]
    fn corrupted_model_asset_is_rejected_by_digest() {
        let temp = TempDir::new();
        std::fs::write(temp.0.join("config.json"), b"{}").unwrap();

        let error = verify_model_assets(&temp.0).unwrap_err();

        assert!(matches!(error, InferenceError::AssetHashMismatch(path) if path == "config.json"));
    }

    #[test]
    fn invalid_embedding_dimension_and_non_finite_values_are_rejected() {
        assert!(matches!(
            validate_embedding(vec![0.0; 383]),
            Err(InferenceError::InvalidEmbeddingDimension(383))
        ));
        assert!(matches!(
            validate_embedding(vec![f32::NAN; 384]),
            Err(InferenceError::InvalidEmbeddingValue)
        ));
        assert!(matches!(
            validate_embedding(vec![0.0; 384]),
            Err(InferenceError::InvalidEmbeddingValue)
        ));
    }

    #[test]
    fn normalized_special_token_count_matches_fastembed_for_fullwidth_input() {
        let special_tokens = br#"{"mask_token":"<mask>"}"#;
        let input = "word ＜mask＞ word";

        let mut loader_tokenizer = unicode_normalizing_tokenizer();
        super::add_fastembed_special_tokens(&mut loader_tokenizer, special_tokens).unwrap();
        let loader_ids = loader_tokenizer
            .encode(input, true)
            .unwrap()
            .get_ids()
            .to_vec();

        let mut fastembed_config = unicode_normalizing_tokenizer();
        fastembed_config
            .add_special_tokens([tokenizers::AddedToken {
                content: "<mask>".to_string(),
                special: true,
                ..Default::default()
            }])
            .unwrap();
        let fastembed_ids = fastembed_config
            .encode(input, true)
            .unwrap()
            .get_ids()
            .to_vec();
        assert_eq!(loader_ids, fastembed_ids);
        assert_eq!(fastembed_ids.len(), 3);

        let mut legacy_config = unicode_normalizing_tokenizer();
        legacy_config
            .add_special_tokens([tokenizers::AddedToken::from("<mask>", true)])
            .unwrap();
        let legacy_count = legacy_config.encode(input, true).unwrap().get_ids().len();
        assert_eq!(legacy_count, 5);
    }

    #[test]
    fn runtime_directory_must_be_absolute_before_its_manifest_is_read() {
        let error = verify_runtime_assets(Path::new("runtime")).unwrap_err();

        assert!(matches!(error, InferenceError::RuntimePathNotAbsolute));
    }

    #[test]
    fn relative_model_directory_is_resolved_before_runtime_validation() {
        assert!(matches!(
            super::LocalEmbeddings::load(Path::new(".")),
            Err(InferenceError::MissingRuntimeManifest)
        ));
    }

    #[test]
    fn explicit_model_directory_symlink_remains_supported() {
        let temp = TempDir::new();
        let real_model_dir = temp.0.join("model-real");
        let explicit_model_dir = temp.0.join("model-link");
        std::fs::create_dir_all(&real_model_dir).unwrap();
        create_directory_link(&real_model_dir, &explicit_model_dir).unwrap();

        assert!(matches!(
            super::LocalEmbeddings::load(&explicit_model_dir),
            Err(InferenceError::MissingRuntimeManifest)
        ));
    }

    #[test]
    fn runtime_directory_symlink_cannot_escape_model_directory() {
        let temp = TempDir::new();
        let model_dir = temp.0.join("model");
        let outside_runtime = temp.0.join("outside/runtime");
        std::fs::create_dir_all(&model_dir).unwrap();
        write_runtime_manifest(
            &outside_runtime,
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            true,
            super::RUNTIME_SOURCE_COMMIT,
            runtime_library_name(),
        );
        create_directory_link(&outside_runtime, &model_dir.join("runtime")).unwrap();

        assert!(matches!(
            super::LocalEmbeddings::load(&model_dir),
            Err(InferenceError::UnsafeAssetPath(path)) if path == "runtime"
        ));
    }

    #[test]
    fn missing_runtime_manifest_is_reported_without_creating_files() {
        let temp = TempDir::new();

        let error = verify_runtime_assets(&temp.0).unwrap_err();

        assert!(matches!(error, InferenceError::MissingRuntimeManifest));
        assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 0);
    }

    #[test]
    fn runtime_manifest_hash_must_match_the_local_library() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        write_runtime_manifest(
            &runtime_dir,
            b"not abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            true,
            super::RUNTIME_SOURCE_COMMIT,
            runtime_library_name(),
        );

        let error = verify_runtime_assets(&runtime_dir).unwrap_err();

        assert!(
            matches!(error, InferenceError::RuntimeHashMismatch(path) if path == runtime_library_name())
        );
    }

    #[test]
    fn runtime_manifest_must_attest_telemetry_disabled_build() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        write_runtime_manifest(
            &runtime_dir,
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            false,
            super::RUNTIME_SOURCE_COMMIT,
            runtime_library_name(),
        );

        let error = verify_runtime_assets(&runtime_dir).unwrap_err();

        assert!(matches!(error, InferenceError::RuntimeTelemetryEnabled));
    }

    #[test]
    fn runtime_manifest_requires_telemetry_metadata_and_rejects_conflicts() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        write_runtime_manifest(
            &runtime_dir,
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            true,
            super::RUNTIME_SOURCE_COMMIT,
            runtime_library_name(),
        );
        let manifest_path = runtime_dir.join("runtime-manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest
            .as_object_mut()
            .unwrap()
            .remove("telemetry_disabled");
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        assert!(matches!(
            verify_runtime_assets(&runtime_dir),
            Err(InferenceError::RuntimeManifest(_))
        ));

        manifest["telemetry_disabled"] = serde_json::Value::Bool(true);
        manifest["build_options"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::Value::String(
                "onnxruntime_USE_TELEMETRY=ON".to_string(),
            ));
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        assert!(matches!(
            verify_runtime_assets(&runtime_dir),
            Err(InferenceError::RuntimeTelemetryEnabled)
        ));
    }

    #[test]
    fn runtime_manifest_must_pin_the_reviewed_upstream_commit() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        write_runtime_manifest(
            &runtime_dir,
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            true,
            &"0".repeat(40),
            runtime_library_name(),
        );

        let error = verify_runtime_assets(&runtime_dir).unwrap_err();

        assert!(matches!(error, InferenceError::RuntimeManifest(_)));
    }

    #[test]
    fn known_telemetry_enabled_windows_runtime_is_rejected_even_if_misdeclared() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        write_runtime_manifest(
            &runtime_dir,
            b"abc",
            super::KNOWN_TELEMETRY_ENABLED_WINDOWS_RUNTIME_SHA256,
            true,
            super::RUNTIME_SOURCE_COMMIT,
            runtime_library_name(),
        );

        let error = verify_runtime_assets(&runtime_dir).unwrap_err();

        assert!(matches!(error, InferenceError::RuntimeTelemetryEnabled));
    }

    #[test]
    fn reviewed_local_runtime_manifest_and_hash_are_accepted() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        write_runtime_manifest(
            &runtime_dir,
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            true,
            super::RUNTIME_SOURCE_COMMIT,
            runtime_library_name(),
        );

        let path = verify_runtime_assets(&runtime_dir).unwrap();

        assert_eq!(
            path,
            runtime_dir
                .join(runtime_library_name())
                .canonicalize()
                .unwrap()
        );
    }

    #[test]
    fn runtime_manifest_cannot_escape_its_runtime_directory() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        write_runtime_manifest(
            &runtime_dir,
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            true,
            super::RUNTIME_SOURCE_COMMIT,
            "../onnxruntime.dll",
        );

        let error = verify_runtime_assets(&runtime_dir).unwrap_err();

        assert!(matches!(error, InferenceError::UnsafeAssetPath(_)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_runtime_library_is_rejected() {
        let temp = TempDir::new();
        let runtime_dir = temp.0.join("runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("target.dll"), b"abc").unwrap();
        std::os::unix::fs::symlink(
            runtime_dir.join("target.dll"),
            runtime_dir.join(runtime_library_name()),
        )
        .unwrap();
        let manifest = serde_json::json!({
            "version": "1.24.4",
            "api": 24,
            "source_repository": "https://github.com/microsoft/onnxruntime",
            "source_commit": super::RUNTIME_SOURCE_COMMIT,
            "platform": super::runtime_platform().unwrap(),
            "cpu_only": true,
            "telemetry_disabled": true,
            "build_options": ["onnxruntime_USE_TELEMETRY=OFF", "onnxruntime_BUILD_SHARED_LIB=ON"],
            "main_library": { "path": runtime_library_name(), "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad" },
            "companion_libraries": []
        });
        std::fs::write(
            runtime_dir.join("runtime-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let error = verify_runtime_assets(&runtime_dir).unwrap_err();

        assert!(matches!(error, InferenceError::UnsafeAssetPath(_)));
    }

    #[test]
    #[ignore = "requires explicit local model and ONNX Runtime assets; never downloads"]
    fn local_model_produces_normalized_384_dimension_embeddings_offline() {
        let model_dir = std::env::var_os("RILTRA_TEST_MODEL_DIR")
            .map(PathBuf::from)
            .expect("set RILTRA_TEST_MODEL_DIR to the provisioned E5 model directory");
        let temp = TempDir::new();
        let cache = temp.0.join("must-not-be-created");
        let _cache_env = EnvVarGuard::set("FASTEMBED_CACHE_DIR", &cache);

        let mut embeddings = super::LocalEmbeddings::load(&model_dir).unwrap();
        let query = embeddings.embed_query("local text").unwrap();
        let passage = embeddings.embed_passage("title", "body").unwrap();
        let long_text = "token ".repeat(700);
        let long = embeddings.embed_query(&long_text).unwrap();
        let mut tokenizer =
            tokenizers::Tokenizer::from_file(model_dir.join("tokenizer.json")).unwrap();
        let special_tokens: serde_json::Value = serde_json::from_slice(
            &std::fs::read(model_dir.join("special_tokens_map.json")).unwrap(),
        )
        .unwrap();
        for value in special_tokens.as_object().unwrap().values() {
            if let Some(content) = value.as_str() {
                tokenizer
                    .add_special_tokens([tokenizers::AddedToken {
                        content: content.to_string(),
                        special: true,
                        ..Default::default()
                    }])
                    .unwrap();
            }
        }
        tokenizer.with_truncation(None).unwrap();
        let long_token_count = tokenizer
            .encode(format!("query: {long_text}"), true)
            .unwrap()
            .get_ids()
            .len();

        for embedding in [&query, &passage, &long] {
            assert_eq!(embedding.vector.len(), 384);
            assert!(embedding.vector.iter().all(|value| value.is_finite()));
            let norm = embedding
                .vector
                .iter()
                .map(|value| value * value)
                .sum::<f32>()
                .sqrt();
            assert!((norm - 1.0).abs() < 0.001, "norm={norm}");
        }
        assert_ne!(
            query.vector, passage.vector,
            "query and passage prefixes differ"
        );
        assert!(!query.embedding_truncated);
        assert_eq!(long.embedding_truncated, long_token_count > 512);
        assert!(long.embedding_truncated);
        assert!(!cache.exists(), "local inference created a model cache");
    }
}
