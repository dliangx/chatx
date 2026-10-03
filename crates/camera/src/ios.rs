//! iOS camera permission helpers.
//!
//! On iOS the camera permission dialog is shown **automatically** by the
//! system when the app first creates an `AVCaptureDeviceInput` while the
//! status is `NotDetermined` (see Apple's `requestAccessForMediaType:
//! completionHandler:` docs). So `Camera::start()` (the shared Apple
//! implementation in `macos.rs`) already handles the prompt — no custom block
//! plumbing is required here.
//!
//! What this module provides is a non-prompting status query so the UI can
//! surface a helpful message (e.g. "camera denied → open Settings") instead of
//! a raw `start()` error.

use objc2_av_foundation::{AVMediaTypeVideo, AVCaptureDevice, AVAuthorizationStatus};

/// Current camera authorization state (queried without prompting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraAuth {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

impl From<AVAuthorizationStatus> for CameraAuth {
    fn from(s: AVAuthorizationStatus) -> Self {
        if s == AVAuthorizationStatus::Authorized {
            CameraAuth::Authorized
        } else if s == AVAuthorizationStatus::Denied {
            CameraAuth::Denied
        } else if s == AVAuthorizationStatus::Restricted {
            CameraAuth::Restricted
        } else {
            CameraAuth::NotDetermined
        }
    }
}

/// Query the current camera authorization without triggering the system
/// prompt. The prompt itself is triggered (as needed) by `Camera::start()`.
pub fn authorization_status() -> CameraAuth {
    let video = match unsafe { AVMediaTypeVideo } {
        Some(v) => v,
        None => return CameraAuth::Denied,
    };
    CameraAuth::from(unsafe { AVCaptureDevice::authorizationStatusForMediaType(video) })
}
