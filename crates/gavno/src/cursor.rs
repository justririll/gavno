//! The compositor's own cursor image (used when no client sets one).

use std::time::Duration;

use xcursor::{
    parser::{parse_xcursor, Image},
    CursorTheme,
};

/// Theme images are resolved only once for each (shape name, integer scale).
pub struct Cursor {
    theme: CursorTheme,
    fallback_theme: CursorTheme,
    cache: std::collections::HashMap<(String, u32), Vec<Image>>,
    size: u32,
}

impl Cursor {
    /// Nominal cursor size in logical pixels.
    pub fn base_size(&self) -> u32 {
        self.size
    }
    /// True when `name` resolved to an animated cursor at any scale.
    pub fn animated(&self, name: &str) -> bool {
        self.cache
            .iter()
            .any(|((n, _), images)| n == name && images.len() > 1 && images.iter().any(|i| i.delay > 0))
    }
    pub fn load() -> Cursor {
        let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "Adwaita".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24u32)
            .clamp(1, 512);
        Cursor {
            theme: CursorTheme::load(&theme),
            fallback_theme: CursorTheme::load("default"),
            cache: Default::default(),
            size,
        }
    }

    pub fn image(&mut self, name: &str, scale: u32, time: Duration) -> Image {
        let scale = scale.clamp(1, 16);
        let key = (name.to_owned(), scale);
        let size = self.size * scale;
        let icons = self.cache.entry(key).or_insert_with(|| {
            let names = cursor_names(name);
            let icons = resolve_names(&names, |name| load_icons(&self.theme, name))
                .or_else(|| resolve_names(&names, |name| load_icons(&self.fallback_theme, name)))
                .unwrap_or_else(|| vec![fallback_arrow()]);
            // Keep only the nearest nominal size, including all its animation frames.
            let selected: Vec<_> = nearest_images(size, &icons).cloned().collect();
            selected
        });
        frame(time.as_millis() as u32, size, icons)
    }
}

/// CSS/Wayland names first, traditional XCursor aliases next, arrow last.
pub fn cursor_names(name: &str) -> Vec<&str> {
    let aliases: &[&str] = match name {
        "default" => &["left_ptr"],
        "pointer" => &["hand2", "hand1"],
        "text" => &["xterm"],
        "crosshair" => &["cross"],
        "wait" => &["watch"],
        "progress" => &["left_ptr_watch", "watch"],
        "move" | "all-scroll" | "all-resize" => &["fleur", "size_all"],
        "grab" => &["openhand", "hand1"],
        "grabbing" => &["closedhand", "hand2"],
        "not-allowed" | "no-drop" => &["crossed_circle", "forbidden"],
        "e-resize" => &["right_side", "size_hor"],
        "w-resize" => &["left_side", "size_hor"],
        "n-resize" => &["top_side", "size_ver"],
        "s-resize" => &["bottom_side", "size_ver"],
        "ne-resize" => &["top_right_corner", "size_bdiag"],
        "nw-resize" => &["top_left_corner", "size_fdiag"],
        "se-resize" => &["bottom_right_corner", "size_fdiag"],
        "sw-resize" => &["bottom_left_corner", "size_bdiag"],
        "ew-resize" | "col-resize" => &["sb_h_double_arrow", "size_hor"],
        "ns-resize" | "row-resize" => &["sb_v_double_arrow", "size_ver"],
        "nesw-resize" => &["size_bdiag", "fd_double_arrow"],
        "nwse-resize" => &["size_fdiag", "bd_double_arrow"],
        "help" => &["question_arrow", "whats_this"],
        "copy" => &["dnd-copy"],
        "alias" => &["dnd-link"],
        "dnd-ask" => &["dnd-copy"],
        _ => &[],
    };
    let mut names = vec![name];
    names.extend(aliases.iter().copied());
    for fallback in ["default", "left_ptr"] {
        if !names.contains(&fallback) {
            names.push(fallback);
        }
    }
    names
}
fn resolve_names<T>(names: &[&str], mut load: impl FnMut(&str) -> Option<T>) -> Option<T> {
    names.iter().find_map(|name| load(name))
}
fn load_icons(theme: &CursorTheme, name: &str) -> Option<Vec<Image>> {
    let data = std::fs::read(theme.load_icon(name)?).ok()?;
    parse_xcursor(&data).filter(|v| {
        !v.is_empty()
            && v.iter().all(|i| {
                i.width > 0
                    && i.height > 0
                    && i.width <= 4096
                    && i.height <= 4096
                    && i.pixels_rgba.len() == i.width as usize * i.height as usize * 4
            })
    })
}

fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    let nearest = images
        .iter()
        .min_by_key(|image| size.abs_diff(image.size))
        .unwrap();
    images
        .iter()
        .filter(move |image| image.width == nearest.width && image.height == nearest.height)
}

fn frame(mut millis: u32, size: u32, images: &[Image]) -> Image {
    let total = nearest_images(size, images).fold(0, |acc, image| acc + image.delay);
    if total == 0 {
        return nearest_images(size, images).next().unwrap().clone();
    }
    millis %= total;
    for img in nearest_images(size, images) {
        if millis < img.delay {
            return img.clone();
        }
        millis -= img.delay;
    }
    unreachable!()
}

/// A plain 24px arrow, white with a dark outline.
fn fallback_arrow() -> Image {
    const W: u32 = 24;
    let mut pixels = vec![0u8; (W * W * 4) as usize];
    for y in 0..W as i32 {
        for x in 0..W as i32 {
            // Arrow: a triangle with its tip at (1,1), 16px tall.
            let inside =
                |x: i32, y: i32| (1..=17).contains(&y) && x >= 1 && x <= 1 + (y - 1) * 2 / 3;
            let fill = inside(x, y);
            let edge = !fill
                && (inside(x - 1, y) || inside(x + 1, y) || inside(x, y - 1) || inside(x, y + 1));
            let px = if fill {
                [255, 255, 255, 255]
            } else if edge {
                [30, 30, 30, 255]
            } else {
                [0; 4]
            };
            let i = ((y as u32 * W + x as u32) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&px);
        }
    }
    Image {
        size: W,
        width: W,
        height: W,
        xhot: 1,
        yhot: 1,
        delay: 0,
        pixels_rgba: pixels,
        pixels_argb: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_names_and_resize_shapes() {
        for (shape, alias) in [
            ("text", "xterm"),
            ("pointer", "hand2"),
            ("ew-resize", "sb_h_double_arrow"),
            ("nwse-resize", "size_fdiag"),
            ("ne-resize", "top_right_corner"),
        ] {
            let names = cursor_names(shape);
            assert_eq!(names[0], shape);
            assert!(names.contains(&alias));
            assert_eq!(&names[names.len() - 2..], &["default", "left_ptr"]);
        }
    }
    #[test]
    fn unavailable_shape_falls_back_in_order() {
        let names = cursor_names("text");
        assert_eq!(
            resolve_names(&names, |n| (n == "xterm").then_some(7)),
            Some(7)
        );
        assert_eq!(
            resolve_names(&names, |n| (n == "left_ptr").then_some(9)),
            Some(9)
        );
        assert_eq!(resolve_names::<u8>(&names, |_| None), None);
        assert_eq!(
            cursor_names("future-shape"),
            vec!["future-shape", "default", "left_ptr"]
        );
    }
    #[test]
    fn animated_theme_frames_wrap() {
        let mut a = fallback_arrow();
        a.delay = 20;
        let mut b = a.clone();
        b.xhot = 2;
        b.delay = 30;
        let icons = vec![a, b];
        assert_eq!(frame(0, 24, &icons).xhot, 1);
        assert_eq!(frame(20, 24, &icons).xhot, 2);
        assert_eq!(frame(50, 24, &icons).xhot, 1);
    }
}
