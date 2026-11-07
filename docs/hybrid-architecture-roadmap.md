# Hybrid architecture roadmap

**Status:** Owner-adopted on 2026-11-06. Dated implementation status is recorded below; each stage still has its own acceptance and owner gates.

This roadmap adopts the architecture proposal dated 2026-11-05 and replaces conflicting earlier direction only where stated below. The current product contract and source remain authoritative for implemented behavior. M8–M10 records and pilot evidence remain historical; this document does not rewrite them.

## Purpose

Riltra is a local-first, evidence-backed code intelligence and engineering knowledge tool. Its intended workflow helps answer:

- **What:** what code does, with source locations and provenance.
- **Impact:** what may be affected by a change, with bounded evidence and honest coverage.
- **Why:** which engineering decisions explain the code and whether their sources remain current.

Correctness, provenance, freshness, repository isolation and explicit uncertainty take priority over ranking. `Candidate`, `Unknown`, `Partial`, `Stale` and `Unverified` must keep their existing meanings. Similarity is relevance evidence, not truth or causality.

## Architecture decisions

- Keep the Rust modular monolith and existing CLI/MCP application boundaries.
- Preserve the existing code-index SQLite store and knowledge SQLite store as separate canonical stores for their domains, including their schemas, migrations, export/import and revision contracts. Do not merge or replace them.
- Keep Tree-sitter, SCIP and opt-in LSP at their existing evidence boundaries. Syntax name matches do not become semantic identity by ranking.
- Keep FTS5 as lexical search. Petgraph is used for bounded graph computation over selected existing evidence; it is not a graph store or an authority for symbol identity.
- Optional semantic retrieval is limited to active engineering-knowledge records. R2a adds FastEmbed local inference and the default-off LanceDB dependency preflight; derived-index lifecycle is R2b, and semantic search/fallback is R2c. SQLite remains canonical. Source-code embedding, hosted inference, automatic model downloads and a second model are out of scope.
- New query paths, including CLI context and MCP reads, must be read-only: they do not create or migrate stores, create locks or caches, download assets, or run providers implicitly. Explicit maintenance commands may update only derived state and must leave canonical records unchanged.
- Preserve existing CLI/MCP names, exit codes, persisted data, deterministic JSON and key ordering. Lexical queries without an opt-in semantic flag must retain their current output contract.
- Core operations remain offline and usable without optional semantic components. Official packages remain core-only unless a later distribution decision explicitly changes that contract.

## Planned components and pins

| Component | Planned use | Initial pin | Status at adoption (2026-11-06) |
|---|---|---:|---|
| [Petgraph](https://crates.io/crates/petgraph/0.8.3) | Bounded graph computation | 0.8.3 | Planned; not added |
| [FastEmbed](https://crates.io/crates/fastembed/7.6.0) | CPU inference for engineering knowledge | 7.6.0 | Optional; not added |
| [LanceDB](https://crates.io/crates/lancedb/0.41.0) | Rebuildable vector index for engineering knowledge | 0.41.0 | Optional; not added |

### Dated implementation status — 2026-11-07

The status column above preserves the 2026-11-06 adoption snapshot. Since then, Petgraph 0.8.3 was added for R1, which merged through PR #44 at master revision `416bbd13`. FastEmbed 7.6.0 local inference and the default-off LanceDB 0.41.0 dependency preflight are included on open PR #45; R2a is pending merge and acceptance. PR #45 does not implement vector-index lifecycle or semantic search: R2b, R2c, R3 and R4 remain planned. This amendment updates status only; stage acceptance criteria and historical evidence remain unchanged.

The single planned model is [`intfloat/multilingual-e5-small`](https://huggingface.co/intfloat/multilingual-e5-small): 384 dimensions, CPU inference, mean pooling, L2 normalization, `query:` / `passage:` prefixes, and at most 512 input tokens. Before implementation, its model, tokenizer and configuration revision and SHA-256 values, plus the local ONNX runtime ABI and checksums, must be pinned and verified. Assets are provisioned explicitly; build and query paths do not download them. Verify model-asset and crate licenses, transitive dependencies, native builds and platform compatibility before introducing optional components. These are initial pins; dependent tasks must resolve the exact manifests and feature graph against the supported toolchain and CI targets, and document any smallest necessary pin correction before implementation.

## Stages and acceptance

Stages remain proposals until their own scope is approved. Completion of a stage requires its acceptance evidence; a green build alone is not enough.

### R0 — Adopt the architecture contract

**Status: adopted; documentation only.** The 2026-11-06 adoption promotes this roadmap and reconciles the active documentation. It adds no dependency or runtime behavior. Future changes still need their own stage approval.

### R1 — Bounded graph context

**Status: implemented and merged through PR #44 (master revision `416bbd13`; 2026-11-07).**

Use Petgraph 0.8.3 for one bounded graph-context operation over evidence read from the existing code-index store and a pinned snapshot. Add optional CLI and read-only MCP surfaces without changing existing queries. Preserve direction, scoped identity, owner bridges, stale and candidate labels, and traversal limits. Distances are local to the inspected subgraph, not repository-wide shortest paths. Acceptance covers cycles, duplicates and same-name identities, candidate shortcuts, empty/stale inputs, budgets, deterministic output, path confinement, read-only behavior, and CLI/MCP parity.

### R2 — Optional semantic knowledge retrieval

**R2a — Local inference and dependencies:** add a default-off Cargo feature with the pins above. Verify provisioned assets and runtime compatibility without network access or automatic cache creation. The core build and query path must work without model/runtime assets.

**Status as of 2026-11-07:** implementation is on open PR #45 and is pending merge and acceptance.

**R2b — Derived index lifecycle:** provide explicit build, status and clear operations under the existing Riltra home. Build from one consistent active-knowledge snapshot; bind rows to repository identity, record revision, content digest and model fingerprint. Publish complete immutable generations atomically; changed source, interrupted builds or invalid paths must not replace the current generation or modify canonical knowledge. Readers and clear must be safe on Windows as well as Unix. SQLite writers are serialized through build publication, but `knowledge restore` requires a quiescent store and is not supported concurrently with vector build or status. Clear operates only on the derived vector store.

Build and status validate canonical stores with the existing SQLite integrity check and stream ordered active rows for the corpus fingerprint. This reads the full canonical database on each snapshot; status does not materialize the active corpus, while build retains it for embedding.

**R2c — Query and fallback:** add opt-in semantic knowledge search and preserve existing lexical behavior when it is absent. Validate candidates against current active revisions, digests, scope and evidence in one knowledge snapshot. Missing or unusable optional components fall back to FTS5 with an explicit reason. Bounded retrieval reports inexact totals honestly and does not invent pagination guarantees. Similarity never creates factual links.

Semantic queries read provisioned model and runtime assets from `RILTRA_MODEL_DIR`; this path is configuration only and is not stored in the vector manifest. Query and fallback paths do not download or provision assets. Queries truncated by the model token window fall back to FTS5 with `query-truncated`; semantic hits report each stored passage's `embedding_truncated` state.

Acceptance covers missing, corrupt or mismatched assets and indexes, stale/retracted revisions, restore with a repeated generation number but different content, cross-repository isolation, filtering, explicit fallback, offline/read-only behavior, and CLI/MCP parity.

### R3 — What / Impact / Why context

Add a small shared composer over the existing code facts, graph context, explicit knowledge bindings and optional relevance. Pin the code anchor used for knowledge reads. Report code and knowledge freshness separately; do not imply atomicity across the stores. Preserve useful code output when optional knowledge is unavailable, while reporting the limitation. Acceptance checks explicit versus unlinked knowledge, source changes between reads, missing or corrupt stores, stable output and byte limits.

### R4 — Integration and handoff

**R4a:** exercise optional native builds and local inference on the existing Windows, Linux and macOS Intel x64 CI targets with explicitly provisioned assets. Keep core-only official packaging, current required job names and release guards intact. Record binary/model/resource observations without general performance or accuracy claims.

**R4b:** document only behavior and evidence actually delivered, including supported targets, asset provisioning, lifecycle, fallback, limits and the lack of cross-store atomicity. Preserve frozen milestone reports and date corrections as amendments.

## Existing checks and deferred supply-chain work

At adoption, CI has a `dependency-audit` job that installs pinned `cargo-audit` 0.22.2 and runs `cargo audit --file Cargo.lock` against RustSec. The test suite checks dependency license metadata against an allowlist, and the installer CI job checks generated release documentation against the locked dependency graph. These checks do not constitute a complete supply-chain program.

Pinning workflow actions by commit SHA, automated dependency update proposals and CI enforcement of commit sign-off remain deferred. Do not describe these as implemented until the workflow and its evidence change.

## Historical distribution status

The earlier M10 consolidation said published-release Phase B had not run when that record was written. A dated amendment records the later Phase B check against the published `0.1.0` assets on 2026-11-04: one hosted run on Windows, Linux and macOS Intel x64. The amendment supersedes only the earlier Phase B status for those assets; it does not establish ARM64, every end-user environment, or least-privilege Windows execution. See [M10 Phase B amendment](m10-phase-b-amendment-2026-11-04.md). Other M8–M10 evidence and pilot history remain unchanged.

## Explicit non-goals

No cloud service, HTTP server, telemetry, hosted model, source-code embedding, graph database, schema merge, crate split, second model, reranker, graph UI, autopatch, or automatic release. Do not promote candidate graph evidence or semantic similarity into `CALLS`, `AFFECTS`, `JUSTIFIES`, `SUPERSEDES`, `SAFE` or exact-coverage claims.
