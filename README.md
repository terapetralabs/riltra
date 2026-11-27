# Riltra

[![CI](https://github.com/terapetralabs/riltra/actions/workflows/ci.yml/badge.svg)](https://github.com/terapetralabs/riltra/actions)

Riltra is a local code map and project notebook for developers and AI coding
agents. It indexes source code on your machine, helps you search symbols and
follow possible code relationships, and links project notes to the code they
describe. Use it from the CLI or connect an agent through its local MCP server.

The default core makes no network calls, uploads no source, and sends no
telemetry. It does not run project code while scanning. A configured language
server is a separate program that may run project tools or access the network.
Riltra is free software under the Apache License 2.0.

Start with the [quick start](#quick-start), or see the [documentation index](docs/README.md).

## What Riltra helps you do

- Find indexed definitions, imports, and call-site candidates.
- Inspect possible callers, callees, paths, and change impact with evidence and
  freshness labels.
- See which definitions a git change touches with `riltra changes`, and which test
  candidates and coverage limits a change has with `riltra checks`.
- Keep revisioned project notes and link them explicitly to source anchors.
- Start with a guided project overview and definition packet using `riltra start` or give an AI coding agent the same repository and note queries through MCP.

Riltra shows when evidence is incomplete. Name-based call matches are
candidates, and an `UNKNOWN` or `partial` impact result is not proof that a
change is safe. See [capabilities and limits](docs/guide/capabilities.md).

## Available today

- Local repository registry, Git status, and health checks.
- A durable SQLite code index with symbol search and explicit freshness.
- A bounded relationship graph and revisioned knowledge store with explicit
  source bindings.
- Selected-anchor context packets with bounded source snippets; choose the
  anchors explicitly and verify source before relying on it. See the [agent
  setup guide](docs/guide/agent-setup.md#selected-anchor-packets).
- Optional SCIP import and operator-configured language-server queries.
- `riltra semantic setup --repo <name>` reports, read-only, the guided SCIP
  producers and language-server settings for a repository, with the commands
  for you to run yourself. Riltra installs and runs no provider. See the
  [capabilities guide](docs/guide/capabilities.md#semantic-provider-setup-read-only).
- Opt-in `riltra watch` (CLI only) rebuilds the code index after source changes.
  See the [capabilities guide](docs/guide/capabilities.md#automatic-index-updates-opt-in).
- `riltra agent-block` prints a Markdown instruction block for your agent's
  instruction file. It is plain text, not JSON, and it writes nothing.
- `riltra map --repo <name>` prints a read-only area map of the indexed snapshot:
  directory areas, entry points, hubs, proven links between areas, name-match
  candidates and bounded static flows, as JSON or Markdown. It creates and
  migrates nothing. See the [capabilities guide](docs/guide/capabilities.md#area-map-read-only).
- `riltra docs scan --repo <name>` reads local Markdown documents and lists their
  sections, their explicit links to indexed definitions, and the records that cite
  them, as one read-only JSON document. `riltra docs ingest` (CLI only, with your
  approval) records the proposals you review. See the [capabilities
  guide](docs/guide/capabilities.md#documents-linked-to-code).
- Optional local vector search for knowledge records. It is off by default,
  requires separately provisioned assets, and falls back to lexical search
  when unavailable. It does not index source code; see the [local inference
  guide](docs/semantic-inference.md).

The [capabilities guide](docs/guide/capabilities.md) describes the supported
languages, evidence labels, provider boundaries, and known limits.

## Quick start

The [`0.3.0` release](https://github.com/terapetralabs/riltra/releases/tag/v0.3.0)
provides native archives for Windows x64, Linux x64, and macOS Intel x64 with
SHA-256 checksums. The default package does not include the optional
semantic-knowledge feature. See [release/INSTALL.md](release/INSTALL.md) for install and
uninstall instructions. The earlier `0.1.0` pre-release is described in the
[changelog](CHANGELOG.md).

To install from source, use a stable Rust toolchain:

```sh
git clone https://github.com/terapetralabs/riltra.git
cd riltra
cargo install --path crates/cli --locked
```

Cargo installs `riltra` in its user binary directory. Add that directory to
`PATH` so your terminal and coding agent can find the command. To use the
checkout without installing, replace `riltra` below with
`cargo run -q -p riltra --`.

```sh
riltra init --repo . --name my-project
riltra index --repo my-project
riltra index status --repo my-project
riltra search --repo my-project --symbol main --limit 10
```

Set `RILTRA_HOME` to choose a different location for the registry and indexes.
Run `riltra --help` for the full command list. Git is needed for repository
fingerprints; without it, Riltra reports that status as unavailable instead of
guessing.

## MCP server (twenty-one read-only tools, one provider-backed tool)

<a id="mcp-server-twelve-read-only-tools-one-provider-backed-tool"></a>
<a id="mcp-server-sixteen-read-only-tools-one-provider-backed-tool"></a>
<a id="mcp-server-eighteen-read-only-tools-one-provider-backed-tool"></a>
<a id="mcp-server-nineteen-read-only-tools-one-provider-backed-tool"></a>
<a id="mcp-server-twenty-read-only-tools-one-provider-backed-tool"></a>

Run `riltra mcp` to start a local stdio MCP server with twenty-two tools: twenty-one
read-only tools for repository, code, text search in files, knowledge, project overview, the area map, selected-anchor packets, change checks, and documents linked to code, plus
`riltra_code_semantic`, which starts the language server configured by the
operator. That server may write its own cache, run project tools, or access the
network. Riltra's MCP handlers do not create, migrate, or replace a database.
A missing index returns `no-index`; an unsupported schema is refused with
`schema_outdated`. Prepare repositories with the CLI before querying them.

By default `riltra mcp` lists nine core tools; `riltra mcp --all-tools` lists
all twenty-two. A client that lets the model call only listed tools needs
`--all-tools` in its MCP configuration to reach the rest; calling by name helps
only a client that calls by name directly.

`riltra grep --repo <name> --pattern <text> [--regex] [--ignore-case] [--path <prefix>] [--limit <n>]`
(MCP `riltra_code_grep`) searches the repository's text files, including
languages without a parser, gives each hit its enclosing indexed definition, and
gives companion files (`companions`) with their evidence labels for the first
five files of the answer only (at most three per kind; test files excluded).

#### Compact text answers (opt-in)

`riltra mcp --compact` answers a successful `riltra_code_grep` with one block of
plain text and **no `structuredContent`**, so the client shows the model that
text instead of the JSON document. Errors stay structured, every other tool and
the CLI are unchanged, and without the flag nothing changes. A client or script
that reads `structuredContent` from `riltra_code_grep` must not use the flag.

One exception: escaping can make the text longer than the JSON, for source with
many C1 control characters or Unicode line separators. When the text would
exceed the 64 KiB response budget, that call is answered with the usual JSON
document and `structuredContent`; nothing is cut to make the text fit. A reader
of the answer should therefore tell the two forms apart: the text starts with
`grep `, the document with `{`.

```
grep repo=app mode=literal pattern=helper
snapshot id=1 freshness=fresh reasons=[]
totals files_scanned=2 files_skipped_binary=0 files_skipped_large=0 files_skipped_unreadable=0 dirs_unreadable=0 dirs_escaped=0 files_matched=2 hits=2
truncated=null caller_gaps=[]
file lib.py hit_count=1
 in function helper line=1
  1:5: def helper():
file note.txt hit_count=1
 no-definition in_reason=not-indexed
  1:6: call helper() later
```

The answer holds the same files, hits, companions with their `why`, `basis` and
`via`, snapshot freshness and reasons, totals, `truncated`, `caller_gaps` and
`empty` as the JSON document, with two deliberate omissions:

- `totals.bytes_read`, a cost figure for the search;
- `in_reason` when it is `indexed`, which is what a hit group without it means.

A value is bare when it holds only letters, digits and `_-./+@~`, and a JSON
string otherwise. A source line follows `line:col: ` verbatim, unless it holds a
control or line-separator character other than a tab; then it follows
`line:col= ` as a JSON string. A source line therefore cannot pose as a label.

For tool selection, result labels, and troubleshooting, see the [agent setup
guide](docs/guide/agent-setup.md).

### Connect a coding agent

Install Riltra first, then register the same local stdio command with your
client. These are user-level examples.

#### Codex

```sh
codex mcp add riltra -- riltra mcp
codex mcp list
```

#### Claude Code

```sh
claude mcp add --scope user --transport stdio riltra -- riltra mcp
claude mcp get riltra
```

#### OpenCode

Merge this into the `mcp.servers` object in the user configuration file
 (`%USERPROFILE%\.config\opencode\opencode.jsonc` on Windows or
`~/.config/opencode/opencode.jsonc` on Linux and macOS):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "riltra": {
        "type": "local",
        "command": ["riltra", "mcp"]
      }
    }
  }
}
```

Restart the client and check that its Riltra tools are listed. If it cannot
find `riltra`, add Cargo's binary directory or the archive install directory
to `PATH`, or configure the full path to the executable.

## Language support

See the [language support matrix](docs/guide/capabilities.md#language-support).
Unsupported extensions are reported as unsupported; Riltra does not guess a
parser.

## Known limitations

Syntax-only graph results are candidates, not resolved relationships. A SCIP
symbol that names more than one definition proves nothing: its calls stay
candidates (`ambiguous:N`). An import made before this check existed proves
nothing either (`identity-unchecked`) until you run `riltra index` and then
`riltra semantic import` again. Other
coverage limits and unsupported cases are listed in the [capabilities
guide](docs/guide/capabilities.md#known-limitations).

## Stability (pre-1.0)

Within `0.3.x`, CLI exit codes (`0` success, `2` usage error, `3` repository or
data error), JSON key order, and MCP tool names are stable. New JSON keys are
opt-in or appended; changes to existing keys are released in a minor version
and described in the release notes. The on-disk index schema is versioned: an
older schema is reported as stale or `schema_outdated` and rebuilt with
`riltra index`, never silently reused. Since `0.3.0`, `riltra mcp` lists nine
core tools by default and `riltra mcp --all-tools` lists all of them; no
existing tool was renamed or removed. These are compatibility commitments for the `0.3.x` line
only. They are not API guarantees for a 1.0 release, and this
release makes no claim about measured results on real projects.

Platform support for the `0.3.0` archives is Windows x64, Linux x64, and macOS
Intel x64. The optional semantic-knowledge feature is not part of the default
package.

## Project status

Riltra is pre-1.0 software. `0.3.0` is a release, not a pre-release. `0.1.0`
was published as a pre-release. Check the [release notes](https://github.com/terapetralabs/riltra/releases)
for the published versions and their limits.

## Documentation

- [Documentation index](docs/README.md)
- [Capabilities and known limits](docs/guide/capabilities.md)
- [Agent setup guide](docs/guide/agent-setup.md)
- [Optional local semantic inference](docs/semantic-inference.md)
- [Install from a release archive](release/INSTALL.md)
- [Public project status](docs/public-status.md)
- [Changelog](CHANGELOG.md)
- [Security policy](SECURITY.md)
- [Contributing](CONTRIBUTING.md)

## Contributing

Contributions are welcome under the Apache License 2.0 (inbound = outbound).
Every commit needs a `Signed-off-by` line (`git commit -s`). See
[CONTRIBUTING.md](CONTRIBUTING.md) for the contributor workflow.

## Reporting a problem or idea

Use the repository's issue forms for bugs and feature requests. Share only
sanitized diagnostics: remove credentials, private source, and identifying
local paths. Report security vulnerabilities through the [private reporting
channel](SECURITY.md#reporting-a-vulnerability), not a public issue.

## Security

Riltra's core is local-first: it uploads nothing, phones home to nothing, and
does not execute repository code while scanning. SCIP import reads an index you
provide; the indexer that produced it is outside Riltra. A configured language
server is also outside the core trust boundary and may run project tools or
access the network. Read the [security policy](SECURITY.md) before enabling a
provider.

## License

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
