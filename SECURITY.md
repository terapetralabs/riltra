# Security Policy

Riltra is local-first: its core performs no source uploads, no telemetry, and no
network calls. Repository inspection uses read-only Git plumbing and never
executes a repository's code.

Optional semantic providers are the exception. They are separate programs that
you configure explicitly, and Riltra never starts one by default:

- **SCIP import** reads an index file that you produce. The program that
  produced it is not part of Riltra; some indexers run a project's build
  scripts or procedural macros, so produce indexes only from source you trust.
- **Language server** (`semantic query --lsp-server`, and the MCP tool
  `riltra_code_semantic` through `RILTRA_LSP_SERVER`) starts the executable you
  name by absolute path. That server reads the repository, may write its own
  index cache, and may run project tools or access the network. While that tool
  is configured, the MCP server is not read-only as a whole. Riltra itself makes
  no network call through either path.

## Supported versions

Riltra is pre-1.0. `0.2.1` is the current release (`0.1.0` was the first
pre-release). Security fixes land on `master` and ship with the next release;
only the latest release is supported.

## Reporting a vulnerability

Use the repository host's **private vulnerability reporting** feature for this
repo. Do not open a public issue for unpatched security bugs.

[Report a vulnerability privately](https://github.com/terapetralabs/riltra/security/advisories/new).
Do not attach private source code, credentials, or unredacted logs to a public
issue or pull request.

Include: affected version/commit, OS, reproduction steps, and impact.
We will acknowledge within 7 days and publish a fix + advisory once resolved.

For ordinary bugs and feature requests, use the public issue forms linked from
the repository's Issues page. The bug form asks for diagnostic details; remove
secrets, private source, and identifying local paths before sharing logs or
examples.

## Scope notes

- The core makes no network calls. The opt-in providers above are outside that
  statement and run with your permissions.
- The repository registry confines every query to canonical, registered roots;
  moved or retargeted roots are rejected, and unreadable Git metadata is
  reported as invalid rather than guessed.

## Repository history

Early commits contain machine-local paths. The history is not rewritten:
documents under `docs/` cite commits by hash as evidence, and a rewrite would
change every hash. A secret and local-path scan (`security/scan.sh`) runs in CI
on every push and pull request. If you believe the history exposes a
credential, report it through the private channel above.
