#!/usr/bin/env bash
# Chatx — macOS desktop build & packaging.
#
# Produces a distributable `Chatx.app` bundle (and optionally a DMG) from the
# `chatx` binary target. Native to macOS, so it runs here out of the box.
#
# Usage:
#   ./scripts/build_osx.sh                 # Release build (default) → target/macos/Chatx.app
#   ./scripts/build_osx.sh --debug         # Debug build (fast iteration)
#   ./scripts/build_osx.sh --dmg          # Also bundle into Chatx.dmg
#   ./scripts/build_osx.sh --sign "Developer ID Application: <Name> (TEAM)"
#   ./scripts/build_osx.sh --clean        # Wipe cargo macos artifacts first
#   ./scripts/build_osx.sh --run          # Build then launch the raw binary
#
# Pre-reqs: macOS 12+, Xcode command-line tools (for `sips`/`iconutil`),
#           rustup + `aarch64-apple-darwin` (or `x86_64-apple-darwin`).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RELEASE="release"
DMG=0
SIGN_IDENTITY=""
CLEAN=0
RUN=0
ICON_SRC="$ROOT/app-icon.png"

while [ $# -gt 0 ]; do
  case "$1" in
    --release)     RELEASE="release" ;;
    --debug)       RELEASE="" ;;
    --dmg)         DMG=1 ;;
    --sign)        shift; SIGN_IDENTITY="${1:-}" ;;
    --clean)       CLEAN=1 ;;
    --run)         RUN=1 ;;
    -h|--help)     grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
    *)             echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

MODE="${RELEASE:-debug}"
OUT_DIR="$ROOT/target/macos"
APP="$OUT_DIR/Chatx.app"

echo "──────── chatx macOS build ($MODE) ────────"

# ── 0. toolchain ────────────────────────────────────────────────────────────
for tool in cargo sips iconutil; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done
# Pick the host arch and confirm the rustup target is present.
ARCH="$(uname -m)"
case "$ARCH" in
  arm64)    TARGET="aarch64-apple-darwin" ;;
  x86_64)   TARGET="x86_64-apple-darwin" ;;
  *)        echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "==> Installing rustup target: $TARGET"
  rustup target add "$TARGET"
fi

# ── 1. clean (optional) ─────────────────────────────────────────────────────
if [ "$CLEAN" -eq 1 ]; then
  echo "==> wiping $ROOT/target/$TARGET (cargo)"
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

# ── 3. assemble the .app bundle ─────────────────────────────────────────────
echo "==> assembling $APP"
rm -rf "$OUT_DIR"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

install -m 0755 "$BIN" "$APP/Contents/MacOS/chatx"

# Icon: PNG → multi-resolution .icns (macOS-native tools, no deps).
if [ -f "$ICON_SRC" ]; then
  echo "==> converting icon: $ICON_SRC → AppIcon.icns"
  ICONSET="$OUT_DIR/AppIcon.iconset"
  mkdir -p "$ICONSET"
  for s in 16 32 128 256 512; do
    sips -z     $s     $s     "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}.png"      >/dev/null
    sips -z $((s*2)) $((s*2)) "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}@2x.png"   >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns" 2>/dev/null \
    || echo "   (iconutil warning — continuing without icns)"
  rm -rf "$ICONSET"
fi

# Info.plist (runtime permissions + bundle metadata).
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>Chatx</string>
    <key>CFBundleDisplayName</key>     <string>Chatx</string>
    <key>CFBundleIdentifier</key>      <string>com.chatx.desktop</string>
    <key>CFBundleVersion</key>         <string>0.1.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleExecutable</key>      <string>chatx</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleIconFile</key>        <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>  <string>12.0</string>
    <key>NSHighResolutionCapable</key> <true/>
    <key>NSPrincipalClass</key>        <string>NSApplication</string>
    <!-- Realtime-comms entitlements (prompts appear on first use). -->
    <key>NSCameraUsageDescription</key>      <string>Chatx uses the camera for video calls and QR scanning.</string>
    <key>NSMicrophoneUsageDescription</key>  <string>Chatx uses the microphone for voice calls.</string>
    <key>NSScreenCaptureUsageDescription</key><string>Chatx captures your screen for screen sharing.</string>
    <key>NSScreenCaptureAware</key>          <true/>
</dict>
</plist>
PLIST

# PkgInfo (classic, still read by some tooling).
printf 'APPL????' > "$APP/Contents/PkgInfo"

# ── 4. code sign (ad-hoc by default so Gatekeeper local runs work) ──────────
echo "==> codesign"
if [ -n "$SIGN_IDENTITY" ]; then
  codesign --force --options runtime --deep \
    --sign "$SIGN_IDENTITY" "$APP"
else
  echo "   (ad-hoc signing — set --sign for a distribution identity)"
  codesign --force --sign - --deep "$APP"
fi
codesign --verify --verbose=2 "$APP" 2>&1 | tail -3

# ── 5. optional DMG ─────────────────────────────────────────────────────────
if [ "$DMG" -eq 1 ]; then
  echo "==> creating DMG"
  hdiutil create -volname "Chatx" -srcfolder "$APP" \
    -ov -format UDZO "$OUT_DIR/Chatx.dmg" >/dev/null
fi

# ── 6. summary / optional launch ────────────────────────────────────────────
echo
echo "──────── done ────────"
ls -lh "$APP/Contents/MacOS/chatx"
du -sh "$APP" | awk '{print "  bundle size: "$1}'
if [ "$DMG" -eq 1 ]; then ls -lh "$OUT_DIR/Chatx.dmg"; fi

if [ "$RUN" -eq 1 ]; then
  echo "==> launching $BIN"
  exec "$BIN"
fi

echo
echo "OK. Install: cp -R \"$APP\" /Applications/   — or open: open \"$APP\""