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
# Mode:     release (default) or debug (set RELEASE=0)
#           Rebuild even if a cached artifact exists: REBUILD=1
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# ── 1. env ─────────────────────────────────────────────────────────────────
source "$ROOT/.cargo/env-android.sh"

MODE="release"
if [[ "${RELEASE:-1}" == "0" ]]; then MODE="debug"; fi
TARGET="aarch64-linux-android"
OUT_DIR="$ROOT/build/android"
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
# Always invoke cargo — it's incremental and a no-op when nothing changed.
# (The old `[[ -x $SO_SRC ]]` shortcut shipped stale .so's whenever sources
# were edited after the first successful build.)
echo "▶ cargo build -p chatx --target $TARGET --$MODE"
cargo build -p chatx --target "$TARGET" --"$MODE"

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

# ── 5b. X mini-app artifacts (x-t/) — built via scripts/build_x.sh ──────────
# Packaged under the APK's assets/ directory. The chat UI's xhost.rs reads it
# from the AssetManager (via `bridge::read_asset` → `NativeBridge.extractAsset`)
# and falls back to the cwd / walking-up path otherwise.
echo "▶ build_x.sh (X mini-app wasm)"
if [ -x "$ROOT/scripts/build_x.sh" ]; then
  chmod +x "$ROOT/scripts/build_x.sh" 2>/dev/null || true
  bash "$ROOT/scripts/build_x.sh" >/dev/null 2>&1 || echo "   (build_x.sh failed — X page will be unavailable in APK)"
fi

# ── 7. zipalign (with 4096-page alignment for native lib mmap) ─────────────
rm -f "$APK_RAW" "$APK_ALIGNED"
echo "▶ zipalign -p 4"
mkdir -p "$OUT_DIR/lib/arm64-v8a"
cp "$SO_STRIPPED" "$OUT_DIR/lib/arm64-v8a/libchatx.so"

# ── Native C++ runtime ─────────────────────────────────────────────────────
# `libchatx.so` is linked against the NDK's shared C++ runtime. On a Mac host
# the toolchain defaults to the shared variant, so the .so carries a
# DT_NEEDED for `libc++_shared.so` (+ `libunwind.so`). If we only ship
# `libchatx.so`, dlopen fails at launch → `UnsatisfiedLinkError` + KILL.
# Ship the arm64-v8a slice from the NDK sysroot alongside the app lib.
NDK_SYS_LIB_DIR="${ANDROID_NDK_HOME%/}/toolchains/llvm/prebuilt/darwin-x86_64/sysroot/usr/lib/aarch64-linux-android"
if [ -d "$NDK_SYS_LIB_DIR" ]; then
  mkdir -p "$OUT_DIR/lib/arm64-v8a"
  for lib in libc++_shared.so libunwind.so; do
    if [ -f "$NDK_SYS_LIB_DIR/$lib" ]; then
      cp -f "$NDK_SYS_LIB_DIR/$lib" "$OUT_DIR/lib/arm64-v8a/$lib"
      echo "   + $lib"
    fi
  done
else
  echo "   ⚠ NDK sysroot not found ($NDK_SYS_LIB_DIR) — APK will crash on launch (missing libc++_shared.so)" >&2
fi

# Add all the libs we staged (libchatx.so + the C++ runtime) to the APK in
# STORE mode (no compression). Android 10+ expects page-aligned uncompressed
# libs so it can mmap them from the APK directly.
cd "$OUT_DIR"
for lib in "$OUT_DIR"/lib/arm64-v8a/*.so; do
  zip -X -0 "$APK_BASE" "lib/arm64-v8a/$(basename "$lib")" >/dev/null
done
# Add classes.dex (our Shell/Capture/NativeBridge classes).
cp classes.dex.staged classes.dex 2>/dev/null || true
zip -X -0 "$APK_BASE" "classes.dex" >/dev/null
# Add the X mini-app assets (built in step 5b).
if [ -d "$OUT_DIR/assets" ]; then
  zip -X -r "$APK_BASE" "assets" >/dev/null
  echo "   -> added assets/"
fi
cd "$ROOT"

"$ZIPALIGN" -p -v -f 4 "$APK_BASE" "$APK_ALIGNED" | tail -n 3
echo "  aligned: $(ls -lh "$APK_ALIGNED" | awk '{print $5}')"

# ── 8. sign ────────────────────────────────────────────────────────────────
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

# ── 9. verify ──────────────────────────────────────────────────────────────
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
echo "--debug apk--"
echo "adb install $APK_OUT"
