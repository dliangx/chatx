import Foundation
import AVFoundation
import CoreVideo
import CoreMedia
import CoreImage

// ──────────────────────────────────────────────────────────────────────────────
// Chatx camera shim — AVFoundation bridge (Swift → Rust via `@_silgen_name`).
//
// Public entry points (C-ABI, called from the Rust `camera` crate):
//   chatx_camera_start(buf, len)   -> Int32  (0 = ok, 1 = error; message in buf)
//   chatx_camera_stop()
//   chatx_camera_is_running()      -> Int32
//   chatx_camera_capture()         -> Int32  (0 = triggered, 1 = not running)
//   chatx_camera_auth_status()     -> Int32  (raw AVCaptureDevice AuthorizationStatus)
//
// Callbacks into Rust (defined in the Rust `camera` crate, imported here via
// `@_silgen_name`):
//   _chatx_rust_qr(ptr: UnsafeRawPointer?, len: Int)
//   _chatx_rust_frame(ptr: UnsafeRawPointer?, len: Int, w: UInt32, h: UInt32, fmt: UInt32)
//   _chatx_rust_capture_done(ok: Int32, path, pathLen, err, errLen)
//
// Pixel format IDs (keep in sync with `crates/bridge::types::PixelFormat`):
//   1 = RGBA8888 (4 B/px)
//   8 = Gray8    (1 B/px, Y-only)
// ──────────────────────────────────────────────────────────────────────────────

// ── Rust callback trampolines ───────────────────────────────────────────────
@_silgen_name("_chatx_rust_qr")
fileprivate func chatx_rust_qr(_ ptr: UnsafeRawPointer?, _ len: Int)
@_silgen_name("_chatx_rust_frame")
fileprivate func chatx_rust_frame(_ ptr: UnsafeRawPointer?, _ len: Int, _ w: UInt32, _ h: UInt32, _ fmt: UInt32)
@_silgen_name("_chatx_rust_capture_done")
fileprivate func chatx_rust_capture_done(_ ok: Int32,
                                         _ path: UnsafeRawPointer?, _ pathLen: Int,
                                         _ err: UnsafeRawPointer?, _ errLen: Int)

// ── Constants ───────────────────────────────────────────────────────────────
private let FMT_RGBA8888: UInt32 = 1
private let FMT_GRAY8:    UInt32 = 8
private let kFMT_32BGRA:        OSType = 0x42475241 // 'BGRA'
private let kFMT_420YBCBR8_3PLANE:  OSType = 0x32767579 // '2vuy'
private let kFMT_420YBCBR8BIPLNAR_VIDEO: OSType = 0x34323076 // '420v'
private let kFMT_420YBCBR8BIPLNAR_FULL:  OSType = 0x34323066 // '420f'
private let kErrMsgCapacity = 512

// ── QR delegate — forwards decoded machine-readable codes to Rust ──────────
private final class QrDelegate: NSObject, AVCaptureMetadataOutputObjectsDelegate {
    func metadataOutput(_ output: AVCaptureMetadataOutput,
                        didOutput metadataObjects: [Any],
                        from connection: AVCaptureConnection) {
        for o in metadataObjects {
            guard let code = o as? AVMetadataMachineReadableCodeObject,
                  let raw = code.stringValue else { continue }
            let payload = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            if payload.isEmpty { continue }
            let utf8 = Array(payload.utf8)
            utf8.withUnsafeBufferPointer { buf in
                if let base = buf.baseAddress {
                    chatx_rust_qr(UnsafeRawPointer(base), buf.count)
                }
            }
        }
    }
}

// ── Video-data delegate — converts CVPixelBuffer → RGBA8 → Rust ─────────────
// We hand the CVPixelBuffer to Core Image, which understands the exact
// YCbCr layout (2vuy / 420v / 420f / BGRA / …) and hands us back a clean
// RGBA8 buffer of the correct size. This is the only robust way to get a
// non-distorted, correct-aspect preview regardless of what the sensor
// emits — manually parsing chroma planes here has been a source of wrong
// strides/distortion across formats.
private final class VideoDelegate: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    private static let ciContext = CIContext(options: nil)

    func captureOutput(_ output: AVCaptureOutput,
                       didOutput sampleBuffer: CMSampleBuffer,
                       from connection: AVCaptureConnection) {
        guard let pb = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let extent = CVPixelBufferGetWidth(pb), eH = CVPixelBufferGetHeight(pb)
        guard extent > 0, eH > 0 else { return }

        // Core Image decodes the exact YCbCr layout (2vuy / 420v / 420f / …)
        // and renders it to a clean BGRA buffer. We scale the preview down to
        // a bounded width so the per-frame copy stays cheap — the UI rescales
        // with `image-fit: cover` anyway, and the QR path is independent.
        let ciImage = CIImage(cvPixelBuffer: pb)
        var tw = extent
        var th = eH
        let maxW = 640
        if extent > maxW {
            let s = CGFloat(maxW) / CGFloat(extent)
            tw = maxW
            th = max(1, Int((CGFloat(eH) * s).rounded()))
        }
        let scale = CGAffineTransform(scaleX: CGFloat(tw) / CGFloat(extent), y: CGFloat(th) / CGFloat(eH))
        let scaled = ciImage.transformed(by: scale)

        guard let outPb = VideoDelegate.render(scaled, w: tw, h: th) else {
            return
        }
        // The rendered buffer is deferred; lock it so the base address is
        // valid on the CPU before we read pixels.
        _ = CVPixelBufferLockBaseAddress(outPb, .readOnly)
        defer { _ = CVPixelBufferUnlockBaseAddress(outPb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(outPb)?.assumingMemoryBound(to: UInt8.self) else {
            return
        }
        let bytesPerRow = CVPixelBufferGetBytesPerRow(outPb)
        let stride = tw * 4
        // Rendered as BGRA → emit RGBA (swap the outer two bytes) packed at a
        // tight w*4 stride, dropping any row padding.
        var tight = [UInt8](repeating: 0, count: tw * th * 4)
        tight.withUnsafeMutableBufferPointer { dstPtr in
            guard let d = dstPtr.baseAddress else { return }
            let dstRow0 = d
            let srcRow0 = base
            for y in 0..<th {
                let dstRow = dstRow0 + y * stride
                let srcRow = srcRow0 + y * bytesPerRow
                for x in 0..<tw {
                    let i = x * 4
                    dstRow[i]     = srcRow[i + 2] // B -> R
                    dstRow[i + 1] = srcRow[i + 1] // G
                    dstRow[i + 2] = srcRow[i]     // R -> B
                    dstRow[i + 3] = 255           // A
                }
            }
        }
        VideoDelegate.deliver(tight, w: tw, h: th, fmt: FMT_RGBA8888)
    }

    /// Render a CIImage into a fresh BGRA CVPixelBuffer of the given size.
    private static func render(_ ciImage: CIImage, w: Int, h: Int) -> CVPixelBuffer? {
        var outPb: CVPixelBuffer?
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault, w, h, kCVPixelFormatType_32BGRA, nil, &outPb
        )
        guard status == kCVReturnSuccess, let pb = outPb else { return nil }
        _ = ciContext.render(ciImage, to: pb)
        return pb
    }

    private static func deliver(_ bytes: [UInt8], w: Int, h: Int, fmt: UInt32) {
        bytes.withUnsafeBufferPointer { buf in
            if let base = buf.baseAddress {
                chatx_rust_frame(UnsafeRawPointer(base),
                                 buf.count,
                                 UInt32(w), UInt32(h), fmt)
            }
        }
    }
}

// ── Photo delegate — writes the JPEG to a temp file and reports to Rust ────
private final class PhotoDelegate: NSObject, AVCapturePhotoCaptureDelegate {
    // Set just before each `capturePhoto` by `CameraShim.cap()` (main-thread
    // hop). Read by `photoOutput(_:didFinishProcessingPhoto:error:)` when the
    // async callback lands — we only start one capture at a time so no
    // additional synchronisation is required.
    fileprivate static let pendingPathLock = NSLock()
    fileprivate static var pendingPathValue = ""

    static func setPending(_ path: String) {
        pendingPathLock.lock()
        pendingPathValue = path
        pendingPathLock.unlock()
    }

    static func takePending() -> String {
        pendingPathLock.lock()
        defer { pendingPathLock.unlock() }
        let v = pendingPathValue
        pendingPathValue = ""
        return v
    }

    func photoOutput(_ output: AVCapturePhotoOutput,
                     didFinishProcessingPhoto photo: AVCapturePhoto,
                     error: (any Error)?) {
        if let err = error {
            report(ok: 0, path: nil, message: err.localizedDescription)
            return
        }
        guard let data = photo.fileDataRepresentation() else {
            report(ok: 0, path: nil, message: "照片数据为空")
            return
        }
        let path = PhotoDelegate.takePending()
        if path.isEmpty {
            report(ok: 0, path: nil, message: "未指定目标路径")
            return
        }
        do {
            try data.write(to: URL(fileURLWithPath: path), options: .atomic)
            report(ok: 1, path: path, message: nil)
        } catch {
            report(ok: 0, path: nil, message: error.localizedDescription)
        }
    }

    private func report(ok: Int32, path: String?, message: String?) {
        let pathUtf8 = path.map { Array($0.utf8) } ?? [UInt8]()
        let msgUtf8 = message.map { Array($0.utf8) } ?? [UInt8]()
        pathUtf8.withUnsafeBufferPointer { pbuf in
            msgUtf8.withUnsafeBufferPointer { mbuf in
                chatx_rust_capture_done(
                    ok,
                    pbuf.baseAddress.map { UnsafeRawPointer($0) },
                    pbuf.count,
                    mbuf.baseAddress.map { UnsafeRawPointer($0) },
                    mbuf.count
                )
            }
        }
    }
}

// Serial delegate queues (Apple requires serial queues for AVFoundation
// output delegates — the `global` pool queues are concurrent and would
// race, and the docs say "the delegate queue must be serial").
private enum Queues {
    static let video = DispatchQueue(label: "com.chatx.camera.video")
    static let metadata = DispatchQueue(label: "com.chatx.camera.metadata")
}

// Internal error type for the configuration closure.
private struct ShErr: Swift.Error {
    let message: String
    init(_ m: String) { self.message = m }
}

// All mutation happens on `queue` (a private serial dispatch queue); this is
// the pattern Apple recommends for AVCaptureSession (configure + start +
// stop from a single serial context). Reads for `is_running()` are
// `atomic` bool.
private final class CameraShim {
    static let shared = CameraShim()
    private let queue = DispatchQueue(label: "com.chatx.camera.shim")
    private let running = NSLock()
    private var _running = false
    private var session: AVCaptureSession?
    private var qrDelegate: QrDelegate?
    private var videoDelegate: VideoDelegate?
    private var videoOutput: AVCaptureVideoDataOutput?
    private var photoOutput: AVCapturePhotoOutput?
    private var photoDelegate: PhotoDelegate?

    var isRunning: Bool {
        running.lock(); defer { running.unlock() }
        return _running
    }
    private func setRunning(_ v: Bool) {
        running.lock(); defer { running.unlock() }
        _running = v
    }

    /// Block up to `timeout` on the user's TCC decision, funnelled through a
    /// semaphore. Mirrors the old objc2 implementation's explicit request.
    private func requestCameraAccessWithPrompt(timeout: TimeInterval) -> Bool {
        let sem = DispatchSemaphore(value: 0)
        var granted = false
        AVCaptureDevice.requestAccess(for: .video) { ok in
            granted = ok
            sem.signal()
        }
        if sem.wait(timeout: .now() + timeout) == .timedOut {
            return false
        }
        return granted
    }

    /// Build + start a capture session for QR + live preview + still photo.
    /// Returns an error message on failure (empty string on success).
    func start() -> String {
        var err = ""
        queue.sync {
            if self._running { return }

            let videoType = AVMediaType.video
            let status = AVCaptureDevice.authorizationStatus(for: videoType)
            FileHandle.standardError.write(Data("[camera] auth status = \(status.rawValue) (0=NotDetermined 1=Restricted 2=Denied 3=Authorized)\n".utf8))
            if status == .notDetermined {
                FileHandle.standardError.write(Data("[camera] notDetermined → prompting TCC (wait 20s)\n".utf8))
                if !self.requestCameraAccessWithPrompt(timeout: 20) {
                    err = "未授权摄像头访问，请前往 系统设置 › 隐私与安全性 › 摄像头 为本应用授权后重试"
                    return
                }
                FileHandle.standardError.write(Data("[camera] TCC granted\n".utf8))
            } else if status == .denied || status == .restricted {
                err = "未授权摄像头访问，请前往 系统设置 › 隐私与安全性 › 摄像头 为本应用授权后重试"
                return
            }

            let session = AVCaptureSession()
            session.sessionPreset = .high

            // Apple's samples (e.g. `AVCaptureSession+PhotoCapture`) attach
            // delegates *before* the `beginConfiguration()` / `commitConfiguration()`
            // window. We do the same here:
            let mdOut = AVCaptureMetadataOutput()
            mdOut.metadataObjectTypes = mdOut.availableMetadataObjectTypes
            let qrDel = QrDelegate()
            mdOut.setMetadataObjectsDelegate(qrDel, queue: Queues.metadata)
            self.qrDelegate = qrDel

            let vOut = AVCaptureVideoDataOutput()
            vOut.alwaysDiscardsLateVideoFrames = true
            let vDel = VideoDelegate()
            vOut.setSampleBufferDelegate(vDel, queue: Queues.video)
            self.videoOutput = vOut
            self.videoDelegate = vDel

            let pOut = AVCapturePhotoOutput()
            let pDel = PhotoDelegate()
            self.photoOutput = pOut
            self.photoDelegate = pDel

            FileHandle.standardError.write(Data("[camera] configuring session in begin/commit window\n".utf8))
            session.beginConfiguration()
            do {
                guard let device = AVCaptureDevice.default(for: .video) else {
                    throw ShErr("未找到可用的摄像头设备")
                }
                FileHandle.standardError.write(Data("[camera] using device: \(device.localizedName)\n".utf8))
                let input = try AVCaptureDeviceInput(device: device)
                guard session.canAddInput(input) else {
                    throw ShErr("摄像头无法加入会话（请确认已授权访问权限）")
                }
                session.addInput(input)
                guard session.canAddOutput(mdOut) else {
                    throw ShErr("添加二维码识别输出失败")
                }
                session.addOutput(mdOut)
                if session.canAddOutput(vOut) {
                    session.addOutput(vOut)
                    FileHandle.standardError.write(Data("[camera] video data output attached (preview)\n".utf8))
                } else {
                    FileHandle.standardError.write(Data("[camera] cannot add video output; preview disabled\n".utf8))
                }
                if session.canAddOutput(pOut) {
                    session.addOutput(pOut)
                }
                session.commitConfiguration()
            } catch let e as ShErr {
                session.commitConfiguration()
                err = e.message
                return
            } catch {
                session.commitConfiguration()
                err = "配置会话失败: \(error.localizedDescription)"
                return
            }

            self.session = session

            FileHandle.standardError.write(Data("[camera] startRunning()…\n".utf8))
            // `startRunning` must be *outside* the begin/commit window (Apple
            // requirement). It is synchronous and returns only when the
            // session is live or has settled; safe to call here.
            session.startRunning()
            FileHandle.standardError.write(Data("[camera] startRunning() done; session live\n".utf8))

            self.setRunning(true)
        }
        return err
    }

    func stop() {
        queue.sync {
            self.setRunning(false)
            if let s = self.session {
                s.stopRunning()
            }
            self.session = nil
            self.qrDelegate = nil
            self.videoOutput = nil
            self.videoDelegate = nil
            self.photoOutput = nil
            self.photoDelegate = nil
        }
    }

    /// Trigger a photo. The result is reported asynchronously via
    /// `chatx_rust_capture_done`. Returns false if the session is not running.
    func capture() -> Bool {
        var ok = false
        queue.sync {
            guard self._running,
                  let out = self.photoOutput,
                  let del = self.photoDelegate else { return }
            let tsMs = Int(Date().timeIntervalSince1970 * 1000)
            let path = NSTemporaryDirectory() + "chatx_photo_\(tsMs).jpg"
            PhotoDelegate.setPending(path)
            let settings = AVCapturePhotoSettings()
            out.capturePhoto(with: settings, delegate: del)
            ok = true
        }
        return ok
    }
}

// ── C-ABI exports (called from Rust) ────────────────────────────────────────

/// Start the camera. `errBuf` is a caller-allocated buffer of `errLen` bytes
/// that receives a UTF-8 error message on failure (empty on success).
/// Returns 0 on success, 1 on failure.
@_cdecl("chatx_camera_start")
public func chatx_camera_start(_ errBuf: UnsafeMutablePointer<UInt8>?, _ errLen: Int) -> Int32 {
    let msg = CameraShim.shared.start()
    if let buf = errBuf, errLen > 0 {
        let bytes = Array(msg.utf8).prefix(errLen - 1)
        for (i, b) in bytes.enumerated() { buf[i] = b }
        buf[bytes.count] = 0
    }
    return msg.isEmpty ? 0 : 1
}

/// Stop the camera.
@_cdecl("chatx_camera_stop")
public func chatx_camera_stop() { CameraShim.shared.stop() }

/// `1` if the capture session is currently running, else `0`.
@_cdecl("chatx_camera_is_running")
public func chatx_camera_is_running() -> Int32 {
    CameraShim.shared.isRunning ? 1 : 0
}

/// Trigger a still-photo capture. Returns 0 on success (delivery is async),
/// 1 if the session is not running.
@_cdecl("chatx_camera_capture")
public func chatx_camera_capture() -> Int32 {
    CameraShim.shared.capture() ? 0 : 1
}

/// Current `AVAuthorizationStatus` for the camera (raw: 0=NotDetermined,
/// 1=Restricted, 2=Denied, 3=Authorized).
@_cdecl("chatx_camera_auth_status")
public func chatx_camera_auth_status() -> Int32 {
    Int32(AVCaptureDevice.authorizationStatus(for: .video).rawValue)
}
