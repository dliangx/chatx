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

    // Java shell → Rust: called when the SAF document picker finishes.
    // Pass the absolute path of the copied file, or "" on cancel / no pick.
    // Implemented in `crates/bridge/src/jni.rs` (Java_com_chatx_NativeBridge_filePickDone).
    public static native int filePickDone(String path);

    // Java shell → Rust: called when the photo picker finishes.
    // Pass the newline-joined absolute paths of the copied images, or "" on
    // cancel / no pick.
    // Implemented in `crates/bridge/src/jni.rs` (Java_com_chatx_NativeBridge_photosPicked).
    public static native int photosPicked(String paths);

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

    // ── Mic control ────────────────────────────────────────────────────────
    //
    // The mic capture loop is opt-in (only runs while a WebRTC call is
    // active). These static voids are invoked by the Rust
    // `bridge::jni` via `call_static_method` so the Rust side can tell the
    // shell to (de)start the {@link Capture.MicLoop}. See the comment
    // above the {@code audioPcmIn} declaration for the threading contract.

    /** Start the {@link Capture.MicLoop} (AudioRecord 16 kHz mono). No-op if
     *  already running or if RECORD_AUDIO is not granted. Safe to call from
     *  the Slint UI thread (we spawn a HandlerThread for the loop itself). */
    public static void startMicCapture() {
        Capture.startMic();
    }

    /** Stop the {@link Capture.MicLoop} and release the {@code AudioRecord}.
     *  No-op if not running. */
    public static void stopMicCapture() {
        Capture.stopMic();
    }

    // ── File picker (SAF) ──────────────────────────────────────────────────
    //
    // Called from Rust. `bridge::jni::request_file_pick()` (see
    // `crates/bridge/src/jni.rs`) invokes this method as a JNI static
    // (`call_native_bridge_void("openFilePicker")`). The picker's result
    // flows back through {@link FilePicker#onActivityResult} and ultimately
    // into {@link #filePickDone(String)} (a JNI native method implemented
    // in {@code crates/bridge/src/jni.rs}).
    public static void openFilePicker() {
        FilePicker.launch();
    }

    /**
     * Reveal a file (absolute path on disk) to the user.
     *
     * <p>Rust calls this as a JNI static from {@code bridge::jni::reveal_file}
     * whenever a file cannot be opened by an in-app viewer (mobile docx/
     * xlsx/pptx, chat files of unsupported extension, …). The file is served
     * through {@link Fp} as a {@code content://com.chatx.fp/...} URI and
     * fired as {@code Intent.createChooser(ACTION_VIEW)} so the user picks
     * a destination app (or the OS file manager) to open it with.
     *
     * <p>If the file is not already under the app's private {@code
     * getFilesDir()/}, we first copy it into that tree — the {@link Fp}
     * provider can only serve paths inside it, and external apps can't read
     * arbitrary {@code file://} URIs without us granting permissions.
     */
    public static void reveal(String path) {
        android.app.Activity a = Shell.activity();
        if (a == null || path == null || path.isEmpty()) {
            android.util.Log.w("chatx.reveal", "no activity or empty path — bailing");
            return;
        }
        java.io.File base = new java.io.File(a.getFilesDir().getAbsolutePath(), "chatx_picked");
        if (!base.isDirectory() && !base.mkdirs()) {
            android.util.Log.w("chatx.reveal", "could not create " + base);
            return;
        }
        java.io.File file = new java.io.File(path);
        if (!file.isFile()) {
            android.util.Log.w("chatx.reveal", "file not found: " + path);
            return;
        }
        // If the file is not already under our private tree, copy it in.
        if (!path.startsWith(base.getAbsolutePath() + java.io.File.separator)) {
            String name = file.getName();
            java.io.File dst = new java.io.File(base, name);
            if (dst.exists()) {
                dst = new java.io.File(base, String.valueOf(System.currentTimeMillis()) + "_" + name);
            }
            java.io.FileInputStream in = null;
            java.io.FileOutputStream out = null;
            try {
                in = new java.io.FileInputStream(file);
                out = new java.io.FileOutputStream(dst);
                byte[] buf = new byte[64 * 1024];
                int n;
                while ((n = in.read(buf)) > 0) {
                    out.write(buf, 0, n);
                }
                file = dst;
                android.util.Log.i("chatx.reveal", "copied " + path + " -> " + file);
            } catch (Throwable t) {
                android.util.Log.e("chatx.reveal", "copy failed: " + t);
                return;
            } finally {
                try { if (in  != null) in.close();  } catch (Throwable ignored) {}
                try { if (out != null) out.close(); } catch (Throwable ignored) {}
            }
        }
        android.net.Uri contentUri = Fp.uriFor(a.getFilesDir(), file);
        if (contentUri == null) {
            android.util.Log.w("chatx.reveal", "uriFor returned null for " + file);
            return;
        }
        String mime = Fp.mimeFor(file.getName());
        android.content.Intent view = new android.content.Intent(android.content.Intent.ACTION_VIEW)
                .setDataAndType(contentUri, mime)
                .addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION);
        android.content.Intent chooser =
                android.content.Intent.createChooser(view, "Open with…");
        try {
            a.startActivity(chooser);
        } catch (Throwable t) {
            android.util.Log.e("chatx.reveal", "startActivity: " + t);
            try {
                android.content.Intent send = new android.content.Intent(android.content.Intent.ACTION_SEND);
                send.setType(mime);
                send.putExtra(android.content.Intent.EXTRA_STREAM, contentUri);
                send.addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION);
                a.startActivity(android.content.Intent.createChooser(send, "Share with…"));
            } catch (Throwable t2) {
                android.util.Log.e("chatx.reveal", "fallback ACTION_SEND also failed: " + t2);
            }
        }
    }

    // ── Photo picker ───────────────────────────────────────────────────────
    //
    // Called from Rust (`bridge::jni::request_photos_pick`), which uses the
    // JNI static call form `(I)V` passing the selection limit `max`. The
    // picker's result flows back through {@link FilePicker#onActivityResult}
    // and ultimately into {@link #photosPicked(String)} (a JNI native method
    // in {@code crates/bridge/src/jni.rs}).
    public static void openPhotosPicker(int max) {
        FilePicker.launchPhotos(max);
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

    // ── Asset installation ─────────────────────────────────────────────────
    // Called from Shell.onCreate: copies all of `assets/x-t/...` into the
    // app's private files dir so the Rust side can read it via a plain
    // filesystem path (`home_dir()` == `getFilesDir()` on Android).
    public static void installAssets(android.app.Activity a) {
        android.content.res.AssetManager am = a.getAssets();
        java.io.File target = new java.io.File(a.getFilesDir().getAbsolutePath(), "x-t");
        copyDir(am, "x-t", target);
    }

    private static void copyDir(android.content.res.AssetManager am, String src, java.io.File dst) {
        String[] entries;
        try {
            entries = am.list(src);
        } catch (Exception e) {
            android.util.Log.w("chatx.xbridge", "list(" + src + ") failed: " + e);
            return;
        }
        if (entries == null) return;
        dst.mkdirs();
        for (String name : entries) {
            String child = src.isEmpty() ? name : src + "/" + name;
            java.io.File out = new java.io.File(dst, name);
            String[] sub;
            try { sub = am.list(child); } catch (Exception e) { sub = null; }
            if (sub != null && sub.length > 0) {
                copyDir(am, child, out);
            } else {
                java.io.InputStream is = null;
                java.io.FileOutputStream os = null;
                try {
                    is = am.open(child);
                    os = new java.io.FileOutputStream(out);
                    byte[] buf = new byte[8192];
                    int n;
                    while ((n = is.read(buf)) > 0) os.write(buf, 0, n);
                    android.util.Log.i("chatx.xbridge", "installAssets: copied " + child + " (" + out.length() + "B)");
                } catch (Exception e) {
                    android.util.Log.e("chatx.xbridge", "installAssets: " + child + " failed: " + e);
                } finally {
                    try { if (is != null) is.close(); } catch (Exception ignored) {}
                    try { if (os != null) os.close(); } catch (Exception ignored) {}
                }
            }
        }
    }

    // ── Asset reading (X mini-app bridge) ─────────────────────────────────
    //
    // Called from Rust (`bridge::read_asset`) to load `x-t/...` plugin
    // artifacts packed under `assets/` in the APK. Streams the asset to
    // `dst` on the regular filesystem (which Rust reads from); returns
    // silently on any miss so the caller's `std::fs::read` fails.

    public static void extractAsset(String src, String dst) {
        android.util.Log.i("chatx.xbridge", "extractAsset ENTER: " + src + " -> " + dst);
        android.app.Activity a = Shell.activity();
        if (a == null) {
            android.util.Log.w("chatx.xbridge", "extractAsset: Shell.activity() == null — bailing");
            return;
        }
        android.content.res.AssetManager am = a.getAssets();
        java.io.InputStream is = null;
        java.io.FileOutputStream os = null;
        try {
            is = am.open(src);
            os = new java.io.FileOutputStream(dst);
            byte[] buf = new byte[8192];
            int total = 0, n;
            while ((n = is.read(buf)) > 0) { os.write(buf, 0, n); total += n; }
            os.close(); os = null;
            is.close(); is = null;
            android.util.Log.i("chatx.xbridge", "extractAsset ok: " + src + " -> " + dst + " (" + total + "B)");
        } catch (Throwable e) {
            try { if (os != null) os.close(); } catch (Exception ignored) {}
            try { if (is != null) is.close(); } catch (Exception ignored) {}
            new java.io.File(dst).delete();
            android.util.Log.e("chatx.xbridge", "extractAsset FAILED: " + src
                + " -> " + dst + " : " + e, e);
            return;
        }
    }
}
