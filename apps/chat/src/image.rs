use slint::Image as SlintImage;
use slint::{SharedPixelBuffer, SharedString};

use render::Renderer;
use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, Side};
use render::canvas::Image;

/// voice record icon + 8 pre-rotated spin frames are now pure Slint paths
/// (see `apps/chat/ui/components/icon.slint`, `Glyph.record`); no more PNG
/// decoding here. `Slint::Image` remains below for avatars and other raster paths.

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

/// Directory where sent / received chat photos live (per-user, not per-chat —
/// mirrors `record::voice_dir`).
pub fn image_dir() -> std::path::PathBuf {
    let dir = chatx_core::identity::DeviceIdentity::home_dir().join("images");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn ext_of(bytes: &[u8], src_ext_hint: Option<&str>) -> String {
    // Prefer a known source extension (e.g. .heic / .webp) so the stored file
    // keeps a format Slint can render; otherwise sniff the content and default
    // to JPEG (the most common picker output).
    if let Some(h) = src_ext_hint {
        let h = h.to_lowercase();
        if h == "jpg" || h == "jpeg" || h == "png" || h == "webp" || h == "heic" {
            return h;
        }
    }
    if bytes.len() >= 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        return "png".into();
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return "heic".into();
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" {
        return "webp".into();
    }
    "jpg".into()
}

/// Write raw image bytes into the chat-images dir and return the stored
/// original path. `src_ext_hint` is the picked/received file's original
/// extension, kept when it is a format Slint can decode (so HEIC/WebP
/// originals stay originals).
///
/// The original is the single full-resolution copy the peer receives and that
/// the full-screen viewer opens lazily. A compressed preview is produced in
/// the **same** blocking task, so when this `Future` resolves both files exist
/// and are fully written — no detached thread racing the chat's lazy
/// preview-loads. Must be awaited from a Tokio runtime context (both callers
/// drive it inside `tokio::spawn` / async fns).
pub async fn store_image_bytes(
    bytes: &[u8],
    src_ext_hint: Option<&str>,
) -> std::io::Result<std::path::PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = image_dir().join(format!("img-{ts}.{}", ext_of(bytes, src_ext_hint)));
    let owned = bytes.to_vec();
    tokio::task::spawn_blocking(move || -> std::io::Result<std::path::PathBuf> {
        std::fs::write(&path, &owned)?;
        // Preview failure is non-fatal: the bubble falls back to loading the
        // original's dimensions, and the original stays the single source of
        // truth for the full-screen viewer.
        let _ = ensure_preview(path.to_string_lossy().as_ref(), 300);
        Ok(path)
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
}

/// The bytes to send for a picked photo: the picker's original file, verbatim.
/// The chat bubble renders this file small (the "compressed preview"), and the
/// full-screen viewer opens the same file at full detail (the "original"), so
/// the peer always has the original image to tap into.
pub fn derive_chat_image_bytes(src: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(src)
}

/// Write an inbound / outgoing document file to the local `files/` dir (sender's basename preserved, sanitized) and return its path.
pub async fn store_file_bytes(
    name: &str,
    bytes: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Sanitize the sender-supplied basename (path separators / control chars) while keeping the extension (viewers detect the format by suffix).
    let base = name
        .rsplit(['/', '\\', '\0'])
        .next()
        .unwrap_or(name)
        .trim();
    let (stem_raw, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], base[i..].to_string()),
        _ => (base, String::new()),
    };
    let stem: String = stem_raw
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let stem = if stem.trim().is_empty() { "file".to_string() } else { stem };
    let dir = chatx_core::identity::DeviceIdentity::home_dir().join("files");
    let path = dir.join(format!("{ts}__{stem}{ext}"));
    let owned = bytes.to_vec();
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, &owned)?;
        Ok::<_, std::io::Error>(path)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Decode the photo at `path` (any format the `image` crate supports — PNG,
/// JPEG, WebP, …) into a full-resolution `SlintImage`. Slint's built-in
/// `load_from_path` only handles PNG/JPEG, so we decode through the `image`
/// crate here to support WebP and the other formats picked/received photos use.
pub fn decode_image(path: &str) -> SlintImage {
    match image::open(std::path::Path::new(path)) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(rgba.width(), rgba.height());
            buf.make_mut_bytes().copy_from_slice(rgba.as_raw());
            SlintImage::from_rgba8(buf)
        }
        Err(e) => {
            tracing::warn!("[image] decode failed for {path}: {e}");
            SlintImage::default()
        }
    }
}

/// On-disk preview path for a chat photo at `original`: same directory, name
/// prefixed with `preview-`, so the compressed copy sits next to the original.
pub fn preview_path(original: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(original);
    let stem = p.file_stem()?.to_string_lossy();
    Some(p.with_file_name(format!("preview-{stem}.jpg")))
}

/// Read just the pixel dimensions of the image at `path` (no full decode),
/// as a `(width, height)` pair. Defaults to `(1, 1)` when it can't be read.
pub fn image_dims(path: impl AsRef<std::path::Path>) -> (u32, u32) {
    image::image_dimensions(path).unwrap_or((1, 1))
}

/// Ensure the compressed preview for `original` exists on disk. The preview is
/// a JPEG capped to `cap` on the long edge (never upscaled) — a few KB for a
/// ~330px thumbnail, versus the multi-MB original the full-screen viewer opens
/// lazily. Returns the preview path on success.
pub fn ensure_preview(original: &str, cap: u32) -> std::io::Result<std::path::PathBuf> {
    let Some(dst) = preview_path(original) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no preview path",
        ));
    };
    if dst.is_file() {
        return Ok(dst);
    }
    let bytes = decode_capped_jpeg_bytes(original, cap)?;
    std::fs::write(&dst, bytes)?;
    Ok(dst)
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
    /// Present when this is a voice-message bubble: (disk path, duration ms).
    /// When set, the pixel buffer is a 1x1 dummy and the caller controls the
    /// *displayed* size (Slint draws the bubble).
    pub voice: Option<(String, u64)>,
    /// Present when this is a photo bubble: the disk path. When set, the pixel
    /// buffer is a 1x1 dummy and Slint draws the image (bubble + viewer).
    pub image: Option<String>,
    /// Present when this is a document-file bubble: (disk path, sender basename); the pixel buffer is a 1x1 dummy and Slint draws the file row.
    pub file: Option<(String, String)>,
    /// The SMALL cached preview path (≤300px JPEG) for this photo. Used by
    /// `to_message_data` to fill `image_img` (the bubble draws the thumb — a
    /// memory-cached hit — and NEVER the big original). Tapping opens the
    /// original at `image`, not here.
    pub preview: Option<String>,
    /// Sender avatar (right for own, left for theirs) shown beside a voice
    /// bubble, mirroring the rasterised text-bubble avatar.
    pub avatar: Option<render::canvas::Image>,
    /// Time label (e.g. "14:32") — displayed under the voice bubble body.
    pub time: String,
}

pub struct ChatMsg {
    pub sender: String,
    pub side: Side,
    pub time: String,
    pub text: String,
    pub avatar: Option<Image>,
    /// A voice message replaces the text body with a play bubble. The `path`
    /// is a local s16le clip and `duration_ms` is what the UI should show.
    pub voice: Option<(String, u64)>,
    /// A photo replaces the text body with an image bubble. The `path` is the
    /// local disk copy (original, possibly downscaled); tapping opens the viewer.
    pub image: Option<String>,
    /// A document file (pdf/docx/xlsx/pptx) replaces the text body with a file row: (disk path, sender basename); tapping a PDF opens the in-app viewer.
    pub file: Option<(String, String)>,
    /// Display-box dimensions derived from the small cached preview's aspect
    /// (identical ratio to the original, but never reads the big original file).
    /// `None` → `render_msg` falls back to measuring the original.
    pub preview_dims: Option<(u32, u32)>,
    /// The small cached preview path (≤300px JPEG) — what the bubble actually
    /// draws. `image` stays the original (for the viewer on tap).
    pub image_preview: Option<String>,
}

pub fn render_msg(
    r: &mut Renderer,
    id: u64,
    msg: &ChatMsg,
    available_w: u32,
    scale: f32,
) -> RenderedMsg {
    // File rows are drawn by Slint (icon + filename), so the rasterised buffer is a 1x1 dummy; the caller controls the displayed size.
    if let Some((path, name)) = &msg.file {
        return RenderedMsg {
            rgba: vec![255u8, 255, 255, 255],
            width: 1,
            height: 1,
            is_self: msg.side == Side::SelfSide,
            text: String::new(),
            voice: None,
            image: None,
            file: Some((path.clone(), name.clone())),
            preview: None,
            avatar: msg.avatar.clone(),
            time: msg.time.clone(),
        };
    }
    // Voice bubbles are drawn by Slint (icon + duration + play state), so the
    // rasterised image is a 1x1 dummy; the caller picks the displayed size in
    // `voice_dim` / `to_message_data_voice`.
    if let Some((path, dur)) = &msg.voice {
        return RenderedMsg {
            rgba: vec![255u8, 255, 255, 255],
            width: 1,
            height: 1,
            is_self: msg.side == Side::SelfSide,
            text: format!("{dur}ms"),
            voice: Some((path.clone(), *dur)),
            image: None,
            file: None,
            preview: None,
            avatar: msg.avatar.clone(),
            time: msg.time.clone(),
        };
    }
    // Photo bubbles are drawn by Slint (the `Image` element shows the disk file
    // and the viewer re-uses the same file), so the pixel buffer is a 1x1 dummy.
    // For the box aspect ratio we PREFER the small cached preview's dimensions
    // (`preview_dims`) — same ratio as the original, but never touches the big
    // original file. We only fall back to reading the original's dimensions when
    // no preview dimensions are available (e.g. preview not yet generated).
    if let Some(path) = &msg.image {
        let (iw, ih) = match msg.preview_dims {
            Some((w, h)) => (w.max(1), h.max(1)),
            None => image::image_dimensions(std::path::Path::new(path)).unwrap_or((1, 1)),
        };
        let max_h = 260.0;
        let iw = iw.max(1) as f64;
        let ih = ih.max(1) as f64;
        // Capped preview width, leaving room for the avatar (36px) + gap (8px).
        let max_w = (available_w as f64 / 2.0).min(220.0) - 44.0;
        let s = (max_w / iw).min(max_h / ih);
        let w_phys = (iw * s * scale as f64).max(1.0) as u32;
        let h_phys = (ih * s * scale as f64).max(1.0) as u32;
        return RenderedMsg {
            rgba: vec![255u8, 255, 255, 255],
            width: w_phys,
            height: h_phys,
            is_self: msg.side == Side::SelfSide,
            text: String::new(),
            voice: None,
            image: Some(path.clone()),
            file: None,
            preview: msg.image_preview.clone(),
            avatar: msg.avatar.clone(),
            time: msg.time.clone(),
        };
    }
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
            voice: None,
            image: None,
            file: None,
            preview: None,
            avatar: None,
            time: msg.time.clone(),
        }
    }

pub fn to_message_data(m: RenderedMsg, scale: f32, top: f32) -> crate::MessageData {
    // Voice bubbles are drawn by Slint; give them a fixed logical size.
    // Image bubbles already carry logical preview dimensions in `width`/`height`
    // (see `render_msg`), same handling as text rows.
    let is_voice = m.voice.is_some();
    let is_image = m.image.is_some();
    let is_file = m.file.is_some();
    let (w, h) = if is_voice {
        (72.0, 54.0)
    } else if is_file {
        // File row: avatar + icon + elided filename, 54px tall like the voice row (Slint truncates long names).
        (150.0, 54.0)
    } else {
        (m.width as f64 / scale as f64, m.height as f64 / scale as f64)
    };
    crate::MessageData {
        bubble: {
            // Image / file rows carry a 1x1 dummy raster (Slint draws them, sized by `width`/`height`), so keep the buffer 1x1 = the 4-byte `rgba`.
            let bw = if is_image || is_file { 1 } else { m.width.max(1) };
            let bh = if is_image || is_file { 1 } else { m.height.max(1) };
            let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(bw, bh);
            buf.make_mut_bytes().copy_from_slice(&m.rgba);
            SlintImage::from_rgba8(buf)
        },
        width: w as f32,
        height: h as f32,
        is_self: m.is_self,
        text: SharedString::from(m.text),
        selected: false,
        top,
        voice_path: m
            .voice
            .as_ref()
            .map(|(p, _)| SharedString::from(p.clone()))
            .unwrap_or_default(),
        voice_label: m
            .voice
            .map(|(_, d)| format_duration_ms(d))
            .map(SharedString::from)
            .unwrap_or_default(),
        file_path: m
            .file
            .as_ref()
            .map(|(p, _)| SharedString::from(p.clone()))
            .unwrap_or_default(),
        file_name: m
            .file
            .as_ref()
            .map(|(_, n)| SharedString::from(n.clone()))
            .unwrap_or_default(),
        image_path: m
            .image
            .clone()
            .map(SharedString::from)
            .unwrap_or_default(),
        // The photo bubble draws the SMALL cached preview (`image_preview`), not
        // the original. `load_image_cached` is a memory hit after the first
        // switch, so re-entering a chat never re-decodes the big full-res file.
        // The full original is only decoded by the viewer on tap (`open_image`).
        image_img: m
            .preview
            .as_ref()
            .map(|p| load_image_cached(p))
            .unwrap_or_else(SlintImage::default),
        avatar: {
            if let Some(a) = &m.avatar {
                let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(a.w, a.h);
                buf.make_mut_bytes().copy_from_slice(&a.rgba);
                SlintImage::from_rgba8(buf)
            } else {
                SlintImage::default()
            }
        },
        voice_time: SharedString::from(m.time),
    }
}

/// Format a duration in milliseconds as "M:SS" (e.g. 45000 → "0:45").
pub fn format_duration_ms(ms: u64) -> String {
    let s = ms / 1000;
    let m = s / 60;
    let rs = s % 60;
    format!("{m}:{rs:02}")
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
