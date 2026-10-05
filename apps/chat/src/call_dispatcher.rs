//! Cross-thread camera / screen dispatcher.
//!
//! The platform capture sinks (`camera::Camera` / `screen::Screen`) are
//! global singletons that run on their own callback threads — not the Slint
//! UI thread. The dispatcher here:
//!
//! 1. Forwards every frame to the active call's `VidSource` (for the
//!    currently-active `camera` or `screen` call) — that's how local camera /
//!    screen frames get to webrtc.
//! 2. Runs a hook (installed by the QR scanner) so the scan preview keeps
//!    working in parallel with an active call.
//!
//! All state is process-shared (`Arc<Mutex<...>>`) because it crosses
//! threads. No `RefCell` / `thread_local` here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// ── active call's video sources (set by `call::start_*`, cleared by `stop`) ─

static CALL_CAM: OnceLock<Mutex<Option<Arc<media::VidSource>>>> = OnceLock::new();
static CALL_SCR: OnceLock<Mutex<Option<Arc<media::VidSource>>>> = OnceLock::new();
/// Local camera preview sink (UI-side). Runs in parallel with the
/// webrtc `VidSource` consumer — the UI wants a fresh frame per camera tick
/// so it can show a picture-in-picture preview of "me".
static CALL_CAM_PREVIEW: OnceLock<Mutex<
    Option<Box<dyn Fn(&[u8], u32, u32) + Send + Sync + 'static>>,
>> = OnceLock::new();
/// Throttle so the preview callback isn't flooded (webrtc encoder caps at
/// ~60fps; the UI preview doesn't need more than ~15fps).
static PREVIEW_LAST_MS: AtomicU64 = AtomicU64::new(0);

fn call_cam_slot() -> &'static Mutex<Option<Arc<media::VidSource>>> {
    CALL_CAM.get_or_init(|| Mutex::new(None))
}
fn call_scr_slot() -> &'static Mutex<Option<Arc<media::VidSource>>> {
    CALL_SCR.get_or_init(|| Mutex::new(None))
}
fn call_cam_preview_slot() -> &'static Mutex<
    Option<Box<dyn Fn(&[u8], u32, u32) + Send + Sync + 'static>>,
> {
    CALL_CAM_PREVIEW.get_or_init(|| Mutex::new(None))
}

/// Install the currently-active camera-side `VidSource` (or `None` on teardown).
pub fn set_cam_source(src: Option<Arc<media::VidSource>>) {
    *call_cam_slot().lock().unwrap() = src;
}
pub fn set_scr_source(src: Option<Arc<media::VidSource>>) {
    *call_scr_slot().lock().unwrap() = src;
}

/// Active call's `AudSource`. Registered when a call starts (see
/// `call::bootstrap_media`); mobile mic frames (`bridge::set_audio_consumer`
/// → `bridge_audio_pcm_in`) flow through [`on_audio_frame`] into the
/// currently-active `AudSource`. Desktop mic also registers to this slot
/// (cpal callback → `audio::set_input_sink` → this same `AudSource`
/// handle), so the two paths converge.
static CALL_AUD: OnceLock<Mutex<Option<Arc<media::AudSource>>>> = OnceLock::new();

fn call_aud_slot() -> &'static Mutex<Option<Arc<media::AudSource>>> {
    CALL_AUD.get_or_init(|| Mutex::new(None))
}
pub fn set_aud_source(src: Option<Arc<media::AudSource>>) {
    *call_aud_slot().lock().unwrap() = src;
}

/// Push a local-mic s16le frame to the active call's `AudSource`. No-op
/// if no active call (bridge shell may still be streaming while we're idle).
pub fn on_audio_frame(s16le: &[u8], rate: u32) {
    if s16le.is_empty() || rate == 0 {
        return;
    }
    if let Some(src) = call_aud_slot().lock().unwrap().clone() {
        src.push(s16le, rate);
    }
}
/// Install / clear the local camera preview sink (runs on the camera thread).
pub fn set_cam_preview(cb: Option<Box<dyn Fn(&[u8], u32, u32) + Send + Sync + 'static>>) {
    *call_cam_preview_slot().lock().unwrap() = cb;
}
pub fn teardown_sources() {
    *call_cam_slot().lock().unwrap() = None;
    *call_scr_slot().lock().unwrap() = None;
    *call_cam_preview_slot().lock().unwrap() = None;
    *call_aud_slot().lock().unwrap() = None;
}

fn frame_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── QR hook (parallel consumer of camera frames) ─────────────────────────

type QrHook = Mutex<Option<Box<dyn Fn(&[u8], u32, u32, u32) + Send + Sync + 'static>>>;
static QR_HOOK: OnceLock<QrHook> = OnceLock::new();

fn qr_hook() -> &'static QrHook {
    QR_HOOK.get_or_init(|| Mutex::new(None))
}

pub fn install_qr_hook(cb: impl Fn(&[u8], u32, u32, u32) + Send + Sync + 'static) {
    *qr_hook().lock().unwrap() = Some(Box::new(cb));
}
pub fn clear_qr_hook() {
    *qr_hook().lock().unwrap() = None;
}

// ── frame sinks (called by the platform capture crates) ──────────────────

/// Installed as [`camera::Camera::set_frame_sink`] (or the Apple/Android/JNI
/// equivalent). `fmt` follows `bridge::types::PixelFormat`:
///   - 1 = RGBA8888 (4 B/px)
///   - 8 = Gray8 (1 B/px)
pub fn on_camera_frame(bytes: &[u8], w: u32, h: u32, fmt: u32) {
    if w == 0 || h == 0 {
        return;
    }
    // 1) forward to the active call's `VidSource` (if any).
    let want_webrtc = call_cam_slot().lock().unwrap().is_some();

    // Convert once if either consumer wants RGBA.
    if want_webrtc || call_cam_preview_slot().lock().unwrap().is_some() {
        let rgba = to_rgba8(bytes, w, h, fmt);

        if want_webrtc {
            if let Some(src) = call_cam_slot().lock().unwrap().clone() {
                src.push(&rgba, w, h);
            }
        }

        // 2) UI preview sink (if any) — throttled to ~15fps. The closure only
        // schedules async work on the UI thread, so calling it under the lock
        // is fine (no re-lock inside).
        let now = frame_ms();
        let last = PREVIEW_LAST_MS.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 66 {
            PREVIEW_LAST_MS.store(now, Ordering::Relaxed);
            let mut g = call_cam_preview_slot().lock().unwrap();
            if let Some(cb) = g.as_mut() {
                cb(&rgba, w, h);
            }
        }
    }

    // 3) forward to the QR hook (if any). Raw bytes + fmt.
    if let Some(cb) = qr_hook().lock().unwrap().as_ref() {
        cb(bytes, w, h, fmt);
    }
}

/// Installed as the bridge `set_screen_consumer` (iOS/Android). `fmt` is a
/// `bridge::types::PixelFormat` value; converted to RGBA8888 before push.
pub fn on_screen_frame(bytes: Vec<u8>, w: u32, h: u32, fmt: u32) {
    if w == 0 || h == 0 {
        return;
    }
    let rgba = to_rgba8(&bytes, w, h, fmt);
    if let Some(src) = call_scr_slot().lock().unwrap().clone() {
        src.push(&rgba, w, h);
    }
}

/// Desktop screen-capture (`screen::Screen`) sink already delivers RGBA8888.
pub fn on_screen_frame_rgba(bytes: Vec<u8>, w: u32, h: u32) {
    on_screen_frame(bytes, w, h, /*RGBA8888*/ 1);
}

// ── format conversion ─────────────────────────────────────────────────────
// `fmt` is one of `bridge::types::PixelFormat` (see that enum for the full
// list). The bridges / capture crates claim to deliver RGBA8888 (1) or Gray8
// (8) in the common paths, but the other values are reachable too (BGRA on
// some shells, RGB565 legacy Android, NV12/I420 as direct passthrough), so
// we convert each explicitly instead of assuming.

fn to_rgba8(bytes: &[u8], w: u32, h: u32, fmt: u32) -> Vec<u8> {
    let n = (w as usize).saturating_mul(h as usize);
    let mut out = vec![0u8; n * 4];
    match fmt {
        // RGBA8888 — already in slint's preferred order.
        1 => {
            let l = (n * 4).min(bytes.len()).min(out.len());
            out[..l].copy_from_slice(&bytes[..l]);
        }
        // RGB888 — 3 B/px, expand by appending alpha.
        2 => {
            let src = n.saturating_mul(3);
            let l = src.min(bytes.len()).min(out.len() / 4 * 3);
            let mut di = 0usize;
            while di < l {
                out[di] = bytes[di];
                out[di + 1] = bytes[di + 1];
                out[di + 2] = bytes[di + 2];
                out[di + 3] = 255;
                di += 4;
            }
        }
        // BGRA8888 — swap R and B.
        7 => {
            let l = (n * 4).min(bytes.len()).min(out.len());
            let mut i = 0usize;
            while i + 3 < l {
                out[i] = bytes[i + 2];
                out[i + 1] = bytes[i + 1];
                out[i + 2] = bytes[i];
                out[i + 3] = bytes[i + 3];
                i += 4;
            }
        }
        // Gray8 — expand Y → R=G=B.
        8 => {
            let mut di = 0usize;
            for &y in bytes.iter().take(n) {
                if di + 4 > out.len() {
                    break;
                }
                out[di] = y;
                out[di + 1] = y;
                out[di + 2] = y;
                out[di + 3] = 255;
                di += 4;
            }
        }
        // RGB565 — 16-bit packed 5-6-5, little-endian u16.
        9 => {
            let mut di = 0usize;
            let mut si = 0usize;
            while si + 1 < bytes.len() && di + 4 <= out.len() {
                let v = u16::from_le_bytes([bytes[si], bytes[si + 1]]);
                let r5 = ((v >> 11) & 0x1f) as u32;
                let g6 = ((v >> 5) & 0x3f) as u32;
                let b5 = (v & 0x1f) as u32;
                out[di] = ((r5 << 3) | (r5 >> 2)) as u8;
                out[di + 1] = ((g6 << 2) | (g6 >> 4)) as u8;
                out[di + 2] = ((b5 << 3) | (b5 >> 2)) as u8;
                out[di + 3] = 255;
                di += 4;
                si += 2;
            }
        }
        // YUV (NV12/NV21/I420/YV12, 3..6) and unknown — the bridges claim to
        // convert away from these before calling in. Best-effort: treat as
        // raw RGBA to avoid dropping the frame entirely; log so a misreport is
        // surfaced.
        f => {
            eprintln!("[dispatcher] camera frame with unexpected fmt={f} ({w}x{h}) — copying raw");
            let l = (n * 4).min(bytes.len()).min(out.len());
            out[..l].copy_from_slice(&bytes[..l]);
        }
    }
    out
}

/// Horizontal mirror of an RGBA8888 buffer (for the local camera PIP so the
/// preview reads as a "mirror" rather than a fixed video feed).
pub fn flip_h(rgba: &mut [u8], w: u32) {
    if w == 0 || rgba.len() < (w as usize) * 4 {
        return;
    }
    let h = (rgba.len() / 4) / (w as usize);
    let row = w as usize * 4;
    for y in 0..h {
        let base = y * row;
        let mut x = 0usize;
        let mut r = (w as usize) - 1;
        while x < r {
            for k in 0..4 {
                let a = base + x * 4 + k;
                let b = base + r * 4 + k;
                rgba.swap(a, b);
            }
            x += 1;
            r -= 1;
        }
    }
}
