//! Android JNI surface (jni 0.21).
//!
//! Kotlin bindings (package `com.chatx`):
//!
//! ```java
//! public final class NativeBridge {
//!     static { System.loadLibrary("bridge"); }
//!     public static native int cameraFrameIn(byte[] data, int len, int width, int height, int fmt);
//!     public static native int audioPcmIn(byte[] data, int len, int sampleRate, int channels);
//!     public static native int screenFrameIn(byte[] data, int len, int width, int height, int fmt);
//!     public static native int audioPlayPcm(byte[] data, int len, int sampleRate, int channels);
//!     public static native int permissionChanged(int perm, int granted);
//! }
//! ```
//!
//! Threading contract:
//!   - `cameraFrameIn` — called from CameraX `AnalyzeImageAnalyzer` thread
//!   - `audioPcmIn`    — called from Oboe input callback
//!   - `screenFrameIn` — called from MediaProjection's `ImageReader` dispatch
//!   - `audioPlayPcm`  — called from the shell's playback thread
//!
//! Consumers must hop to the UI thread (e.g. `slint::invoke_from_event_loop`).

use crate::sinks;

use jni::objects::{JByteArray, JClass};
use jni::sys::{jint, jsize};
use jni::JNIEnv;

pub const ERR_OK: jint = 0;
pub const ERR_NULL_PTR: jint = -1;
pub const ERR_INVALID_ARG: jint = -2;
pub const ERR_NO_CONSUMER: jint = -3;

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_cameraFrameIn(
    env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    width: jint,
    height: jint,
    fmt: jint,
) -> jint {
    if (width <= 0) || (height <= 0) {
        return ERR_INVALID_ARG;
    }
    let owned: Box<[u8]> = match env.convert_byte_array(data) {
        Ok(b) => b.into_boxed_slice(),
        Err(_) => return ERR_NULL_PTR,
    };
    if !sinks::camera::call(owned, width as u32, height as u32) {
        return ERR_NO_CONSUMER;
    }
    let _ = fmt;
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_audioPcmIn(
    env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    sample_rate: jint,
    channels: jint,
) -> jint {
    if (sample_rate <= 0) || (channels != 1 && channels != 2) {
        return ERR_INVALID_ARG;
    }
    let owned: Box<[u8]> = match env.convert_byte_array(data) {
        Ok(b) => b.into_boxed_slice(),
        Err(_) => return ERR_NULL_PTR,
    };
    if owned.is_empty() {
        return ERR_NULL_PTR;
    }
    if !sinks::audio_in::call(owned, sample_rate as u32, channels as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_screenFrameIn(
    env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    width: jint,
    height: jint,
    fmt: jint,
) -> jint {
    if (width <= 0) || (height <= 0) {
        return ERR_INVALID_ARG;
    }
    let owned: Box<[u8]> = match env.convert_byte_array(data) {
        Ok(b) => b.into_boxed_slice(),
        Err(_) => return ERR_NULL_PTR,
    };
    if !sinks::screen::call(owned, width as u32, height as u32) {
        return ERR_NO_CONSUMER;
    }
    let _ = fmt;
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_audioPlayPcm(
    env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    sample_rate: jint,
    channels: jint,
) -> jint {
    if (sample_rate <= 0) || (channels != 1 && channels != 2) {
        return ERR_INVALID_ARG;
    }
    let owned: Box<[u8]> = match env.convert_byte_array(data) {
        Ok(b) => b.into_boxed_slice(),
        Err(_) => return ERR_NULL_PTR,
    };
    if owned.is_empty() {
        return ERR_NULL_PTR;
    }
    if !sinks::audio_out::call(owned, sample_rate as u32, channels as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_permissionChanged(
    _env: JNIEnv,
    _class: JClass,
    perm: jint,
    granted: jint,
) -> jint {
    match perm {
        1 | 2 | 3 => {
            eprintln!("[bridge] permission perm={perm} granted={granted}");
            ERR_OK
        }
        _ => ERR_INVALID_ARG,
    }
}
