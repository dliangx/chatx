#!/usr/bin/env bash
# Build the X mini-program(s) in x/ to WASM artifacts under x-t/.
#
#   x-t/<app_id>/<version>/app.wasm      (wasm32-wasip1)
#   x-t/<app_id>/<version>/manifest.json (name/extension/permissions/sha256)
#
# P0 builds the `hello` smoke plugin only; the five document plugins (P1–P3)
# get added here as their dirs land. Signatures (ed25519) are P4 (x/x-m).

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
X="$ROOT/x"
OUT_ROOT="$ROOT/x-t"

# --- app registry: <name>  ->  <app_id>  (cargo package  ->  x-t layout) ---
# Keep this list in sync with x/README.md §“The five built-in plugins”.
build_app() {
    local pkg="$1" app_id="$2" version="${3:-0.1.0}"
    echo "==> $pkg ($app_id@$version)"
    local wasm
    wasm=$(cd "$X" && cargo build --release --target wasm32-wasip1 -p "$pkg" 2>&1 | tail -n0)
    local artifact="$X/target/wasm32-wasip1/release/${pkg}.wasm"
    [ -f "$artifact" ] || { echo "missing: $artifact (build output):\n${wasm}"; exit 1; }

    local out="$OUT_ROOT/$app_id/$version"
    mkdir -p "$out"
    cp "$artifact" "$out/app.wasm"

    # assets/ (icons etc.) — copy if the package ships them.
    if [ -d "$X/$pkg/assets" ]; then
        mkdir -p "$out/assets"
        cp -R "$X/$pkg/assets/." "$out/assets/"
    fi

    # sha256 of the artifact (verification chain: P4 x/x-m + wasmhost/policy.rs)
    local sha
    if command -v shasum >/dev/null 2>&1; then
        sha=$(shasum -a 256 "$out/app.wasm" | awk '{print $1}')
    else
        sha=$(sha256sum "$out/app.wasm" | awk '{print $1}')
    fi

    cat > "$out/manifest.json" <<EOF
{
  "id": "$app_id",
  "name": "$app_id",
  "version": "$version",
  "extensions": [],
  "permissions": [],
  "max_memory": "256MiB",
  "sha256": "$sha"
}
EOF
    echo "    -> $out/app.wasm ($(wc -c < "$out/app.wasm" | tr -d ' ') bytes, sha256 ${sha:0:12}…)"
}

rustup target add wasm32-wasip1 >/dev/null 2>&1 || true

# P0: hello only.
build_app hello x.hello 0.1.0

echo "done: $OUT_ROOT"
