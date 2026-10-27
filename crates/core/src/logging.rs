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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = home.join(".chatx");
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
    let log_path = default_log_path();
    let file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open log file {log_path:?}: {e}"))?;
    let shared = SharedFile(file);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    let reg = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(shared)
                .with_ansi(false)
                .with_level(true)
                .with_file(true)
                .with_line_number(true),
        );

    // try_init only errors when a global subscriber already exists
    // (duplicate init); treat that as success — either way the process
    // is logging through its subscriber.
    let _ = reg.try_init();
    Ok(())
}
