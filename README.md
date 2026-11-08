# Riltra

[![ci](https://github.com/terapetralabs/riltra/actions/workflows/ci.yml/badge.svg)](https://github.com/terapetralabs/riltra/actions)

Riltra is a local code map and project notebook for developers and AI coding
agents. It indexes source code on your machine, helps you follow relationships
between code, and lets you link project decisions to the code they describe.
Use the CLI yourself or connect an AI coding agent through Riltra's local MCP
server.

Riltra's core needs no Riltra account or cloud service and sends no telemetry.
Optional language-server providers are separate programs; they may run project
tools or access the network. Riltra is free and open-source under the Apache
License 2.0 for personal, organizational, and commercial use.

To try Riltra on a repository, start with the [quick start](#quick-start).

## What Riltra helps you do

Think of Riltra as three connected tools: a searchable source index, a cautious
map of code relationships, and a notebook for project decisions. It helps with
questions such as:

| When you ask… | Riltra helps by… |
|---|---|
| “Where is this symbol defined?” | Finding indexed definitions and their source locations. |
| “What calls this function?” | Showing direct call-site evidence. Name-based matches remain candidates; they are not presented as proven targets. |
| “What might this change affect?” | Following bounded, supported relationships. If evidence is insufficient, the impact verdict stays `UNKNOWN`; incomplete coverage is marked `partial`. |
| “Why does this code have this constraint?” | Storing a revisioned project note and explicitly linking it to a source anchor. |
| “Can my coding agent use this context?” | Exposing the same local repository and knowledge queries through MCP. |

Graph and index answers report evidence status and freshness where applicable.
A candidate is a useful place to inspect, not proof that two symbols are
connected. An empty or partial result is not proof that nothing exists or that
a change is safe. This is how Riltra makes uncertainty visible instead of
guessing.

## Available today

The default core build does the following offline, with no runtime dependencies
beyond itself. The optional semantic-knowledge feature requires local model and
runtime assets, as described below.

- **Repository registry** — register local project roots (`init`) and list
  them as deterministic JSON (`repo list`).
- **Git fingerprint** (requires a Git executable) — per repository: HEAD commit,
  branch, dirty/untracked state, worktree detection, and ignore-file summary
  (`repo status`).
- **Health checks** — path confinement, record integrity, and git readability
  checks with explicit pass/fail (`doctor`).
- **Machine-readable output** — stable JSON with fixed key order, meaningful
  exit codes (`0` ok, `2` usage error, `3` repository/data error), and an
  optional `.riltraignore` file per repository.
- **Syntax baseline** — per-file definitions, imports, and unresolved call
  candidates with line/column spans and byte offsets for Python, JavaScript,
  TypeScript, Rust, C, and C++ (`scan`); other extensions are reported as
  unsupported, never guessed.
- **Durable code index** - SQLite-backed snapshots of the syntax facts with
  source digests, provenance, full-text search, and explicit
  fresh/stale/unknown freshness (`index`, `search`); one database file per
  repository under the local Riltra home.
- **Relationship graph** - `callers`, `callees`, `trace` and `impact` answer
  with evidence and explicit labels. Syntax-only relations are
  `POTENTIAL_CALL` candidates matched by name, never proven calls; only a
  semantic provider's source-checked evidence makes a `CALLS` edge, and an
  impact answer is `UNKNOWN` and `partial` unless it is proven. A C++ data
  member is a valid anchor, and `search --occurrences` lists where an exact
  name occurs (member accesses, the address of a member, same-file macro
  arguments) as a labelled name census, not a binding.
- **Opt-in semantic providers** - import a SCIP index (`semantic import`) or
  query a language server you name by absolute path (`semantic query
  --lsp-server`); the answers carry provider evidence and are never silently
  trusted.
- **Knowledge store** - revisioned decision/constraint/benchmark records per
  repository with deterministic export, backup, verification and fail-closed
  restore (`knowledge ...`). A code-anchor binding (`knowledge bind`,
  `unbind`, `at`) links one exact source anchor to one exact record revision
  with a typed relation. Bindings are stored explicitly, never inferred from
  names or paths; a changed source reports `stale` and is not rebound. A
  binding records a relationship, not the truth of the record.
- **Optional semantic knowledge search** - the default-off `semantic-knowledge`
  feature adds local vector search for active knowledge records, with explicit
  FTS5 fallback when optional assets or index state are unavailable. Manage the
  derived index with `knowledge vector build`, `knowledge vector status`, and
  `knowledge vector clear`; request
  semantic results with `knowledge search --semantic` or MCP `semantic: true`.
  Search reads provisioned model/runtime assets from `RILTRA_MODEL_DIR`; this
  feature does not embed source code, change default lexical search, or ship in
  official packages. See the [local inference guide](docs/semantic-inference.md).
- **MCP server** - `riltra mcp` serves fourteen tools over local stdio: thirteen
  read-only ones and one provider-backed tool that starts the operator's
  language server. Reads never create or migrate a store: a missing
  index returns `no-index` without creating files, and an unsupported schema is
  refused as `schema_outdated` with a CLI migration instruction.

## Quick start

The [`0.1.0` pre-release](https://github.com/terapetralabs/riltra/releases/tag/v0.1.0)
has native archives for Windows x64, Linux x64 and macOS Intel x64, each with a
SHA-256 checksum. The release notes say what the evidence does and does not
show. [release/INSTALL.md](release/INSTALL.md) describes the user-level
installers (`release/install.ps1` and `release/install.sh`), where they put the
files, and how to remove Riltra.

To build from source instead, you need a stable Rust toolchain and `git`:

```powershell
git clone https://github.com/terapetralabs/riltra.git
cd riltra
cargo install --path crates/cli --locked
```

This installs `riltra` in Cargo's user binary directory. Make sure that
directory is on `PATH` so your terminal and coding agent can find it. To run
commands from the checkout without installing, use `cargo run -q -p riltra --`
as shown below.

```powershell
cargo run -q -p riltra -- init --repo . --name riltra
cargo run -q -p riltra -- repo list
cargo run -q -p riltra -- repo status --repo riltra
cargo run -q -p riltra -- doctor --repo riltra
cargo run -q -p riltra -- scan --repo riltra --path crates/cli/src/main.rs
cargo run -q -p riltra -- index --repo riltra
cargo run -q -p riltra -- index status --repo riltra
cargo run -q -p riltra -- search --repo riltra --symbol version --limit 5
```

`repo status` prints one JSON line per repository, for example (trimmed,
illustrative values):

```json
{"name":"riltra","root":"…","git":{"present":true,"valid":true,"head":"…","branch":"master","dirty":false,"untracked":0}}
```

`scan` reports symbol names with source locations, for example (trimmed real
output — `…` marks omitted facts):

```json
{"path":"crates/cli/src/main.rs","status":"ok","language":"rust",
 "definitions":[{"kind":"function","name":"version_line",
 "span":{"start_line":38,"start_col":1,"end_line":40,"end_col":2},"bytes":[1464,1527]}, …],
 "calls":[{"kind":"call","name":"registry::load",
 "span":{"start_line":58,"start_col":25,"end_line":58,"end_col":50},"bytes":[2044,2069]}, …],
 "errors":0}
```

Call names are recorded exactly as written (`w.process`, `registry::load`)
and are never resolved to targets — they are candidates for you or your agent
to follow, not claims.

Notes:

- The registry lives in `%APPDATA%\riltra` (Windows) or `~/.config/riltra`
  (Linux/macOS). Set `RILTRA_HOME` to point it elsewhere (useful for scripts
  and tests).
- Git fingerprints need a Git executable: without one, `init` and `repo list`
  still work, while `repo status` reports `valid:false` and `doctor` flags the
  missing executable — nothing is silently faked.
- Run `cargo run -q -p riltra -- --help` for the full command list.

## MCP server (thirteen read-only tools, one provider-backed tool)

`riltra mcp` starts a local stdio MCP server for coding agents. It exposes
fourteen tools:

- `riltra_repo_list`, `riltra_repo_status`, `riltra_health`
- `riltra_code_search`, `riltra_code_callers`, `riltra_code_callees`,
  `riltra_code_trace`, `riltra_code_impact`, `riltra_context` — bounded code
  graph and impact context, composed with explicit anchor bindings and optional
  lexical or semantic knowledge matches. Bindings stay separate from unlinked
  matches; a match does not establish a code-to-record relationship.
- `riltra_knowledge_search` — literal text or exact evidence-path search by
  default; set `semantic: true` for optional local vector search, which reports
  an explicit FTS5 fallback reason when unavailable. See the [local inference
  guide](docs/semantic-inference.md).
- `riltra_knowledge_show`, `riltra_knowledge_related`
- `riltra_knowledge_at_anchor` — read-only lookup of the bindings at a code
  anchor (`knowledge at`); an unlinked anchor stays unlinked
- `riltra_code_semantic` — definition or references at a position, answered by
  the language server the operator configured in `RILTRA_LSP_SERVER` (an
  absolute path). It is the one tool that starts a subprocess: the server may
  write its own index cache and may reach the network, so the server is no
  longer read-only as a whole. Riltra itself still creates, migrates and
  replaces no database through it.

The server never creates, migrates, or replaces a database. A missing index or
knowledge store returns its documented empty/`no-index` result without creating
files; a store with an unsupported schema is refused as `schema_outdated`
without modification, with a CLI migration instruction. Results keep the same
freshness, provenance, and `POTENTIAL_CALL`/`CALLS` semantics as the CLI.

### Connect a coding agent

Install Riltra once ([quick start](#quick-start)), then configure each coding
agent you use. Each client starts the same local stdio server with `riltra mcp`.
These examples use Riltra's default data directory; set `RILTRA_HOME` only if
you want to store its registry and indexes elsewhere.

#### Codex

Add Riltra to the user-level Codex configuration. Codex CLI and the Codex app
share this MCP configuration:

```powershell
codex mcp add riltra -- riltra mcp
codex mcp list
```

#### Claude Code

Add Riltra at user scope so it is available in all projects:

```powershell
claude mcp add --scope user --transport stdio riltra -- riltra mcp
claude mcp get riltra
```

#### OpenCode

Add this entry to the global user configuration: `%USERPROFILE%\\.config\\opencode\\opencode.jsonc` on Windows, or
`~/.config/opencode/opencode.jsonc` on Linux and macOS. Merge it into the
existing `mcp` object if one is already present:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "riltra": {
        "type": "local",
        "command": ["riltra", "mcp"]
      }
    }
  }
}
```

Restart the client after configuration and confirm that the Riltra tools are
listed. If `riltra` is not found, check that the directory holding it is on
`PATH` (Cargo's binary directory for a source install, the directory the
installer printed for an archive install), or configure the full path to the
installed executable.

For guidance on when an agent should call each tool and how to read its
answers, see the [agent setup guide](docs/guide/agent-setup.md).

## Language support

| Language | Extensions | Syntax facts | Name occurrences | Semantic evidence |
|---|---|---|---|---|
| Python | `.py` | definitions, imports, call candidates | no | SCIP import (contract only) |
| JavaScript | `.js` `.mjs` `.cjs` | same | no | SCIP import (contract only) |
| TypeScript / TSX | `.ts` `.mts` `.cts` `.tsx` | same | no | SCIP import, verified on a fixture (scip-typescript) |
| Rust | `.rs` | same | no | SCIP import, verified on a fixture (rust-analyzer) |
| C | `.c` | same | no | SCIP import (contract only) |
| C++ | `.cc` `.cpp` `.cxx` `.h` `.hh` `.hpp` `.hxx` `.inl` | same, plus classes, structs and data members | yes | language server query (clangd, operator-configured); not verified on a real project |
| CUDA | `.cu` `.cuh` | reported as unsupported, never parsed | no | none |

Other extensions are reported as unsupported, never guessed. Headers parse as
C++.

## Known limitations

- Syntax-only answers are candidates. Without a semantic provider, `callers`
  match call sites by name inside a scope of related files, `trace` ends at the
  anchor (no edge has a resolved target), and `impact` is `UNKNOWN`.
- There is no C++ semantic resolution out of the box and no CUDA support. A C++
  project needs a compilation database and a language server you configure.
- Macro interiors and C++ templates are opaque to the syntax layer. `.inl`
  files are parsed as C++ headers; a fragment that needs its header context
  keeps its syntax errors in the `errors` count. `.ipp` and `.tpp` files are
  not indexed.
- `search --occurrences` is a repository-wide census of an exact name. An empty
  list is not proof that a name is unused.
- macOS continuous integration covers the x64 pool only; macOS ARM64 is not
  covered.
- C++ reference identity, construction-site attribution, complete call chains and affected-set coverage remain
  unverified. Treat C++ answers as candidates.
- The repository history still carries machine-local paths from early commits.
  The history is not rewritten, because documents under `docs/` cite commits by
  hash; see [Repository history](SECURITY.md#repository-history).

## Stability (pre-1.0)

Within `0.1.x` the exit codes (`0` ok, `2` usage, `3` repository/data), the
fixed key order of every JSON document and the MCP tool names are stable; byte
goldens enforce them. New keys appear only behind an opt-in flag or are
appended to the end of a document, and a change to an existing key is made in a
minor version and written in the release notes. The on-disk index schema is
versioned: an older index reads as stale or `schema_outdated` and is rebuilt
with `index`, never silently reused.

## Project status

Riltra is in pre-release development; `0.1.0` is published as a
[pre-release](https://github.com/terapetralabs/riltra/releases/tag/v0.1.0), not
as a stable release. Continuous integration runs format, compile, lint, and test
checks on Windows, Linux, and macOS (x64) for every change. A manually started
workflow builds the native archives of a release candidate, checks each with a
clean-install smoke and writes their checksums; see
[release/README.md](release/README.md).

## Documentation

- [Public project status](docs/public-status.md): current scope and public-facing limits.
- [Changelog](CHANGELOG.md): notable changes per release.
- [Agent setup guide](docs/guide/agent-setup.md): when an agent should call
  Riltra and how to read its answers.
- [Documentation index](docs/README.md): user and contributor documentation.

## Contributing

Contributions are welcome under the Apache License 2.0 (inbound = outbound).
Every commit needs a `Signed-off-by` line (`git commit -s`), and
`cargo fmt`, `cargo clippy`, and `cargo test` must pass. Details live in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Reporting a problem or idea

Use the repository's issue forms for ordinary bug reports and feature requests.
Include only sanitized diagnostics: redact credentials, private source, and
local paths that identify you or your organization. Security vulnerabilities
must go through the [private reporting channel](SECURITY.md#reporting-a-vulnerability),
not a public issue.

## Security

Riltra's core is local-first: it uploads nothing, phones home to nothing, and
never executes a repository's code to inspect it. External providers are
explicit opt-in; a configured language server may run project tools or access
the network. Please report vulnerabilities through the
repository host's private vulnerability reporting — see [SECURITY.md](SECURITY.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
