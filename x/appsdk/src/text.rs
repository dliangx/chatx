//! Single-line, per-character text rasterization (P1, feature `text`).
//!
//! The same `fontdue` (0.9) the host chat compositor runs, so glyph metrics
//! and coverage behaviour match the host UI. No layout/shaping engine on
//! purpose: document plugins draw already-paginated content (page bitmaps
//! from the parser of choice) and only need HUD text, headers and labels —
//! one line at a time, per-character advance, first-font-that-has-the-glyph
//! fallback.
//!
//! Fonts are raw bytes the plugin passes in — wasm32 has no system font
//! directory to enumerate.

use fontdue::{Font, FontSettings};

use crate::Ctx;

/// Ordered fallback list: for each character the first font (in list order)
/// that actually contains the glyph wins.
pub struct Text {
    fonts: Vec<Font>,
}

impl Text {
    /// Build from raw font bytes (TTF / OTF), first = highest priority.
    /// Byte slices that do not parse are skipped, so a bad optional font
    /// never kills the list.
    pub fn new(bytes: &[&[u8]]) -> Text {
        let fonts = bytes
            .iter()
            .filter_map(|b| Font::from_bytes(*b, FontSettings::default()).ok())
            .collect();
        Text { fonts }
    }

    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    fn font_for(&self, ch: char) -> Option<&Font> {
        self.fonts.iter().find(|f| f.has_glyph(ch))
    }

    fn advance_of(&self, ch: char, px: f32) -> f32 {
        self.font_for(ch).map_or(0.0, |f| f.metrics(ch, px).advance_width)
    }

    /// Single-line width in px (per-character advance; no ligatures, no wrap).
    pub fn width(&self, text: &str, px: f32) -> f32 {
        text.chars().map(|c| self.advance_of(c, px)).sum()
    }

    /// Advance width (px) of one character in the fallback list (0 when no
    /// font covers it).
    pub fn advance(&self, ch: char, px: f32) -> f32 {
        self.advance_of(ch, px)
    }

    /// Line box for a nominal font size: first font's line metrics, else 1.2×.
    pub fn line_height(&self, px: f32) -> f32 {
        self.fonts
            .first()
            .and_then(|f| f.horizontal_line_metrics(px))
            .map_or(px * 1.2, |m| m.new_line_size)
    }

    /// Draw one line with the **baseline** at `y` (canvas px), starting at
    /// `x`. Characters no font covers are dropped (zero advance).
    ///
    /// Returns the x of the right edge.
    pub fn draw(
        &self,
        ctx: &mut Ctx,
        x: i32,
        baseline: i32,
        text: &str,
        px: f32,
        rgba: u32,
    ) -> i32 {
        let b = rgba.to_le_bytes();
        let (r, g, bl, a) = (b[0], b[1], b[2], b[3]);
        if a == 0 || self.fonts.is_empty() {
            return x;
        }
        let mut x = x as f32;
        for ch in text.chars() {
            let Some(font) = self.font_for(ch) else {
                continue;
            };
            let (m, coverage) = font.rasterize(ch, px);
            // Same placement formula as the host compositor: bitmap left at
            // `x + xmin`, top at `baseline − ymin − height` (canvas y-down).
            let left = (x + m.xmin as f32).round() as i32;
            let top = (baseline as f32 - m.ymin as f32 - m.height as f32).round() as i32;
            let stride = m.width as usize;
            for sy in 0..m.height as i32 {
                for sx in 0..m.width as i32 {
                    let c = coverage[sy as usize * stride + sx as usize];
                    if c == 0 {
                        continue;
                    }
                    let sa = (a as u32 * c as u32 / 255) as u8;
                    ctx.blend_px(
                        left as isize + sx as isize,
                        top as isize + sy as isize,
                        [r, g, bl, sa],
                    );
                }
            }
            x += m.advance_width;
        }
        x.round() as i32
    }
}
