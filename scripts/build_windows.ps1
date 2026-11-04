# build_windows.ps1 — Chatx Windows desktop build & packaging (native PowerShell).
#
# Runs on a Windows host under Windows PowerShell 5.1+ or PowerShell 7.
# Target: x86_64-pc-windows-msvc (auto-detects VS C++ Build Tools via vswhere)
#
# Produces:  build\windows\chatx-x64-<mode>-windows.zip
#            containing  chatx.exe (GUI app with embedded icon, double-click to launch)
#
# Usage:
#   .\scripts\build_windows.ps1                    # Release (default)
#   .\scripts\build_windows.ps1 -Dev               # Debug build (fast iteration)
#   .\scripts\build_windows.ps1 -Clean             # wipe the target dir first
#
# Pre-reqs:
#   rust, rustup, and VS C++ Build Tools (auto-detected via vswhere)
#   Windows 10+ ships Compress-Archive, so no extra zip tool is needed.
#   Icon is embedded at compile time via winres (app-icon.ico).

[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Dev,
    [switch]$Clean,
    [string]$Target = "x86_64-pc-windows-msvc"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ── paths ─────────────────────────────────────────────────────────────────────
$Root   = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $Root "target\windows"

$Mode = if ($Dev) { "debug" } else { "release" }
if ($Mode -eq "release") { $Release = $true } else { $Release = $false }

function Say($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Ok($m)  { Write-Host "    $m" -ForegroundColor Green }
function Err($m) { $m | ForEach-Object { Write-Host $_ -ForegroundColor Red }; exit 1 }

Write-Host ""
Write-Host ("──── chatx Windows build  (target = {0}, {1}) ────" -f $Target, $Mode)

# ── toolchain sanity ───────────────────────────────────────────────────────────
function CmdOnPath($name) { return [bool](Get-Command -Name $name -ErrorAction SilentlyContinue) }

if (-not (CmdOnPath "cargo"))  { Err "cargo not found on PATH. Install Rust: https://rustup.rs" }
if (-not (CmdOnPath "rustup")) { Err "rustup not found on PATH. Install Rust: https://rustup.rs" }

# ── rustup target ──────────────────────────────────────────────────────────────
$installed = (rustup target list --installed) -split "`n" | ForEach-Object { $_.Trim() }
if ($installed -notcontains $Target) {
    Say "rustup target add $Target"
    rustup target add $Target | Out-Null
    if ($LASTEXITCODE -ne 0) { Err "rustup target add failed for $Target" }
}

# ── C toolchain check (MSVC) ───────────────────────────────────────────────────
if (CmdOnPath "link") {
    Ok "MSVC linker found on PATH (link.exe)"
}
else {
    # Auto-detect VS via vswhere and import vcvars64 environment.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        Err @"
'link.exe' is not on PATH and vswhere.exe was not found.
  Install "C++ build tools" from Visual Studio Installer.
"@
    }
    $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $vsPath) {
        Err @"
No Visual Studio instance with C++ Build Tools (VC.Tools.x86.x64) found.
  Install "C++ build tools" from Visual Studio Installer.
"@
    }
    $vcvars = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"
    if (-not (Test-Path $vcvars)) {
        Err "vcvars64.bat not found at: $vcvars"
    }
    Say "importing MSVC environment from: $vcvars"

    # Run vcvars64.bat in a child cmd, capture the resulting env, import into PowerShell.
    $cmdOut = cmd /c "call `"$vcvars`" && set" 2>$null
    foreach ($line in $cmdOut) {
        if ($line -match "^([^=]+)=(.*)$") {
            $envName  = $Matches[1]
            $envValue = $Matches[2]
            if ($envName -eq "PATH") {
                $env:PATH = $envValue + ";" + $env:PATH
            }
            else {
                Set-Item -Path "env:$envName" -Value $envValue
            }
        }
    }
    if (CmdOnPath "link") {
        Ok "MSVC environment imported (link.exe now on PATH)"
    }
    else {
        Err "Failed to import MSVC environment — link.exe still not found after vcvars64."
    }
}

# ── clean (optional) ───────────────────────────────────────────────────────────
if ($Clean) {
    $td = Join-Path $Root "target\$Target"
    Say "wiping $td"
    if (Test-Path $td) { Remove-Item -Recurse -Force $td }
}

# ── cargo build ────────────────────────────────────────────────────────────────
if ($Release) {
    Say "cargo build -p chatx --target $Target --release"
    cargo build -p chatx --target $Target --release
}
else {
    Say "cargo build -p chatx --target $Target (debug)"
    cargo build -p chatx --target $Target
}
if ($LASTEXITCODE -ne 0) { Err "cargo build failed (exit $LASTEXITCODE)" }

$Bin = Join-Path $Root "target\$Target\$Mode\chatx.exe"
if (-not (Test-Path $Bin)) { Err "binary not found: $Bin" }
Ok "built:  $Bin  ($([math]::Round((Get-Item $Bin).Length / 1MB,1)) MB)"

# ── X mini-app artifacts (x-t/) — same self-contained approach as build_osx ──
# Built via scripts/build_x.sh (wasm32-wasip1); placed next to chatx.exe so the
# X extension page works after extraction. Try a bash; fall back to a prebuilt.
$XBuild = Join-Path $Root "scripts\build_x.sh"
$XWasm = Join-Path $Root "x-t\x.hello\0.1.0\app.wasm"
if (Test-Path $XBuild) {
    $bash = if (Get-Command bash -ErrorAction SilentlyContinue) { "bash" } else { $null }
    if ($bash) {
        Say "building X mini-app (x-t/)"
        Push-Location $Root
        & $bash "scripts/build_x.sh"
        Pop-Location
        if ($LASTEXITCODE -ne 0) { Write-Host "   (build_x.sh failed — X page will be unavailable)" -ForegroundColor Yellow }
    } else {
        Write-Host "   (no bash found to run build_x.sh — relying on a prebuilt x-t/)" -ForegroundColor Yellow
    }
}

# ── package ────────────────────────────────────────────────────────────────────
# Kill any running chatx.exe so we can overwrite the packaged copy.
Get-Process chatx -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

Say "packaging -> $OutDir"
New-Item -ItemType Directory -Force $OutDir | Out-Null

# Overwrite in place (avoids "directory in use" errors when the folder is open in Explorer).
Copy-Item $Bin (Join-Path $OutDir "chatx.exe") -Force

$XDir = Join-Path $Root "x-t"
if (Test-Path $XDir) {
    Remove-Item -Recurse -Force (Join-Path $OutDir "x-t") -ErrorAction SilentlyContinue
    Copy-Item -Recurse -Force $XDir (Join-Path $OutDir "x-t")
    Ok "bundled x-t/ into the package"
} else {
    Write-Host "   (x-t/x.hello artifact absent — X extension page will report missing wasm)" -ForegroundColor Yellow
}

$Zip = Join-Path $OutDir ("chatx-x64-{0}-windows.zip" -f $Mode)
$zipEntries = @( (Join-Path $OutDir "chatx.exe") )
if (Test-Path (Join-Path $OutDir "x-t")) { $zipEntries += (Join-Path $OutDir "x-t") }
Compress-Archive -Path $zipEntries -DestinationPath $Zip -Force
Ok "zip:  $Zip  ($([math]::Round((Get-Item $Zip).Length / 1KB,0)) KB)"

Write-Host ""
Write-Host "──── done ────"
Write-Host ""
Write-Host "OK.  Extract the zip, double-click  chatx.exe  to launch." -ForegroundColor Green
