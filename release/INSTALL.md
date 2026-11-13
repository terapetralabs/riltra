# Installing Riltra

Riltra ships as one native executable. There is no service and no background
process, and the binary makes no network call. The installer runs at user level:
no administrative rights, no PATH change.

## Install from a release

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

Linux or macOS:

```sh
sh install.sh
```

The scripts download the archive for this platform and its `.sha256` file from
the GitHub release, verify the checksum, and then install. To use a local
archive instead, pass it explicitly:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -ArchivePath .\riltra-0.1.0-windows-x64.zip
```

```sh
sh install.sh --archive ./riltra-0.1.0-linux-x64.tar.gz
```

The archive and its `.sha256` must sit together. A missing, malformed, or
mismatched checksum stops the install before anything is extracted. Unsupported
platforms and ARM64 are refused with a non-zero exit.

## Install locations

| Platform | Command | Payload and documents |
|---|---|---|
| Windows x64 | `%LOCALAPPDATA%\Programs\Riltra\bin\riltra.exe` | Same directory |
| Linux x64 | `$HOME/.local/bin/riltra` (symlink) | `$HOME/.local/share/riltra/` |
| macOS Intel x64 | `$HOME/.local/bin/riltra` (symlink) | `$HOME/.local/share/riltra/` |

The installer prints the directory to add to `PATH`; it does not edit a shell
profile, the registry, or a system-wide path. On Linux and macOS the shared
`~/.local/bin` directory receives only the `riltra` symlink; the notices stay in
the private payload directory.

## Reinstall

Run the same installer again. It replaces only files recorded in the Riltra
manifest and refuses to overwrite an unrelated `riltra` file or symlink.
The registry and indexes under `RILTRA_HOME` are separate from the installed
payload and are not removed by reinstalling.

## Check the version and update

Check the installed version with `riltra --version`, then review the [published
releases](https://github.com/terapetralabs/riltra/releases) and their notes for
the version you want. To update an archive install, run the same installer
again with that version selected. The scripts currently default to `0.1.0`;
pass the version listed on the release page without the leading `v` when
selecting another release (replace `0.1.0` with the version you selected):

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Version 0.1.0
```

```sh
sh install.sh --version 0.1.0
```

The installers verify the archive's adjacent SHA-256 file and replace the
installed payload. If you installed from a local archive, use the matching
`-ArchivePath` or `--archive` option again instead. For a Cargo source install,
rebuild and reinstall from the source revision you want using the instructions
in the [README](../README.md#quick-start); the release archive installer does
not manage it.

After updating, check that your MCP client configuration points to the updated
executable and restart the client. Check `riltra --version` and the client's
`riltra_health` result. Follow the release notes if an index rebuild is needed.

## Removal

- Windows: delete `%LOCALAPPDATA%\Programs\Riltra`.
- Linux and macOS: delete the `riltra` symlink in `~/.local/bin` (only after
  checking that it points into `~/.local/share/riltra`) and delete
  `~/.local/share/riltra`.

## First use

```text
riltra init --repo <path-to-a-repository> --name myrepo
riltra index --repo myrepo
riltra search --repo myrepo --symbol <name>
riltra search --repo myrepo --symbol <field-name> --occurrences
riltra callers --repo myrepo --path <file> --line <n> --col <n>
```

Riltra keeps its registry and its databases under `RILTRA_HOME` (by default
`%APPDATA%\riltra` on Windows and `~/.config/riltra` elsewhere), never inside
the repository it indexes. Point `RILTRA_HOME` at a scratch directory to try it
without touching your own registry.

## Using it from an MCP client such as OpenCode

`riltra mcp` serves fifteen tools over stdio (fourteen read-only ones and the
provider-backed `riltra_code_semantic`, which stays off until an operator sets
`RILTRA_LSP_SERVER` to a language server's absolute path). An OpenCode
configuration entry looks like:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "riltra": {
        "type": "local",
        "command": ["<absolute path to riltra>", "mcp"],
        "environment": { "RILTRA_HOME": "<absolute path to a Riltra home>" }
      }
    }
  }
}
```

## What this build is, and is not

A pre-release candidate for local, evidence-first code intelligence. Answers
are source-grounded and labelled: syntax-only relations are
`POTENTIAL_CALL` candidates, never proven calls, and an impact answer is
`UNKNOWN` unless a semantic provider backs it. C++ semantic queries require an
operator-configured language server; results depend on its project index and
compilation database, and references are not call edges. CUDA is not supported.
The archive carries `NOTICE`, `LICENSE`,
`THIRD_PARTY_LICENSES.txt` and `sbom.cdx.json` for the bundled dependencies.
