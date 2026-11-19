//! The shared visual language of GAVNO's popovers and cards.
//!
//! The quick settings panel is the reference look: rounded cards on an 8 px
//! grid, one accent colour taken from the living theme, soft shadow and
//! medium-weight text. Everything that is drawn as a card (quick settings,
//! calendar, notifications, Classic Start) takes its numbers from here so the
//! surfaces cannot drift apart.
use crate::{
    topbar::{
        geometry::Rect,
        Canvas, LINE,
    },
    ui::text::{TextStyleSpec, Weight},
};
use smithay::backend::renderer::gles::GlesRenderer;
use std::time::Duration;

/// Corner radii, in logical px.
pub mod radius {
    /// Key caps, scrollbars, tiny chips.
    pub const CHIP: f32 = 6.0;
    /// Buttons inside a card, text fields.
    pub const FIELD: f32 = 10.0;
    /// List rows and day cells.
    pub const ROW: f32 = 12.0;
    /// Icon discs and sliders.
    pub const DISC: f32 = 14.0;
    /// Tiles and toast cards.
    pub const TILE: f32 = 16.0;
    /// Popover panels.
    pub const PANEL: f32 = 20.0;
}

/// Spacing on the 8 px grid (4 px for tight gaps).
pub mod space {
    pub const XS: i32 = 4;
    pub const SM: i32 = 8;
    pub const LG: i32 = 16;
    pub const XL: i32 = 24;
    pub const XXL: i32 = 32;
}

/// Type scale, logical px.
pub mod size {
    pub const TITLE: f32 = 18.0;
    pub const HEADING: f32 = 14.0;
    pub const BODY: f32 = 13.0;
    pub const LABEL: f32 = 12.5;
    pub const SMALL: f32 = 11.5;
    pub const CAPTION: f32 = 10.5;
}

pub const TEXT: [f32; 4] = [0.96, 0.95, 0.98, 1.0];
/// The card a popover is drawn on.
pub const PANEL_BG: [f32; 4] = [0.065, 0.075, 0.10, 0.98];
/// Resting tile or row.
pub const SURFACE: [f32; 4] = [1.0, 1.0, 1.0, 0.075];
/// Pointer over a row or button.
pub const SURFACE_HOVER: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
/// Button on a card.
pub const SURFACE_RAISED: [f32; 4] = [1.0, 1.0, 1.0, 0.10];
/// Sunken field, such as the search box.
pub const INSET: [f32; 4] = [0.025, 0.03, 0.045, 1.0];
/// Tracks of sliders and scrollbars.
pub const TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.12];
pub const DANGER: [f32; 4] = [1.0, 0.6, 0.5, 1.0];

/// A popover opens in this long, and a page slides in this long.
pub const OPEN_TIME: Duration = Duration::from_millis(120);
pub const SLIDE_TIME: Duration = Duration::from_millis(160);
/// Popover zoom at the start of the opening animation.
pub const OPEN_ZOOM: f64 = 0.96;

pub fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}
pub fn text(size: f32, weight: Weight, color: [f32; 4]) -> TextStyleSpec {
    TextStyleSpec { size, weight, color, tracking: 0.0 }
}
/// `color` with its alpha scaled by `alpha`.
pub fn dim(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], color[3] * alpha]
}
/// The accent as a translucent wash, for selected rows and active cells.
pub fn wash(accent: [f32; 4], alpha: f32) -> [f32; 4] {
    [accent[0], accent[1], accent[2], alpha]
}
/// The outline of a focused or selected element.
pub fn focus_line(accent: [f32; 4]) -> [f32; 4] {
    dim(accent, 0.95)
}
/// Text and icon colour that stays readable on the accent fill.
pub fn on_accent(accent: [f32; 4]) -> [f32; 4] {
    let luma = 0.2126 * accent[0] + 0.7152 * accent[1] + 0.0722 * accent[2];
    if luma > 0.45 {
        [0.06, 0.07, 0.10, 1.0]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    }
}

/// A popover card: shadow, then the panel with its hairline border.
pub fn card(c: &mut Canvas, area: Rect, radius: f32) {
    c.shadow(area, radius);
    c.rect(area, radius, PANEL_BG, 1.0, LINE);
}
/// A row-shaped button or list item; `hover` lights it up.
pub fn row_bg(c: &mut Canvas, area: Rect, hover: bool) {
    c.rect(area, radius::ROW, if hover { SURFACE_HOVER } else { SURFACE }, 0.0, LINE);
}
/// A section header such as "Notifications".
pub fn heading(c: &mut Canvas, r: &mut GlesRenderer, value: &str, x: i32, y: i32, width: i32) {
    c.styled_label(r, value, x, y, width, text(size::CAPTION, Weight::SemiBold, dim(TEXT, 0.55)));
}

/// Width of the widest digit, which every digit cell gets.
fn digit_cell(c: &mut Canvas, style: TextStyleSpec) -> i32 {
    ('0'..='9')
        .map(|d| c.text.measure(&d.to_string(), style).ceil() as i32)
        .max()
        .unwrap_or(0)
}
/// Width of `value` when drawn by `tabular_label`: digits share one cell width.
pub fn tabular_width(c: &mut Canvas, value: &str, style: TextStyleSpec) -> i32 {
    // `measure` follows the last rendered scale; prime it once.
    let _ = c.text.render(" ", style, c.scale);
    let cell = digit_cell(c, style);
    value
        .chars()
        .map(|ch| if ch.is_ascii_digit() { cell } else { c.text.measure(&ch.to_string(), style).ceil() as i32 })
        .sum()
}
/// Draws `value` with fixed-width digits (the font has no `tnum` feature, so
/// each digit is centred in a cell of the widest digit), so a changing clock
/// or percentage does not make its neighbours move.
pub fn tabular_label(c: &mut Canvas, r: &mut GlesRenderer, value: &str, x: i32, y: i32, style: TextStyleSpec) {
    let _ = c.text.render(" ", style, c.scale);
    let cell = digit_cell(c, style);
    let mut at = x;
    for ch in value.chars() {
        let s = ch.to_string();
        let w = c.text.measure(&s, style).ceil() as i32;
        if ch.is_ascii_digit() {
            c.styled_label(r, &s, at + (cell - w) / 2, y, cell + 2, style);
            at += cell;
        } else {
            if !ch.is_whitespace() {
                c.styled_label(r, &s, at, y, w + 2, style);
            }
            at += w;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_accent_picks_the_readable_side() {
        assert_eq!(on_accent([1.0, 1.0, 0.8, 1.0])[0], 0.06);
        assert_eq!(on_accent([0.1, 0.2, 0.6, 1.0]), [1.0; 4]);
    }
    #[test]
    fn dim_scales_only_alpha() {
        assert_eq!(dim([0.2, 0.4, 0.6, 0.5], 0.5), [0.2, 0.4, 0.6, 0.25]);
        assert_eq!(wash([0.2, 0.4, 0.6, 1.0], 0.2), [0.2, 0.4, 0.6, 0.2]);
    }
    #[test]
    fn spacing_stays_on_the_grid() {
        for s in [space::SM, space::LG, space::XL, space::XXL] {
            assert_eq!(s % 8, 0);
        }
        assert_eq!(space::XS * 2, space::SM);
    }
    #[test]
    fn digits_share_one_width() {
        let mut c = Canvas::default();
        let style = text(size::LABEL, Weight::SemiBold, TEXT);
        let a = tabular_width(&mut c, "11:11", style);
        let b = tabular_width(&mut c, "00:08", style);
        assert_eq!(a, b);
    }
}
