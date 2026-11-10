# Changelog

All notable changes to Riltra are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, the [stability notes](README.md#stability-pre-10) say what stays fixed within `0.1.x`.

## [Unreleased]

## [0.2.0-rc.1] - 2026-11-10

### Added

- A `context` CLI and MCP query that composes bounded code graph and impact
  results with explicit knowledge bindings and optional lexical or local
  semantic matches. It keeps unlinked relevance matches separate from explicit
  bindings.
- Opt-in local semantic knowledge search with explicit vector build, status,
  and clear commands and an FTS5 fallback when the derived index or assets are
  unavailable. Official packages remain core-only.
- A manual workflow, `installer-phase-b`, that downloads the assets of a published release and runs the installer checks on them on hosted Windows, Linux and macOS x64 runners ([workflow](.github/workflows/installer-phase-b.yml)).

### Changed

- Python caller queries can include candidates for simple aliased imports. These remain heuristic, unverified candidates and do not establish a binding.

### Evidence status

- [Workflow run 41797080889](https://github.com/terapetralabs/riltra/actions/runs/41797080889) completed successfully for the Phase B Ubuntu, Windows and macOS Intel x64 jobs. The archive download and verification, acceptance-test, and installer-download-path steps passed in those jobs. Conditional targets outside those runners were skipped; this does not establish ARM64 coverage or least-privilege Windows execution.

## [0.1.0] - 2026-11-04

First pre-release. The version number is already in the build. Native archives for Windows x64, Linux x64 and macOS Intel x64 are built by a manually started workflow ([release/README.md](release/README.md)).

### Added

- Repository registry and a read-only Git fingerprint (`init`, `repo list`, `repo status`), and `doctor` health checks.
- Deterministic JSON output with a fixed key order and exit codes `0`, `2` and `3`; an optional per-repository `.riltraignore`.
- Syntax facts with line, column and byte spans (definitions, imports, call candidates) for Python, JavaScript, TypeScript and TSX, Rust, C and C++. Other extensions are reported as unsupported, never guessed (`scan`).
- A durable SQLite code index with source digests, provenance, full-text symbol search and explicit fresh, stale or unknown freshness (`index`, `search`).
- Relationship queries `callers`, `callees`, `trace` and `impact`, labelled by evidence: syntax-only `POTENTIAL_CALL` candidates, source-checked `CALLS` edges from a semantic provider, and impact answers that stay `UNKNOWN` or `partial` unless proven. For C++, `search --occurrences` lists where an exact name occurs as a labelled census.
- Opt-in semantic providers: SCIP import (`semantic import`) and queries to a language server you name by absolute path (`semantic query --lsp-server`).
- A knowledge store of revisioned records per repository, code-anchor bindings (`knowledge bind`, `unbind`, `at`), deterministic export, backup, verification and fail-closed restore.
- An MCP server over local stdio (`riltra mcp`) with thirteen tools: twelve read-only and one provider-backed (`riltra_code_semantic`).
- Release packaging: archives, SHA-256 checksums, third-party notices, a software bill of materials, and user-level installers for Windows (PowerShell) and for Linux and macOS (shell) ([release/INSTALL.md](release/INSTALL.md)).

### Scope and limits of the evidence

- This release makes no general performance, accuracy, product-comparison or agent-benefit claim. Supported scope and known limitations are listed in the [README](README.md).
- C++ results remain candidates, and macOS CI coverage is x64 only ([Known limitations](README.md#known-limitations)).
