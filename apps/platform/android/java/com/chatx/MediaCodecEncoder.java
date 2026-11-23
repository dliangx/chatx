package com.chatx;

import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.media.MediaFormat;
import android.media.Image;
import android.graphics.ImageFormat;
import android.util.Log;

import java.nio.ByteBuffer;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * H.264 (video/avc) **encoder** for local video out in a WebRTC call.
 *
 * <p>Consumes RGBA8888 frames (converted to YUV_420_888 for the
 * {@link MediaCodec} input image) and emits **Annex-B** NAL streams
 * (start-code prefixed, SPS/PPS leading) that Rust pulls out via
 * {@link NativeBridge#h264EncOut(int, byte[])} and writes into the webrtc
 * track.
 *
 * <p>MediaCodec emits **avcC** (4-byte length-prefixed NALs). The chatx wire
 * format is **Annex-B** (what openh264 produces and the webrtc
 * {@code H264Payloader} consumes), so this class re-frames avcC → Annex-B:
 * the codec-config frame (SPS/PPS) is captured and prepended to the first
 * encoded sample, and each sample's NALs are start-code prefixed — matching
 * openh264's "SPS, PPS, IDR" stream order.
 *
 * <p>A call can run several encoders at once (camera + screen), so each
 * {@code handle} identifies one {@link MediaCodec}. If a device has no H.264
 * encoder, {@code new} returns handle 0 and Rust encodes in software
 * (openh264). So a call always encodes — GPU here, CPU otherwise.
 *
 * <p>Thread model: {@code feed} is called from the Rust encode thread; each
 * instance's codec + "last NALU" slot are guarded by {@link #lock}.
 */
public final class MediaCodecEncoder {
    private static final String TAG = "chatx.venc";
    private static final String MIME = "video/avc";

    private MediaCodec codec;         // not final: created lazily on first feed / re-created
    private int cfgW = 0;             // resolution the codec was configured at
    private int cfgH = 0;
    private byte[] spsPps = null;     // Annex-B SPS+PPS, prepended to first sample

    private final Object lock = new Object();
    private byte[] lastNalu;          // Annex-B stream of the last encoded sample
    private boolean started;
    private long ptsUs = 0;
    private static final long FRAME_US = 1000000L / 30;  // 30fps presentation step
    // MediaCodec requires input timestamps to be < 2^33 µs and strictly
    // increasing. 2^33 µs ≈ 29.8 min, so an unbounded counter would overflow
    // on any long call and wedge the encoder — keep the headroom check below.
    private static final long MAX_PTS_US = 0x20000000L;

    // ── handle registry (multiple concurrent encoders: camera + screen) ────
    private static final Map<Integer, MediaCodecEncoder> INSTANCES =
            new ConcurrentHashMap<>();
    private static final AtomicInteger NEXT = new AtomicInteger(1);

    /** Allocate a new encoder handle. The {@link MediaCodec} itself is created
     *  lazily on the first {@link #feed} so it can be configured at the *real*
     *  frame resolution (a screen share is full-desktop size, not 1280×720 —
     *  configuring at a fixed size and then writing larger frames overflowed
     *  the input buffers). Returns a handle (>0), or 0 if the device has no
     *  H.264 encoder (Rust then falls back to software). */
    public static int newHandle() {
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
                Log.w(TAG, "no H.264 encoder on device");
                return 0;
            }
            int h = NEXT.getAndIncrement();
            MediaCodecEncoder inst = new MediaCodecEncoder();
            inst.started = true;
            INSTANCES.put(h, inst);
            return h;
        } catch (Throwable t) {
            Log.w(TAG, "newHandle failed — Rust will fall back to software", t);
            return 0;
        }
    }

    /** Look up an encoder by handle. */
    public static MediaCodecEncoder get(int handle) {
        return INSTANCES.get(handle);
    }

    /** Release + drop an encoder by handle. */
    public static void close(int handle) {
        MediaCodecEncoder inst = INSTANCES.remove(handle);
        if (inst != null) inst.releaseLocked();
    }

    private MediaCodecEncoder() {}

    /** Create + configure + start the codec at the given resolution. Called on
     *  the first {@link #feed} (lazy) and whenever the stream resolution
     *  changes. Must be called with {@link #lock} held. On failure {@code
     *  started} is cleared so {@link #feed} returns false and Rust falls back
     *  to software. */
    private void configureLocked(int w, int h) {
        if (codec != null) {
            try { codec.stop(); } catch (Throwable ignored) {}
            try { codec.release(); } catch (Throwable ignored) {}
            codec = null;
        }
        ptsUs = 0;      // fresh base for the new codec
        spsPps = null;  // fresh IDR will re-emit SPS/PPS
        try {
            MediaFormat fmt = MediaFormat.createVideoFormat(MIME, w, h);
            fmt.setInteger(MediaFormat.KEY_BIT_RATE, 2_000_000);
            fmt.setInteger(MediaFormat.KEY_FRAME_RATE, 30);
            fmt.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2);
            fmt.setInteger(MediaFormat.KEY_COLOR_FORMAT,
                    MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420SemiPlanar);
            MediaCodec c = MediaCodec.createEncoderByType(MIME);
            c.configure(fmt, null, null, 0);
            c.start();
            codec = c;
            cfgW = w;
            cfgH = h;
            Log.i(TAG, "MediaCodec H.264 encoder configured " + w + "x" + h);
        } catch (Throwable t) {
            Log.w(TAG, "configureCodec failed at " + w + "x" + h + " — Rust will fall back to software", t);
            started = false;
        }
    }

    /** Feed one RGBA8888 frame (`rgba.length == w*h*4`). Encodes it and
     *  re-frames the output into Annex-B (SPS/PPS + the sample NALs). */
    public boolean feed(byte[] rgba, int w, int h) {
        if (rgba == null || w <= 0 || h <= 0) return false;
        synchronized (lock) {
            if (!started) return false;
            try {
                // Create the codec on first feed (lazy — we only know the real
                // resolution here), and re-create it whenever the stream
                // resolution changes. Configuring at the actual frame size is
                // what keeps the input buffers from overflowing.
                if (codec == null || cfgW != w || cfgH != h) {
                    configureLocked(w, h);
                }
                // MediaCodec requires input timestamps to stay below 2^33 µs
                // (~29.8 min) and strictly increasing. When we get close, tear
                // down + recreate the codec so the timestamp base restarts at 0.
                // The receiver simply sees a fresh SPS/PPS+IDR (a re-key) — the
                // webrtc payloader re-emits a STAP-A, so playback continues
                // without a visible gap.
                if (ptsUs + FRAME_US > MAX_PTS_US) {
                    configureLocked(w, h);
                }
                if (codec == null || !started) return false; // configure failed
                int inIndex = codec.dequeueInputBuffer(10000);
                if (inIndex < 0) return false;
                Image img = null;
                try {
                    img = codec.getInputImage(inIndex);
                } catch (Throwable t) {
                    // Some encoders don't support image input — fall back to
                    // the raw-buffer path (RGBA) below.
                }
                if (img != null) {
                    fillYuv(rgba, w, h, img);
                    img.close();
                } else {
                    ByteBuffer buf = codec.getInputBuffer(inIndex);
                    if (buf == null) return false;
                    int n = Math.min(rgba.length, buf.remaining());
                    buf.put(rgba, 0, n);
                    buf.position(n);
                }
                codec.queueInputBuffer(inIndex, 0, Math.min(rgba.length, w * h * 4),
                        ptsUs, 0);
                ptsUs += FRAME_US;
                drainOutput();
                return true;
            } catch (Throwable t) {
                Log.w(TAG, "feed failed", t);
                return false;
            }
        }
    }

    /** Pop the last encoded Annex-B NAL stream into {@code dst} (up to
     *  {@code dst.length} bytes). Returns the stream byte length, or 0 if no
     *  frame is ready. */
    public int out(byte[] dst) {
        synchronized (lock) {
            if (lastNalu == null || lastNalu.length == 0) return 0;
            int n = Math.min(lastNalu.length, dst.length);
            System.arraycopy(lastNalu, 0, dst, 0, n);
            lastNalu = null;
            return n;
        }
    }

    private void releaseLocked() {
        started = false;
        if (codec != null) {
            try { codec.stop(); } catch (Throwable ignored) {}
            try { codec.release(); } catch (Throwable ignored) {}
        }
        spsPps = null;
        lastNalu = null;
    }

    // ── encode drain → Annex-B ─────────────────────────────────────────────
    // Called after a successful feed(). Drains all available output buffers,
    // capturing the codec-config frame (SPS/PPS) and re-framing each sample
    // into Annex-B (start-code prefixed NALs, SPS/PPS leading on the first
    // one). Must be called with `lock` held.
    private void drainOutput() {
        if (codec == null) return;
        try {
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            int outIndex;
            byte[] sample = null;
            while ((outIndex = codec.dequeueOutputBuffer(info, 0)) >= 0) {
                if ((info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0) {
                    captureSpsPps(outIndex, info.offset, info.size);
                } else if ((info.flags & MediaCodec.BUFFER_FLAG_END_OF_STREAM) == 0
                        && info.size > 0) {
                    sample = readAnnexbSample(outIndex, info.offset, info.size);
                }
                codec.releaseOutputBuffer(outIndex, false);
            }
            if (sample != null) {
                lastNalu = sample;
            }
        } catch (Throwable t) {
            Log.w(TAG, "drain failed", t);
        }
    }

    /** Capture SPS/PPS from a codec-config output buffer (avcC: length
     *  prefixed NALs) into {@link #spsPps} (start-code prefixed). */
    private void captureSpsPps(int index, int offset, int size) {
        ByteBuffer buf = codec.getOutputBuffer(index);
        if (buf == null) return;
        byte[] raw = new byte[size];
        buf.position(offset);
        buf.get(raw);
        byte[] ab = avccToAnnexb(raw, 0, size);
        if (ab.length > 0) {
            spsPps = ab;
        }
    }

    /** Read one output sample (avcC) and re-frame it as Annex-B, prepending
     *  the captured SPS/PPS (once). */
    private byte[] readAnnexbSample(int index, int offset, int size) {
        ByteBuffer buf = codec.getOutputBuffer(index);
        if (buf == null) return null;
        byte[] raw = new byte[size];
        buf.position(offset);
        buf.get(raw);
        byte[] annexb = avccToAnnexb(raw, 0, size);
        if (spsPps != null) {
            byte[] out = new byte[spsPps.length + annexb.length];
            System.arraycopy(spsPps, 0, out, 0, spsPps.length);
            System.arraycopy(annexb, 0, out, spsPps.length, annexb.length);
            spsPps = null; // emitted once
            return out;
        }
        return annexb;
    }

    /** Convert an avcC byte range (4-byte length-prefixed NALs) to Annex-B
     *  (start-code prefixed NALs). */
    private static byte[] avccToAnnexb(byte[] raw, int off, int size) {
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        int i = off;
        int end = off + size;
        while (i + 4 <= end) {
            int l = ((raw[i] & 0xff) << 24) | ((raw[i + 1] & 0xff) << 16)
                    | ((raw[i + 2] & 0xff) << 8) | (raw[i + 3] & 0xff);
            i += 4;
            if (l <= 0 || i + l > end) break;
            out.write(0x00); out.write(0x00); out.write(0x00); out.write(0x01);
            out.write(raw, i, l);
            i += l;
        }
        return out.toByteArray();
    }

    /** Fill a YUV_420_888 {@link Image} (honouring strides + semi-planar vs
     *  planar chroma) from a tight RGBA8888 buffer. */
    private static void fillYuv(byte[] rgba, int w, int h, Image img) {
        Image.Plane[] planes = img.getPlanes();
        if (planes == null || planes.length < 3) return;
        int yRow = planes[0].getRowStride();
        ByteBuffer yBuf = planes[0].getBuffer();
        int uRow = planes[1].getRowStride();
        int vRow = planes[2].getRowStride();
        int cPix = planes[1].getPixelStride(); // 1 (I420) or 2 (semi-planar)
        int cw = w / 2, ch = h / 2;

        for (int py = 0; py < h; py++) {
            int yOff = py * yRow;
            int rowBase = py * w * 4;
            for (int px = 0; px < w; px++) {
                int r = rgba[rowBase + px * 4] & 0xff;
                int g = rgba[rowBase + px * 4 + 1] & 0xff;
                int b = rgba[rowBase + px * 4 + 2] & 0xff;
                int y = ((66 * r) + (129 * g) + (25 * b) + 128) >> 8;
                yBuf.put(yOff + px, (byte) clamp(y));
            }
        }
        for (int cy = 0; cy < ch; cy++) {
            for (int cx = 0; cx < cw; cx++) {
                int px = cx * 2;
                int py = cy * 2;
                int rowBase = py * w * 4;
                int r = rgba[rowBase + px * 4] & 0xff;
                int g = rgba[rowBase + px * 4 + 1] & 0xff;
                int b = rgba[rowBase + px * 4 + 2] & 0xff;
                int u = ((-38 * r) - (74 * g) + (112 * b) + 128) >> 8;
                int v = ((112 * r) - (94 * g) - (18 * b) + 128) >> 8;
                if (cPix == 2) {
                    // Semi-planar: U and V interleaved in plane 1.
                    int cOff = cy * uRow + cx * 2;
                    planes[1].getBuffer().put(cOff, (byte) clamp(u + 128));
                    planes[1].getBuffer().put(cOff + 1, (byte) clamp(v + 128));
                } else {
                    int cOff = cy * uRow + cx;
                    planes[1].getBuffer().put(cOff, (byte) clamp(u + 128));
                    planes[2].getBuffer().put(cy * vRow + cx, (byte) clamp(v + 128));
                }
            }
        }
    }

    private static int clamp(int v) {
        if (v < 0) return 0;
        if (v > 255) return 255;
        return v;
    }
}
