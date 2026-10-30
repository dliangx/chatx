//! `x.pdfview` — P1 PDF viewer mini-program, built on **pdf-rs**.
//!
//! Rendering: `pdf-render` (pure-Rust `vello_cpu` rasterizer) over
//! `pdf-interpret` + `pdf-syntax`. Fonts come from the document itself
//! (pdf-interpret reads embedded / subset fonts; the `embed-fonts` feature
//! supplies the 14 base-14 substitutes as a fallback — no host font channel
//! needed). Parsing is `pdf_syntax::Pdf::new(bytes)`.
//!
//! **Two view modes only** (per spec):
//!   * **Width**  (宽度适配) — each page scaled to fill the canvas width; a
//!     vertical strip you scrub through.
//!   * **Height** (高度适配) — each page scaled to fill the canvas height; one
//!     page per screen.
//!
//! Gesture model (every value below is **device px**; the host already scales
//! input to device px before forwarding it):
//!   * vertical drag      = scroll (both modes); drag + release in Height mode
//!                          settles on the nearest full page
//!   * wheel / trackpad   = scroll — the app UI's TouchArea `scroll-event`
//!                          sends `EV_SCROLL` carrying the vertical delta
//!   * host mode command  = switch view mode — the app UI's Fit-Width /
//!                          Fit-Height icons send `EV_KEY_DOWN` carrying a
//!                          `VIEW_MODE_*` code, which this plugin honours
//!   * tap left half      = previous page
//!   * tap right half     = next page
//!   * `EV_RESIZE`        = host window drag-resize — `x`/`y` are the new
//!                          canvas size in **device px** (host-scaled, like
//!                          touches); the work canvas is resized and the
//!                          visible pages re-rendered so the frame follows
//!                          the window aspect (no `contain` letterbox)
//!
//! The **toolbar** (the two mode icons + the `n/m` page counter) is owned by
//! the app UI, not by this plugin. The plugin renders pages across the whole
//! canvas and reports its active page / count / mode via
//! `appsdk::report_view`, so the UI can mirror them.
//!
//! **Sharpness:** the host hands us a *logical* canvas size plus the dpr and
//! forwards *device-px* touches. We build the canvas at **device** resolution
//! (`size × dpr`), render every page at that device resolution, and blit 1:1 —
//! so one canvas pixel lands on one physical pixel (no upscaling blur).

use std::cell::RefCell;

use appsdk::{
    begin, poll_input, report_view, rgba, with_ctx, EV_KEY_DOWN, EV_RESIZE, EV_SCROLL,
    EV_TOUCH_DOWN, EV_TOUCH_MOVE, EV_TOUCH_UP, VIEW_MODE_HEIGHT, VIEW_MODE_WIDTH,
};
use pdf_interpret::InterpreterSettings;
use pdf_render::pdf_syntax::Pdf;
use pdf_render::{render as render_page, RenderSettings};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// The two supported view modes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Width,
    Height,
}

/// A rendered, baked-over-white, fully-*opaque* RGBA8 page raster (a cache slot).
struct Rast {
    page: u32,
    w: u32,
    h: u32,
    px: Vec<u8>,
}

struct App {
    /// The parsed document; `None` when init hit a fatal condition.
    doc: Option<Pdf>,
    /// Human-readable fatal condition to paint on the canvas (if any).
    fatal: Option<String>,
    /// Per-page `render_dimensions()` in **points** (rotation-aware).
    page_w: Vec<u32>,
    page_h: Vec<u32>,
    page_count: usize,

    /// Active view mode.
    mode: Mode,
    /// Canvas size in **device px** (= what the host passed, × dpr).
    canvas_w: u32,
    canvas_h: u32,
    dpr: f32,

    /// Top of the viewport inside the vertical content strip (device px).
    offset: i64,

    // touch gesture state
    down: bool,
    last_y: i64,
    press_x: i64,
    press_y: i64,
    moved: bool,

    /// Interpreter settings (embed-fonts default), shared across pages.
    interp: InterpreterSettings,

    /// Render cache (bounded; oldest evicted first).
    cache: Vec<Rast>,

    /// The `(page, page_count, mode)` triple last pushed to the host; lets
    /// [`Self::report_if_changed`] skip redundant reports on unchanged frames.
    reported: Option<(u32, u32, u32)>,

    /// Signature of the last committed frame (see [`Self::frame_sig`]); lets
    /// `compose` skip the repaint + whole-buffer commit while nothing changed.
    last_sig: Option<(i64, i8, u32, u32)>,
}

const CACHE_MAX: usize = 8;
/// Touch movement (px) before a press becomes a drag rather than a tap.
const TAP_THRESH: i64 = 8;

const CANVAS: u32 = rgba(240, 242, 246, 255); // page gutter / background
const BAR: u32 = rgba(22, 24, 30, 255); // error-screen title band
const INK: u32 = rgba(238, 241, 248, 255); // error-screen text on the band

// ---------------------------------------------------------------------------
// Lifecycle exports
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn init(width: u32, height: u32, dpr_x10: u32) -> i32 {
    let dpr = (dpr_x10.max(10) as f32 / 10.0).max(1.0);
    // Logical canvas × dpr → the device-resolution work canvas we commit.
    let canvas_w = ((width as f32) * dpr).round().max(1.0) as u32;
    let canvas_h = ((height as f32) * dpr).round().max(1.0) as u32;

    let (doc, fatal, page_w, page_h) = match appsdk::file::read_all() {
        None => (
            None,
            Some("no document was provided — open a PDF from a chat message".into()),
            Vec::new(),
            Vec::new(),
        ),
        Some(bytes) => match Pdf::new(bytes) {
            Ok(d) => {
                let (pw, ph) = page_dimensions(&d);
                if pw.is_empty() {
                    (Some(d), Some("this PDF has no pages".into()), pw, ph)
                } else {
                    (Some(d), None, pw, ph)
                }
            }
            Err(e) => (
                None,
                Some(format!("could not parse this PDF: {e:?}")),
                Vec::new(),
                Vec::new(),
            ),
        },
    };

    let page_count = page_w.len();
    appsdk::begin(canvas_w, canvas_h, dpr);
    appsdk::log(&format!(
        "x.pdfview: {page_count} page(s), canvas {canvas_w}×{canvas_h} dpr×{dpr}"
    ));

    let app = App {
        doc,
        fatal,
        page_w,
        page_h,
        page_count,
        mode: Mode::Width,
        canvas_w,
        canvas_h,
        dpr,
        offset: 0,
        down: false,
        last_y: 0,
        press_x: 0,
        press_y: 0,
        moved: false,
        interp: InterpreterSettings::default(),
        cache: Vec::new(),
        reported: None,
        last_sig: None,
    };
    APP.with(|a| *a.borrow_mut() = Some(app));
    compose();
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn tick(_dt_ms: i64) -> i32 {
    // Drain queued input (touch / scroll / mode / resize). `compose` below
    // re-renders only when the visible state actually changed, so an idle
    // overlay costs one input poll + one signature compare per tick.
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.handle_input();
        }
    });
    compose();
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn destroy() -> i32 {
    APP.with(|a| *a.borrow_mut() = None);
    0
}

/// Paint + commit the current frame from the thread-local app, and push the
/// refreshed view-state (page / count / mode) to the host so the app UI's
/// toolbar stays in sync.
///
/// **Idle-skip**: a commit means the host reads the *entire* RGBA buffer out
/// of wasm linear memory and the app rebuilds its `Image` texture from it —
/// ~1 MiB of copy per 390×800-logical canvas, 60× per second when done
/// unconditionally, with the plugin thread burning CPU even while the overlay
/// is minimized. Since this viewer is event-driven (no animation), the
/// composed frame is a pure function of the signature below: when nothing
/// changed, skip the repaint + commit entirely.
fn compose() {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            let sig = app.frame_sig();
            if app.last_sig == Some(sig) {
                return; // visible state unchanged → no repaint, no commit
            }
            app.last_sig = Some(sig);
            app.paint_frame();
            app.report_if_changed();
        }
    });
}

/// Per-page `render_dimensions()` in points, from the page tree.
fn page_dimensions(doc: &Pdf) -> (Vec<u32>, Vec<u32>) {
    let pages = doc.pages();
    let mut pw = Vec::with_capacity(pages.len());
    let mut ph = Vec::with_capacity(pages.len());
    for p in pages.iter() {
        let (w, h) = p.render_dimensions();
        pw.push((w.max(1.0)) as u32);
        ph.push((h.max(1.0)) as u32);
    }
    (pw, ph)
}

// ---------------------------------------------------------------------------
// Layout (all lengths in device px)
// ---------------------------------------------------------------------------

impl App {
    /// Onscreen height of one page cell, in the current mode.
    ///
    /// Single source of truth: this is the *same* value the render raster is
    /// sized to ([`Self::page_target`] `.1`), so layout positions and the
    /// blitted raster can never desync into a 1-px seam or overlap.
    fn cell_height(&self, i: usize) -> i64 {
        self.page_target(i).1 as i64
    }

    /// Top edge (device px) of page `idx` in the current mode.
    fn cell_top(&self, idx: usize) -> i64 {
        let top = 0_i64;
        let limit = idx.min(self.page_count);
        let mut acc = top;
        for i in 0..limit {
            acc += self.cell_height(i);
        }
        acc
    }

    /// Height of the whole vertical content strip (device px).
    fn content_h(&self) -> i64 {
        self.cell_top(self.page_count)
    }

    /// Largest legal `offset` (content may extend past it).
    fn max_offset(&self) -> i64 {
        (self.content_h() - self.view_h()).max(0)
    }

    /// (Height mode) settle on the nearest full page so each page fills the
    /// screen and stays top-aligned ("整页吸顶").
    fn snap(&mut self) {
        let ch = self.view_h();
        if ch <= 0 || self.page_count == 0 {
            self.offset = 0;
            return;
        }
        let idx = (self.offset as f64 / ch as f64).round() as i64;
        let idx = idx.clamp(0, self.page_count as i64 - 1);
        self.offset = idx * ch;
    }

    /// Index of the page whose cell contains content-y `y` (device px).
    fn index_at(&self, y: i64) -> usize {
        let mut acc = 0_i64;
        for i in 0..self.page_count {
            acc += self.cell_height(i);
            if y < acc {
                return i;
            }
        }
        self.page_count.max(1) - 1
    }

    /// The raster (w, h) a page should render at in the current mode (device px).
    fn page_target(&self, idx: usize) -> (u32, u32) {
        let pw = self.page_w[idx].max(1) as f32;
        let ph = self.page_h[idx].max(1) as f32;
        match self.mode {
            Mode::Width => (self.canvas_w, (ph * (self.canvas_w as f32 / pw)).max(1.0) as u32),
            // One page fills the whole canvas.
            Mode::Height => {
                let vh = self.view_h().max(1) as u32;
                ((pw * (vh as f32 / ph)).max(1.0) as u32, vh)
            }
        }
    }

    /// Font scale for the 5×7 bitmap HUD (device px per dot).
    fn font_scale(&self) -> i32 {
        (2.0 * self.dpr).round().clamp(1.0, 8.0) as i32
    }

    /// HUD bar height (device px).
    fn hud_h(&self) -> i32 {
        (7.0 * self.font_scale() as f32 + 14.0).round() as i32
    }

    /// The page-strip region: the **whole canvas**. The app UI floats its
    /// toolbar (mode icons + counter) over the top of the committed frame, so
    /// the pages may use the full height — a Height-fit page fills the screen.
    fn view_h(&self) -> i64 {
        (self.canvas_h as i64).max(1)
    }
}

// ---------------------------------------------------------------------------
// Rendering (render-on-demand, baked opaque RGBA8)
// ---------------------------------------------------------------------------

impl App {
    /// Ensure the cache holds a raster for the pages intersecting the current
    /// viewport; render any that are still missing.
    fn ensure_visible(&mut self) {
        if self.page_count == 0 {
            return;
        }
        let top = self.offset;
        let bot = top + self.view_h();
        let mut acc = 0_i64;
        for i in 0..self.page_count {
            let h = self.cell_height(i);
            if acc + h > top && acc < bot {
                self.ensure(i);
            }
            acc += h;
            if acc >= bot {
                break;
            }
        }
    }

    /// Render page `idx` at the current mode's target size into the cache.
    fn ensure(&mut self, idx: usize) {
        if idx >= self.page_count {
            return;
        }
        let (w, h) = self.page_target(idx);
        if self
            .cache
            .iter()
            .any(|r| r.page == idx as u32 && r.w == w && r.h == h)
        {
            return;
        }
        let Some(doc) = &self.doc else {
            return;
        };
        let Some(px) = render_page_rgba(doc, &self.interp, idx, w, h) else {
            return;
        };
        self.cache.push(Rast {
            page: idx as u32,
            w,
            h,
            px,
        });
        while self.cache.len() > CACHE_MAX {
            self.cache.remove(0);
        }
    }

    /// Signature of everything that changes the committed pixels: scroll
    /// position, view mode, and canvas size. Page rasters are immutable and
    /// cached, so with these fixed the composed frame is bit-identical and
    /// `compose` can skip the repaint + commit.
    fn frame_sig(&self) -> (i64, i8, u32, u32) {
        (
            self.offset,
            match self.mode {
                Mode::Width => 0,
                Mode::Height => 1,
            },
            self.canvas_w,
            self.canvas_h,
        )
    }

    /// Compose + commit the current frame: background, visible page(s), HUD.
    fn paint_frame(&mut self) {
        if self.page_count == 0 {
            // Fatal / empty: still commit a frame so the user sees why.
            let msg = self
                .fatal
                .clone()
                .unwrap_or_else(|| "no pages to show".to_string());
            with_ctx(|ctx| {
                ctx.clear(CANVAS);
                let scale = self.font_scale();
                // Center the message in a padded block below a title bar.
                let bar = self.hud_h();
                ctx.fill_rect(0, 0, ctx.width as i32, bar, BAR);
                ctx.fill_text(10, bar / 2 - scale, scale, "pdf", INK);
                ctx.fill_text(
                    10,
                    bar + scale * 2,
                    scale,
                    &truncate(&msg, 44),
                    INK,
                );
                ctx.commit();
            });
            return;
        }

        self.ensure_visible();

        with_ctx(|ctx| {
            ctx.clear(CANVAS);

            let top = self.offset;
            let bot = top + self.view_h();
            for i in 0..self.page_count {
                let cell_top = self.cell_top(i);
                if cell_top >= bot || cell_top + self.cell_height(i) <= top {
                    continue;
                }
                let Some(r) = self.cache.iter().find(|r| r.page == i as u32) else {
                    continue;
                };
                let x = match self.mode {
                    Mode::Height => ((self.canvas_w as i64 - r.w as i64) / 2).max(0),
                    Mode::Width => 0,
                };
                // Pages fill the whole canvas; the app UI floats its toolbar over
                // the top of this committed frame.
                ctx.blit(x as i32, (cell_top - top) as i32, r.w, r.h, &r.px);
            }

            ctx.commit();
        });
    }
}

/// Render PDF page `index` to an opaque RGBA8 raster at the given device-px
/// size, baking alpha over white so the committed buffer is fully opaque.
///
/// Pure — takes the doc directly, so it holds no plugin-state borrow.
fn render_page_rgba(
    doc: &Pdf,
    interp: &InterpreterSettings,
    index: usize,
    w: u32,
    h: u32,
) -> Option<Vec<u8>> {
    let pages = doc.pages();
    if index >= pages.len() {
        return None;
    }
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        appsdk::log(&format!(
            "x.pdfview: page {index} target {w}×{h} exceeds the u16 raster limit"
        ));
        return None;
    }
    let page = &pages[index];
    let (rw, _rh) = page.render_dimensions();
    let scale = (w as f32) / rw.max(1.0);
    let settings = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        width: Some(w as u16),
        height: Some(h as u16),
        ..RenderSettings::default()
    };
    let pixmap = render_page(page, interp, &settings);
    let pw = pixmap.width() as usize;
    let ph = pixmap.height() as usize;
    let src = pixmap.data_as_u8_slice();
    let n = (pw * ph * 4).min(src.len());
    // `src` is *premultiplied* RGBA8. Bake over opaque white:
    //   out = premul_rgb + 255·(1 − a/255) = premul_rgb + (255 − a)
    //   out_a = a + (255 − a) = 255
    let mut px = vec![0u8; n];
    for k in (0..n).step_by(4) {
        let over = (255 - src[k + 3] as i32).max(0);
        px[k] = (src[k] as i32 + over).clamp(0, 255) as u8;
        px[k + 1] = (src[k + 1] as i32 + over).clamp(0, 255) as u8;
        px[k + 2] = (src[k + 2] as i32 + over).clamp(0, 255) as u8;
        px[k + 3] = 255;
    }
    Some(px)
}

/// ASCII-truncate `s` to `max_chars` (the bitmap HUD only has a 5×7 font, so
/// any non-ASCII is dropped here).
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for c in s.chars().take(max_chars) {
        if c.is_ascii() {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

impl App {
    fn handle_input(&mut self) {
        while let Some(ev) = poll_input() {
            let x = ev.x as i64;
            let y = ev.y as i64;
            match ev.kind {
                EV_TOUCH_DOWN => {
                    self.down = true;
                    self.last_y = y;
                    self.press_x = x;
                    self.press_y = y;
                    self.moved = false;
                }
                EV_TOUCH_MOVE => {
                    if !self.down {
                        continue;
                    }
                    let dy = y - self.last_y;
                    self.last_y = y;
                    let moved_px = (x - self.press_x).abs() + (y - self.press_y).abs();
                    if self.moved || moved_px > TAP_THRESH {
                        self.moved = true;
                        // Natural scroll: finger up ⇒ content follows ⇒ offset grows.
                        self.offset = (self.offset - dy).clamp(0, self.max_offset());
                    }
                }
                EV_TOUCH_UP => {
                    self.down = false;
                    if !self.moved {
                        self.on_tap(x, y);
                    } else if self.mode == Mode::Height {
                        // Drag ended: settle on the nearest full page.
                        self.snap();
                    }
                }
                // Wheel / trackpad scroll from the app UI (Slint TouchArea
                // `scroll-event`): `y` = vertical delta in device px. Slint's
                // own `ScrollView`/`TabWidget` use the same convention — a
                // *negative* delta means the user scrolled toward later
                // content — so this is the identical "natural scroll" math as
                // `EV_TOUCH_MOVE` above (`offset -= dy`).
                EV_SCROLL => {
                    if self.page_count == 0 || ev.y == 0 {
                        continue;
                    }
                    let dy = ev.y as i64;
                    self.offset = (self.offset - dy).clamp(0, self.max_offset());
                }
                // Host window drag-resize (the UI pump forwards `EV_RESIZE`):
                // `x`/`y` = the new canvas size in **device px** (the host
                // already scaled logical → device, same as touch). Resize the
                // work canvas (reallocate the frame buffer), drop the rasters
                // (sized for the old canvas), keep the reader at the same
                // position, then re-render (the frame signature changes, so
                // the next `compose` paints + commits fresh).
                EV_RESIZE => {
                    let w = ev.x.max(1) as u32;
                    let h = ev.y.max(1) as u32;
                    if w != self.canvas_w || h != self.canvas_h {
                        self.canvas_w = w;
                        self.canvas_h = h;
                        begin(w, h, self.dpr);
                        self.cache.clear();
                        self.offset = self.offset.clamp(0, self.max_offset());
                        self.reported = None;
                    }
                }
                // The app UI's toolbar sends a mode-switch command through the
                // input channel as a key event whose `code` is a `VIEW_MODE_*`.
                EV_KEY_DOWN => {
                    if ev.code == VIEW_MODE_WIDTH as i32 {
                        self.set_mode(Mode::Width);
                    } else if ev.code == VIEW_MODE_HEIGHT as i32 {
                        self.set_mode(Mode::Height);
                    }
                }
                _ => {}
            }
        }
    }

    /// Switch to view mode `m` (used by the app UI's Fit-Width / Fit-Height
    /// icons). Keeps the reader on the same logical page across the switch.
    fn set_mode(&mut self, m: Mode) {
        if m == self.mode {
            return;
        }
        let keep = self.index_at(self.offset.max(0) + self.view_h() / 2);
        self.mode = m;
        self.cache.clear(); // rasters are sized per mode; re-render fresh
        self.offset = self.cell_top(keep).clamp(0, self.max_offset());
    }

    /// Push the active `(page, count, mode)` to the host when it has changed, so
    /// the app UI's toolbar (icons + counter) mirrors the plugin's state.
    fn report_if_changed(&mut self) {
        if self.page_count == 0 {
            return;
        }
        let page = self.index_at(self.offset.max(0) + self.view_h() / 2) as u32 + 1;
        let mode = match self.mode {
            Mode::Width => VIEW_MODE_WIDTH,
            Mode::Height => VIEW_MODE_HEIGHT,
        };
        let cur = (page, self.page_count as u32, mode);
        if self.reported == Some(cur) {
            return;
        }
        self.reported = Some(cur);
        report_view(page as i32, cur.1, cur.2);
    }

    /// A tap in the content area page-flips horizontally: left half → previous
    /// page, right half → next page. (The app UI's top-bar controls sit on top
    /// and intercept their own touches, so those never reach the plugin;
    /// vertical drags scroll and are handled separately.)
    fn on_tap(&mut self, x: i64, _y: i64) {
        if self.page_count == 0 {
            return;
        }
        let mid = (self.canvas_w as i64) / 2;
        let idx = self.index_at(self.offset.max(0) + self.view_h() / 2);
        let target = if x >= mid { (idx + 1).min(self.page_count - 1) } else { idx.saturating_sub(1) };
        self.offset = self.cell_top(target).clamp(0, self.max_offset());
    }
}
