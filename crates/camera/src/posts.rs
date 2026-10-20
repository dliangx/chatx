//! Persistent storage for social-post images.
//!
//! Moment photos come from the camera / photo picker into paths we did not
//! choose (the system temp dir on Apple, the picker's own copy on desktop),
//! which the OS or the user may clean at any time. Before a post is
//! published we therefore:
//!
//! 1. copy each original into `$CHATX_HOME/media/posts/<group>/n.<ext>` so
//!    it survives reboots and temp-dir cleanup (`store_original`), and
//! 2. derive a small JPEG preview (longest edge ≤ [`PREVIEW_MAX_EDGE`]) into
//!    `preview/n.jpg` for feed-card / grid thumbnails (`make_preview`).
//!
//! When the source format is not decodable by the `image` crate (e.g. HEIC),
//! `make_preview` returns `Err` and the caller falls back to using the
//! original as its own preview.
//!
//! The database stores the *post-store* paths (`media_urls` = originals,
//! `preview_urls` = previews), never the temp paths. `remove_post_files`
//! wipes the whole group dir when a post is deleted.

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
    let dst = group_dir(group).join("preview").join(format!("{index}.jpg"));
    std::fs::create_dir_all(dst.parent().map(Path::to_path_buf).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    small
        .save_with_format(&dst, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    Ok(dst)
}

/// Best-effort cleanup of a post's stored files. Given any of the post's
/// stored paths (original or preview), derive the post group dir and remove
/// the whole tree (originals + preview dir). Errors are skipped — the DB row
/// is the source of truth and must be deletable even if the media is already
/// gone (e.g. CHATX_HOME was wiped from under us).
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
}
