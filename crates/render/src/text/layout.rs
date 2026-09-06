//! High-level rich text layout: spans + inline emoji -> wrapped lines ->
//! positioned glyphs and images (in visual order, bidi-aware).

use crate::emoji::atlas::placeholder;
use crate::emoji::EmojiAtlas;
use crate::font::{FontId, FontManager};
use crate::text::linebreak::{break_opportunities, char_levels, visual_order};
use crate::text::shaper::shape_run;
use crate::text::{Direction, PositionedGlyph, PositionedImage, RichItem, TextLine, TextStyle};
use crate::color::Rgba;
use unicode_bidi::Level;

/// Placeholder used to mark an inline emoji inside the combined bidi string.
const EMOJI_PLACEHOLDER: char = '\u{E000}';

/// Metadata for one character (in logical order) of the combined string.
enum CharMeta {
    Text { color: Rgba },
    Emoji { ch: char },
}

struct CharInfo {
    byte: usize,
    index: u32,
    ch: char,
    rtl: bool,
    font: FontId,
    color: Rgba,
    emoji: Option<char>,
}

/// Lays out plain text (no emoji, single color) into visual lines.
pub fn layout_text(
    manager: &FontManager,
    text: &str,
    style: &TextStyle,
    max_width: f32,
) -> Vec<TextLine> {
    let items = [RichItem::Text(crate::text::Span { text, color: style.color })];
    layout_rich(manager, &EmojiAtlas::new(), &items, style, max_width)
}

/// Lays out rich inline content (styled spans + emoji) into visual lines.
pub fn layout_rich(
    manager: &FontManager,
    emoji: &EmojiAtlas,
    items: &[RichItem],
    style: &TextStyle,
    max_width: f32,
) -> Vec<TextLine> {
    let (combined, meta) = build_combined(items);
    if combined.is_empty() {
        return Vec::new();
    }

    let levels = char_levels(&combined);
    let chars: Vec<CharInfo> = combined
        .char_indices()
        .zip(levels.iter().copied())
        .zip(meta.iter())
        .enumerate()
        .map(|(index, (((byte, ch), level), m))| {
            let rtl = level.is_rtl();
            let (font, color, emoji) = match m {
                CharMeta::Text { color } => (manager.resolve(ch, rtl), *color, None),
                CharMeta::Emoji { ch: e } => (FontId(0), Rgba::TRANSPARENT, Some(*e)),
            };
            CharInfo { byte, index: index as u32, ch, rtl, font, color, emoji }
        })
        .collect();

    let base_rtl = matches!(
        unicode_bidi::get_base_direction(combined.as_str()),
        unicode_bidi::Direction::Rtl
    );

    let ranges = line_ranges(manager, &combined, &chars, style, max_width);
    ranges
        .into_iter()
        .map(|(a, b)| layout_line(manager, emoji, &chars, a, b, style, base_rtl))
        .collect()
}

/// Builds a combined string (emoji -> placeholder) plus per-char metadata.
fn build_combined(items: &[RichItem]) -> (String, Vec<CharMeta>) {
    let mut combined = String::new();
    let mut meta = Vec::new();
    for item in items {
        match item {
            RichItem::Text(span) => {
                for ch in span.text.chars() {
                    combined.push(ch);
                    meta.push(CharMeta::Text { color: span.color });
                }
            }
            RichItem::Emoji(ch) => {
                combined.push(EMOJI_PLACEHOLDER);
                meta.push(CharMeta::Emoji { ch: *ch });
            }
        }
    }
    (combined, meta)
}

/// Computes the line height and baseline offset (from line top) for a line.
fn line_metrics(manager: &FontManager, style: &TextStyle, rtl: bool) -> (f32, f32) {
    let size = style.font_size;
    let mut baseline = size;
    let mut height = size * style.line_height;
    if let Some(lm) = manager
        .primary(rtl)
        .and_then(|id| manager.get(id).font.horizontal_line_metrics(size))
    {
        baseline = lm.ascent;
        height = height.max(lm.new_line_size);
    }
    (height, baseline)
}

/// Greedy line breaking into byte ranges (logical order).
fn line_ranges(
    manager: &FontManager,
    para: &str,
    chars: &[CharInfo],
    style: &TextStyle,
    max_width: f32,
) -> Vec<(usize, usize)> {
    let breaks = break_opportunities(para);
    let mut lines = Vec::new();
    let mut line_start = 0usize;
    let mut last_allowed: Option<usize> = None;

    for &(bi, mandatory) in &breaks {
        if bi <= line_start {
            continue;
        }
        let width = measure(manager, chars, line_start, bi, style);
        if mandatory {
            lines.push((line_start, bi));
            line_start = bi;
            last_allowed = None;
        } else if width > max_width {
            let b = last_allowed.filter(|&x| x > line_start).unwrap_or(bi);
            if b > line_start {
                lines.push((line_start, b));
                line_start = b;
            }
            last_allowed = Some(bi);
        } else {
            last_allowed = Some(bi);
        }
    }
    if line_start < para.len() {
        lines.push((line_start, para.len()));
    }
    lines
}

/// Measures the advance width of the logical byte range `[a, b)`.
fn measure(
    manager: &FontManager,
    chars: &[CharInfo],
    a: usize,
    b: usize,
    style: &TextStyle,
) -> f32 {
    let emoji_w = style.font_size;
    let mut total = 0.0;
    let mut i = 0;
    while i < chars.len() && chars[i].byte < a {
        i += 1;
    }
    while i < chars.len() && chars[i].byte < b {
        if chars[i].emoji.is_some() {
            total += emoji_w;
            i += 1;
            continue;
        }
        let rtl = chars[i].rtl;
        let font = chars[i].font;
        let mut run = String::new();
        while i < chars.len()
            && chars[i].byte < b
            && chars[i].emoji.is_none()
            && chars[i].rtl == rtl
            && chars[i].font == font
        {
            run.push(chars[i].ch);
            i += 1;
        }
        total += shape_run(manager, font, &run, rtl, style.font_size)
            .iter()
            .map(|g| g.x_advance)
            .sum::<f32>();
    }
    total
}

/// Lays out one line (byte range `[a, b)`) into visual-order glyphs/images.
fn layout_line(
    manager: &FontManager,
    emoji: &EmojiAtlas,
    chars: &[CharInfo],
    a: usize,
    b: usize,
    style: &TextStyle,
    base_rtl: bool,
) -> TextLine {
    let line_chars: Vec<&CharInfo> = chars.iter().filter(|c| c.byte >= a && c.byte < b).collect();
    let levels: Vec<Level> = line_chars
        .iter()
        .map(|c| if c.rtl { Level::rtl() } else { Level::ltr() })
        .collect();
    let order = visual_order(&levels);

    let (height, baseline) = line_metrics(manager, style, base_rtl);

    let mut glyphs: Vec<PositionedGlyph> = Vec::new();
    let mut images: Vec<PositionedImage> = Vec::new();
    let mut pen_x = 0.0f32;
    let emoji_size = style.font_size;
    let mut i = 0;

    while i < order.len() {
        let ci = order[i];
        let info = line_chars[ci];

        if let Some(ech) = info.emoji {
            let img = emoji
                .get(ech)
                .cloned()
                .unwrap_or_else(|| placeholder(emoji_size));
            images.push(PositionedImage {
                img,
                x: pen_x,
                y: baseline - emoji_size,
                w: emoji_size,
                h: emoji_size,
                char_index: info.index,
            });
            pen_x += emoji_size;
            i += 1;
            continue;
        }

        let rtl = info.rtl;
        let font = info.font;
        let color = info.color;
        let mut run = String::new();
        let mut run_indices: Vec<u32> = Vec::new();
        let mut j = i;
        while j < order.len() {
            let cj = order[j];
            let c = line_chars[cj];
            if c.emoji.is_none() && c.rtl == rtl && c.font == font && c.color == color {
                run.push(c.ch);
                run_indices.push(c.index);
                j += 1;
            } else {
                break;
            }
        }
        for g in shape_run(manager, font, &run, rtl, style.font_size) {
            // Map the glyph's cluster (byte offset within `run`) to the source
            // char index of that cluster's first character.
            let char_pos = run[..g.cluster as usize].chars().count();
            let char_index = run_indices[char_pos.min(run_indices.len() - 1)];
            glyphs.push(PositionedGlyph {
                font,
                glyph: g.glyph,
                pen_x: pen_x + g.x_offset,
                advance: g.x_advance,
                baseline_y: baseline,
                color,
                char_index,
            });
            pen_x += g.x_advance;
        }
        i = j;
    }

    TextLine {
        glyphs,
        images,
        width: pen_x,
        height,
        base_dir: if base_rtl { Direction::Rtl } else { Direction::Ltr },
    }
}
