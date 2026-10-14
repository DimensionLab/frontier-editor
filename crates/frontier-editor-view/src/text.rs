// SPDX-License-Identifier: Apache-2.0
//! Shaping and rasterisation: cosmic-text (harfrust shaping, swash raster) with an
//! embedded monospace font, a per-line shape cache and a glyph atlas we own.
//!
//! Glyphs are keyed by cosmic-text's [`CacheKey`], which includes the font size bits
//! and a 4-way subpixel bin, so a line moved by a fraction of a pixel (zoom, smooth
//! scroll) reuses the same bitmaps and stays sharp at 1x and 2x.

use cosmic_text::{
    Attrs, AttrsList, CacheKey, Family, FontSystem, Hinting, ShapeLine, Shaping, SwashCache,
    SwashContent, Wrap, fontdb,
};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// DejaVu Sans Mono (Bitstream Vera licence, see `assets/LICENSE_DEJAVU`): embedded so
/// web and native shape identically and the browser needs no font download.
pub const FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSansMono.ttf");
pub const FONT_FAMILY: &str = "DejaVu Sans Mono";

/// Where a rasterised glyph lives in the atlas and how to place it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasGlyph {
    /// Atlas texture coordinates (normalised).
    pub uv: [f32; 4],
    /// Bitmap size in device pixels.
    pub w: u32,
    pub h: u32,
    /// Offset from the glyph origin to the bitmap's top-left, device pixels.
    pub left: i32,
    pub top: i32,
    /// Colour glyphs (emoji) are drawn without tinting.
    pub color: bool,
}

/// Shelf-packed RGBA atlas with one pending-upload queue per frame. Ordinary glyphs are
/// white with their coverage as alpha (tinted when drawn); colour glyphs keep their own
/// premultiplied colours.
pub struct GlyphAtlas {
    pub size: u32,
    shelves: Vec<Shelf>,
    entries: HashMap<CacheKey, Option<AtlasGlyph>>,
    pending: Vec<(u32, u32, u32, u32, Vec<u8>)>,
    pub generation: u64,
    bytes_used: usize,
}

#[derive(Debug, Clone, Copy)]
struct Shelf {
    y: u32,
    height: u32,
    next_x: u32,
}

impl std::fmt::Debug for GlyphAtlas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlyphAtlas")
            .field("size", &self.size)
            .field("glyphs", &self.entries.len())
            .field("generation", &self.generation)
            .finish()
    }
}

impl GlyphAtlas {
    pub fn new(size: u32) -> Self {
        Self {
            size,
            shelves: Vec::new(),
            entries: HashMap::new(),
            pending: Vec::new(),
            generation: 0,
            bytes_used: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }

    /// Drops every glyph; callers re-rasterise lazily. Bumps `generation` so the
    /// renderer reallocates the texture when `size` changed.
    pub fn clear(&mut self, size: u32) {
        self.size = size;
        self.shelves.clear();
        self.entries.clear();
        self.pending.clear();
        self.bytes_used = 0;
        self.generation += 1;
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let pad_w = w + 1;
        let pad_h = h + 1;
        // shelf height buckets of 4 px keep rows reusable across sizes
        let bucket = pad_h.div_ceil(4) * 4;
        for s in &mut self.shelves {
            if s.height == bucket && s.next_x + pad_w <= self.size {
                let x = s.next_x;
                s.next_x += pad_w;
                return Some((x, s.y));
            }
        }
        let y = self.shelves.last().map(|s| s.y + s.height).unwrap_or(0);
        if y + bucket > self.size || pad_w > self.size {
            return None;
        }
        self.shelves.push(Shelf {
            y,
            height: bucket,
            next_x: pad_w,
        });
        Some((0, y))
    }

    /// Returns the glyph, rasterising through `cache` on a miss. `None` for empty
    /// glyphs (spaces) and when the atlas is full (the caller clears and retries).
    pub fn get(
        &mut self,
        font_system: &mut FontSystem,
        cache: &mut SwashCache,
        key: CacheKey,
    ) -> Result<Option<AtlasGlyph>, AtlasFull> {
        if let Some(e) = self.entries.get(&key) {
            return Ok(*e);
        }
        let Some(image) = cache.get_image_uncached(font_system, key) else {
            self.entries.insert(key, None);
            return Ok(None);
        };
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            self.entries.insert(key, None);
            return Ok(None);
        }
        let mask = |a: u8| [255, 255, 255, a];
        let (data, color): (Vec<u8>, bool) = match image.content {
            SwashContent::Mask => (image.data.iter().flat_map(|a| mask(*a)).collect(), false),
            SwashContent::SubpixelMask => (
                image.data.chunks(3).flat_map(|c| mask(c[1])).collect(),
                false,
            ),
            SwashContent::Color => (
                image
                    .data
                    .chunks(4)
                    .flat_map(|c| {
                        // premultiplied, as the shader expects
                        let a = c[3] as u32;
                        [
                            (c[0] as u32 * a / 255) as u8,
                            (c[1] as u32 * a / 255) as u8,
                            (c[2] as u32 * a / 255) as u8,
                            c[3],
                        ]
                    })
                    .collect(),
                true,
            ),
        };
        let Some((x, y)) = self.alloc(w, h) else {
            return Err(AtlasFull);
        };
        let s = self.size as f32;
        let g = AtlasGlyph {
            uv: [
                x as f32 / s,
                y as f32 / s,
                (x + w) as f32 / s,
                (y + h) as f32 / s,
            ],
            w,
            h,
            left: image.placement.left,
            top: image.placement.top,
            color,
        };
        self.bytes_used += data.len();
        self.pending.push((x, y, w, h, data));
        self.entries.insert(key, Some(g));
        Ok(Some(g))
    }

    pub fn take_pending(&mut self) -> Vec<(u32, u32, u32, u32, Vec<u8>)> {
        std::mem::take(&mut self.pending)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasFull;

/// A positioned glyph on one line, in *unscaled* layout units (logical px at the
/// requested font size) plus its rasterised cache key at the device scale.
#[derive(Debug, Clone, Copy)]
pub struct PlacedGlyph {
    pub key: CacheKey,
    /// Device-pixel integer position relative to the line origin.
    pub x: i32,
    pub y: i32,
    /// Char index range of the cluster this glyph belongs to.
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
pub struct ShapedLine {
    pub glyphs: Vec<PlacedGlyph>,
    /// Advance width in device pixels.
    pub width: f32,
}

/// Font metrics for one (font size, scale) pair, device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub font_size: f32,
    pub scale: f32,
    /// Advance of one monospace cell.
    pub cell_w: f32,
    pub line_h: f32,
    pub ascent: f32,
}

pub struct TextSystem {
    pub font_system: FontSystem,
    pub swash: SwashCache,
    pub atlas: GlyphAtlas,
    attrs: AttrsList,
    shape_cache: HashMap<u64, Arc<ShapedLine>>,
    shape_hits: u64,
    shape_misses: u64,
    cell_cache: HashMap<u32, f32>,
    ascent_ratio: f32,
    descent_ratio: f32,
}

impl std::fmt::Debug for TextSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextSystem")
            .field("atlas", &self.atlas)
            .field("shape_cache", &self.shape_cache.len())
            .finish()
    }
}

fn font_database() -> fontdb::Database {
    #[cfg(not(target_arch = "wasm32"))]
    {
        static DB: std::sync::OnceLock<fontdb::Database> = std::sync::OnceLock::new();
        DB.get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_font_source(fontdb::Source::Binary(Arc::new(FONT_BYTES)));
            if std::env::var_os("FRONTIER_EDITOR_NO_SYSTEM_FONTS").is_none() {
                db.load_system_fonts();
            }
            db
        })
        .clone()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let mut db = fontdb::Database::new();
        db.load_font_source(fontdb::Source::Binary(Arc::new(FONT_BYTES)));
        db
    }
}

impl TextSystem {
    /// Width of one cell as a share of the font size (the embedded face is monospace).
    pub fn cell_ratio(&mut self) -> f32 {
        self.cell_width(200.0) / 200.0
    }

    pub fn new() -> Self {
        // The embedded face decides the metrics on every host. On native hosts the
        // system's fonts stand in for what it lacks (CJK, emoji); they are scanned once
        // per process. A browser host has no access to them.
        let mut fs = FontSystem::new_with_locale_and_db("en-US".to_string(), font_database());
        let attrs = Attrs::new().family(Family::Name(FONT_FAMILY));
        let (ascent_ratio, descent_ratio) = {
            let id = fs.db().faces().next().map(|f| f.id);
            match id.and_then(|id| fs.get_font(id, fontdb::Weight::NORMAL)) {
                Some(font) => {
                    let m = font.metrics();
                    let units = m.units_per_em as f32;
                    (m.ascent / units, m.descent.abs() / units)
                }
                None => (0.93, 0.24),
            }
        };
        Self {
            font_system: fs,
            swash: SwashCache::new(),
            atlas: GlyphAtlas::new(1024),
            attrs: AttrsList::new(&attrs),
            shape_cache: HashMap::new(),
            shape_hits: 0,
            shape_misses: 0,
            cell_cache: HashMap::new(),
            ascent_ratio,
            descent_ratio,
        }
    }

    /// Advance of one cell at device font size `px`, measured by shaping.
    fn cell_width(&mut self, px: f32) -> f32 {
        if let Some(w) = self.cell_cache.get(&px.to_bits()) {
            return *w;
        }
        let sample = "M".repeat(100);
        let shaped = ShapeLine::new(
            &mut self.font_system,
            &sample,
            &self.attrs,
            Shaping::Advanced,
            4,
        );
        let lines = shaped.layout(px, None, Wrap::None, None, None, Hinting::Disabled);
        // the mean advance as glyphs are actually placed (integer device pixels)
        let w = lines
            .first()
            .and_then(|l| {
                l.glyphs
                    .get(99)
                    .map(|g| g.physical((0.0, 0.0), 1.0).x as f32 / 99.0)
            })
            .unwrap_or(px * 0.6)
            .max(1.0);
        self.cell_cache.insert(px.to_bits(), w);
        w
    }

    /// Metrics at `font_size` logical px drawn at `scale` (DPR × host zoom).
    pub fn metrics(&mut self, font_size: f32, scale: f32) -> Metrics {
        let px = (font_size * scale * 4.0).round() / 4.0;
        let cell_w = self.cell_width(px);
        let ascent = (self.ascent_ratio * px).round();
        let descent = (self.descent_ratio * px).round();
        let line_h = (ascent + descent + (px * 0.18).round()).max(1.0);
        Metrics {
            font_size,
            scale,
            cell_w,
            line_h,
            ascent,
        }
    }

    pub fn shape_stats(&self) -> (u64, u64, usize) {
        (self.shape_hits, self.shape_misses, self.shape_cache.len())
    }

    /// Shapes `text` at device font size `px` with the monospace cell width forced,
    /// so columns stay aligned even for glyphs the font substitutes.
    pub fn shape(&mut self, text: &str, px: f32, cell_w: f32) -> Arc<ShapedLine> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        px.to_bits().hash(&mut h);
        let key = h.finish();
        if let Some(s) = self.shape_cache.get(&key) {
            self.shape_hits += 1;
            return s.clone();
        }
        self.shape_misses += 1;
        if self.shape_cache.len() > 20_000 {
            self.shape_cache.clear();
        }
        let shaped = ShapeLine::new(
            &mut self.font_system,
            text,
            &self.attrs,
            Shaping::Advanced,
            4,
        );
        let lines = shaped.layout(px, None, Wrap::None, None, Some(cell_w), Hinting::Disabled);
        let mut glyphs = Vec::new();
        let mut width = 0.0f32;
        for line in lines {
            width = width.max(line.w);
            for g in &line.glyphs {
                let p = g.physical((0.0, 0.0), 1.0);
                glyphs.push(PlacedGlyph {
                    key: p.cache_key,
                    x: p.x,
                    y: p.y,
                    start: g.start,
                    end: g.end,
                });
            }
        }
        let out = Arc::new(ShapedLine { glyphs, width });
        self.shape_cache.insert(key, out.clone());
        out
    }

    pub fn invalidate_shapes(&mut self) {
        self.shape_cache.clear();
    }
}

impl Default for TextSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_monospace_and_scale() {
        let mut ts = TextSystem::new();
        let m1 = ts.metrics(13.0, 1.0);
        let m2 = ts.metrics(13.0, 2.0);
        assert!(m1.cell_w >= 7.0 && m1.cell_w <= 9.0, "{m1:?}");
        assert!((m2.cell_w - m1.cell_w * 2.0).abs() <= 0.5, "{m1:?} {m2:?}");
        assert!(m1.line_h > m1.ascent);
    }

    #[test]
    fn shaping_places_one_glyph_per_cell() {
        let mut ts = TextSystem::new();
        let m = ts.metrics(13.0, 2.0);
        let s = ts.shape("ab: 12", m.font_size * m.scale, m.cell_w);
        assert_eq!(s.glyphs.len(), 6);
        for (i, g) in s.glyphs.iter().enumerate() {
            assert!(
                (g.x as f32 - i as f32 * m.cell_w).abs() < 1.0,
                "glyph {i}: {} vs {}",
                g.x,
                i as f32 * m.cell_w
            );
        }
        let again = ts.shape("ab: 12", m.font_size * m.scale, m.cell_w);
        assert!(Arc::ptr_eq(&s, &again));
    }

    #[test]
    fn atlas_rasterises_and_packs() {
        let mut ts = TextSystem::new();
        let m = ts.metrics(13.0, 2.0);
        let s = ts.shape("Hello", m.font_size * m.scale, m.cell_w);
        let TextSystem {
            font_system,
            swash,
            atlas,
            ..
        } = &mut ts;
        for g in &s.glyphs {
            let a = atlas
                .get(font_system, swash, g.key)
                .expect("fits")
                .expect("ink");
            assert!(a.w > 0 && a.h > 0);
            assert!(a.uv[2] <= 1.0 && a.uv[3] <= 1.0);
        }
        assert!(
            atlas.len() >= 4 && atlas.len() <= 5,
            "l is shared up to subpixel bins"
        );
        assert_eq!(atlas.take_pending().len(), atlas.len());
    }
}
