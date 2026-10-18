import Foundation
#if canImport(UIKit)
import UIKit
import PhotosUI
#elseif canImport(AppKit)
import AppKit
#endif
import UniformTypeIdentifiers

@_silgen_name("_chatx_rust_photos_done")
fileprivate func chatx_rust_photos_done(_ ptr: UnsafeRawPointer?, _ len: Int)

private func deliver(_ paths: [String]) {
    var flat = [UInt8]()
    for p in paths {
        let b = Array(p.utf8)
        withUnsafeBytes(of: UInt32(b.count)) { flat.append(contentsOf: $0) }
        flat.append(contentsOf: b)
    }
    flat.withUnsafeBufferPointer { buf in
        if let base = buf.baseAddress {
            chatx_rust_photos_done(UnsafeRawPointer(base), buf.count)
        } else {
            chatx_rust_photos_done(nil, 0)
        }
    }
}

private func saveToAlbumDir(_ data: Data) -> String? {
    let dir = NSTemporaryDirectory() + "chatx_album"
    guard (try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)) != nil else { return nil }
    let tsMs = Int(Date().timeIntervalSince1970 * 1000)
    let target = dir + "/photo_\(tsMs)_\(UUID().uuidString.prefix(6)).jpg"
    guard (try? data.write(to: URL(fileURLWithPath: target))) != nil else { return nil }
    return target
}

#if canImport(UIKit)

private final class PhotoPickerDelegate: NSObject, PHPickerViewControllerDelegate {
    static let shared = PhotoPickerDelegate()
    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        DispatchQueue.main.async { picker.dismiss(animated: true) }
        if results.isEmpty { deliver([]); return }
        let lock = NSLock()
        var remaining = results.count
        var saved: [String?] = Array(repeating: nil, count: results.count)
        func onOne(_ i: Int, _ data: Data?) {
            lock.lock()
            if saved[i] == nil, let d = data, let p = saveToAlbumDir(d) {
                saved[i] = p
            }
            remaining -= 1
            let last = (remaining == 0)
            lock.unlock()
            if last { deliver(saved.compactMap { $0 }) }
        }
        for (i, r) in results.enumerated() {
            r.itemProvider.loadDataRepresentation(forTypeIdentifier: UTType.image.identifier) { data, _ in
                onOne(i, data)
            }
        }
    }
}

#else

private func presentMacPanel(max: Int) -> Bool {
    guard let key = NSApp.keyWindow ?? NSApp.windows.first(where: { $0.isVisible }) else {
        return false
    }
    let panel = NSOpenPanel()
    panel.allowsMultipleSelection = (max > 1)
    panel.canChooseDirectories = false
    panel.canChooseFiles = true
    panel.canCreateDirectories = false
    if #available(macOS 11.0, *) {
        panel.allowedContentTypes = [UTType.image]
    }
    panel.message = "选择一张或多张照片（最多 \(max) 张）"
    panel.prompt = "选择"
    panel.beginSheetModal(for: key) { resp in
        if resp == .OK {
            deliver(panel.urls.map { $0.path })
        } else {
            deliver([])
        }
    }
    return true
}

#endif

private final class ActiveBox {
    #if canImport(UIKit)
    weak var vc: PHPickerViewController?
    #endif
}

private final class PickShim {
    static let shared = PickShim()
    private let lock = NSLock()
    private var active: ActiveBox?

    func start(maxIn: Int) -> Bool {
        let maxN = max(1, min(9, maxIn))
        #if canImport(UIKit)
        guard let sc = UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene })
            .first(where: { $0.activationState == .foregroundActive })
            ?? UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene }).first,
            let win = sc.windows.first(where: { $0.isKeyWindow }) ?? sc.windows.first
        else {
            return false
        }
        var cfg = PHPickerConfiguration(photoLibrary: .shared())
        cfg.selectionLimit = maxN
        let vc = PHPickerViewController(configuration: cfg)
        vc.delegate = PhotoPickerDelegate.shared
        vc.modalPresentationStyle = .fullScreen
        let box = ActiveBox(); box.vc = vc
        lock.lock(); active = box; lock.unlock()
        DispatchQueue.main.async {
            win.rootViewController?.present(vc, animated: true)
        }
        return true
        #else
        return presentMacPanel(max: maxN)
        #endif
    }

    func cancelActive() {
        lock.lock()
        let box = active
        active = nil
        lock.unlock()
        guard let box else { return }
        #if canImport(UIKit)
        DispatchQueue.main.async {
            if let vc = box.vc, vc.presentingViewController != nil {
                vc.dismiss(animated: true)
            }
        }
        #endif
    }
}

@_cdecl("chatx_photos_pick")
public func chatx_photos_pick(_ max: Int32) -> Int32 {
    PickShim.shared.start(maxIn: Int(max)) ? 0 : 1
}

@_cdecl("chatx_photos_pick_cancel")
public func chatx_photos_pick_cancel() {
    PickShim.shared.cancelActive()
}
