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
//! Rust → Java (added for screen-share + mic control):
//!   - `request_screen_share_start()` / `request_screen_share_stop()` call
//!     `NativeBridge.startScreenShare` / `stopScreenShare` as static voids.
//!   - `request_mic_start()` / `request_mic_stop()` call
//!     `NativeBridge.startMicCapture` / `stopMicCapture` as static voids (the
//!     mic only runs while a call is active, saving battery when idle).

use crate::sinks;
use std::sync::OnceLock;

use jni::objects::{JByteArray, JClass, JString, GlobalRef};
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

/// Global reference to the `com.chatx.NativeBridge` class. Captured on the
/// first Java → Rust JNI call by promoting the in-scope `jclass` to a global
/// ref via `new_global_ref`.
///
/// **Why this exists:** `JNIEnv::call_static_method("com/chatx/NativeBridge")`
/// goes through `FindClass`, which in jni 0.21 uses the **boot classloader**
/// (the one attached to the calling thread) — but `com.chatx.NativeBridge`
/// lives in the APK's own `classes.dex`, only visible to the **app
/// classloader**. Calling it this way produces
/// `java.lang.ClassNotFoundException: com.chatx.NativeBridge` even when the
/// class is perfectly loaded. Caching the class object we already received
/// from Java sidesteps `FindClass` entirely.
static BRIDGE_CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// Capture a global handle on the VM on the first callback. Idempotent.
fn maybe_cache_jvm(env: &mut JNIEnv) {
    if VM.get().is_some() {
        return;
    }
    if let Ok(vm) = env.get_java_vm() {
        let _ = VM.set(vm);
    }
}

/// Remember the `com.chatx.NativeBridge` class for later Rust→Java static
/// calls. Called at the top of every Java→Rust JNI entry point so the first
/// one in (even if it's `permissionChanged` before any of our features)
/// seeds the slot. Idempotent.
fn cache_bridge_class(env: &mut JNIEnv, cls: &JClass) {
    if BRIDGE_CLASS.get().is_some() {
        return;
    }
    // Wrap the existing `jclass` pointer in the `JObject<'static>` shape that
    // `new_global_ref` accepts, then promote it to a global ref. This is
    // safe because (a) the pointer is a valid JNI object reference and
    // (b) the JNI spec permits calling `NewGlobalRef` on any valid object
    // reference (including class objects).
    let obj = unsafe { jni::objects::JObject::from_raw(cls.as_raw() as *mut _) };
    if let Ok(gr) = env.new_global_ref(obj) {
        let _ = BRIDGE_CLASS.set(gr);
    }
}

/// Combined setup — cache both the VM and the `NativeBridge` class. Called
/// from every Java→Rust JNI entry point so any subsequent Rust→Java static
/// call (e.g. `request_file_pick`) can attach to the JVM and invoke methods
/// on the class. Idempotent.
fn setup_bridge(env: &mut JNIEnv, cls: &JClass) {
    maybe_cache_jvm(env);
    cache_bridge_class(env, cls);
}

/// Attach the calling thread (as daemon — so it auto-detaches on exit and
/// doesn't hold the JVM open) and call `com.chatx.NativeBridge.<method>`
/// as a static void against the cached class object. Returns `true` on
/// success.
fn call_native_bridge_void(method: &str) -> bool {
    let bridged = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => {
            tracing::warn!("[bridge] {method}: NativeBridge class not yet cached — bailing");
            return false;
        }
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => {
            tracing::warn!("[bridge] {method}: no JVM handle cached yet; skipping");
            return false;
        }
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("[bridge] {method}: attach current thread failed: {e}");
            return false;
        }
    };
    match env.call_static_method::<_, _, _>(bridged, method, "()V", &[]) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("[bridge] {method}: Java static call failed: {e}");
            false
        }
    }
}

/// As [`call_native_bridge_void`] but with a single `int` parameter
/// (signature `(I)V`).
fn call_native_bridge_int(method: &str, value: i32) -> bool {
    let bridged = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => {
            tracing::warn!("[bridge] {method}: NativeBridge class not yet cached — bailing");
            return false;
        }
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => {
            tracing::warn!("[bridge] {method}: no JVM handle cached yet; skipping");
            return false;
        }
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("[bridge] {method}: attach current thread failed: {e}");
            return false;
        }
    };
    let args = [jni::objects::JValue::Int(value)];
    match env.call_static_method::<_, _, _>(bridged, method, "(I)V", &args) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("[bridge] {method}: Java static call failed: {e}");
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

/// Ask the Java shell to start the mic capture loop (`Capture.MicLoop`,
/// AudioRecord 16 kHz mono). Safe to call from any Rust thread; the loop is
/// spawned on its own `HandlerThread` on the Java side. No-op if the JVM is
/// not yet attached, if the shell is idle, or if the mic is already running
/// (idempotent). Pair with [`request_mic_stop`].
pub fn request_mic_start() {
    call_native_bridge_void("startMicCapture");
}

/// Ask the Java shell to stop the mic capture loop and release its
/// `AudioRecord`. No-op if not running.
pub fn request_mic_stop() {
    _ = call_native_bridge_void("stopMicCapture");
}

/// Ask the Java shell to open the system document picker (SAF
/// `Intent.ACTION_OPEN_DOCUMENT`). On success the shell copies the chosen
/// file to the app's private dir and calls back into Rust via
/// [`Java_com_chatx_NativeBridge_filePickDone`]. On cancel the shell calls
/// [`Java_com_chatx_NativeBridge_filePickDone`] with an empty string.
///
/// No-op if there is no live activity.
pub fn request_file_pick() {
    _ = call_native_bridge_void("openFilePicker");
}

/// Ask the Java shell to open the system photo picker
/// (`ACTION_PICK` with `image/*`). On success the shell copies the chosen
/// image(s) to the app's private dir and calls back into Rust via
/// [`Java_com_chatx_NativeBridge_photosPicked`] with the newline-joined
/// absolute paths ("`"` on cancel).
///
/// No-op if there is no live activity.
pub fn request_photos_pick(max: u32) {
    _ = call_native_bridge_int("openPhotosPicker", max as i32);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_photosPicked(
    mut env: JNIEnv,
    cls: JClass,
    paths: JString,
) -> jint {
    setup_bridge(&mut env, &cls);
    let out: Vec<String> = if paths.as_raw().is_null() {
        Vec::new()
    } else if let Ok(js) = env.get_string(&paths) {
        let s: String = js.into();
        s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    } else {
        Vec::new()
    };
    if !sinks::photos::deliver(out) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_cameraFrameIn(
    mut env: JNIEnv,
    cls: JClass,
    data: JByteArray,
    _len: jsize,
    width: jint,
    height: jint,
    fmt: jint,
) -> jint {
    setup_bridge(&mut env, &cls);
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
    cls: JClass,
    data: JByteArray,
    _len: jsize,
    sample_rate: jint,
    channels: jint,
) -> jint {
    setup_bridge(&mut env, &cls);
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
    cls: JClass,
    data: JByteArray,
    _len: jsize,
    width: jint,
    height: jint,
    fmt: jint,
) -> jint {
    setup_bridge(&mut env, &cls);
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
    cls: JClass,
    data: JByteArray,
    _len: jsize,
    sample_rate: jint,
    channels: jint,
) -> jint {
    setup_bridge(&mut env, &cls);
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
    mut env: JNIEnv,
    cls: JClass,
    perm: jint,
    granted: jint,
) -> jint {
    setup_bridge(&mut env, &cls);
    match perm {
        1 | 2 | 3 => {
            tracing::debug!("[bridge] permission perm={perm} granted={granted}");
            ERR_OK
        }
        _ => ERR_INVALID_ARG,
    }
}

/// Java shell → Rust: notification that the system document picker has
/// completed. `NativeBridge.filePickDone(String path)` is called on the main
/// thread after the SAF flow finishes, with the absolute path of the
/// copied file (or "" on cancel / no pick). Delivers the result to the
/// installed `sinks::filepick` consumer (typically `camera::filepick`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_filePickDone(
    mut env: JNIEnv,
    cls: JClass,
    path: JString,
) -> jint {
    setup_bridge(&mut env, &cls);
    let path_owned: Option<String> = if path.as_raw().is_null() {
        None
    } else if let Ok(js) = env.get_string(&path) {
        let s: String = js.into();
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        None
    };
    if !sinks::filepick::deliver(path_owned) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

// ── Asset reading (X mini-app plugins packed in assets/) ──────────────────
//
// Rust → Java: `NativeBridge.extractAsset(String src, String dst) : void`
//
// The Java shell opens the named asset and writes its bytes to `dst` on the
// regular filesystem. Rust then reads it with `std::fs::read` (same call
// the desktop / iOS paths use). This is the simplest correct cross-object
// marshalling path and avoids jni-crate `JValueGen → JByteArray` conversions.
//
// Single well-known temp file: the chat UI invokes `read_asset` once per
// X extension open, so parallelism is not a real concern here.

pub(crate) fn read_asset_jni(path: &str) -> Option<Vec<u8>> {
    let vm = match VM.get() {
        Some(v) => v,
        None => {
            tracing::warn!("[x-bridge] read_asset: no VM cached (no Java→Rust call happened yet)");
            return None;
        }
    };
    let dst = std::env::temp_dir().join("chatx_x_asset");
    let dst_str = dst.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&dst);
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("[x-bridge] read_asset: attach failed: {e}");
            return None;
        }
    };
    let jsrc = match env.new_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[x-bridge] read_asset: new_string(src) failed: {e}");
            return None;
        }
    };
    let jdst = match env.new_string(&dst_str) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[x-bridge] read_asset: new_string(dst) failed: {e}");
            return None;
        }
    };
    match env.call_static_method::<_, _, _>(
        "com/chatx/NativeBridge",
        "extractAsset",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[(&jsrc).into(), (&jdst).into()],
    ) {
        Ok(_) => tracing::info!("[x-bridge] extractAsset call returned ok"),
        Err(e) => {
            tracing::warn!("[x-bridge] read_asset: extractAsset call failed: {e}");
            return None;
        }
    };
    // Check for a pending Java exception (activity null, asset not openable, or
    // Java-side throw) — `call_static_method` may surface it as Err but we
    // clear it here so a follow-up JNI call on this thread doesn't misbehave.
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
        tracing::warn!("[x-bridge] extractAsset left a pending Java exception — most likely Shell.activity() was null or assets.open() threw; check logcat for the Java stack");
        return None;
    }
    match std::fs::read(&dst) {
        Ok(b) => {
            let _ = std::fs::remove_file(&dst);
            tracing::info!("[x-bridge] read_asset: got {} bytes from {dst_str}", b.len());
            Some(b)
        }
        Err(e) => {
            tracing::warn!("[x-bridge] read_asset: read {dst_str} failed: {e} (asset likely missing or activity null)");
            None
        }
    }
}
