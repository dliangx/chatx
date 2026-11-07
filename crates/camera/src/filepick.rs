#[cfg(target_os = "macos")]
use apple as backend;
#[cfg(target_os = "macos")]
mod apple {
    use std::sync::{Mutex, OnceLock};

    static SINK: OnceLock<Mutex<Box<dyn Fn(Option<String>) + Send>>> = OnceLock::new();

    fn deliver(path: Option<String>) {
        if let Some(slot) = SINK.get() {
            if let Ok(g) = slot.lock() {
                (g)(path)
            }
        }
    }

    pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink)
            }
        } else {
            let _ = SINK.set(Mutex::new(Box::new(sink)));
        }
    }

    /// Swift ⇄ Rust trampoline. The macOS AppKit branch of `shim/filepick.swift`
    /// calls this to hand back a `NSOpenPanel` result (`(nil, 0)` on cancel).
    /// The primary file-pick path on macOS is `osascript` (see `pick_file`),
    /// but defining the symbol here is required so that linking the shared
    /// `shim_filepick.o` (which is pulled in to resolve `chatx_open_reveal`
    /// from `reveal.rs`) does not leave an undefined reference.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn _chatx_rust_filepick_done(data: *const u8, n: i32) {
        let path = if data.is_null() || n <= 0 {
            None
        } else {
            let buf = unsafe { std::slice::from_raw_parts(data, n as usize) };
            let s = String::from_utf8_lossy(buf).to_string().trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        deliver(path);
    }

    pub fn pick_file() -> Result<(), String> {
        let child = std::process::Command::new("osascript")
            .arg("-e")
            .arg("set f to choose file with prompt \"Choose a PDF document\"")
            .arg("-e")
            .arg("posix path of f")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to launch file picker: {e}"))?;
        std::thread::spawn(move || match child.wait_with_output() {
            Ok(o) => {
                let line = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string());
                if line.is_none() {
                    let status = o.status.code().unwrap_or(-1);
                    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    if status != -1 {
                        tracing::warn!(
                            "[filepick] osascript cancelled/failed status={status} err={err}"
                        );
                    } else {
                        tracing::debug!("[filepick] osascript exited status={status} err={err}");
                    }
                }
                deliver(line);
            }
            Err(e) => {
                tracing::warn!("[filepick] picker exit error: {e}");
                deliver(None);
            }
        });
        Ok(())
    }
    pub fn cancel() {}
}

/// iOS — driven by the native Swift shim `shim/filepick.swift`
/// (`UIDocumentPickerViewController`). Selection is handed back to Rust via
/// the `_chatx_rust_filepick_done` trampoline as a single absolute path, or
/// `(nil, 0)` on cancel / no file.
#[cfg(target_os = "ios")]
use ios as backend;
#[cfg(target_os = "ios")]
mod ios {
    use std::sync::{Mutex, OnceLock};

    static SINK: OnceLock<Mutex<Box<dyn Fn(Option<String>) + Send>>> = OnceLock::new();

    /// Swift → Rust trampoline (declared via `@_silgen_name("_chatx_rust_filepick_done")`
    /// in `shim/filepick.swift`).
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn _chatx_rust_filepick_done(data: *const u8, n: i32) {
        let path = if data.is_null() || n <= 0 {
            None
        } else {
            let buf = unsafe { std::slice::from_raw_parts(data, n as usize) };
            let s = String::from_utf8_lossy(buf).to_string().trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        };
        if let Some(slot) = SINK.get() {
            if let Ok(g) = slot.lock() {
                (g)(path)
            }
        }
    }

    unsafe extern "C" {
        fn chatx_filepick_pick() -> i32;
        fn chatx_filepick_cancel();
    }

    pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink)
            }
        } else {
            let _ = SINK.set(Mutex::new(Box::new(sink)));
        }
    }

    pub fn pick_file() -> Result<(), String> {
        let r = unsafe { chatx_filepick_pick() };
        if r == 0 {
            Ok(())
        } else {
            Err("Failed to open the file picker (foreground window unavailable)".to_string())
        }
    }

    pub fn cancel() {
        unsafe { chatx_filepick_cancel() }
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
use desktop as backend;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod desktop {
    use std::sync::{Mutex, OnceLock};

    static SINK: OnceLock<Mutex<Box<dyn Fn(Option<String>) + Send>>> = OnceLock::new();

    fn deliver(path: Option<String>) {
        if let Some(slot) = SINK.get() {
            if let Ok(g) = slot.lock() {
                (g)(path)
            }
        }
    }

    pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink)
            }
        } else {
            let _ = SINK.set(Mutex::new(Box::new(sink)));
        }
    }

    pub fn pick_file() -> Result<(), String> {
        let (cmd, args) = if cfg!(target_os = "linux") {
            let bin = if std::path::Path::new("/usr/bin/zenity").exists() {
                "/usr/bin/zenity"
            } else if std::path::Path::new("/usr/bin/kdialog").exists() {
                "/usr/bin/kdialog"
            } else {
                "/usr/bin/zenity"
            };
            let args: Vec<String> = if bin.ends_with("zenity") {
                vec![
                    "--file-selection".into(),
                    "--title=Choose a PDF document".into(),
                ]
            } else {
                vec![
                    "--getopenfile".into(),
                    "*".into(),
                    "--title=Choose a PDF document".into(),
                ]
            };
            (bin.to_string(), args)
        } else {
            let filter = "PDF documents (*.pdf)|*.pdf|All files (*.*)|*.*";
            (
                "powershell.exe".into(),
                vec![
                    "-NoProfile".into(),
                    "-Command".into(),
                    format!(
                        "Add-Type -AssemblyName System.Windows.Forms; $f = New-Object System.Windows.Forms.OpenFileDialog; $f.Multiselect = $false; $f.Filter = '{filter}'; if ($f.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{ Write-Output $f.FileName }}"
                    ),
                ],
            )
        };
        let child = std::process::Command::new(cmd)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to launch file picker: {e}"))?;
        std::thread::spawn(move || match child.wait_with_output() {
            Ok(o) => {
                let line = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty());
                deliver(line);
            }
            Err(e) => {
                tracing::warn!("[filepick] picker exit error: {e}");
                deliver(None);
            }
        });
        Ok(())
    }
    pub fn cancel() {}
}

/// Android — driven by the Java shell's SAF picker
/// (`Intent.ACTION_OPEN_DOCUMENT`). The shell copies the chosen file to the
/// app's private dir and calls back into Rust via
/// `NativeBridge.filePickDone(String)` (a JNI native method in
/// `crates/bridge/src/jni.rs`) which delivers `Some(path)` to the
/// `bridge` file-pick consumer. Cancels deliver `None` (empty path).
#[cfg(target_os = "android")]
use android as backend;
#[cfg(target_os = "android")]
mod android {
    /// Install the consumer that receives the picked file (or `None`).
    /// `bridge` owns the slot; this replaces the previous consumer so a
    /// freshly-armed pick delivers to the latest caller.
    pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(sink: F) {
        bridge::set_file_pick_consumer(Box::new(sink))
    }

    pub fn pick_file() -> Result<(), String> {
        bridge::request_file_pick();
        Ok(())
    }

    pub fn cancel() {}
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "android"
)))]
use stub as backend;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "android"
)))]
mod stub {
    pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(_sink: F) {}
    pub fn pick_file() -> Result<(), String> {
        Err("File picking is not supported on this platform yet".to_string())
    }
    pub fn cancel() {}
}

/// Install the callback that receives the file-picker selection: `Some(path)` on pick, `None` when the user cancelled / nothing was picked.
pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(sink: F) {
    backend::set_file_sink(sink)
}

/// Open the system file picker for a single file; `Err` only if it could not be presented — the selection is delivered asynchronously to the installed sink.
pub fn pick_file() -> Result<(), String> {
    backend::pick_file()
}

/// Cancel any in-flight picker (best-effort; safe if none is active).
pub fn cancel() {
    backend::cancel()
}
