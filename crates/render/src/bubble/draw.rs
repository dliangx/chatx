//! Bubble rendering: chrome (avatar/name/body/tail) + content, into RGBA.

use crate::bubble::layout::{layout_content, ContentLayout};
use crate::bubble::{Bubble, GroupPos, RenderedBubble, Side};
use crate::canvas::{Canvas, Corners};
use crate::color::Rgba;
use crate::emoji::EmojiAtlas;
use crate::font::FontManager;
use crate::text::{layout_text, TextLine, TextStyle};
use crate::theme::Theme;

const GAP: f32 = 14.0;
const TAIL_W: f32 = 10.0;
const TAIL_H: f32 = 18.0;

/// Geometry computed for a bubble, shared between rendering and hit-testing.
pub struct Frame {
    pub theme: Theme, // scaled
    pub content: ContentLayout,
    pub is_self: bool,
    pub show_avatar: bool,
    pub show_name: bool,
    pub show_tail: bool,
    pub avatar_size: f32,
    pub avatar_x: f32,
    pub body: (f32, f32, f32, f32), // (x, y, w, h)
    pub content_origin: (f32, f32),
    pub name_lines: Vec<TextLine>,
    pub name_style: TextStyle,
    pub time_lines: Vec<TextLine>,
    pub time_style: TextStyle,
    pub time_x: f32,
    pub time_y: f32,
    pub size: (u32, u32),
}

/// Computes the geometry and content layout for a bubble.
pub fn compute_frame(
    manager: &FontManager,
    emoji: &EmojiAtlas,
    bubble: &Bubble,
    theme: &Theme,
    available_w: f32,
    scale: f32,
) -> Frame {
    let theme = theme.scaled(scale);
    let available_w = available_w * scale;

    let is_self = bubble.side == Side::SelfSide;
    let show_avatar = bubble.avatar.is_some()
        && (bubble.group == GroupPos::First || bubble.group == GroupPos::Single);
    let show_name = show_avatar;
    let show_tail = show_avatar;

    let avatar_size = theme.avatar_size;

    let max_body_w = (available_w * theme.max_bubble_width_ratio).max(40.0);
    let max_content_w = (max_body_w - theme.padding_x * 2.0).max(10.0);

    let content = layout_content(manager, emoji, bubble, &theme, max_content_w);

    let time_style = TextStyle {
        font_size: theme.time_font_size,
        color: theme.time_color,
        line_height: theme.line_height,
    };
    let time_lines = layout_text(manager, bubble.time, &time_style, max_content_w);
    let time_w = time_lines.iter().map(|l| l.width).fold(0.0f32, f32::max);
    let time_h = time_lines.iter().map(|l| l.height).fold(0.0f32, f32::max);

    let name_style = TextStyle {
        font_size: theme.font_size * 0.85,
        color: theme.sender_color,
        line_height: theme.line_height,
    };
    let name_lines = if show_name {
        layout_text(manager, bubble.sender, &name_style, max_content_w)
    } else {
        Vec::new()
    };
    let name_h = name_lines.iter().map(|l| l.height).fold(0.0f32, f32::max);

    let time_gap = if content.content_h > 0.0 { 4.0 } else { 0.0 };
    let body_w = content.content_w.max(time_w) + theme.padding_x * 2.0;
    let body_h = content.content_h + time_gap + time_h + theme.padding_y * 2.0;

    let avatar_col = if show_avatar { avatar_size + GAP } else { 0.0 };
    let body_y = name_h + if show_name { 4.0 } else { 0.0 };

    // Avatar sits on the left for "other" messages and on the right for "self"
    // messages (mirroring the bubble side).
    let (body_x, avatar_x, total_w) = if is_self {
        let body_x = 0.0;
        let avatar_x = body_w + TAIL_W + if show_avatar { GAP } else { 0.0 };
        let total_w = body_w + TAIL_W + avatar_col;
        (body_x, avatar_x, total_w)
    } else {
        let body_x = avatar_col;
        let avatar_x = 0.0;
        let total_w = avatar_col + body_w;
        (body_x, avatar_x, total_w)
    };

    let total_h = (body_y + body_h).ceil() as u32;

    let content_x = body_x + theme.padding_x;
    let content_y = body_y + theme.padding_y;

    let time_x = body_x + body_w - theme.padding_x - time_w;
    let time_y = body_y + body_h - theme.padding_y - time_h;

    Frame {
        theme,
        content,
        is_self,
        show_avatar,
        show_name,
        show_tail,
        avatar_size,
        avatar_x,
        body: (body_x, body_y, body_w, body_h),
        content_origin: (content_x, content_y),
        name_lines,
        name_style,
        time_lines,
        time_style,
        time_x,
        time_y,
        size: (total_w.ceil() as u32, total_h),
    }
}

pub fn render_bubble(
    manager: &mut FontManager,
    emoji: &EmojiAtlas,
    bubble: &Bubble,
    theme: &Theme,
    available_w: f32,
    scale: f32,
) -> RenderedBubble {
    render_bubble_with_selection(manager, emoji, bubble, theme, available_w, scale, None)
}

/// Renders a bubble with an optional text-selection highlight.
///
/// `selection` is a half-open character range `[start, end)` in the combined
/// text index space (see `text::layout`). `None` renders without a highlight.
pub fn render_bubble_with_selection(
    manager: &mut FontManager,
    emoji: &EmojiAtlas,
    bubble: &Bubble,
    theme: &Theme,
    available_w: f32,
    scale: f32,
    selection: Option<(u32, u32)>,
) -> RenderedBubble {
    let frame = compute_frame(manager, emoji, bubble, theme, available_w, scale);

    let (body_x, body_y, body_w, body_h) = frame.body;
    let (total_w, total_h) = frame.size;
    let (content_x, content_y) = frame.content_origin;

    let mut canvas = Canvas::new(total_w, total_h);

    // Avatar (left for "other", right for "self"), bottom-aligned with body.
    if frame.show_avatar {
        if let Some(img) = bubble.avatar {
            let ax = frame.avatar_x;
            let ay = body_y + body_h - frame.avatar_size;
            canvas.blit_scaled(ax as i32, ay as i32, frame.avatar_size as u32, frame.avatar_size as u32, img);
        }
    }

    // Sender name.
    if frame.show_name {
        draw_lines(&mut canvas, manager, &frame.name_lines, body_x, 0.0, frame.name_style.font_size);
    }

    // Body.
    let body_color = if frame.is_self { frame.theme.bubble_self } else { frame.theme.bubble_other };
    canvas.round_rect(body_x, body_y, body_w, body_h, Corners::all(frame.theme.radius), body_color);
    if frame.show_tail {
        draw_tail(&mut canvas, body_x, body_y, body_w, body_h, TAIL_W, frame.is_self, body_color);
    }

    // Selection highlight (under glyphs).
    if let Some((start, end)) = selection {
        let rects = crate::text::selection::selection_rects(&frame.content.lines, start, end);
        for r in rects {
            canvas.fill_rect(
                (content_x + r.x).round() as i32,
                (content_y + r.y).round() as i32,
                r.w.ceil().max(1.0) as u32,
                r.h.ceil().max(1.0) as u32,
                frame.theme.selection_color,
            );
        }
    }

    // Content.
    let mut content_y = content_y;
    draw_lines(&mut canvas, manager, &frame.content.lines, content_x, content_y, frame.theme.font_size);
    content_y += frame.content.content_h;
    for img in &frame.content.images {
        canvas.blit_scaled(content_x as i32, content_y as i32, img.w as u32, img.h as u32, &img.img);
        content_y += img.h + 6.0;
    }

    // Time (right-aligned within body).
    draw_lines(&mut canvas, manager, &frame.time_lines, frame.time_x, frame.time_y, frame.time_style.font_size);

    RenderedBubble { rgba: canvas.into_rgba(), width: total_w, height: total_h }
}

fn draw_lines(
    canvas: &mut Canvas,
    manager: &mut FontManager,
    lines: &[TextLine],
    x: f32,
    mut y: f32,
    px: f32,
) {
    for line in lines {
        draw_line(canvas, manager, line, x, y, px);
        y += line.height;
    }
}

fn draw_line(canvas: &mut Canvas, manager: &mut FontManager, line: &TextLine, x: f32, y: f32, px: f32) {
    for img in &line.images {
        canvas.blit_scaled((x + img.x) as i32, (y + img.y) as i32, img.w as u32, img.h as u32, &img.img);
    }
    let FontManager { entries, cache, .. } = manager;
    for g in &line.glyphs {
        let font = &entries[g.font.0 as usize].font;
        let bmp = cache.rasterize(font, g.font, g.glyph, px);
        let top_x = (x + g.pen_x + bmp.xmin as f32).round() as i32;
        let top_y = (y + g.baseline_y - bmp.ymin as f32 - bmp.height as f32).round() as i32;
        canvas.blit_mask(top_x, top_y, &bmp.coverage, bmp.width as u32, bmp.height as u32, g.color);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tail(
    canvas: &mut Canvas,
    body_x: f32,
    body_y: f32,
    body_w: f32,
    body_h: f32,
    tail_w: f32,
    is_self: bool,
    color: Rgba,
) {
    // Tail base sits on the straight part of the body edge (above the rounded
    // bottom corner) so it reads as attached rather than detached.
    let y1 = body_y + body_h - 16.0;
    let y0 = y1 - TAIL_H;
    if is_self {
        let x = body_x + body_w;
        fill_triangle(canvas, (x, y0), (x, y1), (x + tail_w, (y0 + y1) / 2.0), color);
    } else {
        let x = body_x;
        fill_triangle(canvas, (x, y0), (x, y1), (x - tail_w, (y0 + y1) / 2.0), color);
    }
}

/// Fills a triangle using an edge-function (barycentric sign) test.
fn fill_triangle(canvas: &mut Canvas, a: (f32, f32), b: (f32, f32), c: (f32, f32), color: Rgba) {
    let min_x = a.0.min(b.0).min(c.0).floor() as i32;
    let max_x = a.0.max(b.0).max(c.0).ceil() as i32;
    let min_y = a.1.min(b.1).min(c.1).floor() as i32;
    let max_y = a.1.max(b.1).max(c.1).ceil() as i32;
    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let p = (px as f32 + 0.5, py as f32 + 0.5);
            if point_in_triangle(p, a, b, c) {
                canvas.blit_mask(px, py, &[255], 1, 1, color);
            }
        }
    }
}

fn point_in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let s1 = cross(b, c, p);
    let s2 = cross(c, a, p);
    let s3 = cross(a, b, p);
    (s1 >= 0.0 && s2 >= 0.0 && s3 >= 0.0) || (s1 <= 0.0 && s2 <= 0.0 && s3 <= 0.0)
}

fn cross(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}
