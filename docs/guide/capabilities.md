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

`query --repo <name> --text <text> [--limit <n>]` (MCP `riltra_code_query`) is
lexical concept search: it ranks indexed definitions for a free text by the
words of their names, paths and doc comments (BM25 over the concept index). It
is lexical only: it does not follow call edges, does not match meaning, and
does not print its score. Terms are the distinct lowercase words of the text
with at least three letters, at most 64, with no stoplist and no stemming. Test
paths rank after production paths unless a term names tests. Each result is a
symbol with `path`, `line`, `name`, `kind` and the terms that matched it. The
default limit is 20 and the maximum is 1000; the text is at most 16384 bytes.
A text with no term answers `no-extractable-term`, and a text that matches no
indexed definition answers `no-match`; neither proves that a concept is absent.
With `--expand` (MCP `expand: true`) the answer adds an `expansion` object: one hop along proven `CALLS` edges from the first five results, at most five added rows per seed and 25 in total, each labelled `Unverified`. It needs a SCIP import and is unavailable while the source is not fresh; see the [agent setup guide](agent-setup.md#opt-in-expansion).

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

## Structure and documentation facts

`riltra index` also stores three more kinds of syntax fact and a word index for
concept search. `context` reports the containment and heritage facts (see
[context fact sections](#context-fact-sections)); no other command or MCP tool
reports the doc-comment and concept-word facts yet, and `scan` output is unchanged.

- **Heritage**: the supertypes a declaration names. Python base classes (not
  keyword arguments or `*splat` bases), JavaScript `extends`, TypeScript and
  Java `extends` and `implements`, a Rust `impl Trait for Type`, C++ base
  classes, and Go embedded struct fields. Each row keeps the supertype as
  written and a normalised name used only as a lookup key (`Base<T>` gives
  `Base`); nothing is resolved, so two types with the same name are not told
  apart. A base that is not a name (a call, an index such as `bases[0]`, a
  number, a C++ pack `Bs...`) keeps its text and has no lookup name. Rust supertraits (`trait A: B`), Go interface embedding and class
  expressions are not recorded.
- **Containment**: the nearest enclosing class, struct, interface, enum,
  record, Rust `impl`, `trait` or `mod`, C++ namespace or TypeScript namespace
  of each definition. A definition nested in a function is attached to the
  container around that function. Go and C record none (a C macro-headed
  struct is the exception).
- **Doc comments**: the comment block directly above a definition (through
  decorators, `export`, `template <...>` and Rust attributes), and a Python
  docstring. A comment that follows code on its own line, or that is
  separated by a blank line, is not attached; a licence header with no blank
  line before the first definition is. Text is kept as written, cut at 4096
  bytes.
- **Concept words**: each symbol's name split into words (`parseHTTPResponse`
  gives `parse`, `http`, `response`), its path split the same way, and its doc
  text, in one full-text table.

The index schema moved to version 6 for these. After upgrading, code-index
reads answer `schema_outdated` until `riltra index` is run; the knowledge store
is separate and is not affected.

## Context fact sections

`context` (by position or `--target`) always appends four sections, read on the
same pinned snapshot as its graph. There is no flag: the byte budget bounds the
cost. `--limit` applies to each list separately, and when `--max-bytes` is too
small, rows are dropped in this order: imported_by, imports, subtypes,
implementations, supertypes, members, callees, then ancestors outermost first.
Every section has a `label`, a `status` (`ok`, `none-recorded`, `truncated` or
`unavailable`), a `reason`, and per-list `total`, `returned`, `omitted_by_limit`
and `omitted_by_budget`.

- **containment** (label `Syntax`): `ancestors` are the enclosing scopes of the
  anchor, innermost first, at most 16 read. A deeper chain sets
  `truncated_depth` and `total: null`. `members` are the direct children. A
  container's `line` comes from the live file only when those bytes still match
  the indexed digest; otherwise it is `null`. `symbol: false` only says that no
  symbol row exists at that container's range.
- **heritage** (label `Heuristic`; names are lookup keys, nothing is resolved):
  `supertypes` are the bases declared at the anchor. `implementations` (a
  `basis` of `name-match`) are rows naming the anchor's name from another
  declaration, for example a Rust `impl Display for Foo` for `struct Foo`.
  `subtypes` (also `name-match`) are rows in any file whose base has the
  anchor's name. `same_name_definitions` counts definitions with that name, and
  same-named types are never merged. The two name lookups run only when the
  anchor is a type (class, struct, interface, type, enum or trait, as stored);
  for any other anchor the reason is `anchor-not-a-type`. Supertype text is cut
  at 512 bytes (`text_truncated`).
- **imports** (label `Syntax`, file level): `imports` are the files the anchor's
  file imports, and `imported_by` the files that import it, each with the line of
  the import statement. An import resolves only when its text names an indexed
  file: Python module paths, literal relative JavaScript and TypeScript imports,
  C and C++ `#include "..."`, and Rust `use crate::`. Other forms, and external
  modules, are counted in `unresolved` and never listed as files.
  The match is a text match of each import statement against indexed file
  paths, not the `IMPORTS` edges (those carry no target). A C or C++ include
  matches only a file of exactly the name it writes; the JavaScript and
  TypeScript suffixes are never tried for it. A link is a lead to open, never a
  resolution.
- **callees**: the same edges, with the same fields, as `riltra callees` for
  this anchor. `label` is the anchor's precision. `POTENTIAL_CALL` stays
  `POTENTIAL_CALL`.

Statuses: `none-recorded` means every list is empty. Its reason is
`syntax-facts-none-recorded` (containment and heritage), `anchor-not-a-type` or
`empty-result-not-coverage-proof` (imports and callees), so an empty list is
never a proof of absence. `--target` is resolved by name on its own read before the snapshot is
pinned (as for the other by-name commands), so a reindex between the two reads
can pair the position with a newer snapshot; the sections themselves are all
read on the one pinned snapshot. `unavailable` is only an identity problem: the anchor
has no single syntax definition (`no-syntax-identity`, or
`ambiguous-syntax-identity` with `candidates`), and the containment and heritage
sections then say so while the graph and callees still answer. A missing index,
an outdated schema and every other failure stay command errors, with no sections.

Limits: Rust supertraits, Go interface embedding, class expressions and
`declare` or `extern "C"` wrappers are not recorded. A Rust impl's name is its
type text as written, so `impl<T> Display for Wrapper<T>` is stored as
`Wrapper<T>` and does not match a `Wrapper` anchor. A semantic (SCIP) anchor is
mapped to its enclosing syntax definition by position, never by its occurrence
span.

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

`riltra mcp` serves sixteen local stdio tools. Fifteen read repository,
index, code, and knowledge data. The remaining tool,
`riltra_code_semantic`, starts the language server configured by the
operator. That process may write a cache, run project tools, or reach the
network. The MCP handlers themselves do not create, migrate, or replace a
database: a missing index returns `no-index`, and an unsupported schema is
refused with `schema_outdated`.

`riltra agent-block` prints the agent instruction block as plain Markdown text, not JSON. It reads no registry or index and writes no file.

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
