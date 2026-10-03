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
