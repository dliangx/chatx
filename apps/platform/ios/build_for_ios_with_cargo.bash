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
# SDK_NAME arrives versioned from xcodebuild (e.g. `iphonesimulator27.0`,
# `iphoneos17.0`), so match on the *family* substring rather than the exact
# name. Failing this check would silently cross-build the simulator slice as
# a device slice (or worse, link the ObjC sim shim into a device binary).
SDK_FAMILY="${SDK_NAME:-iphoneos}"
case "${SDK_FAMILY}" in
  *simulator*) IS_SIM=1 ;;
  *)           IS_SIM=0 ;;
esac
if [ "${IS_SIM}" = "1" ]; then
  # rustc triple for cargo; clang needs the `-simulator` spelling (not `-sim`).
  case "${ARCHS}" in
    *arm64*)  TRIPLE="aarch64-apple-ios-sim";   CLANG_TRIPLE="arm64-apple-ios16.0-simulator" ;;
    *x86_64*) TRIPLE="x86_64-apple-ios-sim";    CLANG_TRIPLE="x86_64-apple-ios16.0-simulator" ;;
    *)        TRIPLE="aarch64-apple-ios-sim";   CLANG_TRIPLE="arm64-apple-ios16.0-simulator" ;;
  esac
else
  TRIPLE="aarch64-apple-ios"
  CLANG_TRIPLE="arm64-apple-ios16.0"
fi
echo "==> rust target: ${TRIPLE}   clang target: ${CLANG_TRIPLE}  (SDK_NAME=${SDK_FAMILY}, ARCHS=${ARCHS:-arm64})"

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
    echo "==> clang ${CLANG_TRIPLE}  ->  ${OBJC_OUT}"
    "${CLANG_BIN}" \
      -target "${CLANG_TRIPLE}" \
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

# ── wire the shim into chatx's link ──────────────────────────────────────────
# We do NOT use RUSTFLAGS=-C link-args: that is global and would inject the
# ObjC object into EVERY dependency dylib's link (e.g. `if-watch`), which
# fails because those have no ObjC runtime. Instead we export CHATX_IOS_SHIM
# so `apps/chat/build.rs` emits `cargo:rustc-link-arg=<shim.o>` for the
# chatx bin — the object is added only to the final `chatx` executable link.
export CHATX_IOS_SHIM="${SHIM_O:-}"

REBUILD=0
if [ ! -x "$BIN_SRC" ]; then
  REBUILD=1
else
  for f in "$ROOT/apps/chat/src/main.rs" \
           "$ROOT/apps/chat/src/lib.rs" \
           "$ROOT/crates/bridge/src/lib.rs" \
           "$ROOT/crates/bridge/src/ffi.rs" \
           "$ROOT/crates/bridge/src/shim_impl.rs" \
           "$ROOT/apps/chat/build.rs"; do
    if [ -e "$f" ] && [ "$f" -nt "$BIN_SRC" ]; then REBUILD=1; break; fi
  done
  if [ -n "$SHIM_O" ] && [ "$SHIM_O" -nt "$BIN_SRC" ]; then REBUILD=1; fi
fi

if [ "$REBUILD" = "1" ]; then
  echo "==> cargo build -p chatx --target $TRIPLE $CARGO_FLAGS $FEATURES (CHATX_IOS_SHIM=${CHATX_IOS_SHIM:-<none>})"
  cargo build -p chatx --target "$TRIPLE" $CARGO_FLAGS $FEATURES
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
