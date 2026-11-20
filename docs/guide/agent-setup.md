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
| Which definitions match a description in words? | `riltra_code_query` | Lexical ranking over names, paths and doc comments. Results carry `path` and `line` but no column; pass `name` as `target` and `path` as `in`. |
| Which definitions does my git change touch? | `riltra_code_changes` | Live parse of the changed files: definitions added, removed or modified. A map of structure, not an impact verdict. |
| What calls this? | `riltra_code_callers` | Anchor with `path`, `line` and `col`. Heuristic edges stay `POTENTIAL_CALL`. |
| What does this call? | `riltra_code_callees` | Same anchor. |
| How might A reach B? | `riltra_code_trace` | Optional `to` (`PATH:LINE:COL`) and `depth` (default 3, 1 to 6). Paths are plausible traversal, not execution. |
| What might this change affect? | `riltra_code_impact` | Returns a coverage verdict; plausible, not execution. Treat `UNKNOWN` and `partial` as "not established". |
| What code, impact and knowledge context is available here? | `riltra_context` | Combines a bounded graph with explicit bindings and optional relevance candidates; an unlinked match is not a proven relationship. |
| What bounded context and source snippets are available for these selected anchors? | `riltra_code_packet` | Pass one to five explicit position or name selectors. Each keeps its own outcome; ambiguous names are not selected automatically. |
| Is there a recorded decision for this code? | `riltra_knowledge_at_anchor` | Explicit bindings at a source position. An unlinked anchor stays unlinked. |
| What does that record say, and what is it linked to? | `riltra_knowledge_show`, `riltra_knowledge_related`, `riltra_knowledge_search` | `show` reads one revision with its evidence freshness; `related` follows links one hop (`direction` is `outbound` or `inbound`); `search` matches literal text or an exact stored evidence path by default. Set `semantic: true` for optional local vector search; when unavailable it reports the FTS5 fallback reason. |
| Definition or references from a language server | `riltra_code_semantic` | Opt-in; see below. |

The MCP tools cannot write. Decisions and their bindings to code are recorded with the CLI (`riltra knowledge ...`, see the [README](../../README.md#available-today)).

## Selected-anchor packets

Use a packet after search or query has shown which anchors matter. Select one to
five anchors explicitly. A position uses `path`, `line`, `col`, and optional
`symbol`; a name uses `target` with optional `in` and `kind`. Duplicate
selectors are allowed and keep separate outcomes, while identical source ranges
share a snippet. Unknown fields, mixed selector forms, zero coordinates, an
empty array, or more than five anchors are rejected. Packet input does not
accept `query` or `semantic`; it does not run lexical or semantic knowledge
retrieval.

Start with a query, then select a result by its `name` and `path`. Query results
do not contain a column, so use a name selector instead of guessing a position:

```sh
riltra query --repo myrepo --text "parse input"
```

Pass a selected result to `packet` to see whether the name resolves uniquely:

```sh
riltra packet --repo myrepo --anchors '[{"target":"parse","in":"src/parser.rs","kind":"function"}]'
```

The same single-quoted JSON literal works in PowerShell and POSIX shells. For a
position from code search, use its repository-relative `path`, 1-based `line`,
and byte-based `col`:

```powershell
riltra packet --repo myrepo --anchors '[{"path":"src/parser.rs","line":42,"col":5}]' --depth 3 --limit 20 --max-bytes 32768
```

The MCP tool `riltra_code_packet` takes `repo`, `anchors`, `depth`, `limit`,
and `max_bytes` with the same defaults and selector forms. For example, its
arguments can be:

```json
{"repo":"myrepo","anchors":[{"target":"parse","in":"src/parser.rs","kind":"function"}],"depth":3,"limit":20,"max_bytes":32768}
```

Defaults are `depth: 3` (1–6), `limit: 20` (1–100 per context), and
`max_bytes: 32768` (8192–131072 JSON bytes). The `anchors` array has a normalized
16 KiB cap in CLI and MCP; the CLI literal also has a raw 16 KiB cap. Snippets
contain at most 40 LF-delimited lines and 4096 bytes; source reading is capped
at 1 MiB per file. The byte budget applies to the serialized response, not
tokens. If required response metadata alone does not fit, CLI exits 2 without
partial output; MCP returns `invalid_input`.

The top-level response has `schema_version: 1` and `operation: "packet"`. Each
anchor has its own status (`resolved`, `ambiguous`, `not-found`, `no-index`, or
`unavailable`). Exit 0 means that the document was formed, not that every
selector resolved, that source is fresh, or that the packet establishes a
change is safe. Invalid selectors/options use exit 2; unsafe paths and fatal
repository or storage failures use exit 3. MCP returns the corresponding tool
error for invalid input or fatal data failures.

`available`, `truncated`, or `unavailable` describes source snippet delivery.
On `source-changed`, reindex and choose the anchor again before relying on a new
context. Packet knowledge lookup is limited to explicit bindings; each anchor
reports its knowledge status and generation separately from the pinned code
snapshot. It does not retrieve related knowledge by words or vector search.
Packet is read-only in both transports: it does not create or migrate code or
knowledge databases. A missing code index produces a `no-index` packet
document.

For more detail, `follow_up.context` names `riltra_context` and gives the
resolved position and arguments for a new request. That request may see a newer
snapshot, so check freshness again. `follow_up.source` is a nullable path/range
descriptor for a client's native source reader; it may be null when no unique
source range is available. Riltra adds no source-range tool. Use the context
descriptor to make a new context request, or follow this sequence when the
source descriptor is present:

The same follow-up can be requested from the CLI. Copy the supplied `repo`,
`path`, `line`, `col`, `depth`, and `limit`; include `--symbol` when the
follow-up's symbol is non-null. For example:

```sh
riltra context --repo demo --path src/main.rs --line 42 --col 9 --depth 3 --limit 20 --max-bytes 131072
```

This is a new context query, not a continuation of the packet transaction. Its
response reports a `snapshot` (`id`, `nonce`, and `revision`), plus
`freshness.status` and `freshness.reasons`. Compare those and the returned
`anchor` identity with the packet's snapshot and resolved anchor; if the
identity changed or freshness is `stale` or `unknown`, reindex and select the
anchor again before relying on the context.

1. Validate that its path resolves inside the repository; reject traversal or
   symlink escapes before reading.
2. Read the complete current file as bytes into one buffer. Compute FNV-1a 64
   over the entire buffer; its digest is 16 lowercase hexadecimal digits.
   Compare it with the descriptor's digest. Do not hash only the indicated
   range: the digest covers the complete indexed file.
3. Only if the digest matches, take `requested_range.start .. requested_range.end`
   from that same buffer. The range is half-open byte offsets. `returned_range`
   describes what the packet snippet returned; it does not change the digest's
   scope.

If containment cannot be verified or the digest differs, do not use those byte
offsets; if it differs (`source-changed`), reindex and resolve the anchor again.
The `fnv1a-64` digest is a non-cryptographic change detector, not proof of
identity, integrity, or safety.

| Follow-up field | Next step |
| --- | --- |
| `follow_up.context` | Call `riltra_context` with its supplied arguments, then check that response's freshness. |
| `follow_up.source` | When present, validate containment, hash the entire file, compare its digest, then slice the half-open `requested_range` from that same buffer. |

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
| `snippet-cap`, `source-changed`, `source-unavailable` | Packet snippet delivery was clipped or current source could not be used. | For `follow_up.source`, use the full-file hash procedure above; if it differs, reindex and select the anchor again. |
| `invalid-source-range` | The pinned index range cannot be applied to the verified UTF-8 source bytes. | The snippet and follow-up source range are withheld; rebuild the index before relying on that anchor's source. |

`impact` also lists the name-matched callers of the anchor and of each node it reaches through a proven edge, for every such node a name can be derived for. Each such path has `"basis": "candidate-name"` (every path has `basis` `proven`, `unproven` or `candidate-name`), and the `candidates` object reports `"followed": false`: candidates are not followed further, so the answer is not the full transitive impact. A node that cannot be seeded makes the answer incomplete and adds `candidate-pass-skipped` to `unresolved`. The verdict stays `UNKNOWN` while any candidate path exists, `--limit` also caps the candidates per node, and `total` is `null` unless every candidate was counted. `bridges` may list owner mappings for candidate callers too. A C/C++ data-member anchor takes no name candidates and stays complete for that reason, and a node reached by two proven routes gets its candidates on the first route only, with `completeness.covered`, `total` and `visits.edges` counting candidate paths and rows too (the status stays `partial` and the verdict `UNKNOWN`).

`impact --summary` adds a `summary` object counted over this traversal: all of its paths, before the page that `--offset` and `--limit` return, so `paths` is never filtered. `--limit` also caps candidates per node, so a small `--limit` can lower the counts, and `candidates_complete: false` says so. Test paths come from a labelled path heuristic: a directory named `test`, `tests`, `__tests__` or `spec`, or a file named `test_*.py`, `*_test.go`, `*.test.ts`, `*.spec.ts` or `*Test.java`. Other conventions (`.tsx`, `_spec.rb`) are not covered. Test paths are left out of `total`, `by_depth` and `distinct_files` unless `--include-tests` is given, and `tests.count` always reports them. `--include-tests` needs `--summary`. `summary.verdict` and `summary.unresolved` are copied from the document, so they can differ with `--offset` or `--limit` (the counts do not depend on `--offset`). `candidates_complete` is also false after a path or edge budget stop, not only after the candidate cap.

`callers`, `callees`, `trace`, `impact` and `context` give each `POTENTIAL_CALL` edge two keys, `candidates` and `candidate_scope`. `candidates` counts every stored function and method definition with the edge's `callee_name` in the same snapshot, repository-wide, including declarations such as Rust trait method signatures. `candidate_scope` places them against the caller file: `same-file` when one of them is in the caller's file, `imported-file` when one is file-linked to the caller by the same rules `callers` uses (a file that imports the module, a file of the same Rust crate source root, or a C/C++ includer; a C++ declaring header is not linked yet), `other-file` when they are all elsewhere, and `none` when `candidates` is 0. `imported-file` is a file-level link, not proof that the call goes to that definition. `candidates` is a count, not a resolution: the edge stays a `POTENTIAL_CALL` with its callee name as written, no target is chosen, and an aliased import (`from a import target as t`) is not counted, so `t()` reports `none`. In the CLI output of `callers`, `callees`, `trace` and `impact` the two keys are the last two keys of the edge object, after `evidence`; `context` and the MCP tools carry the same two keys and sort their keys like all their other fields. Edges of other kinds have no new keys.

Confirmation (`confirm_state`) on a knowledge record is stored provenance, not verification: a confirmed note is still a claim by whoever recorded it.

## Opt-in expansion

`query` (CLI) and `riltra_code_query` (MCP) answer with the lexical `results` alone. Pass `expand: true` (CLI `--expand`, a bare flag) when you also want the definitions that the top results call or are called by, one hop along proven edges. Without it the answer is unchanged. It needs a SCIP import for the repository (`riltra semantic import`); without one the status is `none-imported`.

The `expansion` object comes after `basis`. Its `status` is one of five values. The first rule that applies wins, in this order:

- `none-imported`: no valid SCIP import backs the snapshot, or there is no index. Nothing was mapped.
- `no-seed`: no usable seed. Either the query returned nothing, or every considered seed was skipped; `seeds.items[].skipped` says why (`not-callable`, `unknown-symbol`).
- `stale`: `reasons` names why. `source-not-fresh` means the snapshot is not `fresh` (including `unknown`); `graph-proof-incompatible` means the stored graph proof does not match the snapshot; `import-stale` means a SCIP import that a seed or an edge depends on no longer matches its source. No row is added.
- `no-eligible-edge`: the seeds were mapped, but no proven relation added a row. The reason is `empty-result-not-coverage-proof`.
- `available`: at least one row was added.

A status other than `available` never means that no relations exist. A zero is not coverage: check `seeds` and `unmapped` before you say that a definition has no callers or callees.

What the rules mean in practice:

- Seeds are the first five returned results. Expansion is one hop. It never follows name-matched candidates (`POTENTIAL_CALL`) or imports.
- Only `function` and `method` definitions can be seeds or added rows.
- A seed without a SCIP identity gets outgoing relations only, and its item says `"scip": false`. Its callers were not looked for, so that flag never means "no callers".
- Today only `CALLS` edges add rows. `REFERENCES` edges are stored without a caller identity, so each one is counted in `unmapped` and none becomes a row.
- Every added row has `"precision": "Exact"` and `"evidence_label": "Unverified"`. Expanded rows are never labelled `Current`.
- Expansion runs only when the source matches the index. Uncommitted Git changes, or any source change since indexing, make the snapshot stale, so the answer is `stale` with `source-not-fresh`. This is an accepted limit.
- A failed database read is an error, not a status: the CLI exits 3 with `error:` and the MCP tool answers `store_error`. No partial answer is printed.

The caps are 5 seeds, 5 added rows per seed, 25 in total, and 200 edges read per seed and direction, shared by all of that seed's identities. `omitted` counts eligible rows left out by a cap. `read_bounded` is true when a read stopped at the edge budget, and `truncated` is true when either a row was omitted or a read was bounded. When `read_bounded` is true, `unmapped` is a lower bound.

The `expansion` keys appear in this order: `status`, `reasons`, `limits` (`seeds`, `per_seed`, `total`, `edge_read`), `seeds` (`considered`, `mapped`, `skipped`, `items`), `added`, `omitted`, `truncated`, `rows`. Each row has `path`, `line`, `name`, `kind` and `via`. `via` has `seed` (`path`, `line`, `name`), `edge`, `direction`, `precision` and `evidence_label`. `direction` says what the added row is to the seed: `callee` means the seed calls it, and `caller` means it calls the seed.

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

Paste this into your agent's instruction file (for example `AGENTS.md` or `CLAUDE.md`) and replace `<name>` with the name you gave `riltra init`. `riltra agent-block` prints the same text.

```markdown
## Code intelligence (Riltra)

This repository is indexed by Riltra, a local code map reached through MCP tools named `riltra_*`. It is a map, not a source of truth: confirm anything you rely on by reading the cited source.

- Start with `riltra_repo_status` for `<name>`. If the index is missing or stale, say so and ask the user to run `riltra index --repo <name>`; then continue from the source itself.
- Find a definition by name with `riltra_code_search`, and use its `path`, `span.start_line` and `span.start_col` as the anchor (`path`, `line`, `col`) for `riltra_context`, `riltra_code_callers`, `riltra_code_callees`, `riltra_code_trace` and `riltra_code_impact`.
- Find code by concept with `riltra_code_query` (free text). A query result has no column: pass its `name` as `target` and its `path` as `in` to those tools instead.
- For several selected locations, call `riltra_code_packet` with one to five explicit anchors and inspect every status; exit 0 does not mean all resolved or that a change is safe. If source is `source-changed`, reindex and select the anchor again. Use `follow_up.context` with `riltra_context`. When `follow_up.source` is present, validate repository containment, read the full file, compare its 16-digit lowercase FNV-1a 64 digest, then extract the half-open `requested_range` from that same buffer. Do not hash only the range; FNV is a change detector, not cryptographic proof.
- If a name does not resolve to exactly one definition (status `ambiguous` or `not-found`), do not pick a candidate for the user: read the candidates and the source, then anchor on the right definition with `path`, `line` and `col`.
- Before editing a symbol, run `riltra_code_impact` on it.
- After editing and before committing, run `riltra_code_changes` and read which definitions your change touched.
- `UNKNOWN` or `partial` is not a clean check, and an empty result means "not established", never "nothing exists".
- Use the graph tools first; use text search when they answer empty and for literal strings.
- Never rename by find-and-replace. A `POTENTIAL_CALL` is a lead to open and read, not proof of a call; check each use against the cited source.
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
