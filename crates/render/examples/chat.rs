//! Interactive chat-bubble rendering demo.
//!
//! Renders a handful of demo messages (CJK + Latin + RTL + emoji) into a
//! slint window. Long-press a bubble to select it and reveal a copy button.
//!
//! Run with: `cargo run -p render --example chat`

slint::include_modules!();

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, RenderedBubble, Side};
use render::canvas::Image;
use render::font::FontManager;
use render::theme::Theme;
use render::Renderer;
use slint::{Model, VecModel};

const AVAILABLE_W: u32 = 440;

const ARIAL: &str = "/System/Library/Fonts/Supplemental/Arial.ttf";
const STHEITI: &str = "/System/Library/Fonts/STHeiti Medium.ttc";
const NASKH: &str = "/System/Library/Fonts/Supplemental/DecoTypeNaskh.ttc";
const EMOJI_FONT: &str = "/System/Library/Fonts/Apple Color Emoji.ttc";
const EMOJIS: &[char] = &['😀', '😊', '❤', '👍', '🎉', '🚀', '🔥'];

struct DemoMsg {
    sender: String,
    side: Side,
    time: String,
    text: String,
    avatar: Option<Image>,
}

fn load(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// Builds a simple vertical-gradient avatar image for testing.
fn make_avatar(size: u32, top: (u8, u8, u8), bottom: (u8, u8, u8)) -> Image {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        let t = if size > 1 { y as f32 / (size - 1) as f32 } else { 0.0 };
        let r = (top.0 as f32 + (bottom.0 as f32 - top.0 as f32) * t) as u8;
        let g = (top.1 as f32 + (bottom.1 as f32 - top.1 as f32) * t) as u8;
        let b = (top.2 as f32 + (bottom.2 as f32 - top.2 as f32) * t) as u8;
        for x in 0..size {
            let i = ((y * size + x) as usize) * 4;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
    Image { rgba, w: size, h: size }
}

fn load_fonts(fonts: &mut FontManager) {
    if let (Some(arial), Some(stheiti), Some(naskh)) =
        (load(ARIAL), load(STHEITI), load(NASKH))
    {
        let l0 = fonts.add_font(arial).unwrap();
        let l1 = fonts.add_font(stheiti).unwrap();
        let r0 = fonts.add_font(naskh).unwrap();
        fonts.set_ltr_chain(&[l0, l1]);
        fonts.set_rtl_chain(&[r0, l0]);
    }
}

fn to_slint_image(r: &RenderedBubble) -> slint::Image {
    let rgba = r.to_straight_rgba();
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(r.width, r.height);
    buf.make_mut_bytes().copy_from_slice(&rgba);
    slint::Image::from_rgba8(buf)
}

/// Renders one message into a `MessageData`, optionally with a selection.
fn render_msg(
    renderer: &mut Renderer,
    id: u64,
    msg: &DemoMsg,
    scale: f32,
    selected: bool,
) -> MessageData {
    let segs = parse_text(&msg.text);
    let bubble = Bubble {
        segments: &segs,
        sender: &msg.sender,
        time: &msg.time,
        side: msg.side,
        group: GroupPos::Single,
        avatar: msg.avatar.as_ref(),
    };
    let out = if selected {
        renderer.render_selection(&bubble, AVAILABLE_W, scale, (0, u32::MAX))
    } else {
        renderer.render(id, &bubble, AVAILABLE_W, scale).clone()
    };
    MessageData {
        bubble: to_slint_image(&out),
        width: out.width as f32 / scale,
        height: out.height as f32 / scale,
        is_self: msg.side == Side::SelfSide,
        text: msg.text.clone().into(),
        selected,
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let mut renderer = Renderer::new(Theme::default());
    load_fonts(&mut renderer.fonts);

    let ui = ChatWindow::new()?;
    ui.show()?;

    let alice_avatar = make_avatar(64, (0x4A, 0x90, 0xD9), (0x2C, 0x5F, 0x9E));
    let ali_avatar = make_avatar(64, (0xE0, 0x8A, 0x3C), (0xB5, 0x5E, 0x1E));

    let demo: Vec<DemoMsg> = vec![
        DemoMsg { sender: "Alice".into(), side: Side::Other, time: "09:10".into(), text: "你好！这是我们 P2P 聊天应用的消息气泡渲染测试。".into(), avatar: Some(alice_avatar.clone()) },
        DemoMsg { sender: "Me".into(), side: Side::SelfSide, time: "09:11".into(), text: "看起来不错 😀 中文 + English + 123 混排都能正确换行".into(), avatar: None },
        DemoMsg { sender: "Alice".into(), side: Side::Other, time: "09:12".into(), text: "这条消息特别特别长，用来测试当文本宽度超过气泡最大宽度时，是否能够按照 unicode 换行算法正确地自动换行显示，同时保持气泡宽度不超过上限。".into(), avatar: Some(alice_avatar.clone()) },
        DemoMsg { sender: "Me".into(), side: Side::SelfSide, time: "09:13".into(), text: "链接测试：https://example.com 以及 @Bob 的提及".into(), avatar: None },
        DemoMsg { sender: "Ali".into(), side: Side::Other, time: "09:14".into(), text: "مرحبا بالعالم".into(), avatar: Some(ali_avatar.clone()) },
        DemoMsg { sender: "Me".into(), side: Side::SelfSide, time: "09:15".into(), text: "表情测试 😀😊❤️👍🎉🚀🔥".into(), avatar: Some(ali_avatar.clone()) },
    ];

    let model: Rc<VecModel<MessageData>> = Rc::new(VecModel::default());
    let renderer = Rc::new(RefCell::new(renderer));
    let msgs = Rc::new(demo);
    let scale = Rc::new(Cell::new(1.0f32));
    let pressed = Rc::new(Cell::new(None::<usize>));
    let selected = Rc::new(Cell::new(None::<usize>));
    let ui_weak = ui.as_weak();

    // Initial render, deferred until the window is mapped so the correct
    // device scale factor (retina) is known.
    {
        let ui_weak = ui_weak.clone();
        let renderer = renderer.clone();
        let msgs = msgs.clone();
        let model = model.clone();
        let scale = scale.clone();
        slint::Timer::single_shot(Duration::from_millis(200), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let s = ui.window().scale_factor().max(1.0);
            scale.set(s);
            let mut r = renderer.borrow_mut();
            if let Some(emoji_bytes) = load(EMOJI_FONT) {
                let px = r.theme.font_size * s;
                r.emoji.add_color_font_bytes(&emoji_bytes, 0, EMOJIS, px);
            }
            for (i, msg) in msgs.iter().enumerate() {
                model.push(render_msg(&mut r, i as u64, msg, s, false));
            }
            ui.set_messages(model.into());
        });
    }

    let select_msg = {
        let renderer = renderer.clone();
        let model = model.clone();
        let msgs = msgs.clone();
        let selected = selected.clone();
        let scale = scale.clone();
        move |idx: usize| {
            let s = scale.get();
            // Deselect previous.
            if let Some(prev) = selected.get().filter(|&p| p != idx)
                && let Some(md) = model.row_data(prev)
            {
                let msg = &msgs[prev];
                let mut r = renderer.borrow_mut();
                let fresh = render_msg(&mut r, prev as u64, msg, s, false);
                let mut md = md;
                md.bubble = fresh.bubble;
                md.width = fresh.width;
                md.height = fresh.height;
                md.selected = false;
                model.set_row_data(prev, md);
            }
            if let Some(md) = model.row_data(idx) {
                let msg = &msgs[idx];
                let mut r = renderer.borrow_mut();
                let fresh = render_msg(&mut r, idx as u64, msg, s, true);
                let mut md = md;
                md.bubble = fresh.bubble;
                md.width = fresh.width;
                md.height = fresh.height;
                md.selected = true;
                model.set_row_data(idx, md);
            }
            selected.set(Some(idx));
        }
    };

    ui.on_press({
        let pressed = pressed.clone();
        let select_msg = select_msg.clone();
        move |idx: i32| {
            let idx = idx as usize;
            pressed.set(Some(idx));
            let pressed = pressed.clone();
            let select_msg = select_msg.clone();
            slint::Timer::single_shot(Duration::from_millis(500), move || {
                if pressed.get() == Some(idx) {
                    pressed.set(None);
                    select_msg(idx);
                }
            });
        }
    });

    ui.on_release(move |_idx: i32| {
        pressed.set(None);
    });

    ui.on_copy_text(move |text: slint::SharedString| {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text.to_string());
        }
    });

    ui.run()
}
