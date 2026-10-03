#!/usr/bin/env bash
# Called by Xcode's shell-script build phase (see project.yml / pbxproj).
# Builds the Rust `chatx` binary for the current iOS SDK/arch and drops it
# into the Xcode products dir so the .app bundle contains a real executable.
#
# xcodebuild exports: SDK_NAME, ARCHS, CONFIGURATION, TARGET_BUILD_DIR,
#                     EXECUTABLE_PATH, SRCROOT
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"

# ── pick rustup target triple ────────────────────────────────────────────────
if [ "${SDK_NAME}" = "iphonesimulator" ]; then
  case "${ARCHS}" in
    *arm64*)  TRIPLE="aarch64-apple-ios-sim" ;;
    *x86_64*) TRIPLE="x86_64-apple-ios-sim"  ;;
    *)        TRIPLE="aarch64-apple-ios-sim" ;;
  esac
else
  TRIPLE="aarch64-apple-ios"
fi

# ── resolve build dir & mode ─────────────────────────────────────────────────
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [ "${CONFIGURATION}" = "Release" ]; then
  MODE="release"
  CARGO_FLAGS="--release"
else
  MODE="debug"
  CARGO_FLAGS=""
fi

BIN_SRC="$CARGO_TARGET_DIR/$TRIPLE/$MODE/chatx"

# ── build (skip if artifact is fresh) ───────────────────────────────────────
if [ -x "$BIN_SRC" ] && [ "$BIN_SRC" -nt "$ROOT/apps/chat/src/main.rs" ]; then
  echo "✓ cached: $BIN_SRC"
else
  echo "==> cargo build -p chatx --target $TRIPLE ($MODE)"
  cargo build -p chatx --target "$TRIPLE" $CARGO_FLAGS
fi

[ -x "$BIN_SRC" ] || { echo "✗ rust binary not found: $BIN_SRC" >&2; exit 1; }

# ── install into the Xcode products dir ─────────────────────────────────────
DEST="${TARGET_BUILD_DIR}/${EXECUTABLE_PATH}"
echo "==> $BIN_SRC  ->  $DEST"
mkdir -p "$(dirname "$DEST")"
cp -f "$BIN_SRC" "$DEST"
chmod +x "$DEST"

# ad-hoc sign so the simulator doesn't reject it; xcodebuild will re-sign
# with the real profile later in its own signing phase.
codesign --force --sign - "$DEST" 2>/dev/null || true

echo "✓ installed $DEST ($(du -h "$DEST" | awk '{print $1}'))"
