//! `pdf` — native PDF viewer core (no wasm): an event-driven `Viewer` owning the parsed document + view state + render cache.
//!
//! Two modes, both a continuous vertical strip: Width (page fits canvas width) / Height (page fits canvas height), separated from neighbours by a canvas-gray gutter; a release parks exactly where it is — never a page-covering snap.
//!
//! Gesture coords are all **device px** (the caller scales logical→device): vertical drag / wheel = scroll (`dy` positive = up = toward earlier content, Slint `delta-y` sign), tap left/right half = prev/next page in **Height** mode only (Width mode is the continuous strip, its taps are ignored); plus `set_mode`, `resize`.
//!
//! Rendering: pages rasterised via `pdf-render`/`pdf-interpret`, alpha baked over white (fully opaque), canvas = logical size × dpr, blitted 1:1; commit-on-change — `step` does nothing when the frame signature (offset, mode, canvas size) is unchanged.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use pdf_interpret::InterpreterSettings;
use pdf_render::pdf_syntax::Pdf;
use pdf_render::{RenderSettings, render as render_page};

/// Fit each page to the canvas width (vertical strip).
pub const VIEW_MODE_WIDTH: i32 = 1;
/// Fit each page to the canvas height (one page per screen).
pub const VIEW_MODE_HEIGHT: i32 = 2;

/// The two supported view modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Width,
    Height,
}

impl Mode {
    /// The `VIEW_MODE_*` code the app UI uses to identify a mode.
    pub const fn code(self) -> i32 {
        match self {
            Mode::Width => VIEW_MODE_WIDTH,
            Mode::Height => VIEW_MODE_HEIGHT,
        }
    }

    /// The mode for a `VIEW_MODE_*` code, if any.
    pub const fn from_code(code: i32) -> Option<Mode> {
        match code {
            VIEW_MODE_WIDTH => Some(Mode::Width),
            VIEW_MODE_HEIGHT => Some(Mode::Height),
            _ => None,
        }
    }
}

/// Error opening a document.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not open this file: {0}")]
    Read(String),
    #[error("could not parse this PDF: {0}")]
    Parse(String),
    #[error("this PDF has no pages")]
    NoPages,
}

/// Opaque RGBA8 frame: `rgba` is exactly `width * height * 4` bytes, top-left origin, alpha already baked opaque.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The active view state, reported to the app's top bar (mode icons + `n/m` counter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewState {
    /// Active page, 1-based.
    pub page: i32,
    /// Total page count.
    pub page_count: i32,
    /// Active mode: [`VIEW_MODE_WIDTH`] or [`VIEW_MODE_HEIGHT`].
    pub mode: i32,
}

/// The result of a [`Viewer::step`] pump.
#[derive(Debug)]
pub enum Step {
    /// Visible state unchanged since the last pump — keep the last frame.
    Unchanged,
    /// Visible state changed — repaint with this new frame.
    Frame(Frame),
    /// Fatal (no pages): `frame` is a solid panel; surface `message` in the UI.
    Fatal { frame: Frame, message: String },
}

/// A rendered, baked-over-white, fully-opaque RGBA8 page raster (cache slot).
struct Rast {
    page: u32,
    w: u32,
    h: u32,
    px: Vec<u8>,
}

/// A queued input event (device px / code, per the caller's convention).
#[derive(Clone, Copy, Debug)]
enum Ev {
    TouchDown(i32, i32),
    TouchMove(i32, i32),
    TouchUp(i32, i32),
    /// Vertical scroll delta in device px; positive = up (toward earlier content), negative = down — matches Slint's `delta-y` sign.
    Scroll(i32),
    /// New canvas size in device px.
    Resize(u32, u32),
    /// Switch view mode.
    Mode(Mode),
}

struct Inner {
    /// The parsed document (a loaded viewer always has one).
    doc: Pdf,
    /// Per-page `render_dimensions()` in **points** (rotation-aware).
    page_w: Vec<u32>,
    page_h: Vec<u32>,
    page_count: usize,

    /// Active view mode.
    mode: Mode,
    /// Canvas size in **device px** (= logical × dpr).
    canvas_w: u32,
    canvas_h: u32,

    /// Top of the viewport inside the vertical content strip (device px).
    offset: i64,

    // -- touch gesture state --
    down: bool,
    last_y: i32,
    press_x: i32,
    press_y: i32,
    moved: bool,

    /// Queued input events, drained by [`Viewer::step`].
    input: VecDeque<Ev>,

    /// Interpreter settings (embed-fonts default), shared across pages.
    interp: InterpreterSettings,

    /// Render cache, bounded by bytes (CACHE_MAX_BYTES, floor CACHE_MIN_PAGES; oldest evicted first).
    cache: Vec<Rast>,

    /// Total bytes held in `cache` (tracked so eviction is a sum, not a count).
    cache_bytes: usize,

    /// Last committed frame signature (see `frame_sig`); lets `step` skip the repaint while unchanged.
    last_sig: Option<(i64, i8, u32, u32)>,
}

/// Native PDF viewer: parsed document + view state + render cache; create via `open`/`open_bytes`, forward gestures, call `step` per pump for the next `Frame`.
pub struct Viewer {
    inner: Mutex<Inner>,
}

impl Viewer {
    // -------------------- Open --------------------

    /// Open the PDF at `path` for a `width`×`height` **logical** canvas at dpr `dpr`.
    pub fn open(path: &Path, width: u32, height: u32, dpr: f32) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(|e| Error::Read(e.to_string()))?;
        Self::open_bytes(bytes, width, height, dpr)
    }

    /// Open a PDF from in-memory bytes for a `width`×`height` **logical** canvas at dpr `dpr`.
    pub fn open_bytes(bytes: Vec<u8>, width: u32, height: u32, dpr: f32) -> Result<Self, Error> {
        let dpr = dpr.max(1.0);
        // Logical canvas × dpr → the device-resolution work canvas we commit.
        let canvas_w = ((width as f32) * dpr).round().max(1.0) as u32;
        let canvas_h = ((height as f32) * dpr).round().max(1.0) as u32;

        let doc = Pdf::new(bytes).map_err(|e| Error::Parse(format!("{e:?}")))?;
        let (page_w, page_h) = page_dimensions(&doc);
        let page_count = page_w.len();
        if page_count == 0 {
            return Err(Error::NoPages);
        }

        Ok(Self {
            inner: Mutex::new(Inner {
                doc,
                page_w,
                page_h,
                page_count,
                mode: Mode::Width,
                canvas_w,
                canvas_h,
                offset: 0,
                down: false,
                last_y: 0,
                press_x: 0,
                press_y: 0,
                moved: false,
                input: VecDeque::new(),
                interp: InterpreterSettings::default(),
                cache: Vec::new(),
                cache_bytes: 0,
                last_sig: None,
            }),
        })
    }

    // -------------------- Input (device px; drained on the next step) --------------------

    /// A press began at `(x, y)`.
    pub fn touch_down(&self, x: i32, y: i32) {
        self.inner().input.push_back(Ev::TouchDown(x, y));
    }

    /// The pressed pointer moved to `(x, y)`.
    pub fn touch_move(&self, x: i32, y: i32) {
        self.inner().input.push_back(Ev::TouchMove(x, y));
    }

    /// A press ended at `(x, y)`: a tap page-flips (Height mode only — Width mode ignores taps); a released drag parks exactly where it is (no page-covering snap).
    pub fn touch_up(&self, x: i32, y: i32) {
        self.inner().input.push_back(Ev::TouchUp(x, y));
    }

    /// A wheel/trackpad scroll, vertical delta `dy` in device px (positive = up, Slint `delta-y` sign).
    pub fn scroll(&self, dy: i32) {
        self.inner().input.push_back(Ev::Scroll(dy));
    }

    /// Switch the rendered view mode to `m`.
    pub fn set_mode(&self, m: Mode) {
        self.inner().input.push_back(Ev::Mode(m));
    }

    /// The window was resized to `w`×`h` device px; re-render the canvas.
    pub fn resize(&self, w: u32, h: u32) {
        self.inner().input.push_back(Ev::Resize(w.max(1), h.max(1)));
    }

    // -------------------- Queries the app reads between pumps --------------------

    /// The active `(page, page_count, mode)` triple.
    pub fn view_state(&self) -> ViewState {
        let g = self.inner();
        ViewState {
            page: g.index_at(g.offset.max(0) + g.view_h() / 2) as i32 + 1,
            page_count: g.page_count as i32,
            mode: g.mode.code(),
        }
    }

    /// Total page count.
    pub fn page_count(&self) -> i32 {
        self.inner().page_count as i32
    }

    /// Bytes held by the render cache (bounded at CACHE_MAX_BYTES, floor at CACHE_MIN_PAGES pages).
    pub fn cache_bytes(&self) -> usize {
        self.inner().cache_bytes
    }

    /// Scroll offset in **device px**: top edge of the viewport within the content strip.
    pub fn offset(&self) -> i64 {
        self.inner().offset
    }

    /// Canvas size in **device px** (= logical × dpr at open/resize).
    pub fn canvas_size(&self) -> (u32, u32) {
        let g = self.inner();
        (g.canvas_w, g.canvas_h)
    }

    /// Page whose cell contains content-y `y` (**device px**), clamped to the last page.
    pub fn index_at(&self, y: i64) -> usize {
        self.inner().index_at(y.max(0))
    }

    /// Top edge of page `idx` within the strip (**device px**); `i64::MAX` if out of range.
    pub fn page_top(&self, idx: usize) -> i64 {
        let g = self.inner();
        if idx >= g.page_count {
            i64::MAX
        } else {
            g.cell_top(idx)
        }
    }

    /// The raster size page `idx` renders at in the current mode (**device px**).
    pub fn page_cell(&self, idx: usize) -> (u32, u32) {
        let g = self.inner();
        if idx >= g.page_count {
            (0, 0)
        } else {
            g.page_target(idx)
        }
    }

    /// Ensure page `idx` has a rendered raster in the cache — the **pre-warm / on-demand render** the app calls per tick (no-op if already cached).
    pub fn ensure_page(&self, idx: usize) {
        let mut g = self.inner();
        if idx < g.page_count {
            g.ensure(idx);
        }
    }

    /// Ensure page `idx` is cached, then run `f` with its raster (w, h **device px** + opaque RGBA8 slice) — the zero-clone companion to `page_rgba` (`f` runs while the viewer lock is held).
    pub fn with_raster<F, R>(&self, idx: usize, f: F) -> Option<R>
    where
        F: FnOnce(u32, u32, &[u8]) -> R,
    {
        let mut g = self.inner();
        if idx >= g.page_count {
            return None;
        }
        g.ensure(idx);
        let r = g.cache.iter().find(|r| r.page == idx as u32)?;
        Some(f(r.w, r.h, &r.px))
    }

    /// Page `idx`'s opaque RGBA raster at the current mode's size (**device px**), rendered on demand; `None` if out of range.
    pub fn page_rgba(&self, idx: usize) -> Option<Frame> {
        self.with_raster(idx, |w, h, rgba| Frame {
            width: w,
            height: h,
            rgba: rgba.to_vec(),
        })
    }

    // -------------------- Pump --------------------

    /// Drain queued input **without rendering**; `true` if the committed layout (scroll / mode / canvas) changed, `false` when idle.
    pub fn pump(&self) -> bool {
        let mut g = self.inner();
        if g.input.is_empty() {
            return false;
        }
        let before = g.frame_sig();
        g.drain_input();
        g.frame_sig() != before
    }

    /// One pump: drain input; if the visible state changed, render the visible pages and compose the frame (commit-on-change: idle cost = poll + signature compare).
    pub fn step(&self) -> Step {
        let mut g = self.inner();
        g.drain_input();
        let sig = g.frame_sig();
        if g.last_sig == Some(sig) {
            return Step::Unchanged;
        }
        g.last_sig = Some(sig);
        g.ensure_visible();

        Step::Frame(g.compose())
    }

    /// Lock helper; recovers on poison (we own the only handle).
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

// -------------------- Layout (all lengths in device px) --------------------

impl Inner {
    /// Stride of one page on the strip (device px) = its raster height **plus** the canvas-gray gutter; layout and blit derive from this, so they can't desync.
    fn cell_height(&self, i: usize) -> i64 {
        self.page_target(i).1 as i64 + self.strip_gap()
    }

    /// Bottom edge of page `idx` (top of its trailing gutter, device px).
    fn cell_bottom(&self, idx: usize) -> i64 {
        self.cell_top(idx) + self.page_target(idx).1 as i64
    }

    /// Top edge (device px) of page `idx`; a `strip_gap` gutter follows every page except the last.
    fn cell_top(&self, idx: usize) -> i64 {
        let limit = idx.min(self.page_count - 1);
        let mut acc = 0_i64;
        for i in 0..limit {
            acc += self.cell_height(i);
        }
        acc
    }

    /// Canvas-gray gutter between adjacent pages (device px); in Height mode it scales with the page.
    fn strip_gap(&self) -> i64 {
        match self.mode {
            Mode::Width => GUTTER_DEV_PX,
            Mode::Height => {
                let ph = self.page_h[0].max(1) as f32;
                ((GUTTER_DEV_PX as f32) * (self.view_h() as f32 / ph)).clamp(8.0, 64.0) as i64
            }
        }
    }

    /// Largest legal `offset` (content may extend past it).
    fn max_offset(&self) -> i64 {
        if self.page_count == 0 {
            return 0;
        }
        (self.cell_bottom(self.page_count - 1) - self.view_h()).max(0)
    }

    /// Page whose stride contains content-y `y` (device px); the gutter after a page belongs to that page.
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
            Mode::Width => (
                self.canvas_w,
                (ph * (self.canvas_w as f32 / pw)).max(1.0) as u32,
            ),
            // One page + trailing gutter fills the canvas ⇒ page itself scales to the leftover height.
            Mode::Height => {
                let vh = self.view_h().max(1) as f32;
                let gap = self.strip_gap() as f32;
                let scale = (vh - gap).max(1.0) / ph;
                ((pw * scale).max(1.0) as u32, ((ph * scale).max(1.0)) as u32)
            }
        }
    }

    /// The page-strip region = the **whole canvas** (the app floats its toolbar over the top).
    fn view_h(&self) -> i64 {
        (self.canvas_h as i64).max(1)
    }
}

// -------------------- State updates (gesture / scroll / mode / resize) --------------------

impl Inner {
    /// Drain the queued input events, updating the model — pure bookkeeping, no rendering (that is `step`'s job).
    fn drain_input(&mut self) {
        while let Some(ev) = self.input.pop_front() {
            match ev {
                Ev::TouchDown(x, y) => {
                    self.down = true;
                    self.last_y = y;
                    self.press_x = x;
                    self.press_y = y;
                    self.moved = false;
                }
                Ev::TouchMove(x, y) => {
                    if !self.down {
                        continue;
                    }
                    let dy = y - self.last_y;
                    self.last_y = y;
                    let moved_px = (x - self.press_x).abs() + (y - self.press_y).abs();
                    if self.moved || moved_px > TAP_THRESH {
                        self.moved = true;
                        // Natural scroll: finger up ⇒ content follows ⇒ offset grows.
                        self.offset = (self.offset - dy as i64).clamp(0, self.max_offset());
                    }
                }
                // A release parks exactly where the finger let go (neighbour stays connected, nothing slides in to cover it).
                Ev::TouchUp(x, _y) => {
                    let was_down = self.down;
                    let moved = self.moved;
                    self.down = false;
                    if was_down && !moved {
                        self.on_tap(x);
                    }
                }
                Ev::Scroll(dy) => {
                    if dy == 0 {
                        continue;
                    }
                    // Slint: negative delta-y = toward later content; same natural-scroll math as touch (`offset -= dy`).
                    self.offset = (self.offset - dy as i64).clamp(0, self.max_offset());
                }
                Ev::Resize(w, h) => {
                    if w != self.canvas_w || h != self.canvas_h {
                        self.canvas_w = w;
                        self.canvas_h = h;
                        self.clear_cache();
                        self.offset = self.offset.clamp(0, self.max_offset());
                    }
                }
                Ev::Mode(m) => {
                    self.set_mode(m);
                }
            }
        }
    }

    /// Switch to view mode `m`; keeps the reader on the same logical page.
    fn set_mode(&mut self, m: Mode) {
        if m == self.mode {
            return;
        }
        let keep = self.index_at(self.offset.max(0) + self.view_h() / 2);
        self.mode = m;
        self.clear_cache(); // rasters are sized per mode; re-render fresh
        self.offset = self.cell_top(keep).clamp(0, self.max_offset());
    }

    /// A tap page-flips horizontally: left half → previous, right half → next — **Height mode only**;
    /// Width mode is the continuous strip (drag/wheel scroll) and ignores taps entirely.
    fn on_tap(&mut self, x: i32) {
        if self.mode != Mode::Height || self.page_count == 0 {
            return;
        }
        let mid = (self.canvas_w as i64) / 2;
        let idx = self.index_at(self.offset.max(0) + self.view_h() / 2);
        let target = if i64::from(x) >= mid {
            (idx + 1).min(self.page_count - 1)
        } else {
            idx.saturating_sub(1)
        };
        self.offset = self.cell_top(target).clamp(0, self.max_offset());
    }
}

// -------------------- Rendering (render-on-demand, baked opaque RGBA8) --------------------

impl Inner {
    /// Ensure the cache has rasters for the pages overlapping the current viewport; render the missing ones.
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

    /// Drop every cached raster (size- or mode-dependent; re-render fresh).
    fn clear_cache(&mut self) {
        self.cache.clear();
        self.cache_bytes = 0;
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
        let Some(px) = render_page_rgba(&self.doc, &self.interp, idx, w, h) else {
            return;
        };
        self.cache.push(Rast {
            page: idx as u32,
            w,
            h,
            px,
        });
        self.cache_bytes += self.cache.last().expect("just pushed").px.len();
        // Byte-budget eviction (oldest first), floored at CACHE_MIN_PAGES so the live viewport + neighbors stay cached.
        while self.cache_bytes > CACHE_MAX_BYTES && self.cache.len() > CACHE_MIN_PAGES {
            let old = self.cache.remove(0);
            self.cache_bytes -= old.px.len();
        }
    }

    /// Signature of everything that changes the committed pixels (offset, mode, canvas size); unchanged ⇒ `step` skips the repaint.
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

    /// Compose the current frame: background, then the visible page(s).
    fn compose(&self) -> Frame {
        let (dw, dh) = (self.canvas_w as usize, self.canvas_h as usize);
        // Solid light canvas (page gutter), then overlay the visible page(s).
        let mut row = Vec::with_capacity(dw * 4);
        for _ in 0..dw {
            row.extend_from_slice(&[BG_R, BG_G, BG_B, 255]);
        }
        let mut px = Vec::with_capacity(dw * dh * 4);
        for _ in 0..dh {
            px.extend_from_slice(&row);
        }

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
                Mode::Height => (self.canvas_w as i64 - r.w as i64) / 2, // can be < 0 (centre-crop)
                Mode::Width => 0,
            };
            // SIGNED placement on the strip: a page scrolled above the viewport keeps flowing out the top (overhang clipped off the source), never snapped back to the canvas top.
            blit(&mut px, dw, dh, x, cell_top - top, r.w, r.h, &r.px);
        }

        Frame {
            width: self.canvas_w,
            height: self.canvas_h,
            rgba: px,
        }
    }
}

/// Render PDF page `index` to an opaque RGBA8 raster at a device-px size, alpha baked over white (fully opaque).
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
    // `src` is *premultiplied* RGBA8; bake over opaque white: out = premul_rgb + (255 − a), out_a = 255.
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

/// Soft byte budget for the render cache (stays flat even for full-screen Retina pages).
const CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;
/// Minimum pages the cache always keeps (eviction stops here even over budget) — on-screen page + pre-warmed neighbors.
const CACHE_MIN_PAGES: usize = 3;
/// Touch movement (px) before a press becomes a drag rather than a tap.
const TAP_THRESH: i32 = 8;
/// Canvas-gray gutter between adjacent pages in Width mode (device px) — Height mode scales it with the page (see `strip_gap`).
const GUTTER_DEV_PX: i64 = 24;
/// The page gutter / canvas background (opaque light).
const BG_R: u8 = 240;
const BG_G: u8 = 242;
const BG_B: u8 = 246;

/// Copy an opaque RGBA8 raster `src` (size `sw`×`sh`) into `dst` (size `dw`×`dh`) with top-left at **signed** `(dx, dy)`,
/// clipping both ends: overhang past top/left is cut **from the source** (never re-anchored to the canvas top), past bottom/right is dropped.
#[allow(clippy::too_many_arguments)]
fn blit(dst: &mut [u8], dw: usize, dh: usize, dx: i64, dy: i64, sw: u32, sh: u32, src: &[u8]) {
    // Destination sub-rectangle: (dx,dy)..(dx+sw,dy+sh) ∩ (0,0)..(dw,dh).
    let x0 = dx.max(0) as usize;
    let y0 = dy.max(0) as usize;
    let x1 = (dx + sw as i64).min(dw as i64);
    let y1 = (dy + sh as i64).min(dh as i64);
    if x1 <= x0 as i64 || y1 <= y0 as i64 {
        return; // the page is entirely off-canvas
    }
    let cols = (x1 - x0 as i64) as usize;
    let rows = (y1 - y0 as i64) as usize;
    // Where that rectangle starts inside the source (what scrolled off).
    let sx = (-dx).max(0) as usize;
    let sy = (-dy).max(0) as usize;
    let src_row = sw as usize * 4;
    let dst_row = dw * 4;
    let n = cols * 4;
    for r in 0..rows {
        let s = (sy + r) * src_row + sx * 4;
        let d = (y0 + r) * dst_row + x0 * 4;
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a minimal valid PDF from flat object bodies (`i+1` = `objs[i]`) with a correct xref, exercising the real raster path end-to-end.
    fn build_pdf(objs: Vec<String>) -> Vec<u8> {
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets: Vec<usize> = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{}\nendobj\n", i + 1, body));
        }
        let xref_pos = out.len();
        out.push_str(&format!("xref\n0 {}\n", objs.len() + 1));
        out.push_str("0000000000 65535 f \n");
        for o in &offsets {
            out.push_str(&format!("{:010} 00000 n \n", o));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objs.len() + 1,
            xref_pos
        ));
        out.into_bytes()
    }

    /// A valid single-page PDF (a "Hi" text line on a 612×792 page).
    fn test_pdf() -> Vec<u8> {
        build_pdf(vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources \
               << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .into(),
            "<< /Length 29 >>\nstream\nBT /F1 24 Tf 72 700 Td (Hi) Tj ET\nendstream".into(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ])
    }

    /// An `n`-page PDF (all 612×792, shared content + font) for scrolling tests.
    fn test_pdf_n(n: usize) -> Vec<u8> {
        assert!(n >= 1);
        let kids: Vec<String> = (3..3 + n).map(|i| format!("{i} 0 R")).collect();
        let contents_idx = 3 + n;
        let font_idx = n + 4;
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
        ];
        for _ in 0..n {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources \
                   << /Font << /F1 {font_idx} 0 R >> >> /Contents {contents_idx} 0 R >>"
            ));
        }
        objs.push("<< /Length 29 >>\nstream\nBT /F1 24 Tf 72 700 Td (Hi) Tj ET\nendstream".into());
        objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
        build_pdf(objs)
    }

    #[test]
    fn opens_and_renders_first_page() {
        // 390×800 logical @ dpr 2 → 780×1600 device canvas.
        let v = Viewer::open_bytes(test_pdf(), 390, 800, 2.0).expect("should parse");
        assert_eq!(v.page_count(), 1);

        match v.step() {
            Step::Frame(f) => {
                assert_eq!((f.width, f.height), (780, 1600));
                assert_eq!(f.rgba.len(), (780 * 1600 * 4) as usize);
                // Fully opaque everywhere.
                assert!(
                    f.rgba
                        .iter()
                        .enumerate()
                        .all(|(i, c)| i % 4 != 3 || *c == 255)
                );
            }
            other => panic!("expected a rendered frame, got {other:?}"),
        }

        // A second step with no input → unchanged (commit-on-change invariant).
        assert!(matches!(v.step(), Step::Unchanged));

        let vs = v.view_state();
        assert_eq!(vs.page, 1);
        assert_eq!(vs.page_count, 1);
        assert_eq!(vs.mode, VIEW_MODE_WIDTH);
    }

    #[test]
    fn tap_pages_only_in_height_mode() {
        let v = Viewer::open_bytes(test_pdf_n(2), 390, 800, 1.0).expect("should parse");
        assert!(matches!(v.step(), Step::Frame(_)));

        // Width-fit: taps are ignored (the continuous strip scrolls via drag/wheel) — right half, then left.
        v.touch_down(300, 10); // right half
        v.touch_up(300, 10);
        v.touch_down(10, 10); // left half
        v.touch_up(10, 10);
        assert!(
            matches!(v.step(), Step::Unchanged),
            "width mode must ignore taps"
        );
        assert_eq!(v.view_state().page, 1);

        // Switch to height-fit: the canvas re-renders (signature changes).
        v.set_mode(Mode::Height);
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.view_state().mode, VIEW_MODE_HEIGHT);

        // Height-fit: tap right half → next, tap left half → back to previous.
        v.touch_down(300, 400);
        v.touch_up(300, 400);
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.view_state().page, 2);
        v.touch_down(10, 400);
        v.touch_up(10, 400);
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.view_state().page, 1);
    }

    #[test]
    fn switches_mode_repaint() {
        let v = Viewer::open_bytes(test_pdf(), 390, 800, 1.0).expect("should parse");
        v.set_mode(Mode::Height);
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.view_state().mode, VIEW_MODE_HEIGHT);
        v.set_mode(Mode::Width);
        let _ = v.step();
        assert_eq!(v.view_state().mode, VIEW_MODE_WIDTH);
    }

    /// Height mode: a released drag parks **exactly** where the finger let go (current page + neighbour stay visible together).
    #[test]
    fn height_mode_parks_where_released() {
        // 390×800 logical @ dpr 1 → each page cell is 800px tall here.
        let v = Viewer::open_bytes(test_pdf_n(2), 390, 800, 1.0).expect("should parse");
        v.set_mode(Mode::Height);
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.view_state().page, 1);

        // Drag up by half a page and release mid-transition.
        v.touch_down(195, 400);
        v.touch_move(195, 300); // offset 100
        v.touch_move(195, 0); // offset 400 (the half-page seam is on screen)
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.offset(), 400);
        v.touch_up(195, 0);

        // Parked at exactly 400 — not snapped back to 0, not pushed on to 800 (both pages still composed from that offset).
        assert!(matches!(v.step(), Step::Unchanged | Step::Frame(_)));
        assert_eq!(v.offset(), 400);

        // Wheel keeps scrolling the same strip, also free (no settle).
        v.scroll(-100);
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.offset(), 500);
    }

    /// Regression "next page covers the previous one / current page sticks to the top": the whole content strip must move as one.
    /// Each "Hi" line sits ~76–93px below the cell top (PDF y=700 is measured up from the page bottom), and Height mode on 700×800 makes cells exactly 800px tall: as the offset grows, the topmost dark line must follow it 1:1, never pinned at the top.
    #[test]
    fn scroll_moves_the_whole_strip() {
        let v = Viewer::open_bytes(test_pdf_n(2), 700, 800, 1.0).expect("should parse");
        v.set_mode(Mode::Height);

        let line_at = |v: &Viewer| -> usize {
            let f = match v.step() {
                Step::Frame(f) => f,
                _ => panic!("expected a frame"),
            };
            let w = f.width as usize;
            assert!(w > 0);
            for y in 0..(f.rgba.len() / (w * 4)) {
                let row = &f.rgba[y * w * 4..(y + 1) * w * 4];
                if row.iter().step_by(4).any(|&r| r < 128) {
                    return y;
                }
            }
            panic!("no dark text row found");
        };

        // offset 0: page-1's line, just under the top edge.
        let y0 = line_at(&v);
        assert!((30..170).contains(&y0), "page-1 line at offset 0: {y0}");

        // 200px steps → offsets 200/400/600: page-1's line (93) has flowed off the top, so this lands on page-2's line 893−off = 693/493/293; a page pinned to the top would always report ~93.
        for (off, want) in [(200i64, 693i64), (400, 493), (600, 293)] {
            v.scroll(-200);
            let y = line_at(&v) as i64; // step() inside drains the scroll
            assert_eq!(v.offset(), off);
            assert!(
                (y - want).unsigned_abs() <= 40,
                "at offset {off} the line must be at ~{want}, was {y}"
            );
        }

        // One full page of scroll (clamped at the end of the strip, 776px scrollable): page-2's line now sits where page-1's had sat, page counter is 2.
        v.scroll(-400);
        let y = line_at(&v);
        assert_eq!(v.offset(), 776); // (2×776 + 24 gutter) − 800 viewport
        assert!(
            (30..200).contains(&y),
            "after one full page, page-2's line should be where page-1's was: {y}"
        );
        assert_eq!(v.view_state().page, 2);
    }

    /// Grey gutter: in Width mode a 24px band of canvas-grey separates two pages — middle rows are the background color, paper on either side is white.
    #[test]
    fn gutter_between_pages_is_canvas_gray() {
        let v = Viewer::open_bytes(test_pdf_n(2), 390, 800, 1.0).expect("should parse");
        let px_at = |f: &Frame, x: usize, y: usize| -> (u8, u8, u8) {
            let i = (y * f.width as usize + x) * 4;
            (f.rgba[i], f.rgba[i + 1], f.rgba[i + 2])
        };

        // Park the strip at offset 150 (2×504 pages + 24 gutter − 800 view → clamped to 232); the gutter row is 504−150 .. 528−150 = 354 .. 378.
        v.touch_down(195, 500);
        v.touch_move(195, 350);
        v.touch_up(195, 350);
        let f = match v.step() {
            Step::Frame(f) => f,
            _ => panic!("expected a frame"),
        };
        assert_eq!(v.offset(), 150);
        let (gr, gg, gb) = px_at(&f, 40, 366); // the gutter row
        assert_eq!(
            (gr, gg, gb),
            (BG_R, BG_G, BG_B),
            "gutter row must be canvas gray, got ({gr}, {gg}, {gb})"
        );
        let white_like = |c: (u8, u8, u8)| c.0 >= 250 && c.1 >= 250 && c.2 >= 250;
        assert!(
            white_like(px_at(&f, 40, 300)),
            "page-1 paper above the gutter"
        );
        assert!(
            white_like(px_at(&f, 40, 430)),
            "page-2 paper below the gutter"
        );
    }

    /// Width mode: a released drag parks exactly where it is too (free-strip behaviour).
    #[test]
    fn width_mode_parks_where_released() {
        let v = Viewer::open_bytes(test_pdf_n(2), 390, 800, 1.0).expect("should parse");
        assert!(matches!(v.step(), Step::Frame(_)));

        v.touch_down(195, 600);
        v.touch_move(195, 400); // dragged 200px
        assert!(matches!(v.step(), Step::Frame(_)));
        assert_eq!(v.offset(), 200);
        v.touch_up(195, 400);
        assert_eq!(v.offset(), 200); // parked, untouched
        assert!(matches!(v.step(), Step::Unchanged));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let r = Viewer::open_bytes(b"not a pdf at all".to_vec(), 100, 100, 1.0);
        assert!(r.is_err());
    }
}
