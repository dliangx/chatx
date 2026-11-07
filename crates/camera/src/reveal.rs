//! Reveal (open in system file manager / external-app chooser) a local file
//! path. Used whenever the in-app viewer cannot handle a file (Office docs
//! on mobile, chat files of unsupported extension, …).
//!
//! Per-platform strategy:
//! - **Android** — JNI call into `NativeBridge.reveal(String path)` which
//!   wraps the path in a `content://com.chatx.fp/...` URI (or a `file://`
//!   fallback) and fires `Intent.createChooser(ACTION_VIEW)`.
//! - **iOS** — C-ABI call `chatx_open_reveal(ptr, len)` into the Swift shim
//!   (`shim/filepick.swift`), which presents `UIActivityViewController`
//!   on the foreground window with the file's `file://` URL.
//! - **macOS** — C-ABI call `chatx_open_reveal(ptr, len)` into the Swift
//!   shim, which uses `NSWorkspace.shared.activateFileViewerSelecting` to
//!   open the Finder with the file highlighted (not opened).
//! - **Windows / Linux** — shell out to the OS file manager with the file's
//!   parent directory selected / highlighted (so the user can see exactly
//!   where it lives): `explorer /select,`; `xdg-open` of the parent.

#[cfg(target_os = "android")]
pub fn reveal(path: &str) -> bool {
    bridge::reveal_file(path)
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn reveal(path: &str) -> bool {
    unsafe extern "C" {
        fn chatx_open_reveal(ptr: *const u8, len: i32) -> i32;
    }
    let b = path.as_bytes();
    let r = unsafe { chatx_open_reveal(b.as_ptr(), b.len() as i32) };
    r == 0
}

#[cfg(target_os = "windows")]
pub fn reveal(path: &str) -> bool {
    // `explorer /select,path` opens Explorer with the file highlighted.
    match std::process::Command::new("explorer.exe")
        .arg(format!("/select,{path}"))
        .spawn()
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("[reveal] explorer failed: {e:?}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
pub fn reveal(path: &str) -> bool {
    // Best-effort: open the parent directory (xdg-open is desktop-agnostic);
    // per-DE "select this file" (nautilus --select / dolphin / nemo) is
    // left to the DE to decide what it does with the directory.
    let parent = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    match std::process::Command::new("xdg-open").arg(parent).spawn() {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("[reveal] xdg-open failed: {e:?}");
            false
        }
    }
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "macos",
    target_os = "windows",
    target_os = "linux"
)))]
pub fn reveal(_path: &str) -> bool {
    tracing::warn!("[reveal] not supported on this platform");
    false
}
