
use crate::color::Rgba;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bubble_self: Rgba,
    pub bubble_other: Rgba,
    pub text_self: Rgba,
    pub text_other: Rgba,
    pub link_self: Rgba,
    pub link_other: Rgba,
    pub mention_self: Rgba,
    pub mention_other: Rgba,
    pub time_self: Rgba,
    pub time_other: Rgba,
    pub sender_color: Rgba,
    pub selection_color: Rgba,
    pub radius: f32,
    pub padding_x: f32,
    pub padding_y: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub time_font_size: f32,
    pub avatar_size: f32,
    pub max_bubble_width_ratio: f32,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            bubble_self: Rgba::rgb(0x7D, 0x54, 0xCD),
            bubble_other: Rgba::rgb(0xFF, 0xFF, 0xFF),
            text_self: Rgba::rgb(0xFF, 0xFF, 0xFF),
            text_other: Rgba::rgb(0x00, 0x00, 0x00),
            link_self: Rgba::rgb(0xFF, 0xFF, 0xFF),
            link_other: Rgba::rgb(0x1D, 0x6F, 0xEB),
            mention_self: Rgba::rgb(0xFF, 0xFF, 0xFF),
            mention_other: Rgba::rgb(0x1D, 0x6F, 0xEB),
            time_self: Rgba::rgb(0xCF, 0xC5, 0xEA),
            time_other: Rgba::rgb(0x99, 0x99, 0x99),
            sender_color: Rgba::rgb(0x55, 0x55, 0x55),
            selection_color: Rgba::from_rgba(0x3B, 0x82, 0xF6, 0x66),
            radius: 12.0,
            padding_x: 12.0,
            padding_y: 8.0,
            font_size: 14.0,
            line_height: 1.4,
            time_font_size: 11.0,
            avatar_size: 36.0,
            max_bubble_width_ratio: 0.72,
        }
    }
}

impl Theme {
    /// Dark-scheme chat bubble palette. Self bubbles use the brand purple bg
    /// with white text (consistent with light mode); the other side gets an
    /// elevated slate surface clearly brighter than the dark chat background so
    /// it stays visible (the light-mode analog is white on light grey).
    pub fn dark() -> Theme {
        Theme {
            bubble_self: Rgba::rgb(0x7D, 0x54, 0xCD),
            bubble_other: Rgba::rgb(0x46, 0x4D, 0x60),
            text_self: Rgba::rgb(0xFF, 0xFF, 0xFF),
            text_other: Rgba::rgb(0xF2, 0xF3, 0xF7),
            time_self: Rgba::rgb(0xCF, 0xC5, 0xEA),
            time_other: Rgba::rgb(0xB4, 0xBE, 0xD0),
            sender_color: Rgba::rgb(0xD2, 0xD8, 0xE4),
            ..Default::default()
        }
    }

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
