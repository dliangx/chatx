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

CLEAN=0
DEVICE=0
SIM_NAME=""
TEAM=""
SIM_ID=""

# Resolve an iOS-simulator device id to build against. xcodebuild refuses a
# `name:` destination that matches more than one sim (e.g. two "iPhone 17"),
# and `OS:latest` is not a valid OS token — so we pin by explicit `id:`.
# Preference: an already-booted sim (deterministic, known-alive), else the
# first available one. Emits the device uuid on stdout (empty on failure).
pick_ios_sim_id() {
  xcrun simctl list devices available 2>/dev/null | awk '
    /^-- / {
      h=$0;
      in_ios = (h ~ /^-- iOS /) && (h !~ /tvOS|watchOS|visionOS|Unresolved|Uninstalled/);
      next
    }
    in_ios {
      if (match($0, /[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}/))
        id = substr($0, RSTART, RLENGTH);
      if (id != "") { if ($0 ~ /Booted/) print "B " id; else print "A " id }
    }' | awk '
      { if ($1=="B") boot=$2; else if (av=="") av=$2 }
      END { if (boot!="") print boot; else if (av!="") print av }'
}

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

DEST=""
CONFIG=Debug
SDK="iphonesimulator"
if [ "$DEVICE" -eq 1 ]; then
  DEST="generic/platform=iOS"
  CONFIG=Release
  SDK="iphoneos"
else
  # Simulator build: pin a concrete device.
  #  * --simulator <name|id>: resolve the user's explicit choice.
  #  * otherwise: auto-pick (booted-first, else first available).
  resolve_sim_id() {
    local id_or_name="$1"
    # If it already looks like a UUID, use it verbatim.
    if printf '%s' "$id_or_name" | grep -qE '^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$'; then
      printf '%s\n' "$id_or_name"; return 0
    fi
    # Match by name from the available list.
    xcrun simctl list devices available 2>/dev/null | awk -v want="$id_or_name"'
      /^-- / {
        h=$0;
        in_ios = (h ~ /^-- iOS /) && (h !~ /tvOS|watchOS|visionOS/); next
      }
      in_ios {
        if (index($0, want " (") && index($0,"(") && \
            match($0, /[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}/)) {
          print substr($0, RSTART, RLENGTH); exit
        }
      }'
  }

  if [ -n "$SIM_NAME" ]; then
    SIM_ID="$(resolve_sim_id "$SIM_NAME" || true)"
    [ -n "${SIM_ID:-}" ] || { echo "✗ no iOS simulator matches: $SIM_NAME (try: xcrun simctl list devices available)" >&2; exit 1; }
  else
    SIM_ID="$(pick_ios_sim_id || true)"
    [ -n "${SIM_ID:-}" ] || { echo "✗ no available iOS simulator found (xcrun simctl list devices available)" >&2; exit 1; }
  fi

  echo "==> using iOS simulator id $SIM_ID"
  # Boot it now so xcodebuild/`launch` has a known-alive target.
  # (Booting an already-booted sim is a no-op; cold boot is a few seconds.)
  if ! xcrun simctl list devices 2>/dev/null | grep -E "\(${SIM_ID}\)" | grep -q Booted; then
    xcrun simctl boot "$SIM_ID"
    xcrun simctl bootstatus "$SIM_ID" -b
  fi

  DEST="platform=iOS Simulator,id=$SIM_ID"
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
