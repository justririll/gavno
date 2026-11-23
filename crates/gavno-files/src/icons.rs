//! Folder and file icons rendered once to cached 2x HiDPI textures.
//! Geometry is authored on a 48x48 grid and scaled to the target pixel size.
use gtk::{cairo, gdk, prelude::*};
use std::{cell::RefCell, collections::HashMap, f64::consts::PI, path::Path, rc::Rc};

const FOLDER: Rgb = rgb(0x7f8be6);
const FOLDER_BACK: Rgb = rgb(0x5d68c8);
const FOLDER_INK: (f64, f64, f64, f64) = (20. / 255., 22. / 255., 40. / 255., 0.85);
const FILE: Rgb = rgb(0x262a3a);
const FILE_FOLD: Rgb = rgb(0x30354a);
const FILE_LINE: Rgb = rgb(0x3c4259);

type Rgb = (f64, f64, f64);
const fn rgb(v: u32) -> Rgb {
    (
        ((v >> 16) & 0xff) as f64 / 255.,
        ((v >> 8) & 0xff) as f64 / 255.,
        (v & 0xff) as f64 / 255.,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Glyph {
    None,
    Home,
    Documents,
    Downloads,
    Pictures,
    Music,
    Code,
    Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Code,
    Text,
    Image,
    Audio,
    Video,
    Archive,
    Disc,
    Web,
    Pdf,
    Binary,
}

impl Kind {
    fn color(self) -> Rgb {
        match self {
            Kind::Code => rgb(0x7b5ce0),
            Kind::Text | Kind::Binary => rgb(0x5d6680),
            Kind::Image => rgb(0xd4517a),
            Kind::Audio => rgb(0x329f7b),
            Kind::Video => rgb(0xb84fd4),
            Kind::Archive => rgb(0xc98a1c),
            Kind::Disc => rgb(0x1f9a8a),
            Kind::Web => rgb(0x3d7be0),
            Kind::Pdf => rgb(0xd94b4b),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Folder(Glyph),
    File { label: String, kind: Kind },
}

/// The glyph drawn on a well-known folder.
pub fn folder_glyph(path: &Path, home: &Path) -> Glyph {
    if path == home {
        return Glyph::Home;
    }
    if path == Path::new("/etc") || path == home.join(".config") {
        return Glyph::Config;
    }
    match path.strip_prefix(home).ok().and_then(|p| p.to_str()) {
        Some("Documents") => Glyph::Documents,
        Some("Downloads") => Glyph::Downloads,
        Some("Pictures") => Glyph::Pictures,
        Some("Music") => Glyph::Music,
        Some("Projects") => Glyph::Code,
        _ => Glyph::None,
    }
}

/// Badge label and colour for a file name; `text` says whether the content
/// type is textual, which decides between TXT and BIN when there is no extension.
pub fn file_icon(name: &str, text: bool) -> Icon {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|_| !name.starts_with('.') || name[1..].contains('.'))
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let kind = match ext.as_str() {
        "rs" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "py" | "js" | "mjs" | "ts"
        | "tsx" | "jsx" | "go" | "java" | "kt" | "kts" | "sh" | "bash" | "zsh" | "fish"
        | "toml" | "kdl" | "conf" | "json" | "jsonc" | "yaml" | "yml" | "ini" | "frag"
        | "vert" | "glsl" | "wgsl" | "lua" | "nix" | "zig" | "rb" | "pl" | "mk" | "cmake"
        | "sql" | "dart" | "swift" | "cs" | "php" | "vue" | "svelte" => Kind::Code,
        "md" | "txt" | "log" | "gen" | "rst" | "org" | "csv" | "tsv" | "tex" | "doc"
        | "docx" | "odt" | "rtf" | "xlsx" | "xls" | "pptx" | "ppt" => Kind::Text,
        "pdf" | "djvu" | "epub" => Kind::Pdf,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif" | "heic" | "ico"
        | "tiff" | "tif" | "jxl" | "psd" | "kra" => Kind::Image,
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "aac" | "m4a" | "wma" | "mid" | "midi" => {
            Kind::Audio
        }
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" => Kind::Video,
        "zip" | "xz" | "zst" | "gz" | "tar" | "7z" | "rar" | "bz2" | "tgz" | "lz4" | "deb"
        | "rpm" | "jar" | "apk" | "flatpak" | "appimage" => Kind::Archive,
        "img" | "iso" | "qcow2" | "raw" => Kind::Disc,
        "html" | "htm" | "css" | "scss" | "xml" => Kind::Web,
        "" if text => Kind::Text,
        "" => Kind::Binary,
        _ if text => Kind::Text,
        _ => Kind::Binary,
    };
    let label = if ext.is_empty() {
        if kind == Kind::Binary { "BIN" } else { "TXT" }.to_owned()
    } else {
        ext.to_uppercase().chars().take(4).collect()
    };
    Icon::File { label, kind }
}

/// Caches 2x supersampled icon textures keyed by `(Icon, logical_size)`.
#[derive(Clone)]
pub struct IconCache {
    textures: Rc<RefCell<HashMap<(Icon, i32), gdk::Texture>>>,
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IconCache {
    pub fn new() -> Self {
        Self {
            textures: Rc::new(RefCell::new(HashMap::with_capacity(64))),
        }
    }

    pub fn texture(&self, icon: &Icon, logical_size: i32) -> gdk::Texture {
        let key = (icon.clone(), logical_size);
        if let Some(tex) = self.textures.borrow().get(&key) {
            return tex.clone();
        }
        let tex = rasterize(icon, logical_size);
        let mut map = self.textures.borrow_mut();
        if map.len() >= 512 {
            map.clear();
        }
        map.insert(key, tex.clone());
        tex
    }
}

fn rasterize(icon: &Icon, logical_size: i32) -> gdk::Texture {
    let px = (logical_size * 2).max(16);
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, px, px)
        .expect("cairo icon surface");
    {
        let cr = cairo::Context::new(&surface).expect("cairo icon context");
        cr.scale(f64::from(px) / 48., f64::from(px) / 48.);
        let _ = draw(&cr, icon);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let mut bytes = None;
    let _ = surface.with_data(|slice| {
        bytes = Some(glib::Bytes::from(slice));
    });
    let bytes = bytes.unwrap_or_else(|| glib::Bytes::from_owned(vec![0u8; stride * px as usize]));
    gdk::MemoryTexture::new(
        px,
        px,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &bytes,
        stride,
    )
    .upcast()
}

fn draw(cr: &cairo::Context, icon: &Icon) -> Result<(), cairo::Error> {
    match icon {
        Icon::Folder(glyph) => draw_folder(cr, *glyph),
        Icon::File { label, kind } => draw_file(cr, label, *kind),
    }
}

fn set(cr: &cairo::Context, c: Rgb) {
    cr.set_source_rgb(c.0, c.1, c.2);
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2., 0.);
    cr.arc(x + w - r, y + h - r, r, 0., PI / 2.);
    cr.arc(x + r, y + h - r, r, PI / 2., PI);
    cr.arc(x + r, y + r, r, PI, 3. * PI / 2.);
    cr.close_path();
}

fn draw_folder(cr: &cairo::Context, glyph: Glyph) -> Result<(), cairo::Error> {
    cr.new_path();
    cr.move_to(8., 8.);
    cr.line_to(19., 8.);
    cr.line_to(23., 12.);
    cr.line_to(40., 12.);
    cr.arc(40., 16., 4., -PI / 2., 0.);
    cr.line_to(44., 36.);
    cr.arc(40., 36., 4., 0., PI / 2.);
    cr.line_to(8., 40.);
    cr.arc(8., 36., 4., PI / 2., PI);
    cr.line_to(4., 12.);
    cr.arc(8., 12., 4., PI, 3. * PI / 2.);
    cr.close_path();
    set(cr, FOLDER_BACK);
    cr.fill()?;
    rounded(cr, 4., 14., 40., 26., 4.);
    set(cr, FOLDER);
    cr.fill()?;
    draw_glyph(cr, glyph)
}

fn draw_glyph(cr: &cairo::Context, glyph: Glyph) -> Result<(), cairo::Error> {
    let (r, g, b, a) = FOLDER_INK;
    cr.set_source_rgba(r, g, b, a);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_line_width(2.);
    match glyph {
        Glyph::None => return Ok(()),
        Glyph::Home => {
            cr.move_to(18., 29.);
            cr.line_to(24., 24.);
            cr.line_to(30., 29.);
            cr.line_to(30., 35.);
            cr.line_to(26., 35.);
            cr.line_to(26., 31.);
            cr.line_to(22., 31.);
            cr.line_to(22., 35.);
            cr.line_to(18., 35.);
            cr.close_path();
            cr.fill()?;
        }
        Glyph::Documents => {
            for (y, w) in [(23., 8.), (27., 8.), (31., 5.)] {
                cr.move_to(20., y);
                cr.line_to(20. + w, y);
            }
            cr.stroke()?;
        }
        Glyph::Downloads => {
            cr.set_line_width(2.2);
            cr.move_to(24., 22.);
            cr.line_to(24., 32.);
            cr.move_to(20., 28.);
            cr.line_to(24., 32.);
            cr.line_to(28., 28.);
            cr.stroke()?;
        }
        Glyph::Pictures => {
            rounded(cr, 18., 22., 12., 10., 2.);
            cr.stroke()?;
            cr.set_line_width(1.6);
            cr.move_to(19., 31.);
            cr.line_to(23., 27.);
            cr.line_to(26., 30.);
            cr.line_to(28., 28.);
            cr.line_to(30., 30.);
            cr.stroke()?;
        }
        Glyph::Music => {
            cr.arc(20., 32., 2., 0., 2. * PI);
            cr.stroke()?;
            cr.move_to(22., 32.);
            cr.line_to(22., 23.);
            cr.line_to(29., 21.);
            cr.line_to(29., 29.);
            cr.stroke()?;
        }
        Glyph::Code => {
            cr.set_line_width(2.2);
            cr.move_to(21., 23.);
            cr.line_to(18., 27.);
            cr.line_to(21., 31.);
            cr.move_to(27., 23.);
            cr.line_to(30., 27.);
            cr.line_to(27., 31.);
            cr.stroke()?;
        }
        Glyph::Config => {
            cr.arc(24., 27., 3.2, 0., 2. * PI);
            cr.stroke()?;
            for (x0, y0, x1, y1) in [
                (24., 21.5, 24., 23.5),
                (24., 30.5, 24., 32.5),
                (18.5, 27., 20.5, 27.),
                (27.5, 27., 29.5, 27.),
            ] {
                cr.move_to(x0, y0);
                cr.line_to(x1, y1);
            }
            cr.stroke()?;
        }
    }
    Ok(())
}

fn draw_file(cr: &cairo::Context, label: &str, kind: Kind) -> Result<(), cairo::Error> {
    cr.set_line_width(1.);
    cr.new_path();
    cr.move_to(12., 4.5);
    cr.line_to(29., 4.5);
    cr.line_to(38.5, 14.);
    cr.line_to(38.5, 42.);
    cr.arc(36.5, 42., 2., 0., PI / 2.);
    cr.line_to(12., 44.);
    cr.arc(12., 42., 2., PI / 2., PI);
    cr.line_to(10., 6.5);
    cr.arc(12., 6.5, 2., PI, 3. * PI / 2.);
    cr.close_path();
    set(cr, FILE);
    cr.fill_preserve()?;
    set(cr, FILE_LINE);
    cr.stroke()?;
    cr.move_to(29., 4.5);
    cr.line_to(29., 12.);
    cr.arc_negative(31., 12., 2., PI, PI / 2.);
    cr.line_to(38.5, 14.);
    set(cr, FILE_FOLD);
    cr.fill_preserve()?;
    set(cr, FILE_LINE);
    cr.stroke()?;
    rounded(cr, 12.5, 27., 23., 10., 2.5);
    set(cr, kind.color());
    cr.fill()?;
    cr.select_font_face("monospace", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(6.6);
    let extents = cr.text_extents(label)?;
    cr.move_to(24. - extents.x_advance() / 2., 34.2);
    cr.set_source_rgb(1., 1., 1.);
    cr.show_text(label)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_follow_extensions() {
        assert_eq!(
            file_icon("Cargo.toml", true),
            Icon::File {
                label: "TOML".into(),
                kind: Kind::Code
            }
        );
        assert_eq!(
            file_icon("archive.tar.zst", false),
            Icon::File {
                label: "ZST".into(),
                kind: Kind::Archive
            }
        );
        assert_eq!(
            file_icon("Makefile", true),
            Icon::File {
                label: "TXT".into(),
                kind: Kind::Text
            }
        );
        assert_eq!(
            file_icon(".bashrc", true),
            Icon::File {
                label: "TXT".into(),
                kind: Kind::Text
            }
        );
        assert_eq!(
            file_icon("firmware", false),
            Icon::File {
                label: "BIN".into(),
                kind: Kind::Binary
            }
        );
        assert_eq!(
            file_icon("photo.JPEG", false),
            Icon::File {
                label: "JPEG".into(),
                kind: Kind::Image
            }
        );
        assert_eq!(
            file_icon("track.flac", false),
            Icon::File {
                label: "FLAC".into(),
                kind: Kind::Audio
            }
        );
        assert_eq!(
            file_icon("clip.mkv", false),
            Icon::File {
                label: "MKV".into(),
                kind: Kind::Video
            }
        );
        assert_eq!(
            file_icon("spec.pdf", false),
            Icon::File {
                label: "PDF".into(),
                kind: Kind::Pdf
            }
        );
    }

    #[test]
    fn well_known_folders_get_glyphs() {
        let home = Path::new("/home/u");
        assert_eq!(folder_glyph(home, home), Glyph::Home);
        assert_eq!(
            folder_glyph(&home.join("Downloads"), home),
            Glyph::Downloads
        );
        assert_eq!(folder_glyph(&home.join(".config"), home), Glyph::Config);
        assert_eq!(folder_glyph(Path::new("/etc"), home), Glyph::Config);
        assert_eq!(folder_glyph(&home.join("src"), home), Glyph::None);
    }
}
