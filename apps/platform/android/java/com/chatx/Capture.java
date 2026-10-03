package com.chatx;

import android.app.Activity;
import android.content.pm.PackageManager;
import android.graphics.ImageFormat;
import android.graphics.PixelFormat;
import android.hardware.Camera;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.media.MediaRecorder;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;

import java.nio.ByteBuffer;

/**
 * Camera + microphone capture loop (M5-A smoke test).
 *
 * Camera via legacy Camera1 preview callback (gives us packed RGB_565 directly
 * — no YUV conversion needed to prove the Kotlin → JNI → Rust pipeline).
 * Mic via AudioRecord (mono 16 kHz s16le).
 *
 * Feeds {@link NativeBridge} JNI entry points which live in `libchatx.so`.
 */
public final class Capture {
    private static final String TAG = "chatx.capture";

    private static volatile HandlerThread cameraThread;
    private static volatile Handler cameraHandler;
    private static volatile HandlerThread micThread;
    private static volatile Handler micHandler;
    private static volatile Activity activityRef;

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

    // ── Camera1 preview (RGB_565) ─────────────────────────────────────────
    static final class CameraLoop implements Runnable {
        private static final int W = 320, H = 240;

        @Override
        public void run() {
            Log.i(TAG, "[camera] loop started (Camera1)");
            Camera cam = null;
            try {
                int index = 0;
                int n = Camera.getNumberOfCameras();
                for (int i = 0; i < n; i++) {
                    android.hardware.Camera.CameraInfo info = new android.hardware.Camera.CameraInfo();
                    android.hardware.Camera.getCameraInfo(i, info);
                    if (info.facing == android.hardware.Camera.CameraInfo.CAMERA_FACING_BACK) {
                        index = i; break;
                    }
                }
                cam = Camera.open(index);
                android.hardware.Camera.Parameters p = cam.getParameters();
                p.setPreviewFormat(ImageFormat.RGB_565);
                p.setPreviewSize(W, H);
                cam.setParameters(p);
                cam.setPreviewCallback(new Camera.PreviewCallback() {
                    private int n = 0;
                    @Override
                    public void onPreviewFrame(byte[] data, android.hardware.Camera c) {
                        if (data == null || data.length == 0) return;
                        int r = NativeBridge.cameraFrameIn(data, data.length, W, H, ImageFormat.RGB_565);
                        if ((n++ % 100) == 0) {
                            Log.i(TAG, "[camera] frame#" + n + " " + data.length + "B ret=" + r);
                        }
                    }
                });
                cam.startPreview();
                Log.i(TAG, "[camera] preview started " + W + "x" + H);
            } catch (Throwable t) {
                Log.e(TAG, "[camera] error: " + t);
            }
        }
    }

    // ── Mic (AudioRecord 16 kHz mono s16le, 20 ms frames) ─────────────────
    static final class MicLoop implements Runnable {
        private static final int RATE = 16000;
        private static final int FRAME_S = 320; // 20 ms × 16000 Hz × 2 bytes

        @Override
        public void run() {
            Log.i(TAG, "[mic] loop started" + " @ " + RATE + " Hz");
            int minBuf = android.media.AudioRecord.getMinBufferSize(RATE,
                AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT);
            if (minBuf <= 0) { Log.e(TAG, "[mic] bad minBuf=" + minBuf); return; }
            AudioRecord rec = null;
            try {
                rec = new AudioRecord(MediaRecorder.AudioSource.MIC, RATE,
                    AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, minBuf * 2);
                if (rec.getState() != AudioRecord.STATE_INITIALIZED) {
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
