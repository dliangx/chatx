//! Minimal software compositor operating on a premultiplied RGBA8 buffer.
//!
//! Provides the primitives needed to draw chat bubbles:
//! filled rects, rounded rects (per-corner radius), image blits and
//! coverage-mask blits (used to draw fontdue glyphs and triangles).

use crate::color::Rgba;

/// Converts a premultiplied RGBA buffer to straight (non-premultiplied) alpha.
pub fn unpremultiply(buf: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; buf.len()];
    for (dst, px) in out.chunks_exact_mut(4).zip(buf.chunks_exact(4)) {
        let a = px[3] as u32;
        if a == 0 || a == 255 {
            dst.copy_from_slice(px);
        } else {
            dst[0] = (px[0] as u32 * 255 / a).min(255) as u8;
            dst[1] = (px[1] as u32 * 255 / a).min(255) as u8;
            dst[2] = (px[2] as u32 * 255 / a).min(255) as u8;
            dst[3] = px[3];
        }
    }
    out
}

pub struct Canvas {
    buf: Vec<u8>, // RGBA8, premultiplied alpha
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Corners {
    pub tl: f32,
    pub tr: f32,
    pub br: f32,
    pub bl: f32,
}

impl Corners {
    pub const fn all(radius: f32) -> Corners {
        Corners { tl: radius, tr: radius, br: radius, bl: radius }
    }
}

#[derive(Clone, Debug)]
pub struct Image {
    pub rgba: Vec<u8>, // RGBA8, straight alpha (non-premultiplied)
    pub w: u32,
    pub h: u32,
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Canvas {
        Canvas { buf: vec![0; (w * h * 4) as usize], w, h }
    }

    #[inline]
    fn idx(&self, x: u32, y: u32) -> usize {
        (y * self.w + x) as usize * 4
    }

    pub fn into_rgba(self) -> Vec<u8> {
        self.buf
    }

    pub fn fill(&mut self, color: Rgba) {
        let [r, g, b, a] = [color.r, color.g, color.b, color.a];
        for px in self.buf.chunks_exact_mut(4) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = a;
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Rgba) {
        let (x0, y0) = (x.max(0), y.max(0));
        let x1 = (x + w as i32).min(self.w as i32);
        let y1 = (y + h as i32).min(self.h as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                self.blend_px(px as u32, py as u32, color);
            }
        }
    }

    /// Rounded rectangle with independent corner radii.
    pub fn round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        corners: Corners,
        color: Rgba,
    ) {
        let x0 = x.max(0.0).ceil() as i32;
        let y0 = y.max(0.0).ceil() as i32;
        let x1 = (x + w).min(self.w as f32).floor() as i32;
        let y1 = (y + h).min(self.h as f32).floor() as i32;
        let right = x + w;
        let bottom = y + h;
        for py in y0..y1 {
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let fy = py as f32 + 0.5;
                // Nearest corner center + radius, or `None` for the interior.
                let corner = if fx < x + corners.tl && fy < y + corners.tl {
                    Some((x + corners.tl, y + corners.tl, corners.tl))
                } else if fx > right - corners.tr && fy < y + corners.tr {
                    Some((right - corners.tr, y + corners.tr, corners.tr))
                } else if fx > right - corners.br && fy > bottom - corners.br {
                    Some((right - corners.br, bottom - corners.br, corners.br))
                } else if fx < x + corners.bl && fy > bottom - corners.bl {
                    Some((x + corners.bl, bottom - corners.bl, corners.bl))
                } else {
                    None
                };
                match corner {
                    None => self.blend_px(px as u32, py as u32, color),
                    Some((cx, cy, r)) => {
                        let dx = fx - cx;
                        let dy = fy - cy;
                        if dx * dx + dy * dy <= r * r {
                            self.blend_px(px as u32, py as u32, color);
                        }
                    }
                }
            }
        }
    }

    /// Blit a straight-alpha image with alpha blending.
    pub fn blit(&mut self, dst_x: i32, dst_y: i32, img: &Image) {
        for sy in 0..img.h as i32 {
            for sx in 0..img.w as i32 {
                let dx = dst_x + sx;
                let dy = dst_y + sy;
                if dx < 0 || dy < 0 || dx >= self.w as i32 || dy >= self.h as i32 {
                    continue;
                }
                let si = (sy * img.w as i32 + sx) as usize * 4;
                let a = img.rgba[si + 3] as u32;
                if a == 0 {
                    continue;
                }
                // straight -> premultiplied source
                let src = if a == 255 {
                    Rgba {
                        r: img.rgba[si],
                        g: img.rgba[si + 1],
                        b: img.rgba[si + 2],
                        a: 255,
                    }
                } else {
                    Rgba {
                        r: ((img.rgba[si] as u32 * a + 127) / 255) as u8,
                        g: ((img.rgba[si + 1] as u32 * a + 127) / 255) as u8,
                        b: ((img.rgba[si + 2] as u32 * a + 127) / 255) as u8,
                        a: a as u8,
                    }
                };
                self.blend_px(dx as u32, dy as u32, src);
            }
        }
    }

    /// Blit a single-channel coverage bitmap tinted with `color`.
    /// Used for fontdue glyphs (coverage = alpha).
    pub fn blit_mask(
        &mut self,
        dst_x: i32,
        dst_y: i32,
        coverage: &[u8],
        cw: u32,
        ch: u32,
        color: Rgba,
    ) {
        for sy in 0..ch as i32 {
            for sx in 0..cw as i32 {
                let dx = dst_x + sx;
                let dy = dst_y + sy;
                if dx < 0 || dy < 0 || dx >= self.w as i32 || dy >= self.h as i32 {
                    continue;
                }
                let cov = coverage[(sy * cw as i32 + sx) as usize];
                if cov == 0 {
                    continue;
                }
                let src = if cov == 255 {
                    color
                } else {
                    Rgba {
                        r: (color.r as u32 * cov as u32 / 255) as u8,
                        g: (color.g as u32 * cov as u32 / 255) as u8,
                        b: (color.b as u32 * cov as u32 / 255) as u8,
                        a: (color.a as u32 * cov as u32 / 255) as u8,
                    }
                };
                self.blend_px(dx as u32, dy as u32, src);
            }
        }
    }

    /// Blit an image scaled (nearest-neighbor) to `w` x `h`.
    pub fn blit_scaled(&mut self, dst_x: i32, dst_y: i32, w: u32, h: u32, img: &Image) {
        if w == 0 || h == 0 {
            return;
        }
        for sy in 0..h as i32 {
            let src_y = (sy as u64 * img.h as u64 / h as u64) as u32;
            for sx in 0..w as i32 {
                let dx = dst_x + sx;
                let dy = dst_y + sy;
                if dx < 0 || dy < 0 || dx >= self.w as i32 || dy >= self.h as i32 {
                    continue;
                }
                let src_x = (sx as u64 * img.w as u64 / w as u64) as u32;
                let si = (src_y * img.w + src_x) as usize * 4;
                let a = img.rgba[si + 3] as u32;
                if a == 0 {
                    continue;
                }
                let src = if a == 255 {
                    Rgba { r: img.rgba[si], g: img.rgba[si + 1], b: img.rgba[si + 2], a: 255 }
                } else {
                    Rgba {
                        r: ((img.rgba[si] as u32 * a + 127) / 255) as u8,
                        g: ((img.rgba[si + 1] as u32 * a + 127) / 255) as u8,
                        b: ((img.rgba[si + 2] as u32 * a + 127) / 255) as u8,
                        a: a as u8,
                    }
                };
                self.blend_px(dx as u32, dy as u32, src);
            }
        }
    }

    /// Fills a circle centered at (cx, cy) with radius `r`.
    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgba) {
        let x0 = (cx - r).ceil() as i32;
        let y0 = (cy - r).ceil() as i32;
        let x1 = (cx + r).floor() as i32;
        let y1 = (cy + r).floor() as i32;
        for py in y0..=y1 {
            for px in x0..=x1 {
                if px < 0 || py < 0 || px >= self.w as i32 || py >= self.h as i32 {
                    continue;
                }
                let dx = px as f32 + 0.5 - cx;
                let dy = py as f32 + 0.5 - cy;
                if dx * dx + dy * dy <= r * r {
                    self.blend_px(px as u32, py as u32, color);
                }
            }
        }
    }

    /// Source-over blend of a premultiplied `src` onto a pixel.
    #[inline]
    fn blend_px(&mut self, x: u32, y: u32, src: Rgba) {
        let i = self.idx(x, y);
        let sa = src.a as u32;
        let da = self.buf[i + 3] as u32;
        // out = src + dst * (1 - src_alpha), premultiplied
        let out_a = sa + da * (255 - sa) / 255;
        if out_a == 0 {
            self.buf[i..i + 4].fill(0);
            return;
        }
        let dr = self.buf[i] as u32;
        let dg = self.buf[i + 1] as u32;
        let db = self.buf[i + 2] as u32;
        self.buf[i] = (src.r as u32 + dr * (255 - sa) / 255) as u8;
        self.buf[i + 1] = (src.g as u32 + dg * (255 - sa) / 255) as u8;
        self.buf[i + 2] = (src.b as u32 + db * (255 - sa) / 255) as u8;
        self.buf[i + 3] = out_a as u8;
    }
}


