//! Minimal tracing-subscriber bootstrap: keep only WARN/ERROR, emit each line
//! to both stderr and a log file under `~/.chatx/`.
//!
//! Non-error levels (info/debug/trace) are filtered at runtime so they never
//! touch the file, the terminal, or any I/O — zero perf cost, no `eprintln!`
//! noise. Error + warn lines go to both stderr (for debugging) and the file
//! (for post-hoc search).

use std::io::BufWriter;
use std::path::PathBuf;

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Path used for the rolling log file. Defaults to `~/.chatx/chatx.log`;
/// override with RUST_LOG_FILE.
pub fn default_log_path() -> PathBuf {
    if let Ok(p) = std::env::var("RUST_LOG_FILE") {
        let p = PathBuf::from(p);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        return p;
    }
    // Reuse `home_dir()` so the log lives next to the keystore (and, on
    // Android, lands in the app-private data dir rather than an unwritable
    // `/tmp` — which on Android doesn't exist, so the subscriber init would
    // have been silently dropping every `tracing` call, including auth errors).
    let dir = crate::identity::DeviceIdentity::home_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("chatx.log")
}

/// Shared file handle; every `make_writer` call re-opens the same path via
/// `try_clone` on the underlying fd, so the process writes are appended to the
/// single file regardless of thread.
struct SharedFile(std::fs::File);

impl Clone for SharedFile {
    fn clone(&self) -> Self {
        Self(self.0.try_clone().expect("clone log fd"))
    }
}

impl<'a> MakeWriter<'a> for SharedFile {
    type Writer = BufWriter<std::fs::File>;
    fn make_writer(&'a self) -> Self::Writer {
        BufWriter::new(self.0.try_clone().expect("clone log fd"))
    }
}

/// Idempotent subscriber init. Returns Ok(()) on success, or if a global
/// subscriber is already installed (in which case we leave it alone).
pub fn init_default() -> Result<(), Box<dyn std::error::Error>> {
    init_at(&default_log_path())
}

/// Same as [`init_default`], but writes to an explicit `log_path` (its parent
/// directory is created if needed). Lets a binary — e.g. `chatx-signal` — log
/// to its own location without touching the shared default.
pub fn init_at(log_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open log file {log_path:?}: {e}"))?;
    let shared = SharedFile(file);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    let reg = tracing_subscriber::registry()
        .with(
            // Layer 1: file (persistent, under `home_dir()`; on Android that's
            // the app's private `files` dir, so the log survives reboots).
            tracing_subscriber::fmt::Layer::new()
                .with_writer(shared)
                .with_ansi(false)
                .with_level(true)
                .with_file(true)
                .with_line_number(true),
        )
        .with(
            // Layer 2: stderr — surfaces in `adb logcat` (Android) or the
            // terminal (desktop) for quick post-mortem without `run-as`.
            tracing_subscriber::fmt::Layer::new()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_level(true),
        )
        .with(filter);

    // try_init only errors when a global subscriber already exists
    // (duplicate init); treat that as success — either way the process
    // is logging through its subscriber.
    let _ = reg.try_init();
    Ok(())
}
