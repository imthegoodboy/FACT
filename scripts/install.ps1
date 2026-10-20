# FACT per-user installer.
#
# Downloads a verified release asset by default. To install a local build:
#   .\scripts\install.ps1 -BinaryPath .\target\release\fact.exe -Force
#
# No administrator rights are required. The installer never changes machine-wide
# PATH or installs a service.

[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\FACT"),
    [string]$BinaryPath,
    [switch]$NoPath,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$Repo = "imthegoodboy/FACT"
$AssetArch = "x86_64-pc-windows-msvc"

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Invoke-Download([string]$Uri, [string]$OutFile) {
    $parameters = @{ Uri = $Uri; OutFile = $OutFile }
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        $parameters["UseBasicParsing"] = $true
    }
    Invoke-WebRequest @parameters | Out-Null
}

function Get-WebText([string]$Uri) {
    $parameters = @{
        Uri = $Uri
        Headers = @{ "User-Agent" = "FACT-installer" }
    }
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        $parameters["UseBasicParsing"] = $true
    }
    return (Invoke-WebRequest @parameters).Content
}

function Resolve-NormalizedPath([string]$Path) {
    return [IO.Path]::GetFullPath($Path).TrimEnd('\')
}

function Get-UserPathRaw {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment")
    if (-not $key) {
        return ""
    }
    try {
        $value = $key.GetValue(
            "Path",
            $null,
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
        )
        if ($null -eq $value) {
            return ""
        }
        return [string]$value
    } finally {
        $key.Close()
    }
}

function Send-EnvironmentChanged {
    try {
        if (-not ("FactEnvironmentBroadcast" -as [type])) {
            Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class FactEnvironmentBroadcast {
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
    public static extern IntPtr SendMessageTimeout(
        IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam,
        uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
}
"@
        }
        $result = [UIntPtr]::Zero
        [FactEnvironmentBroadcast]::SendMessageTimeout(
            [IntPtr]0xffff,
            0x001A,
            [UIntPtr]::Zero,
            "Environment",
            2,
            5000,
            [ref]$result
        ) | Out-Null
    } catch {
        # The registry update is still valid; new logons will see it.
    }
}

function Set-UserPathRaw([string]$Value) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
    if (-not $key) {
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Environment")
    }
    try {
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        try {
            if ($key.GetValueKind("Path") -eq [Microsoft.Win32.RegistryValueKind]::String) {
                $kind = [Microsoft.Win32.RegistryValueKind]::String
            }
        } catch {
        }
        if ([string]::IsNullOrWhiteSpace($Value)) {
            $key.DeleteValue("Path", $false)
        } else {
            $key.SetValue("Path", $Value, $kind)
        }
    } finally {
        $key.Close()
    }
    Send-EnvironmentChanged
}

function Add-UserPath([string]$Directory) {
    $target = Resolve-NormalizedPath $Directory
    $userPath = Get-UserPathRaw
    $parts = @()
    if (-not [string]::IsNullOrWhiteSpace($userPath)) {
        $parts = @($userPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    }
    $present = $false
    foreach ($part in $parts) {
        try {
            if ([string]::Equals((Resolve-NormalizedPath $part), $target,
                    [StringComparison]::OrdinalIgnoreCase)) {
                $present = $true
                break
            }
        } catch {
            # Ignore malformed existing PATH entries; do not rewrite them.
        }
    }
    if (-not $present) {
        $newPath = if ($parts.Count -gt 0) { ($parts + $Directory) -join ";" } else { $Directory }
        Set-UserPathRaw $newPath
        Write-Host "Added to the user PATH: $Directory"
    }
    $processParts = @($env:Path -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    $processPresent = $false
    foreach ($part in $processParts) {
        try {
            if ([string]::Equals((Resolve-NormalizedPath $part), $target,
                    [StringComparison]::OrdinalIgnoreCase)) {
                $processPresent = $true
                break
            }
        } catch {
        }
    }
    if (-not $processPresent) {
        $env:Path = "$Directory;$env:Path"
    }
}

function Stop-InstalledDaemon([string]$Executable) {
    if (-not (Test-Path -LiteralPath $Executable)) {
        return
    }
    try {
        & $Executable shutdown 2>$null | Out-Null
    } catch {
        # No daemon is running, or the old binary cannot be launched.
    }
    Start-Sleep -Milliseconds 500
}

function Copy-WithRetry([string]$Source, [string]$Destination) {
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        try {
            Copy-Item -LiteralPath $Source -Destination $Destination -Force
            return
        } catch {
            if ($attempt -eq 29) {
                throw "Could not replace '$Destination'. Close any running FACT UI and retry. $($_.Exception.Message)"
            }
            Start-Sleep -Milliseconds 250
        }
    }
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "FACT requires Windows."
}

if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
    throw "FACT requires Windows."
}

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$InstallDir = Resolve-NormalizedPath $InstallDir
$installedExe = Join-Path $InstallDir "fact.exe"
$tempRoot = Join-Path $env:TEMP ("fact-install-" + [Guid]::NewGuid().ToString("N"))
$tag = $null
$candidate = $null
$resolvedVersion = $null
$downloadedAsset = $null

New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null
try {
    if ($BinaryPath) {
        $candidate = (Resolve-Path -LiteralPath $BinaryPath).Path
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "Binary not found: $BinaryPath"
        }
        if ([IO.Path]::GetFileName($candidate) -ne "fact.exe") {
            throw "BinaryPath must point to fact.exe."
        }
        Write-Step "Using local binary $candidate"
        $binaryVersionOutput = (& $candidate --version).Trim()
        if ($binaryVersionOutput -notmatch '^fact\s+(.+)$') {
            throw "Could not determine the local binary version: $binaryVersionOutput"
        }
        $resolvedVersion = $Matches[1]
        $tag = "v$resolvedVersion"
        Write-Host "Local version: $resolvedVersion (checksums are not applicable to local builds)"
    } else {
        if ($Version -eq "latest") {
            Write-Step "Looking up the latest FACT release"
            $release = Get-WebText "https://api.github.com/repos/$Repo/releases/latest" |
                ConvertFrom-Json
            $tag = $release.tag_name
        } else {
            $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
        }
        if (-not $tag -or -not $tag.StartsWith("v")) {
            throw "Invalid release version: $Version"
        }
        $resolvedVersion = $tag.TrimStart("v")
        $assetName = "fact-$tag-$AssetArch.zip"
        $zipPath = Join-Path $tempRoot $assetName
        $sumsPath = Join-Path $tempRoot "SHA256SUMS.txt"

        Write-Step "Downloading FACT $tag"
        Invoke-Download "https://github.com/$Repo/releases/download/$tag/$assetName" $zipPath
        Invoke-Download "https://github.com/$Repo/releases/download/$tag/SHA256SUMS.txt" $sumsPath

        Write-Step "Verifying SHA-256 checksum"
        $sums = Get-Content -LiteralPath $sumsPath
        $line = $sums | Where-Object {
            $_ -match "\s+$([regex]::Escape($assetName))$"
        } | Select-Object -First 1
        if (-not $line) {
            throw "SHA256SUMS.txt does not contain $assetName"
        }
        $expected = ($line -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($expected -ne $actual) {
            throw "Checksum mismatch for $assetName. Expected $expected, got $actual."
        }
        Write-Host "Checksum OK: $actual"

        $extractDir = Join-Path $tempRoot "extract"
        Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force
        $found = Get-ChildItem -LiteralPath $extractDir -Filter "fact.exe" -Recurse -File |
            Select-Object -First 1
        if (-not $found) {
            throw "The release archive did not contain fact.exe."
        }
        $candidate = $found.FullName
        $reported = (& $candidate --version).Trim()
        if ($reported -ne "fact $resolvedVersion") {
            throw "Downloaded binary reported '$reported', expected 'fact $resolvedVersion'."
        }
        $downloadedAsset = $assetName
    }

    if (Test-Path -LiteralPath $installedExe) {
        $existing = ""
        try {
            $existing = (& $installedExe --version).Trim()
        } catch {
        }
        if (-not $Force -and $existing -eq "fact $resolvedVersion") {
            Write-Host "FACT $resolvedVersion is already installed at $installedExe" -ForegroundColor Green
            if (-not $NoPath) {
                Add-UserPath $InstallDir
            }
            return
        }
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null

    $replacingExisting = Test-Path -LiteralPath $installedExe
    if ($replacingExisting) {
        Write-Step "Stopping the existing FACT daemon"
        Stop-InstalledDaemon $installedExe
    }

    Write-Step "Installing to $InstallDir"
    if ((Resolve-NormalizedPath $candidate) -ne (Resolve-NormalizedPath $installedExe)) {
        Copy-WithRetry $candidate $installedExe
    }

    if (-not $NoPath) {
        Add-UserPath $InstallDir
    }

    $installedVersion = (& $installedExe --version).Trim()
    if ($installedVersion -ne "fact $resolvedVersion") {
        throw "Installed binary reported '$installedVersion', expected 'fact $resolvedVersion'."
    }

    Write-Host ""
    Write-Host "FACT $resolvedVersion installed successfully." -ForegroundColor Green
    Write-Host "  Binary: $installedExe"
    if ($downloadedAsset) {
        Write-Host "  Verified asset: $downloadedAsset"
    } else {
        Write-Host "  Source: local binary (no release checksum)"
    }
    if ($NoPath) {
        Write-Host "  PATH: unchanged (-NoPath). Run the full path or add it yourself."
    } else {
        Write-Host "  PATH: user PATH updated. Open a new terminal to use 'fact' from Explorer."
    }
    Write-Host "  Next: fact doctor; fact --grid 4"
    if ($replacingExisting) {
        Write-Host "  Replaced an existing install; its daemon was asked to stop and saved sessions cold-recover on next launch."
    }
} finally {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
