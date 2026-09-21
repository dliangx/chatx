
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };

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

    pub const fn premultiplied(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }
}

const fn premul(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}
