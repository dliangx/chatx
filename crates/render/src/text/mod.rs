
pub mod linebreak;
pub mod layout;
pub mod selection;
pub mod shaper;

pub use layout::{layout_rich, layout_text};

use crate::canvas::Image;
use crate::color::Rgba;
use crate::font::{FontId, GlyphId};

#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub font_size: f32,
    pub color: Rgba,
    pub line_height: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Ltr,
    Rtl,
}

#[derive(Clone, Copy, Debug)]
pub struct PositionedGlyph {
    pub font: FontId,
    pub glyph: GlyphId,
    pub pen_x: f32,
    pub advance: f32,
    pub baseline_y: f32,
    pub color: Rgba,
    pub char_index: u32,
}

#[derive(Clone, Debug)]
pub struct PositionedImage {
    pub img: Image,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub char_index: u32,
}

pub struct TextLine {
    pub glyphs: Vec<PositionedGlyph>,
    pub images: Vec<PositionedImage>,
    pub width: f32,
    pub height: f32,
    pub base_dir: Direction,
}

#[derive(Clone, Copy, Debug)]
pub struct Span<'a> {
    pub text: &'a str,
    pub color: Rgba,
}

pub enum RichItem<'a> {
    Text(Span<'a>),
    Emoji(char),
}
