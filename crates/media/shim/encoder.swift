import Foundation
import CoreMedia
import CoreVideo
import VideoToolbox

// ──────────────────────────────────────────────────────────────────────────────
// Chatx H.264 encode shim — VideoToolbox (hardware / GPU) encode → Rust.
//
// C-ABI surface (called from the Rust `media` crate, `src/apple.rs`):
//   chatx_h264_enc_new()                          -> Int32  (opaque handle; 0 = fail)
//   chatx_h264_enc_feed(handle, rgba, len, w, h)  -> Int32  (encode one RGBA frame)
//   chatx_h264_enc_out(handle, dst, cap)          -> Int32  (copy the encoded Annex-B
//                                                            NAL stream; 0 = none)
//   chatx_h264_enc_close(handle)                  -> void
//
// A **handle** identifies one `VTCompressionSession`. A call can run several
// encoders at once (camera + screen), so each `VidSource` gets its own handle
// and session. `dst` is allocated by Rust (grow-on-demand, like the decoder).
//
// VideoToolbox emits **avcC** samples (each NAL in the output sample buffer is
// 4-byte length-prefixed). The chatx wire format is **Annex-B** (start-code
// prefixed — what openh264 produces and the webrtc `H264Payloader` consumes),
// so we convert avcC → Annex-B here. SPS/PPS are pulled from the first output
// buffer's format description and prepended (as standalone NALs) to the first
// encoded frame, matching openh264's "SPS, PPS, IDR" output order that the
// payloader re-packages as STAP-A + single-NAL units.
//
// Note: this preview SDK has removed `VTCompressionSessionStart` /
// `SetProperty`; a session is driven directly with `PrepareToEncodeFrames` +
// `EncodeFrame` (verified to produce real H.264 output). Requires macOS 14+.
//
// ── Safety vs. async callbacks ───────────────────────────────────────────────
// `VTCompressionSessionEncodeFrame` is **asynchronous**: its output callback
// runs on a VideoToolbox worker thread (via xpc / dispatch), and may *still be
// in flight* when the *next* `feed` returns an error and Rust goes to close.
// If we invalidated the session — or let this `EncInst` be deallocated — while
// that callback was running, VideoToolbox crashed inside
// `VTEncoderSessionEmitEncodedFrame` / `FigSimpleMutexLock` (the SIGSEGV we
// observed in the field).
//
// To close this cleanly we:
//   1. Track a `pending` count of frames we asked VideoToolbox to emit and
//      have not yet seen the callback for. The callback decrements it; the
//      registry holds its own strong ref for the instance's whole lifetime, so
//      the `EncInst` cannot be deallocated while `pending > 0` (because the
//      callback holds its own strong ref, see (2)).
//   2. Inside the output callback we `retain()` the `EncInst` for the
//      callback's duration, so it survives any concurrent `close()` on the
//      Rust side. The callback only touches its own owned state (no session
//      calls) and drops the ref on exit.
//   3. **Defer `VTCompressionSessionInvalidate` to `deinit`.** `deinit` only
//      runs when the last strong ref is released — which cannot happen until
//      every in-flight callback has exited (each one holds a ref). This
//      eliminates the use-after-free entirely without needing to wait on a
//      condition under the same lock the callback acquires.
// ──────────────────────────────────────────────────────────────────────────────

private let kChatxEncBGRA: OSType = 0x42475241 // 'BGRA'

/// Strong references to `EncInst`s that were closed while a VideoToolbox
/// worker was still in flight (the `pending` count never drained). Holding a
/// strong ref keeps the `VTCompressionSession` alive so the in-flight worker
/// cannot run `EmitEncodedFrame` on a freed session (the SIGSEGV in
/// `FigSimpleMutexLock`). These are intentionally leaked — freeing them would
/// be the crash we are avoiding. One per stuck encoder, on a rare error path.
final class LeakedInstances {
    static let shared = LeakedInstances()
    private let lock = NSLock()
    private var insts: [EncInst] = []
    func add(_ inst: EncInst) {
        lock.lock(); defer { lock.unlock() }
        insts.append(inst)
    }
}

/// One `VTCompressionSession` + its encode state, keyed by handle.
final class EncInst {
    let lock = NSLock()
    var session: VTCompressionSession?
    var sessW: Int = 0
    var sessH: Int = 0
    /// Completed Annex-B NAL streams, ready to be pulled by Rust (`out`). The
    /// output callback appends a frame here (under `lock`); `out` pops the
    /// front. A queue (not a single slot) because VideoToolbox emits
    /// asynchronously and can have more than one frame in flight — Rust drains
    /// it via repeated `out`.
    var outQueue: [[UInt8]] = []
    var spsPps: [UInt8] = []
    var nextPts: Int64 = 0
    var lastErr: Int32 = 0

    /// Count of frames we have handed to `VTCompressionSessionEncodeFrame`
    /// but for which the output callback has not yet fired. Bumped on
    /// successful `EncodeFrame`, decremented when the callback runs.
    ///
    /// This is the bridge between "close ran" and "is it safe to invalidate":
    /// `close()` waits (under `lock`) until `pending == 0` *or* a 5-second
    /// timeout, then clears the session reference. `deinit` is the one place
    /// that actually calls `VTCompressionSessionInvalidate`, so the session is
    /// only invalidated after every strong ref is gone — and a strong ref is
    /// held by every in-flight callback (see (2) in the file header).
    ///
    /// Internal (not private) so the file-scope output callback can touch it.
    var pending = 0

    /// Latched by `close()` so a concurrent `encodeImage` caller sees it has
    /// lost the race and stops trying to use the session.
    private var closeRequested = false

    deinit {
        // Only `deinit` invalidates the session. By the time we are here, no
        // in-flight callback can still hold a strong ref (each retains the
        // instance for its duration), so no VideoToolbox thread can be inside
        // an emit for this session anymore. Safe to invalidate.
        if let s = session {
            VTCompressionSessionInvalidate(s)
        }
        session = nil
    }

    // ── session (re)creation ──────────────────────────────────────────────
    // Assumes `lock` is already held (NSLock is not recursive).
    @discardableResult
    func ensureSessionLocked(w: Int, h: Int) -> Bool {
        if session != nil, w == sessW, h == sessH { return true }
        if session != nil {
            // A session is being replaced (resolution change). Wait for any
            // in-flight callback to finish touching it before we clear the
            // reference. `drainPending` releases the lock while it polls, so
            // the callback can finish its `pending -= 1`.
            lock.unlock()
            let drained = drainPending(timeout: 5.0)
            lock.lock()
            if drained {
                session = nil
            } else {
                // A worker is still in flight for the *old* session. We cannot
                // safely invalidate it (that would be the same UAF as `close`).
                // Leak the old session (keep its ref) and proceed to create a
                // fresh one; the old instance stays alive via `LeakedInstances`.
                FileHandle.standardError.write(Data("[venc] replace: old worker in flight — leaking old session\n".utf8))
                lock.unlock()
                LeakedInstances.shared.add(self)
                lock.lock()
            }
        }
        outQueue = []
        spsPps = []
        guard w > 0, h > 0 else { return false }
        var s: VTCompressionSession?
        let st = VTCompressionSessionCreate(
            allocator: nil,
            width: Int32(w),
            height: Int32(h),
            codecType: kCMVideoCodecType_H264,
            encoderSpecification: nil,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: encOutputCallback,
            refcon: Unmanaged.passUnretained(self).toOpaque(),
            compressionSessionOut: &s
        )
        guard st == 0, let session = s else {
            FileHandle.standardError.write(Data("[venc] SessionCreate=\(st)\n".utf8))
            return false
        }
        let prep = VTCompressionSessionPrepareToEncodeFrames(session)
        FileHandle.standardError.write(Data("[venc] PrepareToEncodeFrames=\(prep) \(w)x\(h)\n".utf8))
        guard prep == 0 else {
            // Session could not be prepared (GPU encoder unavailable / not
            // ready). Returning false lets the Rust side fall back to software
            // *immediately* instead of burning a 5s EncodeFrame timeout per
            // frame (which is what the field log's "EncodeFrame timeout →
            // code -2" was doing).
            VTCompressionSessionInvalidate(session)
            return false
        }
        self.session = session
        sessW = w; sessH = h
        return true
    }

    /// Poll `pending` until it reaches zero, or until the deadline. Returns
    /// `true` if `pending` reached zero (safe to invalidate the session),
    /// `false` if we hit the deadline with `pending` still > 0 (a VideoToolbox
    /// worker may still be mid-emit; the session must be leaked, not freed).
    ///
    /// Called **without** holding `lock`: we poll every ~1 ms, taking the lock
    /// only briefly to read `pending`. The callback takes the same lock to
    /// decrement `pending`, so by the time we observe `pending == 0` the
    /// callback has fully returned and VideoToolbox is done emitting.
    private func drainPending(timeout: TimeInterval) -> Bool {
        let deadline = Date(timeIntervalSinceNow: timeout)
        while true {
            lock.lock()
            let p = pending
            lock.unlock()
            if p == 0 { return true }
            if Date() >= deadline { return false }
            Thread.sleep(forTimeInterval: 0.001)
        }
    }

    /// Tear down this instance. If every in-flight VideoToolbox worker has
    /// finished (or there are none), we clear the session ref and let `deinit`
    /// invalidate it safely. If a worker is still in flight (the timeout case —
    /// the service is gone / stuck), we **leak** the instance (and with it the
    /// session) into a global set instead of freeing it, because freeing the
    /// session while the xpc worker is inside `EmitEncodedFrame` is exactly
    /// what caused the SIGSEGV in `FigSimpleMutexLock`.
    func close() {
        lock.lock()
        closeRequested = true
        lock.unlock()
        // Give a (slow but alive) in-flight worker a short grace period to
        // finish its emit so we can release the session cleanly. If it does
        // not drain, leak the instance (and with it the session) instead of
        // freeing it under a running VideoToolbox worker — freeing is exactly
        // what caused the SIGSEGV in `FigSimpleMutexLock`.
        let drained = drainPending(timeout: 2.0)
        lock.lock()
        if drained {
            // Safe: no worker is in flight. Clear the session ref; `deinit`
            // invalidates it once the last strong ref is gone.
            session = nil
            outQueue = []
            spsPps = []
        } else {
            FileHandle.standardError.write(Data("[venc] close: worker in flight — leaking session to avoid UAF\n".utf8))
        }
        lock.unlock()
        if !drained {
            LeakedInstances.shared.add(self)
        }
    }

    // ── per-frame encode ──────────────────────────────────────────────────
    // The output callback may fire synchronously on THIS thread (inside
    // `EncodeFrame`) or on a VideoToolbox worker thread. It acquires `lock`,
    // so we must NOT hold `lock` while calling `EncodeFrame` or waiting on
    // `sem` (or the callback deadlocks on the lock we hold).
    func feed(_ rgba: UnsafePointer<UInt8>, _ len: Int, _ w: Int, _ h: Int) -> Int32 {
        guard w > 0, h > 0, len >= w * h * 4 else { return -1 }
        guard let pb = Self.rgbaToBgra(rgba, w, h) else { return -1 }
        return self.encodeImage(pb, w, h)
    }

    /// Encode an existing `CVPixelBuffer` directly — no CPU pixel conversion.
    /// The buffer is only *borrowed* for the duration of `EncodeFrame` (the
    /// encoder reads it synchronously before returning), so the caller keeps
    /// ownership / a `Retain` on it. This is the fast path a capture source
    /// (camera / screen) that already produces a `CVPixelBuffer` can use to
    /// avoid a round-trip through CPU RGBA.
    func feedCvpb(_ pb: CVPixelBuffer, _ w: Int, _ h: Int) -> Int32 {
        self.encodeImage(pb, w, h)
    }

    /// Shared core: ensure a session for (w,h), then hand `image` to
    /// `VTCompressionSessionEncodeFrame` and **return immediately**.
    ///
    /// The encoded output is produced **asynchronously** on a VideoToolbox
    /// worker thread: the output callback fires later and appends the Annex-B
    /// stream to `outQueue`, which Rust pulls via `out`. We deliberately do NOT
    /// wait for the callback here — blocking on a single frame's async callback
    /// is exactly what wedged the old synchronous design (the "GPU feed failed
    /// → switch to software" field bug). `pb` must outlive the call (it does —
    /// the caller holds a `Retain` or it is freshly allocated); the encoder
    /// reads it synchronously before `EncodeFrame` returns.
    private func encodeImage(_ pb: CVPixelBuffer, _ w: Int, _ h: Int) -> Int32 {
        var s: VTCompressionSession?
        lock.lock()
        if closeRequested { lock.unlock(); return -1 }
        guard w > 0, h > 0 else { lock.unlock(); return -1 }
        guard ensureSessionLocked(w: w, h: h) else { lock.unlock(); return -1 }
        s = self.session
        guard s != nil else { lock.unlock(); return -1 }
        let pts = CMTimeMake(value: nextPts, timescale: 30)
        let dur = CMTimeMake(value: 1, timescale: 30)
        nextPts += 1
        // Reserve one slot of `pending` for the callback we are about to ask
        // for. The callback will decrement it when it fires. (Still needed for
        // the safe-close / leak decision in `close()`.)
        pending += 1
        lock.unlock()
        guard let s = s else {
            lock.lock(); pending -= 1; lock.unlock()
            return -1
        }

        let st = VTCompressionSessionEncodeFrame(
            s,
            imageBuffer: pb,
            presentationTimeStamp: pts,
            duration: dur,
            frameProperties: nil,
            sourceFrameRefcon: nil,
            infoFlagsOut: nil
        )
        if st != 0 {
            // `EncodeFrame` itself rejected the frame (session torn down, bad
            // buffer, …). The callback will NOT fire for this frame — undo the
            // `pending` reservation and report a hard failure so Rust can drop
            // the frame / fall back.
            lock.lock()
            pending -= 1
            lastErr = Int32(st)
            lock.unlock()
            FileHandle.standardError.write(Data("[venc] EncodeFrame=\(st)\n".utf8))
            return Int32(st)
        }
        // Accepted. The output callback will fire later on a VideoToolbox
        // worker and queue the Annex-B stream for `out`. Return success now.
        return 0
    }

    // ── output (Rust-owned destination) ───────────────────────────────────
    /// Pop the front completed Annex-B stream into `dst` (grow-on-demand via
    /// the caller's repeated `out`). Returns the frame's byte length, or `0`
    /// when nothing is queued yet. Non-blocking: if no frame is ready the
    /// caller simply polls again on the next tick.
    func out(dst: UnsafeMutablePointer<UInt8>, cap: Int) -> Int32 {
        lock.lock(); defer { lock.unlock() }
        guard !outQueue.isEmpty else { return 0 }
        let total = outQueue[0].count
        if cap < total {
            return Int32(total) // need a bigger buffer; frame kept for retry
        }
        let frame = outQueue.removeFirst()
        dst.update(from: frame, count: total)
        return Int32(total)
    }
}

/// File-scope VideoToolbox output callback (top-level so it does not
/// self-reference). Fires on this instance's encode thread or a VT worker.
private func encOutputCallback(
    _ refCon: UnsafeMutableRawPointer?,
    _ sourceFrameRefCon: UnsafeMutableRawPointer?,
    _ status: OSStatus,
    _ infoFlags: VTEncodeInfoFlags,
    _ sampleBuffer: CMSampleBuffer?
) {
    guard let raw = refCon else { return }
    // Retain the instance for the callback's duration. This runs on a
    // VideoToolbox worker thread and may outlive the last strong reference
    // the registry held. Retaining (balanced by the `defer` below) guarantees
    // the `EncInst` (and everything it owns — including the
    // `VTCompressionSession`, whose `deinit` invalidates it) stays alive until
    // we are done, so a concurrent `close()` / dealloc on the Rust side cannot
    // free it under our feet.
    //
    // We use `takeUnretainedValue` + `retain` (NOT `takeRetainedValue`):
    // `takeRetainedValue` assumes the caller already owns a +1 ref we are
    // taking over, but the callback does NOT — it just received an opaque
    // pointer. `takeRetainedValue` would under-count and free the instance
    // prematurely, defeating the whole point of retaining.
    let inst = Unmanaged<EncInst>.fromOpaque(raw).takeUnretainedValue()
    Unmanaged<EncInst>.passUnretained(inst).retain()
    defer { Unmanaged<EncInst>.passUnretained(inst).release() }

    inst.lock.lock(); defer { inst.lock.unlock() }
    // No `sem.signal()` here: the consumer (Rust `out`) no longer blocks on a
    // per-frame semaphore. It simply polls `outQueue`, so the callback only has
    // to enqueue the result (or nothing, on a hard failure) and release the
    // `pending` reservation that `encodeImage` made.
    if status != 0 {
        inst.lastErr = Int32(status)
        inst.pending -= 1
        return
    }
    guard let sb = sampleBuffer else {
        inst.lastErr = -4
        inst.pending -= 1
        return
    }
    // First output frame carries the format description (SPS/PPS).
    if inst.spsPps.isEmpty, let fd = CMSampleBufferGetFormatDescription(sb) {
        _ = EncInst.extractSpsPps(fd, into: &inst.spsPps)
    }
    // Read the avcC sample data (each NAL has a 4-byte length prefix).
    guard let bb = CMSampleBufferGetDataBuffer(sb) else {
        inst.lastErr = -5
        inst.pending -= 1
        return
    }
    let total = Int(CMBlockBufferGetDataLength(bb))
    var data = [UInt8](repeating: 0, count: total)
    let copied = data.withUnsafeMutableBufferPointer { buf -> Bool in
        guard let base = buf.baseAddress else { return false }
        return CMBlockBufferCopyDataBytes(bb, atOffset: 0, dataLength: total, destination: base) == 0
    }
    guard copied else {
        inst.lastErr = -6
        inst.pending -= 1
        return
    }
    // Convert avcC → Annex-B (4-byte length prefix → 00 00 00 01 start code).
    var annexb: [UInt8] = []
    var i = 0
    while i + 4 <= total {
        let l = Int(data[i]) << 24 | Int(data[i+1]) << 16 | Int(data[i+2]) << 8 | Int(data[i+3])
        i += 4
        if l <= 0 || i + l > total { break }
        annexb.append(contentsOf: [0, 0, 0, 1])
        annexb.append(contentsOf: data[i..<i+l])
        i += l
    }
    // Prepend SPS/PPS (each already a full NAL, start-code prefixed) to the
    // first frame, matching openh264's "SPS, PPS, IDR" stream order.
    if !inst.spsPps.isEmpty {
        annexb = inst.spsPps + annexb
        inst.spsPps = []
    }
    // Enqueue for Rust to pull. If the encode loop is behind and the queue
    // already holds unpolled frames, a hardware encoder can in principle drop
    // the *oldest* (or a new) frame; we keep the queue small by bounding it
    // here so memory stays flat if the consumer stalls.
    if annexb.count > 0 {
        if inst.outQueue.count >= 8 { inst.outQueue.removeFirst() }
        inst.outQueue.append(annexb)
    }
    inst.pending -= 1
}

extension EncInst {
    /// Pull SPS/PPS NALs out of the first output buffer's format description,
    /// each start-code prefixed (Annex-B).
    static func extractSpsPps(_ fd: CMFormatDescription, into out: inout [UInt8]) -> Bool {
        guard CMFormatDescriptionGetMediaSubType(fd) == kCMVideoCodecType_H264 else { return false }
        var count: size_t = 0
        _ = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            fd, parameterSetIndex: 0,
            parameterSetPointerOut: nil, parameterSetSizeOut: nil,
            parameterSetCountOut: &count, nalUnitHeaderLengthOut: nil)
        var all: [UInt8] = []
        for idx in 0..<count {
            var p: UnsafePointer<UInt8>?
            var size: size_t = 0
            _ = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                fd, parameterSetIndex: idx,
                parameterSetPointerOut: &p, parameterSetSizeOut: &size,
                parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil)
            guard let ptr = p, size > 0 else { continue }
            all.append(contentsOf: [0, 0, 0, 1])
            all.append(contentsOf: UnsafeBufferPointer(start: ptr, count: Int(size)))
        }
        if !all.isEmpty {
            out = all
            return true
        }
        return false
    }

    /// Convert a tightly-packed RGBA8888 buffer to a BGRA8888 CVPixelBuffer.
    static func rgbaToBgra(_ rgba: UnsafePointer<UInt8>, _ w: Int, _ h: Int) -> CVPixelBuffer? {
        var pb: CVPixelBuffer?
        let st = CVPixelBufferCreate(kCFAllocatorDefault, w, h, kChatxEncBGRA, nil, &pb)
        guard st == 0, let pb = pb else { return nil }
        CVPixelBufferLockBaseAddress(pb, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else {
            return nil
        }
        let bytesPerRow = CVPixelBufferGetBytesPerRow(pb)
        let stride = w * 4
        for y in 0..<h {
            let srcRow = rgba + y * stride
            let dstRow = base + y * bytesPerRow
            for x in 0..<w {
                let i = x * 4
                dstRow[i]     = srcRow[i + 2] // B ← R
                dstRow[i + 1] = srcRow[i + 1] // G
                dstRow[i + 2] = srcRow[i]     // R ← B
                dstRow[i + 3] = 255           // A
            }
        }
        return pb
    }
}

// ── Handle registry (multiple concurrent encoders: camera + screen) ─────────
final class EncRegistry {
    static let shared = EncRegistry()
    private let lock = NSLock()
    private var next: Int32 = 1
    private var insts: [Int32: EncInst] = [:]

    func new() -> Int32 {
        lock.lock(); defer { lock.unlock() }
        let h = next
        next += 1
        insts[h] = EncInst()
        return h
    }

    func get(_ h: Int32) -> EncInst? {
        lock.lock(); defer { lock.unlock() }
        return insts[h]
    }

    func close(_ h: Int32) {
        lock.lock()
        let inst = insts.removeValue(forKey: h)
        lock.unlock()
        // `inst.close()` drains `pending` (with a 5s bound) and clears the
        // session reference. The registry's `removeValue` already dropped its
        // strong ref; the only remaining refs are any in-flight callbacks (each
        // retains for its duration). `deinit` invalidates the session when the
        // last ref is gone.
        inst?.close()
    }
}

// ── C-ABI exports (called from Rust) ─────────────────────────────────────────

@_cdecl("chatx_h264_enc_new")
public func chatx_h264_enc_new() -> Int32 {
    EncRegistry.shared.new()
}

@_cdecl("chatx_h264_enc_feed")
public func chatx_h264_enc_feed(
    _ handle: Int32,
    _ rgba: UnsafePointer<UInt8>,
    _ len: Int32,
    _ w: Int32,
    _ h: Int32
) -> Int32 {
    guard let inst = EncRegistry.shared.get(handle) else { return -1 }
    return inst.feed(rgba, Int(len), Int(w), Int(h))
}

/// Fast path: encode an existing `CVPixelBuffer` (borrowed; the caller keeps
/// ownership). Skips the CPU RGBA→BGRA conversion entirely.
@_cdecl("chatx_h264_enc_feed_cvpb")
public func chatx_h264_enc_feed_cvpb(
    _ handle: Int32,
    _ pb: UnsafeMutableRawPointer,
    _ w: Int32,
    _ h: Int32
) -> Int32 {
    guard let inst = EncRegistry.shared.get(handle) else { return -1 }
    let pixelBuffer = Unmanaged<CVPixelBuffer>.fromOpaque(pb).takeUnretainedValue()
    return inst.feedCvpb(pixelBuffer, Int(w), Int(h))
}

@_cdecl("chatx_h264_enc_out")
public func chatx_h264_enc_out(
    _ handle: Int32,
    _ dst: UnsafeMutablePointer<UInt8>,
    _ cap: Int32
) -> Int32 {
    guard let inst = EncRegistry.shared.get(handle) else { return 0 }
    return inst.out(dst: dst, cap: Int(cap))
}

@_cdecl("chatx_h264_enc_close")
public func chatx_h264_enc_close(_ handle: Int32) {
    EncRegistry.shared.close(handle)
}
