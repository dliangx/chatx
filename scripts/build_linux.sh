#!/usr/bin/env bash
# Chatx — Linux desktop build & packaging (builds natively on a Linux host).
#
# This only works ON Linux: chatx's native dependencies (ring, sqlite, skia,
# webrtc/openh264) contain C code that must be built for the host toolchain,
# so there is no cross-compile path here. Run it on the Linux box that will
# run the app (or any Linux box of the same arch).
#
# Produces `chatx` + a portable `chatx-<arch>-linux-<mode>.tar.gz` (glibc
# build) in build/linux/. The glibc build links against system libraries —
# the summary at the end lists what the target machine needs.
#
# Usage:
#   ./scripts/build_linux.sh              # Release build (default)
#   ./scripts/build_linux.sh --debug      # Debug build (fast iteration)
#   ./scripts/build_linux.sh --cc clang   # Prefer clang (default) | gcc
#   ./scripts/build_linux.sh --clean      # Wipe cargo artifacts first
#
# Needs: rust/cargo + a C compiler (clang preferred, gcc also works).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RELEASE="release"
COMPILER="${COMPILER:-clang}"
CLEAN=0
OUT_DIR="$ROOT/build/linux"

while [ $# -gt 0 ]; do
  case "$1" in
    --release)  RELEASE="release" ;;
    --debug)    RELEASE="" ;;
    --cc)       shift; COMPILER="${1:-clang}" ;;
    --clean)    CLEAN=1 ;;
    -h|--help)  grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
    *)          echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

MODE="${RELEASE:-debug}"

# ── 0. host checks ──────────────────────────────────────────────────────────
if [ "$(uname -s)" != "Linux" ]; then
  echo "✗ this script builds natively for Linux and must run on a Linux host." >&2
  echo "  (current host: $(uname -s))/$(uname -m)" >&2
  exit 1
fi

command -v cargo >/dev/null || { echo "missing tool: cargo" >&2; exit 1; }

# C compiler for the native C deps (ring, sqlite, skia, webrtc) + final link.
if [ -z "$CC" ]; then
  if command -v "$COMPILER" >/dev/null; then
    CC="$COMPILER"
  else
    command -v clang >/dev/null && CC=clang
    command -v gcc   >/dev/null && CC=gcc
  fi
  [ -n "$CC" ] || { echo "no C compiler found (need clang or gcc)" >&2; exit 1; }
fi
# Derive the C++ driver if it isn't set explicitly.
[ -n "$CXX" ] || case "$CC" in
  clang) CXX=clang++ ;;
  gcc)   CXX=g++     ;;
  *)     CXX=""      ;;
esac
if [ -n "$CXX" ]; then command -v "$CXX" >/dev/null || { echo "missing C++ compiler: $CXX" >&2; exit 1; }; fi

HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
  *)        TARGET="${HOST_ARCH}-unknown-linux-gnu" ;;
esac

echo "──────── chatx Linux build (host) ────────"
echo "  arch : $HOST_ARCH  (target $TARGET)"
echo "  mode : $MODE"
echo "  cc   : $CC ${CXX:+/ $CXX}"

# ── 1. rustup target ────────────────────────────────────────────────────────
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "==> rustup target add $TARGET"
  rustup target add "$TARGET"
fi

# ── 2. clean (optional) ─────────────────────────────────────────────────────
if [ "$CLEAN" -eq 1 ]; then
  echo "==> wiping $ROOT/target/$TARGET"
  rm -rf "$ROOT/target/$TARGET"
fi

# ── 3. cargo build ──────────────────────────────────────────────────────────
if [ -n "$RELEASE" ]; then
  echo "==> cargo build -p chatx --target $TARGET --release"
  cargo build -p chatx --target "$TARGET" --release
else
  echo "==> cargo build -p chatx --target $TARGET (debug)"
  cargo build -p chatx --target "$TARGET"
fi
BIN="$ROOT/target/$TARGET/$MODE/chatx"
[ -x "$BIN" ] || { echo "binary not found: $BIN" >&2; exit 1; }

# ── 4. package ──────────────────────────────────────────────────────────────
echo "==> packaging $OUT_DIR"
rm -rf "$OUT_DIR"; mkdir -p "$OUT_DIR"
install -m 0755 "$BIN" "$OUT_DIR/chatx"

# Desktop entry (so `cp -r chatx . && make install` style or manual works).
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
tar -czf "$TARBALL" -C "$OUT_DIR" chatx chatx.desktop chatx.png 2>/dev/null \
  || tar -czf "$TARBALL" -C "$OUT_DIR" chatx chatx.desktop
echo "  -> $TARBALL"

# ── 5. runtime deps the target machine may need for Slint ──────────────────
echo
cat <<EOF
  NOTE: the build links against system libraries. If `./chatx` can't find a
        shared lib on the target machine, install (Debian/Ubuntu):
          apt install libxcb1 libx11-6 libxrandr2 libxkbcommon0 \\
                      libwayland-client0 libpixman-1-2 libfontconfig1
        (Fedora):
          dnf install xcb libX11 libXrandr libxkbcommon wayland libpixman-1 fontconfig
EOF

echo
echo "──────── done ────────"
ls -lh "$TARBALL"
echo
echo "OK. On the Linux machine: tar -xzf $(basename "$TARBALL") && ./chatx"
