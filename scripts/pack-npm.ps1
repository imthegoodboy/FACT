<#
.SYNOPSIS
  Build the fact-terminal npm package from a release fact.exe.

.DESCRIPTION
  Copies the binary and LICENSE into npm\ (next to its own README), sets the package version
  from Cargo.toml, checks that the binary reports the same version and runs
  `npm pack`. The tarball lands in -OutDir. Publishing stays a separate,
  explicit step:

    npm publish .\dist\fact-terminal-X.Y.Z.tgz --access public

.EXAMPLE
  cargo build --release --locked --bin fact
  .\scripts\pack-npm.ps1
#>
[CmdletBinding()]
param(
    [string]$BinaryPath = "target\release\fact.exe",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $line = Select-String -Path Cargo.toml -Pattern '^version = "([^"]+)"$' | Select-Object -First 1
    if (-not $line) { throw "Could not read the workspace version from Cargo.toml" }
    $version = $line.Matches[0].Groups[1].Value

    $binary = Resolve-Path $BinaryPath
    $reported = (& $binary --version).Trim()
    if ($reported -ne "fact $version") {
        throw "$binary reports '$reported' but Cargo.toml is $version; rebuild it first"
    }

    $package = Join-Path $root "npm"
    $bin = Join-Path $package "bin"
    New-Item -ItemType Directory -Force $bin | Out-Null
    Copy-Item $binary (Join-Path $bin "fact.exe") -Force
    Copy-Item (Join-Path $root "LICENSE") (Join-Path $package "LICENSE") -Force

    $manifestPath = Join-Path $package "package.json"
    $manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json
    if ($manifest.version -ne $version) {
        $manifest.version = $version
        $json = ($manifest | ConvertTo-Json -Depth 10) + "`n"
        [System.IO.File]::WriteAllText($manifestPath, $json, (New-Object System.Text.UTF8Encoding $false))
        Write-Host "package.json version set to $version"
    }

    New-Item -ItemType Directory -Force $OutDir | Out-Null
    $destination = Resolve-Path $OutDir
    Push-Location $package
    try {
        npm pack --pack-destination $destination | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "npm pack failed" }
    } finally {
        Pop-Location
    }
    $tarball = Join-Path $destination "fact-terminal-$version.tgz"
    if (-not (Test-Path $tarball)) { throw "Expected $tarball" }
    Write-Host "Packed $tarball"
} finally {
    Pop-Location
}
