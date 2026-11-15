//! On-demand window springs. Interactive operations explicitly cancel them.
use crate::{
    ui::spring::{Spring, SpringParams},
    Gavno,
};
use smithay::{
    desktop::{Space, Window},
    utils::{Logical, Rectangle},
};
use std::time::Duration;

pub type Rect = Rectangle<f64, Logical>;

/// Linear rectangle interpolation, including an anchored edge when both endpoints share it.
pub fn interpolate_rect(a: Rect, b: Rect, t: f64) -> Rect {
    Rectangle::new(
        (
            a.loc.x + (b.loc.x - a.loc.x) * t,
            a.loc.y + (b.loc.y - a.loc.y) * t,
        )
            .into(),
        (
            (a.size.w + (b.size.w - a.size.w) * t).max(1.0),
            (a.size.h + (b.size.h - a.size.h) * t).max(1.0),
        )
            .into(),
    )
}

#[derive(Debug)]
pub struct Motion {
    pub visibility: Spring,
    geometry: Option<(Rect, Rect, Spring)>,
    pub closing: bool,
}
impl Motion {
    pub fn open(now: Duration) -> Self {
        let mut visibility = Spring::new(0.0, SpringParams::SMOOTH);
        visibility.set(1.0);
        visibility.advance(now);
        Self {
            visibility,
            geometry: None,
            closing: false,
        }
    }
    pub fn close(&mut self, now: Duration) {
        self.advance(now);
        self.geometry = None;
        self.closing = true;
        self.visibility.set(0.0);
    }
    pub fn retarget(&mut self, from: Rect, to: Rect, now: Duration) {
        self.advance(now);
        if self
            .geometry
            .as_ref()
            .is_some_and(|(_, target, _)| *target == to)
        {
            return;
        }
        let from = self.rect(from);
        let mut progress = Spring::new(0.0, SpringParams::SNAPPY);
        progress.set(1.0);
        progress.advance(now);
        self.geometry = (from != to).then_some((from, to, progress));
    }
    pub fn seed_velocity(&mut self, velocity: smithay::utils::Point<f64, Logical>) {
        if let Some((from,to,progress))=&mut self.geometry {
            let dx=to.loc.x-from.loc.x;
            let dy=to.loc.y-from.loc.y;
            let length=dx*dx+dy*dy;
            if length>1.0 {progress.velocity=(velocity.x*dx+velocity.y*dy)/length;}
        }
    }
    pub fn cancel(&mut self) {
        self.geometry = None;
        self.visibility.jump(1.0);
    }
    pub fn advance(&mut self, now: Duration) {
        self.visibility.advance(now);
        if let Some((_, _, progress)) = &mut self.geometry {
            progress.advance(now);
        }
    }
    pub fn rect(&self, fallback: Rect) -> Rect {
        self.geometry
            .as_ref()
            .map(|(a, b, p)| interpolate_rect(*a, *b, p.value))
            .unwrap_or(fallback)
    }
    /// Keep scaling late client buffers until both the spring and configure have settled.
    pub fn retire_geometry(&mut self, committed: Rect) {
        if self
            .geometry
            .as_ref()
            .is_some_and(|(_, target, progress)| !progress.is_active() && *target == committed)
        {
            self.geometry = None;
        }
    }
    pub fn active(&self) -> bool {
        self.visibility.is_active()
            || self
                .geometry
                .as_ref()
                .is_some_and(|(_, _, p)| p.is_active())
    }
    pub fn finished(&self) -> bool {
        self.closing && !self.active()
    }
    pub fn scale(&self) -> f64 {
        0.94 + 0.06 * self.visibility.value.clamp(0.0, 1.0)
    }
}

pub struct WindowAnimation {
    pub window: Window,
    pub motion: Motion,
}
#[derive(Default)]
pub struct WindowAnimations {
    pub entries: Vec<WindowAnimation>,
}
impl WindowAnimations {
    pub fn open(&mut self, window: &Window, now: Duration) {
        self.entries.retain(|a| &a.window != window);
        self.entries.push(WindowAnimation {
            window: window.clone(),
            motion: Motion::open(now),
        });
    }
    pub fn close(&mut self, window: &Window, now: Duration) {
        if let Some(a) = self.entries.iter_mut().find(|a| &a.window == window) {
            if !a.motion.closing {
                a.motion.close(now);
            }
        }
    }
    pub fn cancel(&mut self, window: &Window) {
        if let Some(a) = self.entries.iter_mut().find(|a| &a.window == window) {
            a.motion.cancel();
        }
    }
    /// Hold an interactive geometry until the matching client buffer arrives.
    /// The override has no active spring, so a late client never keeps frames ticking.
    pub fn geometry_immediate(&mut self, window: &Window, target: Rect) {
        if self.motion(window).is_none() {
            self.entries.push(WindowAnimation { window: window.clone(), motion: Motion::open(Duration::ZERO) });
        }
        if let Some(a) = self.entries.iter_mut().find(|a| &a.window == window) {
            a.motion.cancel();
            a.motion.geometry = Some((target, target, Spring::new(1.0, SpringParams::SNAPPY)));
        }
    }
    pub fn advance(&mut self, now: Duration, space: &Space<Window>) {
        for a in &mut self.entries {
            a.motion.advance(now);
            if a.motion.geometry.is_none() {
                continue;
            }
            if let Some(loc) = space.element_location(&a.window) {
                // The client may choose a different buffer size from the configure.
                let immediate = a.motion.geometry.as_ref().is_some_and(|(from, to, _)| from == to);
                let size = if immediate {
                    // Interactive tiles keep their exact slot even if a client chooses another size.
                    a.window.geometry().size
                } else {
                    a.window.toplevel().and_then(|t| t.current_state().size).unwrap_or(a.window.geometry().size)
                };
                a.motion
                    .retire_geometry(Rectangle::new(loc, size).to_f64());
            }
        }
        self.entries.retain(|a| !a.motion.finished());
    }
    pub fn is_animating(&self) -> bool {
        self.entries.iter().any(|a| a.motion.active())
    }
    pub fn motion(&self, window: &Window) -> Option<&Motion> {
        self.entries
            .iter()
            .find(|a| &a.window == window)
            .map(|a| &a.motion)
    }
}

/// A null-buffer commit often precedes destruction. Start closing while the
/// last rendered elements are still cached; a later buffer can remap the surface.
pub fn surface_commit(gv: &mut Gavno, window: &Window, has_buffer: bool) {
    if crate::window_mode::mode_state(window).minimized { return; }
    let now = gv.start_time.elapsed();
    if !has_buffer {
        gv.window_animations.close(window, now);
    } else if gv
        .window_animations
        .motion(window)
        .is_none_or(|m| m.closing)
    {
        gv.window_animations.open(window, now);
    }
}

/// Call after mapping/configuring a compositor-directed geometry change. Pending
/// configure size is used until the client commits; live content scales to fit.
pub fn animate_geometry(gv: &mut Gavno, window: &Window, from_rect: Rectangle<i32, Logical>) {
    let Some(loc) = gv.space.element_location(window) else {
        return;
    };
    let size = window
        .toplevel()
        .and_then(|t| t.with_pending_state(|s| s.size))
        .unwrap_or(window.geometry().size);
    let target = Rectangle::new(loc, size).to_f64();
    let now = gv.start_time.elapsed();
    if gv.window_animations.motion(window).is_none() {
        gv.window_animations.open(window, now);
    }
    if let Some(a) = gv
        .window_animations
        .entries
        .iter_mut()
        .find(|a| &a.window == window)
    {
        a.motion.retarget(from_rect.to_f64(), target, now);
    }
    gv.queue_redraw_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(x: f64, w: f64) -> Rect {
        Rectangle::new((x, 20.0).into(), (w, 100.0).into())
    }
    #[test]
    fn immediate_geometry_scales_late_buffers_without_scheduling_frames() {
        let mut h = crate::state::tests::Harness::new();
        let w = h.window();
        h.gv.map_new_window(w.clone());
        let target = r(20.0, 500.0);
        h.gv.window_animations.geometry_immediate(&w, target);
        assert!(!h.gv.window_animations.is_animating());
        for now in [Duration::ZERO, Duration::from_secs(10)] {
            h.gv.window_animations.advance(now, &h.gv.space);
            assert_eq!(h.gv.window_animations.motion(&w).unwrap().rect(r(0.0,1.0)), target);
            assert!(!h.gv.window_animations.is_animating());
        }
    }
    #[test]
    fn open_close_and_idle() {
        let mut m = Motion::open(Duration::ZERO);
        assert!(m.active());
        assert_eq!(m.scale(), 0.94);
        for i in 1..300 {
            m.advance(Duration::from_millis(i * 8));
        }
        assert!(!m.active());
        assert_eq!(m.scale(), 1.0);
        m.close(Duration::from_millis(2400));
        assert!(m.active());
        for i in 301..600 {
            m.advance(Duration::from_millis(i * 8));
        }
        assert!(m.finished());
        assert_eq!(m.visibility.value, 0.0);
    }
    #[test]
    fn retarget_is_continuous_and_cancel_stops() {
        let mut m = Motion::open(Duration::ZERO);
        m.cancel();
        m.retarget(r(0.0, 100.0), r(100.0, 200.0), Duration::ZERO);
        m.advance(Duration::from_millis(100));
        let before = m.rect(r(0.0, 1.0));
        m.retarget(r(100.0, 200.0), r(200.0, 400.0), Duration::from_millis(100));
        assert_eq!(m.rect(r(0.0, 1.0)), before);
        for i in 1..300 {
            m.advance(Duration::from_millis(100 + i * 8));
        }
        assert_eq!(m.rect(r(0.0, 1.0)), r(200.0, 400.0));
        assert!(!m.active());
        m.cancel();
        assert_eq!(m.rect(r(4.0, 5.0)), r(4.0, 5.0));
    }
    #[test]
    fn close_during_open_reverses_without_jumping() {
        let mut m = Motion::open(Duration::ZERO);
        m.advance(Duration::from_millis(80));
        let before = m.visibility.value;
        m.close(Duration::from_millis(80));
        assert_eq!(m.visibility.value, before);
        assert!(m.active());
        for i in 1..400 {
            m.advance(Duration::from_millis(80 + i * 8));
        }
        assert!(m.finished());
    }
    #[test]
    fn geometry_settles_without_waiting_for_client() {
        let mut m = Motion::open(Duration::ZERO);
        m.cancel();
        m.retarget(r(10.0, 100.0), r(20.0, 200.0), Duration::ZERO);
        for i in 1..300 {
            m.advance(Duration::from_millis(i * 8));
        }
        assert!(!m.active());
        assert_eq!(m.rect(r(10.0, 100.0)), r(20.0, 200.0));
        m.retarget(r(10.0, 100.0), r(20.0, 200.0), Duration::from_millis(2400));
        assert!(!m.active());
    }
    #[test]
    fn settled_override_retires_after_target_commit() {
        let mut m = Motion::open(Duration::ZERO);
        m.cancel();
        let target = r(20.0, 200.0);
        m.retarget(r(10.0, 100.0), target, Duration::ZERO);
        m.retire_geometry(target);
        assert!(
            m.geometry.is_some(),
            "an early commit must not stop the spring"
        );
        for i in 1..300 {
            m.advance(Duration::from_millis(i * 8));
        }
        m.retire_geometry(r(10.0, 100.0));
        assert_eq!(m.rect(r(10.0, 100.0)), target, "wait for the target buffer");
        m.retire_geometry(target);
        assert!(m.geometry.is_none());
        assert_eq!(m.rect(r(30.0, 300.0)), r(30.0, 300.0));
        assert!(!m.active());
    }
    #[test]
    fn committed_override_retires_when_spring_settles() {
        let mut m = Motion::open(Duration::ZERO);
        m.cancel();
        let target = r(20.0, 200.0);
        m.retarget(r(10.0, 100.0), target, Duration::ZERO);
        for i in 1..300 {
            m.advance(Duration::from_millis(i * 8));
            m.retire_geometry(target);
        }
        assert!(m.geometry.is_none());
        assert_eq!(m.rect(r(30.0, 300.0)), r(30.0, 300.0));
    }
    #[test]
    fn overshoot_cannot_invert_small_client_rectangles() {
        let rect = interpolate_rect(r(0.0, 2000.0), r(0.0, 1.0), 1.01);
        assert_eq!(rect.size.w, 1.0);
    }
    #[test]
    fn interpolation_keeps_opposite_edge_anchored() {
        let a = r(0.0, 300.0);
        let b = r(100.0, 200.0);
        assert_eq!(interpolate_rect(a, b, 0.0), a);
        assert_eq!(interpolate_rect(a, b, 1.0), b);
        let mid = interpolate_rect(a, b, 0.5);
        assert_eq!(mid, r(50.0, 250.0));
        assert_eq!(mid.loc.x + mid.size.w, 300.0);
    }
}

impl Gavno {
    pub fn seed_geometry_velocity(&mut self, w: &Window, velocity: smithay::utils::Point<f64, Logical>) {
        if let Some(a)=self.window_animations.entries.iter_mut().find(|a| &a.window==w) {a.motion.seed_velocity(velocity);}
    }
}
