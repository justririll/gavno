//! Folder and file icons drawn with cairo, following design/files-mockup.html.
//! Geometry is authored on a 48x48 grid and scaled to the widget size.
use gtk::{cairo, prelude::*};
use std::{f64::consts::PI, path::Path};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Code,
    Text,
    Image,
    Archive,
    Disc,
    Web,
    Binary,
}
impl Kind {
    fn color(self) -> Rgb {
        match self {
            Kind::Code => rgb(0x7b5ce0),
            Kind::Text | Kind::Binary => rgb(0x5d6680),
            Kind::Image => rgb(0xd4517a),
            Kind::Archive => rgb(0xc98a1c),
            Kind::Disc => rgb(0x1f9a8a),
            Kind::Web => rgb(0x3d7be0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
/// type is textual, which decides between TXT and BIN when there is no
/// extension.
pub fn file_icon(name: &str, text: bool) -> Icon {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|_| !name.starts_with('.') || name[1..].contains('.'))
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let kind = match ext.as_str() {
        "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "py" | "js" | "ts" | "go" | "java" | "kt"
        | "sh" | "fish" | "toml" | "kdl" | "conf" | "json" | "yaml" | "yml" | "ini" | "frag"
        | "vert" | "glsl" | "wgsl" | "lua" | "nix" | "zig" | "rb" | "pl" | "mk" | "cmake" => {
            Kind::Code
        }
        "md" | "txt" | "log" | "gen" | "pdf" | "rst" | "org" | "csv" | "tex" => Kind::Text,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif" | "heic" | "ico"
        | "tiff" => Kind::Image,
        "zip" | "xz" | "zst" | "gz" | "tar" | "7z" | "rar" | "bz2" | "tgz" | "lz4" | "deb"
        | "rpm" => Kind::Archive,
        "img" | "iso" | "qcow2" | "raw" => Kind::Disc,
        "html" | "htm" | "css" | "xml" => Kind::Web,
        "" if text => Kind::Text,
        "" => Kind::Binary,
        _ if text => Kind::Text,
        _ => Kind::Binary,
    };
    let label = if ext.is_empty() {
        if kind == Kind::Binary { "BIN" } else { "TXT" }.to_owned()
    } else {
        ext.chars().take(4).collect::<String>().to_uppercase()
    };
    Icon::File { label, kind }
}

impl Icon {
    /// A compact text form stored as the drawing area's widget name, so list
    /// item binds can update the icon without extra per-item state.
    fn encode(&self) -> String {
        match self {
            Icon::Folder(glyph) => format!("d{}", *glyph as u8),
            Icon::File { label, kind } => format!("f{}{label}", *kind as u8),
        }
    }
    fn decode(text: &str) -> Self {
        const GLYPHS: [Glyph; 8] = [
            Glyph::None,
            Glyph::Home,
            Glyph::Documents,
            Glyph::Downloads,
            Glyph::Pictures,
            Glyph::Music,
            Glyph::Code,
            Glyph::Config,
        ];
        const KINDS: [Kind; 7] = [
            Kind::Code,
            Kind::Text,
            Kind::Image,
            Kind::Archive,
            Kind::Disc,
            Kind::Web,
            Kind::Binary,
        ];
        let mut chars = text.chars();
        let tag = chars.next();
        let index = chars.next().and_then(|c| c.to_digit(10)).unwrap_or(0) as usize;
        match tag {
            Some('f') => Icon::File {
                label: chars.collect(),
                kind: KINDS.get(index).copied().unwrap_or(Kind::Binary),
            },
            _ => Icon::Folder(GLYPHS.get(index).copied().unwrap_or(Glyph::None)),
        }
    }
}

/// A square drawing area that paints an [`Icon`]; see [`set_icon`].
pub fn icon_area(size: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(size);
    area.set_content_height(size);
    area.set_can_target(false);
    area.add_css_class("file-icon");
    area.set_draw_func(|area, cr, w, h| {
        let side = f64::from(w.min(h));
        cr.translate((f64::from(w) - side) / 2., (f64::from(h) - side) / 2.);
        cr.scale(side / 48., side / 48.);
        let _ = draw(cr, &Icon::decode(area.widget_name().as_str()));
    });
    area
}

pub fn set_icon(area: &gtk::DrawingArea, icon: &Icon) {
    let name = icon.encode();
    if area.widget_name() != name.as_str() {
        area.set_widget_name(&name);
        area.queue_draw();
    }
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
    // Back sheet with the tab.
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
            Icon::File { label: "TOML".into(), kind: Kind::Code }
        );
        assert_eq!(
            file_icon("archive.tar.zst", false),
            Icon::File { label: "ZST".into(), kind: Kind::Archive }
        );
        assert_eq!(
            file_icon("Makefile", true),
            Icon::File { label: "TXT".into(), kind: Kind::Text }
        );
        assert_eq!(
            file_icon(".bashrc", true),
            Icon::File { label: "TXT".into(), kind: Kind::Text }
        );
        assert_eq!(
            file_icon("firmware", false),
            Icon::File { label: "BIN".into(), kind: Kind::Binary }
        );
        assert_eq!(
            file_icon("photo.JPEG", false),
            Icon::File { label: "JPEG".into(), kind: Kind::Image }
        );
    }
    #[test]
    fn icons_round_trip_through_widget_names() {
        for icon in [
            Icon::Folder(Glyph::Config),
            Icon::Folder(Glyph::None),
            file_icon("a.webp", false),
            file_icon("blob", false),
        ] {
            assert_eq!(Icon::decode(&icon.encode()), icon);
        }
    }
    #[test]
    fn well_known_folders_get_glyphs() {
        let home = Path::new("/home/u");
        assert_eq!(folder_glyph(home, home), Glyph::Home);
        assert_eq!(folder_glyph(&home.join("Downloads"), home), Glyph::Downloads);
        assert_eq!(folder_glyph(&home.join(".config"), home), Glyph::Config);
        assert_eq!(folder_glyph(Path::new("/etc"), home), Glyph::Config);
        assert_eq!(folder_glyph(&home.join("src"), home), Glyph::None);
    }
}
