#!/usr/bin/env bash
# Called by Xcode's shell-script build phase (see project.yml / pbxproj).
#
# Builds the Rust `chatx` binary for the current iOS SDK/arch. When we have a
# complete iOS SDK on this host, we also:
#   1. compile `apps/platform/ios/Sources/ChatxScreenCapture.m` to a .o
#   2. link that .o into the final .app binary via `-Wl,-force_load`
#   3. enable the `bridge/has-ios-shim` cargo feature so the Rust side
#      resolves the `chatx_screen_capture_*` externs (instead of falling
#      back to a no-op with a stderr warning).
#
# If the SDK is missing or stub (the current state on this Mac), the build
# still produces a working binary — the user just can't drive screen-share
# from this host and must run the build on their real dev Mac.
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

# ── mode ─────────────────────────────────────────────────────────────────────
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [ "${CONFIGURATION}" = "Release" ]; then
  MODE="release"
  CARGO_FLAGS="--release"
else
  MODE="debug"
  CARGO_FLAGS=""
fi

BIN_SRC="$CARGO_TARGET_DIR/$TRIPLE/$MODE/chatx"
OBJC_DIR="$ROOT/apps/platform/ios"
OBJC_SHIM="$OBJC_DIR/Sources/ChatxScreenCapture.m"
OBJC_OUT="$OBJC_DIR/build/chatx_screen_capture.o"
SHIM_O=""
HAS_SHIM=0

# ── detect a real iOS SDK (stub SDKs ship 0 headers + no frameworks) ──────
CLANG_BIN="$(xcrun --find clang 2>/dev/null || true)"
SDK_ROOT="$(xcrun --sdk "${SDK_NAME}" --show-sdk-path 2>/dev/null || true)"
if [ -n "${CLANG_BIN}" ] && [ -n "${SDK_ROOT}" ] && [ -d "${SDK_ROOT}" ] \
   && [ -d "${SDK_ROOT}/System/Library/Frameworks/UIKit.framework" ]; then
  # Framework is present → real SDK on this host. Compile the shim.
  mkdir -p "$(dirname "$OBJC_OUT")"
  if [ ! -x "$OBJC_OUT" ] || [ "$OBJC_SHIM" -nt "$OBJC_OUT" ]; then
    echo "==> clang ${TRIPLE}  ->  ${OBJC_OUT}"
    "${CLANG_BIN}" \
      -target "${TRIPLE}" \
      -arch arm64 \
      -isysroot "${SDK_ROOT}" \
      -std=gnu11 \
      -fobjc-arc \
      -O2 \
      -c \
      -o "$OBJC_OUT" \
      "$OBJC_SHIM" || { echo "✗ shim compile failed"; exit 1; }
  fi
  SHIM_O="$OBJC_OUT"
  HAS_SHIM=1
  echo "✓ shim at $OBJC_OUT (will link + enable bridge/has-ios-shim)"
else
  echo "ℹ no real iOS SDK on this host — skipping ObjC shim, screen-share falls back to a no-op"
fi

# ── pick cargo features (bridge/has-ios-shim only when we have the shim) ─
FEATURES=""
if [ "$HAS_SHIM" = "1" ]; then
  FEATURES="--features bridge/has-ios-shim"
fi

# ── cargo build ──────────────────────────────────────────────────────────────
REBUILD=0
if [ ! -x "$BIN_SRC" ]; then
  REBUILD=1
else
  for f in "$ROOT/apps/chat/src/main.rs" \
           "$ROOT/apps/chat/src/lib.rs" \
           "$ROOT/crates/bridge/src/lib.rs" \
           "$ROOT/crates/bridge/src/ffi.rs" \
           "$ROOT/crates/bridge/src/shim_impl.rs"; do
    if [ -e "$f" ] && [ "$f" -nt "$BIN_SRC" ]; then REBUILD=1; break; fi
  done
  if [ -n "$SHIM_O" ] && [ "$SHIM_O" -nt "$BIN_SRC" ]; then REBUILD=1; fi
fi

if [ "$REBUILD" = "1" ]; then
  echo "==> cargo build -p chatx --target $TRIPLE $CARGO_FLAGS $FEATURES"
  if [ -n "$SHIM_O" ]; then
    # Use RUSTFLAGS to force-link the shim into the final binary. `-Wl,-force_load`
    # makes the linker pull in all symbols of the .o even if none are
    # referenced (the `chatx_screen_capture_*` fns *are* referenced from the
    # bridge crate, but force_load is defensive against future symbol pruning).
    RUSTFLAGS="-C link-args=-Wl,-force_load,${SHIM_O}" \
      cargo build -p chatx --target "$TRIPLE" $CARGO_FLAGS $FEATURES
  else
    cargo build -p chatx --target "$TRIPLE" $CARGO_FLAGS
  fi
fi

[ -x "$BIN_SRC" ] || { echo "✗ rust binary not found: $BIN_SRC" >&2; exit 1; }

# ── install into the Xcode products dir ─────────────────────────────────────
DEST="${TARGET_BUILD_DIR}/${EXECUTABLE_PATH}"
echo "==> $BIN_SRC  ->  $DEST"
mkdir -p "$(dirname "$DEST")"
cp -f "$BIN_SRC" "$DEST"
chmod +x "$DEST"

codesign --force --sign - "$DEST" 2>/dev/null || true
echo "✓ installed $DEST ($(du -h "$DEST" | awk '{print $1}'))"
