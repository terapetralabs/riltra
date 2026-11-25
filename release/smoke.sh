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

# MCP stdout is one JSON-RPC response per line. Include the delimiter so id 50
# cannot match id 5, then validate the overview result independently.
overview_response_line() {
  grep -F '"id":5,' "$1" || true
}

overview_response_is_successful() {
  local response="${1:-}"
  case "$response" in *'"id":5,'*) ;; *) return 1 ;; esac
  case "$response" in *'"error":'*|*'"isError":true'*) return 1 ;; esac
  case "$response" in *'"result":'*) ;; *) return 1 ;; esac
  case "$response" in *'"repo":"smoke"'*) ;; *) return 1 ;; esac
  case "$response" in *'"snapshot":'*) return 0 ;; *) return 1 ;; esac
}

# Exercise the exact selector and predicate without a binary or user data.
if [ "${1:-}" = "--test-overview-parser" ]; then
  parser_test_work="$(mktemp -d)"
  trap 'rm -rf "$parser_test_work"' EXIT
  search='{"jsonrpc":"2.0","id":3,"result":{"structuredContent":{"repo":"smoke"}}}'
  success='{"jsonrpc":"2.0","id":5,"result":{"isError":false,"content":[{"text":"{\"repo\":\"smoke\",\"snapshot\":{}}"}],"structuredContent":{"repo":"smoke","snapshot":{}}}}'
  protocol_error='{"jsonrpc":"2.0","id":5,"error":{"code":-32603,"message":"overview failed"}}'
  tool_error='{"jsonrpc":"2.0","id":5,"result":{"isError":true,"content":[{"text":"overview failed"}]}}'
  id_fifty='{"jsonrpc":"2.0","id":50,"result":{"structuredContent":{"repo":"smoke","snapshot":{}}}}'

  expect_overview_rejected() {
    local name="$1" transcript="$2" response
    printf '%s\n' "$transcript" >"$parser_test_work/mcp.out"
    response="$(overview_response_line "$parser_test_work/mcp.out")"
    if overview_response_is_successful "$response"; then
      echo "FAIL parser accepted $name" >&2
      exit 1
    fi
    echo "ok   parser rejects $name"
  }
  expect_overview_accepted() {
    local name="$1" transcript="$2" response
    printf '%s\n' "$transcript" >"$parser_test_work/mcp.out"
    response="$(overview_response_line "$parser_test_work/mcp.out")"
    if ! overview_response_is_successful "$response"; then
      echo "FAIL parser rejected $name" >&2
      exit 1
    fi
    echo "ok   parser accepts $name"
  }

  expect_overview_rejected "successful search with missing overview" "$search"
  expect_overview_rejected "successful search with overview protocol error" "$search
$protocol_error"
  expect_overview_rejected "successful search with overview tool error" "$search
$tool_error"
  expect_overview_rejected "id 50 instead of id 5" "$search
$id_fifty"
  expect_overview_accepted "successful overview id 5" "$search
$success"
  exit 0
fi

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
expect "help start" "riltra start" "$("$bin" --help)"
expect "help overview" "riltra overview --repo <name>" "$("$bin" --help)"
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
grep_doc="$("$bin" grep --repo smoke --pattern 'alpha()')"
expect "grep names the enclosing definition" '"in":{"name":"beta"' "$grep_doc"

# --- the MCP stdio server ----------------------------------------------------
init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"release-smoke","version":"1"}}}'
ready='{"jsonrpc":"2.0","method":"notifications/initialized"}'
list='{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
call='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"riltra_code_search","arguments":{"repo":"smoke","query":"alpha"}}}'
call_overview='{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"riltra_project_overview","arguments":{"repo":"smoke"}}}'
bad='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"riltra_repo_status","arguments":{"repo":"does-not-exist"}}}'
{
  printf '%s\n' "$init"
  sleep 1
  printf '%s\n%s\n%s\n%s\n%s\n' "$ready" "$list" "$call" "$bad" "$call_overview"
  sleep 3
} | "$bin" mcp >"$work/mcp.out" 2>"$work/mcp.err"

out="$(cat "$work/mcp.out")"
lines="$(grep -c . "$work/mcp.out" || true)"
jsonrpc="$(grep -c '^{"jsonrpc":"2.0"' "$work/mcp.out" || true)"
[ "$lines" -gt 0 ] && [ "$lines" = "$jsonrpc" ] && pass "stdout carries only JSON-RPC ($lines lines)" \
  || fail "stdout carries only JSON-RPC (lines=$lines jsonrpc=$jsonrpc)"
tools="$(grep -o '"name":"riltra_[a-z_]*"' "$work/mcp.out" | sort -u | wc -l | tr -d ' ')"
[ "$tools" = "8" ] && pass "eight core tools advertised by default" || fail "eight core tools advertised by default (got $tools)"
expect "overview tool advertised" '"name":"riltra_project_overview"' "$out"
expect "initialize carries instructions" '"instructions"' "$out"
{
  printf '%s\n' "$init"
  sleep 1
  printf '%s\n%s\n' "$ready" "$list"
  sleep 2
} | "$bin" mcp --all-tools >"$work/mcp-all.out" 2>"$work/mcp-all.err"
all_tools="$(grep -o '"name":"riltra_[a-z_]*"' "$work/mcp-all.out" | sort -u | wc -l | tr -d ' ')"
[ "$all_tools" = "22" ] && pass "twenty-two tools advertised with --all-tools" || fail "twenty-two tools advertised with --all-tools (got $all_tools)"
expect "packet tool advertised with --all-tools" '"name":"riltra_code_packet"' "$(cat "$work/mcp-all.out")"
expect "a source-grounded call" 'alpha' "$out"
overview_response="$(overview_response_line "$work/mcp.out")"
overview_response_is_successful "$overview_response" \
  || fail "overview response id 5 must contain a successful smoke project document"
pass "overview response id 5 returned the smoke project"
expect "a classified error" 'not_registered' "$out"
[ ! -s "$work/mcp.err" ] && pass "stderr is empty" || { cat "$work/mcp.err" >&2; fail "stderr is empty"; }

echo "smoke: all checks passed for $bin"
