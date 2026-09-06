//! Glyph coverage bitmap cache keyed by (font, glyph, pixel size).

use crate::font::{FontId, GlyphId};

/// A rasterized glyph's coverage bitmap and positioning metadata.
#[derive(Clone, Debug)]
pub struct GlyphBitmap {
    pub coverage: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Left edge of the bitmap relative to the pen origin.
    pub xmin: i32,
    /// Bottom edge of the bitmap relative to the baseline.
    pub ymin: i32,
    /// Advance width in pixels.
    pub advance: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct CacheKey {
    font: FontId,
    glyph: GlyphId,
    /// Quantized pixel size (px * 2, to allow half-pixel steps).
    px2: u32,
}

pub struct GlyphCache {
    map: std::collections::HashMap<CacheKey, GlyphBitmap>,
}

impl GlyphCache {
    pub fn new() -> GlyphCache {
        GlyphCache { map: std::collections::HashMap::new() }
    }

    fn key(font: FontId, glyph: GlyphId, px: f32) -> CacheKey {
        CacheKey { font, glyph, px2: (px * 2.0).round().max(1.0) as u32 }
    }

    /// Rasterizes (or returns a cached copy of) a glyph at the given size.
    pub fn rasterize(
        &mut self,
        font: &fontdue::Font,
        font_id: FontId,
        glyph: GlyphId,
        px: f32,
    ) -> GlyphBitmap {
        let key = Self::key(font_id, glyph, px);
        if let Some(bmp) = self.map.get(&key) {
            return bmp.clone();
        }
        let (metrics, coverage) = font.rasterize_indexed(glyph, px);
        let bmp = GlyphBitmap {
            coverage,
            width: metrics.width,
            height: metrics.height,
            xmin: metrics.xmin,
            ymin: metrics.ymin,
            advance: metrics.advance_width,
        };
        self.map.insert(key, bmp.clone());
        bmp
    }
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new()
    }
}
