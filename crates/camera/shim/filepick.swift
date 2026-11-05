import Foundation
#if canImport(UIKit)
import UIKit
import UniformTypeIdentifiers
#elseif canImport(AppKit)
import AppKit
#endif

/// Trampoline into Rust (`crates/camera/src/filepick.rs`, `ios` module).
/// - `(nil, 0)` means "no file / cancelled".
/// - `(ptr, len)` carries the UTF-8 bytes of the picked file's absolute path.
@_silgen_name("_chatx_rust_filepick_done")
fileprivate func chatx_rust_filepick_done(_ ptr: UnsafeRawPointer?, _ len: Int)

private func deliver(_ path: String?) {
    if let p = path, !p.isEmpty {
        let bytes = Array(p.utf8)
        bytes.withUnsafeBufferPointer { buf in
            if let base = buf.baseAddress {
                chatx_rust_filepick_done(UnsafeRawPointer(base), buf.count)
            } else {
                chatx_rust_filepick_done(nil, 0)
            }
        }
    } else {
        chatx_rust_filepick_done(nil, 0)
    }
}

// ── iOS: UIDocumentPickerViewController ───────────────────────────────────

#if canImport(UIKit)

/// Copy the security-scoped pick into the app's tmp dir so the Rust side ends
/// up with a stable absolute path that does not need the security scope to be
/// held by the native shell afterwards (the consumer may run on a background
/// thread long after this delegate releases the URL).
private func localise(_ url: URL) -> String {
    let dir = (NSTemporaryDirectory() as NSString).appendingPathComponent("chatx_picked")
    let _ = try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
    let name = (url.lastPathComponent.isEmpty ? "picked" : url.lastPathComponent)
        + "_" + String(Int(Date().timeIntervalSince1970 * 1000))
    let target = (dir as NSString).appendingPathComponent(name)
    let dst = URL(fileURLWithPath: target)
    // `startAccessingSecurityScopedResource` is a no-op (returns false) when
    // `url` is not security-scoped, so it is safe to call unconditionally.
    let owned = url.startAccessingSecurityScopedResource()
    defer { if owned { url.stopAccessingSecurityScopedResource() } }
    if (try? FileManager.default.copyItem(at: url, to: dst)) != nil {
        return target
    }
    // Fallback: hand back the original path — the consumer may still read it
    // while the security scope above is held (only for the duration of this
    // call, but typically enough for the chat sink to copy the bytes).
    return url.path
}

private final class FilePickDelegate: NSObject, UIDocumentPickerDelegate {
    static let shared = FilePickDelegate()

    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        guard let first = urls.first else { deliver(nil); return }
        deliver(localise(first))
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        deliver(nil)
    }
}

private final class FilePickShim {
    static let shared = FilePickShim()
    private let lock = NSLock()
    private weak var active: UIDocumentPickerViewController?

    func start() -> Bool {
        guard let sc = UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene })
            .first(where: { $0.activationState == .foregroundActive })
            ?? UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene }).first,
            let win = sc.windows.first(where: { $0.isKeyWindow }) ?? sc.windows.first
        else {
            return false
        }
        let vc = UIDocumentPickerViewController(forOpeningContentTypes: [.item])
        vc.allowsMultipleSelection = false
        vc.delegate = FilePickDelegate.shared
        vc.modalPresentationStyle = .formSheet
        lock.lock(); active = vc; lock.unlock()
        DispatchQueue.main.async {
            win.rootViewController?.present(vc, animated: true)
        }
        return true
    }

    func cancel() {
        lock.lock()
        let vc = active
        active = nil
        lock.unlock()
        guard let vc else { return }
        if vc.presentingViewController != nil {
            DispatchQueue.main.async { vc.dismiss(animated: true) }
        }
    }
}

@_cdecl("chatx_filepick_pick")
public func chatx_filepick_pick() -> Int32 {
    FilePickShim.shared.start() ? 0 : 1
}

@_cdecl("chatx_filepick_cancel")
public func chatx_filepick_cancel() {
    FilePickShim.shared.cancel()
}

#elseif canImport(AppKit)

// ── macOS: NSOpenPanel ─────────────────────────────────────────────────────

private func presentMacPanel() -> Bool {
    guard let key = NSApp.keyWindow ?? NSApp.windows.first(where: { $0.isVisible }) else {
        deliver(nil)
        return false
    }
    let panel = NSOpenPanel()
    panel.allowsMultipleSelection = false
    panel.canChooseDirectories = false
    panel.canChooseFiles = true
    panel.canCreateDirectories = false
    panel.message = "Choose a document"
    panel.prompt = "Choose"
    panel.beginSheetModal(for: key) { resp in
        if resp == .OK, let u = panel.url {
            deliver(u.path)
        } else {
            deliver(nil)
        }
    }
    return true
}

@_cdecl("chatx_filepick_pick")
public func chatx_filepick_pick() -> Int32 {
    presentMacPanel() ? 0 : 1
}

@_cdecl("chatx_filepick_cancel")
public func chatx_filepick_cancel() {
    // No active state to clear on the macOS path.
}

#endif
