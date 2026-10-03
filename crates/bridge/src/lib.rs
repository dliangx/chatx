//! # `bridge` — stable C-ABI capture surface
//!
//! This crate is the ONLY surface the native shells (Swift on iOS, Kotlin on
//! Android) talk to. It is deliberately dependency-free (no tokio, no slint,
//! no async) so it compiles on every target without pulling in the runtime
//! app.
//!
//! ## Design
//!
//! - **Fixed C-ABI**: all exported fns are `#[no_mangle] extern "C"`, args are
//!   raw pointers / i32 / usize. No Rust pointers or traits cross the boundary.
//! - **Pixel formats / sample layouts** are integer constants (see
//!   [`types::PixelFormat`], [`types::SAMPLE_LAYOUT_S16LE`]).
//! - **Rust-side consumers**: call [`set_camera_consumer`],
//!   [`set_audio_consumer`], [`set_screen_consumer`],
//!   [`set_audio_sink`] on the UI thread during app startup. Callbacks run on
//!   the thread the native shell used — it is the consumer's responsibility to
//!   hop (e.g. `slint::invoke_from_event_loop`).
//! - **Error convention**: every FFI fn returns `i32`. `0` = OK,
//!   negative values are [`ffi::ERR_*`] codes.

pub mod types;

#[cfg(target_os = "ios")]
pub mod ffi;

#[cfg(target_os = "android")]
pub mod jni;

mod sinks;

/// Register the consumer that receives **incoming camera frames** as RGBA8888.
/// Call this on the UI thread during app startup, before the shell starts
/// capture. Replace any previously-installed consumer.
///
/// The callback receives the **already-converted** RGBA bytes (the shell does
/// the pixel-format conversion; this crate never sees NV12/etc.) — the
/// `PixelData.fmt` field will always be [`types::RGBA8888`] when reached here.
///
/// Not thread-restricted, but MUST be installed before the shell calls
/// `bridge_camera_frame_in`. The callback is invoked on whatever thread the
/// native capture side used.
pub fn set_camera_consumer(cb: Box<dyn Fn(&[u8], u32, u32) + Send + 'static>) {
    sinks::camera::set(cb)
}

/// Register the consumer that receives **incoming microphone PCM** (s16le).
/// Sample rate + channel count are passed verbatim in the arguments.
pub fn set_audio_consumer(
    cb: Box<dyn Fn(&[u8], u32, u32) + Send + 'static>,
) {
    sinks::audio_in::set(cb)
}

/// Register the consumer that receives **incoming screen-capture frames** as
/// RGBA8888 (see [`set_camera_consumer`] re: pixel format).
pub fn set_screen_consumer(cb: Box<dyn Fn(&[u8], u32, u32) + Send + 'static>) {
    sinks::screen::set(cb)
}

/// Deliver an already-captured desktop screen frame (RGBA8888, `len == w*h*4`)
/// to the installed screen consumer.
///
/// This is the desktop funnel: the `screen` crate (or any host-side capture)
/// hands it a ready `Vec<u8>`, and it lands in the exact same consumer slot
/// that the iOS/Android FFI path (`bridge_screen_frame_in` → `sinks::screen`)
/// uses. One consumer covers all platforms.
///
/// Returns `true` if a consumer was installed and invoked, `false` if no
/// consumer was present (same meaning as the FFI `ERR_NO_CONSUMER` (-3)).
pub fn deliver_screen_frame(bytes: Vec<u8>, width: u32, height: u32) -> bool {
    sinks::screen::call(bytes.into_boxed_slice(), width, height)
}

/// Register the receiver that plays **outgoing speaker/remote audio** (s16le).
/// The argument is a raw PCM buffer; sample rate + channels are passed.
pub fn set_audio_sink(
    cb: Box<dyn Fn(&[u8], u32, u32) + Send + 'static>,
) {
    sinks::audio_out::set(cb)
}

/// Convenience: install camera consumer that logs frames to stderr. Useful for
/// smoke-testing the FFI path before M2/M4 wires the real UI.
#[cfg(debug_assertions)]
pub fn install_debug_camera_consumer() {
    set_camera_consumer(Box::new(|bytes, w, h| {
        eprintln!("[bridge.debug] camera {}x{} {}B", w, h, bytes.len());
    }));
}

#[cfg(debug_assertions)]
pub fn install_debug_audio_consumer() {
    set_audio_consumer(Box::new(|bytes, rate, ch| {
        eprintln!("[bridge.debug] mic audio {}B @{}Hz/{}ch", bytes.len(), rate, ch);
    }));
}
