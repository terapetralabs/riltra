# ONNX Runtime source provenance

Status: decision record, 2026-11-20. Applies to the native-inference CI job and release builds that use `release/prepare-native-semantic.py`.

## What happened

`github.com/microsoft/onnxruntime` became unavailable around 2026-11-18: `git ls-remote` returns "Repository not found" and the GitHub API returns 404. No official Microsoft or GitHub statement was found at the time of writing; community reports describe a removal, not a move.

Effect on Riltra: the three `native-inference` jobs failed at the `git clone` step of `release/prepare-native-semantic.py` (first seen on pull request #80, 2026-11-19). No Riltra code change caused it.

## Decision

Riltra keeps building ONNX Runtime 1.24.4 from source at commit `2d924974ef147392ced8409d36bd6d2e7fcc8a74` (pinned as `RUNTIME_SOURCE_COMMIT` in `crates/cli/src/inference.rs`).

CI fetches that commit from the mirror `https://github.com/terapetralabs/onnxruntime`, created from the public fork `https://github.com/intel/onnxruntime`, which still contains the commit (message "ORT 1.24.4 release cherry pick round 1 (#27682)"). The mirror is unmodified; its tag `riltra-ort-1.24.4` points to the pinned commit so it stays reachable if branches change.

## Two different URLs

| Name | Value | Meaning |
| --- | --- | --- |
| `SOURCE_REPOSITORY` | `https://github.com/microsoft/onnxruntime` | Provenance identity of the commit, written into runtime manifests and validated by `inference.rs` (`RUNTIME_SOURCE_REPOSITORY`). Changing it would reject existing manifests. |
| `FETCH_REPOSITORY_URL` | `https://github.com/terapetralabs/onnxruntime` | Where CI downloads the commit. May change without affecting manifests. |

## Why the content is still trustworthy

1. A git commit id is a hash of the whole source tree and history.
2. `main()` in `release/prepare-native-semantic.py` checks that `git rev-parse HEAD` equals the pin and fails otherwise.
3. Submodules are pinned by commit, and `cmake/deps.txt` dependencies are pinned by hash.

Limits, stated plainly:

- Git uses SHA-1. It is hardened against known collision attacks, but it is not a modern hash.
- The mirror protects only the main repository. Submodules and `cmake/deps.txt` downloads still come from their own upstreams.
- `microsoft/onnxruntime-extensions` is also unavailable. It is believed unused, because extensions are not enabled by the build options. A green `native-inference` run must confirm this.

**2026-11-20 validation update:** The [PR #80 native-inference run](https://github.com/terapetralabs/riltra/actions/runs/44669579398) built the pinned runtime and passed the isolated native query and lexical-fallback steps on Windows x64, Ubuntu x64, and macOS Intel. This establishes that those tested builds did not require the unavailable extensions repository; it does not establish that every ONNX Runtime build configuration can omit it.

## Alternatives considered

- (a) Prebuilt binaries from NuGet `Microsoft.ML.OnnxRuntime` or PyPI. Rejected for now: Riltra builds with `onnxruntime_USE_TELEMETRY=OFF`, and `inference.rs` refuses the official Windows 1.24.4 `onnxruntime.dll` (SHA-256 `b95efb2113b603bbbf3f191061c5516a871ed546893c820e4f3b7b6c358dbf2a`) as telemetry-enabled.
- (b) Fetching directly from `intel/onnxruntime`. Same integrity, but availability depends on a third party. Acceptable only as a temporary fallback.
- (c) A pure-Rust runtime (for example tract or candle). Not adopted: large change to `inference.rs`, CI and docs; stored embeddings would need rebuilding; no measurement exists yet.

## When to revisit

1. Microsoft's repository is available again. Verify that `git ls-remote https://github.com/microsoft/onnxruntime` works and that `gh api repos/microsoft/onnxruntime/commits/<pin>` returns the commit. Then set `FETCH_REPOSITORY_URL` back to the Microsoft URL. `SOURCE_REPOSITORY` does not change. Keep the mirror until one green run.
2. Switching the fetch source to Intel or another mirror. Verify that the pinned commit exists there. Change only `FETCH_REPOSITORY_URL`, and update this record.
3. Upgrading ONNX Runtime. Pick the new release commit and add it (with a tag) to the mirror. Update `RUNTIME_VERSION`, `RUNTIME_API` and `RUNTIME_SOURCE_COMMIT` in `inference.rs`, and the example in `docs/semantic-inference.md`. Then run native-inference.
4. Replacing ONNX Runtime entirely. Write a new decision record. Affected places include `crates/cli/src/inference.rs`, `release/prepare-native-semantic.py`, the `semantic-native` workflow, `docs/semantic-inference.md`, `docs/licensing.md` and stored knowledge embeddings.

## How to verify

```bash
git ls-remote https://github.com/terapetralabs/onnxruntime refs/tags/riltra-ort-1.24.4
gh api repos/terapetralabs/onnxruntime/commits/2d924974ef147392ced8409d36bd6d2e7fcc8a74 --jq .sha
git ls-remote https://github.com/microsoft/onnxruntime HEAD
```
