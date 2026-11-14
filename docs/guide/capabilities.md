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
| C++ | `.cc`, `.cpp`, `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx`, `.inl` | Same, plus classes, structs, and data members. Riltra runs no preprocessor, so code that relies on macros can lose facts or report parse errors: a macro before a declaration (`EXPORT void f()`), or a macro call without a semicolon inside a class (`DECLARE_X(Foo)`), makes the parser report errors. `class EXPORT Name {...}` is read as a class, but only its member functions with bodies are recorded, as plain functions; data members and declarations are not. A class-scope macro call such as `DECLARE_X(Foo)` can also yield a spurious field fact | Configured language-server query (such as clangd); not verified on a real project |
| Java | `.java` | Classes, records, enums, interfaces, methods and constructors, imports, and method-call candidates attributed to their enclosing method or constructor (a call inside a lambda body stays unattributed; `new T(...)` and `this(...)`/`super(...)` delegation are not call candidates) | None; no SCIP or language-server evidence has been tried |
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

## Changed definitions

`riltra changes --repo <name> [--scope unstaged|staged|all|compare] [--base <ref>] [--limit <n>]`
and the MCP tool `riltra_code_changes` list the definitions a git change
touches. The scopes compare these sides: `unstaged` (index to worktree),
`staged` (HEAD to index), `all` (HEAD to worktree, the default), and `compare`
(`--base` to worktree; `--base` is required for `compare` and refused for every
other scope). Untracked files count as added in every scope except `staged`.
The default `--limit` is 50 files and the maximum is 1000; `total_files` counts
all changed files before the limit.

Refusals. Exit 2 for usage errors and for an unknown `--base` ref. Exit 3 when
the registered root is not the top level of a git work tree: the root must hold
its own `.git`, so a subdirectory of a repository, or an empty `.git` directory
inside a parent repository, is refused and never answered from the parent. Also
exit 3 when the repository has no HEAD commit, when git is missing or older than
2.31, or when the registered root moved or was retargeted (MCP answers with the
same refusal as the other repository tools).


Content. The parsed content is the raw blob or worktree bytes. No clean filter
runs, so the bytes are not converted back to what a filter would store. The
tracked-path hunks come from `git diff`, which keeps git's own text and eol
conversions. The duplicate-path identity and hunks come from a child process
that uses the repository's effective `core.autocrlf` and ignores `.gitattributes`
text and eol rules.


Statuses. The first matching check wins: `unsupported` with reason
`non-utf8-path` (the name is not UTF-8; it is shown lossily and never given to
git), `submodule` (mode 160000 on either side; no content is read), `symlink` (mode 120000 on either side; the target text is never parsed), `filter-managed` (see below), `unmerged`
(a merge conflict with no single version; `change` is `modified`),
`too-large` (over 1 MiB; riltra checks the size before it reads the blob, but git diff still reads tracked blobs for hunks), `unreadable` (not a regular
file, including a nested repository); then `binary` (a NUL byte in the first
8000 bytes); then `unsupported` with `language-unsupported` (only Python,
JavaScript, TypeScript and TSX, Rust, C, C++, Go and Java are parsed in this
release); then `parse-error`
(`syntax-error`); otherwise the change kind. A type change is reported as
`modified`. Submodule, symlink, unmerged and non-UTF-8 files have no hunks, and an empty
untracked file has no hunks. A conflicted path is one entry per path in every scope.

Symbols are `established` only for ordinary files. Other statuses carry no
definitions, and every hunk of the file is listed in `hunks_outside_definitions`
(binary files list none). `hunks_outside_definitions` is not capped: a file with
many changes lists all of its hunks, so the output grows with the change.

A hunk touches a definition when its new-side lines meet the definition's new
span, when a pure deletion lies strictly inside that span (after its first line
and before its last), or when its old-side lines meet the old span. A
definition is `modified` when it is touched on either side, `added` when a hunk
touches it, and `removed` when it is gone from the worktree (removed definitions
are always listed). Nested definitions are each listed when touched, and
duplicate names pair in source order. The header is the first line of a
definition, so `header_touched` only says that a hunk covers that line.

Language notes. A definition is matched by the kind and name the parser stores.
Java classes and records are `class`, interfaces and annotation types
`interface`, enums `type`, and methods, constructors and compact constructors
`method`. Go methods are `method` and are not nested under their receiver type.
A JavaScript or TypeScript function bound to a `const` is a `function`, and a
class field bound to a function is a `method`. C and C++ enums and typedefs are
`type`. In C++ a method defined inside its class body is a `function`, an
out-of-line definition keeps its qualified name (`Widget::reset`), and a
namespace has no entry of its own.

Limits of those notes. A span starts where the grammar's node starts: a Java
annotation above a method is part of the method, while a Go doc comment or a
C++ `template <...>` line is not part of the function it precedes. Because
`header_touched` looks at the first line only, a Java signature on the line
below an annotation is not the header. A C++ data member (`field`) is anchored
on its identifier, so changing the type on an earlier line of a multi-line
declaration does not touch the field (the enclosing class is touched instead).
Java fields are not extracted. A field never gets a caller count. Every `.h`
file is parsed as C++ and macros are not expanded, so a struct member in a `.h`
file is a `field`, and a header that does not parse as C++ is reported
`parse-error`.

A renamed file is looked up in the index under its old path, because the index
holds the old path until it is re-indexed. In the unstaged scope a staged rename shows as
the new path modified (index to worktree), so its callers are looked up under the
new path, which the index holds only after it is re-indexed (`not-in-index` until then); the all and compare scopes show it as `renamed` and look it up under the old path.

`inbound_edges` is the syntax-level inbound count the index holds for the
definition's name, kind and file (the selection `callers` makes), read from the
index without consulting the live source. It is the count as of the index: when
`index_freshness` is `stale`, the count is stale too. It is `null` with
`not-in-index`, `ambiguous-in-index`, `added`, `data-member` (a modified or
removed C++ field when the index is usable: a field is not called, so no count is
looked up; an added field says `added`, and without a usable index the field
carries the index reason below), `no-index` (no index), or
`index-unavailable` (the index is outdated, ahead, corrupt, or has no snapshot or
graph proof; the reason is in `index_freshness`). The structure is still
answered in every case.

Hardening. Every git call that reads repository content or state in `changes`,
including the index freshness check, runs with the same overrides: repository
filter drivers (clean, smudge, process) are emptied and marked not required, the
external diff and fsmonitor hook are emptied, the pager and optional locks are
off, and paths are literal. No repository filter, external diff or fsmonitor hook
runs during `changes`. Three calls are not hardened, and they run no repository
code: `git --version` (the version gate), the listing of filter driver names
(`git config --name-only --get-regexp`), and the work-tree check (`git rev-parse
--show-toplevel`, repository discovery only). The read of `core.autocrlf` (`git
config --get`) goes through the hardened runner. A filter-managed repository can
read as dirty or stale more often than `git status` would report it; that is the
conservative side. Other commands keep the posture they had before.

Filter-managed files. A path whose `filter` attribute has a value other than
`unspecified` or `unset` is `unsupported` with reason `filter-managed`. That
includes a name that no configured driver answers to: git would filter nothing in
that case, so this listing is conservative there. The path's worktree bytes are not
the bytes a driver would store, so nothing is parsed and no hunks or definitions
are listed. The attribute is read with `git check-attr`, which runs nothing. A clean
filter that is not idempotent can make git's own status disagree with this listing,
while index freshness reads stale.

Links in the repository. A path whose directories lead out of the repository through a
link or junction is never read or diffed. It is listed as unsupported with reason
`unreadable`, and it is listed as modified even when git itself would not list it, so the
listing does not depend on the outside content. Index freshness (git status) is global,
and git itself may follow such a directory: a registered repository whose tracked
directories are links is not supported.

The duplicate-path case (a path staged as deleted and re-created untracked, in the
all and compare scopes) is reconciled before the count and the limit. The two sides
are compared first with `git diff --no-index` on two temporary files written in a
fresh directory under the system temporary directory. Exit 0 means
identical, which gives no entry. The worktree side is read only through the same
confined reader as every other worktree read: a link or junction that leads out of
the repository is refused, so the entry is listed as unreadable whatever the outside
file holds. Files over 64 MiB are not proven identical: their entry is listed
(too-large) rather than omitted. Otherwise the hunks come from the same child, and only for files within the scan cap (1 MiB): a larger differing file is listed as too-large without hunks. The
temporary directory is removed when the command ends; the removal is best effort, so
a stray directory can be left behind if removal fails. The temporary files and the
index's SQLite sidecar files are the only writes this feature makes. A read-only
open can leave the `-shm` and `-wal` sidecars beside the index; the existing MCP
read tools do the same, so this is not new to `changes`. The repository is never
written.

The output is the structure of a change, not an impact verdict.

## Optional semantic knowledge search

The default-off `semantic-knowledge` build feature adds local vector search
over active knowledge records. It does not embed source code or change default
lexical search. It requires separately provisioned model and runtime assets;
when the feature, assets, or index are unavailable, search uses FTS5 and
reports the fallback reason. Official packages do not include this feature.
See the [local inference guide](../semantic-inference.md) for setup and its
limits.

## MCP and trust boundary

`riltra mcp` serves fifteen local stdio tools. Fourteen read repository,
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
