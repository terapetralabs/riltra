# Security scans (secrets + local machine paths)

## Dependency vulnerabilities

The CI `dependency-audit` job runs on pushes and pull requests. It installs
`cargo-audit` 0.22.2 with its locked dependencies and checks the committed
`Cargo.lock` against the current RustSec advisory database. Vulnerabilities
and scanner/database failures fail the job; informational warnings remain
visible in its log. No advisories are ignored automatically.

Run locally with `cargo install cargo-audit --version 0.22.2 --locked`, then
`cargo audit --file Cargo.lock`. This CI tool accesses the network to install
and fetch advisories; it does not add network access to Riltra itself.
This detects known dependency advisories, not all vulnerabilities in Riltra's
own code. Keep Gitleaks and correctness/security review as complementary checks.
The new job must be made a required check in repository settings to block merges.

Pinned scanner: **gitleaks 8.30.1** (MIT, github.com/gitleaks/gitleaks), plus
three custom rules in `.gitleaks.toml` for local machine paths: Windows
user-profile paths, unix/macOS home directories, and absolute workspace paths
outside user profiles (Windows drive letters in backslash, forward-slash and
JSON-escaped forms, plus UNC shares). The bundled default credential rules
stay enabled.

## What runs where

| Trigger | Command | Coverage |
| --- | --- | --- |
| pre-commit hook | `security/scan.sh staged` | staged changes |
| pre-push hook | `security/scan.sh outgoing` | commits being pushed + pushed tip tree |
| CI `security` job (push/PR) | `security/scan.sh range <before/base>..<head>` (new branch: `security/scan.sh new-branch <default>` = merge-base..HEAD) + `security/scan.sh tree` | commits introduced by this push/branch + whole committed tree |
| manual | `security/scan.sh tree \| range A..B \| new-branch <default> \| dir <path>` | as requested |

Exit codes: `0` clean, `1` findings, `3` scanner unavailable (fail-closed),
`4` usage, config or preflight error (an unavailable merge-base fails closed).
Findings are printed redacted; secret values are never echoed.

Environment: `GITLEAKS_BIN` (scanner binary; default `gitleaks` from `PATH`),
`GITLEAKS_CONFIG` (default: this repository's `.gitleaks.toml`).

## Install the pinned scanner

Download the v8.30.1 asset for the platform and verify it against the
published `gitleaks_8.30.1_checksums.txt` from the release page, e.g. Linux
x64:

    curl -sSfL -o /tmp/gitleaks.tar.gz \
      https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_linux_x64.tar.gz
    echo "551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb  /tmp/gitleaks.tar.gz" | sha256sum -c -
    tar -xzf /tmp/gitleaks.tar.gz -C "$HOME/.local/bin" gitleaks

On Windows unpack `gitleaks_8.30.1_windows_x64.zip` from the same release and
point `GITLEAKS_BIN` at `gitleaks.exe` from Git Bash, e.g.
`GITLEAKS_BIN=/c/tools/gitleaks.exe`.

## Enable the local hooks (once per clone)

    git config core.hooksPath .githooks

Hooks are a local guard, not enforcement: they can be bypassed with
`git commit --no-verify` / `git push --no-verify`. The CI `security` job is
the backstop; configuring it as a required check on the default branch is a
repository-settings action for the owner.

## Tests and allowlists

    bash security/scan-tests.sh

The suite copies synthetic fixtures (`security/testdata/`) to temporary
directories and checks: blocked fixtures (credential, user-profile, drive and
UNC path shapes; slash and JSON-escaped variants), allowed fixtures (relative
paths, placeholders, system/runner paths), secret redaction, fail-closed
behavior when the scanner is missing, ignored-but-committed files, staged
changes, outgoing commits, revision ranges, new-branch merge-base scanning
(including missing ref/merge-base fail-closed cases), fingerprint ignores (the
exact finding is suppressed, similar content and paths are not), per-file
allowlists (a new file in `security/testdata/` is still detected), and a
clean-tree check of this repository.

Allowlists in `.gitleaks.toml` are intentionally narrow and documented inline:
the verified blocker fixtures (listed one file at a time, never the directory),
CI runner paths (`runner` / `runneradmin`, `D:\a\...`, runner-generated and not
personal), OS-owned Windows directories, verified synthetic path literals in
unit tests, and two reviewed false positives (40-hex commit SHAs matched by the
`sourcegraph-access-token` heuristic, and SHA-256 integrity constants matched
by `generic-api-key`). No blanket allowlists (directories, `*.log`, `*.json`)
are used. The two superseded synthetic findings from the earlier fixture
revision remain pinned by fingerprint in `.gitleaksignore`.
