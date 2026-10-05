//! # `bridge` — stable C-ABI capture surface
//!
//! This crate is the ONLY surface the native shells (Swift on iOS, Kotlin on
//! Android) talk to. It is deliberately dependency-free (no tokio, no slint,
//! no async) so it compiles on every target without pulling in the runtime
//! app.
//!
//! ## Design
//!
//! - **Fixed C-ABI**: all exported fns are `#[unsafe(no_mangle)] extern "C"`,
//!   args are raw pointers / i32 / usize. No Rust pointers or traits cross
//!   the boundary.
//! - **Pixel formats / sample layouts** are integer constants (see
//!   [`types::PixelFormat`]).
//! - **Rust-side consumers**: call [`set_camera_consumer`],
//!   [`set_audio_consumer`], [`set_screen_consumer`],
//!   [`set_audio_sink`] on the UI thread during app startup. Callbacks
//!   run on the thread the native shell used — it is the consumer's
//!   responsibility to hop (e.g. `slint::invoke_from_event_loop`).
//! - **Error convention**: every FFI fn returns `i32`. `0` = OK, negative
//!   values are [`ffi::ERR_*`] codes.
//!
//! ## Pixel format
//!
//! The shell decides how to deliver pixels — it sends `RGBA8888`,
//! `GRAY8`, or one of the packed YUV formats (`NV12`, `NV21`, `I420`,
//! `YV12`), and the Rust consumer receives `fmt` as a 4th argument so it
//! can pick a decode path. The `RGBA8888`-only contract from the crate's
//! v0.1 design is now **advisory** (the shell is encouraged to emit RGBA
//! when convenient), not **required**.

pub mod types;

#[cfg(target_os = "ios")]
pub mod ffi;

#[cfg(all(target_os = "ios", feature = "has-ios-shim"))]
mod shim_impl;

#[cfg(target_os = "android")]
pub mod jni;

mod sinks;

/// Ask the native shell to (start | stop) an in-app screen capture.
/// - **Android**: JNI static call into `com.chatx.NativeBridge.{start,stop}ScreenShare()`
///   (MediaProjection + VirtualDisplay + ImageReader).
/// - **iOS**: extern "C" call into `chatx_screen_capture_{start,stop}()` from
///   the ObjC shim (RPScreenRecorder). No-op with a stderr note when the
///   `has-ios-shim` feature is off (no real iOS SDK on the build host).
#[cfg(target_os = "android")]
pub fn request_screen_share_start() {
    jni::request_screen_share_start();
}
#[cfg(target_os = "android")]
pub fn request_screen_share_stop() {
    jni::request_screen_share_stop();
}

/// Ask the Android shell to (start | stop) the mic capture loop.
///
/// The mic is **opt-in**: it only runs while a call is active. The Rust side
/// holds the `CALL_AUD` slot (see
/// `apps/chat::call_dispatcher::set_aud_source`) so incoming `audioPcmIn`
/// frames land in the active call's `AudSource`. Starting / stopping the
/// shell's `AudioRecord` from the call's own lifecycle avoids holding the
/// microphone open (and draining battery) on every idle second.
///
/// No-op on non-Android targets (cpal is the mic there — see
/// `apps/chat::call::bootstrap_media`).
#[cfg(target_os = "android")]
pub fn request_mic_start() {
    jni::request_mic_start();
}
#[cfg(target_os = "android")]
pub fn request_mic_stop() {
    jni::request_mic_stop();
}
#[cfg(all(target_os = "ios", feature = "has-ios-shim"))]
pub fn request_screen_share_start() {
    shim_impl::request_screen_share_start();
}
#[cfg(all(target_os = "ios", feature = "has-ios-shim"))]
pub fn request_screen_share_stop() {
    shim_impl::request_screen_share_stop();
}
#[cfg(all(target_os = "ios", not(feature = "has-ios-shim")))]
pub fn request_screen_share_start() {
    eprintln!("[bridge] screen-share start: no iOS shim linked (enable feature `has-ios-shim`)");
}
#[cfg(all(target_os = "ios", not(feature = "has-ios-shim")))]
pub fn request_screen_share_stop() {
    eprintln!("[bridge] screen-share stop: no iOS shim linked");
}

/// Register the consumer that receives **incoming camera frames**.
/// Call this on the UI thread during app startup, before the shell starts
/// capture. Replace any previously-installed consumer.
///
/// The callback receives the raw bytes the shell sent, plus `w`, `h`, and
/// `fmt` — a [`types::PixelFormat::as_i32()`] value. The Rust consumer
/// (`camera::android` etc.) is responsible for interpreting `fmt` and
/// selecting the right decode path.
///
/// Not thread-restricted, but MUST be installed before the shell calls
/// `bridge_camera_frame_in`. The callback is invoked on whatever thread the
/// native capture side used.
pub fn set_camera_consumer(cb: Box<dyn Fn(&[u8], u32, u32, u32) + Send + 'static>) {
    sinks::camera::set(cb)
}

/// Register the consumer that receives **incoming microphone PCM** (s16le).
/// Sample rate + channel count are passed verbatim in the arguments.
pub fn set_audio_consumer(
    cb: Box<dyn Fn(&[u8], u32, u32) + Send + 'static>,
) {
    sinks::audio_in::set(cb)
}

/// Register the consumer that receives **incoming screen-capture frames**.
/// Same `(bytes, w, h, fmt)` shape as [`set_camera_consumer`].
pub fn set_screen_consumer(cb: Box<dyn Fn(&[u8], u32, u32, u32) + Send + 'static>) {
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
    sinks::screen::call(
        bytes.into_boxed_slice(),
        width,
        height,
        types::PixelFormat::Rgba8888.as_i32() as u32,
    )
}

/// Register the receiver that plays **outgoing speaker/remote audio** (s16le).
/// The argument is a raw PCM buffer; sample rate + channels are passed.
pub fn set_audio_sink(
    cb: Box<dyn Fn(&[u8], u32, u32) + Send + 'static>,
) {
    sinks::audio_out::set(cb)
}

/// Convenience: install camera consumer that logs frames to stderr. Useful
/// for smoke-testing the FFI path before the UI wires up.
#[cfg(debug_assertions)]
pub fn install_debug_camera_consumer() {
    set_camera_consumer(Box::new(|bytes, w, h, fmt| {
        eprintln!(
            "[bridge.debug] camera {}x{} {}B fmt={}",
            w,
            h,
            bytes.len(),
            fmt
        );
    }));
}

#[cfg(debug_assertions)]
pub fn install_debug_audio_consumer() {
    set_audio_consumer(Box::new(|bytes, rate, ch| {
        eprintln!("[bridge.debug] mic audio {}B @{}Hz/{}ch", bytes.len(), rate, ch);
    }));
}
