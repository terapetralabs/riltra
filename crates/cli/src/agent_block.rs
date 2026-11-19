// SPDX-License-Identifier: Apache-2.0
//! The agent instruction block printed by `riltra agent-block`. This is the one
//! source of truth: `docs/guide/agent-setup.md` embeds the same text, and an
//! integration test asserts the two are byte-identical. Print-only: nothing
//! here reads or writes a file.

pub const BLOCK: &str = r#"## Code intelligence (Riltra)

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
"#;
