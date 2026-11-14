#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Clean-install smoke for a packaged riltra build (design gate G-NAT).
#
# Usage: release/smoke.sh <archive (.tar.gz or .zip) | directory with the binary>
#
# It runs the shipped binary only (no cargo, no source tree) in a scratch
# directory and an isolated RILTRA_HOME: version, help and status, init and
# index of a two-file fixture, a symbol search, an occurrence census, a
# callers query, and the MCP stdio server (initialize, tools/list, a source-
# grounded call, a classified error, a pure JSON-RPC stdout). An archive is
# extracted first, so the check covers what a user would unpack. Exit 0 only
# when every check passes; the first failure stops it with the check's name.
set -euo pipefail

target="${1:?usage: release/smoke.sh <archive or directory with the riltra binary>}"

# A native Windows binary needs Windows paths for its arguments and for
# RILTRA_HOME; elsewhere the path is already native.
native() { if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if [ -f "$target" ]; then
  mkdir -p "$work/unpacked"
  case "$target" in
    *.tar.gz) tar -xzf "$target" -C "$work/unpacked" ;;
    *.zip)
      powershell -NoProfile -Command "Expand-Archive -Force -LiteralPath '$(native "$target")' -DestinationPath '$(native "$work/unpacked")'"
      ;;
    *) echo "smoke: unknown archive type: $target" >&2; exit 2 ;;
  esac
  found="$(find "$work/unpacked" -type f \( -name riltra -o -name riltra.exe \) | head -n 1)"
  [ -n "$found" ] || { echo "smoke: no riltra binary in $target" >&2; exit 2; }
  # The archive must carry the notices a redistribution needs.
  unpacked="$(dirname "$found")"
  for doc in LICENSE NOTICE THIRD_PARTY_LICENSES.txt sbom.cdx.json INSTALL.md; do
    [ -s "$unpacked/$doc" ] || { echo "FAIL archive lacks $doc" >&2; exit 1; }
  done
  echo "ok   archive carries LICENSE, NOTICE, THIRD_PARTY_LICENSES.txt, sbom.cdx.json, INSTALL.md"
  dir="$unpacked"
else
  dir="$target"
fi
bin="$dir/riltra"
[ -x "$bin" ] || bin="$dir/riltra.exe"
[ -x "$bin" ] || { echo "smoke: no riltra binary in $dir" >&2; exit 2; }

export RILTRA_HOME
RILTRA_HOME="$(native "$work/home")"
mkdir -p "$work/home" "$work/repo"

pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; exit 1; }
expect() { # expect <name> <needle> <haystack>
  case "$3" in *"$2"*) pass "$1" ;; *) echo "got: $3" >&2; fail "$1" ;; esac
}

# --- the binary on its own ---------------------------------------------------
expect "version" "riltra " "$("$bin" --version)"
expect "help" "riltra mcp" "$("$bin" --help)"
expect "status" '"networking":"disabled"' "$("$bin" status)"

# --- a fixture repository ----------------------------------------------------
printf 'def alpha():\n    pass\n\ndef beta():\n    alpha()\n' >"$work/repo/a.py"
printf 'struct S {\n  int v = 0;\n};\nint read(S* s) {\n  return s->v;\n}\n' >"$work/repo/s.cc"

expect "init" '"registered":true' "$("$bin" init --repo "$(native "$work/repo")" --name smoke)"
expect "index" '"status_at_write":"fresh"' "$("$bin" index --repo smoke)"
expect "search" '"name":"alpha"' "$("$bin" search --repo smoke --symbol alpha)"
census="$("$bin" search --repo smoke --symbol v --occurrences)"
expect "occurrences" '"binding":"unbound"' "$census"
expect "occurrence item" '"role":"member-access"' "$census"
callers="$("$bin" callers --repo smoke --path a.py --line 1 --col 5)"
expect "callers are candidates" '"kind":"POTENTIAL_CALL"' "$callers"

# --- the MCP stdio server ----------------------------------------------------
init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"release-smoke","version":"1"}}}'
ready='{"jsonrpc":"2.0","method":"notifications/initialized"}'
list='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
call='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"riltra_code_search","arguments":{"repo":"smoke","query":"alpha"}}}'
bad='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"riltra_repo_status","arguments":{"repo":"does-not-exist"}}}'
{
  printf '%s\n' "$init"
  sleep 1
  printf '%s\n%s\n%s\n%s\n' "$ready" "$list" "$call" "$bad"
  sleep 3
} | "$bin" mcp >"$work/mcp.out" 2>"$work/mcp.err"

out="$(cat "$work/mcp.out")"
lines="$(grep -c . "$work/mcp.out" || true)"
jsonrpc="$(grep -c '^{"jsonrpc":"2.0"' "$work/mcp.out" || true)"
[ "$lines" -gt 0 ] && [ "$lines" = "$jsonrpc" ] && pass "stdout carries only JSON-RPC ($lines lines)" \
  || fail "stdout carries only JSON-RPC (lines=$lines jsonrpc=$jsonrpc)"
tools="$(grep -o '"name":"riltra_[a-z_]*"' "$work/mcp.out" | sort -u | wc -l | tr -d ' ')"
[ "$tools" = "15" ] && pass "fifteen tools advertised" || fail "fifteen tools advertised (got $tools)"
expect "a source-grounded call" 'alpha' "$out"
expect "a classified error" 'not_registered' "$out"
[ ! -s "$work/mcp.err" ] && pass "stderr is empty" || { cat "$work/mcp.err" >&2; fail "stderr is empty"; }

echo "smoke: all checks passed for $bin"
