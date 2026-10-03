#!/usr/bin/env bash
# Chatx iOS build script.
#
# Usage:
#   ./scripts/build_ios.sh                 # build for iOS Simulator (Debug)
#   ./scripts/build_ios.sh --device        # build for device (arm64, Release, signed)
#   ./scripts/build_ios.sh --clean         # wipe derived data first
#   ./scripts/build_ios.sh --simulator "iPhone 17"   # explicit simulator
#
# Pre-reqs: macOS, Xcode 15+, cargo, rustup targets:
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios

set -euvx

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IOS_DIR="$ROOT/apps/platform/ios"
DEVELOPER="$ROOT/target/ios-derived"
DEST_DEFAULT="platform=iOS Simulator,name=iPhone 17"

CLEAN=0
DEVICE=0
SIM_NAME=""

while [ $# -gt 0 ]; do
  case "$1" in
    --clean)        CLEAN=1 ;;
    --device)       DEVICE=1 ;;
    --simulator)    shift; SIM_NAME="${1:-}" ;;
    -h|--help)      grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
    *)              echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

# Sanity check: required tools on PATH.
for tool in cargo xcodebuild xcodegen; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

# 0. Wipe cache if asked.
if [ "$CLEAN" -eq 1 ]; then
  echo "==> Wiping derived data ($DEVELOPER) and cargo iOS artifact cache for clean rebuild"
  rm -rf "$DEVELOPER" "$ROOT/target/aarch64-apple-ios" "$ROOT/target/aarch64-apple-ios-sim" "$ROOT/target/x86_64-apple-ios"
fi

# 1. Make sure rustup has all three Apple iOS targets.
REQUIRED_TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
INSTALLED="$(rustup target list --installed)"
for t in "${REQUIRED_TARGETS[@]}"; do
  if ! printf '%s\n' "$INSTALLED" | grep -qx "$t"; then
    echo "==> Installing rustup target: $t"
    rustup target add "$t"
  fi
done

# 2. Regenerate the Xcode project (idempotent, fast) with xcodegen.
echo "==> xcodegen generate (in $IOS_DIR)"
( cd "$IOS_DIR" && xcodegen generate )

cd "$IOS_DIR"

DEST="$DEST_DEFAULT"
CONFIG=Debug
if [ "$DEVICE" -eq 1 ]; then
  DEST="generic/platform=iOS"
  CONFIG=Release
fi
if [ -n "$SIM_NAME" ]; then
  DEST="platform=iOS Simulator,name=$SIM_NAME"
fi

# 3. Build via xcodebuild, reusing the same CARGO_TARGET_DIR so M1's cached
#    skia/slint/winit artifacts apply (no 30-min rebuild on every invocation).
#    This is the critical link that makes M4e iteration fast.
echo "==> xcodebuild (dest: $DEST, config: $CONFIG)"
xcodebuild \
  -project Chatx.xcodeproj \
  -scheme Chatx \
  -configuration "$CONFIG" \
  -sdk "iphonesimulator" \
  -destination "$DEST" \
  -derivedDataPath "$DEVELOPER" \
  CARGO_TARGET_DIR="${ROOT}/target" \
  build 2>&1 | tail -80

echo
echo "OK. App bundle at:"
find "$DEVELOPER/Build/Products" -name "Chatx.app" -maxdepth 5 2>/dev/null || \
  echo "  (no product found — check the xcodebuild log above for errors)"
