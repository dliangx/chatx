use std::path::{Path, PathBuf};

/// Longest edge of a generated preview, in pixels.
pub const PREVIEW_MAX_EDGE: u32 = 512;

/// Root post store: `$CHATX_HOME/media/posts`, falling back to
/// `~/.config/chatx/media/posts` (same resolution as `chatx-core`).
pub fn media_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CHATX_HOME") {
        return PathBuf::from(p).join("media").join("posts");
    }
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())
    } else {
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    };
    PathBuf::from(home)
        .join(".config")
        .join("chatx")
        .join("media")
        .join("posts")
}

fn group_dir(group: &str) -> PathBuf {
    media_dir().join(group)
}

/// Copy one source image into the post store; returns the stored path.
pub fn store_original(src: &Path, group: &str, index: u32) -> Result<PathBuf, String> {
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "jpg".into());
    let dst = group_dir(group).join(format!("{index}.{ext}"));
    std::fs::create_dir_all(dst.parent().map(Path::to_path_buf).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    std::fs::copy(src, &dst).map_err(|e| e.to_string())?;
    Ok(dst)
}

/// Decode `src`, downscale to the preview size, and write a JPEG; returns
/// the stored preview path. Fails (callers fall back to the original) when
/// the source format is not supported by the `image` crate.
///
/// Note: `image` 0.25's stable API does not expose the JPEG quality knob
/// through `save`; the crate's built-in default is used (a visually
/// reasonable mid-range quality). If we ever need to tune it, switch to a
/// custom `jpeg`-crate encoding loop.
pub fn make_preview(src: &Path, group: &str, index: u32) -> Result<PathBuf, String> {
    let img = image::open(src).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    let scale = (PREVIEW_MAX_EDGE as f64 / (w.max(h)) as f64).min(1.0);
    let (nw, nh) = (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    );
    let small = if (w, h) == (nw, nh) {
        img
    } else {
        img.resize(nw, nh, image::imageops::FilterType::Lanczos3)
    };
    let dst = group_dir(group)
        .join("preview")
        .join(format!("{index}.jpg"));
    std::fs::create_dir_all(dst.parent().map(Path::to_path_buf).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    small
        .save_with_format(&dst, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    Ok(dst)
}

/// Write raw image bytes to the original slot of a post group. Returns the
/// stored absolute path. The caller passes the file extension (e.g. "jpg")
/// or empty (in which case we default to "jpg").
pub fn store_bytes(group: &str, index: u32, ext: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let ext = if ext.is_empty() { "jpg" } else { ext };
    let dst = group_dir(group).join(format!("{index}.{ext}"));
    std::fs::create_dir_all(dst.parent().map(Path::to_path_buf).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    std::fs::write(&dst, bytes).map_err(|e| e.to_string())?;
    Ok(dst)
}

/// Write raw image bytes to the preview slot of a post group. Returns the
/// stored absolute path.
pub fn store_preview_bytes(
    group: &str,
    index: u32,
    ext: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let ext = if ext.is_empty() { "jpg" } else { ext };
    let dst = group_dir(group)
        .join("preview")
        .join(format!("{index}.{ext}"));
    std::fs::create_dir_all(dst.parent().map(Path::to_path_buf).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    std::fs::write(&dst, bytes).map_err(|e| e.to_string())?;
    Ok(dst)
}

pub fn remove_post_files(paths: &[String]) {
    let root = std::fs::canonicalize(media_dir()).unwrap_or_else(|_| media_dir());
    for p in paths {
        let Ok(path) = std::fs::canonicalize(Path::new(p)) else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(&root) else {
            continue; // not ours — refuse to delete outside the post store
        };
        // `rel` is either `group/N.ext` (original) or `group/preview/N.jpg`.
        if let Some(group) = rel.components().next() {
            let _ = std::fs::remove_dir_all(root.join(group.as_os_str()));
        }
    }
}

/// Image container, detected from the leading magic bytes rather than the
/// file extension the sender claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Jpeg,
    Png,
    Gif,
    WebP,
    /// HEIC/HEIF — detected but not decodable by this stack today.
    Heic,
    Unknown,
}

/// Detect the image container from the leading magic bytes.
fn sniff_container(bytes: &[u8]) -> Container {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Container::Jpeg;
    }
    if bytes.starts_with(b"GIF8") {
        return Container::Gif;
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Container::Png;
    }
    // WebP: RIFF container + "WEBP" fourcc at offset 8.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP" {
        return Container::WebP;
    }
    // HEIC/HEIF: ISO-BMFF "ftyp" box with an Apple / HEVC brand.
    if bytes.len() >= 12 && bytes[4..8] == *b"ftyp" {
        let brand = std::str::from_utf8(&bytes[8..12]).unwrap_or("");
        if matches!(brand, "heic" | "heix" | "hevc" | "mif1") {
            return Container::Heic;
        }
    }
    Container::Unknown
}

fn webp_to_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Cursor;

    let decoded = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let (w, h) = (decoded.width(), decoded.height());
    if w == 0 || h == 0 {
        return Err("webp has zero dimensions".into());
    }

    let rgba = decoded.into_rgba8();
    let mut rgb = image::RgbImage::new(w, h);
    for (dst, px) in rgb.pixels_mut().zip(rgba.pixels()) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        let inv = 255u32 - a;
        dst[0] = ((r * a + 255 * inv) / 255) as u8;
        dst[1] = ((g * a + 255 * inv) / 255) as u8;
        dst[2] = ((b * a + 255 * inv) / 255) as u8;
    }

    // `write_to` requires `Write + Seek`; a cursor around a `Vec` gives us both.
    let mut cursor = Cursor::new(Vec::new());
    rgb.write_to(&mut cursor, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    Ok(cursor.into_inner())
}

pub fn store_received_image(
    group: &str,
    index: u32,
    is_preview: bool,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let store = |ext: &str, data: &[u8]| {
        if is_preview {
            store_preview_bytes(group, index, ext, data)
        } else {
            store_bytes(group, index, ext, data)
        }
    };
    match sniff_container(bytes) {
        Container::Jpeg => store("jpg", bytes),
        Container::Png => store("png", bytes),
        Container::Gif => store("gif", bytes),
        Container::Heic => store("heic", bytes),
        Container::Unknown => store("jpg", bytes),
        Container::WebP => {
            let jpeg = webp_to_jpeg(bytes)?;
            store("jpg", &jpeg)
        }
    }
}

pub fn repair_stored_formats() -> u32 {
    let base = media_dir();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return 0;
    };
    let mut repaired = 0u32;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        for d in [dir.clone(), dir.join("preview")] {
            let Ok(files) = std::fs::read_dir(&d) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let is_jpg_name = name
                    .rsplit('.')
                    .next()
                    .map(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
                    .unwrap_or(false);
                if !is_jpg_name {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&p) else {
                    continue;
                };
                if sniff_container(&bytes) != Container::WebP {
                    continue;
                }
                let Ok(jpeg) = webp_to_jpeg(&bytes) else {
                    tracing::warn!("[media] could not transcode WebP at {name}; left as-is");
                    continue;
                };
                match std::fs::write(&p, &jpeg) {
                    Ok(()) => repaired += 1,
                    Err(e) => tracing::warn!("[media] failed to rewrite {name}: {e}"),
                }
            }
        }
    }
    repaired
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_preview_roundtrip() {
        let dir = std::env::temp_dir().join(format!("chatx_postest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.png");
        image::RgbaImage::from_pixel(1200, 800, image::Rgba([200, 40, 40, 255]))
            .save(&src)
            .unwrap();

        let group = format!("test_{}", std::process::id());
        let stored = store_original(&src, &group, 1).unwrap();
        let preview = make_preview(&src, &group, 1).unwrap();
        assert!(stored.exists() && preview.exists());

        let p = image::open(&preview).unwrap();
        assert!(p.width().max(p.height()) <= PREVIEW_MAX_EDGE);
        assert!(p.width() > 0 && p.height() > 0);

        remove_post_files(&[
            stored.to_string_lossy().into_owned(),
            preview.to_string_lossy().into_owned(),
        ]);
        assert!(!stored.exists() && !preview.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniff_container_detects_each_format() {
        assert_eq!(
            sniff_container(&[0xFF, 0xD8, 0xFF, 0xE0, 0x0, 0x10]),
            Container::Jpeg
        );
        assert_eq!(sniff_container(b"GIF89a...."), Container::Gif);
        assert_eq!(
            sniff_container(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]),
            Container::Png
        );
        assert_eq!(
            sniff_container(b"RIFF\x00\x00\x00\x00WEBPVP8x...."),
            Container::WebP
        );
        assert_eq!(
            sniff_container(&[0, 0, 0, 20, b'f', b't', b'y', b'p', b'h', b'e', b'i', b'c']),
            Container::Heic
        );
        assert_eq!(
            sniff_container(b"whatever-not-an-image"),
            Container::Unknown
        );
    }

    #[test]
    fn webp_to_jpeg_roundtrip() {
        // Make a small lossless WebP with the codec `image` vendors for us.
        let w: u32 = 4;
        let h: u32 = 4;
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..(w * h) {
            rgb.extend([220u8, 20u8, 30u8]);
        }
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert_eq!(sniff_container(&webp), Container::WebP);

        let jpeg = webp_to_jpeg(&webp).unwrap();
        // Decode the JPEG back and check dimensions + color (lossy → tolerance).
        let decoded = image::ImageReader::new(std::io::Cursor::new(&jpeg))
            .with_guessed_format()
            .unwrap();
        let img = decoded.decode().unwrap();
        assert_eq!((img.width(), img.height()), (w, h));
        let px: Vec<u8> = img.into_rgba8().into_raw();
        assert!(
            px[0] > 200 && px[1] < 60 && px[2] < 60,
            "red pixel off: {:?}",
            &px[..6]
        );
    }
}
