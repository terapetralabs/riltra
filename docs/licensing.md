# Dependency & License Inventory

## CI security tooling

| Package | Version | License | Origin | Distribution |
|---|---|---|---|---|
| cargo-audit | 0.22.2 | Apache-2.0 OR MIT | https://github.com/rustsec/rustsec/tree/main/cargo-audit (crates.io) | CI/local audit tool only; not shipped with Riltra |

Re-check sources at pin time before any release.
Policy: every bundled grammar, indexer binary, or vendored source needs an
entry here (package + exact version + license + origin URL + redistributability
note) in the same PR that introduces it. Never introduce code under
noncommercial/use-restricted licenses or from sources without clear rights.

Current state: tree-sitter parsing (M2) plus a durable SQLite index via
`rusqlite` with bundled SQLite (M3), all pinned and permissive; the project
started dependency-free (see history).

## Original baseline: zero third-party dependencies (pre-syntax)

| Component | Version | License | Origin | Redistributable? |
|---|---|---|---|---|
| Rust std only (no crates.io deps) | toolchain stable | MIT OR Apache-2.0 (std) | rust-lang/rust | yes — linked std, no vendoring |
| Build/test tooling (rustc, cargo, rustfmt, clippy) | stable (see CI log) | MIT OR Apache-2.0 | rust-lang | build-time only, not shipped |

The registry core stays std-only; parsing support arrived with pinned
dependencies (see M2 addition below), each with an SBOM entry.

## Reference notes for future introductions (NOT bundled — audit per package)

- tree-sitter core: MIT — each grammar licensed separately, audit per grammar.
- SCIP spec/tooling: Apache-2.0 — record proto origin + version.
- Rust MCP SDK (`rmcp`): license changed over time; keep the exact LICENSE of
  the pinned version.
- `gix`/gitoxide: MIT OR Apache-2.0.
- clangd/LLVM: Apache-2.0 WITH LLVM-exception — keep notices.
- rust-analyzer: MIT OR Apache-2.0. petgraph: MIT OR Apache-2.0 (optional algo only).

## Audit result (rebased at M3 — the zero-dependency claim below is history)

- `cargo tree --depth 1`: `rusqlite`, `tree-sitter`, and 6 grammar crates;
  full transitive set audited in the M2/M3 sections below.
- License risk: permissive-only (MIT / Apache-2.0 / Unlicense / Zlib /
  Unicode-3.0); no copyleft, no noncommercial code.

## M2 addition: tree-sitter syntax baseline (pinned versions, verified 2026-10-14)

Direct dependencies (all permissive; `Cargo.lock` pins exact versions):

| Package | Version | License | Origin |
|---|---|---|---|
| tree-sitter | 0.25.10 | MIT (declared in package metadata) | https://github.com/tree-sitter/tree-sitter |
| tree-sitter-python | 0.25.0 | MIT (LICENSE file, Max Brunsfeld 2016) | https://github.com/tree-sitter/tree-sitter-python |
| tree-sitter-javascript | 0.25.0 | MIT (LICENSE file, Max Brunsfeld 2014) | https://github.com/tree-sitter/tree-sitter-javascript |
| tree-sitter-typescript | 0.23.2 | MIT (declared; covers typescript + tsx) | https://github.com/tree-sitter/tree-sitter-typescript |
| tree-sitter-rust | 0.24.2 | MIT (LICENSE file, Maxim Sokolov 2017) | https://github.com/tree-sitter/tree-sitter-rust |
| tree-sitter-c | 0.24.2 | MIT (LICENSE file, Max Brunsfeld 2014) | https://github.com/tree-sitter/tree-sitter-c |
| tree-sitter-cpp | 0.23.4 | MIT (declared) | https://github.com/tree-sitter/tree-sitter-cpp |

Build-time transitive dependencies (compiled into the binary via `cc` build scripts):

| Package | Version | License |
|---|---|---|
| cc | 1.8.0 | MIT OR Apache-2.0 |
| find-msvc-tools | 0.1.14 | MIT OR Apache-2.0 |
| shlex | 2.0.1 | MIT OR Apache-2.0 |
| tree-sitter-language | 0.1.9 | MIT (LICENSE file) |
| streaming-iterator | 0.1.9 | MIT OR Apache-2.0 |
| memchr / regex / regex-syntax / regex-automata / aho-corasick | 2.8.3 / 1.13.1 / 0.8.11 / 0.4.18 / 1.1.5 | MIT OR Apache-2.0 / Unlicense OR MIT |
| serde_core / serde_json / itoa / zmij / indexmap / hashbrown / equivalent / foldhash | as pinned in `Cargo.lock` | MIT OR Apache-2.0 (zmij: MIT) |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |

Notes:

- Grammar `.scm` query files are NOT used or vendored: all captures are
  self-authored node-kind matching in `crates/cli/src/syntax.rs`.
- No grammar, parser source, or query file is copied into this repository;
  everything arrives via crates.io with pinned versions.
- At M2, `proc-macro2`/`quote`/`syn`/`serde`/`serde_derive` appeared in the
  resolve graph but were not compiled into that feature set (absent from the
  build cache). Since M7, `serde` is a direct dependency and is compiled into
  the binary (see the M7 addition below); since M8 CP3 the proc-macro trio is
  compiled via `rmcp-macros` (see the CP3 addition below).

## M3 addition: durable SQLite index (pinned versions, verified 2026-10-14)

| Package | Version | License | Origin |
|---|---|---|---|
| rusqlite | 0.40.2 | MIT (declared in package metadata) | https://github.com/rusqlite/rusqlite |
| libsqlite3-sys | 0.38.2 | MIT (declared); bundled SQLite amalgamation is public domain | bundled via `rusqlite/bundled` |
| smallvec | 1.16.3 | MIT OR Apache-2.0 | transitive via rusqlite |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | transitive via rusqlite |
| fallible-iterator | 0.3.0 | MIT/Apache-2.0 | transitive via rusqlite |
| fallible-streaming-iterator | 0.1.9 | MIT/Apache-2.0 | transitive via rusqlite |
| hashlink | 0.12.2 | MIT OR Apache-2.0 | transitive via rusqlite |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | transitive via rusqlite |

Notes:

- The `bundled` feature compiles SQLite from source (no system library);
  FTS5 availability is pinned by test (`fts5_is_available_in_bundled_sqlite`)
  on every platform.
- No graph database, vector store, or second database engine is used.

## M7 addition: structured knowledge store (pinned versions, verified 2026-10-18)

| Package | Version | License | Origin |
|---|---|---|---|
| serde | 1.0.229 | MIT OR Apache-2.0 (declared in package metadata) | https://github.com/serde-rs/serde |

Notes:

- `serde` became a **direct dependency** in M7 (`serde = "1"` in
  `crates/cli/Cargo.toml`), used for strict JSON validation of knowledge
  records (`crates/cli/src/knowledge.rs`, `crates/cli/src/main.rs`). It is
  compiled and linked into the binary.
- Arrives via crates.io and is pinned in `Cargo.lock`; nothing is vendored.
  Redistributable: MIT OR Apache-2.0, permissive, no copyleft.
- Only the base `serde` feature set is enabled (the `derive` feature is off),
  so `serde_derive` was not compiled at M7; since M8 CP1 it is compiled as
  part of the rmcp/schemars graph (see below).

## M8 CP1 addition: MCP stdio server runtime (pinned versions, verified 2026-10-18)

Direct dependencies (owner decision D1; `rmcp` with `default-features = false`
and only the `server` + `transport-io` features — no HTTP/SSE transports,
macros, base64, auth, or elicitation):

| Package | Version | License | Origin |
|---|---|---|---|
| rmcp | 3.5.1 | Apache-2.0 (declared in package metadata; the crate package ships no standalone LICENSE file) | https://github.com/modelcontextprotocol/rust-sdk |
| tokio | 1.53.3 | MIT (declared) | https://github.com/tokio-rs/tokio |

Transitive additions pulled in by the rmcp graph (`Cargo.lock` diff at CP1: 44
new entries, none removed; all permissive, nothing vendored):

| Package | Version | License |
|---|---|---|
| futures / futures-channel / futures-core / futures-executor / futures-io / futures-macro / futures-sink / futures-task / futures-util | 0.3.34 | MIT OR Apache-2.0 |
| tracing / tracing-attributes / tracing-core | 0.1.44 / 0.1.31 / 0.1.36 | MIT |
| tokio-macros / tokio-util | 2.7.2 / 0.7.20 | MIT |
| schemars / schemars_derive / serde_derive_internals | 1.2.3 / 1.2.3 / 0.30.0 | MIT |
| uuid | 1.28.0 | Apache-2.0 OR MIT |
| chrono + platform crates (iana-time-zone, iana-time-zone-haiku, android_system_properties, core-foundation-sys, windows-core, windows-implement, windows-interface, windows-link, windows-result, windows-strings) | 0.4.45 (platform crates as pinned) | MIT OR Apache-2.0 |
| getrandom / r-efi / libc / log / num-traits / autocfg | as pinned in `Cargo.lock` | MIT OR Apache-2.0 |
| dyn-clone / pastey / ref-cast / ref-cast-impl / pin-project-lite | as pinned in `Cargo.lock` | MIT OR Apache-2.0 |
| slab / bytes | 0.4.12 / 1.12.1 | MIT |

Notes:

- `schemars` is not a direct dependency; it arrives with rmcp's `server`
  feature and is compiled into the binary, so it is inventoried here.
- The dependency audit test (`crates/cli/tests/semantic.rs`) bans protoc
  bridges, non-tokio async runtimes, dynamic loading, and network stacks
  (`reqwest`, `hyper`, `ureq`); tokio is allowed only as the M8 MCP stdio
  runtime.
- Redistributable: permissive only (MIT / Apache-2.0 alternatives), no
  copyleft, no noncommercial code.

## M8 CP3 addition: rmcp macro-based tool registration (pinned versions, verified 2026-10-18)

Enabling the `macros` feature of the already-audited `rmcp 3.5.1` dependency
(design gate §2: `#[tool_router]`/`#[tool]`/`#[tool_handler]` with
schemars-derived input schemas; no new transports, no base64/auth features).
`Cargo.lock` diff: 6 new entries, none removed; `serde`'s `derive` feature is
now declared explicitly (serde_derive was already compiled since CP1; no new
package).

| Package | Version | License | Origin |
|---|---|---|---|
| rmcp-macros | 3.5.1 | Apache-2.0 (declared in package metadata) | https://github.com/modelcontextprotocol/rust-sdk |
| darling / darling_core / darling_macro | 0.24.1 | MIT (declared) | https://github.com/TedDriggs/darling |
| ident_case | 1.0.1 | MIT/Apache-2.0 (declared) | https://github.com/TedDriggs/ident_case |
| strsim | 0.11.1 | MIT (declared) | https://github.com/rapidfuzz/strsim-rs |

Build-time transitive dependencies now compiled (previously resolve-graph
only, as pinned in `Cargo.lock`): `proc-macro2` 1.0.107, `quote` 1.0.47,
`syn` 2.0.119 + 3.0.9, `unicode-ident` 1.0.27 — all MIT OR Apache-2.0
(unicode-ident adds Unicode-3.0).

Notes:

- `rmcp-macros` is the proc-macro half of the same `rmcp` package family
  (same repository and license); it is compiled into the build, not shipped
  as a runtime library.
- Redistributable: permissive only; no copyleft, no noncommercial code,
  nothing vendored.

## Security tooling addition: gitleaks (privacy remediation, 2026-10-19)

| Package | Version | License | Origin | Redistributable? |
|---|---|---|---|---|
| gitleaks | 8.30.1 | MIT | https://github.com/gitleaks/gitleaks | tool only - pinned release binary, checksum-verified at fetch time; never compiled into or shipped with `riltra` |

Used by the CI `security` job and the optional local hooks (`security/scan.sh`,
`.githooks/`); it is not a build dependency and no code is vendored.

## R1 addition: bounded graph context (2026-11-06)

| Package | Version | License | Origin |
|---|---|---|---|
| petgraph | 0.8.3 | MIT OR Apache-2.0 | https://github.com/petgraph/petgraph (crates.io) |
| fixedbitset | 0.5.7 | MIT OR Apache-2.0 | https://github.com/petgraph/fixedbitset (transitive, crates.io) |
| foldhash | 0.1.5 | Zlib | https://github.com/orlp/foldhash (transitive, crates.io) |
| hashbrown | 0.15.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown (transitive, crates.io) |

`petgraph` supplies Dijkstra over the bounded subgraph selected from the
existing code index. It has default features disabled; the CLI remains local
and read-only for context queries. The package is permissively licensed and
is included in the generated locked-graph SBOM.

## Pilot tooling: clangd and CMake (clangd semantic provider pilot, 2026-10-21)

| Package | Version | License | Origin | Redistributable? |
|---|---|---|---|---|
| clangd (LLVM) | 23.1.4 | Apache-2.0 WITH LLVM-exception | https://github.com/llvm/llvm-project/releases | tool only - portable release archive, extracted into the pilot's own bench root by `docs/pilots/clangd/method/`; never compiled into or shipped with `riltra` |
| CMake | 4.4.4 | BSD-3-Clause | https://github.com/Kitware/CMake/releases | tool only - portable release archive, extracted into the pilot's own bench root; never compiled into or shipped with `riltra` |

Both are used by the M9 clangd pilot harness only (to run the language
server and to generate a fixture compilation database). They are not build
dependencies, not runtime dependencies, and nothing is vendored. The
compiler that produces the pilot's ground truth is the Microsoft Visual
Studio Build Tools already installed on the pilot host; it is not
redistributed and not referenced by any build.


## M9 release candidate: complete package inventory (2026-10-25)

No dependency changed between `origin/master` and the release candidate
(`Cargo.toml`, `crates/cli/Cargo.toml` and `Cargo.lock` are byte-identical to `origin/master`).
The table lists every third-party package of the locked graph, for every platform
(109 packages), generated from `release/sbom.cdx.json`; the licenses are audited by
the `new_dependencies_are_audited` test and the release tool's `--check` fails when a row is
missing or the SBOM no longer matches `Cargo.lock` (see `release/README.md`). `r-efi` is offered
as `MIT OR Apache-2.0 OR LGPL-2.1-or-later`; the MIT alternative is the one used. Seven crates
(`r-efi`, `rmcp`, `rmcp-macros`, `rsqlite-vfs`, `tree-sitter`, `tree-sitter-cpp`,
`tree-sitter-typescript`) publish no license file, so the release archive carries the
canonical text of the license each declares (`THIRD_PARTY_LICENSES.txt`).

| Package | Version | License | Origin |
|---|---|---|---|
| aho-corasick | 1.1.5 | Unlicense OR MIT | https://github.com/BurntSushi/aho-corasick |
| android_system_properties | 0.1.6 | MIT OR Apache-2.0 | https://github.com/nical/android_system_properties |
| autocfg | 1.5.1 | Apache-2.0 OR MIT | https://github.com/cuviper/autocfg |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | https://github.com/bitflags/bitflags |
| bumpalo | 3.20.3 | MIT OR Apache-2.0 | https://github.com/fitzgen/bumpalo |
| bytes | 1.12.1 | MIT | https://github.com/tokio-rs/bytes |
| cc | 1.8.0 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/cfg-if |
| chrono | 0.4.45 | MIT OR Apache-2.0 | https://github.com/chronotope/chrono |
| core-foundation-sys | 0.8.7 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs |
| darling | 0.24.1 | MIT | https://github.com/TedDriggs/darling |
| darling_core | 0.24.1 | MIT | https://github.com/TedDriggs/darling |
| darling_macro | 0.24.1 | MIT | https://github.com/TedDriggs/darling |
| dyn-clone | 1.0.20 | MIT OR Apache-2.0 | https://github.com/dtolnay/dyn-clone |
| equivalent | 1.0.2 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/equivalent |
| fallible-iterator | 0.3.0 | MIT OR Apache-2.0 | https://github.com/sfackler/rust-fallible-iterator |
| fallible-streaming-iterator | 0.1.9 | MIT OR Apache-2.0 | https://github.com/sfackler/fallible-streaming-iterator |
| find-msvc-tools | 0.1.14 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs |
| foldhash | 0.2.0 | Zlib | https://github.com/orlp/foldhash |
| futures | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-core | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-executor | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-io | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-macro | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-sink | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-task | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| futures-util | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| hashlink | 0.12.2 | MIT OR Apache-2.0 | https://github.com/djc/hashlink |
| iana-time-zone | 0.1.65 | MIT OR Apache-2.0 | https://github.com/strawlab/iana-time-zone |
| iana-time-zone-haiku | 0.1.2 | MIT OR Apache-2.0 | https://github.com/strawlab/iana-time-zone |
| ident_case | 1.0.1 | MIT OR Apache-2.0 | https://github.com/TedDriggs/ident_case |
| indexmap | 2.14.2 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/indexmap |
| itoa | 1.0.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/itoa |
| js-sys | 0.3.106 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/js-sys |
| libc | 0.2.191 | MIT OR Apache-2.0 | https://github.com/rust-lang/libc |
| libsqlite3-sys | 0.38.2 | MIT | https://github.com/rusqlite/rusqlite |
| log | 0.4.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/log |
| memchr | 2.8.3 | Unlicense OR MIT | https://github.com/BurntSushi/memchr |
| num-traits | 0.2.19 | MIT OR Apache-2.0 | https://github.com/rust-num/num-traits |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | https://github.com/matklad/once_cell |
| pastey | 0.2.3 | MIT OR Apache-2.0 | https://github.com/as1100k/pastey |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project-lite |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/pkg-config-rs |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | https://github.com/dtolnay/proc-macro2 |
| protobuf | 3.7.2 | MIT | https://github.com/stepancheg/rust-protobuf/ |
| protobuf-support | 3.7.2 | MIT | https://github.com/stepancheg/rust-protobuf/ |
| quote | 1.0.47 | MIT OR Apache-2.0 | https://github.com/dtolnay/quote |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | https://github.com/r-efi/r-efi |
| ref-cast | 1.0.27 | MIT OR Apache-2.0 | https://github.com/dtolnay/ref-cast |
| ref-cast-impl | 1.0.27 | MIT OR Apache-2.0 | https://github.com/dtolnay/ref-cast |
| regex | 1.13.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| regex-automata | 0.4.18 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| regex-syntax | 0.8.11 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| rmcp | 3.5.1 | Apache-2.0 | https://github.com/modelcontextprotocol/rust-sdk/ |
| rmcp-macros | 3.5.1 | Apache-2.0 | https://github.com/modelcontextprotocol/rust-sdk/ |
| rsqlite-vfs | 0.1.1 | MIT | crates.io |
| rusqlite | 0.40.2 | MIT | https://github.com/rusqlite/rusqlite |
| rustversion | 1.0.23 | MIT OR Apache-2.0 | https://github.com/dtolnay/rustversion |
| schemars | 1.2.3 | MIT | https://github.com/GREsau/schemars |
| schemars_derive | 1.2.3 | MIT | https://github.com/GREsau/schemars |
| serde | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_core | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_derive_internals | 0.30.0 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | https://github.com/serde-rs/json |
| shlex | 2.0.1 | MIT OR Apache-2.0 | https://github.com/comex/rust-shlex |
| slab | 0.4.12 | MIT | https://github.com/tokio-rs/slab |
| smallvec | 1.16.3 | MIT OR Apache-2.0 | https://github.com/servo/rust-smallvec |
| sqlite-wasm-rs | 0.5.5 | MIT | https://github.com/Spxg/sqlite-wasm-rs |
| streaming-iterator | 0.1.9 | MIT OR Apache-2.0 | https://github.com/sfackler/streaming-iterator |
| strsim | 0.11.1 | MIT | https://github.com/rapidfuzz/strsim-rs |
| syn | 2.0.119 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| syn | 3.0.9 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| thiserror | 1.0.69 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| tokio | 1.53.3 | MIT | https://github.com/tokio-rs/tokio |
| tokio-macros | 2.7.2 | MIT | https://github.com/tokio-rs/tokio |
| tokio-util | 0.7.20 | MIT | https://github.com/tokio-rs/tokio |
| tracing | 0.1.44 | MIT | https://github.com/tokio-rs/tracing |
| tracing-attributes | 0.1.31 | MIT | https://github.com/tokio-rs/tracing |
| tracing-core | 0.1.36 | MIT | https://github.com/tokio-rs/tracing |
| tree-sitter | 0.25.10 | MIT | https://github.com/tree-sitter/tree-sitter |
| tree-sitter-c | 0.24.2 | MIT | https://github.com/tree-sitter/tree-sitter-c |
| tree-sitter-cpp | 0.23.4 | MIT | https://github.com/tree-sitter/tree-sitter-cpp |
| tree-sitter-javascript | 0.25.0 | MIT | https://github.com/tree-sitter/tree-sitter-javascript |
| tree-sitter-language | 0.1.9 | MIT | https://github.com/tree-sitter/tree-sitter |
| tree-sitter-python | 0.25.0 | MIT | https://github.com/tree-sitter/tree-sitter-python |
| tree-sitter-rust | 0.24.2 | MIT | https://github.com/tree-sitter/tree-sitter-rust |
| tree-sitter-typescript | 0.23.2 | MIT | https://github.com/tree-sitter/tree-sitter-typescript |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/dtolnay/unicode-ident |
| uuid | 1.28.0 | Apache-2.0 OR MIT | https://github.com/uuid-rs/uuid |
| vcpkg | 0.2.15 | MIT OR Apache-2.0 | https://github.com/mcgoo/vcpkg-rs |
| wasm-bindgen | 0.2.129 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen |
| wasm-bindgen-macro | 0.2.129 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro |
| wasm-bindgen-macro-support | 0.2.129 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/main/crates/macro-support |
| wasm-bindgen-shared | 0.2.129 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/shared |
| windows-core | 0.62.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-implement | 0.60.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-interface | 0.59.3 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-link | 0.2.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-result | 0.4.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-strings | 0.5.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| zmij | 1.0.23 | MIT | https://github.com/dtolnay/zmij |
## Optional semantic graph package additions (2026-11-07)

Cargo metadata with `--all-features` resolves the complete locked graph. The rows below add every package absent from the preceding historical inventory; together, both sections cover the current lockfile. The generated SBOM is a superset of components linked by the feature-off core build.

| Package | Version | License | Origin |
|---|---|---|---|
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | https://github.com/oyvindln/adler2 |
| ahash | 0.8.12 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/ahash |
| allocator-api2 | 0.2.21 | MIT OR Apache-2.0 | https://github.com/zakarumych/allocator-api2 |
| anyhow | 1.0.104 | MIT OR Apache-2.0 | https://github.com/dtolnay/anyhow |
| approx | 0.5.1 | Apache-2.0 | https://github.com/brendanzab/approx |
| arc-swap | 1.9.3 | MIT OR Apache-2.0 | https://github.com/vorner/arc-swap |
| arrayref | 0.3.9 | BSD-2-Clause | https://github.com/droundy/arrayref |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 | https://github.com/bluss/arrayvec |
| arrow | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-arith | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-array | 58.4.1 | Apache-2.0 AND MIT | https://github.com/apache/arrow-rs |
| arrow-buffer | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-cast | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-csv | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-data | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-ipc | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-json | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-ord | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-row | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-schema | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-select | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| arrow-string | 58.4.1 | Apache-2.0 | https://github.com/apache/arrow-rs |
| async_cell | 0.2.3 | MIT | https://gitlab.com/samsartor/async_cell |
| async-channel | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-channel |
| async-compression | 0.4.52 | MIT OR Apache-2.0 | https://github.com/Nullus157/async-compression |
| async-lock | 3.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-lock |
| async-recursion | 1.2.0 | MIT OR Apache-2.0 | https://github.com/dcchut/async-recursion |
| async-trait | 0.1.92 | MIT OR Apache-2.0 | https://github.com/dtolnay/async-trait |
| atoi | 2.0.0 | MIT | https://github.com/pacman82/atoi-rs |
| atomic-waker | 1.1.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/atomic-waker |
| base64 | 0.13.1 | MIT/Apache-2.0 | https://github.com/marshallpierce/rust-base64 |
| base64 | 0.22.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 |
| base64 | 0.23.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 |
| bigdecimal | 0.4.11 | MIT/Apache-2.0 | https://github.com/akubera/bigdecimal-rs |
| bitflags | 1.3.2 | MIT/Apache-2.0 | https://github.com/bitflags/bitflags |
| bitvec | 1.1.1 | MIT | https://github.com/bitvecto-rs/bitvec |
| blake2 | 0.10.6 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| blake3 | 1.8.7 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | https://github.com/BLAKE3-team/BLAKE3 |
| block-buffer | 0.10.4 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| block-buffer | 0.12.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| bs58 | 0.5.1 | MIT/Apache-2.0 | https://github.com/Nullus157/bs58-rs |
| bytecheck | 0.8.3 | MIT | https://github.com/rkyv/bytecheck |
| bytecheck_derive | 0.8.3 | MIT | https://github.com/rkyv/bytecheck |
| bytecount | 0.6.9 | Apache-2.0/MIT | https://github.com/llogiq/bytecount |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/bytemuck |
| bytemuck_derive | 1.12.1 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/bytemuck |
| byteorder | 1.5.0 | Unlicense OR MIT | https://github.com/BurntSushi/byteorder |
| castaway | 0.2.4 | MIT | https://github.com/sagebind/castaway |
| cedarwood | 0.5.0 | BSD-2-Clause | https://github.com/MnO2/cedarwood |
| cfg_aliases | 0.2.2 | MIT | https://github.com/katharostech/cfg_aliases |
| chacha20 | 0.10.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/stream-ciphers |
| chrono-tz | 0.10.4 | MIT OR Apache-2.0 | https://github.com/chronotope/chrono-tz |
| comfy-table | 7.2.2 | MIT | https://github.com/nukesor/comfy-table |
| compact_str | 0.9.1 | MIT | https://github.com/ParkMyCar/compact_str |
| compression-codecs | 0.4.47 | MIT OR Apache-2.0 | https://github.com/Nullus157/async-compression |
| compression-core | 0.4.34 | MIT OR Apache-2.0 | https://github.com/Nullus157/async-compression |
| concurrent-queue | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/concurrent-queue |
| const-oid | 0.10.2 | Apache-2.0 OR MIT | https://github.com/RustCrypto/formats |
| const-random | 0.1.18 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/constrandom |
| const-random-macro | 0.1.16 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/constrandom |
| constant_time_eq | 0.4.2 | CC0-1.0 OR MIT-0 OR Apache-2.0 | https://github.com/cesarb/constant_time_eq |
| core_detect | 1.0.0 | MIT/Apache-2.0 | https://github.com/thomcc/core_detect |
| core-foundation | 0.10.1 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| crc32fast | 1.5.4 | MIT OR Apache-2.0 | https://github.com/srijs/rust-crc32fast |
| crossbeam-channel | 0.5.17 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| crossbeam-deque | 0.8.8 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| crossbeam-epoch | 0.9.21 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| crossbeam-queue | 0.3.14 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| crossbeam-skiplist | 0.1.3 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam |
| crunchy | 0.2.4 | MIT | https://github.com/eira-fransham/crunchy |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| crypto-common | 0.2.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| csv | 1.4.0 | Unlicense/MIT | https://github.com/BurntSushi/rust-csv |
| csv-core | 0.1.13 | Unlicense/MIT | https://github.com/BurntSushi/rust-csv |
| daachorse | 2.1.1 | MIT OR Apache-2.0 | https://github.com/daac-tools/daachorse |
| daachorse | 3.0.3 | MIT OR Apache-2.0 | https://github.com/daac-tools/daachorse |
| darling | 0.20.11 | MIT | https://github.com/TedDriggs/darling |
| darling_core | 0.20.11 | MIT | https://github.com/TedDriggs/darling |
| darling_macro | 0.20.11 | MIT | https://github.com/TedDriggs/darling |
| dary_heap | 0.3.9 | MIT OR Apache-2.0 | https://github.com/hanmertens/dary_heap |
| dashmap | 6.2.1 | MIT | https://github.com/xacrimon/dashmap |
| datafusion | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-catalog | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-catalog-listing | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-common | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-common-runtime | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-datasource | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-datasource-arrow | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-datasource-csv | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-datasource-json | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-doc | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-execution | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-expr | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-expr-common | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-functions | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-functions-aggregate | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-functions-aggregate-common | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-functions-nested | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-functions-table | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-functions-window | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-functions-window-common | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-macros | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-optimizer | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-physical-expr | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-physical-expr-adapter | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-physical-expr-common | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-physical-optimizer | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-physical-plan | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-pruning | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-session | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| datafusion-sql | 54.1.0 | Apache-2.0 | https://github.com/apache/datafusion |
| defmt | 1.2.0 | MIT OR Apache-2.0 | https://github.com/knurling-rs/defmt |
| defmt-macros | 1.2.0 | MIT OR Apache-2.0 | https://github.com/knurling-rs/defmt |
| defmt-parser | 1.1.0 | MIT OR Apache-2.0 | https://github.com/knurling-rs/defmt |
| deranged | 0.5.9 | MIT OR Apache-2.0 | https://github.com/jhpratt/deranged |
| derive_builder | 0.20.2 | MIT OR Apache-2.0 | https://github.com/colin-kiegel/rust-derive-builder |
| derive_builder_core | 0.20.2 | MIT OR Apache-2.0 | https://github.com/colin-kiegel/rust-derive-builder |
| derive_builder_macro | 0.20.2 | MIT OR Apache-2.0 | https://github.com/colin-kiegel/rust-derive-builder |
| digest | 0.10.7 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| digest | 0.11.3 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| dirs | 6.0.0 | MIT OR Apache-2.0 | https://github.com/soc/dirs-rs |
| dirs-sys | 0.5.0 | MIT OR Apache-2.0 | https://github.com/dirs-dev/dirs-sys-rs |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 | https://github.com/yaahc/displaydoc |
| either | 1.19.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/either |
| encoding_rs | 0.8.44 | (Apache-2.0 OR MIT) AND BSD-3-Clause | https://github.com/hsivonen/encoding_rs |
| encoding_rs_io | 0.1.8 | MIT OR Apache-2.0 | https://github.com/BurntSushi/encoding_rs_io |
| errno | 0.3.14 | MIT OR Apache-2.0 | https://github.com/lambda-fairy/rust-errno |
| esaxx-rs | 0.1.10 | Apache-2.0 | https://github.com/Narsil/esaxx-rs |
| ethnum | 1.5.3 | MIT OR Apache-2.0 | https://github.com/nlordell/ethnum-rs |
| event-listener | 5.4.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/event-listener |
| event-listener-strategy | 0.5.4 | Apache-2.0 OR MIT | https://github.com/smol-rs/event-listener-strategy |
| fast-float2 | 0.2.4 | MIT OR Apache-2.0 | https://github.com/Alexhuszagh/fast-float-rust |
| fastembed | 7.6.0 | Apache-2.0 | https://github.com/Anush008/fastembed-rs |
| fastrand | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/fastrand |
| flatbuffers | 25.12.19 | Apache-2.0 | https://github.com/google/flatbuffers |
| flate2 | 1.1.11 | MIT OR Apache-2.0 | https://github.com/rust-lang/flate2-rs |
| fnv | 1.0.7 | Apache-2.0 / MIT | https://github.com/servo/rust-fnv |
| form_urlencoded | 1.2.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url |
| frostem | 1.20260821.8 | BSD-3-Clause | https://github.com/Xuanwo/frostem |
| fsst | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| fst | 0.4.7 | Unlicense/MIT | https://github.com/BurntSushi/fst |
| funty | 2.0.0 | MIT | https://github.com/myrrlyn/funty |
| generator | 0.8.12 | MIT/Apache-2.0 | https://github.com/Xudong-Huang/generator-rs.git |
| generic-array | 0.14.7 | MIT | https://github.com/fizyk20/generic-array.git |
| getrandom | 0.2.17 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| getrandom | 0.3.4 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom |
| glob | 0.3.4 | MIT OR Apache-2.0 | https://github.com/rust-lang/glob |
| h2 | 0.4.20 | MIT | https://github.com/hyperium/h2 |
| half | 2.7.1 | MIT OR Apache-2.0 | https://github.com/VoidStarKat/half-rs |
| hashbrown | 0.12.3 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| hashbrown | 0.14.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| heck | 0.5.0 | MIT OR Apache-2.0 | https://github.com/withoutboats/heck |
| hermit-abi | 0.5.3 | MIT OR Apache-2.0 | https://github.com/hermit-os/hermit-rs |
| hex | 0.4.3 | MIT OR Apache-2.0 | https://github.com/KokaKiwi/rust-hex |
| http | 1.5.0 | MIT OR Apache-2.0 | https://github.com/hyperium/http |
| http-body | 1.1.0 | MIT | https://github.com/hyperium/http-body |
| http-body-util | 0.1.5 | MIT | https://github.com/hyperium/http-body |
| httparse | 1.10.1 | MIT OR Apache-2.0 | https://github.com/seanmonstar/httparse |
| humantime | 2.4.0 | MIT OR Apache-2.0 | https://github.com/chronotope/humantime |
| hybrid-array | 0.4.15 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hybrid-array |
| hyper | 1.12.0 | MIT | https://github.com/hyperium/hyper |
| hyper-rustls | 0.27.10 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/hyper-rustls |
| hyper-util | 0.1.21 | MIT | https://github.com/hyperium/hyper-util |
| hyperloglogplus | 0.4.1 | MIT | https://github.com/tabac/hyperloglog.rs |
| icu_collections | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_locale_core | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_locale_fallback | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_locale_fallback_data | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_normalizer | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_normalizer_data | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_properties | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_properties_data | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_provider | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_segmenter | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| icu_segmenter_data | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| idna | 1.1.0 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ |
| idna_adapter | 1.2.2 | Apache-2.0 OR MIT | https://github.com/hsivonen/idna_adapter |
| indexmap | 1.9.3 | Apache-2.0 OR MIT | https://github.com/bluss/indexmap |
| io-uring | 0.7.15 | MIT OR Apache-2.0 | https://github.com/tokio-rs/io-uring |
| ipnet | 2.12.2 | MIT OR Apache-2.0 | https://github.com/krisprice/ipnet |
| itertools | 0.14.0 | MIT OR Apache-2.0 | https://github.com/rust-itertools/itertools |
| itertools | 0.15.0 | MIT OR Apache-2.0 | https://github.com/rust-itertools/itertools |
| jieba-macros | 0.10.4 | MIT | https://github.com/messense/jieba-rs |
| jieba-rs | 0.10.4 | MIT | https://github.com/messense/jieba-rs |
| jiff | 0.2.39 | Unlicense OR MIT | https://github.com/BurntSushi/jiff |
| jiff-core | 0.1.2 | Unlicense OR MIT | https://github.com/BurntSushi/jiff |
| jiff-static | 0.2.39 | Unlicense OR MIT | https://github.com/BurntSushi/jiff |
| jiff-tzdb | 0.1.9 | Unlicense OR MIT | https://github.com/BurntSushi/jiff |
| jiff-tzdb-platform | 0.1.3 | Unlicense OR MIT | https://github.com/BurntSushi/jiff |
| jobserver | 0.1.35 | MIT OR Apache-2.0 | https://github.com/rust-lang/jobserver-rs |
| kanaria | 0.2.0 | MIT | https://github.com/sam-osamu/com.kanaria |
| lance | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-arrow | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-arrow-scalar | 58.0.0 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-arrow-stats | 58.0.0 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-bitpacking | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-core | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-datafusion | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-datagen | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-derive | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-encoding | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-file | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-index | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-index-core | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-io | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-jsonb | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-linalg | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-namespace | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-namespace-impls | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-namespace-reqwest-client | 0.13.0 | Apache-2.0 | https://crates.io/crates/lance-namespace-reqwest-client |
| lance-select | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-table | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lance-tokenizer | 14.0.2 | Apache-2.0 | https://github.com/lance-format/lance |
| lancedb | 0.41.0 | Apache-2.0 | https://github.com/lancedb/lancedb |
| lazy_static | 1.5.1 | MIT OR Apache-2.0 | https://github.com/rust-lang-nursery/lazy-static.rs |
| lexical-core | 1.0.6 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-parse-float | 1.0.6 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-parse-integer | 1.0.6 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-util | 1.0.7 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-write-float | 1.0.6 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-write-integer | 1.0.6 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| libloading | 0.9.0 | ISC | https://github.com/nagisa/rust_libloading/ |
| libm | 0.2.16 | MIT | https://github.com/rust-lang/compiler-builtins |
| libredox | 0.1.25 | MIT | https://gitlab.redox-os.org/redox-os/libredox.git |
| libsais-rs | 0.2.2 | Apache-2.0 | https://github.com/henriksson-lab/libsais-rs |
| lindera | 3.0.7 | MIT | https://github.com/lindera/lindera |
| lindera-dictionary | 3.0.7 | MIT | https://github.com/lindera/lindera |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/sunfishcode/linux-raw-sys |
| litemap | 0.8.3 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| lock_api | 0.4.14 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| loom | 0.7.2 | MIT | https://github.com/tokio-rs/loom |
| lru-slab | 0.1.3 | MIT OR Apache-2.0 OR Zlib | https://github.com/Ralith/lru-slab |
| lz4 | 1.28.1 | MIT | https://github.com/10xGenomics/lz4-rs |
| lz4_flex | 0.13.1 | MIT | https://github.com/pseitz/lz4_flex |
| lz4-sys | 1.11.1+lz4-1.10.0 | MIT | https://github.com/10xGenomics/lz4-rs |
| macro_rules_attribute | 0.2.3 | Apache-2.0 OR MIT OR Zlib | https://github.com/danielhenrymantilla/macro_rules_attribute-rs |
| macro_rules_attribute-proc_macro | 0.2.3 | Apache-2.0 OR MIT OR Zlib | https://github.com/danielhenrymantilla/macro_rules_attribute-rs |
| matchers | 0.2.0 | MIT | https://github.com/hawkw/matchers |
| matrixmultiply | 0.3.11 | MIT/Apache-2.0 | https://github.com/bluss/matrixmultiply/ |
| md-5 | 0.11.0 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| memmap2 | 0.9.12 | MIT OR Apache-2.0 | https://github.com/RazrFalcon/memmap2-rs |
| mime | 0.3.18 | MIT OR Apache-2.0 | https://github.com/hyperium/mime |
| mime_guess | 2.0.5 | MIT | https://github.com/abonander/mime_guess |
| minimal-lexical | 0.2.1 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/minimal-lexical |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 | https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide |
| mio | 1.2.4 | MIT | https://github.com/tokio-rs/mio |
| moka | 0.12.16 | (MIT OR Apache-2.0) AND Apache-2.0 | https://github.com/moka-rs/moka |
| monostate | 0.1.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/monostate |
| monostate-impl | 0.1.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/monostate |
| multimap | 0.10.1 | MIT OR Apache-2.0 | https://github.com/havarnov/multimap |
| multiversion_no_op | 1.0.0 | Apache-2.0 OR MIT | https://github.com/hsivonen/multiversion_no_op |
| munge | 0.4.7 | MIT | https://github.com/djkoloski/munge |
| munge_macro | 0.4.7 | MIT | https://github.com/djkoloski/munge |
| nalgebra | 0.33.3 | Apache-2.0 | https://github.com/dimforge/nalgebra |
| ndarray | 0.16.1 | MIT OR Apache-2.0 | https://github.com/rust-ndarray/ndarray |
| ndarray | 0.17.2 | MIT OR Apache-2.0 | https://github.com/rust-ndarray/ndarray |
| nix | 0.31.3 | MIT | https://github.com/nix-rust/nix |
| nom | 7.1.3 | MIT | https://github.com/Geal/nom |
| nom | 8.0.0 | MIT | https://github.com/rust-bakery/nom |
| nu-ansi-term | 0.50.3 | MIT | https://github.com/nushell/nu-ansi-term |
| num_cpus | 1.17.0 | MIT OR Apache-2.0 | https://github.com/seanmonstar/num_cpus |
| num-bigint | 0.4.8 | MIT OR Apache-2.0 | https://github.com/rust-num/num-bigint |
| num-complex | 0.4.6 | MIT OR Apache-2.0 | https://github.com/rust-num/num-complex |
| num-conv | 0.2.2 | MIT OR Apache-2.0 | https://github.com/jhpratt/num-conv |
| num-integer | 0.1.47 | MIT OR Apache-2.0 | https://github.com/rust-num/num-integer |
| num-rational | 0.4.2 | MIT OR Apache-2.0 | https://github.com/rust-num/num-rational |
| object_store | 0.13.2 | MIT/Apache-2.0 | https://github.com/apache/arrow-rs-object-store |
| object_store | 0.14.2 | MIT/Apache-2.0 | https://github.com/apache/arrow-rs-object-store |
| onig | 6.5.3 | MIT | https://github.com/iwillspeak/rust-onig |
| onig_sys | 69.9.3 | MIT | https://github.com/rust-onig/rust-onig |
| openssl-probe | 0.2.2 | MIT OR Apache-2.0 | https://github.com/rustls/openssl-probe |
| option-ext | 0.2.0 | MPL-2.0 | https://github.com/soc/option-ext.git |
| ordered-float | 5.5.1 | MIT | https://github.com/reem/rust-ordered-float |
| ort | 2.0.0-rc.15 | MIT OR Apache-2.0 | https://github.com/pykeio/ort |
| ort-sys | 2.0.0-rc.15 | MIT OR Apache-2.0 | https://github.com/pykeio/ort |
| parking | 2.2.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/parking |
| parking_lot | 0.12.5 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| parking_lot_core | 0.9.12 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot |
| paste | 1.0.15 | MIT OR Apache-2.0 | https://github.com/dtolnay/paste |
| path_abs | 0.5.1 | MIT OR Apache-2.0 | https://github.com/vitiral/path_abs |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ |
| permutation | 0.4.1 | Apache-2.0/MIT | https://github.com/jeremysalwen/rust-permutations |
| phf | 0.12.1 | MIT | https://github.com/rust-phf/rust-phf |
| phf | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| phf_codegen | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| phf_generator | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| phf_shared | 0.12.1 | MIT | https://github.com/rust-phf/rust-phf |
| phf_shared | 0.13.1 | MIT | https://github.com/rust-phf/rust-phf |
| pin-project | 1.1.13 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project |
| pin-project-internal | 1.1.13 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project |
| portable-atomic | 1.15.0 | Apache-2.0 OR MIT | https://github.com/taiki-e/portable-atomic |
| portable-atomic-util | 0.2.8 | Apache-2.0 OR MIT | https://github.com/taiki-e/portable-atomic-util |
| potential_utf | 0.1.6 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| powerfmt | 0.2.1 | MIT OR Apache-2.0 | https://github.com/jhpratt/powerfmt |
| ppv-lite86 | 0.2.21 | MIT OR Apache-2.0 | https://github.com/cryptocorrosion/cryptocorrosion |
| prettyplease | 0.2.37 | MIT OR Apache-2.0 | https://github.com/dtolnay/prettyplease |
| prost | 0.14.4 | Apache-2.0 | https://github.com/tokio-rs/prost |
| prost-build | 0.14.4 | Apache-2.0 | https://github.com/tokio-rs/prost |
| prost-derive | 0.14.4 | Apache-2.0 | https://github.com/tokio-rs/prost |
| prost-types | 0.14.4 | Apache-2.0 | https://github.com/tokio-rs/prost |
| ptr_meta | 0.3.2 | MIT | https://github.com/rkyv/ptr_meta |
| ptr_meta_derive | 0.3.2 | MIT | https://github.com/rkyv/ptr_meta |
| quick_cache | 0.6.24 | MIT | https://github.com/arthurprs/quick-cache |
| quinn | 0.11.12 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn |
| quinn-proto | 0.11.19 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn |
| quinn-udp | 0.5.16 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn |
| r-efi | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | https://github.com/r-efi/r-efi |
| radium | 0.7.0 | MIT | https://github.com/bitvecto-rs/radium |
| rancor | 0.1.3 | MIT | https://github.com/rkyv/rancor |
| rand | 0.10.3 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| rand | 0.9.5 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| rand_chacha | 0.9.0 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| rand_core | 0.10.1 | MIT OR Apache-2.0 | https://github.com/rust-random/rand_core |
| rand_core | 0.9.5 | MIT OR Apache-2.0 | https://github.com/rust-random/rand |
| rand_distr | 0.5.1 | MIT OR Apache-2.0 | https://github.com/rust-random/rand_distr |
| rand_pcg | 0.10.2 | MIT OR Apache-2.0 | https://github.com/rust-random/rngs |
| rand_xoshiro | 0.7.0 | MIT OR Apache-2.0 | https://github.com/rust-random/rngs |
| rangemap | 1.8.0 | MIT/Apache-2.0 | https://github.com/jeffparsons/rangemap |
| rawpointer | 0.2.1 | MIT/Apache-2.0 | https://github.com/bluss/rawpointer/ |
| rayon | 1.12.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/rayon |
| rayon-cond | 0.4.0 | Apache-2.0/MIT | https://github.com/cuviper/rayon-cond |
| rayon-core | 1.13.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/rayon |
| redox_syscall | 0.5.18 | MIT | https://gitlab.redox-os.org/redox-os/syscall |
| redox_users | 0.5.3 | MIT | https://gitlab.redox-os.org/redox-os/users |
| rend | 0.5.4 | MIT | https://github.com/djkoloski/rend |
| reqwest | 0.12.28 | MIT OR Apache-2.0 | https://github.com/seanmonstar/reqwest |
| ring | 0.17.14 | Apache-2.0 AND ISC | https://github.com/briansmith/ring |
| rkyv | 0.8.18 | MIT | https://github.com/rkyv/rkyv |
| rkyv_derive | 0.8.18 | MIT | https://github.com/rkyv/rkyv |
| roaring | 0.11.5 | MIT OR Apache-2.0 | https://github.com/RoaringBitmap/roaring-rs |
| rustc_version | 0.4.1 | MIT OR Apache-2.0 | https://github.com/djc/rustc-version-rs |
| rustc-hash | 2.1.3 | Apache-2.0 OR MIT | https://github.com/rust-lang/rustc-hash |
| rustix | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/rustix |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/rustls |
| rustls-native-certs | 0.8.5 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/rustls-native-certs |
| rustls-pki-types | 1.15.1 | MIT OR Apache-2.0 | https://github.com/rustls/pki-types |
| rustls-webpki | 0.103.16 | ISC | https://github.com/rustls/webpki |
| ryu | 1.0.23 | Apache-2.0 OR BSL-1.0 | https://github.com/dtolnay/ryu |
| safe_arch | 0.7.4 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/safe_arch |
| safetensors | 0.8.0 | Apache-2.0 | https://github.com/huggingface/safetensors |
| same-file | 1.0.6 | Unlicense/MIT | https://github.com/BurntSushi/same-file |
| schannel | 0.1.30 | MIT | https://github.com/steffengy/schannel-rs |
| schemars | 0.9.0 | MIT | https://github.com/GREsau/schemars |
| scoped-tls | 1.0.1 | MIT/Apache-2.0 | https://github.com/alexcrichton/scoped-tls |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 | https://github.com/bluss/scopeguard |
| security-framework | 3.7.0 | MIT OR Apache-2.0 | https://github.com/kornelski/rust-security-framework |
| security-framework-sys | 2.17.0 | MIT OR Apache-2.0 | https://github.com/kornelski/rust-security-framework |
| semver | 1.0.28 | MIT OR Apache-2.0 | https://github.com/dtolnay/semver |
| seq-macro | 0.3.6 | MIT OR Apache-2.0 | https://github.com/dtolnay/seq-macro |
| serde_repr | 0.1.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/serde-repr |
| serde_urlencoded | 0.7.1 | MIT/Apache-2.0 | https://github.com/nox/serde_urlencoded |
| serde_with | 3.24.0 | MIT OR Apache-2.0 | https://github.com/jonasbb/serde_with/ |
| serde_with_macros | 3.24.0 | MIT OR Apache-2.0 | https://github.com/jonasbb/serde_with/ |
| serde_yaml_ng | 0.10.0 | MIT | https://github.com/acatton/serde-yaml-ng |
| sha1_smol | 1.0.1 | BSD-3-Clause | https://github.com/mitsuhiko/sha1-smol |
| sha2 | 0.10.9 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| sha2 | 0.11.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| sharded-slab | 0.1.7 | MIT | https://github.com/hawkw/sharded-slab |
| signal-hook-registry | 1.4.8 | MIT OR Apache-2.0 | https://github.com/vorner/signal-hook |
| simba | 0.9.1 | Apache-2.0 | https://github.com/dimforge/simba |
| simd-adler32 | 0.3.10 | MIT | https://github.com/mcountryman/simd-adler32 |
| simdutf8 | 0.1.5 | MIT OR Apache-2.0 | https://github.com/rusticstuff/simdutf8 |
| siphasher | 1.0.4 | MIT OR Apache-2.0 | https://github.com/jedisct1/rust-siphash |
| snafu | 0.8.9 | MIT OR Apache-2.0 | https://github.com/shepmaster/snafu |
| snafu | 0.9.2 | MIT OR Apache-2.0 | https://github.com/shepmaster/snafu |
| snafu-derive | 0.8.9 | MIT OR Apache-2.0 | https://github.com/shepmaster/snafu |
| snafu-derive | 0.9.2 | MIT OR Apache-2.0 | https://github.com/shepmaster/snafu |
| socket2 | 0.6.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/socket2 |
| spm_precompiled | 0.1.4 | Apache-2.0 | https://github.com/huggingface/spm_precompiled |
| sqlparser | 0.62.0 | Apache-2.0 | https://github.com/apache/datafusion-sqlparser-rs |
| sqlparser_derive | 0.5.0 | Apache-2.0 | https://github.com/sqlparser-rs/sqlparser-rs |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 | https://github.com/storyyeller/stable_deref_trait |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 | https://github.com/nvzqz/static-assertions-rs |
| std_prelude | 0.2.12 | MIT | https://github.com/vitiral/std_prelude |
| stfu8 | 0.2.7 | MIT OR Apache-2.0 | https://github.com/vitiral/stfu8 |
| stop-words | 0.10.1 | MIT OR Apache-2.0 | https://github.com/cmccomb/rust-stop-words |
| strum | 0.28.0 | MIT | https://github.com/Peternator7/strum |
| strum_macros | 0.28.0 | MIT | https://github.com/Peternator7/strum |
| subtle | 2.6.1 | BSD-3-Clause | https://github.com/dalek-cryptography/subtle |
| sync_wrapper | 1.0.2 | Apache-2.0 | https://github.com/Actyx/sync_wrapper |
| synstructure | 0.14.0 | MIT | https://github.com/mystor/synstructure |
| tagptr | 0.2.0 | MIT/Apache-2.0 | https://github.com/oliver-giersch/tagptr.git |
| tap | 1.0.1 | MIT | https://github.com/myrrlyn/tap |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | https://github.com/Stebalien/tempfile |
| thread_local | 1.1.10 | MIT OR Apache-2.0 | https://github.com/Amanieu/thread_local-rs |
| thread-tree | 0.3.3 | MIT OR Apache-2.0 | https://github.com/bluss/thread-tree |
| time | 0.3.55 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| time-core | 0.1.9 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| time-macros | 0.2.32 | MIT OR Apache-2.0 | https://github.com/time-rs/time |
| tiny-keccak | 2.0.2 | CC0-1.0 | https://crates.io/crates/tiny-keccak |
| tinystr | 0.8.5 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| tinyvec | 1.13.3 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/tinyvec |
| tokenizers | 0.23.2 | Apache-2.0 | https://github.com/huggingface/tokenizers |
| tokio-rustls | 0.26.6 | MIT OR Apache-2.0 | https://github.com/rustls/tokio-rustls |
| tokio-stream | 0.1.19 | MIT | https://github.com/tokio-rs/tokio |
| tower | 0.5.3 | MIT | https://github.com/tower-rs/tower |
| tower-http | 0.6.11 | MIT | https://github.com/tower-rs/tower-http |
| tower-layer | 0.3.3 | MIT | https://github.com/tower-rs/tower |
| tower-service | 0.3.3 | MIT | https://github.com/tower-rs/tower |
| tracing-log | 0.2.0 | MIT | https://github.com/tokio-rs/tracing |
| tracing-subscriber | 0.3.23 | MIT | https://github.com/tokio-rs/tracing |
| try-lock | 0.2.5 | MIT | https://github.com/seanmonstar/try-lock |
| twox-hash | 2.1.5 | MIT | https://github.com/shepmaster/twox-hash |
| typenum | 1.20.1 | MIT OR Apache-2.0 | https://github.com/paholg/typenum |
| unicase | 2.10.0 | MIT OR Apache-2.0 | https://github.com/seanmonstar/unicase |
| unicode_categories | 0.1.1 | MIT OR Apache-2.0 | https://github.com/swgillespie/unicode-categories |
| unicode-blocks | 0.1.11 | MIT | https://github.com/magiclen/unicode-blocks |
| unicode-normalization | 0.1.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/unicode-rs/unicode-normalization |
| unicode-normalization-alignments | 0.1.12 | MIT/Apache-2.0 | https://github.com/n1t0/unicode-normalization |
| unicode-segmentation | 1.13.3 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-segmentation |
| unicode-width | 0.2.3 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-width |
| unsafe-libyaml | 0.2.11 | MIT | https://github.com/dtolnay/unsafe-libyaml |
| untrusted | 0.9.0 | ISC | https://github.com/briansmith/untrusted |
| url | 2.5.8 | MIT OR Apache-2.0 | https://github.com/servo/rust-url |
| utf_types | 1.0.0 | Apache-2.0 OR MIT | https://github.com/hsivonen/utf_types |
| utf8_iter | 1.1.0 | Apache-2.0 OR MIT | https://github.com/hsivonen/utf8_iter |
| utf8-ranges | 1.0.5 | Unlicense/MIT | https://github.com/BurntSushi/utf8-ranges |
| valuable | 0.1.1 | MIT | https://github.com/tokio-rs/valuable |
| version_check | 0.9.5 | MIT/Apache-2.0 | https://github.com/SergioBenitez/version_check |
| walkdir | 2.5.0 | Unlicense/MIT | https://github.com/BurntSushi/walkdir |
| want | 0.3.2 | MIT | https://github.com/seanmonstar/want |
| wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wasi |
| wasip2 | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wasi-rs |
| wasm-bindgen-futures | 0.4.79 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/futures |
| wasm-streams | 0.4.2 | MIT OR Apache-2.0 | https://github.com/MattiasBuelens/wasm-streams/ |
| web-sys | 0.3.106 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/web-sys |
| web-time | 1.1.0 | MIT OR Apache-2.0 | https://github.com/daxpedda/web-time |
| wide | 0.7.33 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/wide |
| winapi-util | 0.1.11 | Unlicense OR MIT | https://github.com/BurntSushi/winapi-util |
| windows_aarch64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows_aarch64_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows_i686_gnu | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows_i686_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows_i686_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows_x86_64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-core | 0.100.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-implement | 0.100.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-interface | 0.100.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-link | 0.100.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-result | 0.100.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-strings | 0.100.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-sys | 0.52.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs |
| wit-bindgen | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wit-bindgen |
| writeable | 0.6.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| wyz | 0.5.1 | MIT | https://github.com/myrrlyn/wyz |
| xxhash-rust | 0.8.19 | BSL-1.0 | https://github.com/DoumanAsh/xxhash-rust |
| yoke | 0.8.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| yoke-derive | 0.8.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| zerocopy | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy |
| zerocopy-derive | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy |
| zerofrom | 0.1.8 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| zerofrom-derive | 0.1.8 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| zeroize | 1.9.1 | Apache-2.0 OR MIT | https://github.com/RustCrypto/utils |
| zerotrie | 0.2.5 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| zerovec | 0.11.9 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| zerovec-derive | 0.11.7 | Unicode-3.0 | https://github.com/unicode-org/icu4x |
| zlib-rs | 0.6.8 | Zlib | https://github.com/trifectatechfoundation/zlib-rs |
| zstd | 0.13.4 | BSD-3-Clause | https://github.com/gyscos/zstd-rs |
| zstd | 0.14.3 | BSD-3-Clause | https://github.com/gyscos/zstd-rs |
| zstd-safe | 7.4.0 | BSD-3-Clause | https://github.com/gyscos/zstd-rs |
| zstd-safe | 8.2.0 | BSD-3-Clause | https://github.com/gyscos/zstd-rs |
| zstd-sys | 2.1.2+zstd.1.5.7 | BSD-3-Clause | https://github.com/gyscos/zstd-rs |

## R2a direct dependencies and distribution scope

| Package | Version | License | Origin | Use and distribution |
|---|---|---|---|---|
| fastembed | 7.6.0 | Apache-2.0 | https://github.com/Anush008/fastembed-rs | Optional `semantic-knowledge` source build; not linked in official feature-off archives. Hub downloads disabled. |
| futures-util | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | Optional bounded streaming of LanceDB validation batches; not linked in official feature-off archives. |
| lancedb | 0.41.0 | Apache-2.0 | https://github.com/lancedb/lancedb | Optional local vector-store source build; not linked in official feature-off archives. |
| ort | 2.0.0-rc.15 | MIT OR Apache-2.0 | https://github.com/pykeio/ort | Optional runtime loader; dynamic library must be explicitly provisioned locally and hash-verified. |
| sha2 | 0.10.9 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes | Optional SHA-256 asset verification. |
| tokenizers | 0.23.2 | Apache-2.0 | https://github.com/huggingface/tokenizers | Optional local token counting and model tokenizer. |

The optional LanceDB graph includes `reqwest` through `lance-namespace-reqwest-client` even with LanceDB default features disabled. The feature audit records this network-capable transitive dependency separately; the task 3 inference loader only accepts local paths and never calls that client. Core feature selection rejects HTTP clients and dynamic loading.

Two optional native crates bundle separately licensed C sources: `onig_sys` 69.9.3 declares MIT, while its bundled Oniguruma code in `oniguruma/COPYING` is BSD-2-Clause; `lz4-sys` 1.11.1+lz4-1.10.0 declares MIT, while its LZ4 source has a BSD-2-Clause license in `liblz4/lib/LICENSE`. The separately compiled `liblz4/lib/xxhash.c` also carries Yann Collet's xxHash BSD-2-Clause copyright and license header; that attribution is not covered by the LZ4 license file. Release notice generation includes both nested license files and the xxHash source header, and fails if either license text or the expected header is missing or unrecognized. The `lz4-sys` build script compiles the `liblz4/lib/` sources, not the separate `liblz4/programs/` GPL code.

The generated SBOM combines the Cargo crate license expression with BSD-2-Clause for these two bundled-source components; the Cargo tree audit continues to check Cargo's published metadata expression separately.

`option-ext` 0.2.0 is MPL-2.0 and is reached through the optional Lance index/dirs graph. The official archive remains feature-off and excludes this package. The release notice generator includes the crate license text. Mozilla describes MPL-2.0 as file-level copyleft and says unchanged executable distributors must tell recipients where source for the MPL-covered code is available; optional executable distribution must meet that source notice requirement before it is offered: https://www.mozilla.org/en-US/MPL/2.0/FAQ/.

The remaining optional SPDX expressions are reviewed as exact alternatives in the feature audit. In particular, `LGPL-2.1-or-later` appears only as an alternative alongside MIT and Apache-2.0; `BSL-1.0` is Boost Software License 1.0, and is distinct from the Business Source License.
