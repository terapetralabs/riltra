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
- See which definitions a git change touches with `riltra changes`.
- Keep revisioned project notes and link them explicitly to source anchors.
- Give an AI coding agent the same repository and note queries through MCP.

Riltra shows when evidence is incomplete. Name-based call matches are
candidates, and an `UNKNOWN` or `partial` impact result is not proof that a
change is safe. See [capabilities and limits](docs/guide/capabilities.md).

## Available today

- Local repository registry, Git status, and health checks.
- A durable SQLite code index with symbol search and explicit freshness.
- A bounded relationship graph and revisioned knowledge store with explicit
  source bindings.
- Optional SCIP import and operator-configured language-server queries.
- Optional local vector search for knowledge records. It is off by default,
  requires separately provisioned assets, and falls back to lexical search
  when unavailable. It does not index source code; see the [local inference
  guide](docs/semantic-inference.md).

The [capabilities guide](docs/guide/capabilities.md) describes the supported
languages, evidence labels, provider boundaries, and known limits.

## Quick start

The [`0.1.0` pre-release](https://github.com/terapetralabs/riltra/releases/tag/v0.1.0)
provides native archives for Windows x64, Linux x64, and macOS Intel x64 with
SHA-256 checksums. See [release/INSTALL.md](release/INSTALL.md) for install and
uninstall instructions.

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

## MCP server (fourteen read-only tools, one provider-backed tool)

<a id="mcp-server-twelve-read-only-tools-one-provider-backed-tool"></a>

Run `riltra mcp` to start a local stdio MCP server with fifteen tools: fourteen
read-only tools for repository, code, and knowledge queries, plus
`riltra_code_semantic`, which starts the language server configured by the
operator. That server may write its own cache, run project tools, or access the
network. Riltra's MCP handlers do not create, migrate, or replace a database.
A missing index returns `no-index`; an unsupported schema is refused with
`schema_outdated`. Prepare repositories with the CLI before querying them.

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

Syntax-only graph results are candidates, not resolved relationships. Other
coverage limits and unsupported cases are listed in the [capabilities
guide](docs/guide/capabilities.md#known-limitations).

## Stability (pre-1.0)

Within `0.1.x`, CLI exit codes (`0` success, `2` usage error, `3` repository or
data error), JSON key order, and MCP tool names are stable. New JSON keys are
opt-in or appended; changes to existing keys are released in a minor version
and described in the release notes. The on-disk index schema is versioned: an
older schema is reported as stale or `schema_outdated` and rebuilt with
`riltra index`, never silently reused.

## Project status

Riltra is pre-release software; `0.1.0` is published as a pre-release, not a
stable release. Check the [release notes](https://github.com/terapetralabs/riltra/releases)
for the published version and its limits.

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
