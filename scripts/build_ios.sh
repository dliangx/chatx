#!/usr/bin/env bash
# Chatx iOS build script.
#
# Usage:
#   ./scripts/build_ios.sh                 # build for iOS Simulator (Debug)
#   ./scripts/build_ios.sh --device        # build for device (arm64, Release, signed)
#   ./scripts/build_ios.sh --team "TEAMID" # development team for device signing
#   ./scripts/build_ios.sh --clean         # wipe derived data first
#   ./scripts/build_ios.sh --simulator "iPhone 17"   # explicit simulator
#
# Pre-reqs: macOS, Xcode 15+, cargo, rustup targets:
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios

set -euvx
set -o pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IOS_DIR="$ROOT/apps/platform/ios"
DEVELOPER="$ROOT/target/ios-derived"
DEST_DEFAULT="platform=iOS Simulator,name=iPhone 17"

CLEAN=0
DEVICE=0
SIM_NAME=""
TEAM=""

while [ $# -gt 0 ]; do
  case "$1" in
    --clean)        CLEAN=1 ;;
    --device)       DEVICE=1 ;;
    --team)         shift; TEAM="${1:-}" ;;
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
SDK="iphonesimulator"
if [ "$DEVICE" -eq 1 ]; then
  DEST="generic/platform=iOS"
  CONFIG=Release
  SDK="iphoneos"
fi
if [ -n "$SIM_NAME" ]; then
  DEST="platform=iOS Simulator,name=$SIM_NAME"
fi

# Device builds need a signing team (Apple Developer ID). There is no reliable
# way to auto-derive the team id from local keychain material, so require it.
if [ "$DEVICE" -eq 1 ] && [ -z "$TEAM" ]; then
  cat >&2 <<'EOF'
==> Device build requires a signing team.

    1) Get a 10-char team id:
         security find-identity -v -p codesigning   # confirm a cert exists
         # …or in Xcode → Settings → Accounts → your team, copy the Team ID.

    2) Re-run with it:
         ./scripts/build_ios.sh --device --team YOURTEAMID

   For an uninstalled, unsigned build (won't run on a real device) use the
   simulator path instead:
         ./scripts/build_ios.sh
EOF
  exit 1
fi

# 3. Build via xcodebuild, reusing the same CARGO_TARGET_DIR so M1's cached
#    skia/slint/winit artifacts apply (no 30-min rebuild on every invocation).
#    This is the critical link that makes M4e iteration fast.
BUILD_SETTINGS=(CARGO_TARGET_DIR="${ROOT}/target")
if [ "$DEVICE" -eq 1 ]; then
  echo "==> signing with team: $TEAM"
  BUILD_SETTINGS+=(DEVELOPMENT_TEAM="$TEAM")
fi
echo "==> xcodebuild (dest: $DEST, config: $CONFIG, sdk: $SDK)"
xcodebuild \
  -project Chatx.xcodeproj \
  -scheme Chatx \
  -configuration "$CONFIG" \
  -sdk "$SDK" \
  -destination "$DEST" \
  -derivedDataPath "$DEVELOPER" \
  "${BUILD_SETTINGS[@]}" \
  build 2>&1 | tail -80

echo
echo "OK. App bundle at:"
find "$DEVELOPER/Build/Products" -name "Chatx.app" -maxdepth 5 2>/dev/null || \
  echo "  (no product found — check the xcodebuild log above for errors)"
