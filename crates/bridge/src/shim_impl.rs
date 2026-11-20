//! iOS shim bindings (RPScreenRecorder-based, compiled from ObjC into the
//! same final .app executable — see `apps/platform/ios/build_for_ios_with_cargo.bash`).
//!
//! The ObjC shim (`apps/platform/ios/Sources/ChatxScreenCapture.m`) provides
//! `chatx_screen_capture_{start,stop}`. The build script compiles it and
//! force-links the resulting `.o` into the final binary; this module declares
//! the externs and calls them.

/// Ask the iOS shell to bring up RPScreenRecorder and start the capture.
pub fn request_screen_share_start() {
    unsafe extern "C" {
        fn chatx_screen_capture_start();
    }
    unsafe {
        chatx_screen_capture_start();
    }
}

/// Stop the active iOS capture and clean up the recorder.
pub fn request_screen_share_stop() {
    unsafe extern "C" {
        fn chatx_screen_capture_stop();
    }
    unsafe {
        chatx_screen_capture_stop();
    }
}

/// Ask the iOS shell to post a system notification with `title` / `body`.
/// The shell wraps it in `UNUserNotificationCenter` (see
/// `apps/platform/ios/Sources/ChatxNotify.m`).
pub fn notify_message(title: &str, body: &str) {
    unsafe extern "C" {
        fn chatx_notify_show(title: *const i8, body: *const i8);
    }
    unsafe {
        // Convert Rust `&str` (UTF-8) to a NUL-terminated C string for FFI.
        let mut t = title.as_bytes().to_vec();
        let mut b = body.as_bytes().to_vec();
        t.push(0);
        b.push(0);
        chatx_notify_show(t.as_ptr() as *const i8, b.as_ptr() as *const i8);
    }
}
