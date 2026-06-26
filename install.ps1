# PocketVeto Windows install script (PowerShell).
#
# Downloads the release .exe from GitHub Releases, puts it in a PATH
# directory, and runs `pocket-veto init`.
#
# Usage:
#   irm https://github.com/pocket-veto/pocket-veto/releases/latest/download/install.ps1 | iex
#
# Or to skip interactive init:
#   & install.ps1 -SkipBt

[CmdletBinding()]
param(
    [switch]$SkipBt,
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\pocket-veto"
)

$ErrorActionPreference = "Stop"

$Repo = "pocket-veto/pocket-veto"
$BinName = "pocket-veto.exe"
$Asset = "pocket-veto-x86_64-pc-windows-msvc.exe"
$Url = "https://github.com/$Repo/releases/latest/download/$Asset"

Write-Host "Downloading $Url"
try {
    Invoke-WebRequest -Uri $Url -OutFile "$env:TEMP\$Asset" -UseBasicParsing
} catch {
    Write-Error "install.ps1: download failed for $Url`: $_"
    exit 1
}

if (-not (Test-Path "$env:TEMP\$Asset")) {
    Write-Error "install.ps1: download did not produce $Asset"
    exit 1
}

# Install to a per-user directory (no admin needed) and add it to PATH.
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
}

$Dest = Join-Path $InstallDir $BinName
Move-Item -Force "$env:TEMP\$Asset" $Dest

# Add to user PATH if not already present.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
    Write-Host "Added $InstallDir to your user PATH. Open a new terminal for it to take effect."
}

Write-Host "Installed $BinName to $InstallDir"

# Run init, passing through the skip-bt flag if set.
if ($SkipBt) {
    & $Dest init --skip-bt
} else {
    & $Dest init
}
