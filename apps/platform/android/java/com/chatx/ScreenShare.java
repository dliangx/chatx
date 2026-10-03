package com.chatx;

import android.app.Activity;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.PixelFormat;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.Image;
import android.media.ImageReader;
import android.media.projection.MediaProjection;
import android.media.projection.MediaProjectionManager;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.DisplayMetrics;
import android.util.Log;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.lang.ref.WeakReference;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Android screen capture (M7b) via
 * {@link MediaProjection} + {@link VirtualDisplay} + {@link ImageReader}
 * (RGBA-8888).
 *
 * Flow:
 *   1. Rust (Slint "Start screen share" button) →
 *      {@link NativeBridge#startScreenShare()} → JNI →
 *      {@link #requestPermission(Activity)}
 *   2. User approves → {@link Activity#onActivityResult(int, int, Intent)}
 *      arrives on the activity → {@link #onActivityResult(int, int, Intent)}
 *      builds a {@link MediaProjection}, a {@link VirtualDisplay} bound to a
 *      {@link ImageReader} (RGBA-8888).
 *   3. Every frame the reader delivers → extracted → pushed into
 *      {@link NativeBridge#screenFrameIn(byte[], int, int, int, int)}
 *      with {@code fmt = 1} (RGBA8888). The Rust-side consumer slot
 *      (installed in {@code apps/chat/src/lib.rs::install_bridge_sinks})
 *      does whatever it wants with the bytes (currently just logs).
 *   4. Rust → JNI → {@link #stop()} tears everything down when the user
 *      taps "Stop".
 *
 * All state is process-global (statics) because the Shell / Activity can be
 * recreated on configuration changes but we don't want the capture loop to
 * die with the activity.
 */
public final class ScreenShare {
    private static final String TAG = "chatx.screenshare";
    /** Bridge PixelFormat.Rgba8888 (see crates/bridge/src/types.rs). */
    private static final int FMT_RGBA8888 = 1;
    /** startActivityForResult req code — must match what Shell.onActivityResult
     *  expects, and is forwarded back to Android's onActivityResult. */
    private static final int REQ_MEDIA = 0x5E;

    private static volatile MediaProjection projection;
    private static volatile VirtualDisplay virtualDisplay;
    private static volatile ImageReader reader;
    private static volatile HandlerThread captureThread;

    private static final AtomicBoolean startFlag = new AtomicBoolean(false);

    /** Called from Rust (JNI) to kick off the consent dialog. Must be on
     *  the main thread (Shell calls it from the UI thread, which is where
     *  the Slint event loop delivers Rust callbacks). */
    public static void requestPermission(final Activity activity) {
        if (activity == null) {
            Log.e(TAG, "[screen] requestPermission: no activity");
            return;
        }
        activity.runOnUiThread(new Runnable() {
            @Override public void run() {
                try {
                    MediaProjectionManager mpm =
                        (MediaProjectionManager) activity
                            .getSystemService(android.content.Context.MEDIA_PROJECTION_SERVICE);
                    if (mpm == null) {
                        Log.e(TAG, "[screen] no MediaProjectionManager");
                        return;
                    }
                    Log.i(TAG, "[screen] launching consent dialog");
                    activity.startActivityForResult(mpm.createScreenCaptureIntent(), REQ_MEDIA);
                } catch (Throwable t) {
                    Log.e(TAG, "[screen] startActivityForResult failed: " + t);
                }
            }
        });
    }

    /** Called from {@link android.app.Activity#onActivityResult}. Must be
     *  on the main thread (Android routes activity callbacks there). */
    public static void onActivityResult(int resultCode, Intent data) {
        final Activity activity = Shell.activity();
        if (activity == null) {
            // No live activity — user dismissed or the process restarted.
            // Bail out cleanly.
            teardown();
            return;
        }
        if (resultCode != Activity.RESULT_OK || data == null) {
            Log.i(TAG, "[screen] user declined consent (code=" + resultCode + ")");
            teardown();
            return;
        }
        MediaProjectionManager mpm =
            (MediaProjectionManager) activity
                .getSystemService(android.content.Context.MEDIA_PROJECTION_SERVICE);
        if (mpm == null) {
            teardown();
            return;
        }
        MediaProjection mp;
        try {
            mp = mpm.getMediaProjection(resultCode, data);
        } catch (Throwable t) {
            Log.e(TAG, "[screen] getMediaProjection failed: " + t);
            return;
        }
        if (mp == null) {
            teardown();
            return;
        }
        projection = mp;

        final DisplayMetrics dm = new DisplayMetrics();
        if (Build.VERSION.SDK_INT >= 17) {
            activity.getWindowManager().getDefaultDisplay().getRealMetrics(dm);
        } else {
            activity.getWindowManager().getDefaultDisplay().getMetrics(dm);
        }
        final int w = dm.widthPixels;
        final int h = dm.heightPixels;

        HandlerThread ct = new HandlerThread("chatx.screen", 5);
        ct.start();
        captureThread = ct;
        final Handler ch = new Handler(ct.getLooper());

        ImageReader ir = ImageReader.newInstance(w, h, PixelFormat.RGBA_8888, /*maxImages=*/ 2);
        reader = ir;

        ir.setOnImageAvailableListener(new ImageReader.OnImageAvailableListener() {
            @Override
            public void onImageAvailable(ImageReader r) {
                onFrame(r.acquireLatestImage());
            }
        }, ch);

        VirtualDisplay vd = mp.createVirtualDisplay(
            "chatx-screen",
            w, h, dm.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            ir.getSurface(),
            null,
            ch);
        if (vd == null) {
            Log.e(TAG, "[screen] createVirtualDisplay returned null");
            teardown();
            return;
        }
        virtualDisplay = vd;

        if (!startFlag.compareAndSet(false, true)) {
            // Already running — this is a re-request; ignore.
            Log.i(TAG, "[screen] already captured; ignoring duplicate");
            return;
        }
        Log.i(TAG, "[screen] capture started @ " + w + "x" + h + " rgba8888");
    }

    /** Called from Rust (JNI) to stop the capture. */
    public static void stop() {
        teardown();
        Log.i(TAG, "[screen] capture stopped");
    }

    public static boolean isRunning() { return startFlag.get(); }

    private static synchronized void teardown() {
        startFlag.set(false);
        VirtualDisplay vd = virtualDisplay;
        virtualDisplay = null;
        ImageReader ir = reader;
        reader = null;
        MediaProjection mp = projection;
        projection = null;
        try { if (vd != null) vd.release(); } catch (Throwable ignored) {}
        try { if (ir != null) ir.close(); } catch (Throwable ignored) {}
        try { if (mp != null) mp.stop(); } catch (Throwable ignored) {}
        HandlerThread ct = captureThread;
        captureThread = null;
        if (ct != null) {
            try { ct.quitSafely(); } catch (Throwable ignored) {}
        }
    }

    /** Pull an RGBA-8888 Image out of the reader, flatten to a packed
     *  {@code w × h × 4} byte array (rowStride-aware), push it into
     *  {@link NativeBridge#screenFrameIn}, then close the Image. */
    private static void onFrame(Image img) {
        if (img == null || img.getFormat() != PixelFormat.RGBA_8888) {
            if (img != null) img.close();
            return;
        }
        try {
            int w = img.getWidth();
            int h = img.getHeight();
            Image.Plane p = img.getPlanes()[0];
            java.nio.ByteBuffer buf = p.getBuffer();
            int rowStride = p.getRowStride();
            byte[] out = new byte[w * h * 4];
            int pos = 0;
            for (int row = 0; row < h; row++) {
                buf.position(row * rowStride);
                buf.get(out, pos, w * 4);
                pos += w * 4;
            }
            NativeBridge.screenFrameIn(out, out.length, w, h, FMT_RGBA8888);
        } finally {
            img.close();
        }
    }
}
