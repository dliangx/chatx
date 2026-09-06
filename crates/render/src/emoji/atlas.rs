//! Emoji atlas.
//!
//! Maps emoji characters to pre-rasterized `Image`s. Color emoji require a
//! CBDT/sbix bitmap source (e.g. Noto Color Emoji); P0 falls back to a
//! monochrome glyph rasterized with fontdue or a placeholder box.

use crate::canvas::Image;
use crate::color::Rgba;
use crate::font::{FontManager, GlyphId};
use std::collections::HashMap;

pub struct EmojiAtlas {
    images: HashMap<char, Image>,
}

impl EmojiAtlas {
    pub fn new() -> EmojiAtlas {
        EmojiAtlas { images: HashMap::new() }
    }

    pub fn insert(&mut self, ch: char, image: Image) {
        self.images.insert(ch, image);
    }

    pub fn get(&self, ch: char) -> Option<&Image> {
        self.images.get(&ch)
    }

    pub fn contains(&self, ch: char) -> bool {
        self.images.contains_key(&ch)
    }

    /// Rasterizes emoji codepoints as monochrome glyphs using a font that
    /// contains them (e.g. a monochrome symbol font). Color is tinted.
    pub fn add_monochrome(
        &mut self,
        manager: &FontManager,
        font: crate::font::FontId,
        chars: &[char],
        px: f32,
        color: Rgba,
    ) {
        for &ch in chars {
            let glyph: GlyphId = manager.get(font).font.lookup_glyph_index(ch);
            if glyph == 0 {
                continue;
            }
            let (metrics, coverage) = manager.get(font).font.rasterize_indexed(glyph, px);
            if metrics.width == 0 || metrics.height == 0 {
                continue;
            }
            let mut image = Image {
                rgba: vec![0; metrics.width * metrics.height * 4],
                w: metrics.width as u32,
                h: metrics.height as u32,
            };
            for (i, &cov) in coverage.iter().enumerate() {
                let a = cov;
                image.rgba[i * 4] = color.r;
                image.rgba[i * 4 + 1] = color.g;
                image.rgba[i * 4 + 2] = color.b;
                image.rgba[i * 4 + 3] = a;
            }
            self.insert(ch, image);
        }
    }

    /// Loads color emoji from a bitmap-emoji font (e.g. Apple Color Emoji's
    /// sbix table) at the given pixel size.
    pub fn add_color_font(&mut self, face: &ttf_parser::Face, chars: &[char], px: f32) {
        for &ch in chars {
            let Some(gid) = face.glyph_index(ch) else { continue };
            if gid.0 == 0 {
                continue;
            }
            let Some(img) = face.glyph_raster_image(gid, px.round() as u16) else { continue };
            if img.format != ttf_parser::RasterImageFormat::PNG {
                continue;
            }
            if let Some((rgba, w, h)) = decode_png(img.data) {
                self.insert(ch, Image { rgba, w, h });
            }
        }
    }

    /// Convenience wrapper over [`add_color_font`] taking raw font bytes.
    pub fn add_color_font_bytes(
        &mut self,
        bytes: &[u8],
        face_index: u32,
        chars: &[char],
        px: f32,
    ) {
        if let Ok(face) = ttf_parser::Face::parse(bytes, face_index) {
            self.add_color_font(&face, chars, px);
        }
    }
}

impl Default for EmojiAtlas {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes a PNG into straight-alpha RGBA8.
fn decode_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (ct, _) = reader.output_color_type();
    let w = info.width;
    let h = info.height;
    let rgba = match ct {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
            for i in 0..(w as usize) * (h as usize) {
                out[i * 4..i * 4 + 3].copy_from_slice(&buf[i * 3..i * 3 + 3]);
                out[i * 4 + 3] = 255;
            }
            out
        }
        _ => return None,
    };
    Some((rgba, w, h))
}

/// A fallback placeholder image (rounded square) for emoji with no bitmap.
pub fn placeholder(px: f32) -> Image {
    let size = px.ceil().max(1.0) as u32;
    let mut image = Image {
        rgba: vec![0; (size * size * 4) as usize],
        w: size,
        h: size,
    };
    let radius = (size as f32 * 0.3) as u32;
    for y in 0..size {
        for x in 0..size {
            let inside = rounded(x, y, size, radius);
            if inside {
                let i = (y * size + x) as usize * 4;
                image.rgba[i] = 200;
                image.rgba[i + 1] = 200;
                image.rgba[i + 2] = 200;
                image.rgba[i + 3] = 255;
            }
        }
    }
    image
}

fn rounded(x: u32, y: u32, size: u32, r: u32) -> bool {
    let (fx, fy) = (x as f32, y as f32);
    let (cx, cy, rad) = if fx < r as f32 && fy < r as f32 {
        (r as f32, r as f32, r as f32)
    } else if fx > (size - r) as f32 && fy < r as f32 {
        ((size - r) as f32, r as f32, r as f32)
    } else if fx > (size - r) as f32 && fy > (size - r) as f32 {
        ((size - r) as f32, (size - r) as f32, r as f32)
    } else if fx < r as f32 && fy > (size - r) as f32 {
        (r as f32, (size - r) as f32, r as f32)
    } else {
        return true;
    };
    let dx = fx - cx;
    let dy = fy - cy;
    dx * dx + dy * dy <= rad * rad
}
