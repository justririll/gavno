//! Transient feedback for volume, microphone, brightness and keyboard layout:
//! one small capsule under the bar. A one-shot deadline (armed only while the
//! capsule is visible) starts the fade out; nothing runs while it is hidden.
use super::{
    geometry::{rect, Rect},
    status::{self, Volume},
    Canvas, LINE, PANEL,
};
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    ui::{
        spring::{Spring, SpringParams},
        style::{self as look, ease_out, size as type_size, text, TEXT},
        text::Weight,
    },
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    utils::{Logical, Size},
};
use std::time::Duration;

/// How long the capsule stays after the last change.
pub const HOLD: Duration = Duration::from_millis(1200);
/// Fade in and fade out.
pub const FADE_IN: Duration = Duration::from_millis(120);
pub const FADE_OUT: Duration = Duration::from_millis(160);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Volume,
    Microphone,
    Brightness,
    /// The active keyboard layout, drawn as its short name.
    Layout,
}

#[derive(Clone, Copy, Debug)]
struct Fade {
    from: f64,
    to: f64,
    start: Duration,
    length: Duration,
}

pub struct Osd {
    kind: Kind,
    muted: bool,
    label: String,
    level: Spring,
    opacity: f64,
    fade: Option<Fade>,
    pub deadline: Option<Duration>,
    views: Vec<(Output, Canvas)>,
    icons: Icons,
}
impl Default for Osd {
    fn default() -> Self {
        Self {
            kind: Kind::Volume,
            muted: false,
            label: String::new(),
            level: Spring::new(0.0, SpringParams::SNAPPY),
            opacity: 0.0,
            fade: None,
            deadline: None,
            views: Vec::new(),
            icons: Default::default(),
        }
    }
}

/// Where the parts of the capsule go, in logical pixels.
#[derive(Debug, PartialEq, Eq)]
pub struct Geometry {
    pub capsule: Rect,
    pub icon: Rect,
    /// The slider track; empty for the layout capsule.
    pub track: Rect,
    /// The percentage column, right aligned to its right edge.
    pub value: Rect,
}
const PAD: i32 = 14;
const HEIGHT: i32 = 40;
const TRACK_W: i32 = 112;
const GAP: i32 = 10;
/// Capsule under the bar, centred. `value_width` is the widest value text
/// ("100%" or "Muted"); `label_width` is the layout name for `Kind::Layout`.
pub fn geometry(size: Size<i32, Logical>, kind: Kind, value_width: i32, label_width: i32) -> Geometry {
    let slider = kind != Kind::Layout;
    let inner = if slider { 16 + GAP + TRACK_W + GAP + value_width } else { 16 + GAP + label_width };
    let width = (inner + 2 * PAD).min((size.w - 16).max(1));
    let capsule = rect((size.w - width) / 2, PANEL + look::space::SM, width, HEIGHT);
    let icon = rect(capsule.loc.x + PAD, capsule.loc.y + (HEIGHT - 16) / 2, 16, 16);
    let at = icon.loc.x + 16 + GAP;
    if slider {
        Geometry {
            capsule,
            icon,
            track: rect(at, capsule.loc.y + (HEIGHT - 6) / 2, TRACK_W, 6),
            value: rect(at + TRACK_W + GAP, capsule.loc.y, value_width, HEIGHT),
        }
    } else {
        Geometry { capsule, icon, track: rect(at, capsule.loc.y, 0, 0), value: rect(at, capsule.loc.y, label_width, HEIGHT) }
    }
}
/// The percentage text of `level`.
pub fn percent(level: f64) -> String {
    format!("{}%", (level.clamp(0.0, 1.0) * 100.0).round() as i32)
}

impl Osd {
    pub fn show(&mut self, kind: Kind, value: f64, muted: bool, now: Duration) {
        self.begin(kind, now);
        self.muted = muted;
        self.level.advance(now);
        self.level.set(value.clamp(0.0, 1.0));
    }
    /// Shows the new keyboard layout, e.g. "EN" or "RU".
    pub fn show_layout(&mut self, label: &str, now: Duration) {
        self.begin(Kind::Layout, now);
        self.muted = false;
        self.label.clear();
        self.label.push_str(label);
    }
    fn begin(&mut self, kind: Kind, now: Duration) {
        self.advance(now);
        // Another kind replaces the slider position instead of sliding from it.
        if self.kind != kind || self.opacity < 0.001 {
            self.level.jump(self.level.target);
        }
        self.kind = kind;
        if self.opacity < 1.0 && self.fade.is_none_or(|f| f.to < 1.0) {
            self.fade = Some(Fade { from: self.opacity, to: 1.0, start: now, length: FADE_IN });
        }
        self.deadline = Some(now + HOLD);
    }
    pub fn expire(&mut self, now: Duration) {
        if self.deadline.is_some_and(|t| now >= t) {
            self.deadline = None;
            self.progress(now);
            self.fade = Some(Fade { from: self.opacity, to: 0.0, start: now, length: FADE_OUT });
        }
    }
    fn progress(&mut self, now: Duration) {
        let Some(f) = self.fade else { return };
        let t = now.saturating_sub(f.start).as_secs_f64() / f.length.as_secs_f64();
        if t >= 1.0 {
            self.opacity = f.to;
            self.fade = None;
        } else {
            self.opacity = f.from + (f.to - f.from) * ease_out(t);
        }
    }
    pub fn advance(&mut self, now: Duration) {
        self.expire(now);
        self.progress(now);
        self.level.advance(now);
    }
    pub fn is_animating(&self) -> bool {
        self.fade.is_some() || (self.opacity > 0.001 && self.level.is_active())
    }
    pub fn visible(&self) -> bool {
        self.opacity > 0.001
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    fn icon_name(&self) -> &'static str {
        match self.kind {
            Kind::Volume => status::volume_icon(Some(Volume { level: self.level.target, muted: self.muted })),
            Kind::Microphone => {
                if self.muted {
                    "microphone-sensitivity-muted-symbolic"
                } else {
                    "audio-input-microphone-symbolic"
                }
            }
            Kind::Brightness => "display-brightness-symbolic",
            Kind::Layout => "input-keyboard-symbolic",
        }
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        now: Duration,
        theme: &str,
        accent: [f32; 4],
    ) -> Vec<GavnoRenderElement> {
        self.advance(now);
        if self.opacity < 0.001 {
            return Vec::new();
        }
        let i = if let Some(i) = self.views.iter().position(|(o, _)| o == output) {
            i
        } else {
            self.views.push((output.clone(), Canvas::default()));
            self.views.len() - 1
        };
        let icon_name = self.icon_name();
        let (kind, muted, level, alpha) = (self.kind, self.muted, self.level.value.clamp(0.0, 1.0), self.opacity);
        let label = self.label.clone();
        let c = &mut self.views[i].1;
        c.begin(renderer, output.current_scale().fractional_scale());
        c.alpha = alpha.clamp(0.0, 1.0) as f32;
        let value_style = text(type_size::LABEL, Weight::SemiBold, if muted { look::dim(TEXT, 0.55) } else { TEXT });
        let label_style = text(type_size::HEADING, Weight::SemiBold, TEXT);
        // `measure` follows the last rendered scale; prime it once per frame.
        let _ = c.text.render(" ", label_style, c.scale);
        let value_width = look::tabular_width(c, "100%", value_style).max(c.text.measure("Muted", value_style).ceil() as i32);
        let label_width = c.text.measure(&label, label_style).ceil() as i32;
        let g = geometry(size, kind, value_width, label_width);
        look::card(c, g.capsule, HEIGHT as f32 / 2.0);
        if !c.icon(renderer, &mut self.icons, &[icon_name], theme, g.icon) {
            let glyph = match kind {
                Kind::Volume => "V",
                Kind::Microphone => "M",
                Kind::Brightness => "B",
                Kind::Layout => "K",
            };
            c.label(renderer, glyph, g.icon.loc.x, g.icon.loc.y - 1, 16, 13.0);
        }
        if kind == Kind::Layout {
            c.styled_label(renderer, &label, g.value.loc.x, g.capsule.loc.y + (HEIGHT - 20) / 2, label_width + 2, label_style);
        } else {
            c.rect(g.track, 3.0, look::TRACK, 0.0, LINE);
            let fill = (g.track.size.w as f64 * level).round() as i32;
            let mut color = accent;
            if muted {
                color[3] = 0.30;
            }
            if fill > 0 {
                c.rect(rect(g.track.loc.x, g.track.loc.y, fill.max(6), g.track.size.h), 3.0, color, 0.0, LINE);
            }
            // The knob rides the end of the fill, kept inside the track.
            let knob_x = (g.track.loc.x + fill - 6).clamp(g.track.loc.x - 2, g.track.loc.x + g.track.size.w - 10);
            c.rect(rect(knob_x, g.track.loc.y - 3, 12, 12), 6.0, if muted { look::dim(TEXT, 0.6) } else { TEXT }, 0.0, LINE);
            let y = g.capsule.loc.y + (HEIGHT - 18) / 2;
            if muted {
                let w = c.text.measure("Muted", value_style).ceil() as i32;
                c.styled_label(renderer, "Muted", g.value.loc.x + g.value.size.w - w, y, w + 2, value_style);
            } else {
                let text_value = percent(level);
                let w = look::tabular_width(c, &text_value, value_style);
                look::tabular_label(c, renderer, &text_value, g.value.loc.x + g.value.size.w - w, y, value_style);
            }
        }
        c.finish()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn run(o: &mut Osd, from_ms: u64, to_ms: u64) {
        for ms in (from_ms..=to_ms).step_by(8) {
            o.advance(Duration::from_millis(ms));
        }
    }
    #[test]
    fn fades_in_over_120ms_holds_then_fades_out_and_stops() {
        let mut o = Osd::default();
        assert!(!o.is_animating() && !o.visible());
        o.show(Kind::Volume, 0.7, false, Duration::ZERO);
        assert!(o.is_animating());
        o.advance(Duration::from_millis(60));
        assert!(o.opacity > 0.0 && o.opacity < 1.0);
        o.advance(Duration::from_millis(120));
        assert_eq!(o.opacity, 1.0);
        run(&mut o, 120, 1199);
        assert_eq!(o.opacity, 1.0);
        assert_eq!(o.deadline, Some(HOLD));
        o.advance(HOLD);
        assert!(o.deadline.is_none());
        assert!(o.is_animating(), "fading out");
        run(&mut o, 1200, 1500);
        assert!(!o.is_animating());
        assert_eq!(o.opacity, 0.0);
        assert!(!o.visible());
    }
    #[test]
    fn settled_capsule_needs_no_frames_between_show_and_deadline() {
        let mut o = Osd::default();
        o.show(Kind::Brightness, 0.4, false, Duration::ZERO);
        run(&mut o, 0, 600);
        assert!(!o.is_animating(), "fully visible and settled: no redraw until the one-shot timer");
    }
    #[test]
    fn another_change_extends_deadline_and_keeps_it_visible() {
        let mut o = Osd::default();
        o.show(Kind::Volume, 0.7, false, Duration::ZERO);
        o.show(Kind::Brightness, 0.4, false, Duration::from_millis(1000));
        o.expire(Duration::from_millis(1200));
        assert_eq!(o.deadline, Some(Duration::from_millis(2200)));
        assert_eq!(o.opacity, 1.0);
        assert_eq!(o.kind(), Kind::Brightness);
    }
    #[test]
    fn change_during_fade_out_fades_back_in() {
        let mut o = Osd::default();
        o.show(Kind::Volume, 0.7, false, Duration::ZERO);
        run(&mut o, 0, 1300);
        let mid = o.opacity;
        assert!(mid > 0.0 && mid < 1.0);
        o.show(Kind::Volume, 0.8, false, Duration::from_millis(1304));
        run(&mut o, 1304, 1500);
        assert_eq!(o.opacity, 1.0);
        assert!(o.deadline.is_some());
    }
    #[test]
    fn layout_osd_carries_its_label() {
        let mut o = Osd::default();
        o.show_layout("RU", Duration::ZERO);
        assert_eq!(o.kind(), Kind::Layout);
        assert_eq!(o.label, "RU");
        assert_eq!(o.icon_name(), "input-keyboard-symbolic");
    }
    #[test]
    fn percent_rounds_and_clamps() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.704), "70%");
        assert_eq!(percent(1.7), "100%");
        assert_eq!(percent(-1.0), "0%");
    }
    #[test]
    fn capsule_is_centred_under_the_bar_and_parts_fit_inside() {
        for (w, h) in [(1280, 800), (2134, 1200), (320, 600)] {
            for kind in [Kind::Volume, Kind::Microphone, Kind::Brightness, Kind::Layout] {
                let g = geometry((w, h).into(), kind, 40, 24);
                assert!(g.capsule.loc.y >= PANEL, "{w}: below the bar");
                assert!(rect(0, 0, w, h).contains_rect(g.capsule));
                if w >= 400 {
                    assert!((g.capsule.loc.x * 2 + g.capsule.size.w - w).abs() <= 1, "centred");
                    assert!(g.capsule.contains_rect(g.icon));
                    assert!(g.capsule.contains_rect(g.value));
                    if kind != Kind::Layout {
                        assert!(g.capsule.contains_rect(g.track));
                        assert!(!g.track.overlaps(g.value) && !g.track.overlaps(g.icon));
                    }
                }
            }
        }
    }
}
