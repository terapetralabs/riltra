# Capabilities and limits

Riltra indexes local source files and stored project notes. Its syntax layer
reports facts it can identify and labels uncertain relationships; it does not
guess unsupported languages or claim that a name match proves a call. Use this
guide to check the current scope before relying on an answer.

## Language support

| Language | Extensions | Syntax facts | Optional semantic evidence |
|---|---|---|---|
| Python | `.py` | Definitions, imports, and call-site candidates | SCIP import contract only |
| JavaScript | `.js`, `.mjs`, `.cjs` | Same | SCIP import contract only |
| TypeScript / TSX | `.ts`, `.mts`, `.cts`, `.tsx` | Same | SCIP import verified on a fixture with `scip-typescript` |
| Rust | `.rs` | Same | SCIP import verified on a fixture with `rust-analyzer` |
| C | `.c` | Same | SCIP import contract only |
| C++ | `.cc`, `.cpp`, `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx`, `.inl` | Same, plus classes, structs, and data members | Configured language-server query (such as clangd); not verified on a real project |
| Go | `.go` | Functions, methods, struct/interface/type declarations, imports (a grouped `import (...)` is one fact), and call-site candidates | None; no SCIP or language-server evidence has been tried |
| CUDA | `.cu`, `.cuh` | Unsupported; not parsed | None |

Other extensions are reported as unsupported. Headers are parsed as C++; a
`.inl` fragment that needs its header context may contain syntax errors.
Macro interiors and C++ templates are opaque to the syntax layer, and `.ipp`
and `.tpp` files are not indexed.

The optional SCIP path imports an index that you provide; it does not run the
indexer. A configured language server answers definition or reference queries
at a position. It must be explicitly configured, and may have its own project
or network behavior. Neither provider is enabled by default.

## Search, relationships, and freshness

The durable code index stores syntax facts and source digests. Symbol search
returns indexed definitions and locations. In C++, `search --occurrences`
adds an exact-name census of occurrences such as member accesses, address of a
member, and same-file macro arguments. It is a name census, not a resolved
binding; an empty result does not prove that a name is unused.

Syntax-only call sites are `POTENTIAL_CALL` candidates matched by name. They
are not resolved to targets. A `CALLS` edge requires semantic evidence that
Riltra checked against the source. Traces are bounded plausible paths, not
executed call chains. Impact is `UNKNOWN` or marked `partial` unless the
available evidence establishes the affected set. Neither label means a change
is safe.

Index and source-binding freshness is reported as `fresh`, `stale`, or
`unknown`. A changed source makes its binding stale; Riltra does not rebind it
automatically. Knowledge records are revisioned, and explicit bindings point
to one record revision. A binding records a relationship, not whether the
record itself is true.

## Optional semantic knowledge search

The default-off `semantic-knowledge` build feature adds local vector search
over active knowledge records. It does not embed source code or change default
lexical search. It requires separately provisioned model and runtime assets;
when the feature, assets, or index are unavailable, search uses FTS5 and
reports the fallback reason. Official packages do not include this feature.
See the [local inference guide](../semantic-inference.md) for setup and its
limits.

## MCP and trust boundary

`riltra mcp` serves fourteen local stdio tools. Thirteen read repository,
index, code, and knowledge data. The remaining tool,
`riltra_code_semantic`, starts the language server configured by the
operator. That process may write a cache, run project tools, or reach the
network. The MCP handlers themselves do not create, migrate, or replace a
database: a missing index returns `no-index`, and an unsupported schema is
refused with `schema_outdated`.

See the [agent setup guide](agent-setup.md) for tool selection, output labels,
and client configuration. For the core and provider security details, read the
[security policy](../../SECURITY.md).

## Known limitations

- Without semantic evidence, `callers` matches names within related files,
  `trace` cannot follow a resolved target, and impact remains
  `UNKNOWN`/partial when evidence does not establish coverage.
- C++ semantic resolution is not available out of the box. A C++ project needs
  a compilation database and an operator-configured language server; the
  current C++ answers remain candidates where identity or coverage is
  unverified.
- CUDA is unsupported. Other unsupported file extensions are reported without
  parsing.
- `search --occurrences` is an exact-name census, not proof that a symbol is
  bound or unused.
- macOS CI covers the x64 runner pool; macOS ARM64 is not covered.

## Compatibility

The published `0.1.x` CLI exit codes, JSON key order, and MCP tool names are
covered by the compatibility notes in the [README](../../README.md#stability-pre-10).
The on-disk index schema is versioned; an unsupported schema is refused rather
than silently read as current.
