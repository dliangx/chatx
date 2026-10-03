# Android cross-compile environment for chatx.
export ANDROID_HOME="$HOME/Library/Android/sdk"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export ANDROID_NDK_ROOT="$ANDROID_HOME/android-ndk-r27c"
export ANDROID_NDK_HOME="$ANDROID_NDK_ROOT"
export NDK_HOME="$ANDROID_NDK_ROOT"

NDKBIN="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/darwin-x86_64/bin"
export PATH="$NDKBIN:$PATH"

export CC_aarch64_linux_android="aarch64-linux-android-clang"
export CXX_aarch64_linux_android="aarch64-linux-android-clang++"

# Java toolchain for slint-backend-android-activity's build.rs (Java → D8 dex).
export JAVA_HOME="$(/usr/libexec/java_home -v 17 2>/dev/null)"
export PATH="$JAVA_HOME/bin:$PATH"
# skia-bindings build script expects ANDROID_NDK (not ANDROID_NDK_ROOT).
export ANDROID_NDK="$ANDROID_NDK_ROOT"
