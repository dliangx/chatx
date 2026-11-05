package com.chatx;

/**
 * Storage Access Framework / photo picker for Android.
 *
 * <p>Two launchers, both driven from Rust via JNI statics on
 * {@link NativeBridge}:
 *
 * <ul>
 *   <li>{@link NativeBridge#openFilePicker()} → SAF
 *       {@code ACTION_OPEN_DOCUMENT} (any file, single). Result path goes
 *       back to Rust as a single path.</li>
 *   <li>{@link NativeBridge#openPhotosPicker(int max)} →
 *       {@code ACTION_GET_CONTENT} with {@code image/*} +
 *       {@code EXTRA_ALLOW_MULTIPLE}. Up to {@code max} images are
 *       allowed; result is a newline-joined list of paths.</li>
 * </ul>
 *
 * <p>In both cases the shell copies the chosen content into the app's
 * private {@code getFilesDir()} directory (so the Rust side gets a stable
 * filesystem path readable from any thread) and hands the path(s) back to
 * Rust through {@link NativeBridge#filePickDone(String)} or {@link
 * NativeBridge#photosPicked(String)} respectively (JNI native methods in
 * {@code crates/bridge/src/jni.rs}).
 *
 * <p>Uses the classic {@code startActivityForResult} / {@code onActivityResult}
 * pattern (same flow as {@link ScreenShare}'s MediaProjection consent
 * dialog), so the activity reference is captured at attach time and the
 * result is delivered to the activity that launched it.
 */
final class FilePicker {
    private static final String TAG = "chatx.fp";
    static final int REQ_FILE = 0x65;
    static final int REQ_PHOTOS = 0x66;
    private static volatile android.app.Activity activity;

    private FilePicker() {}

    /** Remember the activity reference so launchers can call
     *  {@code startActivityForResult}. Called from {@link Shell#onCreate}. */
    static void attach(android.app.Activity a) {
        activity = a;
    }

    /** Release the reference. Called from {@link Shell#onDestroy}. */
    static void detach() {
        activity = null;
    }

    /** Launch the SAF "open document" intent for one file. No-op if not attached. */
    static void launch() {
        android.app.Activity a = activity;
        if (a == null) {
            android.util.Log.w(TAG, "launch: no activity — bailing");
            return;
        }
        android.content.Intent intent =
                new android.content.Intent(android.content.Intent.ACTION_OPEN_DOCUMENT)
                        .addCategory(android.content.Intent.CATEGORY_OPENABLE);
        // Accept any file type (chat enforces its own extension allow-list
        // after the pick; PDF / Office viewers likewise gate on extension).
        intent.setType("*/*");
        try {
            a.startActivityForResult(intent, REQ_FILE);
        } catch (Throwable t) {
            android.util.Log.e(TAG, "launch failed: " + t, t);
            NativeBridge.filePickDone("");
        }
    }

    /** Launch the system photo picker (up to {@code max} images). No-op if
     *  not attached. */
    static void launchPhotos(int max) {
        android.app.Activity a = activity;
        if (a == null) {
            android.util.Log.w(TAG, "launchPhotos: no activity — bailing");
            NativeBridge.photosPicked("");
            return;
        }
        int cap = Math.max(1, Math.min(9, max));
        android.content.Intent intent = new android.content.Intent(android.content.Intent.ACTION_GET_CONTENT);
        intent.addCategory(android.content.Intent.CATEGORY_OPENABLE);
        intent.setType("image/*");
        intent.putExtra(android.content.Intent.EXTRA_ALLOW_MULTIPLE, cap > 1);
        try {
            a.startActivityForResult(intent, REQ_PHOTOS);
        } catch (Throwable t) {
            android.util.Log.e(TAG, "launchPhotos failed: " + t, t);
            NativeBridge.photosPicked("");
        }
    }

    /** Forward the SAF result back into the picker. Called from
     *  {@link Shell#onActivityResult}. */
    static void onActivityResult(int requestCode, int resultCode, android.content.Intent data) {
        android.app.Activity a = activity;
        if (requestCode == REQ_FILE) {
            if (a == null) {
                android.util.Log.w(TAG, "onActivityResult(file) lost the activity ref — bailing");
                NativeBridge.filePickDone("");
                return;
            }
            if (resultCode == android.app.Activity.RESULT_OK
                    && data != null && data.getData() != null) {
                deliverOne(a, data.getData());
            } else {
                NativeBridge.filePickDone("");
            }
            return;
        }
        if (requestCode == REQ_PHOTOS) {
            if (a == null) {
                android.util.Log.w(TAG, "onActivityResult(photos) lost the activity ref — bailing");
                NativeBridge.photosPicked("");
                return;
            }
            if (resultCode != android.app.Activity.RESULT_OK || data == null) {
                NativeBridge.photosPicked("");
                return;
            }
            java.util.List<android.net.Uri> uris = new java.util.ArrayList<>();
            android.content.ClipData clip = data.getClipData();
            if (clip != null) {
                for (int i = 0; i < clip.getItemCount(); i++) {
                    android.net.Uri u = clip.getItemAt(i).getUri();
                    if (u != null) uris.add(u);
                }
            } else if (data.getData() != null) {
                uris.add(data.getData());
            }
            if (uris.isEmpty()) {
                NativeBridge.photosPicked("");
                return;
            }
            StringBuilder joined = new StringBuilder();
            boolean any = false;
            for (android.net.Uri u : uris) {
                String p = deliverOne(a, u);
                if (p == null || p.isEmpty()) continue;
                if (any) joined.append('\n');
                joined.append(p);
                any = true;
            }
            NativeBridge.photosPicked(any ? joined.toString() : "");
            return;
        }
        // Unknown request code — ignore.
    }

    /** Copy one picked Uri into the app's private dir and return its
     *  absolute path, or {@code null} on any failure. */
    private static String deliverOne(android.app.Activity a, android.net.Uri uri) {
        // Resolve a display name via the resolver (works for content:// and
        // file:// alike). If that fails, fall back to the URI's last path.
        String name = null;
        try (android.database.Cursor c = a.getContentResolver().query(uri, null, null, null, null)) {
            if (c != null && c.moveToFirst()) {
                int idx = c.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME);
                if (idx >= 0) name = c.getString(idx);
            }
        } catch (Throwable ignored) {}
        if (name == null || name.isEmpty()) {
            String last = uri.getLastPathSegment();
            name = (last == null || last.isEmpty()) ? ("picked_" + System.currentTimeMillis()) : last;
        }
        name = name.replaceAll("[/\\\\\\x00-\\x1f]", "_");

        java.io.File dir = new java.io.File(a.getFilesDir().getAbsolutePath(), "chatx_picked");
        if (!dir.isDirectory() && !dir.mkdirs()) dir = a.getFilesDir();
        java.io.File dst = new java.io.File(dir, name);
        if (dst.exists()) dst = new java.io.File(dir, System.currentTimeMillis() + "_" + name);

        android.content.res.AssetFileDescriptor fd = null;
        java.io.InputStream in = null;
        java.io.FileOutputStream out = null;
        try {
            fd = a.getContentResolver().openAssetFileDescriptor(uri, "r");
            if (fd == null) throw new java.io.IOException("openAssetFileDescriptor returned null");
            in = fd.createInputStream();
            out = new java.io.FileOutputStream(dst);
            byte[] buf = new byte[64 * 1024];
            int n; long total = 0;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
                total += n;
            }
            android.util.Log.i(TAG, "picked " + uri + " -> " + dst + " (" + total + "B)");
            return dst.getAbsolutePath();
        } catch (Throwable t) {
            android.util.Log.e(TAG, "deliver failed: " + t, t);
            try { if (dst != null && dst.exists()) dst.delete(); } catch (Throwable ignored) {}
            return null;
        } finally {
            try { if (out != null) out.close(); } catch (Throwable ignored) {}
            try { if (in != null) in.close(); } catch (Throwable ignored) {}
            try { if (fd != null) fd.close(); } catch (Throwable ignored) {}
        }
    }
}
