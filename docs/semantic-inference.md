# Optional local semantic inference

## Using semantic knowledge search

The `semantic-knowledge` feature is default-off and is not included in official
packages. A source build of the feature also needs the Protocol Buffers
compiler `protoc` on `PATH`; the LanceDB build script compiles protobuf
sources. CI pins and version-checks `protoc` 33.5 inside its own runners before
the optional-feature build ([workflow](../.github/workflows/ci.yml)). That CI
setup does not install `protoc` for local builds. From the repository root,
after installing the prerequisite, build a Riltra binary with the feature:

```sh
cargo install --path crates/cli --locked --features semantic-knowledge
```

Use `knowledge vector build --repo <name> --model-dir <model-dir>` to create the
derived index, then request semantic results with `knowledge search --semantic`
or MCP `riltra_knowledge_search` with `semantic: true`. The query process reads
assets from `RILTRA_MODEL_DIR`.
Vector data is derived from active knowledge records; SQLite remains canonical.
If the feature, model/runtime assets, or usable vector index is unavailable,
search falls back to FTS5 with an explicit reason. Default lexical searches do
not load the model.

Inspect the derived index with `riltra knowledge vector status --repo <name>`.
After changing active knowledge or model assets, rebuild it with the same build
command. To discard derived vectors, run `riltra knowledge vector clear --repo <name>`;
knowledge records remain in SQLite. A stale or missing index causes an explicit
lexical fallback until it is rebuilt.

Before `knowledge restore`, close all processes using the repository's knowledge
store, including vector builds, status checks, and searches. Concurrent access
during restore is not supported. After restoring, check vector status and rebuild
the index if it is stale.

The local model is `intfloat/multilingual-e5-small`. Only record title and body
are embedded, up to 512 tokens; `embedding_truncated` marks truncated passages.
An overlong query falls back with `query-truncated`, while lexical search still
uses the full text. Semantic search requires a nonempty query, does not support
historical-status flags or a nonzero offset, and reports inexact totals rather
than offering subsequent pages. A similarity match is a relevance candidate,
not proof of a relationship to code.

The `semantic-knowledge` Cargo feature is off by default. Building it does not
fetch a model or an ONNX Runtime library. The inference loader accepts local
model files whose hashes match `crates/cli/src/model_manifest.json`. Obtain each
listed file from the exact model revision in that manifest, preserve its
relative path, and verify its SHA-256 before use. Provisioning is an explicit
operator step; neither the build nor a query downloads the assets. A separate
runtime manifest is also required under the supplied model directory:

```text
<model-dir>/
  config.json
  tokenizer.json
  tokenizer_config.json
  special_tokens_map.json
  onnx/model.onnx
  runtime/
    runtime-manifest.json
    <runtime library files>
```

`runtime-manifest.json` identifies the local shared library, its SHA-256,
companion libraries, target platform, and build attestation. It must use ONNX
Runtime 1.24.4, API 24, and upstream source commit
`2d924974ef147392ced8409d36bd6d2e7fcc8a74`. The accepted platform strings are
`windows-x86_64`, `linux-x86_64`, and `macos-x86_64`. `main_library.path` must
be a filename in `runtime/`; companion paths must remain relative to that
directory. The loader rejects traversal and symlinks and hashes every listed
runtime file before loading it.

Example shape (replace library names and hash values with those from the local
build):

```json
{
  "version": "1.24.4",
  "api": 24,
  "source_repository": "https://github.com/microsoft/onnxruntime",
  "source_commit": "2d924974ef147392ced8409d36bd6d2e7fcc8a74",
  "platform": "windows-x86_64",
  "cpu_only": true,
  "telemetry_disabled": true,
  "build_options": [
    "onnxruntime_USE_TELEMETRY=OFF",
    "onnxruntime_BUILD_SHARED_LIB=ON"
  ],
  "main_library": {
    "path": "onnxruntime.dll",
    "sha256": "<64 lowercase hexadecimal digits>"
  },
  "companion_libraries": [
    {
      "path": "onnxruntime_providers_shared.dll",
      "sha256": "<64 lowercase hexadecimal digits>"
    }
  ]
}
```

The manifest is an operator-reviewed local assertion. Its hash detects a file
that differs from the reviewed manifest, but the unsigned manifest and binary
do not prove build provenance or prove that telemetry was disabled. Build
ONNX Runtime from the pinned source with telemetry disabled, inspect the
resulting build configuration, then record the exact library hashes. The
loader rejects the known telemetry-enabled Microsoft Windows 1.24.4 DLL even
if its local manifest claims otherwise. No public archive currently includes
this optional runtime.

The [native CI workflow](../.github/workflows/semantic-native.yml) provisions
the pinned model and a telemetry-disabled runtime for its own test. On Windows
x64, Linux x64, and macOS Intel, [run 42611986561](https://github.com/terapetralabs/riltra/actions/runs/42611986561)
passed a small bilingual retrieval fixture, a direct outbound-denial probe,
query-side file snapshots, and a missing-model fallback test. This is a smoke
test for those assets and runners, not a general retrieval-quality or platform
support guarantee. The provisioning helper is CI-only; local operators must
provide and review their own assets and runtime manifest.
