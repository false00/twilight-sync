#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDirectory = '',
    [switch]$NoLaunch,
    [switch]$NoShortcut
)

& {
    param($Options)
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'

    function Get-TwilightRepository {
        return 'false00/twilight-sync'
    }

    function Assert-TwilightArchitecture {
        param([string]$Architecture = [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE', 'Machine'))
        if ([string]::IsNullOrWhiteSpace($Architecture)) {
            throw 'Cannot detect the native Windows architecture: the machine PROCESSOR_ARCHITECTURE value is missing.'
        }
        if (-not [Environment]::Is64BitOperatingSystem -or $Architecture -ine 'AMD64') {
            throw 'The published installer currently supports Windows x64 (Intel/AMD). ARM64 users must build from source.'
        }
    }

    function Assert-TwilightPath {
        param([Parameter(Mandatory = $true)][string]$Path)
        if (-not [IO.Path]::IsPathRooted($Path) -or $Path -match '(^|[\\/])\.\.([\\/]|$)' -or $Path.StartsWith('\\')) {
            throw 'Network, device, and parent-traversal paths are not supported.'
        }
        if ($Path -notmatch '^[A-Za-z]:\\' -or $Path.Length -le 3 -or $Path.Substring(2).Contains(':')) {
            throw 'Choose an absolute, local Windows installation folder without alternate data streams.'
        }
        foreach ($part in [regex]::Split($Path.Substring(3), '[\\/]')) {
            if ($part -match '[<>:"|?*\x00-\x1f]' -or $part.EndsWith('.') -or $part.EndsWith(' ')) {
                throw 'The installation path has an unsafe Windows component.'
            }
            $full = [IO.Path]::GetFullPath($Path)
            $stem = $part.Split('.')[0]
            if ($stem -match '^(CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])$') {
                throw 'Windows device names are not allowed in installation paths.'
            }
        }
        $current = $full
        while ($null -ne $current) {
            if (Test-Path -LiteralPath $current) {
                $item = Get-Item -LiteralPath $current -Force
                if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    throw "Symbolic links and reparse points are not allowed: $current"
                }
            }
            $parent = [IO.Directory]::GetParent($current)
            if ($null -eq $parent) { $current = $null } else { $current = $parent.FullName }
        }
        return $full
    }

    function Assert-TwilightUrl {
        param([Parameter(Mandatory = $true)][uri]$Uri)
        $allowed = @('api.github.com', 'github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com')
        if ($Uri.Scheme -cne 'https' -or $Uri.Port -ne 443 -or $Uri.UserInfo -ne '' -or $allowed -notcontains $Uri.DnsSafeHost) {
            throw "Refusing a download URL outside the HTTPS GitHub allowlist: $($Uri.Host)"
        }
    }

    function New-TwilightClient {
        Add-Type -AssemblyName System.Net.Http
        $handler = [Net.Http.HttpClientHandler]::new()
        $handler.AllowAutoRedirect = $false
        $handler.SslProtocols = [Security.Authentication.SslProtocols]::Tls12
        $client = [Net.Http.HttpClient]::new($handler)
        $client.Timeout = [TimeSpan]::FromMinutes(5)
        $client.DefaultRequestHeaders.UserAgent.ParseAdd('twilight-sync-installer')
        $client.DefaultRequestHeaders.Accept.ParseAdd('application/vnd.github+json')
        return $client
    }

    function Get-TwilightResponse {
        param($Client, [uri]$Uri)
        for ($redirect = 0; $redirect -le 5; $redirect++) {
            Assert-TwilightUrl $Uri
            $response = $Client.GetAsync($Uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            if ($response.IsSuccessStatusCode) { return $response }
            $status = [int]$response.StatusCode
            if ($status -in @(301, 302, 303, 307, 308)) {
                $location = $response.Headers.Location
                $response.Dispose()
                if ($null -eq $location) { throw 'The download redirect had no destination.' }
                $Uri = [uri]::new($Uri, $location)
                continue
            }
            $response.Dispose()
            throw "GitHub returned HTTP $status. Check your connection or API rate limit and retry."
        }
        throw 'Too many GitHub download redirects.'
    }

    function Read-TwilightText {
        param($Client, [uri]$Uri, [long]$Limit)
        $response = Get-TwilightResponse $Client $Uri
        $stream = $null
        $memory = [IO.MemoryStream]::new()
        $timeout = [Threading.CancellationTokenSource]::new([TimeSpan]::FromSeconds(60))
        try {
            $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
            $buffer = New-Object byte[] 16384
            while (($count = $stream.ReadAsync($buffer, 0, $buffer.Length, $timeout.Token).GetAwaiter().GetResult()) -gt 0) {
                if ($memory.Length + $count -gt $Limit) { throw 'GitHub metadata exceeded its size limit.' }
                $memory.Write($buffer, 0, $count)
            }
            $encoding = [Text.UTF8Encoding]::new($false, $true)
            return $encoding.GetString($memory.ToArray())
        } finally {
            if ($null -ne $stream) { $stream.Dispose() }
            $memory.Dispose()
            $timeout.Dispose()
            $response.Dispose()
        }
    }

    function Get-TwilightRelease {
        param($Client, [string]$RequestedVersion)
        if ($RequestedVersion -notmatch '^(latest|v?[0-9][A-Za-z0-9._-]{0,63})$' -or $RequestedVersion.Contains('..')) {
            throw 'Version must be latest or a release tag such as v0.2.0.'
        }
        $repository = Get-TwilightRepository
        if ($RequestedVersion -eq 'latest') {
            $url = "https://api.github.com/repos/$repository/releases/latest"
        } else {
            if ($RequestedVersion -match '^[0-9]') { $RequestedVersion = "v$RequestedVersion" }
            $url = "https://api.github.com/repos/$repository/releases/tags/$([uri]::EscapeDataString($RequestedVersion))"
        }
        $text = Read-TwilightText $Client ([uri]$url) (256 * 1024)
        $release = ConvertFrom-Json -InputObject $text
        if ($null -eq $release -or $release.tag_name -notmatch '^v[0-9][A-Za-z0-9._-]{0,63}$' -or $release.tag_name.Contains('..')) {
            throw 'GitHub returned an invalid release tag.'
        }
        if ($release.draft -or $release.prerelease) { throw 'The installer accepts published stable releases only.' }
        if ($RequestedVersion -ne 'latest' -and $release.tag_name -cne $RequestedVersion) {
            throw 'GitHub returned a different version than requested.'
        }
        return $release
    }

    function Get-TwilightAsset {
        param($Release, [string]$Name, [long]$Limit)
        $matches = @($Release.assets | Where-Object { $_.name -ceq $Name })
        if ($matches.Count -ne 1) { throw "Release $($Release.tag_name) must contain exactly one $Name asset." }
        $asset = $matches[0]
        $expected = "https://github.com/$(Get-TwilightRepository)/releases/download/$($Release.tag_name)/$Name"
        if ($asset.browser_download_url -cne $expected -or [long]$asset.size -le 0 -or [long]$asset.size -gt $Limit) {
            throw "The $Name asset has an unexpected source or size."
        }
        if ($asset.digest -notmatch '^sha256:([a-fA-F0-9]{64})$') {
            throw "The $Name asset has no published SHA-256 digest. Retry after the release has finished publishing."
        }
        Assert-TwilightUrl ([uri]$asset.browser_download_url)
        return $asset
    }

    function Get-TwilightChecksum {
        param([string]$Text, [string]$Name)
        $pattern = '^(?<hash>[a-fA-F0-9]{64})[ \t]+\*?' + [regex]::Escape($Name) + '$'
        $found = @()
        foreach ($line in ($Text -split '\r?\n')) {
            $match = [regex]::Match($line, $pattern)
            if ($match.Success) { $found += $match.Groups['hash'].Value.ToLowerInvariant() }
        }
        if ($found.Count -ne 1) { throw "SHA256SUMS.txt must contain exactly one checksum for $Name." }
        return $found[0]
    }

    function Receive-TwilightFile {
        param($Client, $Asset, [string]$Destination, [string]$Hash)
        $response = Get-TwilightResponse $Client ([uri]$Asset.browser_download_url)
        $stream = $null
        $output = $null
        $timeout = [Threading.CancellationTokenSource]::new([TimeSpan]::FromMinutes(5))
        try {
            $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
            $output = [IO.File]::Open($Destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            $buffer = New-Object byte[] 65536
            $total = [long]0
            while (($count = $stream.ReadAsync($buffer, 0, $buffer.Length, $timeout.Token).GetAwaiter().GetResult()) -gt 0) {
                $total += $count
                if ($total -gt [long]$Asset.size) { throw 'The executable exceeds its declared download size.' }
                $output.Write($buffer, 0, $count)
            }
            $output.Flush($true)
            $output.Dispose()
            $output = $null
            if ($total -ne [long]$Asset.size) { throw 'The executable download was truncated.' }
            if ((Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Hash) {
                throw 'Executable SHA-256 mismatch. Nothing was installed.'
            }
        } finally {
            if ($null -ne $output) { $output.Dispose() }
            if ($null -ne $stream) { $stream.Dispose() }
            $timeout.Dispose()
            $response.Dispose()
        }
    }

    function Assert-TwilightExecutable {
        param([string]$Path)
        $stream = [IO.File]::OpenRead($Path)
        $reader = [IO.BinaryReader]::new($stream)
        try {
            if ($stream.Length -lt 256 -or $reader.ReadUInt16() -ne 0x5a4d) { throw 'Downloaded file is not a Windows executable.' }
            $stream.Position = 0x3c
            $offset = $reader.ReadUInt32()
            if ($offset -lt 64 -or $offset + 26 -gt $stream.Length) { throw 'Downloaded executable has an invalid PE header.' }
            $stream.Position = $offset
            if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) {
                throw 'This release is not a Windows x64 executable.'
            }
            $stream.Position = $offset + 24
            if ($reader.ReadUInt16() -ne 0x20b) { throw 'The downloaded executable is not PE32+.' }
        } finally { $reader.Dispose() }
    }

    function New-TwilightShortcut {
        param([string]$Executable)
        $programs = [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
        $path = Join-Path $programs 'Twilight Sync.lnk'
        $null = Assert-TwilightPath $path
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $null
        try {
            $shortcut = $shell.CreateShortcut($path)
            if ((Test-Path -LiteralPath $path) -and -not [string]::Equals($shortcut.TargetPath, $Executable, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'A different Start-menu shortcut already uses the Twilight Sync name.'
            }
            $shortcut.TargetPath = $Executable
            $shortcut.WorkingDirectory = [IO.Path]::GetDirectoryName($Executable)
            $shortcut.Description = 'Private-LAN save sync and texture management for Dusklight'
            $shortcut.IconLocation = "$Executable,0"
            $shortcut.Save()
        } finally {
            if ($null -ne $shortcut) { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shortcut) }
            $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
        }
        return $path
    }

    function Install-TwilightSync {
        param([string]$Version = 'latest', [string]$InstallDirectory = '', [switch]$NoLaunch, [switch]$NoShortcut)
        if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Twilight Sync currently supports Windows only.' }
        Assert-TwilightArchitecture
        if ([string]::IsNullOrWhiteSpace($InstallDirectory)) {
            $local = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
            $InstallDirectory = Join-Path $local 'Programs\TwilightSync'
        }
        $directory = Assert-TwilightPath $InstallDirectory
        $null = [IO.Directory]::CreateDirectory($directory)
        $lockPath = Join-Path $directory '.install.lock'
        $null = Assert-TwilightPath $lockPath
        $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $client = $null
        $stage = $null
        $preserveStage = $false
        try {
            $client = New-TwilightClient
            $release = Get-TwilightRelease $client $Version
            $name = 'twilight-sync-windows-x86_64.exe'
            $asset = Get-TwilightAsset $release $name (64 * 1024 * 1024)
            $checksums = Get-TwilightAsset $release 'SHA256SUMS.txt' (64 * 1024)
            $text = Read-TwilightText $client ([uri]$checksums.browser_download_url) (64 * 1024)
            $textBytes = [Text.Encoding]::UTF8.GetBytes($text)
            $sha = [Security.Cryptography.SHA256]::Create()
            try { $textHash = ([BitConverter]::ToString($sha.ComputeHash($textBytes))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
            if ($checksums.digest.Substring(7).ToLowerInvariant() -cne $textHash) { throw 'Published checksum-file SHA-256 mismatch.' }
            $expected = Get-TwilightChecksum $text $name
            if ($asset.digest.Substring(7).ToLowerInvariant() -cne $expected) { throw 'Release checksum sources disagree.' }
            $executable = Join-Path $directory 'twilight-sync.exe'
            $receiptPath = Join-Path $directory '.twilight-sync-install.json'
            $null = Assert-TwilightPath $executable
            $null = Assert-TwilightPath $receiptPath
            $hadExecutable = Test-Path -LiteralPath $executable
            $hadReceipt = Test-Path -LiteralPath $receiptPath
            if ($hadExecutable -and -not $hadReceipt) {
                throw 'The destination already contains an unmanaged twilight-sync.exe. Choose an empty folder instead of overwriting it.'
            }
            $old = $null
            if ($hadReceipt) {
                if ((Get-Item -LiteralPath $receiptPath).Length -gt 16384) { throw 'Installation receipt exceeds its size limit.' }
                $old = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($receiptPath))
                if ($old.format -ne 1 -or $old.repository -cne (Get-TwilightRepository) -or $old.executable -cne 'twilight-sync.exe' -or $old.sha256 -notmatch '^[a-f0-9]{64}$') {
                    throw 'The existing installation receipt is invalid.'
                }
                if ($hadExecutable -and (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant() -cne $old.sha256) {
                    throw 'The existing executable was modified. It was not overwritten; preserve it and choose an empty installation folder.'
                }
            }
            $changed = -not ($hadExecutable -and $hadReceipt -and $old.version -ceq $release.tag_name -and $old.sha256 -ceq $expected)
            if ($changed) {
                if ($hadExecutable) {
                    try {
                        $probe = [IO.File]::Open($executable, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
                        $probe.Dispose()
                    } catch [IO.IOException] {
                        throw 'The app is in use. Stop Dusklight, quit Twilight Sync from its tray menu, and rerun the installer. No process was terminated.'
                    }
                }
                $stage = Join-Path $directory ('.install-' + [guid]::NewGuid().ToString('N'))
                $null = [IO.Directory]::CreateDirectory($stage)
                $download = Join-Path $stage 'twilight-sync.exe'
                Receive-TwilightFile $client $asset $download $expected
                Assert-TwilightExecutable $download
                $receipt = [ordered]@{ format = 1; repository = Get-TwilightRepository; version = $release.tag_name; architecture = 'x86_64'; executable = 'twilight-sync.exe'; sha256 = $expected }
                $stagedReceipt = Join-Path $stage 'receipt.json'
                [IO.File]::WriteAllText($stagedReceipt, ($receipt | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
                $backupExe = Join-Path $stage 'previous.exe'
                $backupReceipt = Join-Path $stage 'previous.json'
                $movedExe = $false
                $movedReceipt = $false
                try {
                    if ($hadExecutable) { [IO.File]::Replace($download, $executable, $backupExe) }
                    else { [IO.File]::Move($download, $executable) }
                    $movedExe = $true
                    if ($hadReceipt) { [IO.File]::Replace($stagedReceipt, $receiptPath, $backupReceipt) }
                    else { [IO.File]::Move($stagedReceipt, $receiptPath) }
                    $movedReceipt = $true
                } catch {
                    $failure = $_
                    try {
                        if ($movedReceipt) {
                            if ($hadReceipt) { [IO.File]::Replace($backupReceipt, $receiptPath, (Join-Path $stage 'failed.json')) }
                            else { [IO.File]::Delete($receiptPath) }
                        }
                        if ($movedExe) {
                            if ($hadExecutable) { [IO.File]::Replace($backupExe, $executable, (Join-Path $stage 'failed.exe')) }
                            else { [IO.File]::Delete($executable) }
                        }
                    } catch {
                        $preserveStage = $true
                        throw "Installation failed and rollback needs manual review. Previous files remain in $stage. $failure ; $_"
                    }
                    throw "Installation failed; the previous installation was restored. $failure"
                }
            }
            $shortcut = $null
            if (-not $NoShortcut) {
                try { $shortcut = New-TwilightShortcut $executable }
                catch { throw "The verified app is installed at $executable, but its shortcut could not be created. Use -NoShortcut or open that executable directly. $_" }
            }
            $launched = $false
            if (-not $NoLaunch -and $changed) {
                $null = Start-Process -FilePath $executable -WorkingDirectory $directory -PassThru
                $launched = $true
            }
            return [pscustomobject]@{ Version = $release.tag_name; Executable = $executable; Changed = $changed; Shortcut = $shortcut; Launched = $launched }
        } finally {
            try {
                if ($null -ne $client) { $client.Dispose() }
                if (-not $preserveStage -and $null -ne $stage -and (Test-Path -LiteralPath $stage)) {
                    # Only these installer-owned files are eligible for cleanup; never recurse into the install root.
                    foreach ($name in @('twilight-sync.exe', 'receipt.json', 'previous.exe', 'previous.json', 'failed.exe', 'failed.json')) {
                        $path = Join-Path $stage $name
                        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
                    }
                    [IO.Directory]::Delete($stage, $false)
                }
            } finally { $lock.Dispose() }
        }
    }

    $result = Install-TwilightSync -Version $Options.Version -InstallDirectory $Options.InstallDirectory -NoLaunch:$Options.NoLaunch -NoShortcut:$Options.NoShortcut
    Write-Host "Twilight Sync $($result.Version) is ready."
    Write-Host "Installed for your user: $($result.Executable)"
    if (-not $result.Changed) { Write-Host 'Already up to date. Open Twilight Sync from the Start menu.' }
    elseif (-not $result.Launched) { Write-Host 'No app was launched. Open Twilight Sync when you are ready.' }
    Write-Host 'Saves, pairing keys, texture libraries, and startup preferences were not changed by this installer.'
} @{ Version = $Version; InstallDirectory = $InstallDirectory; NoLaunch = [bool]$NoLaunch; NoShortcut = [bool]$NoShortcut }
