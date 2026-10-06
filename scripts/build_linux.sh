#!/usr/bin/env bash
# Chatx — Linux desktop build & packaging.
#
# Works two ways:
#
#   1. Native (on Linux, recommended — simplest):
#        ./scripts/build_linux.sh
#      Uses the host clang/gcc.
#
#   2. Cross from macOS:
#        brew tap messense/macos-cross-toolchains
#        brew install x86_64-unknown-linux-gnu          # glibc target (default)
#        #   or:  brew install x86_64-unknown-linux-musl   # for --musl
#        CX_ARCH=x86_64 ./scripts/build_linux.sh   # from an arm64 Mac
#
# Produces `chatx` + a portable `chatx-<arch>-linux-<mode>.tar.gz` (glibc
# build by default, musl with `--musl`) in `build/linux/`.
#
# Usage:
#   ./scripts/build_linux.sh                 # Release, host-arch target
#   ./scripts/build_linux.sh --debug         # Debug build
#   ./scripts/build_linux.sh --musl          # Cross-build with musl libc
#   ./scripts/build_linux.sh --clean         # Wipe cargo artifacts first
#   CX_ARCH=x86_64 ./scripts/build_linux.sh  # Force the x86_64 target (e.g. from an arm64 Mac)
#
# Set CC / CXX / AR in the environment to force a specific toolchain.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RELEASE="release"
MUSL=0
CLEAN=0
OUT_DIR="$ROOT/build/linux"

while [ $# -gt 0 ]; do
  case "$1" in
    --release)  RELEASE="release" ;;
    --debug)    RELEASE="" ;;
    --musl)     MUSL=1 ;;
    --clean)    CLEAN=1 ;;
    -h|--help)  grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
    *)          echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

MODE="${RELEASE:-debug}"
if [ "$MUSL" = "1" ]; then LIBC="musl"; else LIBC="gnu"; fi
# Map `uname -m` (arm64/x86_64/aarch64) to a Rust target arch.
HOST_ARCH="$(uname -m)"
case "$HOST_ARCH" in
  x86_64|amd64)   RUST_ARCH="x86_64"   ;;
  arm64|aarch64)  RUST_ARCH="aarch64"  ;;
  *)              RUST_ARCH="$HOST_ARCH" ;;
esac
# Cross-compile for a different target arch (used when the host-arch toolchain
# is absent but a cross-compiler for this arch is installed, e.g. an arm64 Mac
# with the x86_64-linux-gnu toolchain): CX_ARCH=x86_64 ./scripts/build_linux.sh
if [ -n "${CX_ARCH:-}" ]; then RUST_ARCH="$CX_ARCH"; fi
TARGET="${RUST_ARCH}-unknown-linux-${LIBC}"

ON_LINUX=false
[ "$(uname -s)" = "Linux" ] && ON_LINUX=true

# ── CC/CXX/AR selection ─────────────────────────────────────────────────────
# Priority:
#   1. CC / CXX / AR already in the environment (explicit user choice)
#   2. A named cross-compiler on PATH  (e.g. x86_64-linux-gnu-gcc via Homebrew)
#   3. Fallback to host cc (Linux native)
#
# For macOS cross, the recommended install (see header) gives us (2). We never
# auto-install anything.

if [ -z "${CC:-}" ]; then
  CC=""
  # 1) A real gcc cross-compiler on PATH (Homebrew x86_64-linux-gnu / musl).
  CROSS_CC_NAMES=(
    "${RUST_ARCH}-linux-gnu-gcc"
    "${RUST_ARCH}-linux-musl-gcc"
    "${RUST_ARCH}-linux-gnu-clang"
  )
  for c in "${CROSS_CC_NAMES[@]}"; do
    if command -v "$c" >/dev/null; then CC="$c"; break; fi
  done

  # 2) Native Linux: fall back to the distro's cc.
  if [ -z "$CC" ] && $ON_LINUX; then CC="$(command -v cc || echo cc)"; fi

  if [ -z "$CC" ] && ! $ON_LINUX; then
    echo "✗ no Linux C compiler found on this non-Linux host. Install one:" >&2
    cat >&2 <<'EOF'

    # Preferred (Homebrew, real gcc cross toolchain):
      brew tap messense/macos-cross-toolchains
      brew install x86_64-linux-gnu          # glibc
      # or:  brew install x86_64-linux-musl  # musl (with --musl)
    Then re-run this script.
EOF
    exit 1
  fi
fi

# Derive CXX + AR to match whichever CC we picked.
if [ -z "${CXX:-}" ]; then
  CXX=""
  case "$CC" in
    *-gcc)   CXX="${CC%-gcc}-g++" ;;
    gcc)     CXX="g++" ;;
  esac
fi
if [ -z "${AR:-}" ]; then
  AR=""
  case "$CC" in
    *-gcc)   AR="${CC%-gcc}-ar" ;;
    gcc)     AR="ar" ;;
  esac
fi

if $ON_LINUX; then
  # Native: host == target, a bare CC is correct and harmless.
  [ -n "$CC" ]  && export CC
  [ -n "$CXX" ] && export CXX
  [ -n "$AR" ]  && export AR
else
  # Cross-build from a non-Linux host.
  #
  # IMPORTANT: do NOT export bare CC/CXX/AR. Those are consulted by cc-rs for
  # *host* build-dependencies (e.g. `ring`'s build step compiles native C for
  # the Apple/Windows host), and forcing the Linux cross-compiler there produces
  # bogus Apple/Windows flags.
  #
  # cc-rs (see its `get_var`) resolves the compiler for a crate as:
  #   CC_<target>  →  CC_<target with '-' replaced by '_'>  →  …  →  CC
  # so the first form below (lowercase, underscored) is exactly what it reads.
  # Dashes aren't valid in shell var names, so we use the underscore form.
  CC_TU="$(printf '%s' "$TARGET" | tr '-' '_')"
  [ -n "$CC" ]  && export "CC_${CC_TU}=$CC"
  [ -n "$CXX" ] && export "CXX_${CC_TU}=$CXX"
  [ -n "$AR" ]  && export "AR_${CC_TU}=$AR"

  # rustc, meanwhile, needs the cross-compiler as its *linker* (otherwise it
  # invokes the host `cc` — AppleClang on macOS — which can't emit a Linux ELF).
  # Cargo reads the linker from CARGO_TARGET_<UPPERCASED>_LINKER.
  TU="$(printf '%s' "$TARGET" | tr '[:lower:]' '[:upper:]' | tr '-' '_')"
  LINKER_NAME="$(basename "$CC")"
  export "CARGO_TARGET_${TU}_LINKER=$LINKER_NAME"
fi

if ! $ON_LINUX; then
  # Cross-build from a non-Linux host: a valid Linux C toolchain must be
  # present, otherwise cc-rs silently falls back to the host compiler and
  # produces a non-Linux ELF (which will not link against the Rust target).
  if ! [[ "$CC" == *linux* ]]; then
    echo "✗ cross-build requested from $(uname -s) but no Linux C toolchain found." >&2
    echo "  This would silently target $(uname -s) (cc=$(command -v cc)), producing a broken binary for $TARGET." >&2
    echo "" >&2
    echo "Install one of:" >&2
    echo "  brew install messense/macos-cross-toolchains/x86_64-linux-gnu     # x86_64 glibc" >&2
    echo "  brew install messense/macos-cross-toolchains/x86_64-linux-musl    # x86_64 musl" >&2
    exit 1
  fi
fi

echo "──────── chatx Linux build ────────"
if $ON_LINUX; then HOST_KIND="native-linux"; else HOST_KIND="cross from $(uname -s)"; fi
echo "  host   : $(uname -s)/$(uname -m) ($HOST_KIND)"
echo "  target : $TARGET"
echo "  mode   : $MODE"
echo "  cc     : ${CC:-<unset>}  ${CXX:+cxx: $CXX}  ${AR:+ar: $AR}"

# ── rustup target ───────────────────────────────────────────────────────────
command -v cargo   >/dev/null || { echo "missing tool: cargo" >&2; exit 1; }
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

# ── package ─────────────────────────────────────────────────────────────────
echo "==> packaging $OUT_DIR"
rm -rf "$OUT_DIR"; mkdir -p "$OUT_DIR"
install -m 0755 "$BIN" "$OUT_DIR/chatx"

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

# ── what the *target* machine needs (glibc build) ──────────────────────────
echo
if [ "$MUSL" -eq 0 ]; then
  cat <<EOF
  NOTE: glibc build links against shared system libraries. On the target machine:
    Debian/Ubuntu:
      apt install libxcb1 libx11-6 libxrandr2 libxkbcommon0 \\
                  libwayland-client0 libpixman-1-2 libfontconfig1
    Fedora:
      dnf install xcb libX11 libXrandr libxkbcommon wayland libpixman-1 fontconfig
    Arch:
      pacman -S libxcb libx11 libxrandr libxkbcommon wayland libpipewire
    (pipewire/pulseaudio optional — only needed for audio)
EOF
else
  echo "  NOTE: musl build is closer to statically linked; most distros need no extra libs."
fi

echo
echo "──────── done ────────"
ls -lh "$TARBALL"
echo
echo "OK. On the target box: tar -xzf $(basename "$TARBALL") && ./chatx"
