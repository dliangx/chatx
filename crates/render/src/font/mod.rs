
pub mod cache;

pub use cache::GlyphCache;

use fontdue::FontSettings;

pub type GlyphId = u16;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FontId(pub u16);

pub struct FontEntry {
    pub face: rustybuzz::Face<'static>,
    pub font: fontdue::Font,
    pub units_per_em: f32,
}

impl FontEntry {
    pub fn load(bytes: Vec<u8>) -> Option<FontEntry> {
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let face = rustybuzz::Face::from_slice(leaked, 0)?;
        let units_per_em = face.units_per_em() as f32;
        let font = fontdue::Font::from_bytes(leaked, FontSettings::default()).ok()?;
        Some(FontEntry { face, font, units_per_em })
    }

    pub fn scale_for(&self, px: f32) -> f32 {
        px / self.units_per_em
    }

    pub fn has_char(&self, c: char) -> bool {
        self.font.lookup_glyph_index(c) != 0
    }
}

#[derive(Default)]
pub struct FontManager {
    pub(crate) entries: Vec<FontEntry>,
    ltr_chain: Vec<FontId>,
    rtl_chain: Vec<FontId>,
    pub(crate) cache: GlyphCache,
}

impl FontManager {
    pub fn new() -> FontManager {
        FontManager::default()
    }

    pub fn add_font(&mut self, bytes: Vec<u8>) -> Option<FontId> {
        let entry = FontEntry::load(bytes)?;
        let id = FontId(self.entries.len() as u16);
        self.entries.push(entry);
        Some(id)
    }

    pub fn set_ltr_chain(&mut self, chain: &[FontId]) {
        self.ltr_chain = chain.to_vec();
    }

    pub fn set_rtl_chain(&mut self, chain: &[FontId]) {
        self.rtl_chain = chain.to_vec();
    }

    pub fn get(&self, id: FontId) -> &FontEntry {
        &self.entries[id.0 as usize]
    }

    pub fn resolve(&self, c: char, rtl: bool) -> FontId {
        let chain = if rtl { &self.rtl_chain } else { &self.ltr_chain };
        for id in chain {
            if self.entries[id.0 as usize].has_char(c) {
                return *id;
            }
        }
        chain
            .first()
            .copied()
            .unwrap_or(FontId(0))
    }

    pub fn primary(&self, rtl: bool) -> Option<FontId> {
        let chain = if rtl { &self.rtl_chain } else { &self.ltr_chain };
        chain.first().copied()
    }
}
