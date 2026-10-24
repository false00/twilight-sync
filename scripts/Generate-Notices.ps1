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
    $json = (& cargo metadata --locked --format-version 1 --manifest-path (Join-Path $root 'Cargo.toml')) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve dependency source locations.' }
    $metadata = ConvertFrom-Json -InputObject $json -AsHashtable
    $fonts = @($metadata.packages | Where-Object { $_.name -ceq 'epaint_default_fonts' })
    if ($fonts.Count -ne 1) { throw 'Review the bundled-font license selection before generating notices.' }
    $fontRoot = Join-Path (Split-Path -Parent $fonts[0].manifest_path) 'fonts'
    $text = [Text.StringBuilder]::new()
    $null = $text.AppendLine('Generated third-party and bundled-font notices for Twilight Sync.')
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
    $null = $text.AppendLine('Embedded dashboard fonts retain the following additional licenses:')
    $null = $text.AppendLine("Source: epaint_default_fonts $($fonts[0].version) on crates.io.")
    foreach ($name in @('emoji-icon-font-mit-license.txt', 'Hack-Regular.txt', 'OFL.txt', 'UFL.txt')) {
        $null = $text.AppendLine()
        $null = $text.AppendLine("=== Bundled font notice: $name ===")
        $null = $text.AppendLine([IO.File]::ReadAllText((Join-Path $fontRoot $name)))
    }
    [IO.File]::WriteAllText((Join-Path $root 'THIRD_PARTY_NOTICES.txt'), $text.ToString(), [Text.UTF8Encoding]::new($false))
    Write-Host 'Generated THIRD_PARTY_NOTICES.txt, including the embedded font notices.'
} finally {
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
}
