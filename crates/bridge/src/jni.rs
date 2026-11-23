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

/// Call a static `void (String)` method on the cached `NativeBridge` class.
/// Returns `true` on success. Used for things like `reveal(String path)`.
fn call_native_bridge_string(method: &str, value: &str) -> bool {
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
    let arg = match env.new_string(value) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[bridge] {method}: new_string failed: {e}");
            return false;
        }
    };
    match env.call_static_method::<_, _, _>(bridged, method, "(Ljava/lang/String;)V", &[(&arg).into()]) {
        Ok(_) => {
            // `startActivity` and `createChooser` on the Java side usually
            // swallow their own throwables (we wrapped them in `try/catch`),
            // but if an exception is still pending here — e.g. the activity
            // was already gone — clear it so a follow-up JNI call on this
            // thread doesn't abort.
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            true
        }
        Err(e) => {
            tracing::warn!("[bridge] {method}: Java static call failed: {e}");
            false
        }
    }
}

/// Call a static `void (String, String)` method on the cached
/// `NativeBridge` class. Returns `true` on success. Used for
/// `notifyMessage(title, body)`.
fn call_native_bridge_two_strings(method: &str, a: &str, b: &str) -> bool {
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
    let sa = match env.new_string(a) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[bridge] {method}: new_string (arg1) failed: {e}");
            return false;
        }
    };
    let sb = match env.new_string(b) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("[bridge] {method}: new_string (arg2) failed: {e}");
            return false;
        }
    };
    match env.call_static_method::<_, _, _>(
        bridged,
        method,
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[(&sa).into(), (&sb).into()],
    ) {
        Ok(_) => {
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            true
        }
        Err(e) => {
            tracing::warn!("[bridge] {method}: Java static call failed: {e}");
            false
        }
    }
}

/// Ask the Java shell to post a system notification with `title` / `body`.
/// The shell wraps it in `NotificationCompat` via `com.chatx.Notify` and
/// fires the POST_NOTIFICATIONS prompt on Android 13+ if needed.
pub fn notify_message(title: &str, body: &str) {
    _ = call_native_bridge_two_strings("notifyMessage", title, body);
}

/// Ask the Java shell to (start | stop) an in-app screen capture.
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

// ── H.264 video decode (MediaCodec) — remote video in a call ──────────────
//
// Rust → Java: `NativeBridge.h264Init() : boolean`,
//   `NativeBridge.h264Feed(byte[]) : boolean`,
//   `NativeBridge.h264Out(byte[] dst, int[] dims) : int`,
//   `NativeBridge.h264Close() : void`.
//
// The Java shell (`com.chatx.MediaCodecDecoder`) owns the `MediaCodec` and a
// YUV→RGBA conversion (OpenGL). Rust (`media::android`) calls these through
// the helpers below — they reuse the cached `NativeBridge` class + JVM, so no
// `FindClass` classloader problems. Each helper returns `false` on any failure
// so the caller can fall back to software decode.

/// Open (or reuse) the MediaCodec decoder in the Java shell. `false` if the
/// JVM isn't attached yet or the call failed (caller then uses software).
pub fn h264_init() -> bool {
    call_static_bool("h264Init", "()Z")
}

/// Feed one Annex-B NAL unit to the MediaCodec decoder. `false` on error.
pub fn h264_feed(nal: &[u8]) -> bool {
    let bridge = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => return false,
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => return false,
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let jarr = match env.new_byte_array(nal.len().try_into().unwrap()) {
        Ok(a) => a,
        Err(_) => return false,
    };
    let jarr_raw = jarr.as_raw();
    let jbytes: Vec<jni::sys::jbyte> = nal.iter().map(|b| *b as jni::sys::jbyte).collect();
    if env.set_byte_array_region(jarr, 0, &jbytes).is_err() {
        return false;
    }
    // `set_byte_array_region` consumed the local-ref handle; re-wrap the same
    // raw array for the method call and drop that extra handle afterwards.
    let jarr2 = unsafe { jni::objects::JByteArray::from_raw(jarr_raw) };
    let ok = match env.call_static_method::<_, _, _>(bridge, "h264Feed", "([B)Z", &[(&jarr2).into()]) {
        Ok(v) => v.z().unwrap_or(false),
        Err(_) => false,
    };
    let _ = env.delete_local_ref(jarr2);
    ok
}

/// Pull the next decoded RGBA8888 frame into `dst` (up to its len). On
/// success `dims[0]` = width, `dims[1]` = height, and the returned `usize` is
/// the frame byte length. `None` if no frame is ready or on error.
pub fn h264_out(dst: &mut [u8], dims: &mut [u32; 2]) -> Option<usize> {
    let bridge = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => return None,
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => return None,
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return None,
    };
    let jdst = match env.new_byte_array(dst.len().try_into().unwrap()) {
        Ok(a) => a,
        Err(_) => return None,
    };
    let jdims = match env.new_int_array(2) {
        Ok(a) => a,
        Err(_) => return None,
    };
    let len = match env.call_static_method::<_, _, _>(
        bridge,
        "h264Out",
        "([B[I)I",
        &[(&jdst).into(), (&jdims).into()],
    ) {
        Ok(v) => v.i().map(|n| n as i32).unwrap_or(-1),
        Err(_) => return None,
    };
    if len <= 0 {
        return None;
    }
    let len = (len as usize).min(dst.len());
    let mut region: Vec<jni::sys::jbyte> = vec![0i8; len];
    if env.get_byte_array_region(jdst, 0, region.as_mut_slice()).is_ok() {
        let bytes: Vec<u8> = region.iter().map(|b| *b as u8).collect();
        dst[..len].copy_from_slice(&bytes);
    }
    let mut dw: [jni::sys::jint; 2] = [0; 2];
    if env.get_int_array_region(jdims, 0, &mut dw).is_ok() {
        dims[0] = dw[0] as u32;
        dims[1] = dw[1] as u32;
    }
    Some(len)
}

/// Release the MediaCodec decoder in the Java shell.
pub fn h264_close() {
    call_static_bool("h264Close", "()Z");
}

// ── H.264 video **encode** (MediaCodec) — local video out in a call ────────
//
// The symmetric encode side of the decoder above. Rust (`media::android`)
// calls these; the Java shell (`com.chatx.MediaCodecEncoder`) owns a
// `MediaCodec` encoder. The encoder consumes RGBA8888 and emits **avcC** NALs
// (config + sample); the shell re-frames them into **Annex-B** (start-code
// prefixed, SPS/PPS leading) so the wire format matches openh264 + the webrtc
// `H264Payloader`. Any helper returns `false`/`None` on failure so the caller
// falls back to software encode.

/// Allocate + start a MediaCodec encoder in the Java shell. Returns the
/// handle (>0) or `0` on failure (no H.264 encoder / JVM not ready — caller
/// then uses software encode).
pub fn h264enc_new() -> i32 {
    let bridge = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => return 0,
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => return 0,
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return 0,
    };
    match env.call_static_method::<_, _, _>(bridge, "h264EncNew", "()I", &[]) {
        Ok(v) => v.i().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Feed one RGBA8888 frame to the MediaCodec encoder (by handle). `false` on error.
pub fn h264enc_feed(handle: i32, rgba: &[u8], w: u32, h: u32) -> bool {
    let bridge = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => return false,
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => return false,
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let jarr = match env.new_byte_array(rgba.len().try_into().unwrap()) {
        Ok(a) => a,
        Err(_) => return false,
    };
    let jarr_raw = jarr.as_raw();
    let jbytes: Vec<jni::sys::jbyte> = rgba.iter().map(|b| *b as jni::sys::jbyte).collect();
    if env.set_byte_array_region(jarr, 0, &jbytes).is_err() {
        return false;
    }
    let jarr2 = unsafe { jni::objects::JByteArray::from_raw(jarr_raw) };
    let args = [
        jni::objects::JValue::Int(handle),
        (&jarr2).into(),
        jni::objects::JValue::Int(w as i32),
        jni::objects::JValue::Int(h as i32),
    ];
    let ok = match env.call_static_method::<_, _, _>(bridge, "h264EncFeed", "(I[BII)Z", &args) {
        Ok(v) => v.z().unwrap_or(false),
        Err(_) => false,
    };
    let _ = env.delete_local_ref(jarr2);
    ok
}

/// Pull the next encoded Annex-B NAL stream (by handle) into `dst` (up to its
/// len). Returns the byte length, or `None` if no frame is ready / on error.
pub fn h264enc_out(handle: i32, dst: &mut [u8]) -> Option<usize> {
    let bridge = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => return None,
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => return None,
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return None,
    };
    let jdst = match env.new_byte_array(dst.len().try_into().unwrap()) {
        Ok(a) => a,
        Err(_) => return None,
    };
    let args = [jni::objects::JValue::Int(handle), (&jdst).into()];
    let len = match env.call_static_method::<_, _, _>(bridge, "h264EncOut", "(I[B)I", &args) {
        Ok(v) => v.i().map(|n| n as i32).unwrap_or(-1),
        Err(_) => return None,
    };
    if len <= 0 {
        return None;
    }
    let len = (len as usize).min(dst.len());
    let mut region: Vec<jni::sys::jbyte> = vec![0i8; len];
    if env.get_byte_array_region(jdst, 0, region.as_mut_slice()).is_ok() {
        let bytes: Vec<u8> = region.iter().map(|b| *b as u8).collect();
        dst[..len].copy_from_slice(&bytes);
    }
    Some(len)
}

/// Release the MediaCodec encoder (by handle) in the Java shell.
pub fn h264enc_close(handle: i32) {
    let bridge = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => return,
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => return,
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return,
    };
    let _ = env.call_static_method::<_, _, _>(bridge, "h264EncClose", "(I)V", &[jni::objects::JValue::Int(handle)]);
}

/// Call a static `boolean()` method on the cached `NativeBridge` class.
fn call_static_bool(method: &str, _sig: &str) -> bool {
    let bridged = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => return false,
    };
    let vm = match VM.get() {
        Some(v) => v,
        None => return false,
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return false,
    };
    match env.call_static_method::<_, _, _>(bridged, method, "()Z", &[]) {
        Ok(v) => v.z().unwrap_or(false),
        Err(_) => false,
    }
}

/// Ask the Java shell to open the system photo picker
/// (`Intent.ACTION_PICK` with `image/*`). On success the shell copies the chosen
/// image(s) to the app's private dir and calls back into Rust via
/// [`Java_com_chatx_NativeBridge_photosPicked`] with the newline-joined
/// absolute paths ("`"` on cancel).
///
/// No-op if there is no live activity.
pub fn request_photos_pick(max: u32) {
    _ = call_native_bridge_int("openPhotosPicker", max as i32);
}

/// Ask the Java shell to reveal an absolute-paths file in the system file
/// picker / external-app chooser. No-op if not a valid path or the JVM is
/// not attached.
pub fn reveal_file(path: &str) -> bool {
    call_native_bridge_string("reveal", path)
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

// ── LLM streaming (mobile AI-assistant copilot) ───────────────────────────
//
// The Kotlin shell runs the local model with a platform-native engine
// (MLC-LLM / MediaPipe LLM / llama.cpp-NDK) and streams the assistant's
// reply back here, two calls apart:
//
//   `NativeBridge.llmToken(String chunk) : int`  — one text fragment
//   `NativeBridge.llmDone(int error) : int`       — once at end-of-turn
//                                                   (error 0 = success)
//
// Both route to the `sinks::llm` slot the Rust consumer installs via
// `bridge::set_llm_token_consumer` / `set_llm_done_consumer`.

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_llmToken(
    mut env: JNIEnv,
    cls: JClass,
    text: JString,
) -> jint {
    setup_bridge(&mut env, &cls);
    let owned: String = if text.as_raw().is_null() {
        return ERR_NULL_PTR;
    } else if let Ok(js) = env.get_string(&text) {
        js.into()
    } else {
        return ERR_NULL_PTR;
    };
    if !sinks::llm::deliver_token(owned) {
        return ERR_NO_CONSUMER;
    }
    ERR_OK
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatx_NativeBridge_llmDone(
    mut env: JNIEnv,
    cls: JClass,
    error: jint,
) -> jint {
    setup_bridge(&mut env, &cls);
    if !sinks::llm::deliver_done(error) {
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
    let bridge = match BRIDGE_CLASS.get() {
        Some(c) => c,
        None => {
            tracing::warn!("[x-bridge] read_asset: NativeBridge class not yet cached — bailing");
            return None;
        }
    };
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
        bridge,
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
