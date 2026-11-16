//! Transient media feedback. A one-shot deadline starts a finite closing spring.
use super::{
    geometry::rect,
    status::{self, Volume},
    Canvas, GLASS, LINE,
};
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    ui::spring::{Spring, SpringParams},
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    utils::{Logical, Size},
};
use std::time::Duration;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Volume,
    Microphone,
    Brightness,
}
pub struct Osd {
    kind: Kind,
    muted: bool,
    level: Spring,
    opacity: Spring,
    pub deadline: Option<Duration>,
    views: Vec<(Output, Canvas)>,
    icons: Icons,
}
impl Default for Osd {
    fn default() -> Self {
        Self {
            kind: Kind::Volume,
            muted: false,
            level: Spring::new(0.0, SpringParams::SNAPPY),
            opacity: Spring::new(
                0.0,
                SpringParams {
                    k: 900.0,
                    zeta: 0.9,
                },
            ),
            deadline: None,
            views: Vec::new(),
            icons: Default::default(),
        }
    }
}
impl Osd {
    pub fn show(&mut self, kind: Kind, value: f64, muted: bool, now: Duration) {
        self.level.advance(now);
        self.opacity.advance(now);
        self.kind = kind;
        self.muted = muted;
        self.level.set(value.clamp(0.0, 1.0));
        self.opacity.set(1.0);
        self.deadline = Some(now + Duration::from_millis(1500));
    }
    pub fn expire(&mut self, now: Duration) {
        if self.deadline.is_some_and(|t| now >= t) {
            self.deadline = None;
            self.opacity.advance(now);
            self.opacity.set(0.0);
        }
    }
    pub fn advance(&mut self, now: Duration) {
        self.expire(now);
        self.opacity.advance(now);
        self.level.advance(now);
    }
    pub fn is_animating(&self) -> bool {
        self.opacity.is_active() || (self.opacity.value > 0.001 && self.level.is_active())
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
        if self.opacity.value < 0.001 {
            return Vec::new();
        }
        let i = if let Some(i) = self.views.iter().position(|(o, _)| o == output) {
            i
        } else {
            self.views.push((output.clone(), Canvas::default()));
            self.views.len() - 1
        };
        let c = &mut self.views[i].1;
        c.begin(renderer, output.current_scale().fractional_scale());
        c.alpha = self.opacity.value.clamp(0.0, 1.0) as f32;
        let p = rect((size.w - 240) / 2, (size.h - 172).max(40), 240, 64);
        c.shadow(p, 22.0);
        c.rect(p, 22.0, GLASS, 1.0, LINE);
        let icon = match self.kind {
            Kind::Volume => status::volume_icon(Some(Volume {
                level: self.level.target,
                muted: self.muted,
            })),
            Kind::Microphone => {
                if self.muted {
                    "microphone-sensitivity-muted-symbolic"
                } else {
                    "audio-input-microphone-symbolic"
                }
            }
            Kind::Brightness => "display-brightness-symbolic",
        };
        if !c.icon(
            renderer,
            &mut self.icons,
            &[icon],
            theme,
            rect(p.loc.x + 20, p.loc.y + 24, 16, 16),
        ) {
            c.label(
                renderer,
                match self.kind {
                    Kind::Volume => "V",
                    Kind::Microphone => "M",
                    Kind::Brightness => "B",
                },
                p.loc.x + 20,
                p.loc.y + 22,
                20,
                13.0,
            );
        }
        c.rect(
            rect(p.loc.x + 56, p.loc.y + 28, 164, 8),
            4.0,
            [1.0, 1.0, 1.0, 0.15],
            0.0,
            LINE,
        );
        let mut color = accent;
        if self.muted {
            color[3] = 0.25;
        }
        c.rect(
            rect(
                p.loc.x + 56,
                p.loc.y + 28,
                (164.0 * self.level.value.clamp(0.0, 1.0)).round() as i32,
                8,
            ),
            4.0,
            color,
            0.0,
            LINE,
        );
        c.finish()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stops_after_two_seconds() {
        let mut o = Osd::default();
        o.show(Kind::Volume, 0.7, false, Duration::ZERO);
        for i in 0..=250 {
            o.advance(Duration::from_millis(i * 8));
        }
        assert!(o.deadline.is_none());
        assert!(!o.is_animating());
        assert_eq!(o.opacity.value, 0.0);
    }
    #[test]
    fn another_change_extends_deadline() {
        let mut o = Osd::default();
        o.show(Kind::Volume, 0.7, false, Duration::ZERO);
        o.show(Kind::Brightness, 0.4, false, Duration::from_secs(1));
        o.expire(Duration::from_millis(1500));
        assert_eq!(o.deadline, Some(Duration::from_millis(2500)));
    }
}
