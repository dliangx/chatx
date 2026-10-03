//! Shared QR decode path for the non-Apple targets (Windows / Linux / Android).
//!
//! All three feed an 8-bit greyscale (`Y800`) image into `zbar`, enable only
//! the QR-code symbology, and report the first successful payload to a
//! caller-supplied sink (each platform hosts its own single-slot sink).
//!
//! `zbar_symbol_type_e` has no `PartialEq` impl, so the discriminant is read
//! via `from` and compared as an `i32` (C `enum` values are their literal
//! `i32` discriminants on every ABI zbar ships for).

use std::os::raw::c_char;

const ZBAR_QRCODE: i32 = 64;

/// Decode `gray` (an `w × h` 8-bit greyscale plane) and, if a QR code is
/// found, invoke `on_qr` with its payload. No-op on any decode failure — the
/// caller pumps the next frame.
pub fn scan_gray(gray: &[u8], w: u32, h: u32, on_qr: &dyn Fn(String)) {
    use zbar::zbar::*;
    unsafe {
        let scanner = zbar_image_scanner_create();
        if scanner.is_null() {
            return;
        }
        zbar_image_scanner_set_config(
            scanner,
            zbar_symbol_type_e::ZBAR_QRCODE,
            zbar_config_e::ZBAR_CFG_ENABLE,
            1,
        );
        let img = zbar_image_create();
        if img.is_null() {
            zbar_image_scanner_destroy(scanner);
            return;
        }
        let y800: [u8; 4] = *b"Y800";
        let format = u32::from_ne_bytes(y800);
        zbar_image_set_format(img, format);
        zbar_image_set_size(img, w, h);
        zbar_image_set_data(img, gray.as_ptr(), gray.len() as u32, zbar_image_free_data);
        let n = zbar_scan_image(scanner, img);
        if n > 0 {
            let mut sym = zbar_image_first_symbol(img);
            while !sym.is_null() {
                let typ_raw: i32 = std::mem::transmute(zbar_symbol_get_type(sym));
                if typ_raw == ZBAR_QRCODE {
                    let p: *const c_char = zbar_symbol_get_data(sym);
                    if !p.is_null() {
                        let text = std::ffi::CStr::from_ptr(p)
                            .to_string_lossy()
                            .into_owned()
                            .trim()
                            .to_string();
                        if !text.is_empty() {
                            on_qr(text);
                        }
                    }
                }
                sym = zbar_symbol_next(sym);
            }
        }
        zbar_image_destroy(img);
        zbar_image_scanner_destroy(scanner);
    }
}
