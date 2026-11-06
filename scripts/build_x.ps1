# build_x.ps1 — Build the X mini-program(s) in x/ to WASM artifacts under x-t/.
#
#   x-t/<app_id>/<version>/app.wasm      (wasm32-wasip1)
#   x-t/<app_id>/<version>/manifest.json (name/extension/permissions/sha256)
#
# P0 builds the `hello` smoke plugin only; the five document plugins (P1–P3)
# get added here as their dirs land. Signatures (ed25519) are P4 (x/x-m).
#
# Usage:
#   .\scripts\build_x.ps1

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$Root  = Split-Path -Parent $PSScriptRoot
$X     = Join-Path $Root "x"
$OutRoot = Join-Path $Root "x-t"

function Say($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Ok($m)  { Write-Host "    $m" -ForegroundColor Green }
function Err($m) { Write-Host "    $m" -ForegroundColor Red; exit 1 }

function Build-App {
    param(
        [string]$Pkg,
        [string]$AppId,
        [string]$Version = "0.1.0"
    )

    Say "$Pkg ($AppId@$Version)"

    $wasmLog = ""
    try {
        Push-Location $X
        $prevErr = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        $wasmLog = cargo build --release --target wasm32-wasip1 -p $Pkg 2>&1
        $ErrorActionPreference = $prevErr
        if ($LASTEXITCODE -ne 0) {
            Pop-Location
            Err "cargo build failed:`n$wasmLog"
        }
        Pop-Location
    } catch {
        if ($X -and (Test-Path $X)) { Pop-Location }
        Err "cargo build failed: $_"
    }

    $artifact = Join-Path $X "target\wasm32-wasip1\release\${Pkg}.wasm"
    if (-not (Test-Path $artifact)) {
        Err "missing: $artifact (build output):`n$wasmLog"
    }

    $out = Join-Path $OutRoot "$AppId\$Version"
    New-Item -ItemType Directory -Force $out | Out-Null
    Copy-Item $artifact (Join-Path $out "app.wasm") -Force

    # assets/ (icons etc.) — copy if the package ships them.
    $assetsSrc = Join-Path $X "$Pkg\assets"
    if (Test-Path $assetsSrc) {
        $assetsDst = Join-Path $out "assets"
        New-Item -ItemType Directory -Force $assetsDst | Out-Null
        Copy-Item -Recurse -Force (Join-Path $assetsSrc "*") $assetsDst
    }

    # sha256 of the artifact
    $hash = (Get-FileHash (Join-Path $out "app.wasm") -Algorithm SHA256).Hash.ToLower()

    $manifest = @"
{
  "id": "$AppId",
  "name": "$AppId",
  "version": "$Version",
  "extensions": [],
  "permissions": [],
  "max_memory": "256MiB",
  "sha256": "$hash"
}
"@
    # Write manifest as UTF8 without BOM
    $manifestPath = Join-Path $out "manifest.json"
    [System.IO.File]::WriteAllText($manifestPath, $manifest)

    $size = (Get-Item (Join-Path $out "app.wasm")).Length
    Ok " -> $out\app.wasm ($size bytes, sha256 $($hash.Substring(0, 12))...)"
}

# rustup target
$prevErrorAction = $ErrorActionPreference
$ErrorActionPreference = "Continue"
rustup target add wasm32-wasip1 2>$null
$ErrorActionPreference = $prevErrorAction
if ($LASTEXITCODE -ne 0) {
    Err "Failed to add wasm32-wasip1 target"
}

# P0: hello only.
Build-App -Pkg "hello" -AppId "x.hello" -Version "0.1.0"

Write-Host ""
Ok "done: $OutRoot"
