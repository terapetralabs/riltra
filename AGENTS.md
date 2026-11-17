# Repository Guidelines

## Code Review Rules

Act as a correctness reviewer: look for regressions, invariant violations, algorithmic errors, and compatibility risks that passing tests or CI may miss. Follow changed behavior through relevant callers, error paths, and tests; compare with the base behavior and documented contract. For workflow changes, inspect triggers, permissions, conditions, dependencies, and cancellation or timeout effects on release artifacts.

For each consequential change, identify the promised behavior, trace the implementation or evidence that establishes it, and try a concrete counterexample before accepting it. For an algorithm, check its preconditions, boundary cases and failure behavior against its callers; for a factual claim, ask whether the cited check could pass while the claim is false. Examine relevant unchanged code rather than treating the diff or a passing CI result as the complete proof. Choose checks relevant to the change, not a mechanical checklist applied to every file.

- Preserve truthful evidence and freshness: incomplete, stale, or approximate results must not become complete, fresh, or exact claims. Preserve deterministic JSON and ordering where the contract requires it. Fix the computation or retain an honest limitation label rather than hiding missing evidence.
- Preserve local-first trust boundaries: confine paths to their allowed roots, keep MCP read-only, and keep core operations offline. External providers require explicit opt-in. Reject unsafe input before side effects; preserve validation and error handling on failure paths.
- Preserve existing CLI, MCP, JSON, and persisted-data contracts. Trace affected consumers before flagging a break; intentional contract changes need an explicit migration or compatibility path. Check empty and partial inputs, duplicate records, stale state, platform-specific paths, and interrupted operations when relevant to the change.

Report actionable defects introduced by the PR, with a concrete triggering condition, user-visible consequence, and the smallest relevant changed line range. Support claims with code, a reproducible example, or an existing contract; state necessary assumptions. Check whether existing guards or callers already prevent the failure before reporting it. Missing tests alone, formatting, naming preferences, and speculative future requirements are not defects. Do not inflate severity or invent findings to fill a quota; a review with no supported findings is valid. Do not claim tests were run without execution evidence. CI and required approvals remain the enforcement mechanisms.

### Documentation and evidence claims

Documentation-only PRs require correctness review too. For each consequential new or strengthened claim (test completion, supported platforms, security properties, release readiness, accuracy or performance), trace it to the relevant implementation and primary evidence. The PR description and another document repeating the claim are not independent verification.

- Inspect the referenced workflow and test assertions. Check the exact revision/tag, assets, inputs, runner architecture, permissions, conditional steps and failure propagation. A green job alone does not prove that the intended checks ran or that their assertions support the claim.
- For cited Actions runs, inspect job/step conclusions and relevant logs when available. Match reported counts and outcomes to actual assertions; distinguish skipped checks, static checks, smoke tests and execution under an unprivileged account. Limit conclusions to the versions, platforms, corpus and conditions actually exercised.
- Compare changed claims with related README, changelog, roadmap and release records. Preserve historical statements with dated amendments; flag only contradictions that materially mislead users or release decisions.
- If primary evidence cannot be accessed, mark the claim as unverified in the review summary and name the missing evidence. Do not fabricate a defect or treat unavailable evidence as proof of failure. Never say a claim was verified from its wording alone.

When the review interface supports a summary, briefly identify the claims checked, the evidence inspected (paths/revisions/run IDs), and remaining verification limits, including when there are no findings. Keep inline findings for supported actionable defects; do not inflate severity to bypass the interface's reporting threshold. These instructions guide review effort and do not guarantee external-log access or override the hosted review service's output format.

## Project Structure & Module Organization

Riltra is a local-first Rust CLI and MCP server. The Cargo workspace contains one binary crate, `crates/cli`: implementation lives in `src/`, integration tests in `tests/`, and example helpers in `examples/`. Test fixtures and byte-exact golden outputs are under `crates/cli/tests/fixtures/` and `golden/`. Shared fixtures, protocol definitions, documentation, release scripts, and security tooling live in `fixtures/`, `proto/`, `docs/`, `release/`, and `security/` respectively.

## Build, Test, and Development Commands

Use stable Rust and Git. From the repository root:

- `cargo build --locked` builds the CLI with the committed lockfile.
- `cargo run -q -p riltra -- --help` runs the CLI and lists commands.
- `cargo fmt --all -- --check` checks formatting; `cargo fmt --all` applies it.
- `cargo clippy --all-targets -- -D warnings` checks lint warnings.
- `cargo check --locked` checks compilation; `cargo test --locked` runs the test suite.
- `cargo test --locked --test mcp` runs one integration test target.

CI checks Windows, Linux, and macOS. Set `RILTRA_HOME` to a scratch directory when manually running commands that create registries or databases.

On a fresh checkout, run `cargo fetch --locked` before `cargo test`: the dependency-audit integration tests invoke nested `cargo tree --offline` commands and need the locked registry sources cached, including platform-specific packages. This fetch is build/test tooling only; Riltra's core runtime remains offline. Repeat the fetch before running those tests with Cargo offline mode if the cache is incomplete.

## Coding Style & Testing

Follow Rust 2021 conventions and rustfmt: four-space indentation, `snake_case` functions and modules, and `CamelCase` types. Add `// SPDX-License-Identifier: Apache-2.0` to new source files. Keep unit tests inline and integration tests in the relevant `crates/cli/tests/*.rs` target; use descriptive test names. Tests use Rust's built-in test framework and isolated temporary homes. Update fixtures and goldens deliberately, reviewing byte-exact output changes. No numeric coverage threshold is specified.

Preserve deterministic JSON, evidence and freshness labels, path confinement, and read-only MCP behavior. Core paths must not access the network; external providers are opt-in. Do not hand-edit generated `scip_proto.rs`.

Choose the simplest design that meets the current requirement. Give a function or module one responsibility, and do not add traits, generics, options or layers for variation that does not exist yet. Do not refactor unrelated code to satisfy a design principle. Follow the conventions of the surrounding module; if a design preference conflicts with an existing CLI, MCP, JSON or stored-data contract, the contract wins and the conflict is stated in the PR. A `// shortcut:` comment marks a deliberate simplification together with its upgrade trigger; keep it accurate.

## Commits & Pull Requests

Recent commits use concise conventional-style subjects such as `test: document impact evidence coverage gaps` and `docs(e2): record baseline`. Every commit requires a DCO sign-off: use `git commit -s`. In pull requests, describe the change and its validation, link relevant issues, and include evidence for behavioral changes. Run formatting, Clippy, and tests before pushing. Document each new dependency's package, version, license, and origin in `docs/licensing.md` in the same PR; see `CONTRIBUTING.md` for licensing and benchmark requirements.

Before committing or pushing, set GITLEAKS_BIN to your local pinned Gitleaks 8.30.1 executable if it is not on PATH. See security/README.md for installation and scan details.
