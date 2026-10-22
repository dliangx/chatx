use slint::{Image as SlintImage, SharedPixelBuffer, SharedString};

use render::Renderer;
use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, Side};
use render::canvas::Image;

pub(crate) static RENDERER: std::sync::OnceLock<std::sync::Mutex<Renderer>> =
    std::sync::OnceLock::new();

/// Bound the cache so pixel buffers for off-screen / old covers and avatars get
/// reclaimed when the user navigates away (otherwise each `SlintImage` clone
/// keeps the decoded bitmap alive forever). 256 entries is plenty for the feed
/// + a few contact-list pages, and is small compared to a full-screen cover
/// bitmap.
const IMAGE_CACHE_MAX: usize = 256;

thread_local! {
    static IMAGE_CACHE: std::cell::RefCell<(
        std::collections::HashMap<String, SlintImage>,
        std::vec::Vec<String>,
    )> = std::cell::RefCell::new((std::collections::HashMap::new(), std::vec::Vec::new()));
}

pub fn load_image_cached(path: &str) -> SlintImage {
    let key = path.to_string();
    IMAGE_CACHE.with(|cell| {
        let (map, order) = &mut *cell.borrow_mut();
        if let Some(img) = map.get(&key).cloned() {
            if let Some(pos) = order.iter().position(|k| *k == key) {
                order.remove(pos);
            }
            order.push(key.clone());
            return img;
        }
        let img = SlintImage::load_from_path(std::path::Path::new(&key)).unwrap_or_default();
        while order.len() >= IMAGE_CACHE_MAX {
            if let Some(oldest) = order.first().cloned() {
                order.remove(0);
                map.remove(&oldest);
            } else {
                break;
            }
        }
        order.push(key.clone());
        map.insert(key, img.clone());
        img
    })
}

/// Decode `path` and downscale it to a side-capped square of at most `cap`px,
/// preserving the source aspect ratio (never upscales). The result is a
/// pixel-buffer `SlintImage` that `Image` elements can sample at a smaller
/// display size (e.g. a ProfileItem 40x40 avatar) without holding the full
/// bitmap.
fn to_pixel_buffer(img: image::DynamicImage, cap: u32) -> SlintImage {
    let w = img.width();
    let h = img.height();
    let longest = w.max(h).min(cap);
    let scale = longest as f64 / w.max(h) as f64;
    let (nw, nh) = ((w as f64 * scale) as u32, (h as f64 * scale) as u32);
    let resized = if nw != w || nh != h {
        img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let rgba = resized.to_rgba8();
    let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(rgba.width(), rgba.height());
    buf.make_mut_bytes().copy_from_slice(rgba.as_raw());
    SlintImage::from_rgba8(buf)
}

/// Load `path` into a capped, decode-once 40x40 avatar `SlintImage`. Empty /
/// missing paths yield the default image so the slot shows its placeholder.
pub fn load_avatar_path_capped(path: &str, cap: u32) -> SlintImage {
    if path.is_empty() {
        return SlintImage::default();
    }
    match image::open(std::path::Path::new(path)) {
        Ok(img) => to_pixel_buffer(img, cap),
        Err(_) => SlintImage::default(),
    }
}

/// Compress the avatar at `path` down to at most `cap`×`cap` (preserving
/// aspect ratio, never upscaling), re-encode as JPEG, and write it to
/// `dst_path`. Returns `Ok(())` on success.
///
/// The wire profile carries a base64-encoded, compact JPEG of the avatar so
/// receiving devices can persist a small on-disk copy without shipping the
/// source image — a 40×40 JPEG is a few KB even for the most detailed face.
pub fn compress_avatar_to_disk(
    src_path: &str,
    dst_path: &std::path::Path,
    cap: u32,
) -> std::io::Result<()> {
    let jpg_bytes = decode_capped_jpeg_bytes(src_path, cap)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(dst_path, jpg_bytes)?;
    Ok(())
}

/// Decode the image at `path`, downscale it to at most `cap`×`cap` (preserving
/// aspect ratio, never upscaling), and re-encode as JPEG bytes.
fn decode_capped_jpeg_bytes(src_path: &str, cap: u32) -> std::io::Result<Vec<u8>> {
    let img = image::open(std::path::Path::new(src_path))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    let resized: image::DynamicImage = if longest > cap {
        let scale = cap as f64 / longest as f64;
        let nw = (w as f64 * scale).max(1.0) as u32;
        let nh = (h as f64 * scale).max(1.0) as u32;
        img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    // `write_to` on a `Cursor<Vec<u8>>` lets us collect the encoded JPEG in
    // memory; `save_with_format` only writes to a path.
    let mut buf: std::io::Cursor<Vec<u8>> = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(buf.into_inner())
}

/// Load an avatar from `path`, cap it to `cap`×`cap`, re-encode as JPEG, and
/// return a base64 string (for embedding in a FriendQ profile over the P2P
/// wire). Returns `None` if the file cannot be decoded.
pub fn avatar_to_base64(path: &str, cap: u32) -> Option<String> {
    use base64::Engine as _;
    let bytes = decode_capped_jpeg_bytes(path, cap).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Decode `bytes` (JPEG / PNG / any format the `image` crate supports) and
/// produce a capped, RGBA8-pixel-buffer `SlintImage` at most `cap`×`cap`.
/// Empty / undecodable payloads return the default (`empty`) image so the
/// slot keeps its placeholder.
pub fn image_from_bytes(bytes: &[u8], cap: u32) -> SlintImage {
    if bytes.is_empty() {
        return SlintImage::default();
    }
    match image::load_from_memory(bytes) {
        Ok(img) => to_pixel_buffer(img, cap),
        Err(_) => SlintImage::default(),
    }
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
