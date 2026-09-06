//! Integration tests using system fonts (macOS).

use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, Segment, Side};
use render::canvas::{Canvas, Corners};
use render::color::Rgba;
use render::emoji::EmojiAtlas;
use render::font::FontManager;
use render::text::selection::{hit_test, selection_rects};
use render::text::{layout_text, TextStyle};
use render::theme::Theme;

const ARIAL: &str = "/System/Library/Fonts/Supplemental/Arial.ttf";
const STHEITI: &str = "/System/Library/Fonts/STHeiti Medium.ttc";
const NASKH: &str = "/System/Library/Fonts/Supplemental/DecoTypeNaskh.ttc";

fn load(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn make_manager() -> Option<FontManager> {
    let arial = load(ARIAL)?;
    let stheiti = load(STHEITI)?;
    let naskh = load(NASKH)?;

    let mut m = FontManager::new();
    let l0 = m.add_font(arial)?;
    let l1 = m.add_font(stheiti)?;
    let r0 = m.add_font(naskh)?;
    m.set_ltr_chain(&[l0, l1]);
    m.set_rtl_chain(&[r0, l0]);
    Some(m)
}

fn text_style(color: Rgba) -> TextStyle {
    TextStyle { font_size: 16.0, color, line_height: 1.4 }
}

#[test]
fn layout_mixed_cjk_latin_wraps() {
    let Some(mut m) = make_manager() else { return };
    let style = text_style(Rgba::rgb(0, 0, 0));
    let lines = layout_text(&mut m, "你好Hello世界123这是很长的一段测试文本用来验证自动换行是否正常工作", &style, 120.0);
    assert!(lines.len() > 1, "expected wrapping into multiple lines, got {}", lines.len());
    for l in &lines {
        assert!(l.width <= 120.0 + 1.0, "line width {} exceeds max", l.width);
        assert!(!l.glyphs.is_empty());
    }
}

#[test]
fn layout_pure_arabic_is_rtl() {
    let Some(mut m) = make_manager() else { return };
    let style = text_style(Rgba::rgb(0, 0, 0));
    let lines = layout_text(&mut m, "مرحبا بالعالم", &style, 200.0);
    assert!(!lines.is_empty());
    for l in &lines {
        assert_eq!(l.base_dir, render::text::Direction::Rtl);
        assert!(!l.glyphs.is_empty());
    }
}

#[test]
fn layout_rtl_with_embedded_ltr_numbers() {
    let Some(mut m) = make_manager() else { return };
    let style = text_style(Rgba::rgb(0, 0, 0));
    // RTL text containing LTR digits.
    let lines = layout_text(&mut m, "السعر 1234 ريال", &style, 200.0);
    assert!(!lines.is_empty());
    assert!(lines[0].glyphs.len() >= 2);
}

#[test]
fn parse_segments() {
    let segs = parse_text("hello @bob check https://x.com 😀");
    assert!(matches!(&segs[0], Segment::Text(_)));
    assert!(segs.iter().any(|s| matches!(s, Segment::Mention(_))));
    assert!(segs.iter().any(|s| matches!(s, Segment::Link(_))));
    assert!(segs.iter().any(|s| matches!(s, Segment::Emoji(_))));
}

#[test]
fn canvas_round_rect_covers_center() {
    let mut c = Canvas::new(20, 20);
    c.round_rect(2.0, 2.0, 16.0, 16.0, Corners::all(4.0), Rgba::rgb(255, 0, 0));
    let rgba = c.into_rgba();
    // Center pixel should be filled.
    let idx = (10 * 20 + 10) * 4;
    assert_eq!(rgba[idx + 3], 255);
    // Corner pixel should be transparent.
    let corner = (2 * 20 + 2) * 4;
    assert_eq!(rgba[corner + 3], 0);
}

#[test]
fn bubble_render_produces_texture() {
    let Some(mut m) = make_manager() else { return };
    let emoji = EmojiAtlas::new();
    let theme = Theme::default();

    let segs = vec![
        Segment::Text("你好，这是来自 P2P 聊天的消息气泡测试".to_string()),
        Segment::Emoji('😀'),
    ];
    let bubble = Bubble {
        segments: &segs,
        sender: "Alice",
        time: "10:30",
        side: Side::Other,
        group: GroupPos::Single,
        avatar: None,
    };

    let out = render::bubble::draw::render_bubble(&mut m, &emoji, &bubble, &theme, 300.0, 1.0);
    assert!(out.width > 0 && out.height > 0);
    assert_eq!(out.rgba.len() as u32, out.width * out.height * 4);
    // Some pixel is non-transparent (bubble body drawn).
    let any_opaque = out.rgba.chunks_exact(4).any(|p| p[3] != 0);
    assert!(any_opaque, "bubble should have opaque pixels");
}

#[test]
fn color_emoji_decodes() {
    let Ok(bytes) = std::fs::read("/System/Library/Fonts/Apple Color Emoji.ttc") else { return };
    let mut emoji = EmojiAtlas::new();
    emoji.add_color_font_bytes(&bytes, 0, &['😀', '❤', '👍'], 32.0);

    for ch in ['😀', '❤', '👍'] {
        let img = emoji.get(ch).unwrap_or_else(|| panic!("missing emoji {ch}"));
        assert!(img.w > 0 && img.h > 0, "emoji {ch} image empty");
        let colorful = img
            .rgba
            .chunks_exact(4)
            .filter(|p| {
                p[3] != 0
                    && (p[0].max(p[1]).max(p[2]) as i32 - p[0].min(p[1]).min(p[2]) as i32) > 30
            })
            .count();
        assert!(colorful > 0, "emoji {ch} should contain colored pixels");
    }
}

#[test]
fn hit_test_maps_point_to_char() {
    let Some(mut m) = make_manager() else { return };
    let style = text_style(Rgba::rgb(0, 0, 0));
    let text = "Hello world";
    let lines = layout_text(&mut m, text, &style, 400.0);
    assert_eq!(lines.len(), 1);

    // Start of the line maps to the first char.
    let hit = hit_test(&lines, 0.0, 0.0).unwrap();
    assert_eq!(hit.char_index, 0);
    assert!(!hit.after);

    // End of the line maps to the last char.
    let width = lines[0].width;
    let hit = hit_test(&lines, width, 0.0).unwrap();
    assert_eq!(hit.char_index, (text.chars().count() - 1) as u32);
    assert!(hit.after);
}

#[test]
fn selection_rects_cover_range() {
    let Some(mut m) = make_manager() else { return };
    let style = text_style(Rgba::rgb(0, 0, 0));
    let lines = layout_text(&mut m, "Hello world", &style, 400.0);

    // Selecting "Hello" (indices 0..5) should produce a single leading rect.
    let rects = selection_rects(&lines, 0, 5);
    assert!(!rects.is_empty());
    assert!(rects[0].x >= 0.0);
    assert!(rects[0].w > 0.0);

    // Empty selection produces no rects.
    assert!(selection_rects(&lines, 3, 3).is_empty());
}

#[test]
fn selection_highlight_renders() {
    let Some(mut m) = make_manager() else { return };
    let emoji = EmojiAtlas::new();
    let theme = Theme::default();
    let segs = vec![Segment::Text("select me please".to_string())];
    let bubble = Bubble {
        segments: &segs,
        sender: "A",
        time: "10:00",
        side: Side::Other,
        group: GroupPos::Single,
        avatar: None,
    };

    let plain = render::bubble::draw::render_bubble(&mut m, &emoji, &bubble, &theme, 300.0, 1.0);
    let selected = render::bubble::draw::render_bubble_with_selection(
        &mut m, &emoji, &bubble, &theme, 300.0, 1.0, Some((0, 6)),
    );
    assert_eq!(plain.width, selected.width);
    assert_eq!(plain.height, selected.height);

    // The selection highlight (blue) introduces new colored pixels.
    let has_blue = selected
        .rgba
        .chunks_exact(4)
        .any(|p| p[3] != 0 && p[2] as i32 > p[0] as i32 + 30 && p[2] as i32 > p[1] as i32 + 30);
    assert!(has_blue, "selection highlight should introduce blue pixels");
}
