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
or network behavior. Neither provider is enabled by default. To see which
guided producers and language-server settings are present, see [semantic
provider setup](#semantic-provider-setup-read-only).

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

## Automatic index updates (opt-in)

`riltra watch --repo <name> [--interval-ms <n>] [--debounce-ms <n>] [--max-wait-ms <n>] [--max-builds <n>]` rebuilds the code index after source changes. It is opt-in and CLI only: there is no MCP tool for it, and nothing starts it automatically.

- It polls the repository. `--interval-ms` sets the poll interval (default `1000`, allowed `100` to `60000`). `--debounce-ms` sets how long changes must stay quiet before a build (default `500`, allowed `0` to `60000`).
- It rebuilds only the code index, through the same incremental index that `riltra index` runs. It never runs project builds, LSP servers, SCIP imports or semantic providers.

Locks:

- One watcher runs per index. A second `riltra watch` for the same repository exits `3` with `watch-busy:`. A running watcher does not block `riltra index`.
- One rebuild or import writer runs per index at a time. A contending write reports `index-busy:` and exits `3`; it does not wait. Schema migrations are serialized by SQLite, not by this lock, so the guarantee is one rebuild or import writer per index, not one SQLite writer.
- A root equal to or inside `RILTRA_HOME` is refused with `watch-root-overlaps-home:` and exit `3`.

What triggers a rebuild:

- a file is added, deleted or renamed, or a file changes length or modification time;
- an ignore or config change, such as an edit to `.riltraignore`;
- a git HEAD or branch change, even when no file changed.

Limits:

- An edit that keeps the same length and lands inside one filesystem mtime tick can be missed until the next change, a HEAD or branch change, or a manual `riltra index`.
- Each poll walks the repository, so its cost grows with the size of the repository.
- Freshness stays `stale` while the worktree is dirty, even right after a rebuild.
- After a failed build, watch waits for a new change before it builds again, and it polls more slowly (up to `60000` ms between polls).
- Ignored files and unsupported files do not trigger a rebuild, so the unsupported-file counts in the index summary can lag until the next rebuild.

CLI behaviour:

- `--max-builds <n>` (1 or more) stops after `n` successful builds and exits `0`. Failed builds do not count.
- `--max-wait-ms <n>` (`1000` to `600000`) caps how long continuous changes can delay a build. When it is not given, the cap is the larger of `10000` and the debounce. A value below `--debounce-ms` is a usage error (exit `2`).
- A repository that was never indexed, or whose index is stale, is built right after start.
- Each event is one single-line JSON object on stdout: `started`, `built`, `failed`, `busy` or `stopped`. A `busy` event means the writer lock was held; the change stays pending and is retried after another debounce period. Diagnostics go to stderr.
- Exit codes: `0` when `--max-builds` is reached, `2` for usage errors, `3` for a repository, data or storage error. If an event cannot be written (for example, stdout is closed), watch exits `3`.

## Selected-anchor packets

`packet` combines results for one to five anchors that you choose after search
or query. A position selector contains `path`, `line` and `col` (and may include
`symbol`); a name selector contains `target` and may narrow with `in` and
`kind`. Exact unique names resolve. Ambiguous, missing, no-index, and
unavailable anchors remain per-anchor outcomes; a duplicate selector remains a
separate outcome but can share one snippet. The packet never selects a
candidate for you.

The response carries one result envelope with the per-anchor contexts, explicit
knowledge bindings and source snippets that fit the `max_bytes` JSON-response
budget. Depth defaults to 3 (range 1–6), context limit to 20 (range 1–100), and
`max_bytes` to 32768 (range 8192–131072). The anchors array is at most 16384
normalized bytes in both transports; CLI JSON input also has a 16384-byte raw
cap. Snippets are capped at 40 LF-delimited lines and 4096 bytes each, and a
source file larger than the indexer's 1 MiB read cap is unavailable.

`available`, `truncated` and `unavailable` describe snippet delivery. A
`source-changed` result means the current source digest differs from the index;
reindex and select the anchor again before using a new context. A uniquely
identified stored span may still appear as stale metadata; ambiguous or missing
syntax identity is not guessed. `invalid-source-range` means a stored range does
not fit the verified UTF-8 source and its snippet and follow-up range are
withheld. Other unavailable reasons are reported per anchor/snippet. A digest
labelled `fnv1a-64` is a non-cryptographic change detector, not proof that
content is identical or safe.
When `follow_up.source` is present (it is null without a unique source range),
its digest covers the whole indexed file. Validate repository containment,
read and hash the full current file, compare the 16-digit lowercase hex digest,
then slice the half-open `requested_range` from that same buffer. Do not hash
only the range.
Exit 0 means a packet document was formed, even when some anchors are ambiguous,
not found, no-index or unavailable. Invalid input and a response whose required
metadata cannot fit `max_bytes` exit 2 without partial output; path or storage
failures remain errors. A packet is not a change-safety verdict.

Packet knowledge is limited to explicit bindings. `knowledge_status` and
`knowledge_generation` are reported per anchor; the knowledge database has its
own read generation and is not part of the pinned code snapshot. Packet anchors
always use `query: null` and `semantic: false` internally; packet does not run
lexical or semantic retrieval. Both the CLI command and MCP tool are read-only;
they do not create or migrate databases, and a missing code index returns a
`no-index` outcome.

## Structure and documentation facts

`riltra index` also stores three more kinds of syntax fact and a word index for
concept search. `context` reports the containment and heritage facts (see
[context fact sections](#context-fact-sections)). `query` ranks definitions by
the concept words, which include the doc-comment text (see [search,
relationships, and freshness](#search-relationships-and-freshness)); no command
prints the doc-comment text itself, and `scan` output is unchanged.

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

## Change checks (read-only)

`riltra checks --repo <name> [--scope unstaged|staged|all|compare] [--base <ref>] [--limit <n>]`
and the MCP tool `riltra_change_checks` (input `repo`, `scope`, `base`, `limit`)
print one JSON document of test candidates and coverage limits for a git change.
The scopes, the `--limit` default and maximum, and the refusals are those of
[changed definitions](#changed-definitions). The document is a suggestion. Riltra
runs no test and no project tool, so an empty `test_candidates` list does not mean
that no test is needed, and the project's required checks still apply. The CLI
and the MCP tool return the same JSON document; the CLI's stdout adds one trailing
newline.

Reads and writes. The index is opened once, read-only, in one transaction. A
missing index is reported as `no-index` and is not created. An older index schema
is reported as `schema-outdated` and is not migrated, and a newer one is reported as
`schema-ahead` and not read. The registry, every `.db` file and the repository stay unchanged.
The only writes are the temporary directory that `changes` uses for duplicate paths
and SQLite's `-wal` and `-shm` sidecars beside an existing index. Git runs only as
read-only plumbing. There is no network call, no provider, no language server, no
project tool and no test runner.

Exit codes. `0` prints the report, including for a missing index and an unsupported
schema. `2` is a usage error or an unknown `--base` ref. `3` is every refusal that
`changes` gives with exit 3, an unknown repository, and a storage read error, including
one inside a traversal (for example a failed read of a SCIP owner mapping, which is
never read as "no mapping"). MCP answers exit 2 as `invalid_input` and exit 3 as `store_error`.

Bases. Each reason keeps its own basis. A candidate's basis is its strongest
reason, in this order:

- `proven-edge`: a stored call or reference edge with proven evidence, reached from
  the changed definition in the index snapshot.
- `unproven-edge`: a stored edge reached through a path whose evidence is not proven.
- `name-candidate`: a syntax-only name match. It is recorded as written and is not
  resolved to a definition.
- `path-convention`: a file-name rule (below), from the changed file.

A quoted instruction is not a candidate. It has basis `explicit-instruction` and its
own state, described below.

File-name rule. For each changed file `dir/stem.ext` in a language riltra parses,
whether or not a definition in it changed, a snapshot file `p` is a candidate when
`p` is a test path (a file under a `test`, `tests`, `__tests__` or `spec` directory,
or a file named `test_*.py`, `*_test.go`, `*.test.ts`, `*.spec.ts` or `*Test.java`)
and its file name without extension is exactly one of `stem`, `test_stem`,
`stem_test`, `stem.test`, `stem.spec` or `stemTest` (case-sensitive). The reason's
source is the changed file, and its detail is `same-stem`. The rule works only on
file names. Without an index there is no file list, so it gives nothing, and the
`no-index` limit says so.

Repository instructions. Riltra reads `README.md`, `AGENTS.md`, `CLAUDE.md`,
`CONTRIBUTING.md`, `BUILDING.md` and `TESTING.md` at the root and in every ancestor
directory of a changed file, at most 20 files, each read up to 262,144 bytes. A
quoted line is a line inside a fenced code block that contains `test` as a whole
word, case-insensitively. A fence opens on a line that starts with three backticks
or three tildes, and the fence lines themselves are never quoted. Each quoted line
is trimmed and cut to 200 bytes on a character boundary. Quoted lines are data:
riltra never runs them, and a quoted line is not permission to run anything.

Traversal. For each changed definition that resolves to one indexed symbol, riltra
walks the stored graph from the definition's snapshot row to depth 3. The walk
does not look the definition up again by line and column, so a definition edited
after indexing is still traversed. Its freshness and verification labels come from
the same evaluation as `impact`. A traversed definition carries the uncertainty
`snapshot definition: resolved from the index, not from the live source`, so its
impact verdict stays `UNKNOWN`. A dependent file whose path is a test path becomes a
candidate reason. Any other dependent file is listed as a dependent reason of its
review area.

`dependent_count` counts what the traversed definitions reached. When definitions
were not traversed, because of a cap or the time limit, or when the graph is partial,
the count is a lower bound. The `definitions-not-analysed`, `traversal-time-budget` and
`partial-graph` limits say so.

Statuses. A changed definition is `analysed`, `added` (no index key yet),
`not-in-index`, `ambiguous-in-index`, `data-member`, `not-traversed` (beyond the
traversal cap, or no graph proof), `time-budget`, `anchor-unavailable` or
`index-unavailable`. A removed file's definitions are traversed from their snapshot
rows; a removed file is listed in `changed`, and the `index-stale` limit is set. A
changed test file is kept in the report, and it can be a candidate.

Coverage limits. `coverage_limits` lists the limits that apply, in this fixed order.
Each has a count and at most 20 paths, sorted, with the rest counted:

1. `no-index`, `schema-outdated`, `schema-ahead`, `no-snapshot`: no graph or file list was read.
2. `index-stale`: the index is older than the worktree; its reasons are listed.
3. `graph-proof-incompatible`: edges are withheld; only the file-name rule ran.
4. `no-semantic-import`: the snapshot has no semantic import, so no edge can be proven.
5. `partial-graph`: a traversal is partial or its verdict is `UNKNOWN`.
6. `configuration-changed`: a changed build, manifest or configuration file, or a file under `.github/`.
7. `file-not-analysed`: a changed file that riltra does not parse (unsupported, binary, too large, unreadable, a submodule, a symlink, unmerged or a parse error).
8. `hunks-outside-definitions`: a file has a hunk outside every definition.
9. `added-definitions`: added definitions have no indexed dependents yet.
10. `definitions-not-analysed`: a changed definition was not analysed or was not traversed.
11. `traversal-time-budget`: a traversal reached its 2,000 ms limit, or no traversal started
    after the 10,000 ms total. Each such definition is `time-budget` and contributes nothing.
12. `changes-truncated`: `changes` listed fewer files than it found.
13. `no-test-candidates`: the list of test candidates is empty. An empty list is not "no test is needed".

Bounds. The default is 50 changed files (`--limit` takes 1 to 1000). The report
lists at most 200 definitions, traverses at most 20 definitions, walks depth 3,
lists at most 50 test candidates with 5 reasons each, reads at most 20 instruction
files and quotes at most 20 lines, and lists at most 30 review areas, each with at
most 20 changed files and 10 dependent files, with at most 3 reasons each. Each
traversal has 2,000 ms, and once 10,000 ms of traversal time has passed no new
traversal starts; the definitions that were not started are `time-budget`. Every
dropped item is counted in `omissions` with its section, reason, and count (`null`
when riltra cannot know it). The JSON is at most 262,144 bytes. When it would be
larger, riltra drops quoted instructions first, then extra reasons, dependent files,
test candidates, listed definitions and changed files per area, and last the limit
paths, and counts each dropped item under `json_budget`.

Time limit. The output is deterministic: two runs on the same index revision,
worktree and git state are byte-identical, unless a traversal reaches the
2,000 ms limit or the 10,000 ms total in one run and not in the other. A timed-out
definition contributes nothing and is listed under `traversal-time-budget`, however
far it got.

Next actions. The document ends with fixed suggestions, each at most once. A stale or
missing index suggests `riltra index --repo <name>`. A graph proof that is
incompatible suggests `riltra semantic import` or `riltra index --full`. A missing
semantic import suggests `riltra semantic setup`. The last action has no command:
run the checks your project requires. riltra does not run tests.

Accepted limits. There is no import of coverage reports yet; a coverage import
would be a later extension. riltra keeps no runtime data about which tests exercise
which code. The file-name rule covers only the forms listed above. Inline Rust
`#[cfg(test)]` modules are not detected. The traversal depth is 3. An instruction
fence closes on the next line that starts with the same three-character marker, so
a line such as a backtick fence with a language name (for example three backticks
followed by `sh`) closes the block early. A
missing instruction file is skipped silently, and an unreadable one is counted in
`omissions` as `instructions` with reason `unreadable`.

## Area map (read-only)

`riltra map --repo <name> [--depth 1..4] [--format json|markdown]` and the MCP
tool `riltra_project_map` (input `repo`, optional `depth`, default 2) print one
area map of an indexed snapshot: directory areas with their entry points and
hubs, proven links between areas, separate name-match candidates, and up to three
bounded static flows from `main`. The JSON document is the same from the CLI and
MCP. Markdown is CLI only.

- **Reads and writes.** One report reads one database revision: the first read
  pins it, and a rebuild or semantic import that commits during the report is not
  mixed in. The report names that revision with `snapshot.nonce` and
  `snapshot.generation`, so two reports can be told apart. The map reads the code
  index through the read-only path and the git freshness check the index already
  runs. It creates and migrates nothing. A missing index answers `no-index`, and an
  older or newer schema is reported without migration. A storage read error makes
  `map` exit 3 (MCP `store_error`). SQLite can create `-wal` and `-shm` sidecars
  beside an existing index, as the other read tools do; `registry.json` and the
  database file are not changed. No network call, provider or project tool runs.
- **Areas.** A file belongs to the area made from the first `depth` components of
  its parent directory; a file at the root is in `.`. The `grouping` object states
  that areas are navigation hints from directory structure, not architecture
  boundaries.
- **Edges.** Proven edges are `CALLS` and `REFERENCES` edges with `Exact` precision
  and a known callee. Only proven edges become links, hub counts and flow paths.
  `POTENTIAL_CALL` rows are syntax-only: a callee name is matched to the snapshot's
  function and method definitions with that name, and gives a candidate link when
  every match is in one other area. Candidate links are name matches, never
  relations, and are never traversed. Matches in two or more areas are counted, not
  linked. `IMPORTS` rows are syntax-only and unresolved: they are counted per area
  and never linked.
- **Flows.** Entry points are definitions named `main`. A file with no `main` may
  still get a `conventional-path` entry point, but it gets no flow. Up to three
  `main` entries are traced three hops deep, as `riltra trace` does, and only paths
  whose every edge is proven are kept. Each flow keeps its trace's verification,
  freshness, freshness reasons and completeness, so a flow can be `ok` and `Stale`
  at once. A static path is not proof of execution. `trace_command` is a suggestion
  for you to run; riltra never runs it. A SCIP callee whose spans in one file are
  ambiguous is not listed as a flow definition or a hub.
- **Time limit.** A flow whose trace reaches its 2,000 ms deadline reports
  `time-budget` with no partial content. Two runs on the same revision are
  byte-identical unless a deadline is reached in one run and not in the other; then
  that flow's entry, its omission, and the next actions and byte-budget trimming
  that depend on it can differ.
- **Graph proof.** Before any stored relationship is read, the report checks that
  the database carries a current graph-proof token for its snapshot. Without one (an
  index built under older proof rules, or a token for another revision),
  `graph_proof` is `incompatible`. The report still lists areas, files, languages,
  definitions, entry points and imports, but withholds every link, candidate, hub,
  flow and graph counter: they are empty lists or `null`, never zero. The next
  action is to validate the graph with `riltra semantic import` or
  `riltra index --full --repo <name>`. riltra does not repair the token, and the map
  never writes it.
- **Completeness and bounds.** One report reads at most 50,000 edge rows.
  `completeness` says whether all edges and all proven relations were read; when
  they were not, links and hubs describe only what was read. `import_verification`
  counts every semantic import before any list cap, so a `Stale` import past the
  20-import list still appears there and triggers its next action. The named bounds
  are: 50 areas (the ones with most files), 3 entry points and 3 hubs per area, 100
  links, 100 candidate links, 3 flows of depth 3, and per flow 1,000 paths, 2,000 ms,
  10 area paths and 20 definitions; 20 imports listed; and 262,144 bytes of JSON.
  Anything dropped is counted in `omissions`, and a count riltra cannot know is
  `null`. Entry-point and hub omissions count only the areas kept after the area
  cap. When the JSON is still over the byte budget, items are removed in a fixed
  order, each counted as `json_budget`.
- **Accepted limits.** Grouping is by directory only, with no clustering. Flows
  need SCIP-backed edges, so a syntax-only repository has no links and no flows. A
  flow is at most three hops from `main`. A flow can reach its time limit, and then
  its status can differ between runs.

## Semantic provider setup (read-only)

`riltra semantic setup --repo <name> [--lsp-server <abs-path>]` prints one JSON
report on the semantic providers for a registered repository. It reads files,
the index, git metadata and `PATH`. It installs nothing and runs no provider or
language server. Its only subprocess is the read-only git check that
`riltra index status` also runs, and only for a repository with its own `.git`.
Its only file effect is that SQLite can create `-wal` and `-shm` sidecars beside
an existing index. An older index is reported as `schema_outdated`, not
migrated.

The report separates three things:

- **Index state.** `index.status` is `ready`, `no-index`, `schema_outdated` or
  `schema_ahead`. Freshness and its reasons are reported only for a ready index.
  `fresh` is reported only after the git check confirms it.
- **Scan completeness.** A directory that cannot be read, or that points outside
  the repository, makes `scan.complete` false. The language list and the
  manifests may then be incomplete.
- **Imports and providers.** Each SCIP import is listed with its verification.
  Each family present in the repository gets its capability values and the
  state of its producer and language server.

Languages are listed per family with these capability values:

| Family | Languages | SCIP import | Language server | Guided producer |
|---|---|---|---|---|
| typescript | typescript, tsx | verified-fixture | contract-only | scip-typescript |
| rust | rust | verified-fixture | contract-only | rust-analyzer |
| python | python | contract-only | contract-only | none |
| javascript | javascript | contract-only | contract-only | none |
| c | c | contract-only | contract-only | none |
| cpp | cpp | contract-only | contract-only | none |
| go | go | not-tried | not-tried | none |
| java | java | not-tried | not-tried | none |

`verified-fixture` means an import from that real producer was verified on a
Riltra fixture. `contract-only` means Riltra implements the transport but no
real provider was verified for this language. `not-tried` means no evidence
exists. TypeScript and Rust are the only languages with guided producers.

**Producers.** For a guided family, the report names the producer and the
command to run yourself from the repository root. Riltra does not run that
command or check its output.

- `scip-typescript` needs a `tsconfig.json` at the root and is run as
  `scip-typescript index`. Without `tsconfig.json`, the command is
  `scip-typescript index --infer-tsconfig`.
- `rust-analyzer` needs a `Cargo.toml` at the root and is run as
  `rust-analyzer scip .`. Without `Cargo.toml`, no command is offered.

After the producer writes `index.scip`, import it with the command the report
gives, `riltra semantic import --repo <name> --scip index.scip`. The import path
for artifacts you provide is unchanged.

For `scip-typescript`, Riltra first looks in the repository's `node_modules/.bin`,
then in each `PATH` directory in order. `rust-analyzer` is looked up on `PATH`
only. On Windows a producer is found only as a `.exe`, `.cmd` or `.bat`
file; npm's extensionless shell shim is not used, because PowerShell cannot run
it. On other systems the file name has no extension. The first regular file
wins and is never executed. `found` means only that a file with that name
exists. It does not show that the file runs or that it is the expected tool. A
producer that is not found is reported as `not-found`, with a step to install
it yourself; Riltra does not install providers.

**Language server.** The server path comes from `--lsp-server`, or else from the
`RILTRA_LSP_SERVER` environment variable; the report's `source` field says which
(`--lsp-server`, `RILTRA_LSP_SERVER` or `none`). An empty
`RILTRA_LSP_SERVER` counts as unset. An empty `--lsp-server` value is a usage
error. The path is classified from its file name without starting it. The file
name is taken without `.exe`, `.cmd` or `.bat`, and lowercased on Windows. The
state is one of:

- `not-configured`: no path is given.
- `not-absolute`: the path is not absolute.
- `not-found`: the path is not a regular file.
- `launch-incompatible`: the name is `typescript-language-server`. That server
  needs `--stdio`, and Riltra starts language servers with no arguments, so
  use the SCIP path for TypeScript.
- `configured`: the name is the server expected for the family (`rust-analyzer`
  for Rust).
- `mismatch`: the name is a known server for another family.
- `unrecognized`: any other name. Riltra cannot tell which language that server
  serves.

A configured language server answers position queries, and the references it
returns are locations, not call edges. The setup report does not change that.

**Next steps.** `next` lists the steps in a fixed order: build or rebuild the
code index, the partial-scan notice, the producer steps, re-running stale
imports, and the optional language-server and TypeScript notes. A step text
never contains a command; a command is in its own field, or `null` when there
is none. Commands are quoted for the shell in the report's `shell` field:
PowerShell on Windows, POSIX sh elsewhere. They are meant to be run from the
repository root, and the repository name is quoted.

Exit codes follow the CLI contract. An unknown repository, a moved root, or a
storage error exits with 3. A usage error exits with 2. Every other case,
including a missing or outdated index, exits with 0.

## Optional semantic knowledge search

The default-off `semantic-knowledge` build feature adds local vector search
over active knowledge records. It does not embed source code or change default
lexical search. It requires separately provisioned model and runtime assets;
when the feature, assets, or index are unavailable, search uses FTS5 and
reports the fallback reason. Official packages do not include this feature.
See the [local inference guide](../semantic-inference.md) for setup and its
limits.

## MCP and trust boundary

`riltra mcp` serves twenty local stdio tools. Nineteen read repository,
index, code, knowledge, project overview, area map, selected-anchor packet, and change-check data. The remaining tool,
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
- `riltra watch` polls the source; it does not use OS file events. An edit that
  keeps the same length and lands inside one mtime tick can be missed until the
  next change, a HEAD or branch change, or a manual `riltra index`. See
  [automatic index updates](#automatic-index-updates-opt-in).

## Compatibility

The published `0.1.x` CLI exit codes, JSON key order, and MCP tool names are
covered by the compatibility notes in the [README](../../README.md#stability-pre-10).
The on-disk index schema is versioned; an unsupported schema is refused rather
than silently read as current.
