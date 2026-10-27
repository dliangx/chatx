//! Shared QR decode path for the non-Apple targets (Windows / Linux / Android).
//!
//! All three feed an 8-bit greyscale plane to the decoder, which reports the
//! first successful payload to a caller-supplied sink (each platform hosts its
//! own single-slot sink). Decoding is delegated to `rxing`, the pure-Rust port
//! of ZXing, so the full multi-format reader (QR Code, 1D, Data Matrix, …) is
//! available.

use rxing::{
    BinaryBitmap, DecodeHints, FilteredImageReader, Luma8LuminanceSource, MultiFormatReader,
    Reader, common::HybridBinarizer,
};

// `bridge::types::PixelFormat` values (inlined to avoid a `bridge` dep on Apple).
pub const FMT_RGBA8888: u32 = 1;
pub const FMT_GRAY8: u32 = 8;
pub const FMT_RGB565: u32 = 9;

/// `bytes` is a `w*h*4` u8 RGBA8888 stream (R,G,B,A per pixel). Produce `w*h`
/// Y8.
fn rgba8888_to_greyscale(bytes: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let n = (w as usize) * (h as usize);
    if bytes.len() < n * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    let mut p = 0usize;
    for _ in 0..n {
        let r = bytes[p] as u32;
        let g = bytes[p + 1] as u32;
        let b = bytes[p + 2] as u32;
        p += 4;
        let y = (77 * r + 150 * g + 29 * b) / 256;
        out.push(y as u8);
    }
    Some(out)
}

/// Dispatch a capture frame into `scan_gray`. `fmt` is a `PixelFormat` value.
pub fn scan_frame(bytes: &[u8], w: u32, h: u32, fmt: u32, on_qr: &dyn Fn(String)) {
    let gray: Vec<u8> = match fmt {
        FMT_GRAY8 => {
            if bytes.len() < (w as usize) * (h as usize) {
                tracing::debug!(
                    "[camera] scan: gray buffer {}B < w*h={} — dropped",
                    bytes.len(),
                    (w as usize) * (h as usize)
                );
                return;
            }
            bytes.to_vec()
        }
        FMT_RGBA8888 => match rgba8888_to_greyscale(bytes, w, h) {
            Some(g) => g,
            None => {
                tracing::warn!(
                    "[camera] scan: rgba8888 conversion failed {}x{} {}B",
                    w,
                    h,
                    bytes.len()
                );
                return;
            }
        },
        _ => {
            tracing::warn!(
                "[camera] scan: unsupported pixel fmt={} ({}x{} {}B) — dropped",
                fmt,
                w,
                h,
                bytes.len()
            );
            return;
        }
    };
    scan_gray(&gray, w, h, on_qr);
}

/// Decode a QR / barcode from a local image file (JPEG/PNG), optionally fit
/// into the `target` box, and return its payload (or `None`).
pub fn decode_local_photo(path: &str, target: Option<(u32, u32)>) -> Option<String> {
    let dynamic = image::open(path).ok()?;
    let (img, w_px, h_px) = match target {
        Some((w, h)) => {
            let resized = dynamic.resize(w, h, image::imageops::FilterType::Triangle);
            (
                resized.to_rgba8().into_raw(),
                resized.width(),
                resized.height(),
            )
        }
        None => (
            dynamic.to_rgba8().into_raw(),
            dynamic.width(),
            dynamic.height(),
        ),
    };
    let out: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    scan_frame(&img, w_px, h_px, FMT_RGBA8888, &|t| {
        *out.borrow_mut() = Some(t)
    });
    out.into_inner()
}

/// Decode `gray` (an `w × h` 8-bit greyscale plane, `0` = black); on success
/// invoke `on_qr` with the payload, otherwise log and no-op.
///
/// Uses `FilteredImageReader` (up/down-scale pyramid + `TryHarder`), which is
/// required to decode small / low-res codes.
pub fn scan_gray(gray: &[u8], w: u32, h: u32, on_qr: &dyn Fn(String)) {
    if gray.len() < (w as usize) * (h as usize) {
        tracing::debug!(
            "[camera] scan: buffer {}B < w*h={} — dropped",
            gray.len(),
            (w as usize) * (h as usize)
        );
        return;
    }

    let Ok(source) = Luma8LuminanceSource::new(gray.to_vec(), w, h) else {
        tracing::warn!(
            "[camera] scan: Luma8LuminanceSource rejected {}x{} {}B",
            w,
            h,
            gray.len()
        );
        return;
    };
    let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
    let mut reader = FilteredImageReader::new(MultiFormatReader::default());
    match reader.decode_with_hints(&mut bitmap, &DecodeHints::default()) {
        Ok(result) => {
            let text = result.getText().trim().to_string();
            if text.is_empty() {
                tracing::debug!("[camera] scan: decoded but payload empty");
                return;
            }
            tracing::debug!("[camera] scan: QR detected = \"{}\"", text);
            on_qr(text);
        }
        Err(_) => {}
    }
}
