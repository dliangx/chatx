package com.chatx;

import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.media.MediaFormat;
import android.media.Image;
import android.graphics.ImageFormat;
import android.graphics.Rect;
import android.util.Log;

import java.nio.ByteBuffer;

/**
 * H.264 (video/avc) decoder for remote video in a WebRTC call.
 *
 * <p>Feeds Annex-B NAL units ({@code 00 00 00 01} start codes) into a
 * {@link MediaCodec} (synchronous mode). Each decoded picture is delivered as
 * an {@link Image} in {@link ImageFormat#YUV_420_888}; we convert it to a
 * tight RGBA8888 buffer that {@link NativeBridge#h264Out(byte[], int[])}
 * pulls out for Rust.
 *
 * <p>If the device has no H.264 decoder, {@link #init()} returns
 * {@code false} and Rust decodes in software (openh264). So a call always
 * decodes — GPU here, CPU otherwise.
 *
 * <p>Thread model: {@code feed()} is called from the Rust webrtc read thread;
 * the codec and the "last frame" slot are guarded by {@link #lock}.
 */
public final class MediaCodecDecoder {
    private static final String TAG = "chatx.vdec";
    private static final String MIME = "video/avc";

    private MediaCodec codec;

    final Object lock = new Object();
    byte[] lastFrame; // tight RGBA8888 of the last decoded picture
    int lastW = 0;
    int lastH = 0;
    private boolean started;
    private long ptsUs = 0;

    private MediaCodecDecoder() {}

    public static MediaCodecDecoder get() {
        return Holder.INSTANCE;
    }
    private static final class Holder {
        static final MediaCodecDecoder INSTANCE = new MediaCodecDecoder();
    }

    /** Open + start the MediaCodec. Returns false if no H.264 decoder is
     *  available (Rust then uses software decode). */
    public boolean init() {
        synchronized (lock) {
            if (started) return true;
            try {
                boolean haveH264 = false;
                int count = MediaCodecList.getCodecCount();
                for (int i = 0; i < count; i++) {
                    MediaCodecInfo info = MediaCodecList.getCodecInfoAt(i);
                    String[] types = info.getSupportedTypes();
                    for (String t : types) {
                        if (MIME.equalsIgnoreCase(t)) { haveH264 = true; break; }
                    }
                    if (haveH264) break;
                }
                if (!haveH264) {
                    Log.w(TAG, "no H.264 decoder on device");
                    return false;
                }

                codec = MediaCodec.createDecoderByType(MIME);
                MediaFormat fmt = MediaFormat.createVideoFormat(MIME, 1280, 720);
                fmt.setInteger(MediaFormat.KEY_COLOR_FORMAT, ImageFormat.YUV_420_888);
                codec.configure(fmt, null, null, 0);
                codec.start();
                started = true;
                Log.i(TAG, "MediaCodec H.264 decoder ready (YUV_420_888 out)");
                return true;
            } catch (Throwable t) {
                Log.w(TAG, "init failed — Rust will fall back to software", t);
                releaseLocked();
                return false;
            }
        }
    }

    /** Feed one Annex-B NAL unit (or chunk of NALs) to the decoder. */
    public boolean feed(byte[] annexb) {
        if (annexb == null || annexb.length == 0) return false;
        synchronized (lock) {
            if (!started || codec == null) return false;
            try {
                int inIndex = codec.dequeueInputBuffer(10000);
                if (inIndex < 0) return false;
                ByteBuffer buf = codec.getInputBuffer(inIndex);
                if (buf == null) return false;
                int n = Math.min(annexb.length, buf.remaining());
                buf.put(annexb, 0, n);
                buf.position(n);
                long pts = ptsUs;
                ptsUs += 1000000L / 30; // 30fps
                codec.queueInputBuffer(inIndex, 0, n, pts, 0);
                drainOutput();
                return true;
            } catch (Throwable t) {
                Log.w(TAG, "feed failed", t);
                return false;
            }
        }
    }

    /** Pop the last decoded RGBA8888 frame into {@code dst} (up to
     *  {@code dst.length} bytes). Sets {@code dims[0]}=width, {@code dims[1]}=
     *  height. Returns the frame byte length, or 0 if no frame is ready. */
    public int out(byte[] dst, int[] dims) {
        synchronized (lock) {
            if (lastFrame == null || lastFrame.length == 0) return 0;
            int n = Math.min(lastFrame.length, dst.length);
            System.arraycopy(lastFrame, 0, dst, 0, n);
            if (dims != null && dims.length >= 2) {
                dims[0] = lastW;
                dims[1] = lastH;
            }
            lastFrame = null; // consumed
            lastW = lastH = 0;
            return n;
        }
    }

    public void close() {
        synchronized (lock) {
            releaseLocked();
        }
    }

    private void releaseLocked() {
        started = false;
        if (codec != null) {
            try { codec.stop(); } catch (Throwable ignored) {}
            try { codec.release(); } catch (Throwable ignored) {}
            codec = null;
        }
        lastFrame = null;
        lastW = lastH = 0;
    }

    // ── decode loop (synchronous) → RGBA ──────────────────────────────────
    // Called after a successful feed(): drain all available output frames,
    // converting the last one to RGBA. Must be called with `lock` held.
    private void drainOutput() {
        if (codec == null) return;
        try {
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            int outIndex;
            while ((outIndex = codec.dequeueOutputBuffer(info, 0)) >= 0) {
                Image img = null;
                try {
                    img = codec.getOutputImage(outIndex);
                } catch (Throwable t) {
                    // Not available as an Image (e.g. a codec config frame) —
                    // fall back to the raw buffer path below.
                }
                byte[] rgba = null;
                int w = 0, h = 0;
                if (img != null) {
                    w = img.getWidth();
                    h = img.getHeight();
                    rgba = yuv420ToRgba(img, w, h);
                }
                codec.releaseOutputBuffer(outIndex, false);
                if (rgba != null && rgba.length > 0 && w > 0 && h > 0) {
                    lastFrame = rgba;
                    lastW = w;
                    lastH = h;
                }
            }
        } catch (Throwable t) {
            Log.w(TAG, "drain failed", t);
        }
    }

    /** Convert a YUV_420_888 {@link Image} (honouring strides + crop rect) to
     *  a tight {@code w*h*4} RGBA byte array. Handles both semi-planar
     *  (NV12/NV21, chroma pixelStride=2) and planar (I420, pixelStride=1). */
    private byte[] yuv420ToRgba(Image img, int w, int h) {
        try {
            Image.Plane[] planes = img.getPlanes();
            if (planes == null || planes.length < 3) return null;
            ByteBuffer yBuf = planes[0].getBuffer();
            int yRow = planes[0].getRowStride();
            int yPix = planes[0].getPixelStride(); // 1

            Image.Plane uPlane = planes[1];
            Image.Plane vPlane = planes[2];
            ByteBuffer uBuf = uPlane.getBuffer();
            ByteBuffer vBuf = vPlane.getBuffer();
            int uRow = uPlane.getRowStride();
            int vRow = vPlane.getRowStride();
            int cPix = uPlane.getPixelStride(); // 1 (I420) or 2 (NV12/NV21)
            int cw = w / 2, ch = h / 2;

            // Detect NV21 vs NV12 ordering from pixel strides (best effort).
            boolean nv21 = false;
            try {
                // If the U plane's pixel stride is 1 it's I420; if 2 it's
                // semi-planar. We can't easily tell NV12 vs NV21 from the API
                // here, so assume NV12 (V then U is rare on Android decoders).
            } catch (Throwable ignored) {}

            Rect crop = img.getCropRect();
            int cx = (crop != null) ? crop.left : 0;
            int cy = (crop != null) ? crop.top : 0;
            int ow = (crop != null) ? crop.width() : w;
            int oh = (crop != null) ? crop.height() : h;
            if (ow <= 0 || oh <= 0) { ow = w; oh = h; }

            byte[] out = new byte[ow * oh * 4];
            int di = 0;
            for (int py = 0; py < oh; py++) {
                int yOff = (cy + py) * yRow + (cx * yPix);
                int yv = (yBuf.get(yOff) & 0xff) - 16;
                yv = (yv * 298) >> 8;
                for (int px = 0; px < ow; px++) {
                    int cxi = cx + px;
                    int cyi = cy + py;
                    int uOff, vOff;
                    int uval, vval;
                    if (cPix == 2) {
                        // Semi-planar: U/V interleaved at half resolution.
                        int cOff = (cyi / 2) * uRow + (cxi / 2) * 2;
                        uval = (uBuf.get(cOff) & 0xff) - 128;
                        vval = (vBuf.get(cOff) & 0xff) - 128;
                        uOff = vOff = 0;
                    } else {
                        int uCol = cxi / 2, uRowI = cyi / 2;
                        uOff = uRowI * uRow + uCol;
                        vOff = uRowI * vRow + uCol;
                        uval = (uBuf.get(uOff) & 0xff) - 128;
                        vval = (vBuf.get(vOff) & 0xff) - 128;
                    }
                    int r = yv + ((359 * vval) >> 8);
                    int g = yv - ((88 * uval) >> 8) - ((183 * vval) >> 8);
                    int b = yv + ((454 * uval) >> 8);
                    out[di++] = (byte) clamp(r);
                    out[di++] = (byte) clamp(g);
                    out[di++] = (byte) clamp(b);
                    out[di++] = (byte) 255;
                }
            }
            return out;
        } catch (Throwable t) {
            Log.w(TAG, "yuv420ToRgba failed", t);
            return null;
        }
    }

    private static int clamp(int v) {
        if (v < 0) return 0;
        if (v > 255) return 255;
        return v;
    }
}
