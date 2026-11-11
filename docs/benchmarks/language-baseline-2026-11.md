# Language baseline, 2026-11-11 (Tahap 0)

Scope: what the **existing** syntax layer extracts for the 5 languages that have an M6 corpus. No code was changed. Binary: debug build of commit `2e72f16`. Reproduce: `python -I docs/benchmarks/baseline_m6.py <abs-binary> <abs-m6-fixture-dir> <scratch-dir>` (it sets its own `RILTRA_HOME` under the scratch dir).

## Method
- For each M6 fixture, run `riltra scan` and compare with the manifest occurrences (authored independently of tree-sitter, see `tests/fixtures/m6/README.md`).
- **Definitions:** an expected definition matches a scanned definition with the same name whose span contains the manifest line. Precision = matched scanned / all scanned in the same files. Recall = matched / manifest definitions.
- **Calls:** only on cases with no non-call references (`inbound_references == 0`), counted once per call position. `dynamic_dispatch` is left out: its only non-definition occurrence is the receiver (`obj`), which its manifest says must not prove a member call. A call candidate matches when its text, recorded as written (`obj.target`), contains the manifest text on the expected line.

## Result
| Language | Files | Def recall | Def precision* | Call-candidate recall | Parse errors |
|---|---|---|---|---|---|
| Python | 16 | 35/38 | 35/39 | 15/15 | 0 |
| TypeScript | 16 | 35/38 | 35/39 | 15/15 | 0 |
| Rust | 17 | 38/41 | 38/43 | 15/15 | 0 |
| C | 15 | 29/36 | 29/37 | 12/12 | 0 |
| C++ | 16 | 35/38 | 35/40 | 15/15 | 0 |
| JavaScript, TSX | - | no corpus | - | - | - |

*Lower bound. Manifests list only the definitions that matter to a call-graph case, so extra scanned definitions (for example unused classes `A`, `B`) count against precision without being wrong.

## Reading the misses
Every definition miss is a **non-function binding**: a local alias (`alias = target`), a parameter (`obj`, `target`), or a C struct-field function pointer. They are SCIP-level symbols, not syntax definitions, so they are scope differences, not extraction bugs. Call candidates are complete on the pure-call cases that are counted (15 per language, 12 for C); cases with non-call references and the receiver-only `dynamic_dispatch` case are not counted. An earlier draft of this table counted the receiver and a call listed once per competing symbol, giving 17/17 (C: 14/14); that was wrong.

## Limits
- M6 is a SCIP relationship corpus. It is **not** a per-language syntax ground truth, so it cannot grade imports, classes, methods, or generics. Tahap 1 needs dedicated syntax manifests per language.
- JavaScript and TSX have no corpus; do not claim coverage for them from this table.
- The fixtures are small and synthetic; the table says nothing about real repositories.
- One manifest (`rs/impact_missing_caller.json`) has no matching source file and was skipped.

## Output size (secondary, no target)
Own repository, indexed into a scratch home (`riltra index`: 303 files parsed, 3224 definitions, 1 parse error, 1212 unsupported files). Bytes are CLI output, which the MCP tools reuse byte for byte; tokens are not measured, only roughly bytes/4. Sizes include snapshot metadata (root, branch, head, nonce, timestamp), so they vary by a few dozen bytes between runs and checkouts; read them as approximate.

| Query | Approx. size |
|---|---|
| `search --symbol classify`, `parse_source` or `open_read` (`--limit` 5 or 20) | 0.6-0.9 KB |
| `callers` on `classify` | 4.9 KB |
| `context` (`--max-bytes` 8192 or 32768) | 3.0 KB |

Search results stay near 1 KB, so output size is not a bottleneck for these queries. The snapshot was dirty (`freshness: stale`, reason `dirty`).
