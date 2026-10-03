package com.chatx;

import android.app.Activity;
import android.content.pm.PackageManager;
import android.graphics.ImageFormat;
import android.graphics.Rect;
import android.util.Size;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCaptureSession;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraDevice;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.CameraMetadata;
import android.hardware.camera2.CaptureRequest;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.Image;
import android.media.ImageReader;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;

import java.util.Arrays;

/**
 * Camera + microphone capture loop (M7).
 *
 * Camera via {@code Camera2} + {@link ImageReader} ({@link
 * ImageFormat#YUV_420_888}) at a resolution closest to 640×480 (prefer the
 * largest supported size ≥ 640×480). Every available frame is reduced to
 * its Y plane and pushed through
 * {@link NativeBridge#cameraFrameIn(byte[], int, int, int)} with
 * {@code fmt = 8} (Gray8) — zbar on the Rust side consumes that directly
 * with no colour-space arithmetic.
 *
 * Mic via {@code AudioRecord} (mono 16 kHz s16le) unchanged from prior
 * versions.
 *
 * Feeds {@link NativeBridge} JNI entry points which live in {@code
 * libchatx.so}.
 */
public final class Capture {
    private static final String TAG = "chatx.capture";
    /** Target size for the QR-scan camera. If the device doesn't expose
     *  this exact size we pick the closest supported one (≥ target). */
    private static final int TARGET_W = 640;
    private static final int TARGET_H = 480;
    /** Bridge PixelFormat.Gray8 (see crates/bridge/src/types.rs). */
    private static final int FMT_GRAY8 = 8;

    private static volatile HandlerThread cameraThread;
    private static volatile Handler cameraHandler;
    private static volatile HandlerThread micThread;
    private static volatile Handler micHandler;
    private static volatile Activity activityRef;

    /** Set in {@link CameraLoop#onOpened}, torn down in {@link
     *  CameraLoop#onDisconnected}/{@link #stop()}. */
    private static volatile CameraDevice cameraDevice;
    private static volatile CameraCaptureSession cameraSession;
    private static volatile ImageReader imageReader;

    public static synchronized void start(Activity activity) {
        if (cameraThread != null) {
            Log.i(TAG, "capture already running");
            return;
        }
        activityRef = activity;
        if (!hasPerm(android.Manifest.permission.CAMERA) || !hasPerm(android.Manifest.permission.RECORD_AUDIO)) {
            activity.requestPermissions(
                new String[]{android.Manifest.permission.CAMERA, android.Manifest.permission.RECORD_AUDIO}, /*req=*/ 1);
        }

        HandlerThread ct = new HandlerThread("chatx.camera", 5);
        ct.start();
        Handler ch = new Handler(ct.getLooper());
        cameraThread = ct; cameraHandler = ch;
        ch.post(new CameraLoop());

        HandlerThread mt = new HandlerThread("chatx.mic", 4);
        mt.start();
        Handler mh = new Handler(mt.getLooper());
        micThread = mt; micHandler = mh;
        mh.post(new MicLoop());

        Log.i(TAG, "capture dispatched (camera + mic)");
    }

    public static synchronized void stop() {
        if (cameraThread == null) return;

        // Tear down the camera2 session first so we don't keep pumping
        // frames into a thread we're about to quit.
        CameraCaptureSession session = cameraSession;
        CameraDevice device = cameraDevice;
        ImageReader reader = imageReader;
        cameraSession = null;
        cameraDevice = null;
        imageReader = null;
        try { if (session != null) session.close(); } catch (Throwable ignored) {}
        try { if (device  != null) device.close();  } catch (Throwable ignored) {}
        try { if (reader  != null) reader.close();  } catch (Throwable ignored) {}

        cameraThread.quitSafely(); cameraThread = null; cameraHandler = null;
        micThread.quitSafely();    micThread = null;    micHandler = null;
        activityRef = null;
        Log.i(TAG, "capture stopped");
    }

    private static boolean hasPerm(String perm) {
        Activity a = activityRef;
        if (a == null) return false;
        return a.checkSelfPermission(perm) == PackageManager.PERMISSION_GRANTED;
    }

    // ── Camera2 preview (YUV_420_888 → Y plane → GRAY8) ───────────────────
    static final class CameraLoop implements Runnable {
        /** Counts frames pushed to the Rust side, so we can throttle
         *  logs to ~1 per 30 frames. */
        private int frameN = 0;
        private int resolvedW = 0;
        private int resolvedH = 0;

        @Override
        public void run() {
            Log.i(TAG, "[camera] loop started (Camera2)");
            Activity activity = activityRef;
            if (activity == null) {
                Log.e(TAG, "[camera] no activity; aborting");
                return;
            }
            CameraManager cm = (CameraManager) activity.getSystemService(android.hardware.camera2.CameraManager.class);
            if (cm == null) {
                Log.e(TAG, "[camera] no CameraManager; aborting");
                return;
            }

            // Prefer a back-facing camera; fall back to the first one.
            String cameraId = null;
            try {
                for (String id : cm.getCameraIdList()) {
                    Integer facing = cm.getCameraCharacteristics(id)
                        .get(CameraCharacteristics.LENS_FACING);
                    if (facing != null && facing == CameraCharacteristics.LENS_FACING_BACK) {
                        cameraId = id;
                        break;
                    }
                }
                if (cameraId == null) {
                    String[] ids = cm.getCameraIdList();
                    if (ids.length == 0) {
                        Log.e(TAG, "[camera] no cameras found");
                        return;
                    }
                    cameraId = ids[0];
                }
            } catch (Throwable t) {
                Log.e(TAG, "[camera] enumerate cameras failed: " + t);
                return;
            }

            Size chosen = pickBestSize(cm, cameraId);
            if (chosen == null) {
                Log.e(TAG, "[camera] could not resolve an image size");
                return;
            }
            Log.i(TAG, "[camera] opening id=" + cameraId + " @ " + chosen.getWidth() + "x" + chosen.getHeight());
            this.resolvedW = chosen.getWidth();
            this.resolvedH = chosen.getHeight();

            final ImageReader reader = ImageReader.newInstance(
                chosen.getWidth(), chosen.getHeight(), ImageFormat.YUV_420_888, /*maxImages=*/ 2);
            imageReader = reader;
            reader.setOnImageAvailableListener(
                new ImageReader.OnImageAvailableListener() {
                    @Override public void onImageAvailable(ImageReader r) {
                        onFrame(r.acquireLatestImage());
                    }
                },
                cameraHandler);

            try {
                // CAMERA permission has already been granted by Shell.
                cm.openCamera(cameraId, new DeviceCallback(), cameraHandler);
            } catch (Throwable t) {
                Log.e(TAG, "[camera] openCamera failed: " + t);
            }
        }

        /** Pick the largest supported YUV_420_888 output size that is at
         *  least TARGET_W × TARGET_H; fall back to the overall largest if
         *  nothing meets the target. */
        private static Size pickBestSize(CameraManager cm, String id) {
            StreamConfigurationMap map;
            Size[] sizes;
            try {
                map = cm.getCameraCharacteristics(id)
                    .get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
            } catch (Throwable t) {
                Log.e(TAG, "map failed: " + t);
                return null;
            }
            if (map == null) return null;
            sizes = map.getOutputSizes(ImageFormat.YUV_420_888);
            if (sizes == null || sizes.length == 0) {
                // Device doesn't expose YUV_420_888 (very rare). Try the
                // nearest we can — fall back to a sensible default.
                return new Size(TARGET_W, TARGET_H);
            }
            Size best = null;
            for (Size s : sizes) {
                if (s.getWidth() >= TARGET_W && s.getHeight() >= TARGET_H) {
                    if (best == null || (s.getWidth() * s.getHeight()) > (best.getWidth() * best.getHeight())) {
                        best = s;
                    }
                }
            }
            if (best == null) {
                for (Size s : sizes) {
                    if (best == null || (s.getWidth() * s.getHeight()) > (best.getWidth() * best.getHeight())) {
                        best = s;
                    }
                }
            }
            return best;
        }

        private final class DeviceCallback extends CameraDevice.StateCallback {
            @Override
            public void onOpened(CameraDevice device) {
                cameraDevice = device;
                Log.i(TAG, "[camera] opened");
                startRepeating(device);
            }
            @Override
            public void onDisconnected(CameraDevice device) {
                Log.i(TAG, "[camera] disconnected");
                device.close();
                cameraDevice = null;
            }
            @Override
            public void onError(CameraDevice device, int error) {
                Log.e(TAG, "[camera] camera error code=" + error);
                device.close();
                cameraDevice = null;
            }
        }

        private void startRepeating(final CameraDevice device) {
            try {
                CaptureRequest.Builder req = device.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW);
                req.addTarget(imageReader.getSurface());
                req.set(CaptureRequest.CONTROL_MODE, CameraMetadata.CONTROL_MODE_AUTO);
                device.createCaptureSession(
                    Arrays.asList(imageReader.getSurface()),
                    new CameraCaptureSession.StateCallback() {
                        @Override
                        public void onConfigured(final CameraCaptureSession session) {
                            cameraSession = session;
                            try {
                                session.setRepeatingRequest(req.build(), null, cameraHandler);
                                Log.i(TAG, "[camera] preview started");
                            } catch (Throwable t) {
                                Log.e(TAG, "[camera] setRepeatingRequest failed: " + t);
                            }
                        }
                        @Override
                        public void onConfigureFailed(CameraCaptureSession session) {
                            Log.e(TAG, "[camera] session.configure failed");
                        }
                    },
                    cameraHandler);
            } catch (Throwable t) {
                Log.e(TAG, "[camera] createCaptureSession failed: " + t);
            }
        }

        /** Pull the Y plane out of a {@code YUV_420_888} Image, copy it
         *  into a packed {@code w × h} byte buffer (rowStride-aware), push
         *  to Rust via {@link NativeBridge#cameraFrameIn}, then close the
         *  Image. */
        private void onFrame(Image img) {
            if (img == null || img.getFormat() != ImageFormat.YUV_420_888) {
                if (img != null) img.close();
                return;
            }
            try {
                int w = img.getWidth();
                int h = img.getHeight();
                Image.Plane yPlane = img.getPlanes()[0];
                java.nio.ByteBuffer buf = yPlane.getBuffer();
                int rowStride = yPlane.getRowStride();
                byte[] yBuf = new byte[w * h];
                int pos = 0;
                for (int row = 0; row < h; row++) {
                    buf.position(row * rowStride);
                    buf.get(yBuf, pos, w);
                    pos += w;
                }
                int r = NativeBridge.cameraFrameIn(yBuf, yBuf.length, w, h, FMT_GRAY8);
                if ((frameN++ % 30) == 0) {
                    Log.i(TAG, "[camera] frame#" + frameN + " " + yBuf.length + "B " +
                        w + "x" + h + " fmt=" + FMT_GRAY8 + " ret=" + r);
                }
            } finally {
                img.close();
            }
        }
    }

    // ── Mic (AudioRecord 16 kHz mono s16le, 20 ms frames) ─────────────────
    static final class MicLoop implements Runnable {
        private static final int RATE = 16000;
        private static final int FRAME_S = 320; // 20 ms × 16000 Hz × 2 bytes

        @Override
        public void run() {
            Log.i(TAG, "[mic] loop started @ " + RATE + " Hz");
            int minBuf = android.media.AudioRecord.getMinBufferSize(RATE,
                android.media.AudioFormat.CHANNEL_IN_MONO, android.media.AudioFormat.ENCODING_PCM_16BIT);
            if (minBuf <= 0) { Log.e(TAG, "[mic] bad minBuf=" + minBuf); return; }
            android.media.AudioRecord rec = null;
            try {
                rec = new android.media.AudioRecord(android.media.MediaRecorder.AudioSource.MIC, RATE,
                    android.media.AudioFormat.CHANNEL_IN_MONO, android.media.AudioFormat.ENCODING_PCM_16BIT, minBuf * 2);
                if (rec.getState() != android.media.AudioRecord.STATE_INITIALIZED) {
                    Log.e(TAG, "[mic] not initialized"); return;
                }
                byte[] frame = new byte[FRAME_S * 2];
                rec.startRecording();
                int n = 0;
                while (!Thread.currentThread().isInterrupted()) {
                    int got = rec.read(frame, 0, frame.length);
                    if (got > 0) {
                        int r = NativeBridge.audioPcmIn(frame, got, RATE, 1);
                        if ((n++ % 200) == 0) Log.i(TAG, "[mic] frame#" + n + " got=" + got + " ret=" + r);
                    }
                }
            } catch (Throwable t) {
                Log.e(TAG, "[mic] error: " + t);
            } finally {
                if (rec != null) { try { rec.stop(); } catch (Throwable ignored) {} rec.release(); }
            }
        }
    }
}
