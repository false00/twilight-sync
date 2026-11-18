#Requires -Version 7.0
[CmdletBinding()]
param([string]$CargoAbout = 'cargo-about')

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('twilight-notices-' + [guid]::NewGuid().ToString('N') + '.txt')
try {
    & $CargoAbout generate (Join-Path $PSScriptRoot 'licenses.hbs') --config (Join-Path $root 'about.toml') --manifest-path (Join-Path $root 'Cargo.toml') --target x86_64-pc-windows-msvc --locked --fail --output-file $temporary
    if ($LASTEXITCODE -ne 0) { throw 'Dependency license generation failed.' }
    $generated = [IO.File]::ReadAllText($temporary)
    $text = [Text.StringBuilder]::new()
    $null = $text.AppendLine('Generated third-party notices for Twilight Sync.')
    $null = $text.AppendLine('Manifest fingerprints use UTF-8 text with LF-normalized line endings.')
    foreach ($name in @('Cargo.toml', 'Cargo.lock')) {
        $content = [IO.File]::ReadAllText((Join-Path $root $name)).Replace("`r`n", "`n")
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $hash = ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($content)))).Replace('-', '').ToLowerInvariant() }
        finally { $sha.Dispose() }
        $null = $text.AppendLine("$name SHA-256: $hash")
    }
    $null = $text.AppendLine()
    $null = $text.AppendLine($generated)
    $null = $text.AppendLine('Slint and the i-slint components are used under LicenseRef-Slint-Royalty-free-2.0.')
    $null = $text.AppendLine('The required Slint attribution badge is displayed beside the download links at https://github.com/false00/twilight-sync.')
    $null = $text.AppendLine([IO.File]::ReadAllText((Join-Path $root 'assets\licenses\Slint-Royalty-Free-2.0.txt')))
    [IO.File]::WriteAllText((Join-Path $root 'THIRD_PARTY_NOTICES.txt'), $text.ToString(), [Text.UTF8Encoding]::new($false))
    Write-Host 'Generated THIRD_PARTY_NOTICES.txt. Slint attribution is on the public project/download page.'
} finally {
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
}
