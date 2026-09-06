//! RGBA color, stored as premultiplied alpha internally.

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgba {
    /// premultiplied: r,g,b already scaled by alpha
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };

    /// From straight (non-premultiplied) components.
    pub const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        if a == 255 {
            Rgba { r, g, b, a }
        } else {
            Rgba {
                r: premul(r, a),
                g: premul(g, a),
                b: premul(b, a),
                a,
            }
        }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 255 }
    }

    /// Premultiplied from an already-premultiplied component set.
    pub const fn premultiplied(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }
}

const fn premul(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}
