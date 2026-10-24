#Requires -Version 5.1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$installer = Join-Path $repository 'install.ps1'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
if ($errors.Count -gt 0) { throw ($errors | Out-String) }

# Load only local function declarations. Do not execute the installer entry point.
$definitions = $ast.FindAll({
    param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst]
}, $true)
foreach ($definition in $definitions) {
    . ([scriptblock]::Create($definition.Extent.Text))
}

$script:Passed = 0
function Assert-Installer {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "FAIL: $Message" }
    $script:Passed++
    Write-Host "PASS: $Message"
}

function Assert-InstallerThrows {
    param([scriptblock]$Action, [string]$Expected)
    $failure = $null
    try { & $Action | Out-Null } catch { $failure = $_ }
    if ($null -eq $failure -or $failure.ToString() -notmatch [regex]::Escape($Expected)) {
        throw "Expected error containing '$Expected', got '$failure'."
    }
    $script:Passed++
    Write-Host "PASS: rejects $Expected"
}

function Get-FixtureHash {
    param([byte[]]$Bytes)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Set-ReleaseFixture {
    param([string]$Version, [byte]$Marker)
    $bytes = New-Object byte[] 512
    $bytes[0] = 0x4d
    $bytes[1] = 0x5a
    [BitConverter]::GetBytes([uint32]0x80).CopyTo($bytes, 0x3c)
    $bytes[0x80] = 0x50
    $bytes[0x81] = 0x45
    $bytes[0x84] = 0x64
    $bytes[0x85] = 0x86
    $bytes[0x98] = 0x0b
    $bytes[0x99] = 0x02
    $bytes[300] = $Marker
    $script:FixtureBytes = $bytes
    $script:FixtureHash = Get-FixtureHash $bytes
    $name = 'twilight-sync-windows-x86_64.exe'
    $script:ChecksumText = "$script:FixtureHash  $name`n"
    $checksumHash = Get-FixtureHash ([Text.Encoding]::UTF8.GetBytes($script:ChecksumText))
    $base = "https://github.com/false00/twilight-sync/releases/download/$Version"
    $script:ReleaseFixture = [pscustomobject]@{
        tag_name = $Version
        draft = $false
        prerelease = $false
        assets = @(
            [pscustomobject]@{ name = $name; size = $bytes.Length; browser_download_url = "$base/$name"; digest = "sha256:$script:FixtureHash" },
            [pscustomobject]@{ name = 'SHA256SUMS.txt'; size = ([Text.Encoding]::UTF8.GetBytes($script:ChecksumText)).Length; browser_download_url = "$base/SHA256SUMS.txt"; digest = "sha256:$checksumHash" }
        )
    }
}

# Replace only network I/O with synthetic bytes. These fixtures are never launched.
function Read-TwilightText {
    param($Client, [uri]$Uri, [long]$Limit)
    if ($Uri.Host -eq 'api.github.com') { return ($script:ReleaseFixture | ConvertTo-Json -Depth 8) }
    if ($Uri.AbsolutePath.EndsWith('/SHA256SUMS.txt')) { return $script:ChecksumText }
    throw "Unexpected fixture request: $Uri"
}

$script:Fetches = 0
function Receive-TwilightFile {
    param($Client, $Asset, [string]$Destination, [string]$Hash)
    $script:Fetches++
    [IO.File]::WriteAllBytes($Destination, $script:FixtureBytes)
    if ((Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Hash) {
        throw 'Executable SHA-256 mismatch. Nothing was installed.'
    }
}

$root = Join-Path ([IO.Path]::GetTempPath()) ('twilight-installer-tests-' + [guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($root)
try {
    Assert-Installer ($definitions.Count -ge 10) 'installer parses without executing its entry point'
    foreach ($path in @('relative', 'C:\', 'C:\Temp\..\escape', '\\server\share', 'C:\Temp\CON.app', 'C:\Temp\file.exe:stream', 'C:\Temp\ambiguous.')) {
        Assert-InstallerThrows { Assert-TwilightPath $path } $(if ($path -in @('relative', 'C:\Temp\..\escape', '\\server\share')) { 'not supported' } elseif ($path -eq 'C:\') { 'absolute, local' } elseif ($path -eq 'C:\Temp\CON.app') { 'device names' } elseif ($path -eq 'C:\Temp\file.exe:stream') { 'alternate data streams' } else { 'unsafe Windows component' })
    }
    foreach ($url in @('http://github.com/file', 'https://github.com.evil.example/file', 'https://user@github.com/file', 'https://github.com:444/file')) {
        Assert-InstallerThrows { Assert-TwilightUrl ([uri]$url) } 'HTTPS GitHub allowlist'
    }
    Assert-TwilightUrl ([uri]'https://release-assets.githubusercontent.com/file')
    Assert-Installer $true 'known HTTPS asset host is accepted'

    Set-ReleaseFixture 'v0.2.0' 1
    $client = New-TwilightClient
    try {
        $release = Get-TwilightRelease $client '0.2.0'
        Assert-Installer ($release.tag_name -eq 'v0.2.0') 'version argument resolves the requested stable tag'
        Assert-InstallerThrows { Get-TwilightRelease $client '../bad' } 'Version must be latest'
        Assert-InstallerThrows { Get-TwilightRelease $client 'v0.2.1' } 'different version'
        $asset = Get-TwilightAsset $release 'twilight-sync-windows-x86_64.exe' 1000
        Assert-Installer ($asset.size -eq 512) 'exact expected release asset is selected'
        $asset.browser_download_url = 'https://github.com/other/repository/file.exe'
        Assert-InstallerThrows { Get-TwilightAsset $release $asset.name 1000 } 'unexpected source or size'
    } finally { $client.Dispose() }
    Set-ReleaseFixture 'v0.2.0' 1
    Assert-InstallerThrows { Get-TwilightChecksum "$script:ChecksumText$script:ChecksumText" 'twilight-sync-windows-x86_64.exe' } 'exactly one checksum'

    $directory = Join-Path $root 'managed'
    $first = Install-TwilightSync -InstallDirectory $directory -NoLaunch -NoShortcut
    Assert-Installer ($first.Changed -and -not $first.Launched -and $null -eq $first.Shortcut) 'new installation honors no-launch and no-shortcut'
    Assert-Installer ((Get-FileHash -LiteralPath $first.Executable -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $script:FixtureHash) 'installed bytes match the published checksum'
    $fetches = $script:Fetches
    $same = Install-TwilightSync -InstallDirectory $directory -NoLaunch -NoShortcut
    Assert-Installer (-not $same.Changed -and $script:Fetches -eq $fetches) 'repeat installation is idempotent without redownloading'

    $before = [IO.File]::ReadAllBytes($first.Executable)
    $receiptPath = Join-Path $directory '.twilight-sync-install.json'
    $beforeReceipt = [IO.File]::ReadAllText($receiptPath)
    Set-ReleaseFixture 'v0.2.1' 2
    $receiptLock = [IO.File]::Open($receiptPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        Assert-InstallerThrows { Install-TwilightSync -InstallDirectory $directory -NoLaunch -NoShortcut } 'previous installation was restored'
    } finally { $receiptLock.Dispose() }
    Assert-Installer ((Get-FixtureHash ([IO.File]::ReadAllBytes($first.Executable))) -ceq (Get-FixtureHash $before)) 'failed receipt commit rolls back the executable'
    Assert-Installer ([IO.File]::ReadAllText($receiptPath) -ceq $beforeReceipt) 'failed upgrade preserves the previous receipt'
    $updated = Install-TwilightSync -InstallDirectory $directory -NoLaunch -NoShortcut
    Assert-Installer ($updated.Changed -and $updated.Version -eq 'v0.2.1') 'verified upgrades keep the executable path stable'
    Assert-Installer (@(Get-ChildItem -LiteralPath $directory -Directory -Force).Count -eq 0) 'successful and rolled-back stages are cleaned up'

    [IO.File]::WriteAllText($updated.Executable, 'user-modified executable')
    Assert-InstallerThrows { Install-TwilightSync -InstallDirectory $directory -NoLaunch -NoShortcut } 'existing executable was modified'
    Assert-Installer ([IO.File]::ReadAllText($updated.Executable) -ceq 'user-modified executable') 'modified files are never overwritten'

    $unmanaged = Join-Path $root 'unmanaged'
    $null = [IO.Directory]::CreateDirectory($unmanaged)
    [IO.File]::WriteAllText((Join-Path $unmanaged 'twilight-sync.exe'), 'unrelated file')
    Assert-InstallerThrows { Install-TwilightSync -InstallDirectory $unmanaged -NoLaunch -NoShortcut } 'unmanaged twilight-sync.exe'
    Write-Host "Installer tests passed: $script:Passed assertions on PowerShell $($PSVersionTable.PSVersion)."
} finally {
    # This is the specific test directory created above, never a user install or data directory.
    [IO.Directory]::Delete($root, $true)
}
