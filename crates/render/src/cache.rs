//! Bubble texture cache keyed by (message id, width bucket, scale, theme version).

use crate::bubble::RenderedBubble;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key {
    id: u64,
    width_bucket: u32,
    scale_bucket: u32,
    theme_version: u64,
}

pub struct TextureCache {
    map: HashMap<Key, RenderedBubble>,
}

impl TextureCache {
    pub fn new() -> TextureCache {
        TextureCache { map: HashMap::new() }
    }

    pub fn get(&self, id: u64, width: u32, scale: f32, theme_version: u64) -> Option<&RenderedBubble> {
        self.map.get(&Self::key(id, width, scale, theme_version))
    }

    pub fn insert(&mut self, id: u64, width: u32, scale: f32, theme_version: u64, bubble: RenderedBubble) {
        self.map.insert(Self::key(id, width, scale, theme_version), bubble);
    }

    /// Drops entries whose width bucket no longer matches `width`.
    pub fn evict_width(&mut self, width: u32) {
        let bucket = width_bucket(width);
        self.map.retain(|k, _| k.width_bucket == bucket);
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    fn key(id: u64, width: u32, scale: f32, theme_version: u64) -> Key {
        Key {
            id,
            width_bucket: width_bucket(width),
            scale_bucket: scale_bucket(scale),
            theme_version,
        }
    }
}

/// Quantizes width to buckets (rounded to 8px) so minor resizes reuse textures.
fn width_bucket(width: u32) -> u32 {
    (width + 4) / 8
}

/// Quantizes scale to 1/4 increments so similar ratios share textures.
fn scale_bucket(scale: f32) -> u32 {
    (scale * 4.0).round() as u32
}

impl Default for TextureCache {
    fn default() -> Self {
        Self::new()
    }
}
