#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Package the release-profile build of one platform (design gate decision
# D-M9-6: a native archive, a checksum and the notices; no installer and no
# signing infrastructure).
#
# Usage: release/package.sh <platform> <zip|tar.gz> [binary]
#   platform  part of the file name, e.g. windows-x64, linux-x64, macos-x64
#   binary    defaults to target/release/riltra (or riltra.exe)
#
# Output in dist/: riltra-<version>-<platform>.<ext> and its .sha256. The
# archive holds one directory with the binary, LICENSE, NOTICE,
# THIRD_PARTY_LICENSES.txt, sbom.cdx.json and INSTALL.md. The two generated
# documents come from the locked dependency graph (see release/README.md).
set -euo pipefail

platform="${1:?usage: release/package.sh <platform> <zip|tar.gz> [binary]}"
ext="${2:?usage: release/package.sh <platform> <zip|tar.gz> [binary]}"
root="$(cd "$(dirname "$0")/.." && pwd)"

bin="${3:-$root/target/release/riltra}"
[ -x "$bin" ] || bin="$bin.exe"
[ -x "$bin" ] || { echo "package: no release binary (run cargo build --release --locked)" >&2; exit 2; }

# The macOS ARM64 archive must carry an arm64 binary; a build from another
# architecture would ship under the wrong platform name. Other platforms are
# not checked here.
if [ "$platform" = macos-arm64 ]; then
  command -v lipo >/dev/null 2>&1 || { echo "package: lipo is required for macos-arm64" >&2; exit 2; }
  archs="$(lipo -archs "$bin")" || { echo "package: lipo could not read $bin" >&2; exit 2; }
  # lipo may pad its answer with whitespace; only the surrounding whitespace goes.
  arch_word="${archs#"${archs%%[![:space:]]*}"}"
  arch_word="${arch_word%"${arch_word##*[![:space:]]}"}"
  [ "$arch_word" = arm64 ] || { echo "package: $bin is '$archs', expected arm64 for macos-arm64" >&2; exit 2; }
fi

native() { if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

version="$("$bin" --version | awk '{print $2}')"
name="riltra-$version-$platform"
dist="$root/dist"
stage="$dist/stage/$name"

rm -rf "$dist/stage" "$dist/$name.$ext" "$dist/$name.$ext.sha256"
mkdir -p "$stage"
cp "$bin" "$stage/"
cp "$root/LICENSE" "$root/NOTICE" "$root/release/INSTALL.md" "$stage/"
(cd "$root" && cargo run --locked -q --example release_docs -- --out "$stage")

case "$ext" in
  tar.gz) tar -czf "$dist/$name.tar.gz" -C "$dist/stage" "$name" ;;
  zip)
    powershell -NoProfile -Command "Compress-Archive -Force -Path '$(native "$stage")' -DestinationPath '$(native "$dist/$name.zip")'"
    ;;
  *) echo "package: unknown archive type '$ext'" >&2; exit 2 ;;
esac
(cd "$dist" && sha256 "$name.$ext" >"$name.$ext.sha256")
rm -rf "$dist/stage"

echo "packaged $dist/$name.$ext"
cat "$dist/$name.$ext.sha256"
