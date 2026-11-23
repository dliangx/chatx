//! Shared RGBA8888 ↔ NV12 pixel conversion, used by the Windows (Media
//! Foundation) and Linux (VAAPI) backends.
//!
//! NV12 is the universal GPU video format:
//!   - Plane 0 (Y):  `w * h` bytes
//!   - Plane 1 (UV): `w * h / 2` bytes (interleaved U,V at half resolution)
//!
//! Total: `w * h * 3 / 2` bytes.
//!
//! BT.601 coefficients, integer math (no FP) for speed.

/// Convert a tightly-packed RGBA8888 buffer to NV12.
/// `out` must be at least `w * h * 3 / 2` bytes.
///
/// Kept for a future VAAPI / MF hardware-encode path (which needs NV12 input);
/// the software [openh264] encoder takes RGBA directly and does not use this.
#[allow(dead_code)]
pub fn rgba_to_nv12(rgba: &[u8], w: u32, h: u32, out: &mut [u8]) {
    let w = w as usize;
    let h = h as usize;
    let y_len = w * h;
    let base = out.as_mut_ptr();

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let r = rgba[i] as i32;
            let g = rgba[i + 1] as i32;
            let b = rgba[i + 2] as i32;

            let yy = (77 * r + 150 * g + 29 * b) >> 8;
            unsafe { *base.add(y * w + x) = yy.clamp(0, 255) as u8 };

            if (x & 1) == 0 && (y & 1) == 0 {
                let xx = x / 2;
                let y2 = y / 2;
                let u = ((-43 * r - 85 * g + 128 * b) >> 8) + 128;
                let v = ((128 * r - 107 * g - 21 * b) >> 8) + 128;
                let uv = y_len + (y2 * w + xx) * 2;
                unsafe {
                    *base.add(uv) = u.clamp(0, 255) as u8;
                    *base.add(uv + 1) = v.clamp(0, 255) as u8;
                };
            }
        }
    }
}

/// Convert an NV12 buffer (Y plane + interleaved UV plane) to tightly-packed
/// RGBA8888. `out` must be at least `w * h * 4` bytes.
pub fn nv12_to_rgba(nv12: &[u8], w: u32, h: u32, out: &mut [u8]) {
    let w = w as usize;
    let h = h as usize;
    let y_len = w * h;
    let y_base = nv12.as_ptr();
    let out_base = out.as_mut_ptr();

    for y in 0..h {
        for x in 0..w {
            let yy = unsafe { *y_base.add(y * w + x) } as i32;
            let uv = y_len + ((y / 2) * w + x / 2) * 2;
            let u = unsafe { *y_base.add(uv) } as i32;
            let v = unsafe { *y_base.add(uv + 1) } as i32;

            let r = (yy + ((359 * (v - 128)) >> 8)).clamp(0, 255) as u8;
            let g = (yy - ((88 * (u - 128)) >> 8) - ((183 * (v - 128)) >> 8)).clamp(0, 255) as u8;
            let b = (yy + ((454 * (u - 128)) >> 8)).clamp(0, 255) as u8;

            let i = (y * w + x) * 4;
            unsafe {
                *out_base.add(i) = r;
                *out_base.add(i + 1) = g;
                *out_base.add(i + 2) = b;
                *out_base.add(i + 3) = 255;
            };
        }
    }
}

/// Size in bytes of an NV12 buffer for the given dimensions.
pub fn nv12_size(w: u32, h: u32) -> usize {
    (w as usize) * (h as usize) * 3 / 2
}
