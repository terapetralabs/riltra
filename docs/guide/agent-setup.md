# Agent setup guide

This guide is for people who connect an AI coding agent to Riltra, and for the agent itself. It covers what to prepare, which tool answers which question, how to read the labels in the answers, and a snippet to paste into your agent's instruction file.

It assumes Riltra is installed and its MCP server is registered with your client; the [README](../../README.md#connect-a-coding-agent) shows how. Riltra is a map of your code, not a source of truth: an answer is a place to look, and the cited source is what to rely on.

## Prepare the repository

The MCP tools only read. They never create or migrate an index, so a person or a script prepares each repository once, in a terminal:

```text
riltra init --repo <path-to-repository> --name <name>
riltra index --repo <name>
riltra index status --repo <name>
```

Use `<name>` as the `repo` argument of every tool. Re-run `riltra index --repo <name>` after significant changes; `index status` and the freshness labels in answers tell you when the index no longer matches the source. If you set `RILTRA_HOME` in your terminal, give your MCP client the same value, or it will look somewhere else.

For optional semantic knowledge search, build the derived vector index from
the CLI with `riltra knowledge vector build --repo <name> --model-dir <model-dir>`.
The MCP client must also receive `RILTRA_MODEL_DIR` pointing at the provisioned
model/runtime assets. This feature requires a Riltra build with the default-off
`semantic-knowledge` feature; see the [local inference guide](../semantic-inference.md).

## Which tool for which question

| You want to know | Call | Notes |
|---|---|---|
| Is Riltra working, and which repositories are registered? | `riltra_health`, `riltra_repo_list` | Read-only; they create and open no storage. |
| Is this repository indexed, and is the index current? | `riltra_repo_status` | Includes `index_status`. A missing index answers `no-index`; nothing is created. |
| Where is a symbol defined? | `riltra_code_search` | `query` is the symbol text. `limit` (default 50, maximum 1000) and `offset` page the results. `occurrences: true` adds a name census for C++ only. |
| What calls this? | `riltra_code_callers` | Anchor with `path`, `line` and `col`. Heuristic edges stay `POTENTIAL_CALL`. |
| What does this call? | `riltra_code_callees` | Same anchor. |
| How might A reach B? | `riltra_code_trace` | Optional `to` (`PATH:LINE:COL`) and `depth` (default 3, 1 to 6). Paths are plausible traversal, not execution. |
| What might this change affect? | `riltra_code_impact` | Returns a coverage verdict; plausible, not execution. Treat `UNKNOWN` and `partial` as "not established". |
| What code, impact and knowledge context is available here? | `riltra_context` | Combines a bounded graph with explicit bindings and optional relevance candidates; an unlinked match is not a proven relationship. |
| Is there a recorded decision for this code? | `riltra_knowledge_at_anchor` | Explicit bindings at a source position. An unlinked anchor stays unlinked. |
| What does that record say, and what is it linked to? | `riltra_knowledge_show`, `riltra_knowledge_related`, `riltra_knowledge_search` | `show` reads one revision with its evidence freshness; `related` follows links one hop (`direction` is `outbound` or `inbound`); `search` matches literal text or an exact stored evidence path by default. Set `semantic: true` for optional local vector search; when unavailable it reports the FTS5 fallback reason. |
| Definition or references from a language server | `riltra_code_semantic` | Opt-in; see below. |

The MCP tools cannot write. Decisions and their bindings to code are recorded with the CLI (`riltra knowledge ...`, see the [README](../../README.md#available-today)).

**`riltra_code_semantic` is different.** It works only when an operator has set `RILTRA_LSP_SERVER` to the absolute path of a language server, and it starts that server as a local process. The server reads the project, may write its own cache, and may run project tools or reach the network, so this tool is not read-only. The references it returns are locations, not call edges, and an empty answer does not mean the symbol does not exist.

## Reading the answers

| Label | Meaning | What to do |
|---|---|---|
| `POTENTIAL_CALL` | A call site matched by name; it is not resolved to a target. | Open the source and check. It is a lead, not proof. |
| `CALLS` | A call edge backed by a semantic provider's source-checked evidence. | Stronger than a candidate, and present only when a semantic provider supplied the evidence. Still cite the span. |
| `Heuristic`, `Unverified` | The evidence is not proven. Provider answers and stored notes arrive as `Unverified`. | Treat as a lead, and say so when you report it. |
| `UNKNOWN` | The default impact verdict: not established. | It never means "safe". |
| `partial` | Coverage is incomplete (unresolved owners or targets, missing provider coverage). | A short or empty list does not mean nothing is affected. |
| `fresh`, `stale`, `unknown` | Whether the index, or the source a stored note points at, still matches the current source. | On `stale`, re-run `riltra index` and re-read the source. A stale binding is never re-bound automatically. |
| `no-index` | No index exists for this repository. | Ask the user to run `riltra init` and `riltra index`. |
| `schema_outdated` | A store was written by a different Riltra version and was left untouched. | Follow the migration instruction in the response; `riltra index` rebuilds the code index. |

Confirmation (`confirm_state`) on a knowledge record is stored provenance, not verification: a confirmed note is still a claim by whoever recorded it.

## Positions

`line` and `col` are 1-based. For the graph tools (`callers`, `callees`, `trace`, `impact`) the column counts bytes, so on a line with non-ASCII text it differs from an editor's column. Take `line` and `col` from the span of a search result instead of computing them. `riltra_code_semantic` takes its column in the language server's position unit (UTF-16 by default), which is not the byte column.

`callers`, `callees`, `trace`, `impact` and `context` also accept a definition name in place of a position: `--target <name> [--in <relpath>] [--kind <kind>]` on the CLI, and `target`, `in` and `kind` on the MCP tools. The two forms are mutually exclusive. A name resolves only when exactly one definition has that exact name; otherwise the answer is a document with status `ambiguous` (up to 20 candidates listed), `not-found`, or `no-index`, and the exit code stays 0. A substring match never resolves by itself, even when it is the only hit. Substring matches are true substrings of the name, case-insensitive for ASCII letters (SQLite LIKE); exact names are case-sensitive. `total` counts every definition that matches (`total_exact` is always `true` for these answers); the candidate list holds up to 20 and `truncated` says when more exist. `--kind` takes the kind as stored for each language, which differs: Python functions and methods are `function`, Go, JavaScript, TypeScript and Java methods are `method`, and classes are `class`.

## Excluding paths

Put a `.riltraignore` file in the repository root to keep generated or vendored directories out of the index.

```text
# One repository-relative path per line.
target/
node_modules/
packages/app/node_modules/
vendor/
```

- Blank lines and lines starting with `#` are skipped. A line starting with `!` is skipped too, because negation is not supported.
- A line matches one exact path or, when it ends with `/`, that directory and everything under it.
- Matching starts at the repository root and there are no wildcards, so `node_modules/` does not match `packages/app/node_modules/`; list each directory.
- Re-run `riltra index` after changing the file.

## A snippet for your agent's instructions

Paste this into your agent's instruction file (for example `AGENTS.md` or `CLAUDE.md`) and replace `<name>` with the name you gave `riltra init`.

```markdown
## Code intelligence (Riltra)

This repository is indexed by Riltra, a local code map reached through MCP tools named `riltra_*`. It is a map, not a source of truth: confirm anything you rely on by reading the cited source.

- Start with `riltra_repo_status` for `<name>`. If the index is missing or stale, say so and ask the user to run `riltra index --repo <name>`; then continue from the source itself.
- Find a definition with `riltra_code_search`, and pass the `path`, `line` and `col` it returns to `riltra_code_callers`, `riltra_code_callees`, `riltra_code_trace` and `riltra_code_impact`.
- `POTENTIAL_CALL` is a lead to open and read, never proof of a call. An impact answer of `UNKNOWN` or `partial` means "not established", never "safe". An empty result is not proof that nothing exists.
- Before changing code, call `riltra_knowledge_at_anchor` on it. Report what it returns as a stored, `Unverified` note with its revision, not as established fact.
- Say which Riltra answers you used and which labels they carried.
```

## When something is off

- **No Riltra tools appear.** Restart the client, then run `riltra --version` in the same environment. If `riltra` is not on `PATH`, configure the client with the full path to the executable.
- **Every answer is `no-index`.** The repository was not indexed, or the client uses a different `RILTRA_HOME` than your terminal.
- **`schema_outdated`.** See the table above.
- **`riltra_code_semantic` refuses.** `RILTRA_LSP_SERVER` is not set to an absolute path, or the path you passed does not exist inside the repository (the refusal reads `document path escapes repository`).
- **`callers` is empty.** Without a semantic provider, callers match call sites by name within related files, so an empty list is not proof that nothing calls the function ([Known limitations](../../README.md#known-limitations)).

## Limits

Supported languages, evidence labels and known limitations are listed in the
[README](../../README.md). For the current user-facing scope, see the [public
project status](../public-status.md).
