use slint::{Image as SlintImage, SharedPixelBuffer, SharedString};

use render::Renderer;
use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, Side};
use render::canvas::Image;

pub(crate) static RENDERER: std::sync::OnceLock<std::sync::Mutex<Renderer>> =
    std::sync::OnceLock::new();

thread_local! {
    static IMAGE_CACHE: std::cell::RefCell<std::collections::HashMap<String, SlintImage>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub fn load_image_cached(path: &str) -> SlintImage {
    let key = path.to_string();
    let cached = IMAGE_CACHE.with(|c| c.borrow().get(&key).cloned());
    if let Some(img) = cached {
        return img;
    }
    let img = SlintImage::load_from_path(std::path::Path::new(&key)).unwrap_or_default();
    IMAGE_CACHE.with(|c| c.borrow_mut().insert(key, img.clone()));
    img
}

const FONT_LATIN: &[u8] = include_bytes!("../fonts/NotoSans.ttf");
const FONT_CJK: &[u8] = include_bytes!("../fonts/NotoSansSC.ttf");
const FONT_EMOJI: &[u8] = include_bytes!("../fonts/NotoColorEmoji.ttf");

fn renderer() -> Renderer {
    let mut r = Renderer::new(render::theme::Theme::default());
    for bytes in [FONT_LATIN, FONT_CJK] {
        if let Some(id) = r.fonts.add_font(bytes) {
            let chain = r.fonts.ltr_chain().to_vec();
            r.fonts.set_ltr_chain(&{
                let mut v = chain;
                v.push(id);
                v
            });
            let rchain = r.fonts.rtl_chain().to_vec();
            r.fonts.set_rtl_chain(&{
                let mut v = rchain;
                v.push(id);
                v
            });
        }
    }
    let px = r.theme.font_size.max(16.0);
    r.emoji.add_color_font_bytes(
        FONT_EMOJI,
        0,
        &[
            '😀', '😂', '😅', '😉', '😊', '😍', '😘', '😜', '😎', '😢', '😭', '😡', '👍', '👎',
            '🙏', '👏', '🎉', '🔥', '💯', '🚀',
        ],
        px,
    );
    r
}

pub fn ensure_renderer() {
    let _ = RENDERER.get_or_init(|| std::sync::Mutex::new(renderer()));
}

pub fn make_avatar(seed: &str, size: u32) -> Image {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let hue = (h % 360) as f32 / 360.0;
    let hsl_to_rgb = |h: f32, s: f32, l: f32| -> (u8, u8, u8) {
        let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
        let hp = h * 6.0;
        let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
        let (r, g, b) = match hp as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, 0.0),
        };
        let m = l - c / 2.0;
        (
            ((r + m) * 255.0) as u8,
            ((g + m) * 255.0) as u8,
            ((b + m) * 255.0) as u8,
        )
    };
    let top = hsl_to_rgb(hue, 0.55, 0.62);
    let bottom = hsl_to_rgb(hue, 0.75, 0.35);
    let n = size.max(1) as usize;
    let mut rgba = vec![0u8; n * n * 4];
    for y in 0..n {
        let t = if n > 1 {
            y as f32 / (n - 1) as f32
        } else {
            0.0
        };
        let rr = (top.0 as f32 + (bottom.0 as f32 - top.0 as f32) * t) as u8;
        let gg = (top.1 as f32 + (bottom.1 as f32 - top.1 as f32) * t) as u8;
        let bb = (top.2 as f32 + (bottom.2 as f32 - top.2 as f32) * t) as u8;
        for x in 0..n {
            let i = (y * n + x) * 4;
            rgba[i] = rr;
            rgba[i + 1] = gg;
            rgba[i + 2] = bb;
            rgba[i + 3] = 255;
        }
    }
    Image {
        rgba,
        w: n as u32,
        h: n as u32,
    }
}

pub struct RenderedMsg {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub is_self: bool,
    pub text: String,
}

pub struct ChatMsg {
    pub sender: String,
    pub side: Side,
    pub time: String,
    pub text: String,
    pub avatar: Option<Image>,
}

pub fn render_msg(
    r: &mut Renderer,
    id: u64,
    msg: &ChatMsg,
    available_w: u32,
    scale: f32,
) -> RenderedMsg {
    let segs = parse_text(&msg.text);
    let bubble = Bubble {
        segments: &segs,
        sender: msg.sender.as_str(),
        time: msg.time.as_str(),
        side: msg.side,
        group: GroupPos::Single,
        avatar: msg.avatar.as_ref(),
    };
    let out = r.render(id, &bubble, available_w, scale).clone();
    RenderedMsg {
        rgba: out.to_straight_rgba(),
        width: out.width,
        height: out.height,
        is_self: msg.side == Side::SelfSide,
        text: msg.text.clone(),
    }
}

pub fn to_message_data(m: RenderedMsg, scale: f32, top: f32) -> crate::MessageData {
    let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(m.width, m.height);
    buf.make_mut_bytes().copy_from_slice(&m.rgba);
    crate::MessageData {
        bubble: SlintImage::from_rgba8(buf),
        width: (m.width as f64 / scale as f64) as f32,
        height: (m.height as f64 / scale as f64) as f32,
        is_self: m.is_self,
        text: SharedString::from(m.text),
        selected: false,
        top,
    }
}

pub fn make_preview_image(data: &[u8], w: u32, h: u32, fmt: u32) -> slint::Image {
    if w == 0 || h == 0 {
        return slint::Image::default();
    }
    let n = (w as usize) * (h as usize);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    let dst = buf.make_mut_bytes();
    match fmt {
        1 => {
            copy_rgba(data, dst, n);
        }
        8 => {
            let mut di = 0usize;
            for &y in data.iter().take(n) {
                if di + 4 > dst.len() {
                    break;
                }
                dst[di] = y;
                dst[di + 1] = y;
                dst[di + 2] = y;
                dst[di + 3] = 255;
                di += 4;
            }
        }
        _ => {}
    }
    slint::Image::from_rgba8(buf)
}

fn copy_rgba(src: &[u8], dst: &mut [u8], n: usize) {
    let want = n * 4;
    let len = want.min(src.len()).min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
}

pub fn qr_image(text: &str) -> slint::Image {
    use qrcode::{Color, QrCode};
    let code = match QrCode::new(text.as_bytes()) {
        Ok(c) => c,
        Err(_) => return slint::Image::default(),
    };
    let n = code.width();
    let scale = 8usize;
    let size = n * scale;
    let colors = code.to_colors();
    let mut rgba = vec![255u8; size * size * 4];
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = x * scale + dx;
                        let py = y * scale + dy;
                        let i = (py * size + px) * 4;
                        rgba[i] = 0;
                        rgba[i + 1] = 0;
                        rgba[i + 2] = 0;
                        rgba[i + 3] = 255;
                    }
                }
            }
        }
    }
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size as u32, size as u32);
    buf.make_mut_bytes().copy_from_slice(&rgba);
    slint::Image::from_rgba8(buf)
}
