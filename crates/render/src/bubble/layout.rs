//! Bubble content layout: inline text/emoji + block images.

use crate::bubble::{Bubble, Segment};
use crate::canvas::Image;
use crate::emoji::EmojiAtlas;
use crate::font::FontManager;
use crate::text::{layout_rich, RichItem, Span, TextLine, TextStyle};
use crate::theme::Theme;

/// Laid-out bubble content (relative geometry; positions are content-local).
pub struct ContentLayout {
    pub lines: Vec<TextLine>,
    pub images: Vec<BlockImage>,
    pub content_w: f32,
    pub content_h: f32,
}

pub struct BlockImage {
    pub img: Image,
    pub w: f32,
    pub h: f32,
}

pub fn layout_content(
    manager: &FontManager,
    emoji: &EmojiAtlas,
    bubble: &Bubble,
    theme: &Theme,
    max_content_w: f32,
) -> ContentLayout {
    let style = TextStyle {
        font_size: theme.font_size,
        color: text_color(bubble, theme),
        line_height: theme.line_height,
    };

    let mut items: Vec<RichItem> = Vec::new();
    let mut images: Vec<BlockImage> = Vec::new();

    for seg in bubble.segments {
        match seg {
            Segment::Text(s) => items.push(RichItem::Text(Span { text: s, color: style.color })),
            Segment::Mention(s) => items.push(RichItem::Text(Span { text: s, color: theme.mention_color })),
            Segment::Link(s) => items.push(RichItem::Text(Span { text: s, color: theme.link_color })),
            Segment::Emoji(c) => items.push(RichItem::Emoji(*c)),
            Segment::Image(_) => {}
        }
    }

    let lines = layout_rich(manager, emoji, &items, &style, max_content_w);

    for seg in bubble.segments {
        if let Segment::Image(img) = seg {
            images.push(scale_image(img, max_content_w));
        }
    }

    let mut content_w = 0.0f32;
    let mut content_h = 0.0f32;
    for line in &lines {
        content_w = content_w.max(line.width);
        content_h += line.height;
    }
    for img in &images {
        content_w = content_w.max(img.w);
        content_h += img.h + GAP;
    }

    ContentLayout { lines, images, content_w, content_h }
}

const GAP: f32 = 6.0;

fn scale_image(img: &Image, max_w: f32) -> BlockImage {
    let nw = img.w as f32;
    let nh = img.h as f32;
    if nw <= max_w {
        return BlockImage { img: img.clone(), w: nw, h: nh };
    }
    let scale = max_w / nw;
    BlockImage { img: img.clone(), w: max_w, h: nh * scale }
}

fn text_color(bubble: &Bubble, theme: &Theme) -> crate::color::Rgba {
    match bubble.side {
        crate::bubble::Side::SelfSide => theme.text_self,
        crate::bubble::Side::Other => theme.text_other,
    }
}
