//! Dumps rendered demo bubbles to PNG files for visual inspection.
//! Usage: cargo run -p render --example dump [output_dir]

use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, Side};
use render::theme::Theme;
use render::Renderer;

const ARIAL: &str = "/System/Library/Fonts/Supplemental/Arial.ttf";
const STHEITI: &str = "/System/Library/Fonts/STHeiti Medium.ttc";
const NASKH: &str = "/System/Library/Fonts/Supplemental/DecoTypeNaskh.ttc";
const EMOJI_FONT: &str = "/System/Library/Fonts/Apple Color Emoji.ttc";
const EMOJIS: &[char] = &['😀', '😊', '❤', '👍', '🎉', '🚀', '🔥'];

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "/tmp/bubbles".to_string());
    std::fs::create_dir_all(&out_dir).unwrap();

    let mut renderer = Renderer::new(Theme::default());
    if let (Some(a), Some(s), Some(n)) = (
        std::fs::read(ARIAL).ok(),
        std::fs::read(STHEITI).ok(),
        std::fs::read(NASKH).ok(),
    ) {
        let l0 = renderer.fonts.add_font(a).unwrap();
        let l1 = renderer.fonts.add_font(s).unwrap();
        let r0 = renderer.fonts.add_font(n).unwrap();
        renderer.fonts.set_ltr_chain(&[l0, l1]);
        renderer.fonts.set_rtl_chain(&[r0, l0]);
    }
    if let Some(emoji_bytes) = std::fs::read(EMOJI_FONT).ok() {
        let px = renderer.theme.font_size;
        renderer.emoji.add_color_font_bytes(&emoji_bytes, 0, EMOJIS, px);
    }

    let demo: Vec<(&str, Side, &str, &str)> = vec![
        ("Alice", Side::Other, "09:10", "你好！这是我们 P2P 聊天应用的消息气泡渲染测试。"),
        ("Me", Side::SelfSide, "09:11", "看起来不错 😀 中文 + English + 123 混排都能正确换行"),
        ("Alice", Side::Other, "09:12", "这条消息特别特别长，用来测试当文本宽度超过气泡最大宽度时，是否能够按照 unicode 换行算法正确地自动换行显示，同时保持气泡宽度不超过上限。"),
        ("Me", Side::SelfSide, "09:13", "链接测试：https://example.com 以及 @Bob 的提及"),
        ("Ali", Side::Other, "09:14", "مرحبا بالعالم"),
        ("Me", Side::SelfSide, "09:15", "表情测试 😀😊❤️👍🎉🚀🔥"),
    ];

    for (i, (sender, side, time, text)) in demo.iter().enumerate() {
        let segs = parse_text(text);
        let bubble = Bubble {
            segments: &segs,
            sender,
            time,
            side: *side,
            group: GroupPos::Single,
            avatar: None,
        };
        let out = renderer.render(i as u64, &bubble, 420, 1.0);
        let rgba = out.to_straight_rgba();
        let (opaque, dark, light) = stats(&rgba);
        println!("bubble_{}: {}x{} opaque={} dark(text)={} light(white)={} colorful={}", i, out.width, out.height, opaque, dark, light, count_colorful(&rgba));
        write_png(&format!("{}/bubble_{}.png", out_dir, i), &rgba, out.width, out.height);
    }

    println!("wrote bubbles to {}", out_dir);
}

fn stats(rgba: &[u8]) -> (usize, usize, usize) {
    let (mut opaque, mut dark, mut light) = (0, 0, 0);
    for px in rgba.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        if a > 0 {
            opaque += 1;
            if r < 120 && g < 120 && b < 120 {
                dark += 1;
            }
            if r > 230 && g > 230 && b > 230 {
                light += 1;
            }
        }
    }
    (opaque, dark, light)
}

fn count_colorful(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4)
        .filter(|px| {
            if px[3] == 0 {
                return false;
            }
            let max = px[0].max(px[1]).max(px[2]);
            let min = px[0].min(px[1]).min(px[2]);
            max as i32 - min as i32 > 30
        })
        .count()
}

fn write_png(path: &str, rgba: &[u8], w: u32, h: u32) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
}
