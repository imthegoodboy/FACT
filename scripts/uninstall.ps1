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

function Remove-UserPath([string]$Directory) {
    $target = Resolve-NormalizedPath $Directory
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
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
        [Environment]::SetEnvironmentVariable("Path", ($kept -join ";"), "User")
        Write-Host "Removed from the user PATH: $Directory"
    }
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
