//! The compositor's own cursor image (used when no client sets one).

use std::time::Duration;

use xcursor::{
    parser::{parse_xcursor, Image},
    CursorTheme,
};

pub struct Cursor {
    icons: Vec<Image>,
    size: u32,
}

impl Cursor {
    pub fn load() -> Cursor {
        let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "Adwaita".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        let icons = load_icons(&theme)
            .or_else(|| load_icons("default"))
            .unwrap_or_else(|| {
                tracing::warn!(theme, "no xcursor theme found, using the built-in arrow");
                vec![fallback_arrow()]
            });
        Cursor { icons, size }
    }

    /// The frame to show at `time` for an integer buffer `scale`.
    pub fn image(&self, scale: u32, time: Duration) -> Image {
        frame(time.as_millis() as u32, self.size * scale, &self.icons)
    }
}

fn load_icons(theme: &str) -> Option<Vec<Image>> {
    let path = CursorTheme::load(theme)
        .load_icon("default")
        .or_else(|| CursorTheme::load(theme).load_icon("left_ptr"))?;
    let data = std::fs::read(path).ok()?;
    parse_xcursor(&data).filter(|v| !v.is_empty())
}

fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    let nearest = images
        .iter()
        .min_by_key(|image| (size as i32 - image.size as i32).abs())
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
            let inside = |x: i32, y: i32| (1..=17).contains(&y) && x >= 1 && x <= 1 + (y - 1) * 2 / 3;
            let fill = inside(x, y);
            let edge = !fill
                && (inside(x - 1, y) || inside(x + 1, y) || inside(x, y - 1) || inside(x, y + 1));
            let px = if fill { [255, 255, 255, 255] } else if edge { [30, 30, 30, 255] } else { [0; 4] };
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
