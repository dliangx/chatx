//! Text shaping, bidi, line breaking and layout.

pub mod linebreak;
pub mod layout;
pub mod selection;
pub mod shaper;

pub use layout::{layout_rich, layout_text};

use crate::canvas::Image;
use crate::color::Rgba;
use crate::font::{FontId, GlyphId};

/// Styling for a run of text.
#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub font_size: f32,
    pub color: Rgba,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Ltr,
    Rtl,
}

/// A glyph positioned in a line, ready to rasterize and blit.
#[derive(Clone, Copy, Debug)]
pub struct PositionedGlyph {
    pub font: FontId,
    pub glyph: GlyphId,
    /// X position of the pen (baseline origin) relative to the line left edge.
    pub pen_x: f32,
    /// Horizontal advance of this glyph in pixels.
    pub advance: f32,
    /// Y position of the baseline relative to the line top edge.
    pub baseline_y: f32,
    pub color: Rgba,
    /// Character index (in the source combined text) this glyph originated from.
    pub char_index: u32,
}

/// An inline image (emoji) positioned within a line.
#[derive(Clone, Debug)]
pub struct PositionedImage {
    pub img: Image,
    /// Left edge relative to the line left edge.
    pub x: f32,
    /// Top edge relative to the line top edge.
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Character index (in the source combined text) of this image.
    pub char_index: u32,
}

/// A single laid-out line of text, glyphs and images in visual order.
pub struct TextLine {
    pub glyphs: Vec<PositionedGlyph>,
    pub images: Vec<PositionedImage>,
    pub width: f32,
    pub height: f32,
    pub base_dir: Direction,
}

/// A styled span of text.
#[derive(Clone, Copy, Debug)]
pub struct Span<'a> {
    pub text: &'a str,
    pub color: Rgba,
}

/// A rich inline item: either styled text or an inline emoji.
pub enum RichItem<'a> {
    Text(Span<'a>),
    Emoji(char),
}
