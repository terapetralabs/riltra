# SPDX-License-Identifier: Apache-2.0
#
# Minimal user-level installer for a Riltra release archive (M10 Task 5).
#
# Local archive:  install.ps1 -ArchivePath <riltra-<version>-windows-x64.zip>
# Release asset:  install.ps1 [-Version 0.2.1]
#
# The payload (binary, LICENSE, NOTICE, THIRD_PARTY_LICENSES.txt, sbom.cdx.json
# and INSTALL.md) goes to %LOCALAPPDATA%\Programs\Riltra\bin. PATH is printed,
# not changed. No administrative rights are needed or requested.
param(
    [string]$ArchivePath,
    [string]$Version = '0.2.1'
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Fail([string]$Message) {
    [Console]::Error.WriteLine("install: $Message")
    exit 1
}

$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64') {
    Fail "unsupported platform Windows/$arch; supported: Windows x64"
}
if (-not $env:LOCALAPPDATA) {
    Fail 'LOCALAPPDATA is not set'
}

$installParent = Join-Path $env:LOCALAPPDATA 'Programs\Riltra'
$installDir = Join-Path $installParent 'bin'
New-Item -ItemType Directory -Force -Path $installParent | Out-Null
$stage = Join-Path $installParent ('.stage-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $stage | Out-Null

try {
    if ($ArchivePath) {
        $archive = (Resolve-Path -LiteralPath $ArchivePath).Path
        if ([System.IO.Path]::GetFileName($archive) -notmatch '^riltra-.+-windows-x64\.zip$') {
            Fail "archive does not name a windows-x64 release: $([System.IO.Path]::GetFileName($archive))"
        }
    } else {
        $archive = Join-Path $stage "riltra-$Version-windows-x64.zip"
        $base = "https://github.com/terapetralabs/riltra/releases/download/v$Version"
        $params = @{ Uri = "$base/riltra-$Version-windows-x64.zip"; OutFile = $archive }
        if ($PSVersionTable.PSVersion.Major -lt 6) { $params.UseBasicParsing = $true }
        Invoke-WebRequest @params
        $params.Uri = "$base/riltra-$Version-windows-x64.zip.sha256"
        $params.OutFile = "$archive.sha256"
        Invoke-WebRequest @params
    }

    $shaFile = "$archive.sha256"
    if (-not (Test-Path -LiteralPath $shaFile -PathType Leaf)) {
        Fail "missing checksum file: $shaFile"
    }
    $fields = ((Get-Content -LiteralPath $shaFile -Raw).Trim() -split '\s+')
    $expected = $fields[0]
    if ($expected -notmatch '^[0-9a-fA-F]{64}$') {
        Fail "malformed checksum file: $shaFile"
    }
    if ($fields.Count -ge 2) {
        $named = $fields[1].TrimStart('*')
        if ($named -ne [System.IO.Path]::GetFileName($archive)) {
            Fail "checksum names $named, not $([System.IO.Path]::GetFileName($archive))"
        }
    }
    $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    if ($actual -ne $expected.ToUpperInvariant()) {
        Fail "SHA-256 mismatch for $archive"
    }

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $unpack = Join-Path $stage 'unpack'
    New-Item -ItemType Directory -Force -Path $unpack | Out-Null
    $root = $null
    $zip = $null
    try {
        $zip = [System.IO.Compression.ZipFile]::OpenRead($archive)
        $prefix = [System.IO.Path]::GetFullPath($unpack) + [System.IO.Path]::DirectorySeparatorChar
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            if ([string]::IsNullOrWhiteSpace($name)) { Fail 'archive has an empty entry name' }
            if ($name.StartsWith('/') -or $name.Contains(':')) {
                Fail "unsafe archive entry: $name"
            }
            if ([System.IO.Path]::IsPathRooted($name)) { Fail "unsafe archive entry: $name" }
            $parts = $name.Split('/')
            if (($parts -contains '..') -or ($parts -contains '.')) {
                Fail "unsafe archive entry: $name"
            }
            $top = $parts[0]
            if ([string]::IsNullOrEmpty($top)) { Fail "unsafe archive entry: $name" }
            if ($null -eq $root) { $root = $top } elseif ($top -ne $root) { Fail 'archive must have one top-level directory' }
            $mode = ($entry.ExternalAttributes -shr 16) -band 0xFFFF
            if (($mode -band 0xF000) -eq 0xA000) { Fail "unsafe archive entry: $name" }
            $full = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($unpack, $name))
            if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
                Fail "unsafe archive entry: $name"
            }
        }
    }
    catch {
        Fail $_.Exception.Message
    }
    finally {
        if ($null -ne $zip) { $zip.Dispose() }
    }
    if ($null -eq $root) { Fail 'archive is empty' }

    Expand-Archive -LiteralPath $archive -DestinationPath $unpack -Force
    $source = Join-Path $unpack $root
    if (-not (Test-Path -LiteralPath (Join-Path $source 'riltra.exe') -PathType Leaf)) {
        Fail 'archive does not contain riltra.exe'
    }
    foreach ($doc in @('LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.txt', 'sbom.cdx.json', 'INSTALL.md')) {
        $path = Join-Path $source $doc
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { Fail "archive does not contain $doc" }
        if ((Get-Item -LiteralPath $path).Length -eq 0) { Fail "archive contains an empty $doc" }
    }

    $files = @(Get-ChildItem -LiteralPath $source -Recurse -File -Force |
        ForEach-Object { $_.FullName.Substring($source.Length + 1).Replace('\', '/') } |
        Sort-Object)
    if ($files.Count -eq 0) { Fail 'archive payload is empty' }
    foreach ($rel in $files) {
        if ($rel.StartsWith('/') -or $rel.Contains('\') -or $rel.Contains(':') -or
            $rel -eq '..' -or $rel.StartsWith('../') -or $rel.Contains('/../') -or $rel.EndsWith('/..')) {
            Fail "unsafe payload path: $rel"
        }
    }
    Set-Content -LiteralPath (Join-Path $source '.riltra-manifest') -Value $files -Encoding Ascii

    if (Test-Path -LiteralPath $installDir) {
        if (-not (Test-Path -LiteralPath $installDir -PathType Container)) {
            Fail "payload path is not a directory: $installDir"
        }
        $items = @(Get-ChildItem -LiteralPath $installDir -Force -Recurse)
        if ($items.Count -gt 0) {
            $manifest = Join-Path $installDir '.riltra-manifest'
            if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
                Fail "existing payload is not a Riltra installation: $installDir"
            }
            $allowed = @('.riltra-manifest') + @(Get-Content -LiteralPath $manifest)
            foreach ($rel in $allowed) {
                if ($rel.StartsWith('/') -or $rel.Contains('\') -or $rel.Contains(':') -or
                    $rel -eq '..' -or $rel.StartsWith('../') -or $rel.Contains('/../') -or $rel.EndsWith('/..')) {
                    Fail "invalid manifest entry: $rel"
                }
            }
            foreach ($item in $items) {
                $rel = $item.FullName.Substring($installDir.Length + 1).Replace('\', '/')
                if ($item.PSIsContainer) {
                    $prefix = $rel + '/'
                    if (-not ($allowed | Where-Object { $_.StartsWith($prefix, [StringComparison]::Ordinal) })) {
                        Fail "unexpected directory in payload: $rel"
                    }
                } elseif ($allowed -notcontains $rel) {
                    Fail "unexpected file in payload: $rel"
                }
            }
        }
    }

    $backup = $null
    if (Test-Path -LiteralPath $installDir) {
        $backup = Join-Path $installParent ('.previous-' + [guid]::NewGuid().ToString('N'))
        [System.IO.Directory]::Move($installDir, $backup)
    }
    try {
        [System.IO.Directory]::Move($source, $installDir)
    }
    catch {
        if ($null -ne $backup -and (Test-Path -LiteralPath $backup)) {
            try { [System.IO.Directory]::Move($backup, $installDir) } catch { }
        }
        Fail $_.Exception.Message
    }
    if ($null -ne $backup) {
        Remove-Item -LiteralPath $backup -Recurse -Force
    }

    Write-Output "installed $(Join-Path $installDir 'riltra.exe')"
    Write-Output "Add to PATH: $installDir"
}
finally {
    if (Test-Path -LiteralPath $stage) {
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
    }
}
