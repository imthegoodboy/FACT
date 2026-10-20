# Offline regression test for the installer's release-download path.
#
# This does not touch the network or the user's real PATH. It builds a fake
# release archive from a local fact.exe, mocks Invoke-WebRequest, and exercises
# pinned, normalized and latest-version installs.
#
#   .\scripts\test-install.ps1 -FactExe .\target\debug\fact.exe

[CmdletBinding()]
param(
    [string]$FactExe = (Join-Path $PSScriptRoot "..\target\debug\fact.exe")
)

$ErrorActionPreference = "Stop"
$installer = Join-Path $PSScriptRoot "install.ps1"
if (-not (Test-Path -LiteralPath $FactExe -PathType Leaf)) {
    throw "fact.exe not found: $FactExe"
}
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "installer not found: $installer"
}
$FactExe = (Resolve-Path -LiteralPath $FactExe).Path

$fixtureRoot = Join-Path $env:TEMP ("fact-install-test-" + [Guid]::NewGuid().ToString("N"))
$fixtureStage = Join-Path $fixtureRoot "stage"
$fixtureAssetName = "fact-v0.1.0-x86_64-pc-windows-msvc.zip"
$fixtureZip = Join-Path $fixtureRoot $fixtureAssetName
$fixtureSums = Join-Path $fixtureRoot "SHA256SUMS.txt"
$global:FactInstallTestRequests = [System.Collections.Generic.List[string]]::new()

function Invoke-WebRequest {
    [CmdletBinding()]
    param(
        [string]$Uri,
        [string]$OutFile,
        [switch]$UseBasicParsing,
        [hashtable]$Headers
    )

    $global:FactInstallTestRequests.Add($Uri)
    if ($Uri -match '/releases/latest$') {
        return [pscustomobject]@{
            Content = '{"tag_name":"v0.1.0"}'
            StatusCode = 200
        }
    }
    if ($Uri -match '/releases/download/([^/]+)/([^/]+)$') {
        $tag = $Matches[1]
        $name = $Matches[2]
        if ($tag -ne "v0.1.0") {
            throw "Installer requested an unexpected release tag '$tag' from $Uri"
        }
        if ($name -eq "SHA256SUMS.txt") {
            Copy-Item -LiteralPath $fixtureSums -Destination $OutFile
            return
        }
        if ($name -eq $fixtureAssetName) {
            Copy-Item -LiteralPath $fixtureZip -Destination $OutFile
            return
        }
    }
    throw "Installer requested an unexpected URL: $Uri"
}

function Install-Case {
    param(
        [string]$Label,
        [string]$RequestedVersion
    )
    $installDir = Join-Path $fixtureRoot "install-$Label"
    $parameters = @{
        InstallDir = $installDir
        NoPath = $true
        Force = $true
    }
    if ($RequestedVersion) {
        $parameters["Version"] = $RequestedVersion
    }
    & $installer @parameters
    $installedExe = Join-Path $installDir "fact.exe"
    if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) {
        throw "Install case '$Label' did not create fact.exe"
    }
    $reported = (& $installedExe --version).Trim()
    if ($reported -ne "fact 0.1.0") {
        throw "Install case '$Label' reported '$reported', expected 'fact 0.1.0'"
    }
}

try {
    New-Item -ItemType Directory -Path $fixtureStage -Force | Out-Null
    Copy-Item -LiteralPath $FactExe -Destination (Join-Path $fixtureStage "fact.exe")
    Compress-Archive -LiteralPath (Join-Path $fixtureStage "fact.exe") -DestinationPath $fixtureZip -Force
    $hash = (Get-FileHash -LiteralPath $fixtureZip -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath $fixtureSums -Value "$hash  $fixtureAssetName"

    Install-Case -Label "pinned" -RequestedVersion "v0.1.0"
    Install-Case -Label "normalized" -RequestedVersion "0.1.0"
    Install-Case -Label "latest"

    $expected = @(
        "https://github.com/imthegoodboy/FACT/releases/download/v0.1.0/$fixtureAssetName",
        "https://github.com/imthegoodboy/FACT/releases/download/v0.1.0/SHA256SUMS.txt"
    )
    foreach ($uri in $expected) {
        if (-not $global:FactInstallTestRequests.Contains($uri)) {
            throw "Installer never requested $uri"
        }
    }
    if (-not ($global:FactInstallTestRequests | Where-Object { $_ -match '/releases/latest$' })) {
        throw "The latest-version install did not query the latest release"
    }

    Write-Output "INSTALLER_OFFLINE_TEST_OK"
} finally {
    Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}

