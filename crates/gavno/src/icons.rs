//! Icon theme lookup and bounded, size-specific premultiplied raster caches.
pub(crate) mod app_worker;
use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::Transform,
};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
};

/// Upper bound on SVG bytes handed to the parser.
const MAX_SVG_BYTES: u64 = 8 * 1024 * 1024;

/// Prefer the configured theme, its inherited themes, then hicolor and pixmaps.
pub fn lookup(name: &str, theme: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let absolute = Path::new(name);
    if absolute.is_absolute() {
        return absolute.is_file().then(|| absolute.to_owned());
    }
    if name.is_empty() || name.contains('/') || theme.contains('/') {
        return None;
    }
    fn visit(theme: &str, dirs: &[PathBuf], seen: &mut HashSet<String>, themes: &mut Vec<String>) {
        if themes.len() >= 24 || !seen.insert(theme.into()) || theme.contains('/') {
            return;
        }
        themes.push(theme.into());
        for dir in dirs {
            if let Ok(index) =
                std::fs::read_to_string(dir.join("icons").join(theme).join("index.theme"))
            {
                if let Some(inherits) = index.lines().find_map(|l| l.strip_prefix("Inherits=")) {
                    for parent in inherits.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        visit(parent, dirs, seen, themes);
                    }
                }
            }
        }
    }
    let mut themes = Vec::new();
    let mut seen = HashSet::new();
    visit(theme, dirs, &mut seen, &mut themes);
    visit("hicolor", dirs, &mut seen, &mut themes);
    for theme in themes {
        let mut sizes = vec![
            "256x256".to_string(),
            "128x128".into(),
            "scalable".into(),
            "512x512".into(),
            "64x64".into(),
            "48x48".into(),
            "32x32".into(),
            "symbolic".into(),
        ];
        for dir in dirs {
            if let Ok(index) =
                std::fs::read_to_string(dir.join("icons").join(&theme).join("index.theme"))
            {
                for subdir in index
                    .lines()
                    .filter_map(|l| l.strip_prefix("Directories="))
                    .flat_map(|l| l.split(','))
                {
                    if !subdir.contains("..") && !subdir.starts_with('/') {
                        sizes.push(subdir.trim().into());
                    }
                }
            }
        }
        for size in sizes {
            for dir in dirs {
                for context in ["apps", "applications", ""] {
                    let base = dir.join("icons").join(&theme).join(&size).join(context);
                    for ext in ["png", "svg"] {
                        let p = base.join(if name.ends_with(".png") || name.ends_with(".svg") {
                            name.to_owned()
                        } else {
                            format!("{name}.{ext}")
                        });
                        if p.is_file() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    for dir in dirs {
        for ext in ["png", "svg"] {
            let p = dir.join("pixmaps").join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[derive(Default)]
pub struct Icons {
    cache: HashMap<(String, u32, bool), Option<MemoryRenderBuffer>>,
    async_apps: bool,
    paths: HashMap<String, Option<PathBuf>>,
    colored: HashMap<(String, u32, [u8; 3]), Option<MemoryRenderBuffer>>,
}
impl Icons {
    pub(crate) fn nonblocking_apps(&mut self) { self.async_apps = true; }
    pub fn get(&mut self, name: &str, size: u32, theme: &str) -> Option<MemoryRenderBuffer> {
        self.load(name, size, theme, false)
    }
    /// Symbolic icons are dark-on-transparent; bars ask for premultiplied white.
    pub fn get_tinted(&mut self, name: &str, size: u32, theme: &str) -> Option<MemoryRenderBuffer> {
        self.load(name, size, theme, true)
    }
    /// Cache an alert tint separately from the normal dark-shell white tint.
    pub fn get_tinted_color(&mut self, name: &str, size: u32, theme: &str, color: [u8; 3]) -> Option<MemoryRenderBuffer> {
        let size = size.clamp(1, 1024);
        let key = (name.to_owned(), size, color);
        if let Some(buffer) = self.colored.get(&key) { return buffer.clone(); }
        if self.async_apps && app_worker::enabled() {
            let buffer = app_worker::request(name, size, theme, true, Some(color))?;
            if self.colored.len() > 128 { self.colored.clear(); }
            self.colored.insert(key, buffer.clone());
            return buffer;
        }
        self.get_tinted(name, size, theme)?;
        let buffer = self.paths.get(name).and_then(Option::as_ref).and_then(|p| raster_color(p, size, true, Some(color)));
        if self.colored.len() > 128 { self.colored.clear(); }
        self.colored.insert(key, buffer.clone());
        buffer
    }
    fn load(
        &mut self,
        name: &str,
        size: u32,
        theme: &str,
        tint: bool,
    ) -> Option<MemoryRenderBuffer> {
        let size = size.clamp(1, 1024);
        let key = (name.to_owned(), size, tint);
        if let Some(buffer) = self.cache.get(&key) {
            return buffer.clone();
        }
        if self.async_apps && app_worker::enabled() {
            let buffer = app_worker::request(name, size, theme, tint, None)?;
            if self.cache.len() > 512 { self.cache.clear(); }
            self.cache.insert(key, buffer.clone());
            return buffer;
        }
        let path = self.paths.entry(name.into()).or_insert_with(|| lookup_icon(name, theme));
        let buffer = path.as_ref().and_then(|p| raster(p, size, tint));
        // Magnification uses a stable maximum raster, so this remains small.
        if self.cache.len() > 512 {
            self.cache.clear();
        }
        self.cache.insert(key, buffer.clone());
        buffer
    }
}
fn lookup_icon(name: &str, theme: &str) -> Option<PathBuf> {
    let _startup = crate::perf::Startup::new("icon.theme_lookup.begin", "icon.theme_lookup.end");
    let mut dirs = crate::desktop_entry::data_dirs();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.insert(0, PathBuf::from(home).join(".icons").parent()?.to_owned());
    }
    lookup(name, theme, &dirs)
}
/// Replace premultiplied RGBA color with white while keeping the original alpha.
fn tint_white(pixels: &mut [u8]) {
    for p in pixels.chunks_exact_mut(4) {
        let alpha = p[3];
        p[..3].fill(alpha);
    }
}
/// Read an SVG while bounding the bytes handed to the parser. Gzip-compressed
/// SVG is rejected instead of decompressed, since its expanded size is unknown.
fn read_svg(path: &Path) -> Option<Vec<u8>> {
    if path.extension().is_some_and(|e| e == "svgz") {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = file.take(MAX_SVG_BYTES + 1);
    let mut data = Vec::new();
    reader.read_to_end(&mut data).ok()?;
    if data.len() as u64 > MAX_SVG_BYTES || data.starts_with(&[0x1f, 0x8b]) {
        return None;
    }
    Some(data)
}
fn raster(path: &Path, size: u32, tint: bool) -> Option<MemoryRenderBuffer> {
    raster_color(path, size, tint, None)
}
fn raster_color(path: &Path, size: u32, tint: bool, color: Option<[u8; 3]>) -> Option<MemoryRenderBuffer> {
    let _startup = crate::perf::Startup::new("image.decode.begin", "image.decode.end");
    if crate::perf::in_startup() { tracing::info!(target: "gavno::startup", stage = "image.decode.path", ms = crate::perf::elapsed_ms(), path = %path.display(), size); }
    let mut pixels = if path
        .extension()
        .is_some_and(|e| e == "svg" || e == "svgz")
    {
        let data = read_svg(path)?;
        crate::perf::startup("image.svg.read");
        let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
        crate::perf::startup("image.svg.parsed");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
        let s = (size as f32 / tree.size().width()).min(size as f32 / tree.size().height());
        let t = resvg::tiny_skia::Transform::from_row(
            s,
            0.0,
            0.0,
            s,
            (size as f32 - tree.size().width() * s) * 0.5,
            (size as f32 - tree.size().height() * s) * 0.5,
        );
        resvg::render(&tree, t, &mut pixmap.as_mut());
        crate::perf::startup("image.svg.rasterized");
        pixmap.take()
    } else {
        let mut decoder = png::Decoder::new(std::fs::File::open(path).ok()?);
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().ok()?;
        let (w, h) = (reader.info().width, reader.info().height);
        if w == 0 || h == 0 || w > 4096 || h > 4096 {
            return None;
        }
        let mut data = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut data).ok()?;
        crate::perf::startup("image.png.decoded");
        let channels = match info.color_type {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Grayscale => 1,
            _ => return None,
        };
        let factor = (size as f64 / w as f64).min(size as f64 / h as f64);
        let dw = (w as f64 * factor).round().max(1.0) as u32;
        let dh = (h as f64 * factor).round().max(1.0) as u32;
        let mut pixels = vec![0; (size * size * 4) as usize];
        for y in 0..dh {
            for x in 0..dw {
                let sx = (x as f64 / factor).floor().min((w - 1) as f64) as usize;
                let sy = (y as f64 / factor).floor().min((h - 1) as f64) as usize;
                let src = &data[(sy * w as usize + sx) * channels..];
                let (r, g, b, a) = match channels {
                    4 => (src[0], src[1], src[2], src[3]),
                    3 => (src[0], src[1], src[2], 255),
                    2 => (src[0], src[0], src[0], src[1]),
                    _ => (src[0], src[0], src[0], 255),
                };
                let i = (((y + (size - dh) / 2) * size + x + (size - dw) / 2) * 4) as usize;
                pixels[i..i + 4].copy_from_slice(&[
                    (r as u16 * a as u16 / 255) as u8,
                    (g as u16 * a as u16 / 255) as u8,
                    (b as u16 * a as u16 / 255) as u8,
                    a,
                ]);
            }
        }
        pixels
    };
    if tint {
        tint_white(&mut pixels);
    }
    for p in pixels.chunks_exact_mut(4) {
        if let Some(color) = color {
            for i in 0..3 { p[i] = (color[i] as u16 * p[3] as u16 / 255) as u8; }
        }
        p.swap(0, 2);
    }
    Some(MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (size as i32, size as i32),
        1,
        Transform::Normal,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_size_and_fallback_order() {
        let root = std::env::temp_dir().join(format!("gavno-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for p in [
            "icons/test/128x128/apps/app.png",
            "icons/test/scalable/apps/app.svg",
            "icons/hicolor/256x256/apps/app.png",
        ] {
            let p = root.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"").unwrap();
        }
        assert_eq!(
            lookup("app", "test", &[root.clone()]),
            Some(root.join("icons/test/128x128/apps/app.png"))
        );
        std::fs::remove_file(root.join("icons/test/128x128/apps/app.png")).unwrap();
        assert_eq!(
            lookup("app", "test", &[root.clone()]),
            Some(root.join("icons/test/scalable/apps/app.svg"))
        );
        assert_eq!(
            lookup("app", "missing", &[root.clone()]),
            Some(root.join("icons/hicolor/256x256/apps/app.png"))
        );
        assert!(lookup("../escape", "test", &[root.clone()]).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn white_tint_keeps_alpha_and_premultiplies() {
        let mut pixels = [
            10, 20, 30, 255, // opaque colored pixel
            1, 2, 3, 128, // translucent colored pixel
            7, 8, 9, 0, // fully transparent pixel
        ];
        tint_white(&mut pixels);
        assert_eq!(
            pixels,
            [255, 255, 255, 255, 128, 128, 128, 128, 0, 0, 0, 0]
        );
    }
    #[test]
    fn svg_reads_are_bounded_and_reject_gzip() {
        let root = std::env::temp_dir().join(format!("gavno-icons-svg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let good = root.join("good.svg");
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        std::fs::write(&good, svg).unwrap();
        assert_eq!(read_svg(&good).unwrap().as_slice(), svg);

        // Oversized files are rejected without reading the whole file.
        let oversized = root.join("oversized.svg");
        std::fs::write(&oversized, vec![b' '; MAX_SVG_BYTES as usize + 1]).unwrap();
        assert!(read_svg(&oversized).is_none());

        // Both the .svgz extension and the gzip magic are rejected before the
        // parser can decompress untrusted data.
        let gzip = root.join("gzip.svg");
        std::fs::write(&gzip, [0x1f, 0x8b, 0x08, 0x00]).unwrap();
        assert!(read_svg(&gzip).is_none());
        let svgz = root.join("plain.svgz");
        std::fs::write(&svgz, svg).unwrap();
        assert!(read_svg(&svgz).is_none());

        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Music covers are decoded only on the music worker, then uploaded by the renderer.
pub(crate) fn music_cover(path: &str) -> Option<MemoryRenderBuffer> {
    let path = Path::new(path);
    if !path.is_absolute() || path.extension()?.to_str()? != "png" || std::fs::metadata(path).ok()?.len() > 16 * 1024 * 1024 { return None; }
    raster(path, 96, false)
}
