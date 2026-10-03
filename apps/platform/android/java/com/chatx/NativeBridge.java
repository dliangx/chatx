package com.chatx;

/**
 * JNI binding to chatx's Rust `crates/bridge/src/jni.rs`.
 *
 * The native methods live in `libchatx.so` (the Slint cdylib). `bridge` is an
 * rlib linked into that cdylib, so its `#[no_mangle]` JNI exports are here.
 *
 * Threading contract:
 *   - `cameraFrameIn` — called from the Camera2 `ImageReader` executor thread
 *   - `audioPcmIn`    — called from the AudioRecord reader thread
 *   - `screenFrameIn` — called from the MediaProjection ImageReader thread
 *   - `audioPlayPcm`  — called on the shell's playback thread
 */
public final class NativeBridge {
    static { System.loadLibrary("chatx"); }

    public static native int cameraFrameIn(byte[] data, int len, int width, int height, int fmt);
    public static native int audioPcmIn(byte[] data, int len, int sampleRate, int channels);
    public static native int screenFrameIn(byte[] data, int len, int width, int height, int fmt);
    public static native int audioPlayPcm(byte[] data, int len, int sampleRate, int channels);
    public static native int permissionChanged(int perm, int granted);

    // ── Screen share ─────────────────────────────────────────────────────
    //
    // These are *regular* static methods (not native). Rust calls them as
    // JNI statics (`call_static_void_method("startScreenShare", "()V")`)
    // and they funnel into {@link ScreenShare}, which is where the
    // MediaProjection / VirtualDisplay / ImageReader loop lives.
    //
    // The reverse direction (Java → Rust) is {@link #screenFrameIn(byte[], int, int, int, int)} —
    // which *is* native and implemented in `crates/bridge/src/jni.rs`.

    /** Launch the MediaProjection consent flow (must be on the UI thread). */
    public static void startScreenShare() {
        ScreenShare.requestPermission(Shell.activity());
    }

    /** Stop the capture and release the projection + virtual display. */
    public static void stopScreenShare() {
        ScreenShare.stop();
    }

    private NativeBridge() {}

    /** Called from `Shell.onActivityCreate`. Auto-starts capture streams. */
    public static void autoStart(android.app.Activity activity) {
        Capture.start(activity);
    }

    /** Called from `Shell.onActivityDestroy`. */
    public static void autoStop() {
        Capture.stop();
    }
}
