# build_windows.ps1 — Chatx Windows desktop build & packaging (native PowerShell).
#
# Runs on a Windows host under Windows PowerShell 5.1+ or PowerShell 7.
#   * x86_64-pc-windows-gnu  (default) — needs MinGW-w64 (gcc) on PATH
#   * x86_64-pc-windows-msvc          — needs Visual Studio C++ Build Tools
#
# Produces:  build\windows\chatx-x64-<mode>-windows.zip
#            containing  chatx.exe + chatx.bat (double-click launcher)
#
# Usage:
#   .\scripts\build_windows.ps1                    # Release (default), windows-gnu
#   .\scripts\build_windows.ps1 -Debug             # Debug build (fast iteration)
#   .\scripts\build_windows.ps1 -Msvc              # MSVC toolchain instead of MinGW
#   .\scripts\build_windows.ps1 -Clean             # wipe the target dir first
#
# Pre-reqs:
#   rust, rustup, and one C toolchain:
#     - MinGW-w64 gcc on PATH      (scoop install mingw / choco install mingw)
#     - or VS C++ Build Tools       (for -Msvc, with link.exe reachable)
#     Windows 10+ ships Compress-Archive, so no extra zip tool is needed.

[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Debug,
    [switch]$Msvc,
    [switch]$Clean,
    [string]$Target = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ── paths ─────────────────────────────────────────────────────────────────────
$Root   = Split-Path -Parent $PSScriptRoot
$OutDir = Join-Path $Root "build\windows"

if ($Target -eq "") {
    if ($Msvc) { $Target = "x86_64-pc-windows-msvc" }
    else       { $Target = "x86_64-pc-windows-gnu" }
}
$Mode = if ($Debug) { "debug" } else { "release" }
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

# ── C toolchain check ──────────────────────────────────────────────────────────
if ($Target -like "*msvc") {
    if (CmdOnPath "link") {
        Ok "MSVC linker found on PATH (link.exe)"
    }
    else {
        Err @"
MSVC target selected but 'link.exe' is not on PATH.
  Options:
    1) Re-run from a "Developer PowerShell for VS" (or open VS and run).
    2) Install "C++ build tools" from Visual Studio Installer.
    3) Or drop -Msvc and use MinGW instead:   .\scripts\build_windows.ps1
"@
    }
}
else {
    # MinGW (gnu) path: need gcc.exe. Prefer x86_64-w64-mingw32-gcc if present,
    # else require plain gcc.exe (the 64-bit one) on PATH.
    if (CmdOnPath "x86_64-w64-mingw32-gcc") {
        $env:CC  = "x86_64-w64-mingw32-gcc"
        $env:AR  = "x86_64-w64-mingw32-ar"
        $env:CXX = "x86_64-w64-mingw32-g++"
        Ok "using MinGW: $env:CC  $env:CXX"
    }
    elseif (CmdOnPath "gcc.exe") {
        Ok "using host gcc.exe on PATH"
    }
    else {
        Err @"
No MinGW-w64 C compiler found on PATH.  Install one then re-run:

    scoop install mingw            (scoop)
    choco install mingw            (choco)
    winget install msys2           (msys2, then  'pacman -S mingw-w64-x86_64-gcc')

and make sure  gcc.exe / g++.exe / ar.exe  are on PATH.
"@
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

# ── package ────────────────────────────────────────────────────────────────────
Say "packaging -> $OutDir"
if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Force $OutDir | Out-Null

Copy-Item $Bin (Join-Path $OutDir "chatx.exe")

# Double-click launcher.
$bat = @(
    '@echo off',
    'title Chatx',
    'cd /d "%~dp0"',
    'chatx.exe %*',
    'pause'
)
Set-Content -Path (Join-Path $OutDir "chatx.bat") -Value $bat -Encoding ASCII

$Zip = Join-Path $OutDir ("chatx-x64-{0}-windows.zip" -f $Mode)
$items = @(
    (Join-Path $OutDir "chatx.exe"),
    (Join-Path $OutDir "chatx.bat")
)
Compress-Archive -Path $items -DestinationPath $Zip -Force
Ok "zip:  $Zip  ($([math]::Round((Get-Item $Zip).Length / 1KB,0)) KB)"

Write-Host ""
Write-Host "──── done ────"
Write-Host ""
Write-Host ("OK.  Extract the zip, run  chatx.exe   (or double-click chatx.bat).") -ForegroundColor Green
