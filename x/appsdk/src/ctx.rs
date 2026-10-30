//! The plugin's drawing context: an RGBA8 frame buffer in the plugin's own
//! linear memory, plus the commit/import plumbing.
//!
//! One `Ctx` exists per plugin instance (thread-local in `appsdk`); the
//! plugin draws into it and calls [`Ctx::commit`] to hand a frame to the
//! host (see `ctx.commit` — the host copies the buffer out and parks it for
//! the UI thread).

use crate::draw;

/// Pack an RGBA color (straight alpha), stored as little-endian in the buf.
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16) | ((a as u32) << 24)
}

/// Canvas buffer + drawing primitives (all in canvas pixels).
pub struct Ctx {
    pub width: u32,
    pub height: u32,
    /// Effective device-pixel-ratio (1.0 = 1:1).
    pub dpr: f32,
    buf: Vec<u8>,
}

impl Ctx {
    pub fn new(width: u32, height: u32, dpr: f32) -> Self {
        let buf = vec![0u8; (width * height * 4) as usize];
        Self {
            width,
            height,
            dpr,
            buf,
        }
    }

    /// Fill the whole canvas.
    pub fn clear(&mut self, rgba: u32) {
        let b = rgba.to_le_bytes();
        for chunk in self.buf.chunks_mut(4) {
            chunk.copy_from_slice(&b);
        }
    }

    pub(crate) fn set_px(&mut self, x: isize, y: isize, rgba: u32) {
        if x < 0 || y < 0 || (x as u32) >= self.width || (y as u32) >= self.height {
            return;
        }
        let b = rgba.to_le_bytes();
        let i = ((y * self.width as isize + x) * 4) as usize;
        self.buf[i..i + 4].copy_from_slice(&b);
    }

    /// Fill a rectangle (axis-aligned).
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, rgba: u32) {
        let x0 = x.max(0) as isize;
        let y0 = y.max(0) as isize;
        let x1 = ((x + w) as i32).min(self.width as i32) as isize;
        let y1 = ((y + h) as i32).min(self.height as i32) as isize;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let b = rgba.to_le_bytes();
        for yy in y0..y1 {
            for xx in x0..x1 {
                let i = ((yy * self.width as isize + xx) * 4) as usize;
                self.buf[i..i + 4].copy_from_slice(&b);
            }
        }
    }

    /// Fill a circle.
    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, rgba: u32) {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx as i64 * dx as i64 + dy as i64 * dy as i64 <= r as i64 * r as i64 {
                    self.set_px(cx as isize + dx as isize, cy as isize + dy as isize, rgba);
                }
            }
        }
    }

    /// Blend one source-over pixel (straight alpha) at `x`,`y`.
    ///
    /// Same straight-alpha formula the host chat compositor uses
    /// (`crates/render` `canvas::blend_px`): `out = src + dst·(1−sa)`.
    /// (Only `text.rs` calls it — allow dead-code when `text` is off.)
    #[allow(dead_code)]
    pub(crate) fn blend_px(&mut self, x: isize, y: isize, src: [u8; 4]) {
        if x < 0 || y < 0 || (x as u32) >= self.width || (y as u32) >= self.height {
            return;
        }
        let sa = src[3] as u32;
        if sa == 0 {
            return;
        }
        let i = ((y * self.width as isize + x) * 4) as usize;
        let da = self.buf[i + 3] as u32;
        let out_a = sa + da * (255 - sa) / 255;
        self.buf[i] = (src[0] as u32 + self.buf[i] as u32 * (255 - sa) / 255) as u8;
        self.buf[i + 1] = (src[1] as u32 + self.buf[i + 1] as u32 * (255 - sa) / 255) as u8;
        self.buf[i + 2] = (src[2] as u32 + self.buf[i + 2] as u32 * (255 - sa) / 255) as u8;
        self.buf[i + 3] = out_a as u8;
    }

    /// Draw text with the 5×7 bitmap font (see [`crate::draw`]).
    ///
    /// `scale` is the pixel size of one font dot (2 → 10×14 px glyphs).
    /// Returns the x of the right edge (so callers can chain).
    pub fn fill_text(&mut self, mut x: i32, y: i32, scale: i32, text: &str, rgba: u32) -> i32 {
        let mut pen = draw::FontPen {
            ctx: self,
            y,
            scale: scale.max(1),
            color: rgba,
        };
        for ch in text.chars() {
            x = draw::draw_char(&mut pen, ch, x);
        }
        x
    }

    /// Blit an RGBA8 (straight alpha) bitmap into the canvas at `(dst_x,
    /// dst_y)`, clipping rows/columns that fall outside the canvas.
    ///
    /// Straight **replace** (not blend) — document plugins blit opaque page
    /// rasters onto the background; `rgba` should be `w * h * 4` bytes,
    /// row-major, stride `w * 4`.
    pub fn blit(&mut self, dst_x: i32, dst_y: i32, w: u32, h: u32, rgba: &[u8]) {
        let w = w as isize;
        let h = h as isize;
        if w <= 0 || h <= 0 || rgba.len() < (w * h * 4) as usize {
            return;
        }
        for row in 0..h {
            let cy = dst_y as isize + row;
            if cy < 0 || cy >= self.height as isize {
                continue;
            }
            let srow = (row * w * 4) as usize;
            let drow = (cy * self.width as isize * 4) as usize;
            for col in 0..w {
                let cx = dst_x as isize + col;
                if cx < 0 || cx >= self.width as isize {
                    continue;
                }
                let si = srow + (col * 4) as usize;
                let di = drow + (cx * 4) as usize;
                self.buf[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
            }
        }
    }

    /// Hand the current buffer to the host as a new frame.
    pub fn commit(&self) {
        let bytes = (self.width as u32) * 4;
        unsafe {
            crate::x_frame_commit(
                self.buf.as_ptr() as u32,
                self.width,
                self.height,
                bytes,
            );
        }
    }
}
