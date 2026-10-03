#!/usr/bin/env bash
# One-shot Android packaging script (C-step / slint shell / no Kotlin).
#
#   1. cargo build   → target/aarch64-linux-android/<mode>/libchatx.so
#   2. llvm-strip    → ~71MB stripped copy
#   3. aapt2 link    → base.apk (manifest only, no res)
#   4. zipalign -p 4 → 4-byte + 4096-page aligned
#   5. apksigner     → signed debug APK at apps/platform/android/build/Chatx.apk
#
# Requires: .cargo/env-android.sh already sourced, or this script sources it.
# Mode:     debug (default, fast) or release (set RELEASE=1)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# ── 1. env ─────────────────────────────────────────────────────────────────
source "$ROOT/.cargo/env-android.sh"

MODE="debug"
if [[ "${RELEASE:-0}" == "1" ]]; then MODE="release"; fi
TARGET="aarch64-linux-android"
OUT_DIR="$ROOT/apps/platform/android/build"
SO_SRC="$ROOT/target/$TARGET/$MODE/libchatx.so"
SO_STRIPPED="$ROOT/target/libchatx-stripped-$MODE.so"
MANIFEST="$ROOT/apps/platform/android/AndroidManifest.xml"
APK_BASE="$OUT_DIR/base.apk"
APK_RAW="$OUT_DIR/raw.apk"
APK_ALIGNED="$OUT_DIR/aligned.apk"
APK_OUT="$OUT_DIR/Chatx.apk"

BT="$ANDROID_HOME/build-tools/36.0.0"
AAPT2="$BT/aapt2"
ZIPALIGN="$BT/zipalign"
APKSIGNER="$BT/apksigner"



echo "──────── chatx Android packaging ($MODE) ────────"

# ── 2. cargo build ─────────────────────────────────────────────────────────
if [[ ! -x "$SO_SRC" || "${REBUILD:-0}" == "1" ]]; then
  echo "▶ cargo build -p chatx --target $TARGET --$MODE"
  cargo build -p chatx --target "$TARGET" --"$MODE"
else
  echo "✓ found $SO_SRC (rebuild with REBUILD=1)"
fi

# ── 3. strip ───────────────────────────────────────────────────────────────
echo "▶ llvm-strip"
"$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-strip" --strip-unneeded \
  -o "$SO_STRIPPED" "$SO_SRC"
ls -lh "$SO_SRC" "$SO_STRIPPED"

# ── 4. aapt2 link (no resources, just validates and wraps the manifest) ───
mkdir -p "$OUT_DIR"
rm -f "$APK_BASE"
echo "▶ aapt2 link"
ANDROID_JAR="$ANDROID_HOME/platforms/android-37.0/android.jar"
"$AAPT2" link \
  --manifest "$MANIFEST" \
  -I "$ANDROID_JAR" \
  -o "$APK_BASE"
echo "  base: $(ls -lh "$APK_BASE" | awk '{print $5}') contents:"
"$AAPT2" dump badging "$APK_BASE" | head -n 3
unzip -l "$APK_BASE"

# ── 4b. Compile + dex the Kotlin/Java app shell (Shell, Capture, NativeBridge) ──
# Slint's SlintAndroidJavaHelper lives INSIDE libchatx.so as a separate dex
# (compiled by slint's build.rs). Our own classes (Shell, Capture, NativeBridge)
# are the APK's top-level classes.dex.
JDK_HOME="$(/usr/libexec/java_home -v 17 2>/dev/null)"
echo "▶ javac + d8 (apps shell)"
CLASSES_DIR="$OUT_DIR/classes"
rm -rf "$CLASSES_DIR" "$OUT_DIR/classes.dex" && mkdir -p "$CLASSES_DIR"
JAVA_SRC="$ROOT/apps/platform/android/java"
"$JDK_HOME/bin/javac" \
  --release 11 \
  -classpath "$ANDROID_HOME/platforms/android-37.0/android.jar" \
  -d "$CLASSES_DIR" \
  "$JAVA_SRC"/com/chatx/*.java
# d8 doesn't expand globs; collect the .class paths into an array.
CLASS_FILES=()
while IFS= read -r -d '' f; do CLASS_FILES+=("$f"); done < <(find "$CLASSES_DIR" -name '*.class' -print0)
if [[ ${#CLASS_FILES[@]} -eq 0 ]]; then
  echo "✗ no .class files to dex" >&2; exit 1
fi
echo "  ${#CLASS_FILES[@]} .class files:"
for f in "${CLASS_FILES[@]}"; do echo "    $f"; done
"$BT/d8" \
  --release \
  --lib "$ANDROID_HOME/platforms/android-37.0/android.jar" \
  --min-api 26 \
  --output "$OUT_DIR" \
  "${CLASS_FILES[@]}"
ls -lh "$OUT_DIR/classes.dex"

# ── 5. zipalign (with 4096-page alignment for native lib mmap) ─────────────
rm -f "$APK_RAW" "$APK_ALIGNED"
echo "▶ zipalign -p 4"
mkdir -p "$OUT_DIR/lib/arm64-v8a"
cp "$SO_STRIPPED" "$OUT_DIR/lib/arm64-v8a/libchatx.so"

# Add the .so to the APK in STORE mode (no compression). Android 10+ expects
# page-aligned uncompressed libs so it can mmap them from the APK directly.
cd "$OUT_DIR"
zip -X -0 "$APK_BASE" "lib/arm64-v8a/libchatx.so" >/dev/null
# Add classes.dex (our Shell/Capture/NativeBridge classes).
cp classes.dex.staged classes.dex 2>/dev/null || true
zip -X -0 "$APK_BASE" "classes.dex" >/dev/null
cd "$ROOT"

"$ZIPALIGN" -p -v -f 4 "$APK_BASE" "$APK_ALIGNED" | tail -n 3
echo "  aligned: $(ls -lh "$APK_ALIGNED" | awk '{print $5}')"

# ── 6. sign ────────────────────────────────────────────────────────────────
KS="$HOME/.android/debug.keystore"
if [[ ! -f "$KS" ]]; then
  echo "creating debug keystore: $KS"
  JDK_HOME="$(/usr/libexec/java_home -v 17 2>/dev/null)"
  "$JDK_HOME/bin/keytool" -genkeypair -v \
    -keystore "$KS" -storepass android -keypass android \
    -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=Android,O=Android,C=US"
fi
rm -f "$APK_OUT"
echo "▶ apksigner sign"
"$APKSIGNER" sign \
  --ks "$KS" --ks-pass pass:android --ks-key-alias androiddebugkey --key-pass pass:android \
  --v1-signing-enabled true --v2-signing-enabled true --v3-signing-enabled true \
  --out "$APK_OUT" "$APK_ALIGNED"

# ── 7. verify ──────────────────────────────────────────────────────────────
echo "──────── result ────────"
ls -lh "$APK_OUT"
echo
echo "▶ aapt dump badging:"
"$AAPT2" dump badging "$APK_OUT" | head -n 5
echo
echo "▶ apksigner verify --verbose:"
"$APKSIGNER" verify --verbose "$APK_OUT" | head -n 10
echo
echo "▶ contents:"
unzip -l "$APK_OUT"
echo
echo "DONE ✓  install with: adb install $APK_OUT"
