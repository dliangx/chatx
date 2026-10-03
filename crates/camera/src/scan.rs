//! Shared QR decode path for the non-Apple targets (Windows / Linux / Android).
//!
//! All three feed an 8-bit greyscale plane to the decoder, which reports the
//! first successful payload to a caller-supplied sink (each platform hosts its
//! own single-slot sink).

/// Decode `gray` (an `w × h` 8-bit greyscale plane, `0` = black) and, if a QR
/// code is found, invoke `on_qr` with its payload. No-op on any decode failure —
/// the caller pumps the next frame.
pub fn scan_gray(gray: &[u8], w: u32, h: u32, on_qr: &dyn Fn(String)) {
    let wu = w as usize;
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(
        wu,
        h as usize,
        |x, y| gray.get(y * wu + x).copied().unwrap_or(255),
    );
    for grid in img.detect_grids() {
        if let Ok((_meta, content)) = grid.decode() {
            let text = content.trim().to_string();
            if !text.is_empty() {
                on_qr(text);
                return;
            }
        }
    }
}
