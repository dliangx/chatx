
use crate::font::{FontId, GlyphId};

#[derive(Clone, Debug)]
pub struct GlyphBitmap {
    pub coverage: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub xmin: i32,
    pub ymin: i32,
    pub advance: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct CacheKey {
    font: FontId,
    glyph: GlyphId,
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
