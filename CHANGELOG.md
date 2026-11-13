# Changelog

All notable changes to Riltra are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, the [stability notes](README.md#stability-pre-10) say what stays fixed within `0.1.x`.

## [Unreleased]

### Added

- `riltra changes` and the MCP tool `riltra_code_changes`: the definitions a git change
  touches (scopes unstaged, staged, all, compare), from a live parse of Python,
  Rust and TypeScript files, with caller counts as the index holds them.
- Syntax facts for Go (`.go`) and Java (`.java`): definitions, imports and
  call candidates. Syntax-only; calls stay `POTENTIAL_CALL` and no SCIP or
  language-server evidence is claimed.
- Opt-in `--progress` flag for `scan` and `index`: prints live progress lines
  (files parsed and elapsed seconds) to stderr only. The JSON on stdout is
  unchanged, and the flag is off by default.
- By-name anchors for `callers`, `callees`, `trace`, `impact` and `context`:
  `--target <name>` with optional `--in` and `--kind` (MCP: `target`, `in`,
  `kind`). The MCP graph and context tools no longer require `path`, `line`
  and `col` in their schema when `target` is given. Ambiguous and unknown
  names answer a document (`ambiguous`, `not-found`, `no-index`) instead of
  guessing. `--kind` takes the kind as stored per language (Python functions
  and methods are `function`; Go, JavaScript, TypeScript and Java methods are
  `method`).

- `impact --summary` and `--include-tests` (MCP: `summary`, `include_tests`) add a counted summary of the impact traversal: totals by distance and basis, distinct dependent files, and a labelled test-path heuristic. The summary describes this traversal, `--limit` also caps candidates per node (so a small limit can lower the counts; `candidates_complete: false` says so), and `paths` is never filtered. `summary.verdict` and `summary.unresolved` are copied from the document, so they can differ with `--offset` or `--limit` (the counts do not depend on `--offset`); `candidates_complete` is also false after a path or edge budget stop, not only after the candidate cap.

### Changed

- `impact` now lists name-matched callers as paths with `basis: "candidate-name"`, without following them further. Each impact path gains a `basis` key (`proven`, `unproven` or `candidate-name`), and the answer gains a `candidates` object. The verdict is unchanged and stays `UNKNOWN` while a candidate path exists; `completeness` counts and `visits.edges` include the candidate paths and rows.
- `callers`, `callees`, `trace`, `impact` and `context` add `candidates` and `candidate_scope` to each `POTENTIAL_CALL` edge: the count of name-matched function and method definitions (declarations included) and their place relative to the caller (`same-file`, `imported-file`, `other-file`, `none`). `imported-file` is a file-level link, not a resolution. In the CLI output of `callers`, `callees`, `trace` and `impact` they are the last two keys of the edge object; `context` and the MCP tools sort their keys, so the two keys sit in sorted order there. Derived at read time; no schema or parser change, and the edge is not promoted to `CALLS`.

### Fixed

- Syntax calibration against hand-written fixtures: Rust enums, traits, trait
  method signatures and type aliases; C and C++ enums and typedefs; C++ `using`
  aliases; and TypeScript enums, type aliases and abstract classes are now
  extracted as definitions. A bare `struct X *p;` or forward declaration
  (`class X;`) is no longer reported as a definition. Rust
  `extern` function signatures are now function definitions. `PARSER_VERSION` is now
  11 (Go and Java were 9 and 10), so existing indexes rebuild.
- JavaScript and TypeScript `function*` declarations are extracted as function
  definitions.
- JavaScript and TypeScript names bound to a function expression
  (`const f = () => {}`, `const f = function () {}`, class fields such as
  `m = () => {}`) are now definitions. Calls inside them stay without a named
  owner: an anonymous callable has no persisted named owner. `PARSER_VERSION`
  is now 12.

- C and C++ `class EXPORT Name {...}` and `struct EXPORT Name {...}` (an unknown
  macro between the keyword and the name) are now class and struct definitions;
  they were reported as a function named after the class. `PARSER_VERSION` is
  now 13. Member functions with bodies of such a class are recorded as plain
  functions; data members and member declarations are not recorded. A macro
  with arguments (`class API(1) Name`) still fails to parse and is still
  reported as a function. A macro that hides a parameter list can be read as a
  struct.

### Known behaviour of the new definition kinds

- `callers` anchored on a type, enum or trait now resolves and lists heuristic
  `POTENTIAL_CALL` edges whose name matches (for example a TypeScript `type Foo`
  beside a function `Foo`, or a C++ functional cast `V(3)` for `using V = int`).
  `impact` and `trace` on such an anchor return nothing.
- A Rust trait method signature and its impl in the same file are two
  candidates, so calls can be labelled `ambiguous:2` instead of `dynamic`. Edge
  counts do not change.
- Existing knowledge bindings made at a bare `struct X *p;` or forward
  declaration, or inside `typedef struct X {...} X;`, can read as unresolved
  after the reindex because the definition they anchored on changed or moved.

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
