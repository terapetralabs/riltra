#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Minimal user-level installer for a Riltra release archive (M10 Task 5).
#
# Local archive:  install.sh --archive <riltra-<version>-<platform>.tar.gz>
# Release asset:  install.sh [--version 0.3.0]
#
# The payload (binary, LICENSE, NOTICE, THIRD_PARTY_LICENSES.txt, sbom.cdx.json
# and INSTALL.md) goes to $HOME/.local/share/riltra. The user-visible command is
# a symlink at $HOME/.local/bin/riltra. Nothing outside those paths is written;
# PATH is printed, not changed.
set -eu

version=0.3.0
archive=
while [ $# -gt 0 ]; do
    case "$1" in
        --archive)
            [ $# -ge 2 ] || { echo "install: --archive needs a path" >&2; exit 2; }
            archive=$2
            shift 2
            ;;
        --version)
            [ $# -ge 2 ] || { echo "install: --version needs a value" >&2; exit 2; }
            version=$2
            shift 2
            ;;
        -h|--help)
            echo "usage: install.sh [--archive <tar.gz>] [--version <version>]"
            exit 0
            ;;
        *)
            echo "install: unknown option $1" >&2
            exit 2
            ;;
    esac
done

fail() {
    echo "install: $*" >&2
    exit 1
}

os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Linux) platform=linux-x64 ;;
    Darwin) platform=macos-x64 ;;
    *) fail "unsupported platform $os/$arch; supported: Linux x64, macOS Intel x64" ;;
esac
case "$arch" in
    x86_64|amd64) ;;
    *) fail "unsupported platform $os/$arch; supported: Linux x64, macOS Intel x64" ;;
esac
[ -n "${HOME:-}" ] || fail "HOME is not set"

share="$HOME/.local/share"
payload="$share/riltra"
bindir="$HOME/.local/bin"
link="$bindir/riltra"

mkdir -p "$share" || fail "cannot create $share"
stage=$(mktemp -d "$share/.riltra-stage.XXXXXX") || fail "cannot create a staging directory"
[ -n "$stage" ] || fail "cannot create a staging directory"
trap 'rm -rf "$stage"' EXIT HUP INT TERM

if [ -n "$archive" ]; then
    [ -f "$archive" ] || fail "archive not found: $archive"
    case "$(basename "$archive")" in
        riltra-*-"$platform".tar.gz) ;;
        *) fail "archive does not name a $platform release: $(basename "$archive")" ;;
    esac
else
    archive="$stage/riltra-$version-$platform.tar.gz"
    base="https://github.com/terapetralabs/riltra/releases/download/v$version"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$base/riltra-$version-$platform.tar.gz" -o "$archive" || fail "download failed"
        curl -fsSL "$base/riltra-$version-$platform.tar.gz.sha256" -o "$archive.sha256" || fail "checksum download failed"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$base/riltra-$version-$platform.tar.gz" -O "$archive" || fail "download failed"
        wget -q "$base/riltra-$version-$platform.tar.gz.sha256" -O "$archive.sha256" || fail "checksum download failed"
    else
        fail "curl or wget is required"
    fi
fi

sha_file="$archive.sha256"
[ -f "$sha_file" ] || fail "missing checksum file: $sha_file"
expected=$(awk 'NF { print $1; exit }' "$sha_file")
case "$expected" in
    ''|*[!0-9a-fA-F]*) fail "malformed checksum file: $sha_file" ;;
esac
[ "${#expected}" -eq 64 ] || fail "malformed checksum file: $sha_file"
named=$(awk 'NF >= 2 { print $2; exit }' "$sha_file")
if [ -n "$named" ]; then
    named=${named#\*}
    [ "$named" = "$(basename "$archive")" ] || fail "checksum names $named, not $(basename "$archive")"
fi
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$archive" | awk '{print $1}')
else
    fail "sha256sum or shasum is required to verify the archive"
fi
[ "$actual" = "$expected" ] || fail "SHA-256 mismatch for $archive"

listing="$stage/listing"
tar -tzf "$archive" >"$listing" 2>"$stage/tar.err" || fail "archive is not a readable tar.gz"
[ -s "$listing" ] || fail "archive is empty"
root=
while IFS= read -r entry; do
    [ -n "$entry" ] || fail "archive has an empty entry name"
    case "$entry" in
        /*) fail "unsafe archive entry: $entry" ;;
        *\\*) fail "unsafe archive entry: $entry" ;;
    esac
    case "/$entry/" in
        *"/../"*) fail "unsafe archive entry: $entry" ;;
    esac
    trimmed=${entry%/}
    top=${trimmed%%/*}
    case "$top" in
        ''|.|..) fail "unsafe archive entry: $entry" ;;
    esac
    if [ -z "$root" ]; then
        root=$top
    elif [ "$top" != "$root" ]; then
        fail "archive must have one top-level directory"
    fi
done <"$listing"
[ -n "$root" ] || fail "archive is empty"

tar -tvzf "$archive" >"$stage/types" 2>/dev/null || fail "archive is not a readable tar.gz"
while IFS= read -r line; do
    case "$line" in
        -*) ;;
        d*) ;;
        *) fail "unsafe archive entry type: ${line%% *}" ;;
    esac
done <"$stage/types"

mkdir -p "$stage/unpack"
# --no-same-owner only matters for root, and BSD tar is strict about option
# order, so the normal user path stays with plain bundled flags.
if [ "$(id -u)" -eq 0 ]; then
    tar -xzf "$archive" --no-same-owner -C "$stage/unpack" || fail "cannot extract archive"
else
    tar -xzf "$archive" -C "$stage/unpack" || fail "cannot extract archive"
fi
source="$stage/unpack/$root"
[ -d "$source" ] || fail "archive has no $root directory"
links=$(find "$stage/unpack" -type l -print)
[ -z "$links" ] || fail "archive contains a symbolic link"
[ -s "$source/riltra" ] || fail "archive does not contain riltra"
for doc in LICENSE NOTICE THIRD_PARTY_LICENSES.txt sbom.cdx.json INSTALL.md; do
    [ -s "$source/$doc" ] || fail "archive does not contain $doc"
done

files="$stage/files"
( cd "$source" && find . -type f ! -name '.riltra-manifest' -print ) |
    sed 's|^\./||' | LC_ALL=C sort >"$files"
[ -s "$files" ] || fail "archive payload is empty"
while IFS= read -r rel; do
    case "$rel" in
        ''|/*|..|../*|*/../*|*/..) fail "unsafe payload path: $rel" ;;
    esac
done <"$files"
cat "$files" >"$source/.riltra-manifest"

if [ -e "$payload" ] || [ -L "$payload" ]; then
    [ -d "$payload" ] && [ ! -L "$payload" ] || fail "payload path is not a directory: $payload"
    payload_entries=
    for entry in "$payload"/* "$payload"/.[!.]* "$payload"/..?*; do
        if [ -e "$entry" ] || [ -L "$entry" ]; then
            payload_entries=1
            break
        fi
    done
    if [ -n "$payload_entries" ]; then
        [ -f "$payload/.riltra-manifest" ] || fail "existing payload is not a Riltra installation: $payload"
        allowed="$stage/allowed"
        { printf '%s\n' '.riltra-manifest'; cat "$payload/.riltra-manifest"; } |
            LC_ALL=C sort -u >"$allowed"
        while IFS= read -r rel; do
            case "$rel" in
                ''|/*|..|../*|*/../*|*/..) fail "invalid manifest entry: $rel" ;;
            esac
        done <"$allowed"
        actual="$stage/actual"
        ( cd "$payload" && find . -print ) | sed 's|^\./||' |
            LC_ALL=C sort >"$actual"
        while IFS= read -r rel; do
            case "$rel" in
                ''|.) continue ;;
            esac
            if [ -d "$payload/$rel" ] && [ ! -L "$payload/$rel" ]; then
                if ! awk -v p="$rel/" 'index($0, p) == 1 { found = 1 } END { exit !found }' "$allowed"; then
                    fail "unexpected directory in payload: $rel"
                fi
            else
                grep -Fqx -- "$rel" "$allowed" || fail "unexpected file in payload: $rel"
            fi
        done <"$actual"
    fi
fi

if [ -L "$link" ]; then
    [ "$(readlink "$link")" = "$payload/riltra" ] || fail "refusing to replace $link: it is not a Riltra link"
elif [ -e "$link" ]; then
    fail "refusing to replace $link: it is not a Riltra link"
fi
if [ -e "$bindir" ] && [ ! -d "$bindir" ]; then
    fail "refusing to write $bindir: it is not a directory"
fi
mkdir -p "$bindir" || fail "cannot create $bindir"

chmod 755 "$source/riltra" || fail "cannot mark the binary executable"
backup=
if [ -e "$payload" ]; then
    backup="$share/.riltra-previous.$$"
    mv "$payload" "$backup" || fail "cannot move the old payload aside"
fi
if ! mv "$source" "$payload"; then
    if [ -n "$backup" ]; then
        mv "$backup" "$payload" || true
    fi
    fail "cannot move the new payload into place"
fi
if [ -n "$backup" ]; then
    rm -rf "$backup" || fail "cannot remove the old payload"
fi

tmp_link="$bindir/.riltra-link.$$"
ln -s "$payload/riltra" "$tmp_link" || fail "cannot create the link"
mv -f "$tmp_link" "$link" || { rm -f "$tmp_link"; fail "cannot install the link"; }

echo "installed $link"
echo "Add to PATH: export PATH=\"$bindir:\$PATH\""
