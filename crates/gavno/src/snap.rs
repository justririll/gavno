//! Snap geometry and keyboard navigation. The math is independent of clients.
use crate::{
    window_mode::{mode_state, WindowMode},
    Gavno,
};
use smithay::{
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle, Size},
    wayland::{compositor::with_states, shell::xdg::SurfaceCachedState},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Maximized,
}

impl Zone {
    /// The mockup's work area includes an outer gap as well as internal gaps.
    /// Integer remainders belong to the right/bottom cell, so no pixels vanish.
    pub fn rect(self, work: Rectangle<i32, Logical>, gap: i32) -> Rectangle<i32, Logical> {
        let gx = gap.max(0).min((work.size.w.max(1) - 1) / 3);
        let gy = gap.max(0).min((work.size.h.max(1) - 1) / 3);
        let area = Rectangle::new(
            (work.loc.x + gx, work.loc.y + gy).into(),
            ((work.size.w - 2 * gx).max(1), (work.size.h - 2 * gy).max(1)).into(),
        );
        let iw = gx.min((area.size.w - 1).max(0));
        let ih = gy.min((area.size.h - 1).max(0));
        let lw = ((area.size.w - iw) / 2).max(1);
        let th = ((area.size.h - ih) / 2).max(1);
        let rw = (area.size.w - iw - lw).max(1);
        let bh = (area.size.h - ih - th).max(1);
        let right = area.loc.x + area.size.w - rw;
        let bottom = area.loc.y + area.size.h - bh;
        match self {
            Self::Maximized => area,
            Self::Left => Rectangle::new(area.loc, (lw, area.size.h).into()),
            Self::Right => Rectangle::new((right, area.loc.y).into(), (rw, area.size.h).into()),
            Self::TopLeft => Rectangle::new(area.loc, (lw, th).into()),
            Self::TopRight => Rectangle::new((right, area.loc.y).into(), (rw, th).into()),
            Self::BottomLeft => Rectangle::new((area.loc.x, bottom).into(), (lw, bh).into()),
            Self::BottomRight => Rectangle::new((right, bottom).into(), (rw, bh).into()),
        }
    }
    pub fn mode(self) -> WindowMode {
        if self == Self::Maximized {
            WindowMode::Maximized
        } else {
            WindowMode::Snapped(self)
        }
    }
}

/// Detection uses output edges, not panel/exclusive-zone edges.
pub fn zone_at(p: Point<f64, Logical>, output: Rectangle<i32, Logical>) -> Option<Zone> {
    if !output.to_f64().contains(p) {
        return None;
    }
    let x = p.x - output.loc.x as f64;
    let y = p.y - output.loc.y as f64;
    let r = output.size.w as f64 - x;
    let b = output.size.h as f64 - y;
    if x <= 32.0 && y <= 32.0 {
        Some(Zone::TopLeft)
    } else if r <= 32.0 && y <= 32.0 {
        Some(Zone::TopRight)
    } else if x <= 32.0 && b <= 32.0 {
        Some(Zone::BottomLeft)
    } else if r <= 32.0 && b <= 32.0 {
        Some(Zone::BottomRight)
    } else if y <= 8.0 {
        Some(Zone::Maximized)
    } else if x <= 8.0 {
        Some(Zone::Left)
    } else if r <= 8.0 {
        Some(Zone::Right)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub fn neighbour(
    origin: Rectangle<i32, Logical>,
    candidates: &[Rectangle<i32, Logical>],
    dir: Direction,
) -> Option<usize> {
    let center = |r: Rectangle<i32, Logical>| {
        (
            r.loc.x as f64 + r.size.w as f64 / 2.0,
            r.loc.y as f64 + r.size.h as f64 / 2.0,
        )
    };
    let (x, y) = center(origin);
    candidates
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let (cx, cy) = center(*r);
            let (primary, cross) = match dir {
                Direction::Left => (x - cx, (cy - y).abs()),
                Direction::Right => (cx - x, (cy - y).abs()),
                Direction::Up => (y - cy, (cx - x).abs()),
                Direction::Down => (cy - y, (cx - x).abs()),
            };
            (primary > 1.0).then_some((i, primary + 2.0 * cross))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Restore around the original horizontal grab fraction, as Windows does.
pub fn restored_drag_origin(
    start: Point<f64, Logical>,
    current: Rectangle<i32, Logical>,
    restored: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let offset = start - current.loc.to_f64();
    let fraction = (offset.x / current.size.w.max(1) as f64).clamp(0.0, 1.0);
    (start
        - Point::from((
            fraction * restored.w.max(1) as f64,
            offset.y.clamp(0.0, restored.h.max(1) as f64),
        )))
    .to_i32_round()
}

/// Stable, unique MRU order; also used with simple ids in tests.
pub fn touch_mru<T: PartialEq + Clone>(list: &mut Vec<T>, item: &T) {
    list.retain(|w| w != item);
    list.insert(0, item.clone());
}

pub fn resize_step(
    mut rect: Rectangle<i32, Logical>,
    dir: Direction,
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let bound = |v: i32, lo: i32, hi: i32| {
        v.clamp(lo.max(1), if hi > 0 { hi.max(lo.max(1)) } else { i32::MAX })
    };
    match dir {
        Direction::Left => rect.size.w = bound(rect.size.w.saturating_sub(40), min.w, max.w),
        Direction::Right => rect.size.w = bound(rect.size.w.saturating_add(40), min.w, max.w),
        Direction::Up => rect.size.h = bound(rect.size.h.saturating_sub(40), min.h, max.h),
        Direction::Down => rect.size.h = bound(rect.size.h.saturating_add(40), min.h, max.h),
    }
    rect
}

pub fn keyboard_mode(mode: WindowMode, dir: Direction) -> WindowMode {
    use Direction::*;
    use Zone::{BottomLeft, BottomRight, TopLeft, TopRight};
    match (dir, mode) {
        (Left, WindowMode::Snapped(Zone::Right)) | (Right, WindowMode::Snapped(Zone::Left)) => {
            WindowMode::Floating
        }
        (Left, WindowMode::Snapped(TopRight | TopLeft)) => WindowMode::Snapped(TopLeft),
        (Left, WindowMode::Snapped(BottomRight | BottomLeft)) => WindowMode::Snapped(BottomLeft),
        (Right, WindowMode::Snapped(TopLeft | TopRight)) => WindowMode::Snapped(TopRight),
        (Right, WindowMode::Snapped(BottomLeft | BottomRight)) => WindowMode::Snapped(BottomRight),
        (Left, _) => WindowMode::Snapped(Zone::Left),
        (Right, _) => WindowMode::Snapped(Zone::Right),
        (Up, WindowMode::Snapped(Zone::Left | BottomLeft)) => WindowMode::Snapped(TopLeft),
        (Up, WindowMode::Snapped(Zone::Right | BottomRight)) => WindowMode::Snapped(TopRight),
        (Up, _) => WindowMode::Maximized,
        (Down, WindowMode::Snapped(Zone::Left)) => WindowMode::Snapped(BottomLeft),
        (Down, WindowMode::Snapped(Zone::Right)) => WindowMode::Snapped(BottomRight),
        (Down, WindowMode::Snapped(TopLeft)) => WindowMode::Snapped(Zone::Left),
        (Down, WindowMode::Snapped(TopRight)) => WindowMode::Snapped(Zone::Right),
        (Down, WindowMode::Snapped(BottomLeft | BottomRight)) => mode,
        (Down, _) => WindowMode::Floating,
    }
}

impl Gavno {
    pub fn window_rect(&self, w: &Window) -> Option<Rectangle<i32, Logical>> {
        self.space
            .element_location(w)
            .map(|loc| Rectangle::new(loc, w.geometry().size))
    }
    pub fn navigation_neighbour(&self, w: &Window, dir: Direction) -> Option<Window> {
        let origin = self.window_rect(w)?;
        let pool: Vec<_> = self
            .space
            .elements()
            .filter(|n| *n != w)
            .filter_map(|n| self.window_rect(n).map(|r| (n.clone(), r)))
            .collect();
        neighbour(
            origin,
            &pool.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
            dir,
        )
        .map(|i| pool[i].0.clone())
    }
    pub fn focus_direction(&mut self, dir: Direction) {
        // Every window of a console fills it: focus walks them in order.
        if self.tty.active() {
            self.tty_cycle_focus(matches!(dir, Direction::Right | Direction::Down));
            return;
        }
        let Some(w) = self.focused_window() else {
            self.focus_topmost();
            return;
        };
        if let Some(n) = self.navigation_neighbour(&w, dir) {
            self.focus_window(Some(&n));
        }
    }
    pub fn snap_direction(&mut self, dir: Direction) {
        self.guard(crate::module::Feature::Snap, Default::default(), |gv| gv.snap_direction_feature(dir))
    }

    fn snap_direction_feature(&mut self, dir: Direction) {
        if self.focused_window().is_some_and(|w| self.tiling_workspace(&w)) { self.move_tile(dir); return; }
        let Some(w) = self.focused_window() else {
            return;
        };
        let mode = mode_state(&w).mode;
        let repeat = matches!(
            (dir, mode),
            (Direction::Left, WindowMode::Snapped(Zone::Left))
                | (Direction::Right, WindowMode::Snapped(Zone::Right))
        );
        if repeat {
            let Some(current) = self.output_for_window(&w) else {
                return;
            };
            let mut outputs: Vec<Output> = self.space.outputs().cloned().collect();
            outputs.sort_by_key(|o| self.space.output_geometry(o).map(|r| (r.loc.x, r.loc.y)));
            if outputs.len() < 2 {
                return;
            }
            let Some(i) = outputs.iter().position(|o| *o == current) else {
                return;
            };
            let next = if dir == Direction::Left {
                (i + outputs.len() - 1) % outputs.len()
            } else {
                (i + 1) % outputs.len()
            };
            self.set_window_mode(&w, mode, Some(outputs[next].clone()));
        } else {
            let next = keyboard_mode(mode, dir);
            if next != mode {
                self.set_window_mode(&w, next, None);
            }
        }
    }
    pub fn resize_direction(&mut self, dir: Direction) {
        self.guard(crate::module::Feature::Snap, Default::default(), |gv| gv.resize_direction_feature(dir))
    }

    fn resize_direction_feature(&mut self, dir: Direction) {
        if let Some(w) = self.focused_window().filter(|w| self.tiled(w)) {
            let delta = if matches!(dir, Direction::Left | Direction::Up) { -40 } else { 40 };
            self.resize_tile(&w,dir,delta,true); return;
        }
        let Some(w) = self.focused_window() else {
            return;
        };
        if mode_state(&w).mode != WindowMode::Floating {
            return;
        }
        let (Some(r), Some(t)) = (self.window_rect(&w), w.toplevel()) else {
            return;
        };
        let (min, max) = with_states(t.wl_surface(), |s| {
            let mut cached = s.cached_state.get::<SurfaceCachedState>();
            let c = cached.current();
            (c.min_size, c.max_size)
        });
        // Pending size keeps rapid key repeats from losing steps before commit.
        let size = t.with_pending_state(|s| s.size).unwrap_or(r.size);
        let target = resize_step(Rectangle::new(r.loc, size), dir, min, max);
        crate::grabs::resize_grab::clear_anchor(&w);
        self.features.call(crate::module::Feature::Animations, (), || self.window_animations.cancel(&w));
        t.with_pending_state(|s| s.size = Some(target.size));
        t.send_pending_configure();
        self.queue_redraw_all();
    }
    pub fn swap_direction(&mut self, dir: Direction) {
        self.guard(crate::module::Feature::Tiling, Default::default(), |gv| gv.swap_direction_feature(dir))
    }

    fn swap_direction_feature(&mut self, dir: Direction) {
        if self.focused_window().is_some_and(|w| self.tiling_workspace(&w)) { self.move_tile(dir); return; }
        let Some(w) = self.focused_window() else {
            return;
        };
        let Some(n) = self.navigation_neighbour(&w, dir) else {
            return;
        };
        let (Some(a), Some(b)) = (self.window_rect(&w), self.window_rect(&n)) else {
            return;
        };
        // Modes and their floating restore records travel with the slots.
        let sa = mode_state(&w);
        let sb = mode_state(&n);
        crate::window_mode::with_mode_state(&w, |s| *s = sb);
        crate::window_mode::with_mode_state(&n, |s| *s = sa);
        self.configure_slot(&w, b, sb.mode);
        self.configure_slot(&n, a, sa.mode);
        self.queue_redraw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }
    #[test]
    fn zone_rectangles_include_outer_and_internal_gaps() {
        let work = r(100, 50, 1001, 801);
        assert_eq!(Zone::Maximized.rect(work, 12), r(112, 62, 977, 777));
        assert_eq!(Zone::Left.rect(work, 12), r(112, 62, 482, 777));
        assert_eq!(Zone::Right.rect(work, 12), r(606, 62, 483, 777));
        assert_eq!(Zone::TopLeft.rect(work, 12), r(112, 62, 482, 382));
        assert_eq!(Zone::TopRight.rect(work, 12), r(606, 62, 483, 382));
        assert_eq!(Zone::BottomLeft.rect(work, 12), r(112, 456, 482, 383));
        assert_eq!(Zone::BottomRight.rect(work, 12), r(606, 456, 483, 383));
    }
    #[test]
    fn gaps_are_safe_for_tiny_outputs_and_negative_config() {
        for w in 1..60 {
            for h in 1..60 {
                for gap in [-10, 0, 12, 1000] {
                    let work = r(-30, -20, w, h);
                    for z in [
                        Zone::Left,
                        Zone::Right,
                        Zone::TopLeft,
                        Zone::TopRight,
                        Zone::BottomLeft,
                        Zone::BottomRight,
                        Zone::Maximized,
                    ] {
                        let a = z.rect(work, gap);
                        assert!(a.size.w > 0 && a.size.h > 0);
                        assert!(a.loc.x >= work.loc.x && a.loc.y >= work.loc.y);
                        assert!(
                            a.loc.x + a.size.w <= work.loc.x + w
                                && a.loc.y + a.size.h <= work.loc.y + h
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn detection_all_zones_and_thresholds_on_offset_output() {
        let output = r(-1000, 50, 1000, 800);
        for (x, y, zone) in [
            (0.0, 300.0, Some(Zone::Left)),
            (8.0, 300.0, Some(Zone::Left)),
            (8.1, 300.0, None),
            (999.0, 300.0, Some(Zone::Right)),
            (500.0, 8.0, Some(Zone::Maximized)),
            (32.0, 32.0, Some(Zone::TopLeft)),
            (968.0, 32.0, Some(Zone::TopRight)),
            (32.0, 768.0, Some(Zone::BottomLeft)),
            (968.0, 768.0, Some(Zone::BottomRight)),
            (500.0, 799.0, None),
            (500.0, 400.0, None),
            (-0.1, 10.0, None),
            (1000.0, 0.0, None),
        ] {
            assert_eq!(
                zone_at((x - 1000.0, y + 50.0).into(), output),
                zone,
                "{x}, {y}"
            );
        }
    }
    #[test]
    fn directional_neighbour_weights_cross_axis_and_ignores_wrong_side() {
        let origin = r(0, 0, 100, 100);
        let candidates = [
            r(-100, 0, 100, 100),
            r(50, 300, 100, 100),
            r(150, 0, 100, 100),
            r(0, -100, 100, 100),
            r(0, 100, 100, 100),
        ];
        assert_eq!(neighbour(origin, &candidates, Direction::Right), Some(2));
        assert_eq!(neighbour(origin, &candidates, Direction::Left), Some(0));
        assert_eq!(neighbour(origin, &candidates, Direction::Up), Some(3));
        assert_eq!(neighbour(origin, &candidates, Direction::Down), Some(4));
        assert_eq!(neighbour(origin, &[], Direction::Down), None);
        assert_eq!(neighbour(origin, &[origin], Direction::Down), None);
    }
    #[test]
    fn drag_restore_keeps_horizontal_grab_fraction() {
        let start = Point::from((1000.0, 120.0));
        let origin = restored_drag_origin(start, r(100, 100, 1200, 800), (600, 400).into());
        assert_eq!(origin, Point::from((550, 100)));
        assert_eq!(start.x - origin.x as f64, 600.0 * 0.75);
        assert_eq!(
            restored_drag_origin(
                (200.0, 900.0).into(),
                r(100, 100, 1200, 800),
                (600, 400).into()
            ),
            Point::from((150, 500))
        );
    }

    #[test]
    fn mru_is_unique_and_reorders_only_the_focused_item() {
        let mut mru = Vec::new();
        for id in [1, 2, 3, 2, 2] {
            touch_mru(&mut mru, &id);
        }
        assert_eq!(mru, vec![2, 3, 1]);
        mru.retain(|id| *id != 3);
        touch_mru(&mut mru, &1);
        assert_eq!(mru, vec![1, 2]);
    }
    #[test]
    fn resize_steps_keep_top_left_and_obey_client_limits() {
        let original = r(-500, 81, 600, 400);
        for (dir, w, h) in [
            (Direction::Left, 560, 400),
            (Direction::Right, 640, 400),
            (Direction::Up, 600, 360),
            (Direction::Down, 600, 440),
        ] {
            let resized = resize_step(original, dir, (1, 1).into(), (0, 0).into());
            assert_eq!(resized, r(-500, 81, w, h));
        }
        assert_eq!(
            resize_step(
                r(12, 42, 10, 20),
                Direction::Left,
                (5, 5).into(),
                (0, 0).into()
            ),
            r(12, 42, 5, 20)
        );
        assert_eq!(
            resize_step(original, Direction::Down, (1, 1).into(), (800, 420).into()),
            r(-500, 81, 600, 420)
        );
        assert_eq!(
            resize_step(original, Direction::Right, (700, 1).into(), (650, 0).into()),
            r(-500, 81, 700, 400)
        );
    }
    #[test]
    fn keyboard_half_quarter_and_restore_transitions() {
        use Direction::*;
        let left = WindowMode::Snapped(Zone::Left);
        let bl = WindowMode::Snapped(Zone::BottomLeft);
        assert_eq!(keyboard_mode(WindowMode::Floating, Left), left);
        assert_eq!(keyboard_mode(left, Left), left);
        assert_eq!(keyboard_mode(left, Up), WindowMode::Snapped(Zone::TopLeft));
        assert_eq!(keyboard_mode(left, Down), bl);
        assert_eq!(keyboard_mode(bl, Up), WindowMode::Snapped(Zone::TopLeft));
        assert_eq!(
            keyboard_mode(WindowMode::Maximized, Down),
            WindowMode::Floating
        );
        assert_eq!(
            keyboard_mode(WindowMode::Snapped(Zone::TopLeft), Up),
            WindowMode::Maximized
        );
        assert_eq!(keyboard_mode(left, Right), WindowMode::Floating);
        assert_eq!(
            keyboard_mode(WindowMode::Floating, Down),
            WindowMode::Floating
        );
    }
}
