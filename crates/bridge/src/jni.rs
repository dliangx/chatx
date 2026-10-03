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
//!
//!     public static void startScreenShare();  // Java-implemented
//!     public static void stopScreenShare();   // Java-implemented
//! }
//! ```
//!
//! Threading contract (Java → Rust):
//!   - `cameraFrameIn` — Camera2 `ImageReader` executor thread
//!   - `audioPcmIn`    — AudioRecord reader thread
//!   - `screenFrameIn` — MediaProjection `ImageReader` dispatch thread
//!   - `audioPlayPcm`  — playback thread
//!
//! Rust → Java (added for screen-share control):
//!   - `request_screen_share_start()` / `request_screen_share_stop()` call
//!     `NativeBridge.startScreenShare` / `stopScreenShare` as static voids.

use crate::sinks;
use std::sync::OnceLock;

use jni::objects::{JByteArray, JClass};
use jni::sys::{jint, jsize};
use jni::{JavaVM, JNIEnv};

pub const ERR_OK: jint = 0;
pub const ERR_NULL_PTR: jint = -1;
pub const ERR_INVALID_ARG: jint = -2;
pub const ERR_NO_CONSUMER: jint = -3;

/// Cached on the first Java → Rust callback so that *any* subsequent Rust
/// thread (most importantly the Slint UI thread, which has no env handle of
/// its own) can attach to the JVM and call back into Java.
static VM: OnceLock<JavaVM> = OnceLock::new();

/// Capture a global handle on the VM on the first callback. Idempotent.
fn maybe_cache_jvm(env: &mut JNIEnv) {
    if VM.get().is_some() {
        return;
    }
    if let Ok(vm) = env.get_java_vm() {
        let _ = VM.set(vm);
    }
}

/// Attach the calling thread (as daemon — so it auto-detaches on exit and
/// doesn't hold the JVM open) and call `com.chatx.NativeBridge.<method>`
/// as a static void. Returns `true` on success.
fn call_native_bridge_void(method: &str) -> bool {
    let vm = match VM.get() {
        Some(v) => v,
        None => {
            eprintln!("[bridge] {method}: no JVM handle cached yet; skipping");
            return false;
        }
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[bridge] {method}: attach current thread failed: {e}");
            return false;
        }
    };
    match env.call_static_method::<_, _, _>("com.chatx.NativeBridge", method, "()V", &[]) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("[bridge] {method}: Java static call failed: {e}");
            false
        }
    }
}

/// Ask the Java shell to launch the MediaProjection consent dialog + start
/// the screen-capture loop. Safe to call from any Rust thread; no-op if the
/// JVM isn't attached yet.
pub fn request_screen_share_start() {
    call_native_bridge_void("startScreenShare");
}

/// Ask the Java shell to tear down the screen-capture loop and its
/// projection / virtual display.
pub fn request_screen_share_stop() {
    _ = call_native_bridge_void("stopScreenShare");
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_cameraFrameIn(
    mut env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    width: jint,
    height: jint,
    fmt: jint,
) -> jint {
    maybe_cache_jvm(&mut env);
    if (width <= 0) || (height <= 0) {
        return ERR_INVALID_ARG;
    }
    let owned: Box<[u8]> = match env.convert_byte_array(data) {
        Ok(b) => b.into_boxed_slice(),
        Err(_) => return ERR_NULL_PTR,
    };
    if !sinks::camera::call(owned, width as u32, height as u32, fmt as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_audioPcmIn(
    mut env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    sample_rate: jint,
    channels: jint,
) -> jint {
    maybe_cache_jvm(&mut env);
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
    mut env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    width: jint,
    height: jint,
    fmt: jint,
) -> jint {
    maybe_cache_jvm(&mut env);
    if (width <= 0) || (height <= 0) {
        return ERR_INVALID_ARG;
    }
    let owned: Box<[u8]> = match env.convert_byte_array(data) {
        Ok(b) => b.into_boxed_slice(),
        Err(_) => return ERR_NULL_PTR,
    };
    if !sinks::screen::call(owned, width as u32, height as u32, fmt as u32) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_audioPlayPcm(
    mut env: JNIEnv,
    _class: JClass,
    data: JByteArray,
    _len: jsize,
    sample_rate: jint,
    channels: jint,
) -> jint {
    maybe_cache_jvm(&mut env);
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
