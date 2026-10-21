# FACT per-user uninstaller.
#
# By default this removes the program and its user PATH entry. The optional
# configuration in %LOCALAPPDATA%\FACT is kept.
#
# Remove configuration as well (irreversible):
#   .\scripts\uninstall.ps1 -PurgeConfig

[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\FACT"),
    [Alias("PurgeData")]
    [switch]$PurgeConfig
)

$ErrorActionPreference = "Stop"

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
        # New logons will still see the registry update.
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

function Remove-UserPath([string]$Directory) {
    $target = Resolve-NormalizedPath $Directory
    $userPath = Get-UserPathRaw
    if ([string]::IsNullOrWhiteSpace($userPath)) {
        return
    }
    $kept = @()
    $removed = $false
    foreach ($part in ($userPath -split ";")) {
        if ([string]::IsNullOrWhiteSpace($part)) {
            continue
        }
        try {
            if ([string]::Equals((Resolve-NormalizedPath $part), $target,
                    [StringComparison]::OrdinalIgnoreCase)) {
                $removed = $true
                continue
            }
        } catch {
        }
        $kept += $part
    }
    if ($removed) {
        Set-UserPathRaw ($kept -join ";")
        Write-Host "Removed from the user PATH: $Directory"
    }
    $processKept = @()
    foreach ($part in ($env:Path -split ";")) {
        if ([string]::IsNullOrWhiteSpace($part)) {
            continue
        }
        try {
            if ([string]::Equals((Resolve-NormalizedPath $part), $target,
                    [StringComparison]::OrdinalIgnoreCase)) {
                continue
            }
        } catch {
        }
        $processKept += $part
    }
    $env:Path = $processKept -join ";"
}

$InstallDir = Resolve-NormalizedPath $InstallDir
$exe = Join-Path $InstallDir "fact.exe"

if (Test-Path -LiteralPath $exe) {
    try {
        Remove-Item -LiteralPath $exe -Force
        Write-Host "Removed $exe"
    } catch {
        throw "Could not remove '$exe'. Close any running FACT UI and retry. $($_.Exception.Message)"
    }
} else {
    Write-Host "No installed binary found at $exe"
}

Remove-UserPath $InstallDir

if (Test-Path -LiteralPath $InstallDir) {
    $remaining = @(Get-ChildItem -LiteralPath $InstallDir -Force)
    if ($remaining.Count -eq 0) {
        Remove-Item -LiteralPath $InstallDir -Force
    } else {
        Write-Host "Kept non-FACT files in $InstallDir"
    }
}

if ($PurgeConfig) {
    $target = if ($env:FACT_HOME) {
        $env:FACT_HOME
    } else {
        Join-Path $env:LOCALAPPDATA "FACT"
    }
    if (Test-Path -LiteralPath $target) {
        if (-not (Test-Path -LiteralPath $target -PathType Container)) {
            throw "Refusing to purge '$target': it is not a directory."
        }
        $resolved = Resolve-NormalizedPath $target
        $root = [IO.Path]::GetPathRoot($resolved).TrimEnd('\')
        $profile = Resolve-NormalizedPath $env:USERPROFILE
        if ($resolved -eq $root -or $resolved -eq $profile) {
            throw "Refusing to purge unsafe configuration path: $resolved"
        }
        if ($profile.StartsWith("$resolved\", [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to purge an ancestor of the user profile: $resolved"
        }
        $children = @(Get-ChildItem -LiteralPath $resolved -Force)
        if ($children.Count -gt 0) {
            $markers = @("config.toml")
            $looksLikeFact = $false
            foreach ($marker in $markers) {
                if (Test-Path -LiteralPath (Join-Path $resolved $marker)) {
                    $looksLikeFact = $true
                    break
                }
            }
            if (-not $looksLikeFact) {
                throw "Refusing to purge '$resolved': it has no FACT configuration markers. Remove it manually if this is intentional."
            }
        }
        Remove-Item -LiteralPath $resolved -Recurse -Force
        Write-Host "Purged FACT configuration: $resolved" -ForegroundColor Yellow
    } else {
        Write-Host "No FACT configuration found at $target"
    }
} else {
    $configRoot = if ($env:FACT_HOME) { $env:FACT_HOME } else {
        Join-Path $env:LOCALAPPDATA "FACT"
    }
    Write-Host "Configuration kept at $configRoot (use -PurgeConfig to remove)."
}

Write-Host "Uninstall complete."
