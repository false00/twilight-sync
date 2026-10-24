#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$ExecutablePath = '',
    [string]$OutputDirectory = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = [IO.File]::ReadAllText((Join-Path $root 'Cargo.toml'))
$match = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $match.Success -or $Version -cne ('v' + $match.Groups[1].Value)) {
    throw 'The release tag must exactly match the Cargo.toml package version.'
}
if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') { throw 'Only stable vMAJOR.MINOR.PATCH releases are packaged.' }
$notices = [IO.File]::ReadAllText((Join-Path $root 'THIRD_PARTY_NOTICES.txt'))
foreach ($name in @('Cargo.toml', 'Cargo.lock')) {
    $content = [IO.File]::ReadAllText((Join-Path $root $name)).Replace("`r`n", "`n")
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $hash = ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($content)))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
    if (-not $notices.Contains("$name SHA-256: $hash")) { throw 'Third-party notices are stale; run scripts\Generate-Notices.ps1 before packaging.' }
}
if ([string]::IsNullOrWhiteSpace($ExecutablePath)) { $ExecutablePath = Join-Path $root 'target\release\twilight-sync.exe' }
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) { $OutputDirectory = Join-Path $root 'dist' }
$executable = Get-Item -LiteralPath $ExecutablePath
if ($executable.Length -le 0 -or $executable.Length -gt 64MB) { throw 'Release executable has an invalid size.' }
$null = [IO.Directory]::CreateDirectory($OutputDirectory)
foreach ($path in @($OutputDirectory, (Join-Path $OutputDirectory 'twilight-sync-windows-x86_64.exe'), (Join-Path $OutputDirectory 'install.ps1'), (Join-Path $OutputDirectory 'SHA256SUMS.txt'))) {
    if (Test-Path -LiteralPath $path) {
        if (((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Release output cannot contain reparse points.' }
    }
}
Copy-Item -LiteralPath $ExecutablePath -Destination (Join-Path $OutputDirectory 'twilight-sync-windows-x86_64.exe') -Force
$bootstrap = [IO.File]::ReadAllText((Join-Path $root 'install.ps1')).Replace("`r`n", "`n")
[IO.File]::WriteAllText((Join-Path $OutputDirectory 'install.ps1'), $bootstrap, [Text.UTF8Encoding]::new($false))
$lines = foreach ($name in @('twilight-sync-windows-x86_64.exe', 'install.ps1')) {
    $hash = (Get-FileHash -LiteralPath (Join-Path $OutputDirectory $name) -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $name"
}
[IO.File]::WriteAllText((Join-Path $OutputDirectory 'SHA256SUMS.txt'), (($lines -join "`n") + "`n"), [Text.ASCIIEncoding]::new())
Write-Host "Prepared $Version Windows x64 assets in $OutputDirectory."
