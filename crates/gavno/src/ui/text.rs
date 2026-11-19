//! Text rasterized on the CPU into a premultiplied ARGB buffer.
//!
//! Text in GAVNO is small and changes rarely (labels, a clock that ticks once a
//! minute), so each string is rasterized once per (text, size, weight,
//! scale) and cached as a GPU-uploadable buffer.

use std::{cell::RefCell, collections::{HashMap, HashSet}, path::PathBuf, sync::{Arc, Weak}};

use fontdue::{
    layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle},
    Font, FontSettings,
};
use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::Transform,
};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weight {
    Thin,
    Light,
    Regular,
    Medium,
    SemiBold,
}

// Canvases live on the renderer thread. Share immutable parsed fonts there,
// without locks or retaining fonts after the last canvas releases them. A new
// window's labels must not rescan/read/parse fonts the shell already uses.
thread_local! {
    static SHARED_FONTS: RefCell<HashMap<Weight, Weak<Font>>> = RefCell::new(HashMap::new());
}

/// Font files by weight. IBM Plex Sans first, Liberation Sans as the fallback
/// that is always installed.
fn candidates(weight: Weight) -> &'static [&'static str] {
    match weight {
        Weight::Thin => &["IBMPlexSans-ExtraLight", "IBMPlexSans-Thin", "IBMPlexSans-Light", "LiberationSans-Regular"],
        Weight::Light => &["IBMPlexSans-Light", "LiberationSans-Regular"],
        Weight::Regular => &["IBMPlexSans-Regular", "LiberationSans-Regular"],
        Weight::Medium => &["IBMPlexSans-Medium", "IBMPlexSans-Regular", "LiberationSans-Bold"],
        Weight::SemiBold => &["IBMPlexSans-SemiBold", "IBMPlexSans-Medium", "LiberationSans-Bold"],
    }
}

fn find_font_file(stem: &str) -> Option<PathBuf> {
    let dirs = ["/usr/share/fonts", "/usr/local/share/fonts"];
    fn walk(dir: &std::path::Path, stem: &str, depth: u32) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth < 8 {
                    subdirs.push(path);
                }
            } else if path.file_stem().and_then(|s| s.to_str()) == Some(stem)
                && matches!(path.extension().and_then(|e| e.to_str()), Some("ttf" | "otf"))
            {
                return Some(path);
            }
        }
        subdirs.into_iter().find_map(|d| walk(&d, stem, depth + 1))
    }
    dirs.iter().find_map(|d| walk(std::path::Path::new(d), stem, 0))
}

#[derive(Debug, Clone, Copy)]
pub struct TextStyleSpec {
    pub size: f32,
    pub weight: Weight,
    /// Straight (not premultiplied) RGBA.
    pub color: [f32; 4],
    /// Extra spacing between letters, in logical px.
    pub tracking: f32,
}

/// A rasterized string: the buffer plus its logical size.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TextImage {
    pub buffer: MemoryRenderBuffer,
    /// Size in physical pixels of the raster.
    pub width: i32,
    pub height: i32,
    /// Logical size to draw it at. The raster is padded so that
    /// `round(logical * scale) == physical` exactly, which keeps glyphs
    /// pixel-sharp at fractional scales.
    pub logical_w: i32,
    pub logical_h: i32,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    text: String,
    size_milli: u32,
    weight: Weight,
    color: [u8; 4],
    tracking_milli: i32,
    scale_milli: u32,
}

pub struct TextRenderer {
    fonts: HashMap<Weight, Arc<Font>>,
    cache: HashMap<Key, TextImage>,
    /// Scale of the last render; `measure` lays text out at it, because the
    /// rasterizer snaps every glyph to whole physical pixels.
    measure_scale: f64,
    /// Opt-in scopes used by cached shells to release obsolete raster images.
    frame_keys: Option<HashSet<Key>>,
}

impl TextRenderer {
    pub fn new() -> Self {
        Self {
            fonts: HashMap::new(),
            cache: HashMap::new(),
            measure_scale: 1.0,
            frame_keys: None,
        }
    }

    pub(crate) fn begin_frame_cache(&mut self) {
        self.frame_keys = Some(HashSet::new());
    }
    pub(crate) fn end_frame_cache(&mut self) {
        if let Some(keys) = self.frame_keys.take() {
            self.cache.retain(|key, _| keys.contains(key));
        }
    }
    pub(crate) fn clear_images(&mut self) { self.cache.clear(); }

    fn font(&mut self, weight: Weight) -> Option<Arc<Font>> {
        if let Some(f) = self.fonts.get(&weight) {
            return Some(f.clone());
        }
        if let Some(font) = SHARED_FONTS.with(|fonts| fonts.borrow().get(&weight).and_then(Weak::upgrade)) {
            self.fonts.insert(weight, font.clone());
            return Some(font);
        }
        let _startup = crate::perf::Startup::new("font.load.begin", "font.load.end");
        let font = candidates(weight).iter().find_map(|stem| {
            let path = find_font_file(stem)?;
            if crate::perf::in_startup() { tracing::info!(target: "gavno::startup", stage = "font.found", ms = crate::perf::elapsed_ms(), ?weight, path = %path.display()); }
            let data = std::fs::read(&path).ok()?;
            crate::perf::startup("font.read");
            let font = Font::from_bytes(data, FontSettings::default()).ok()?;
            if crate::perf::in_startup() { tracing::info!(target: "gavno::startup", stage = "font.parsed", ms = crate::perf::elapsed_ms(), ?weight, path = %path.display()); }
            tracing::debug!(?path, ?weight, "font loaded");
            Some(Arc::new(font))
        })?;
        SHARED_FONTS.with(|fonts| { fonts.borrow_mut().insert(weight, Arc::downgrade(&font)); });
        self.fonts.insert(weight, font.clone());
        Some(font)
    }

    /// Logical width of `text`, for layout.
    pub fn measure(&mut self, text: &str, style: TextStyleSpec) -> f32 {
        let Some(font) = self.font(style.weight) else { return 0.0 };
        let mut advance = 0.0;
        let count = text.chars().count();
        for (i, ch) in text.chars().enumerate() {
            advance += font.metrics(ch, style.size).advance_width;
            if i + 1 < count {
                advance += style.tracking;
            }
        }
        // The drawn width: glyph positions are snapped to physical pixels,
        // which makes a line several percent wider than its advances.
        let scale = self.measure_scale as f32;
        let tracking = style.tracking * scale;
        let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
        layout.reset(&LayoutSettings::default());
        layout.append(&[font.as_ref()], &TextStyle::new(text, style.size * scale, 0));
        let ink = layout
            .glyphs()
            .iter()
            .enumerate()
            .map(|(i, g)| g.x + i as f32 * tracking + g.width as f32)
            .fold(0.0f32, f32::max);
        advance.max(ink / scale)
    }

    /// Cell width of one digit: the widest digit of the font at this style, so
    /// a ticking clock never shifts its neighbours.
    pub fn digit_cell(&mut self, style: TextStyleSpec) -> f32 {
        ('0'..='9')
            .map(|d| self.measure(d.encode_utf8(&mut [0; 4]), style))
            .fold(0.0, f32::max)
    }

    /// Per-character x offsets of `text` with every digit in a fixed cell
    /// (digits centred in it), and the total logical width.
    pub fn tabular_cells(&mut self, text: &str, style: TextStyleSpec) -> (Vec<(char, f32)>, f32) {
        let cell = self.digit_cell(style);
        let mut x = 0.0;
        let mut cells = Vec::with_capacity(text.len());
        for ch in text.chars() {
            let own = self.measure(ch.encode_utf8(&mut [0; 4]), style);
            if ch.is_ascii_digit() {
                cells.push((ch, x + (cell - own) / 2.0));
                x += cell + style.tracking;
            } else {
                cells.push((ch, x));
                x += own + style.tracking;
            }
        }
        (cells, (x - style.tracking).max(0.0))
    }

    /// Rasterizes `text` at `scale` (physical px per logical px).
    pub fn render(&mut self, text: &str, style: TextStyleSpec, scale: f64) -> Option<TextImage> {
        let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let key = Key {
            text: text.to_owned(),
            size_milli: (style.size * 1000.0) as u32,
            weight: style.weight,
            color: [to8(style.color[0]), to8(style.color[1]), to8(style.color[2]), to8(style.color[3])],
            tracking_milli: (style.tracking * 1000.0) as i32,
            scale_milli: (scale * 1000.0) as u32,
        };
        if let Some(keys) = &mut self.frame_keys { keys.insert(key.clone()); }
        if let Some(img) = self.cache.get(&key) {
            return Some(img.clone());
        }
        if self.cache.len() >= 512 { self.cache.clear(); }
        self.measure_scale = scale.max(0.5);
        let font = self.font(style.weight)?;
        let px = style.size * scale as f32;
        let tracking = style.tracking * scale as f32;

        let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
        layout.reset(&LayoutSettings::default());
        layout.append(&[font.as_ref()], &TextStyle::new(text, px, 0));
        let glyphs = layout.glyphs().clone();
        let line_height = layout.height().ceil().max(1.0);

        // Letter spacing shifts glyph i by i * tracking.
        let ink_width = glyphs
            .iter()
            .enumerate()
            .map(|(i, g)| g.x + i as f32 * tracking + g.width as f32)
            .fold(0.0f32, f32::max)
            .ceil() as i32
            + 2;
        if ink_width <= 2 || line_height <= 0.0 {
            return None;
        }
        let logical_w = (ink_width as f64 / scale).ceil() as i32;
        let logical_h = (line_height as f64 / scale).ceil() as i32;
        let width = (logical_w as f64 * scale).round() as i32;
        let height = (logical_h as f64 * scale).round() as i32;

        let mut coverage = vec![0u8; (width * height) as usize];
        for (i, g) in glyphs.iter().enumerate() {
            if g.width == 0 || g.height == 0 {
                continue;
            }
            let (metrics, bitmap) = font.rasterize_config(g.key);
            let ox = (g.x + i as f32 * tracking).round() as i32 + 1;
            let oy = g.y.round() as i32;
            for row in 0..metrics.height as i32 {
                for col in 0..metrics.width as i32 {
                    let (x, y) = (ox + col, oy + row);
                    if x < 0 || y < 0 || x >= width || y >= height {
                        continue;
                    }
                    let c = bitmap[(row * metrics.width as i32 + col) as usize];
                    let dst = &mut coverage[(y * width + x) as usize];
                    *dst = (*dst).max(c);
                }
            }
        }

        // ARGB8888 little endian = B, G, R, A bytes; premultiplied.
        let [r, g, b, a] = style.color;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for (i, c) in coverage.iter().enumerate() {
            if *c == 0 {
                continue;
            }
            let alpha = (*c as f32 / 255.0) * a;
            pixels[i * 4] = to8(b * alpha);
            pixels[i * 4 + 1] = to8(g * alpha);
            pixels[i * 4 + 2] = to8(r * alpha);
            pixels[i * 4 + 3] = to8(alpha);
        }
        let buffer = MemoryRenderBuffer::from_slice(&pixels, Fourcc::Argb8888, (width, height), 1, Transform::Normal, None);
        let img = TextImage {
            buffer,
            width,
            height,
            logical_w,
            logical_h,
        };

        // Bound the cache: strings like the clock change over time.
        if self.cache.len() > 256 {
            self.cache.clear();
        }
        self.cache.insert(key, img.clone());
        Some(img)
    }
}

#[cfg(test)]
mod measure_tests {
    use super::*;
    #[test]
    fn tabular_text_has_the_same_width_for_every_digit_string() {
        let mut t = TextRenderer::new();
        let style = crate::topbar::label_style(14.0);
        if t.font(style.weight).is_none() { return; }
        let widths: Vec<f32> = ["0:00", "1:11", "8:88", "4:57"]
            .iter()
            .map(|s| t.tabular_cells(s, style).1)
            .collect();
        assert!(widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.01), "{widths:?}");
        let (cells, _) = t.tabular_cells("12:30", style);
        assert_eq!(cells.len(), 5);
        assert!(cells.windows(2).all(|w| w[1].1 > w[0].1));
    }
    #[test]
    fn canvases_share_parsed_fonts_and_release_them_with_their_last_owner() {
        let mut first = TextRenderer::new();
        let mut second = TextRenderer::new();
        let Some(font) = first.font(Weight::Medium) else { return };
        let shared = second.font(Weight::Medium).unwrap();
        assert!(Arc::ptr_eq(&font, &shared));
        let weak = Arc::downgrade(&font);
        drop(font); drop(shared); drop(first); drop(second);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn measure_never_underestimates_the_rendered_width() {
        let mut t = TextRenderer::new();
        let style = crate::topbar::label_style(12.0);
        let text = "finally gets cut off with an ellipsis at the end of the third line";
        for scale in [1.0, 1.75, 2.0] {
            let Some(image) = t.render(text, style, scale) else { return };
            // render() pads by 2 physical px and rounds up to a logical px.
            assert!(t.measure(text, style) + 2.0 >= image.logical_w as f32, "scale {scale}");
        }
    }
}
