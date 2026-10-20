# FACT per-user uninstaller.
#
# By default this removes the program and its user PATH entry. Sessions,
# configuration and logs in %LOCALAPPDATA%\FACT are kept.
#
# Remove data as well (irreversible):
#   .\scripts\uninstall.ps1 -PurgeData

[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\FACT"),
    [string]$DataDir,
    [switch]$PurgeData
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
    Write-Host "Stopping FACT daemon..."
    try {
        & $exe shutdown 2>$null | Out-Null
    } catch {
    }
    Start-Sleep -Milliseconds 500
}

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

if ($PurgeData) {
    $target = if ($DataDir) {
        $DataDir
    } elseif ($env:FACT_HOME) {
        $env:FACT_HOME
    } else {
        Join-Path $env:LOCALAPPDATA "FACT"
    }
    if (Test-Path -LiteralPath $target) {
        $resolved = Resolve-NormalizedPath $target
        $root = [IO.Path]::GetPathRoot($resolved).TrimEnd('\')
        if ($resolved -eq $root -or $resolved -eq (Resolve-NormalizedPath $env:USERPROFILE)) {
            throw "Refusing to purge unsafe data path: $resolved"
        }
        Remove-Item -LiteralPath $resolved -Recurse -Force
        Write-Host "Purged FACT data: $resolved" -ForegroundColor Yellow
    } else {
        Write-Host "No FACT data found at $target"
    }
} else {
    $data = if ($DataDir) { $DataDir } elseif ($env:FACT_HOME) { $env:FACT_HOME } else {
        Join-Path $env:LOCALAPPDATA "FACT"
    }
    Write-Host "Sessions and configuration kept at $data (use -PurgeData to remove)."
}

Write-Host "Uninstall complete."
