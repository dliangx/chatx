//! Visual theme for bubbles and text.

use crate::color::Rgba;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bubble_self: Rgba,
    pub bubble_other: Rgba,
    pub text_self: Rgba,
    pub text_other: Rgba,
    pub link_color: Rgba,
    pub mention_color: Rgba,
    pub time_color: Rgba,
    pub sender_color: Rgba,
    pub selection_color: Rgba,
    pub radius: f32,
    pub padding_x: f32,
    pub padding_y: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub time_font_size: f32,
    pub avatar_size: f32,
    /// Max bubble width as a ratio of the available width.
    pub max_bubble_width_ratio: f32,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            bubble_self: Rgba::rgb(0x95, 0xEC, 0x69),
            bubble_other: Rgba::rgb(0xFF, 0xFF, 0xFF),
            text_self: Rgba::rgb(0x00, 0x00, 0x00),
            text_other: Rgba::rgb(0x00, 0x00, 0x00),
            link_color: Rgba::rgb(0x1D, 0x6F, 0xEB),
            mention_color: Rgba::rgb(0x1D, 0x6F, 0xEB),
            time_color: Rgba::rgb(0x99, 0x99, 0x99),
            sender_color: Rgba::rgb(0x55, 0x55, 0x55),
            selection_color: Rgba::from_rgba(0x3B, 0x82, 0xF6, 0x66),
            radius: 12.0,
            padding_x: 12.0,
            padding_y: 8.0,
            font_size: 16.0,
            line_height: 1.4,
            time_font_size: 11.0,
            avatar_size: 36.0,
            max_bubble_width_ratio: 0.72,
        }
    }
}

impl Theme {
    /// Returns a copy with all pixel dimensions scaled by `f` (e.g. a device
    /// pixel ratio). Used to render crisp textures on high-DPI displays.
    pub fn scaled(&self, f: f32) -> Theme {
        Theme {
            radius: self.radius * f,
            padding_x: self.padding_x * f,
            padding_y: self.padding_y * f,
            font_size: self.font_size * f,
            time_font_size: self.time_font_size * f,
            avatar_size: self.avatar_size * f,
            ..*self
        }
    }
}
