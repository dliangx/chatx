use std::path::Path;
use std::sync::{LazyLock, Mutex};

use pdf::{Frame, Mode, Step, Viewer};

/// The one live native PDF viewer, if any.
static VIEWER: LazyLock<Mutex<Option<Viewer>>> = LazyLock::new(|| Mutex::new(None));

/// True while a viewer is live.
pub fn is_open() -> bool {
    VIEWER.lock().unwrap().is_some()
}

/// Open the PDF at `path`, sized for a `width`×`height` logical canvas at a
/// device-pixel-ratio of `dpr`. Re-opening before the old one is closed keeps
/// the live instance (an overlay re-enter is a no-op for the instance).
pub fn open_pdf(path: &str, width: u32, height: u32, dpr: f32) -> Result<(), String> {
    if is_open() {
        return Ok(());
    }
    let v = Viewer::open(Path::new(path), width, height, dpr).map_err(|e| e.to_string())?;
    *VIEWER.lock().unwrap() = Some(v);
    tracing::info!("[pdf] opened {} ({}×{} @ dpr×{dpr})", path, width, height);
    Ok(())
}

// ── input forwarding (logical px → device px happens in the caller) ────────

/// Forward a touch event. `kind`: 1 = down, 2 = move, 3 = up.
pub fn touch(kind: i32, x: i32, y: i32, dpr_x10: u32) {
    if let Some(v) = VIEWER.lock().unwrap().as_ref() {
        let sx = (dpr_x10 as f32 / 10.0).max(1.0);
        let dx = (x as f32 * sx) as i32;
        let dy = (y as f32 * sx) as i32;
        match kind {
            1 => v.touch_down(dx, dy),
            2 => v.touch_move(dx, dy),
            3 => v.touch_up(dx, dy),
            _ => {}
        }
    }
}

/// Forward a wheel / trackpad scroll delta (logical px).
pub fn scroll(dy: f32, dpr_x10: u32) {
    if let Some(v) = VIEWER.lock().unwrap().as_ref() {
        let sx = (dpr_x10 as f32 / 10.0).max(1.0);
        v.scroll((dy * sx) as i32);
    }
}

/// Switch the rendered view mode (`code`: [`pdf::VIEW_MODE_WIDTH`] /
/// [`pdf::VIEW_MODE_HEIGHT`]).
pub fn set_mode(code: i32) {
    if let Some(m) = Mode::from_code(code) {
        if let Some(v) = VIEWER.lock().unwrap().as_ref() {
            v.set_mode(m);
        }
    }
}

/// The window was resized to `w`×`h` device px; re-render the canvas.
pub fn resize(w: u32, h: u32) {
    if let Some(v) = VIEWER.lock().unwrap().as_ref() {
        v.resize(w, h);
    }
}

/// Shut the viewer down (idempotent) and drop the instance — its parsed
/// document and the byte-budgeted render cache go with it. The UI releases its
/// one committed frame image separately (`PdfrsHost.frame → default`).
pub fn close() {
    VIEWER.lock().unwrap().take();
}

// ── pump → the committed canvas frame ───────────────────────────────────────

/// One committed frame plus the view-state that belongs to it (the toolbar's
/// page counter / mode). The image is the **full canvas at device px** — the
/// app shows it scaled to the window (1:1 physical pixels).
pub struct Commit {
    pub img: slint::Image,
    pub page: i32,
    pub page_count: i32,
    pub mode: i32,
    /// Canvas size in **device px** (diagnostics; the UI fills its window).
    pub canvas_w: u32,
    pub canvas_h: u32,
}

/// The outcome of one `pump-pdf` tick.
pub enum Tick {
    /// Nothing visible changed — keep the image on screen (no re-upload).
    Unchanged,
    /// A new committed frame to show.
    Commit(Commit),
    /// A fatal viewer state (e.g. the document has no renderable pages):
    /// show the solid panel and surface the message.
    Fatal { img: slint::Image, message: String },
}

/// Drive one step: drain input, pre-warm + render the visible neighborhood and
/// compose the frame — only when the visible state actually changed.
/// `None` when no viewer is live.
pub fn tick() -> Option<Tick> {
    let guard = VIEWER.lock().unwrap();
    let v = guard.as_ref()?;
    match v.step() {
        Step::Unchanged => Some(Tick::Unchanged),
        Step::Frame(f) => {
            let vs = v.view_state();
            let (cw, ch) = v.canvas_size();
            Some(Tick::Commit(Commit {
                img: frame_image(&f),
                page: vs.page,
                page_count: vs.page_count,
                mode: vs.mode,
                canvas_w: cw,
                canvas_h: ch,
            }))
        }
        Step::Fatal { frame, message } => Some(Tick::Fatal {
            img: frame_image(&frame),
            message,
        }),
    }
}

/// Convert the composed canvas frame into a Slint `image` property value
/// (one copy out of the viewer's frame buffer into Slint's shared pixel
/// buffer; the viewer's `Frame` is then dropped).
pub fn frame_image(f: &Frame) -> slint::Image {
    if f.width == 0 || f.height == 0 || f.rgba.is_empty() {
        return slint::Image::default();
    }
    let n = (f.width as usize).saturating_mul(f.height as usize);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(f.width, f.height);
    let dst = buf.make_mut_bytes();
    let l = (n * 4).min(f.rgba.len()).min(dst.len());
    dst[..l].copy_from_slice(&f.rgba[..l]);
    slint::Image::from_rgba8(buf)
}
