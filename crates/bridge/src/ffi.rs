//! iOS C-ABI surface.
//!
//! All fns are `#[unsafe(no_mangle)] extern "C"` to match Swift `import`
//! expectations (Swift 6 / recent Xcode deprecates the implicit-no-unsafe
//! form for FFI definitions).
//!
//! Every fn returns `i32`:
//!   0      OK
//!  -1      ERR_NULL_PTR
//!  -2      ERR_INVALID_ARG
//!  -3      ERR_NO_CONSUMER    (Rust side never installed a handler)
//!  -4      ERR_UNSUPPORTED
//!  -5      ERR_INTERNAL
//!
//! Threading contract:
//!   - `bridge_camera_frame_in` — called from an AVFoundation metadata /
//!     video queue. May be concurrent with itself on future multi-stream.
//!     Callback runs on the caller's thread.
//!   - `bridge_audio_pcm_in`    — called from an `AVAudioEngine` render
//!     callback or Core Audio input tap. Low latency is the caller's
//!     responsibility; this crate does no blocking.
//!   - `bridge_screen_frame_in` — called from a ReplayKit / Broadcast
//!     Extension.
//!   - `bridge_audio_play_pcm`  — called from the shell when it wants to
//!     play remote speaker audio. Callback runs on the caller's thread; the
//!     consumer is expected to queue (or drop) as needed.

use crate::sinks;

pub const ERR_OK: i32 = 0;
pub const ERR_NULL_PTR: i32 = -1;
pub const ERR_INVALID_ARG: i32 = -2;
pub const ERR_NO_CONSUMER: i32 = -3;
pub const ERR_UNSUPPORTED: i32 = -4;
pub const ERR_INTERNAL: i32 = -5;

/// Incoming camera frame.
///
/// # Arguments
/// - `data`:  pointer to the pixel buffer (native shell's buffer, remains
///            valid for the duration of the call)
/// - `len`:   byte length of `data`
/// - `width`: width in pixels
/// - `height`: height in pixels
/// - `fmt`:   a [`crate::types::PixelFormat::as_i32()`] value (typically 1 = RGBA8888)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bridge_camera_frame_in(
    data: *const u8,
    len: usize,
    width: u32,
    height: u32,
    fmt: i32,
) -> i32 {
    check_incoming(data, len, width, height, fmt);
    let owned;
    {
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        owned = slice.to_vec().into_boxed_slice();
    }
    if !sinks::camera::call(owned, width, height, fmt as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

/// Incoming microphone PCM (s16le, mono/stereo).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bridge_audio_pcm_in(
    data: *const u8,
    len: usize,
    sample_rate: u32,
    channels: i32,
) -> i32 {
    if data.is_null() || len == 0 {
        return ERR_NULL_PTR;
    }
    if sample_rate == 0 || (channels != 1 && channels != 2) {
        return ERR_INVALID_ARG;
    }
    // s16le ⇒ bytes must be an even multiple of (1 sample * 2 bytes * channels)
    // for strict sanity, but shells often deliver burst-sized chunks that do
    // not line up perfectly; we do NOT reject on odd length.
    let owned;
    {
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        owned = slice.to_vec().into_boxed_slice();
    }
    if !sinks::audio_in::call(owned, sample_rate, channels as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

/// Incoming screen-capture frame (ReplayKit on iOS).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bridge_screen_frame_in(
    data: *const u8,
    len: usize,
    width: u32,
    height: u32,
    fmt: i32,
) -> i32 {
    check_incoming(data, len, width, height, fmt);
    let owned;
    {
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        owned = slice.to_vec().into_boxed_slice();
    }
    if !sinks::screen::call(owned, width, height, fmt as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

/// Outgoing speaker / remote audio (s16le).
///
/// The native shell's audio output node (AVAudioEngine output / remote I/O
/// unit on iOS) pulls from here — or, more commonly, the shell invokes us
/// with the received remote PCM and plays it into its own output path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bridge_audio_play_pcm(
    data: *const u8,
    len: usize,
    sample_rate: u32,
    channels: i32,
) -> i32 {
    if data.is_null() || len == 0 {
        return ERR_NULL_PTR;
    }
    if sample_rate == 0 || (channels != 1 && channels != 2) {
        return ERR_INVALID_ARG;
    }
    let owned;
    {
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        owned = slice.to_vec().into_boxed_slice();
    }
    if !sinks::audio_out::call(owned, sample_rate, channels as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

/// Report a native-shell-side permission state change. The consumer (M2)
/// typically uses this to disable the corresponding UI.
///
/// `perm` values: 1=camera, 2=microphone, 3=screen
/// `granted`:     1=granted, 0=denied
#[unsafe(no_mangle)]
pub extern "C" fn bridge_permission_changed(perm: i32, granted: i32) -> i32 {
    match perm {
        1 | 2 | 3 => {
            eprintln!("[bridge] permission perm={perm} granted={granted}");
            ERR_OK
        }
        _ => ERR_INVALID_ARG,
    }
}

fn check_incoming(data: *const u8, len: usize, width: u32, height: u32, _fmt: i32) -> i32 {
    if data.is_null() || len == 0 || width == 0 || height == 0 {
        ERR_NULL_PTR
    } else {
        ERR_OK
    }
}
