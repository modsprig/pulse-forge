# PulseForge Windows installer — per-user, no elevation, no admin.
#
# Installs the release binaries into %LOCALAPPDATA%\Programs\PulseForge,
# adds that directory to the *user* PATH (new terminals only), and creates
# a Start-Menu shortcut for the GUI. Idempotent: re-running refreshes the
# binaries. `-Uninstall` removes everything this script created.
#
# Usage (from the unzipped release archive):
#   powershell -ExecutionPolicy Bypass -File install.ps1
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
#
# Test-friendly overrides (not needed for normal installs):
#   -InstallDir <path>   install somewhere else
#   -NoPath              do not touch the user PATH
#   -NoShortcut          do not create the Start-Menu shortcut
param(
    [switch]$Uninstall,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\PulseForge"),
    [switch]$NoPath,
    [switch]$NoShortcut
)
$ErrorActionPreference = "Stop"

$Cli = Join-Path $InstallDir "pulseforge.exe"
$Gui = Join-Path $InstallDir "pulseforge-gui.exe"

if ($Uninstall) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $kept = ($userPath -split ";" | Where-Object { $_ -and $_ -ne $InstallDir }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $kept, "User")
    $lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "PulseForge.lnk"
    if (Test-Path $lnk) { Remove-Item $lnk -Force }
    if (Test-Path $InstallDir) { Remove-Item -Recurse -Force $InstallDir }
    Write-Host "PulseForge uninstalled (PATH entry, shortcut, binaries)."
    exit 0
}

# Locate the payload: next to this script, or one directory below it
# (the release zip extracts into pulseforge-<version>-windows-x64/).
$payload = $PSScriptRoot
if (-not $payload) { $payload = (Get-Location).Path }
if (-not (Test-Path (Join-Path $payload "pulseforge.exe"))) {
    $candidate = Get-ChildItem $payload -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName "pulseforge.exe") } |
        Select-Object -First 1
    if ($candidate) {
        $payload = $candidate.FullName
    } else {
        throw "pulseforge.exe not found next to install.ps1 - unzip the release archive first."
    }
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item (Join-Path $payload "pulseforge.exe") $InstallDir -Force
if (Test-Path (Join-Path $payload "pulseforge-gui.exe")) {
    Copy-Item (Join-Path $payload "pulseforge-gui.exe") $InstallDir -Force
}
if (Test-Path (Join-Path $payload "docs")) {
    Copy-Item (Join-Path $payload "docs") $InstallDir -Recurse -Force
}

if (-not $NoPath) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $InstallDir) {
        $updated = $userPath.TrimEnd(";") + ";" + $InstallDir
        [Environment]::SetEnvironmentVariable("Path", $updated, "User")
        Write-Host "Added $InstallDir to your user PATH (takes effect in NEW terminals)."
    }
}

if (-not $NoShortcut) {
    if (Test-Path $Gui) {
        $programs = [Environment]::GetFolderPath("Programs")
        $shell = New-Object -ComObject WScript.Shell
        $lnk = $shell.CreateShortcut((Join-Path $programs "PulseForge.lnk"))
        $lnk.TargetPath = $Gui
        $lnk.WorkingDirectory = $InstallDir
        $lnk.Save()
    }
}

# Verify the installed CLI actually runs.
& $Cli version
Write-Host ""
Write-Host "PulseForge installed: $InstallDir"
Write-Host "Next steps (in a NEW terminal): pulseforge capabilities"
Write-Host "Uninstall: powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall"
