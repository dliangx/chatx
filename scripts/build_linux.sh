#!/usr/bin/env bash
# Chatx — Linux desktop build & packaging.
#
# Native to Linux: it builds against the host toolchain + system GUI libraries
# (slint → wayland/X11 → fontconfig, xkbcommon, gbm, drm, libseat, pipewire,
# alsa, zbus). There is no macOS/Windows cross path here — cross-compiling a
# full GUI stack needs a complete per-target Linux sysroot + pkg-config and a
# dozen shared libs at runtime, which is fragile and not worth maintaining.
#
#   Linux host (native, any arch)      →  ./scripts/build_linux.sh
#   x86_64 Linux CI building an ARM64  →  CX_ARCH=aarch64 ./scripts/build_linux.sh
#                                         (host must have the cross gcc + sysroot)
#   macOS / Windows                    →  build inside a Linux container/VM, e.g.:
#       docker run --rm -it -v "$PWD":/w -w /w \
#         rust:1-x86_64 bash -lc "apt-get update && apt-get install -y \
#         pkg-config libxcb1-dev libx11-dev libxrandr-dev libxkbcommon-dev \
#         libwayland-dev libpixman-1-dev libfontconfig1-dev libegl-dev \
#         libgbm-dev libasound2-dev libpipewire-0.3-dev \
#         && cargo build -p chatx --release && cp target/release/chatx ."
#
# Produces `chatx` + a portable `chatx-<arch>-linux-<mode>.tar.gz` (glibc) in
# `build/linux/`. For a static build, install the matching `*-musl` dev
# packages and run with `LIBC=musl ./scripts/build_linux.sh`.
#
# Usage:
#   ./scripts/build_linux.sh                 # Release, host-arch target
#   ./scripts/build_linux.sh --debug         # Debug build
#   ./scripts/build_linux.sh --clean         # Wipe cargo artifacts for target first
#   CX_ARCH=aarch64 ./scripts/build_linux.sh # Cross arch on a Linux that has the toolchain
#
# Set CC / CXX / AR in the environment to force a specific compiler.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RELEASE="release"
CLEAN=0
OUT_DIR="$ROOT/build/linux"

while [ $# -gt 0 ]; do
  case "$1" in
    --release)  RELEASE="release" ;;
    --debug)    RELEASE="" ;;
    --clean)    CLEAN=1 ;;
    -h|--help)  grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
    *)          echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

MODE="${RELEASE:-debug}"
LIBC="${LIBC:-gnu}"   # override with LIBC=musl for a static build (needs musl dev pkgs)
# Map `uname -m` (arm64/x86_64/aarch64) to a Rust target arch.
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  x86_64|amd64)   RUST_ARCH="x86_64"   ;;
  arm64|aarch64)  RUST_ARCH="aarch64"  ;;
  *)              RUST_ARCH="$HOST_ARCH" ;;
esac
# Cross-arch on a Linux host that has the matching cross toolchain + sysroot
# (e.g. an x86_64 CI producing an aarch64 build for an ARM server).
if [ -n "${CX_ARCH:-}" ]; then RUST_ARCH="$CX_ARCH"; fi
TARGET="${RUST_ARCH}-unknown-linux-${LIBC}"

ON_LINUX=false
[ "$(uname -s)" = "Linux" ] && ON_LINUX=true

if ! $ON_LINUX; then
  cat >&2 <<EOF
✗ build_linux.sh must run on a Linux host (it links against system GUI libs).

You are on $(uname -s). Options:
  1. Run this script on a Linux machine / VM (preferred).
  2. Build inside a Linux container with the GUI dev packages installed —
     see the Docker one-liner in this script's header.

There is no macOS/Windows cross path: a full slint GUI app needs a complete
per-target sysroot + pkg-config + ~a dozen shared libs, so we only build natively.
EOF
  exit 1
fi

# ── toolchain ───────────────────────────────────────────────────────────────
# Native / Linux cross-arch: use the host gcc (or a CC in the environment).
if [ -z "${CC:-}" ]; then
  # A same-arch or cross gcc on PATH (e.g. aarch64-linux-gnu-gcc) wins, else cc.
  for c in "${RUST_ARCH}-linux-gnu-gcc" "cc" "gcc"; do
    if command -v "$c" >/dev/null; then CC="$c"; break; fi
  done
fi
CXX=""
case "${CC}" in
  *-gcc) CXX="${CC%-gcc}-g++" ;;
  gcc|cc) CXX="${CC}++" ;;
esac
AR=""
case "${CC}" in
  *-gcc) AR="${CC%-gcc}-ar" ;;
  gcc)  AR="ar" ;;
  cc)   AR="gcc-ar" ;;
esac
[ -n "$CC" ]  && export CC
[ -n "$CXX" ] && export CXX
[ -n "$AR" ]  && export AR

# rustc needs the (cross-)gcc as its linker when building a different arch;
# for a same-arch native build the default linker already works, but pinning it
# to the chosen CC is harmless and makes cross-arch on Linux deterministic.
if [ -n "${CX_ARCH:-}" ]; then
  TU="$(printf '%s' "$TARGET" | tr '[:lower:]' '[:upper:]' | tr '-' '_')"
  export "CARGO_TARGET_${TU}_LINKER=$(basename "$CC")"
fi

echo "──────── chatx Linux build ────────"
echo "  host   : $(uname -s)/$(uname -m) (Linux native)"
echo "  target : $TARGET"
echo "  mode   : $MODE"
echo "  cc     : ${CC:-unset}${CXX:+   cxx: $CXX}${AR:+   ar: $AR}"

# ── GUI build deps (informational — required to compile) ───────────────────
# If the host lacks these the build will fail; print a pointer to install them.
if ! pkg-config --exists fontconfig 2>/dev/null; then
  cat >&2 <<'EOF'

⚠ fontconfig not found via pkg-config. The desktop build needs the dev packages
  (Debian/Ubuntu):
    apt install pkg-config libxcb1-dev libx11-dev libxrandr-dev libxkbcommon-dev \
                libwayland-dev libpixman-1-dev libfontconfig1-dev libegl-dev \
                libgbm-dev libasound2-dev libpipewire-0.3-dev
(Fedora: dnf install pkgconf pcre2-devel xcb-devel libX11-devel libXrandr-devel
         libxkbcommon-devel wayland-devel pixman-devel fontconfig-devel
         mesa-libEGL-devel mesa-libgbm-devel alsa-lib-devel pipewire-devel)
Continuing — the build will surface a hard error if any are missing.
EOF
fi

# ── rustup target ───────────────────────────────────────────────────────────
command -v cargo >/dev/null || { echo "missing tool: cargo" >&2; exit 1; }
if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "==> rustup target add $TARGET"
  rustup target add "$TARGET"
fi

# ── clean (optional) ────────────────────────────────────────────────────────
if [ "$CLEAN" -eq 1 ]; then
  echo "==> wiping $ROOT/target/$TARGET"
  rm -rf "$ROOT/target/$TARGET"
fi

# ── build ───────────────────────────────────────────────────────────────────
if [ -n "$RELEASE" ]; then
  echo "==> cargo build -p chatx --target $TARGET --release"
  cargo build -p chatx --target "$TARGET" --release
else
  echo "==> cargo build -p chatx --target $TARGET (debug)"
  cargo build -p chatx --target "$TARGET"
fi
BIN="$ROOT/target/$TARGET/$MODE/chatx"
[ -x "$BIN" ] || { echo "binary not found: $BIN" >&2; exit 1; }

# ── X mini-app artifacts (x-t/) — same self-contained approach as build_osx ──
# Built via scripts/build_x.sh (wasm32-wasip1); bundled next to the exe so the
# X extension page works after extraction. Warn + continue if unavailable.
if [ -x "$ROOT/scripts/build_x.sh" ]; then
  echo "==> building X mini-app (x-t/)"
  chmod +x "$ROOT/scripts/build_x.sh" 2>/dev/null || true
  bash "$ROOT/scripts/build_x.sh" || echo "   (build_x.sh failed — X page will be unavailable)"
fi

# ── package ─────────────────────────────────────────────────────────────────
echo "==> packaging $OUT_DIR"
rm -rf "$OUT_DIR"; mkdir -p "$OUT_DIR"
install -m 0755 "$BIN" "$OUT_DIR/chatx"

HAVE_X=0
if [ -f "$ROOT/x-t/x.hello/0.1.0/app.wasm" ]; then
  cp -R "$ROOT/x-t" "$OUT_DIR/x-t"
  HAVE_X=1
  echo "    -> bundled x-t/ ($(du -sh "$OUT_DIR/x-t" | awk '{print $1}'))"
else
  echo "   (x-t/x.hello artifact absent — X extension page will report missing wasm)"
fi

cat > "$OUT_DIR/chatx.desktop" <<'DESK'
[Desktop Entry]
Name=Chatx
Comment=Realtime P2P chat
Exec=chatx
Type=Application
Categories=Network;InstantMessaging;
Icon=chatx
DESK
if [ -f "$ROOT/app-icon.png" ]; then
  install -m 0644 "$ROOT/app-icon.png" "$OUT_DIR/chatx.png"
  sed -i.bak 's#^Icon=chatx#Icon=chatx.png#' "$OUT_DIR/chatx.desktop" && rm -f "$OUT_DIR/chatx.desktop.bak"
fi

TARBALL="$OUT_DIR/chatx-${TARGET/-unknown-/}-${MODE}.tar.gz"
# Collect whatever we actually produced (icon may be absent) into the archive.
ENTRIES=(chatx chatx.desktop)
[ -f "$OUT_DIR/chatx.png" ] && ENTRIES+=(chatx.png)
[ "$HAVE_X" -eq 1 ] && ENTRIES+=(x-t)
tar -czf "$TARBALL" -C "$OUT_DIR" "${ENTRIES[@]}"
echo "  -> $TARBALL (${#ENTRIES[@]} items: ${ENTRIES[*]})"

# ── what the *target* machine needs (glibc build) ──────────────────────────
echo
cat <<'EOF'
  NOTE: glibc build links shared system libraries. On the target machine:
    Debian/Ubuntu:
      apt install libxcb1 libx11-6 libxrandr2 libxkbcommon0 \
                  libwayland-client0 libpixman-1-2 libfontconfig1 libegl1 libgbm1
    Fedora:
      dnf install xcb libX11 libXrandr libxkbcommon wayland libpixman-1 fontconfig libEGL libgbm
    Arch:
      pacman -S libxcb libx11 libxrandr libxkbcommon wayland libpipewire
    (pipewire/pulseaudio optional — only needed for audio)
EOF
echo
echo "──────── done ────────"
ls -lh "$TARBALL"
echo
echo "OK. On the target box: tar -xzf $(basename "$TARBALL") && ./chatx"
