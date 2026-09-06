//! Shaping of a single direction+font run via rustybuzz.

use crate::font::{FontId, FontManager, GlyphId};

/// A glyph produced by shaping, before rasterization.
#[derive(Clone, Copy, Debug)]
pub struct ShapedGlyph {
    pub glyph: GlyphId,
    /// Horizontal advance in pixels.
    pub x_advance: f32,
    /// Horizontal offset in pixels (mark positioning).
    pub x_offset: f32,
    /// Vertical offset in pixels (mark positioning).
    pub y_offset: f32,
    /// Byte offset of the originating character within the shaped run.
    pub cluster: u32,
}

/// Shapes a run of text with a single font and direction.
///
/// Advances and offsets are returned in pixels at the given font size.
/// For RTL runs, glyphs are returned in visual order (right-to-left input,
/// left-to-right output), which is what the compositor needs.
pub fn shape_run(
    manager: &FontManager,
    font: FontId,
    text: &str,
    rtl: bool,
    px: f32,
) -> Vec<ShapedGlyph> {
    let entry = manager.get(font);
    let scale = entry.scale_for(px);

    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(text);
    buf.set_direction(if rtl {
        rustybuzz::Direction::RightToLeft
    } else {
        rustybuzz::Direction::LeftToRight
    });
    buf.guess_segment_properties();

    let out = rustybuzz::shape(&entry.face, &[], buf);
    out.glyph_infos()
        .iter()
        .zip(out.glyph_positions().iter())
        .map(|(info, pos)| ShapedGlyph {
            glyph: info.glyph_id as u16,
            x_advance: pos.x_advance as f32 * scale,
            x_offset: pos.x_offset as f32 * scale,
            y_offset: pos.y_offset as f32 * scale,
            cluster: info.cluster,
        })
        .collect()
}
