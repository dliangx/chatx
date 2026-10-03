//! Consumer slot storage.
//!
//! Each slot holds exactly one callback, replaceable at any time (most
//! recent wins). Callbacks are invoked on the calling thread — the slot
//! itself does no cross-thread dispatch.

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

macro_rules! frame_slot {
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

frame_slot!(camera);
frame_slot!(audio_in);
frame_slot!(screen);
frame_slot!(audio_out);
