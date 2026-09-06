//! Custom software bubble renderer.
//!
//! Pipeline: message segments -> bidi + line break + shape -> text layout ->
//! compositor (fontdue glyphs + emoji atlas + bubble chrome) -> RGBA texture.

pub mod bubble;
pub mod cache;
pub mod canvas;
pub mod color;
pub mod emoji;
pub mod font;
pub mod text;
pub mod theme;

use bubble::{Bubble, RenderedBubble};
use cache::TextureCache;
use emoji::EmojiAtlas;
use font::FontManager;
use theme::Theme;

/// Renders plain text (white background, black glyphs) to a straight-alpha RGBA
/// buffer. Primarily for visual comparison / testing.
pub fn render_text_rgba(
    manager: &mut FontManager,
    text: &str,
    font_size: f32,
    max_width: f32,
) -> (Vec<u8>, u32, u32) {
    use canvas::Canvas;
    use color::Rgba;
    use text::layout_text;

    let style = text::TextStyle {
        font_size,
        color: Rgba::rgb(0, 0, 0),
        line_height: 1.25,
    };
    let lines = layout_text(manager, text, &style, max_width);
    let h = lines.iter().map(|l| l.height).sum::<f32>().ceil().max(1.0) as u32;
    let w = max_width.ceil().max(1.0) as u32;

    let mut canvas = Canvas::new(w, h);
    canvas.fill(Rgba::rgb(255, 255, 255));

    let FontManager { entries, cache, .. } = manager;
    let mut y = 0.0f32;
    for line in &lines {
        for g in &line.glyphs {
            let font = &entries[g.font.0 as usize].font;
            let bmp = cache.rasterize(font, g.font, g.glyph, font_size);
            let top_x = (g.pen_x + bmp.xmin as f32).round() as i32;
            let top_y = (y + g.baseline_y - bmp.ymin as f32 - bmp.height as f32).round() as i32;
            canvas.blit_mask(
                top_x,
                top_y,
                &bmp.coverage,
                bmp.width as u32,
                bmp.height as u32,
                g.color,
            );
        }
        y += line.height;
    }

    let rgba = canvas::unpremultiply(&canvas.into_rgba());
    (rgba, w, h)
}

/// Convenience renderer bundling fonts, emoji and a texture cache.
pub struct Renderer {
    pub fonts: FontManager,
    pub emoji: EmojiAtlas,
    pub theme: Theme,
    cache: TextureCache,
    theme_version: u64,
}

impl Renderer {
    pub fn new(theme: Theme) -> Renderer {
        Renderer {
            fonts: FontManager::new(),
            emoji: EmojiAtlas::new(),
            theme,
            cache: TextureCache::new(),
            theme_version: 0,
        }
    }

    /// Bump when the theme changes so cached textures are invalidated.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.theme_version = self.theme_version.wrapping_add(1);
    }

    /// Renders a bubble, returning a cached texture if available.
    ///
    /// `available_w` is the logical width; `scale` is the device pixel ratio
    /// (1.0 = normal, 2.0 = retina). The returned texture is `scale`x the
    /// logical size.
    pub fn render(
        &mut self,
        id: u64,
        bubble: &Bubble,
        available_w: u32,
        scale: f32,
    ) -> &RenderedBubble {
        let key_w = available_w;
        if self.cache.get(id, key_w, scale, self.theme_version).is_some() {
            return self.cache.get(id, key_w, scale, self.theme_version).unwrap();
        }
        let out = bubble::draw::render_bubble(
            &mut self.fonts,
            &self.emoji,
            bubble,
            &self.theme,
            available_w as f32,
            scale,
        );
        self.cache.insert(id, key_w, scale, self.theme_version, out);
        self.cache.get(id, key_w, scale, self.theme_version).unwrap()
    }

    /// Renders a bubble with a text-selection highlight. Not cached (selection
    /// changes every frame while dragging).
    pub fn render_selection(
        &mut self,
        bubble: &Bubble,
        available_w: u32,
        scale: f32,
        selection: (u32, u32),
    ) -> RenderedBubble {
        bubble::draw::render_bubble_with_selection(
            &mut self.fonts,
            &self.emoji,
            bubble,
            &self.theme,
            available_w as f32,
            scale,
            Some(selection),
        )
    }

    /// Maps a point (in bubble-texture pixel coordinates, i.e. already scaled)
    /// to the character index at that point.
    ///
    /// Returns the character index in the combined text space, and whether the
    /// point is closer to the character's trailing edge.
    pub fn hit_test(
        &self,
        bubble: &Bubble,
        available_w: u32,
        scale: f32,
        x: f32,
        y: f32,
    ) -> Option<(u32, bool)> {
        let frame = bubble::draw::compute_frame(
            &self.fonts,
            &self.emoji,
            bubble,
            &self.theme,
            available_w as f32,
            scale,
        );
        let (ox, oy) = frame.content_origin;
        let hit = text::selection::hit_test(&frame.content.lines, x - ox, y - oy)?;
        Some((hit.char_index, hit.after))
    }
}
