//! Single-document file picker for the chat "send a file" flow.
//!
//! Mirrors [`crate::album`] but returns exactly one file path, aimed at the
//! message-able documents (pdf / docx / xlsx / pptx). Per-target backends:
//!
//! - **macOS** — `osascript` NSOpenPanel (`choose file`, POSIX path on
//!   stdout). Extension filtering is enforced by the app layer — the CLI
//!   panel's `of type` UTI list is version-sensitive and unreliable.
//! - **Windows** — PowerShell `OpenFileDialog` (single selection) with a
//!   documents filter as default.
//! - **Linux** — `zenity --file-selection` (kdialog fallback); no extension
//!   filter available through the CLI, so the app enforces it afterwards.
//! - **Fallback (iOS / Android / other)** — no-op error; `pick_file` returns
//!   `Err` so the UI can show "not supported" and stay functional.
//!
//! Public surface (free functions under `camera::filepick`) is identical
//! across all targets:
//!   - `set_file_sink(F: Fn(Option<String>) + Send + 'static)`
//!   - `pick_file() -> Result<(), String>`
//!   - `cancel()`

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

    pub fn pick_file() -> Result<(), String> {
        // One file only. On macOS .docx/.xlsx/.pptx are directory-bundles;
        // the open panel treats them as single files by default, and the
        // app side validates the extension afterwards.
        let child = std::process::Command::new("osascript")
            .arg("-e")
            .arg("set f to choose file with prompt \"Choose a file to send\"")
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
                        // -1 or non-ok status with no stdout: cancelled or failed.
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
                    "--title=Choose a file to send".into(),
                ]
            } else {
                vec![
                    "--getopenfile".into(),
                    "*".into(),
                    "--title=Choose a file to send".into(),
                ]
            };
            (bin.to_string(), args)
        } else {
            let filter =
                "Documents (*.pdf;*.doc;*.docx;*.xls;*.xlsx;*.ppt;*.pptx;*.txt)|*.pdf;*.doc;*.docx;*.xls;*.xlsx;*.ppt;*.pptx;*.txt|All files (*.*)|*.*";
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

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux"
)))]
use stub as backend;
#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux"
)))]
mod stub {
    pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(_sink: F) {}
    pub fn pick_file() -> Result<(), String> {
        Err(
            "File picking is not supported on this platform yet; please open with a supporting app"
                .to_string(),
        )
    }
    pub fn cancel() {}
}

/// Install the callback that receives the file-picker selection:
/// `Some(path)` on pick, `None` when the user cancelled / nothing was picked.
pub fn set_file_sink<F: Fn(Option<String>) + Send + 'static>(sink: F) {
    backend::set_file_sink(sink)
}

/// Open the system file picker for a single document. Returns `Err` only if
/// the picker could not be presented — the user's selection is delivered
/// asynchronously to the installed sink.
pub fn pick_file() -> Result<(), String> {
    backend::pick_file()
}

/// Cancel any in-flight picker (best-effort; safe if none is active).
pub fn cancel() {
    backend::cancel()
}
