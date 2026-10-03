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
