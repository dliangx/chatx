//! Consumer slot storage.
//!
//! Each slot holds exactly one callback, replaceable at any time (most
//! recent wins). Callbacks are invoked on the calling thread — the slot
//! itself does no cross-thread dispatch.
//!
//! Two slot shapes, matching the two kinds of frames the bridge carries:
//!
//! - **Pixel** (camera, screen): `(bytes, w, h, fmt)` — `fmt` is a
//!   [`crate::types::PixelFormat::as_i32()`] value. Introduced so the Rust
//!   consumer (e.g. `camera::android`) can dispatch on the shell's pixel
//!   format; the shell is contractually allowed to emit anything the
//!   consumer advertised.
//! - **Audio** (mic, speaker): `(bytes, rate, channels)` — PCM, no notion
//!   of "format" beyond `s16le` which the crate-wide constant documents.

use std::sync::Mutex;

pub struct Slot<F> {
    inner: Mutex<Option<Box<dyn Fn(F) + Send + 'static>>>,
}

impl<F> Slot<F> {
    pub const fn new() -> Self {
        Slot {
            inner: Mutex::new(None),
        }
    }

    pub fn set(&self, cb: Box<dyn Fn(F) + Send + 'static>) {
        *self.inner.lock().unwrap() = Some(cb);
    }

    pub fn call(&self, input: F) -> bool {
        let guard = self.inner.lock().unwrap();
        match guard.as_ref() {
            Some(cb) => {
                cb(input);
                true
            }
            None => false,
        }
    }
}

/// 4-ary pixel slot: `(bytes, width, height, fmt)`.
macro_rules! pixel_slot {
    ($name:ident) => {
        pub mod $name {
            use crate::sinks::Slot;
            use std::sync::OnceLock;
            #[doc(hidden)]
            pub(crate) static SLOT: OnceLock<Slot<(Box<[u8]>, u32, u32, u32)>> = OnceLock::new();

            fn slot() -> &'static Slot<(Box<[u8]>, u32, u32, u32)> {
                SLOT.get_or_init(|| Slot::new())
            }

            pub fn set(cb: Box<dyn Fn(&[u8], u32, u32, u32) + Send + 'static>) {
                slot().set(Box::new(move |owned: (Box<[u8]>, u32, u32, u32)| {
                    let (bytes, a, b, f) = owned;
                    cb(&bytes, a, b, f);
                }));
            }

            /// Invoke the installed consumer, if any. Returns `true` when a
            /// consumer was present and ran, `false` otherwise.
            ///
            /// Cross-target: on mobile this is driven by `jni.rs`/`ffi.rs`;
            /// on desktop most slots are installed but only `deliver_screen_*`
            /// routes through `sinks::screen::call`, leaving the rest unreferenced —
            /// hence the `allow(dead_code)`.
            #[allow(dead_code)]
            pub fn call(bytes: Box<[u8]>, a: u32, b: u32, fmt: u32) -> bool {
                slot().call((bytes, a, b, fmt))
            }
        }
    };
}

/// 3-ary audio slot: `(bytes, sample_rate, channels)`.
macro_rules! audio_slot {
    ($name:ident) => {
        pub mod $name {
            use crate::sinks::Slot;
            use std::sync::OnceLock;
            #[doc(hidden)]
            pub(crate) static SLOT: OnceLock<Slot<(Box<[u8]>, u32, u32)>> = OnceLock::new();

            fn slot() -> &'static Slot<(Box<[u8]>, u32, u32)> {
                SLOT.get_or_init(|| Slot::new())
            }

            pub fn set(cb: Box<dyn Fn(&[u8], u32, u32) + Send + 'static>) {
                slot().set(Box::new(move |owned: (Box<[u8]>, u32, u32)| {
                    let (bytes, a, b) = owned;
                    cb(&bytes, a, b);
                }));
            }

            #[allow(dead_code)]
            pub fn call(bytes: Box<[u8]>, a: u32, b: u32) -> bool {
                slot().call((bytes, a, b))
            }
        }
    };
}

pixel_slot!(camera);
audio_slot!(audio_in);
pixel_slot!(screen);
audio_slot!(audio_out);

/// Single-string slot used by the mobile file pickers (iOS / Android).
///
/// - **iOS** — the Swift shim (`shim/filepick.swift`) delivers a single
///   absolute path (or `None` on cancel) via `_chatx_rust_filepick_done`.
/// - **Android** — the Java shell copies the picked file to the app's private
///   `getFilesDir()` and calls `NativeBridge.filePickDone(String)` (native
///   method implemented in `jni.rs`), which delivers `Some(path)` to this slot.
/// `None` is delivered (empty path) on cancel.
///
/// The consumer (typically `camera::filepick`) installs a `Fn(Option<String>)`
/// callback on this slot.
#[allow(dead_code)]
pub mod filepick {
    use super::Slot;
    use std::sync::OnceLock;

    #[doc(hidden)]
    pub(crate) static SLOT: OnceLock<Slot<Option<String>>> = OnceLock::new();

    fn slot() -> &'static Slot<Option<String>> {
        SLOT.get_or_init(|| Slot::new())
    }

    /// Install the consumer called when the user makes (or cancels) a selection.
    /// `Some(path)` = file picked (absolute path readable by the consumer);
    /// `None` = user cancelled or the picker could not present.
    pub fn set(cb: Box<dyn Fn(Option<String>) + Send + 'static>) {
        slot().set(cb)
    }

    /// Deliver a picked-file path to the installed consumer. `empty` (or
    /// `None`) means "no selection" / "cancelled". Returns `true` when a
    /// consumer was installed and ran, `false` otherwise.
    pub fn deliver(path: Option<String>) -> bool {
        match path {
            Some(p) if !p.trim().is_empty() => slot().call(Some(p)),
            _ => slot().call(None),
        }
    }
}

/// Multi-file slot used by the mobile photo pickers (iOS / Android).
///
/// The consumer (typically `camera::album`) installs a `Fn(Vec<String>)`
/// callback on this slot; an empty `Vec` means "user cancelled / nothing
/// picked".
#[allow(dead_code)]
pub mod photos {
    use super::Slot;
    use std::sync::OnceLock;

    #[doc(hidden)]
    pub(crate) static SLOT: OnceLock<Slot<Vec<String>>> = OnceLock::new();

    fn slot() -> &'static Slot<Vec<String>> {
        SLOT.get_or_init(|| Slot::new())
    }

    pub fn set(cb: Box<dyn Fn(Vec<String>) + Send + 'static>) {
        slot().set(cb)
    }

    /// Deliver a list of picked-file paths (or an empty vec on cancel).
    pub fn deliver(paths: Vec<String>) -> bool {
        // Filter out empty / whitespace entries (e.g. trailing delimiters in
        // the null-joined transport string).
        let cleaned: Vec<String> = paths
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        slot().call(cleaned)
    }
}
