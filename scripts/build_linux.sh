#!/usr/bin/env bash
# Chatx — Linux desktop build & packaging.
#
# Runs natively on a Linux host (recommended) or cross-compiles from macOS /
# Windows *if you have the matching C toolchain*:
#
#   Native (on Linux):            x86_64-unknown-linux-gnu   (apt deps below)
#   Cross macOS  -> x86_64-linux: zigcc or x86_64-linux-gnu-gcc (see "install")
#
# Produces `chatx` + a portable `chatx-x86_64-linux.tar.gz` (glibc build) in
# build/linux/. Slint's runtime needs a handful of system libs on the target
# machine — the summary at the end lists them.
#
# Usage:
#   ./scripts/build_linux.sh                  # Release (default), host arch (gnu)
#   ./scripts/build_linux.sh --debug          # Debug build (fast iteration)
#   ./scripts/build_linux.sh --musl          # glibc -> musl (static C runtime)
#   ./scripts/build_linux.sh --target aarch64-unknown-linux-gnu
#   ./scripts/build_linux.sh --clean
#
# Set CHATX_LDFLAGS / CC / CXX to steer the linker if you use a custom toolchain.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RELEASE="release"
MUSL=0
TARGET="${TARGET:-x86_64-unknown-linux-gnu}"
CLEAN=0
OUT_DIR="$ROOT/build/linux"

while [ $# -gt 0 ]; do
  case "$1" in
    --release)  RELEASE="release" ;;
    --debug)    RELEASE="" ;;
    --musl)     MUSL=1; TARGET="x86_64-unknown-linux-musl" ;;
    --target)   shift; TARGET="${1:-}" ;;
    --clean)    CLEAN=1 ;;
    -h|--help)  grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
    *)          echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

MODE="${RELEASE:-debug}"
echo "──────── chatx Linux build (target=$TARGET, $MODE) ────────"

# ── 0. toolchain detection ──────────────────────────────────────────────────
need() { command -v "$1" >/dev/null; }
for tool in cargo; do
  need "$tool" || { echo "missing tool: $tool" >&2; exit 1; }
done

# rustup target
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "==> rustup target add $TARGET"
  rustup target add "$TARGET"
fi

# C toolchain for `ring` (the only native-C dep in the tree). Presence of a
# matching gcc is required at link time.
CROSS_GCC=""
case "$TARGET" in
  *gnu)   CROSS_GCC="${CC:-}" ; [ -z "$CROSS_GCC" ] && need x86_64-linux-gnu-gcc && CROSS_GCC="x86_64-linux-gnu-gcc" ;;
  *musl)  CROSS_GCC="${CC:-}" ; [ -z "$CROSS_GCC" ] && need x86_64-linux-musl-gcc && CROSS_GCC="x86_64-linux-musl-gcc" ;;
  *aarch64*) CROSS_GCC="${CC:-}" ; [ -z "$CROSS_GCC" ] && need aarch64-linux-gnu-gcc && CROSS_GCC="aarch64-linux-gnu-gcc" ;;
esac

# On a native glibc host we can link with the host C compiler even for gnu.
NATIVE_LINUX=false
[ "$(uname -s)" = "Linux" ] && [ "${TARGET#*-unknown-linux-}" = "gnu" ] && NATIVE_LINUX=true

if [ -z "$CROSS_GCC" ] && ! $NATIVE_LINUX; then
  cat <<'EOF'
==> No C toolchain found for cross-compiling `ring` / linking the binary.
    Install one, then re-run:

    # Option A — zig (cross toolchain, single binary):
    #   brew install zig            (macOS)   |   see https://ziglang.org  (other)
    #   export CC="zig cc -target x86_64-linux-gnu"
    #   export CXX="zig c++ -target x86_64-linux-gnu"
    #   export AR="zig ar"
    #   ./scripts/build_linux.sh

    # Option B — the official cross toolchain:
    #   https://github.com/tpoisonooo/cross (Docker)  or  install-cross-tools (macOS)

    # Native glibc host? Just run this script directly on the Linux box.
EOF
  exit 1
fi

if [ -n "$CROSS_GCC" ]; then
  export CC="$CROSS_GCC"
  export CXX="${CXX:-${CROSS_GCC%\-*}-g++}"
  echo "==> using CC=$CC CXX=$CXX"
fi

# ── 1. clean (optional) ─────────────────────────────────────────────────────
if [ "$CLEAN" -eq 1 ]; then
  echo "==> wiping $ROOT/target/$TARGET"
  rm -rf "$ROOT/target/$TARGET"
fi

# ── 2. cargo build ──────────────────────────────────────────────────────────
if [ -n "$RELEASE" ]; then
  echo "==> cargo build -p chatx --target $TARGET --release"
  cargo build -p chatx --target "$TARGET" --release
else
  echo "==> cargo build -p chatx --target $TARGET (debug)"
  cargo build -p chatx --target "$TARGET"
fi
BIN="$ROOT/target/$TARGET/$MODE/chatx"
[ -x "$BIN" ] || { echo "binary not found: $BIN" >&2; exit 1; }

# ── 3. package ──────────────────────────────────────────────────────────────
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

TARBALL="$OUT_DIR/chatx-${TARGET/unknown/}-${MODE}.tar.gz"
tar -czf "$TARBALL" -C "$OUT_DIR" chatx chatx.desktop chatx.png 2>/dev/null \
  || tar -czf "$TARBALL" -C "$OUT_DIR" chatx chatx.desktop
echo "  -> $TARBALL"

# ── 4. runtime deps the *target* machine needs for Slint (glibc build) ──────
if [ "$MUSL" -eq 0 ]; then
  cat <<EOF

  NOTE: the glibc build links against system libraries. On the target machine
        install (Debian/Ubuntu):
          apt install libxcb1 libx11-6 libxrandr2 libxkbcommon0 \\
                      libwayland-client0 libpixman-1-2 libfontconfig1 \\
                      libpipewire-0.3-0   # (pipewire optional; software render only needs the rest)
        (Fedora):
          dnf install xcb libX11 libXrandr libxkbcommon wayland libpixman-1 \\
                      fontconfig pipewire-libs
EOF
fi

echo
echo "──────── done ────────"
ls -lh "$TARBALL"
echo
echo "OK. Extract on the target and run: ./chatx"