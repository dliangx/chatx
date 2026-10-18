//! Photo-album picker for the "share a moment" flow.
//!
//! Per-target backends:
//! - **Apple (macOS / iOS)** — Swift shim (`shim/photos.swift` compiled by
//!   `build.rs`): `PHPickerViewController` on iOS, `NSOpenPanel` on macOS.
//!   Selection delivered to Rust via the `_chatx_rust_photos_done` trampoline
//!   as a flat `[len: u32 LE][path]…` byte stream of locally-saved copies of
//!   the picked assets.
//! - **Desktop (Windows / Linux)** — spawn the native file dialog on a
//!   background thread; parse stdout for the chosen paths and deliver to the
//!   installed sink. No new deps (`std::process::Command` only).
//! - **Fallback** — no-op; `pick_photos` returns `Err` so the UI can show a
//!   "暂不支持相册选图" message and still let the user use the camera flow.
//!
//! Public surface (free functions under `camera::album`) is identical
//! across all targets:
//!   - `set_photos_sink(F: Fn(Vec<String>) + Send + 'static)`
//!   - `pick_photos(max: u32) -> Result<(), String>`
//!   - `cancel()`

#[cfg(any(target_os = "macos", target_os = "ios"))]
use apple as backend;
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple {
    use std::sync::{Mutex, OnceLock};

    static PHOTOS_SINK: OnceLock<Mutex<Box<dyn Fn(Vec<String>) + Send>>> = OnceLock::new();

    /// Swift → Rust trampoline (declared via `@_silgen_name` in
    /// `shim/photos.swift`).
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn _chatx_rust_photos_done(data: *const u8, n: i32) {
        let out: Vec<String> = if data.is_null() || n <= 0 {
            Vec::new()
        } else {
            let buf = unsafe { std::slice::from_raw_parts(data, n as usize) };
            let mut out = Vec::new();
            let mut i = 0usize;
            while i + 4 <= buf.len() {
                let mut a = [0u8; 4];
                a.copy_from_slice(&buf[i..i + 4]);
                let len = u32::from_le_bytes(a) as usize;
                i += 4;
                if i + len > buf.len() {
                    break;
                }
                let s = String::from_utf8_lossy(&buf[i..i + len])
                    .to_string()
                    .trim()
                    .to_string();
                i += len;
                if !s.is_empty() {
                    out.push(s);
                }
            }
            out
        };
        if let Some(slot) = PHOTOS_SINK.get() {
            if let Ok(g) = slot.lock() { (g)(out) }
        }
    }

    unsafe extern "C" {
        fn chatx_photos_pick(max: i32) -> i32;
        fn chatx_photos_pick_cancel();
    }

    pub fn set_photos_sink<F: Fn(Vec<String>) + Send + 'static>(sink: F) {
        if let Some(slot) = PHOTOS_SINK.get() {
            if let Ok(mut g) = slot.lock() { *g = Box::new(sink) }
        } else {
            let _ = PHOTOS_SINK.set(Mutex::new(Box::new(sink)));
        }
    }
    pub fn pick_photos(max: u32) -> Result<(), String> {
        let r = unsafe { chatx_photos_pick(max as i32) };
        if r == 0 { Ok(()) } else { Err("无法打开系统相册（请检查相册权限 / Info.plist）".to_string()) }
    }
    pub fn cancel() {
        unsafe { chatx_photos_pick_cancel() }
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
use desktop as backend;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod desktop {
    use std::sync::{Mutex, OnceLock};

    static SINK: OnceLock<Mutex<Box<dyn Fn(Vec<String>) + Send>>> = OnceLock::new();

    pub fn set_photos_sink<F: Fn(Vec<String>) + Send + 'static>(sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() { *g = Box::new(sink) }
        } else {
            let _ = SINK.set(Mutex::new(Box::new(sink)));
        }
    }

    fn deliver(paths: Vec<String>) {
        if let Some(slot) = SINK.get() { if let Ok(g) = slot.lock() { (g)(paths) } }
    }

    pub fn pick_photos(max: u32) -> Result<(), String> {
        let max = (max as usize).max(1).min(9);
        let (cmd, args) = if cfg!(target_os = "linux") {
            let bin = if std::path::Path::new("/usr/bin/zenity").exists() { "/usr/bin/zenity" }
                else if std::path::Path::new("/usr/bin/kdialog").exists() { "/usr/bin/kdialog" }
                else { "/usr/bin/zenity" };
            let args: Vec<String> = if bin.ends_with("zenity") {
                vec!["--file-selection".into(), "--multiple".into(), "--title=选择照片".into()]
            } else {
                vec!["--getopenfile".into(), "*".into(), "--multiple".into(), "--title=选择照片".into()]
            };
            (bin, args)
        } else {
            (
                "powershell.exe",
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    "Add-Type -AssemblyName System.Windows.Forms; $f = New-Object System.Windows.Forms.OpenFileDialog; $f.Multiselect = $true; $f.Filter = '图片 (*.jpg;*.jpeg;*.png;*.heic;*.webp)|*.jpg;*.jpeg;*.png;*.heic;*.webp'; if ($f.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { $f.FileNames | ForEach-Object { Write-Output $_ } }".into(),
                ],
            )
        };
        let mut child = std::process::Command::new(cmd)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("无法启动选图程序: {e}"))?;
        std::thread::spawn(move || {
            match child.wait_with_output() {
                Ok(o) => {
                    let mut paths: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .take(max)
                        .collect();
                    if paths.len() > max { paths.truncate(max) }
                    deliver(paths);
                }
                Err(e) => {
                    eprintln!("[album] picker exit error: {e}");
                    deliver(Vec::new());
                }
            }
        });
        Ok(())
    }
    pub fn cancel() {}
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux"
)))]
use stub as backend;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux"
)))]
mod stub {
    pub fn set_photos_sink<F: Fn(Vec<String>) + Send + 'static>(_sink: F) {}
    pub fn pick_photos(_max: u32) -> Result<(), String> {
        Err("当前平台暂不支持相册选图，请使用拍照".to_string())
    }
    pub fn cancel() {}
}

/// Install the callback that receives the photo-picker selection:
/// a list of on-disk paths (in user-selected order) of locally-saved
/// copies of the picked photos. Empty `Vec` ⇒ user cancelled / nothing picked.
pub fn set_photos_sink<F: Fn(Vec<String>) + Send + 'static>(sink: F) {
    backend::set_photos_sink(sink)
}

/// Open the system photo picker (up to `max` photos). Returns `Err` only if
/// the picker could not be presented — the user's selection is delivered
/// asynchronously to the installed sink.
pub fn pick_photos(max: u32) -> Result<(), String> {
    backend::pick_photos(max)
}

/// Cancel any in-flight picker (best-effort; safe if none is active).
pub fn cancel() {
    backend::cancel()
}
