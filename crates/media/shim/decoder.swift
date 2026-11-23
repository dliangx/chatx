import Foundation
import CoreMedia
import CoreVideo
import VideoToolbox
import CoreImage

// ──────────────────────────────────────────────────────────────────────────────
// Chatx H.264 decode shim — VideoToolbox (hardware / GPU) decode → Rust.
//
// C-ABI surface (called from the Rust `media` crate, `src/apple.rs`):
//   chatx_h264_dec_new()                         -> Int32  (opaque handle; 0 = fail)
//   chatx_h264_dec_feed(handle, p, len)          -> Int32  (feed one Annex-B chunk; 0 = accepted)
//   chatx_h264_dec_out(handle, dst, cap, &w, &h) -> Int32  (copy ≤cap bytes of the next
//                                                             RGBA8888 frame into Rust's
//                                                             `dst`; 0 = none ready)
//   chatx_h264_dec_close(handle)                 -> void
//
// A **handle** identifies one `VTDecompressionSession` + its state. A call can
// run several decoders at once (camera track + screen track) without them
// colliding — the same registry pattern the encode shim uses. (The previous
// version was a process-wide singleton, so two video tracks shared one session
// and one output slot and corrupted each other.)
//
// Decoding is **asynchronous**: `VTDecompressionSessionDecodeFrame` invokes its
// completion on a VideoToolbox queue, which may be *the calling thread*. The
// completion converts the decoded `CVPixelBuffer` to RGBA8888 (Core Image) and
// appends it to a small per-instance frame queue; Rust pulls frames out via
// `chatx_h264_dec_out`.
//
// CRITICAL — the "video shows then freezes" fix: we must NOT hold the instance
// lock across `VTDecompressionSessionDecodeFrame`. If the completion fires
// synchronously on this thread, it would try to re-take the (non-recursive)
// lock and the whole call dead-locks on the last good frame. So `feed`
// releases the lock BEFORE the decode call, and the completion does its
// (expensive) RGBA conversion OUTSIDE the lock, then briefly locks only to
// enqueue the frame. This mirrors the encode shim, which already guards the
// same hazard.
// ──────────────────────────────────────────────────────────────────────────────

private let kFMT_32BGRA: OSType = 0x42475241 // 'BGRA'
private let kCIContext = CIContext(options: nil)

/// One `VTDecompressionSession` + its decode state, keyed by handle.
final class DecInst {
    let lock = NSLock()
    var session: VTDecompressionSession?
    var format: CMVideoFormatDescription?
    var initialized = false
    /// Decoded pictures awaiting `out`. A small queue (not a single slot) so an
    /// async decode + a slower sync drain never overwrites an unread frame.
    var queue: [(w: Int, h: Int, rgba: [UInt8])] = []
    /// Monotonic presentation-timestamp counter (in 1/90000s ticks). Each
    /// decoded sample must carry a strictly increasing PTS, or
    /// `VTDecompressionSessionDecodeFrame` rejects later frames with
    /// `kVTVideoDecoderBadDataErr` (-12909).
    var nextPts: Int64 = 0

    func close() {
        lock.lock(); defer { lock.unlock() }
        if let s = session {
            VTDecompressionSessionInvalidate(s)
        }
        session = nil
        format = nil
        initialized = false
        queue.removeAll()
    }

    // Accumulated SPS/PPS NALs across feed calls (see file header).
    var spsNals: [[UInt8]] = []
    var ppsNals: [[UInt8]] = []

    // Assumes `lock` is already held (NSLock is not recursive).
    @discardableResult
    private func ensureSessionLocked() -> Bool {
        guard !initialized, let f = format else { return initialized }
        guard let s = Self.makeSession(format: f) else { return false }
        session = s
        initialized = true
        return true
    }

    /// Try to build the format description from accumulated SPS + PPS NALs.
    /// Returns true if the format was successfully built and stored in
    /// `self.format`.
    private func tryBuildFormatLocked() -> Bool {
        guard !initialized, spsNals.isEmpty == false, ppsNals.isEmpty == false else { return false }
        // Concatenate SPS then PPS into a flat buffer for a valid C pointer.
        var flat = [UInt8]()
        var entries: [(offset: Int, size: Int)] = []
        for nal in spsNals {
            entries.append((flat.count, nal.count))
            flat.append(contentsOf: nal)
        }
        for nal in ppsNals {
            entries.append((flat.count, nal.count))
            flat.append(contentsOf: nal)
        }
        let entryCount = entries.count
        let dbgSps = spsNals.map { $0.map { String(format: "%02x", $0) }.joined(separator: " ") }
        let dbgPps = ppsNals.map { $0.map { String(format: "%02x", $0) }.joined(separator: " ") }
        FileHandle.standardError.write(Data("[vdec] buildFormat sps=\(dbgSps) pps=\(dbgPps) n=\(entryCount)\n".utf8))
        var f: CMVideoFormatDescription?
        var st: OSStatus = -1
        let _ = flat.withUnsafeBufferPointer { fp in
            guard let base = fp.baseAddress else { return }
            let ptrs: [UnsafePointer<UInt8>] = entries.map { base + $0.offset }
            let sizes: [Int] = entries.map { $0.size }
            _ = ptrs.withUnsafeBufferPointer { pp in
                _ = sizes.withUnsafeBufferPointer { ss in
                    guard let ppb = pp.baseAddress, let ssb = ss.baseAddress else { return }
                    st = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        allocator: kCFAllocatorDefault,
                        parameterSetCount: entryCount,
                        parameterSetPointers: ppb,
                        parameterSetSizes: ssb,
                        // H.264 NAL units carry a 1-byte header (NRI | type).
                        // The previous value of 4 made
                        // `CMVideoFormatDescriptionCreateFromH264ParameterSets`
                        // read past the header and reject the parameter sets
                        // (`kCMError_InvalidFormatDescription` -12712/-12714).
                        nalUnitHeaderLength: 1,
                        formatDescriptionOut: &f
                    )
                }
            }
        }
        guard st == 0, let f = f else {
            FileHandle.standardError.write(Data("[vdec] CreateFromH264ParameterSets=\(st)\n".utf8))
            return false
        }
        self.format = f
        return true
    }

    func feed(_ annexb: UnsafePointer<UInt8>, _ len: Int) -> Int32 {
        var session: VTDecompressionSession?
        var format: CMVideoFormatDescription?
        var sb: CMSampleBuffer?
        lock.lock()
        guard len > 0 else { lock.unlock(); return -1 }
        if !initialized {
            // Accumulate SPS / PPS NALs from this chunk. The webrtc H.264
            // payloader typically emits SPS+PPS in a single STAP-A packet, but
            // they can also arrive in separate packets; we must not require
            // both in the *first* chunk.
            for nal in Self.nals(annexb, len) {
                guard let first = nal.first else { continue }
                let t = first & 0x1F
                if t == 7 { spsNals.append(nal) }
                else if t == 8 { ppsNals.append(nal) }
            }
            // Try to build the format once we have at least one SPS and one PPS.
            if spsNals.isEmpty || ppsNals.isEmpty {
                // Not enough parameter sets yet — accept the chunk (no-op) so
                // we don't hard-fail on a parameter-set-only packet.
                lock.unlock()
                return 0
            }
            guard tryBuildFormatLocked() else {
                // Hard failure: format description could not be built.
                lock.unlock()
                return -1
            }
            guard ensureSessionLocked() else {
                lock.unlock()
                return -1
            }
        }

        // Only decode chunks that carry a real VCL slice (IDR type 5, or
        // non-IDR type 1). Parameter-set-only (7/8) and SEI-only (6) chunks
        // are NOT decoded: the SPS/PPS already live in the format description
        // (built above) and the SEI is optional metadata. Feeding them to
        // `VTDecompressionSessionDecodeFrame` made VideoToolbox reject them
        // with `kVTVideoDecoderBadDataErr` (-12909).
        let hasSlice = Self.nals(annexb, len).contains { nal in
            guard let b = nal.first else { return false }
            let t = b & 0x1F
            return t == 1 || t == 5
        }
        guard hasSlice else {
            lock.unlock()
            return 0
        }

        session = self.session
        format = self.format
        guard let s = session, let f = format else { lock.unlock(); return -1 }
        // Allocate a strictly-increasing PTS for this picture (30 fps tick),
        // then advance the counter — both under the lock so the value we hand
        // to the sample buffer is the one we intend.
        let pts = nextPts
        nextPts += 90000 / 30
        sb = Self.makeSampleBuffer(annexb, len, f, pts: pts)
        guard sb != nil else { lock.unlock(); return -1 }
        let decodeSession = s
        let sampleBuffer = sb!
        // RELEASE the lock before the (possibly-synchronous) decode call — see
        // the file header. The completion may run on this thread and re-locks.
        lock.unlock()

        let inst = self
        var infoFlags: VTDecodeInfoFlags = []
        let r = VTDecompressionSessionDecodeFrame(
            decodeSession,
            sampleBuffer: sampleBuffer,
            flags: [],
            infoFlagsOut: &infoFlags
        ) { status, _, imageBuffer, _, _ in
            FileHandle.standardError.write(Data("[vdec] completion status=\(status) pb=\(imageBuffer != nil)\n".utf8))
            inst.present(status: status, imageBuffer: imageBuffer)
        }
        if r != 0 {
            FileHandle.standardError.write(Data("[vdec] DecodeFrame=\(r)\n".utf8))
        }
        return 0
    }

    // ── output (Rust-owned destination) ───────────────────────────────────
    /// Returns the next decoded picture's byte count when one is ready, else 0.
    /// If `dst` is too small (`cap < frame size`), the frame is NOT consumed —
    /// the caller grows `dst` and calls again (the encoder caps output at ≤ 4K,
    /// so this converges in one growth). A successful copy sets `outW`/`outH`
    /// and consumes the frame.
    func out(dst: UnsafeMutablePointer<UInt8>, cap: Int,
             _ outWp: UnsafeMutablePointer<UInt32>,
             _ outHp: UnsafeMutablePointer<UInt32>) -> Int32 {
        lock.lock(); defer { lock.unlock() }
        guard let f = queue.first else { return 0 }
        outWp.pointee = UInt32(f.w)
        outHp.pointee = UInt32(f.h)
        if cap < f.rgba.count {
            return Int32(f.rgba.count) // need a bigger buffer; frame kept
        }
        dst.update(from: f.rgba, count: f.rgba.count)
        queue.removeFirst()
        return Int32(f.rgba.count)
    }

    // Completion handler — runs on the VideoToolbox queue OR the calling thread
    // (synchronous). Convert the (expensive) RGBA OUTSIDE the lock, then briefly
    // lock only to enqueue. `feed` has already released the lock by the time
    // this can run, so there is no self-deadlock.
    func present(status: OSStatus, imageBuffer: CVPixelBuffer?) {
        guard status == 0, let pb = imageBuffer else { return }
        let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
        guard w > 0, h > 0, let rgba = Self.convertToRGBA(pb, w, h) else { return }
        lock.lock()
        queue.append((w, h, rgba))
        if queue.count > 4 {
            queue.removeFirst(queue.count - 4)
        }
        lock.unlock()
    }

    // ── Annex-B → NAL list ────────────────────────────────────────────────
    private static func nals(_ annexb: UnsafePointer<UInt8>, _ len: Int) -> [[UInt8]] {
        let all = annexb
        var out: [[UInt8]] = []
        var i = 0
        while i + 4 <= len {
            if all[i] == 0, all[i + 1] == 0, all[i + 2] == 0, all[i + 3] == 1 {
                let start = i + 4
                var j = start
                // Advance to the next start code OR the end of the buffer.
                // The previous `while j + 4 <= len` guard stopped `j` from
                // advancing once it was within 3 bytes of the buffer end, so a
                // FINAL NAL near the end (the PPS in an "SPS+PPS" chunk) was
                // truncated to its first byte — feeding an invalid parameter
                // set to `CMVideoFormatDescriptionCreateFromH264ParameterSets`
                // (-12714). We must let `j` run to `len`.
                while j < len {
                    if j + 4 <= len, all[j] == 0, all[j + 1] == 0, all[j + 2] == 0, all[j + 3] == 1 {
                        break
                    }
                    j += 1
                }
                if j > start {
                    var nal = [UInt8](repeating: 0, count: j - start)
                    nal.withUnsafeMutableBufferPointer { dst in
                        dst.baseAddress!.update(from: all + start, count: j - start)
                    }
                    out.append(nal)
                }
                i = j
            } else {
                i += 1
            }
        }
        return out
    }

    /// Build the AVCC sample buffer for one Annex-B chunk (each NAL gets a
    /// 4-byte length prefix). Returns nil if the chunk carries no NALs. `pts`
    /// is the picture's presentation timestamp (monotonic, in 90 kHz ticks).
    private static func makeSampleBuffer(
        _ annexb: UnsafePointer<UInt8>, _ len: Int,
        _ format: CMVideoFormatDescription,
        pts: Int64
    ) -> CMSampleBuffer? {
        let nals = Self.nals(annexb, len)
        guard !nals.isEmpty else { return nil }
        // Flatten into AVCC (4-byte length prefix + NAL bytes).
        var avcc = [UInt8]()
        for nal in nals {
            let l = nal.count
            avcc.append(UInt8((l >> 24) & 0xff))
            avcc.append(UInt8((l >> 16) & 0xff))
            avcc.append(UInt8((l >> 8) & 0xff))
            avcc.append(UInt8(l & 0xff))
            avcc.append(contentsOf: nal)
        }

        // `avcc` is a *local* buffer that is freed the moment this function
        // returns — but VideoToolbox reads the bitstream **asynchronously,
        // later**, on its own decode queue. The old code passed the local array
        // directly with `blockAllocator: kCFAllocatorNull`, which tells
        // CoreFoundation it does NOT own the memory block — a dangling pointer
        // by decode time. The async decoder then read freed memory and rejected
        // the frame with `kVTVideoDecoderBadDataErr` (-12909): exactly the
        // "remote screen share never renders" symptom. The synchronous pure-Rust
        // openh264 path never hit it because it reads the bytes inline while
        // `avcc` is still alive.
        //
        // Fix: copy the AVCC bytes into a heap allocation and hand ownership to
        // the block buffer (`blockAllocator: kCFAllocatorDefault` = CF frees it
        // on dealloc), so the bitstream is guaranteed to outlive this function
        // and be valid at the (later) async decode.
        let base = UnsafeMutableRawPointer.allocate(byteCount: avcc.count, alignment: 16)
        avcc.withUnsafeBytes { src in
            if let s = src.baseAddress {
                base.copyMemory(from: s, byteCount: avcc.count)
            }
        }
        var blockBuffer: CMBlockBuffer?
        let bbSt = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: base,
            blockLength: avcc.count,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: avcc.count,
            flags: 0,
            blockBufferOut: &blockBuffer
        )
        guard bbSt == 0, let blockBuffer = blockBuffer else {
            FileHandle.standardError.write(Data("[vdec] BlockBuffer=\(bbSt)\n".utf8))
            return nil
        }

        var sb: CMSampleBuffer?
        var timing = Self.timing(pts)
        let sizes = [CFIndex(avcc.count)]
        let sbSt = sizes.withUnsafeBufferPointer { sp in
            withUnsafePointer(to: &timing) { tp in
                CMSampleBufferCreate(
                    allocator: kCFAllocatorDefault,
                    dataBuffer: blockBuffer,
                    dataReady: true,
                    makeDataReadyCallback: nil,
                    refcon: nil,
                    formatDescription: format,
                    sampleCount: 1,
                    sampleTimingEntryCount: 1,
                    sampleTimingArray: tp,
                    sampleSizeEntryCount: 1,
                    sampleSizeArray: sp.baseAddress,
                    sampleBufferOut: &sb
                )
            }
        }
        guard sbSt == 0, let sb = sb else {
            FileHandle.standardError.write(Data("[vdec] SampleBuffer=\(sbSt)\n".utf8))
            return nil
        }
        return sb
    }

    private static func timing(_ pts: Int64) -> CMSampleTimingInfo {
        // 90 kHz timescale; duration 1/30 s. PTS must be monotonic (see
        // `DecInst.nextPts`) — reusing PTS=0 for every picture made
        // VideoToolbox reject frames with `kVTVideoDecoderBadDataErr`.
        var t = CMSampleTimingInfo()
        t.duration = CMTime(value: 1, timescale: 30)
        t.presentationTimeStamp = CMTime(value: pts, timescale: 90_000)
        t.decodeTimeStamp = CMTime(value: pts, timescale: 90_000)
        return t
    }

    private static func makeSession(format: CMVideoFormatDescription) -> VTDecompressionSession? {
        var s: VTDecompressionSession?
        let st = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: format,
            decoderSpecification: nil,
            imageBufferAttributes: nil,
            decompressionSessionOut: &s
        )
        guard st == 0, let s = s else {
            FileHandle.standardError.write(Data("[vdec] SessionCreate=\(st)\n".utf8))
            return nil
        }
        return s
    }

    private static func convertToRGBA(_ pb: CVPixelBuffer, _ w: Int, _ h: Int) -> [UInt8]? {
        // Render the decoded CVPixelBuffer into a tightly-packed BGRA buffer
        // (the same Core Image path the camera shim uses — it understands every
        // YCbCr layout the decoder may emit), then convert BGRA → RGBA.
        var outPb: CVPixelBuffer?
        let createSt = CVPixelBufferCreate(
            kCFAllocatorDefault, w, h, kFMT_32BGRA, nil, &outPb
        )
        guard createSt == 0, let outPb = outPb else { return nil }
        kCIContext.render(CIImage(cvPixelBuffer: pb), to: outPb)

        CVPixelBufferLockBaseAddress(outPb, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(outPb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(outPb)?.assumingMemoryBound(to: UInt8.self) else {
            return nil
        }
        let bytesPerRow = CVPixelBufferGetBytesPerRow(outPb)
        let stride = w * 4
        var out = [UInt8](repeating: 0, count: w * h * 4)
        out.withUnsafeMutableBufferPointer { dst in
            guard let d = dst.baseAddress else { return }
            for y in 0..<h {
                let srcRow = base + y * bytesPerRow
                let dstRow = d + y * stride
                for x in 0..<w {
                    let i = x * 4
                    dstRow[i]     = srcRow[i + 2] // B → R
                    dstRow[i + 1] = srcRow[i + 1] // G
                    dstRow[i + 2] = srcRow[i]     // R → B
                    dstRow[i + 3] = 255           // A
                }
            }
        }
        return out
    }
}

// ── Handle registry (multiple concurrent decoders: cam + screen tracks) ──────
final class DecRegistry {
    static let shared = DecRegistry()
    private let lock = NSLock()
    private var next: Int32 = 1
    private var insts: [Int32: DecInst] = [:]

    func new() -> Int32 {
        lock.lock(); defer { lock.unlock() }
        let h = next
        next += 1
        insts[h] = DecInst()
        return h
    }

    func get(_ h: Int32) -> DecInst? {
        lock.lock(); defer { lock.unlock() }
        return insts[h]
    }

    func close(_ h: Int32) {
        lock.lock()
        let inst = insts.removeValue(forKey: h)
        lock.unlock()
        inst?.close()
    }
}

// ── C-ABI exports (called from Rust) ─────────────────────────────────────────

@_cdecl("chatx_h264_dec_new")
public func chatx_h264_dec_new() -> Int32 {
    DecRegistry.shared.new()
}

@_cdecl("chatx_h264_dec_feed")
public func chatx_h264_dec_feed(_ handle: Int32, _ ptr: UnsafePointer<UInt8>, _ len: Int) -> Int32 {
    guard let inst = DecRegistry.shared.get(handle) else { return -1 }
    return inst.feed(ptr, len)
}

@_cdecl("chatx_h264_dec_out")
public func chatx_h264_dec_out(
    _ handle: Int32,
    _ dst: UnsafeMutablePointer<UInt8>,
    _ cap: Int,
    _ outW: UnsafeMutablePointer<UInt32>,
    _ outH: UnsafeMutablePointer<UInt32>
) -> Int32 {
    guard let inst = DecRegistry.shared.get(handle) else { return 0 }
    return inst.out(dst: dst, cap: cap, outW, outH)
}

@_cdecl("chatx_h264_dec_close")
public func chatx_h264_dec_close(_ handle: Int32) {
    DecRegistry.shared.close(handle)
}
