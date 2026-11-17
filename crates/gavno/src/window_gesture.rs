//! Window gestures and shared, anchored interactive resize geometry.
use crate::{
    gesture::MotionHistory,
    grabs::resize_grab::{self, ResizeEdge as Edge},
    snap::{Direction, Zone},
    window_mode::{mode_state, WindowMode},
    Gavno,
};
use smithay::{
    desktop::Window,
    output::Output,
    utils::{IsAlive, Logical, Point, Rectangle},
    wayland::{compositor::with_states, shell::xdg::SurfaceCachedState},
};

type Rect = Rectangle<i32, Logical>;
type Pos = Point<f64, Logical>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Landing {
    Snap(Zone),
    Minimize,
}
/// Project before clamping: a fast throw beyond the panel still reaches its edge.
pub fn landing(position: Pos, velocity: Pos, output: Rect) -> Option<Landing> {
    let p = position + velocity.upscale(0.18);
    let x = (p.x - output.loc.x as f64).clamp(0.0, (output.size.w - 1).max(0) as f64);
    let y = (p.y - output.loc.y as f64).clamp(0.0, (output.size.h - 1).max(0) as f64);
    let right = output.size.w as f64 - x;
    let bottom = output.size.h as f64 - y;
    // Match the mockup's 16 px edges and 110 px corner reach.
    let zone = if y <= 16.0 {
        if x <= 110.0 {
            Zone::TopLeft
        } else if right <= 110.0 {
            Zone::TopRight
        } else {
            Zone::Maximized
        }
    } else if bottom <= 16.0 {
        if x <= 110.0 {
            Zone::BottomLeft
        } else if right <= 110.0 {
            Zone::BottomRight
        } else {
            return Some(Landing::Minimize);
        }
    } else if x <= 16.0 {
        if y <= 110.0 {
            Zone::TopLeft
        } else if bottom <= 110.0 {
            Zone::BottomLeft
        } else {
            Zone::Left
        }
    } else if right <= 16.0 {
        if y <= 110.0 {
            Zone::TopRight
        } else if bottom <= 110.0 {
            Zone::BottomRight
        } else {
            Zone::Right
        }
    } else {
        return None;
    };
    Some(Landing::Snap(zone))
}

pub fn nearest_edges(r: Rect, p: Pos) -> Edge {
    (if p.x < r.loc.x as f64 + r.size.w as f64 / 2.0 {
        Edge::LEFT
    } else {
        Edge::RIGHT
    }) | (if p.y < r.loc.y as f64 + r.size.h as f64 / 2.0 {
        Edge::TOP
    } else {
        Edge::BOTTOM
    })
}
pub fn nearest_edge(r: Rect, p: Pos) -> Edge {
    [
        (Edge::LEFT, (p.x - r.loc.x as f64).abs()),
        (
            Edge::RIGHT,
            (p.x - (r.loc.x as f64 + r.size.w as f64)).abs(),
        ),
        (Edge::TOP, (p.y - r.loc.y as f64).abs()),
        (
            Edge::BOTTOM,
            (p.y - (r.loc.y as f64 + r.size.h as f64)).abs(),
        ),
    ]
    .into_iter()
    .min_by(|a, b| a.1.total_cmp(&b.1))
    .map(|v| v.0)
    .unwrap_or(Edge::RIGHT)
}
pub fn anchor_for(edges: Edge) -> Pos {
    (
        if edges.contains(Edge::LEFT) { 1.0 } else { 0.0 },
        if edges.contains(Edge::TOP) { 1.0 } else { 0.0 },
    )
        .into()
}
pub fn resized(r: Rect, edges: Edge, delta: Pos) -> Rect {
    let width = if edges.intersects(Edge::LEFT | Edge::RIGHT) {
        (r.size.w as f64
            + if edges.contains(Edge::LEFT) {
                -delta.x
            } else {
                delta.x
            })
        .round()
        .clamp(1.0, i32::MAX as f64) as i32
    } else {
        r.size.w
    };
    let height = if edges.intersects(Edge::TOP | Edge::BOTTOM) {
        (r.size.h as f64
            + if edges.contains(Edge::TOP) {
                -delta.y
            } else {
                delta.y
            })
        .round()
        .clamp(1.0, i32::MAX as f64) as i32
    } else {
        r.size.h
    };
    anchored_size(r, (width, height).into(), anchor_for(edges))
}
pub fn anchored_size(r: Rect, size: smithay::utils::Size<i32, Logical>, anchor: Pos) -> Rect {
    Rectangle::new(
        (
            r.loc
                .x
                .saturating_add(((r.size.w as f64 - size.w as f64) * anchor.x).round() as i32),
            r.loc
                .y
                .saturating_add(((r.size.h as f64 - size.h as f64) * anchor.y).round() as i32),
        )
            .into(),
        size,
    )
}

struct Flick {
    window: Option<Window>,
    delta: Pos,
    history: MotionHistory,
    active: [bool; 2],
}
struct Drag {
    window: Window,
    output: Output,
    initial: Rect,
    pointer: Pos,
    delta: Pos,
    history: MotionHistory,
    moved: bool,
    original_rect: Rect,
    original_state: crate::window_mode::ModeState,
}
struct Pinch {
    window: Window,
    initial: Rect,
    anchor: Pos,
    edges: Edge,
    previous: f64,
    started: bool,
    tile_original: Option<(Output, usize, crate::tiling::Tree<Window>)>,
}
struct Scroll {
    claimed: bool,
    window: Option<Window>,
    edges: Edge,
    delta: Pos,
    initial: Option<Rect>,
    active: [bool; 2],
    workspace: f64,
}
#[derive(Default)]
pub struct WindowGestures {
    flick: Option<Flick>,
    drag: Option<Drag>,
    pinch: Option<Pinch>,
    scroll: Option<Scroll>,
    pub pressure: Pressure,

}

impl Gavno {
    /// Mapping changes must discard streams without restoring their old geometry.
    pub(crate) fn cancel_window_gestures(&mut self) {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.cancel_window_gestures_feature())
    }

    fn cancel_window_gestures_feature(&mut self) {
        let drag = self.window_gestures.drag.take().is_some();
        self.end_window_pinch(false);
        if let Some(s) = self.window_gestures.scroll.take() {
            if let Some(w) = s.window {
                self.finish_interactive_resize(&w);
            }
        }
        if drag {
            self.workspaces.moving_tile = None;
            self.features.call(crate::module::Feature::Switcher, (), || self.overlay.hide_preview(self.start_time.elapsed()));
            self.queue_redraw_all();
        }
    }
    fn gesture_window_is_active(&self, w: &Window) -> bool {
        w.alive()
            && !mode_state(w).minimized
            && self.space.element_location(w).is_some()
            && self.workspaces.membership(w).is_some_and(|(o, i)| {
                self.space.output_geometry(&o).is_some()
                    && self.workspaces.get(&o).is_some_and(|owner| owner.list.active == i)
            })
    }
    fn drag_destination(&self, w: &Window, position: Pos) -> Option<Output> {
        self.space.output_under(position).next().cloned()
            .or_else(|| self.workspaces.membership(w).map(|v| v.0)
                .filter(|o| self.space.output_geometry(o).is_some()))
            .or_else(|| self.output_for_window(w))
    }
    pub fn interactive_window_at(&self, pos: Pos) -> Option<Window> {
        self.space
            .elements()
            .rev()
            .find(|w| {
                self.interactive_rect(w)
                    .is_some_and(|r| r.to_f64().contains(pos))
            })
            .cloned()
            .or_else(|| self.space.element_under(pos).map(|(w, _)| w.clone()))
    }
    pub fn gesture_window(&self) -> Option<Window> {
        self.features.read(crate::module::Feature::WindowGestures, Default::default(), || self.gesture_window_feature())
    }

    fn gesture_window_feature(&self) -> Option<Window> {
        let pos = self.pointer_location();
        if self.shell_contains(pos)
            || self.desktop_transform_at(pos)
            || self
                .layer_hit(
                    pos,
                    &[
                        smithay::wayland::shell::wlr_layer::Layer::Overlay,
                        smithay::wayland::shell::wlr_layer::Layer::Top,
                    ],
                )
                .is_some()
        {
            return None;
        }
        self.interactive_window_at(pos)
            .or_else(|| self.focused_window())
    }
    pub fn window_gesture_locked(&self) -> bool {
        self.features.read(crate::module::Feature::WindowGestures, Default::default(), || self.window_gesture_locked_feature())
    }

    fn window_gesture_locked_feature(&self) -> bool {
        use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint};
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        let Some(surface) = pointer.current_focus() else {
            return false;
        };
        with_pointer_constraint(&surface, &pointer, |c| {
            c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
        })
    }
    /// Pending geometry is authoritative while clients are responding to configure.
    pub fn interactive_rect(&self, w: &Window) -> Option<Rect> {
        self.space.element_location(w)?;
        if let Some(rect) = resize_grab::directed_rect(w) {
            return Some(rect);
        }
        Some(Rectangle::new(
            self.space.element_location(w)?,
            w.toplevel()
                .and_then(|t| t.with_pending_state(|s| s.size))
                .unwrap_or(w.geometry().size),
        ))
    }
    pub fn prepare_interactive(&mut self, w: &Window) {
        let directed = resize_grab::directed_rect(w);
        resize_grab::clear_anchor(w);
        if let Some(rect) = directed {
            if let Some(t) = w.toplevel() {
                t.with_pending_state(|s| s.size = Some(rect.size));
            }
            self.space.map_element(w.clone(), rect.loc, false);
        }
        self.features.call(crate::module::Feature::Animations, (), || self.window_animations.cancel(w));
        if mode_state(w).mode != WindowMode::Floating {
            self.set_window_mode(w, WindowMode::Floating, None);
            self.features.call(crate::module::Feature::Animations, (), || self.window_animations.cancel(w));
            self.reflow_tiling(false);
        }
        crate::window_mode::cancel_natural_center(w);
    }
    /// Resize from the visible slot, including snapped/maximized windows.
    pub fn prepare_interactive_resize(&mut self, w: &Window) {
        let current = self.interactive_rect(w);
        let fixed = mode_state(w).mode != WindowMode::Floating;
        self.prepare_interactive(w);
        if fixed {
            if let Some(r) = current {
                if let Some(t) = w.toplevel() {
                    t.with_pending_state(|s| s.size = Some(r.size));
                    t.send_pending_configure();
                }
                self.space.map_element(w.clone(), r.loc, false);
                self.features.call(crate::module::Feature::Animations, (), || self.window_animations.geometry_immediate(w, r.to_f64()));
            }
        }
    }
    pub fn apply_interactive_resize(&mut self, w: &Window, requested: Rect, anchor: Pos) {
        if !w.alive() || self.space.element_location(w).is_none() {
            return;
        }
        crate::window_mode::cancel_natural_center(w);
        let Some(t) = w.toplevel() else { return };
        let (min, max) = with_states(t.wl_surface(), |s| {
            let mut c = s.cached_state.get::<SurfaceCachedState>();
            let c = c.current();
            (c.min_size, c.max_size)
        });
        let size = (
            requested.size.w.clamp(
                min.w.max(1),
                if max.w > 0 {
                    max.w.max(min.w.max(1))
                } else {
                    i32::MAX
                },
            ),
            requested.size.h.clamp(
                min.h.max(1),
                if max.h > 0 {
                    max.h.max(min.h.max(1))
                } else {
                    i32::MAX
                },
            ),
        )
            .into();
        let rect = anchored_size(requested, size, anchor);
        if self.interactive_rect(w) == Some(rect) {
            return;
        }
        resize_grab::directed_anchor(t.wl_surface(), rect, anchor, self.interactive_rect(w).unwrap_or(rect));
        t.with_pending_state(|s| {s.size=Some(size); s.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing);});
        t.send_pending_configure();
        self.space.map_element(w.clone(), rect.loc, false);
        self.save_workspace_location(w, rect.loc);
        self.features.call(crate::module::Feature::Animations, (), || self.window_animations.geometry_immediate(w, rect.to_f64()));
        self.queue_redraw_all();
    }
    pub fn finish_interactive_resize(&mut self, w: &Window) {
        resize_grab::finish_directed(w);
        if let Some(t) = w.toplevel() {
            t.with_pending_state(|s|s.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing));
            t.send_pending_configure();
        }
    }
    pub fn resize_edges_by(&mut self, w: &Window, edges: Edge, delta: Pos) {
        if self.tiled(w) {
            for (edge, dir, d) in [
                (Edge::LEFT, Direction::Left, delta.x),
                (Edge::RIGHT, Direction::Right, delta.x),
                (Edge::TOP, Direction::Up, delta.y),
                (Edge::BOTTOM, Direction::Down, delta.y),
            ] {
                if edges.contains(edge) && d.round() != 0.0 {
                    self.resize_nearest_split(w, dir, d.round() as i32);
                }
            }
        } else if let Some(r) = self.interactive_rect(w) {
            self.apply_interactive_resize(w, resized(r, edges, delta), anchor_for(edges));
        }
    }
    pub fn begin_window_drag(&mut self, time: u64) {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.begin_window_drag_feature(time))
    }

    fn begin_window_drag_feature(&mut self, time: u64) {
        let Some(w) = self.gesture_window() else {
            return;
        };
        let Some(output) = self
            .workspaces
            .membership(&w)
            .map(|v| v.0)
            .or_else(|| self.active_output())
        else {
            return;
        };
        let Some(initial) = self.interactive_rect(&w) else {
            return;
        };
        let pointer = self.pointer_location();
        let mut history = MotionHistory::default();
        history.push(time, (0.0, 0.0).into());
        crate::window_mode::cancel_natural_center(&w);
        self.window_gestures.drag = Some(Drag {
            original_rect: initial,
            original_state: mode_state(&w),
            window: w,
            output,
            initial,
            pointer,
            delta: (0.0, 0.0).into(),
            history,
            moved: false,
        });
    }
    pub fn update_window_drag(&mut self, delta: Pos, time: u64) -> bool {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.update_window_drag_feature(delta, time))
    }

    fn update_window_drag_feature(&mut self, delta: Pos, time: u64) -> bool {
        let Some(mut d) = self.window_gestures.drag.take() else {
            return false;
        };
        if !self.gesture_window_is_active(&d.window) {
            self.workspaces.moving_tile = None;
            self.features.call(crate::module::Feature::Switcher, (), || self.overlay.hide_preview(self.start_time.elapsed()));
            self.queue_redraw_all();
            return true;
        }
        if delta.x.is_finite()
            && delta.y.is_finite()
            && delta != (0.0, 0.0).into()
            && d.window.alive()
        {
            if !d.moved {
                self.prepare_interactive(&d.window);
                // Restoring a snapped window preserves the pointer's horizontal fraction.
                if let Some(restored) = self.interactive_rect(&d.window) {
                    if restored.size != d.initial.size {
                        d.initial.loc =
                            crate::snap::restored_drag_origin(d.pointer, d.initial, restored.size);
                        d.initial.size = restored.size;
                    }
                }
                self.focus_window(Some(&d.window));
                d.moved = true;
            }
            d.delta += delta;
            d.history.push(time, d.delta);
            let loc = d.initial.loc.to_f64() + d.delta;
            self.features.call(crate::module::Feature::Animations, (), || self.window_animations.cancel(&d.window));
            self.space
                .map_element(d.window.clone(), loc.to_i32_round(), true);
            if self.tiled(&d.window) {
                self.workspaces.moving_tile = Some(d.window.clone());
                self.preview_tile_drop(&d.window, d.pointer + d.delta);
            } else {
                if let Some(output) = self.drag_destination(&d.window, d.pointer + d.delta) {
                    self.preview_window_landing(&d.window, &output, d.pointer + d.delta);
                }
            }
            self.queue_redraw_all();
        }
        self.window_gestures.drag = Some(d);
        true
    }
    pub fn end_window_drag(&mut self, time: u64, cancelled: bool) -> bool {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.end_window_drag_feature(time, cancelled))
    }

    fn end_window_drag_feature(&mut self, time: u64, cancelled: bool) -> bool {
        let Some(mut d) = self.window_gestures.drag.take() else {
            return false;
        };
        if !d.moved {
            return true;
        }
        if !self.gesture_window_is_active(&d.window) {
            self.workspaces.moving_tile = None;
            self.features.call(crate::module::Feature::Switcher, (), || self.overlay.hide_preview(self.start_time.elapsed()));
            self.queue_redraw_all();
            return true;
        }
        d.history.push(time, d.delta);
        let velocity = if cancelled {
            (0.0, 0.0).into()
        } else {
            d.history.velocity(time)
        };
        self.workspaces.moving_tile = None;
        self.features.call(crate::module::Feature::Switcher, (), || self.overlay.hide_preview(self.start_time.elapsed()));
        if self.tiled(&d.window) {
            if !cancelled && velocity.x.hypot(velocity.y) > 600.0 {
                let dir = if velocity.x.abs() > velocity.y.abs() {
                    if velocity.x < 0.0 {
                        Direction::Left
                    } else {
                        Direction::Right
                    }
                } else if velocity.y < 0.0 {
                    Direction::Up
                } else {
                    Direction::Down
                };
                self.move_tile_window(&d.window, dir);
            } else {
                self.drop_tile(
                    &d.window,
                    if cancelled {
                        (-1e9, -1e9).into()
                    } else {
                        d.pointer + d.delta
                    },
                );
            }
        } else if cancelled {
            if d.original_state.mode != WindowMode::Floating {
                self.set_window_mode(&d.window, d.original_state.mode, Some(d.output));
                crate::window_mode::with_mode_state(&d.window, |s| *s = d.original_state);
            } else {
                let from = self.interactive_rect(&d.window).unwrap_or(d.initial);
                self.space
                    .map_element(d.window.clone(), d.original_rect.loc, false);
                crate::anim::animate_geometry(self, &d.window, from);
            }
        } else if let Some(output) = self.drag_destination(&d.window, d.pointer + d.delta) {
            let Some(geo) = self.space.output_geometry(&output) else { return true };
            match self.landing_for_features(d.pointer + d.delta, velocity, geo) {
                Some(Landing::Minimize) => {
                    self.minimize_window(&d.window);
                    self.seed_minimize_velocity(&d.window, velocity);
                }
                Some(Landing::Snap(z)) => {
                    self.guard(crate::module::Feature::Snap, (), |gv| {
                        gv.set_window_mode(&d.window, z.mode(), Some(output));
                        gv.seed_geometry_velocity(&d.window, velocity);
                    });
                }
                None => {
                    if let Some(from) = self.interactive_rect(&d.window) {
                        let mut to = from;
                        to.loc = (from.loc.to_f64() + velocity.upscale(0.18)).to_i32_round();
                        // Keep a reachable portion of a thrown window on its output.
                        let visible_w = 48.min(to.size.w.max(1)).min(geo.size.w.max(1));
                        let visible_h = 48.min(to.size.h.max(1)).min(geo.size.h.max(1));
                        to.loc.x = to.loc.x.clamp(
                            geo.loc
                                .x
                                .saturating_sub(to.size.w)
                                .saturating_add(visible_w),
                            geo.loc.x + geo.size.w - visible_w,
                        );
                        to.loc.y = to
                            .loc
                            .y
                            .clamp(geo.loc.y, geo.loc.y + geo.size.h - visible_h);
                        self.space.map_element(d.window.clone(), to.loc, false);
                        crate::anim::animate_geometry(self, &d.window, from);
                        self.seed_geometry_velocity(&d.window, velocity);
                    }
                }
            }
            if let Some(loc) = self.space.element_location(&d.window) {
                self.save_workspace_location(&d.window, loc);
            }
        }
        self.queue_redraw_all();
        true
    }
    fn landing_for_features(&self, p: Pos, velocity: Pos, output: Rect) -> Option<Landing> {
        landing(p, velocity, output).filter(|target| match target {
            Landing::Snap(_) => self.features.on(crate::module::Feature::Snap),
            Landing::Minimize => self.features.on(crate::module::Feature::Dock),
        })
    }

    fn preview_window_landing(&mut self, w: &Window, output: &Output, p: Pos) {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.preview_window_landing_feature(w, output, p))
    }

    fn preview_window_landing_feature(&mut self, w: &Window, output: &Output, p: Pos) {
        let Some(geo) = self.space.output_geometry(output) else {
            return;
        };
        match self.landing_for_features(p, (0.0, 0.0).into(), geo) {
            Some(Landing::Snap(z)) => {
                let r = z.rect(self.work_area(output), self.config.gap);
                self.guard(crate::module::Feature::Switcher, (), |gv| gv.overlay.preview(
                    output.clone(),
                    z,
                    gv.interactive_rect(w).unwrap_or(r),
                    r,
                    gv.start_time.elapsed(),
                ));
            }
            Some(Landing::Minimize) => {
                let r = Rectangle::new(
                    (geo.loc.x + geo.size.w / 2 - 24, geo.loc.y + geo.size.h - 64).into(),
                    (48, 48).into(),
                );
                self.guard(crate::module::Feature::Switcher, (), |gv| gv.overlay.preview(
                    output.clone(),
                    Zone::Maximized,
                    gv.interactive_rect(w).unwrap_or(r),
                    r,
                    gv.start_time.elapsed(),
                ));
            }
            None => self.features.call(crate::module::Feature::Switcher, (), || self.overlay.hide_preview(self.start_time.elapsed())),
        }
    }
    pub fn tile_resize_edges(&self, w: &Window, preferred: Edge) -> Edge {
        self.features.read(crate::module::Feature::Tiling, preferred, || {
            let Some((o, i)) = self.workspaces.membership(w) else {
                return preferred;
            };
            let Some(owner) = self.workspaces.get(&o) else {
                return preferred;
            };
            let tree = &owner.list.items[i].tree;
            let area = self.work_area(&o);
            let gap = self.config.gap;
            let mut edges = Edge::empty();
            for (edge, dir) in [
                (Edge::LEFT, Direction::Left),
                (Edge::RIGHT, Direction::Right),
                (Edge::TOP, Direction::Up),
                (Edge::BOTTOM, Direction::Down),
            ] {
                if preferred.contains(edge) {
                    if tree.side(w, dir, area, gap).is_some() {
                        edges |= edge;
                    }
                }
            }
            edges

        })
    }
    pub fn begin_window_pinch(&mut self, proportional: bool) {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.begin_window_pinch_feature(proportional))
    }

    fn begin_window_pinch_feature(&mut self, proportional: bool) {
        if proportional
            && self
                .interactive_window_at(self.pointer_location())
                .is_none()
        {
            return;
        }
        let Some(window) = self.gesture_window() else {
            return;
        };
        let Some(initial) = self.interactive_rect(&window) else {
            return;
        };
        let fixed = nearest_edge(initial, self.pointer_location());
        let anchor = if proportional {
            (0.5, 0.5).into()
        } else if fixed == Edge::LEFT {
            (0.0, 0.5).into()
        } else if fixed == Edge::RIGHT {
            (1.0, 0.5).into()
        } else if fixed == Edge::TOP {
            (0.5, 0.0).into()
        } else {
            (0.5, 1.0).into()
        };
        let preferred = nearest_edges(initial, self.pointer_location());
        let edges = if self.tiled(&window) {
            self.tile_resize_edges(&window, preferred)
        } else {
            preferred
        };
        let tile_original = if self.tiled(&window) {
            self.workspaces.membership(&window).and_then(|(o, i)| {
                self.workspaces
                    .get(&o)
                    .map(|owner| (o.clone(), i, owner.list.items[i].tree.clone()))
            })
        } else {
            None
        };
        crate::window_mode::cancel_natural_center(&window);
        self.window_gestures.pinch = Some(Pinch {
            window,
            initial,
            anchor,
            edges,
            previous: 1.0,
            started: false,
            tile_original,
        });
    }
    pub fn update_window_pinch(&mut self, scale: f64) {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.update_window_pinch_feature(scale))
    }

    fn update_window_pinch_feature(&mut self, scale: f64) {
        let Some(mut p) = self.window_gestures.pinch.take() else {
            return;
        };
        if !self.gesture_window_is_active(&p.window) {
            self.finish_interactive_resize(&p.window);
            return;
        }
        if scale.is_finite() && scale > 0.0 && scale != p.previous {
            if !p.started {
                self.prepare_interactive_resize(&p.window);
                p.initial = self.interactive_rect(&p.window).unwrap_or(p.initial);
                p.started = true;
            }
            if self.tiled(&p.window) {
                let mut delta = Point::from((
                    (p.initial.size.w as f64 * (scale - 1.0)).round()
                        - (p.initial.size.w as f64 * (p.previous - 1.0)).round(),
                    (p.initial.size.h as f64 * (scale - 1.0)).round()
                        - (p.initial.size.h as f64 * (p.previous - 1.0)).round(),
                ));
                if p.edges.contains(Edge::LEFT) {
                    delta.x = -delta.x;
                }
                if p.edges.contains(Edge::TOP) {
                    delta.y = -delta.y;
                }
                self.resize_edges_by(&p.window, p.edges, delta);
            } else {
                let size = (
                    (p.initial.size.w as f64 * scale)
                        .round()
                        .clamp(1.0, 32768.0) as i32,
                    (p.initial.size.h as f64 * scale)
                        .round()
                        .clamp(1.0, 32768.0) as i32,
                )
                    .into();
                self.apply_interactive_resize(
                    &p.window,
                    anchored_size(p.initial, size, p.anchor),
                    p.anchor,
                );
            }
            p.previous = scale;
        }
        self.window_gestures.pinch = Some(p);
    }
    pub fn end_window_pinch(&mut self, cancelled: bool) {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.end_window_pinch_feature(cancelled))
    }

    fn end_window_pinch_feature(&mut self, cancelled: bool) {
        if let Some(p) = self.window_gestures.pinch.take() {
            if cancelled && p.started && self.gesture_window_is_active(&p.window) {
                if self.tiled(&p.window) {
                    if let Some((o, i, tree)) = p.tile_original {
                        if self.workspaces.membership(&p.window) == Some((o.clone(), i)) {
                            if let Some(ws) = self.workspaces.ensure(&o).list.items.get_mut(i) {
                                ws.tree = tree;
                            }
                            self.reflow_tiling(true);
                        }
                    }
                } else {
                    let from = self.interactive_rect(&p.window);
                    self.apply_interactive_resize(&p.window, p.initial, p.anchor);
                    self.features.call(crate::module::Feature::Animations, (), || self.window_animations.cancel(&p.window));
                    if let Some(from) = from {
                        crate::anim::animate_geometry(self, &p.window, from);
                    }
                }
            }
            if p.started {
                self.finish_interactive_resize(&p.window);
            }
        }
    }
    /// Finger-axis streams have no begin event; latch on their first non-stop frame.
    pub fn precision_scroll(&mut self, delta: Pos, present: [Option<f64>; 2]) -> bool {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.precision_scroll_feature(delta, present))
    }

    fn precision_scroll_feature(&mut self, delta: Pos, present: [Option<f64>; 2]) -> bool {
        if self.window_gestures.scroll.is_none() {
            if delta == (0.0, 0.0).into() {
                return false;
            }
            let claimed = self
                .seat
                .get_keyboard()
                .is_some_and(|k| k.modifier_state().logo)
                && !self.shell_contains(self.pointer_location());
            let window = if claimed
                && !self.features.read(crate::module::Feature::Overview, false, || self.overview.visible())
                && !self.shell_contains(self.pointer_location())
            {
                self.interactive_window_at(self.pointer_location())
            } else {
                None
            };
            let initial = window.as_ref().and_then(|w| self.interactive_rect(w));
            let edges = initial
                .map(|r| nearest_edges(r, self.pointer_location()))
                .unwrap_or(Edge::empty());
            if let Some(w) = &window {
                if claimed {
                    self.prepare_interactive_resize(w);
                }
            }
            let initial = window.as_ref().and_then(|w| self.interactive_rect(w));
            self.window_gestures.scroll = Some(Scroll {
                claimed,
                window,
                edges,
                initial,
                delta: (0.0, 0.0).into(),
                active: [false; 2],
                workspace: 0.0,
            });
        }
        let mut s = self.window_gestures.scroll.take().unwrap();
        if s.window.as_ref().is_some_and(|w| !self.gesture_window_is_active(w)) {
            if let Some(w) = s.window { self.finish_interactive_resize(&w); }
            return s.claimed;
        }
        for (i, v) in present.into_iter().enumerate() {
            if let Some(v) = v {
                s.active[i] = v != 0.0;
            }
        }
        if s.claimed {
            if let Some(w) = &s.window {
                if self.tiled(w) {
                    let old = s.delta;
                    s.delta += delta;
                    self.resize_edges_by(
                        w,
                        s.edges,
                        (
                            s.delta.x.round() - old.x.round(),
                            s.delta.y.round() - old.y.round(),
                        )
                            .into(),
                    );
                } else if let Some(r) = s.initial {
                    s.delta += delta;
                    self.apply_interactive_resize(
                        w,
                        resized(r, s.edges, s.delta),
                        anchor_for(s.edges),
                    );
                }
            } else {
                // Scroll axes describe viewport movement; the window delta is
                // content motion, so left/up content selects the next workspace.
                s.workspace -= if delta.x.abs() > delta.y.abs() {
                    delta.x
                } else {
                    delta.y
                };
                if s.workspace.abs() >= 30.0 {
                    self.workspace_step(s.workspace < 0.0);
                    s.workspace = 0.0;
                }
            }
        }
        let claimed = s.claimed;
        if s.active.iter().any(|v| *v) {
            self.window_gestures.scroll = Some(s);
        } else if let Some(w) = s.window {
            self.finish_interactive_resize(&w);
        }
        claimed
    }
}

/// Pressure uses only motion rejected by clamping, never time spent hovering.
#[derive(Default)]
pub struct Pressure {
    amount: f64,
    latched: bool,
}
impl Pressure {
    pub fn push(&mut self, current: Pos, proposed: Pos, output: Rect) -> bool {
        let near =
            current.x - output.loc.x as f64 <= 16.0 && current.y - output.loc.y as f64 <= 16.0;
        if !near {
            self.amount = 0.0;
            self.latched = false;
            return false;
        }
        let dx = (output.loc.x as f64 - proposed.x).max(0.0);
        let dy = (output.loc.y as f64 - proposed.y).max(0.0);
        if dx == 0.0 && dy == 0.0 && (proposed.x > current.x || proposed.y > current.y) {
            self.amount = 0.0;
        }
        self.amount = (self.amount + dx.hypot(dy)).min(100.0);
        if self.amount >= 100.0 && !self.latched {
            self.latched = true;
            true
        } else {
            false
        }
    }
}

impl Gavno {
    pub fn overview_flick_axis(
        &mut self,
        delta: Pos,
        time: u64,
        present: [Option<f64>; 2],
    ) -> bool {
        if !self.features.on(crate::module::Feature::WindowGestures) { return false; }
        self.guard(crate::module::Feature::Overview, Default::default(), |gv| {
            if !gv.features.read(crate::module::Feature::Overview, false, || gv.overview.visible()) {
                gv.window_gestures.flick = None;
                return false;
            }
            if gv.window_gestures.flick.is_none() {
                let w = gv
                    .overview
                    .output
                    .as_ref()
                    .and_then(|o| gv.space.output_geometry(o))
                    .and_then(|geo| {
                        let pos = gv.pointer_location() - geo.loc.to_f64();
                        gv.overview
                            .hits
                            .iter()
                            .rev()
                            .find(|v| v.rect.contains(pos))
                            .map(|v| v.window.clone())
                    });
                if let Some(window) = &w {
                    use crate::ui::spring::{Spring, SpringParams};
                    gv.overview.flick = Some((
                        window.clone(),
                        Spring::new(0.0, SpringParams::SNAPPY),
                        Spring::new(0.0, SpringParams::SNAPPY),
                    ));
                }
                let mut history = MotionHistory::default();
                history.push(time, (0.0, 0.0).into());
                gv.window_gestures.flick = Some(Flick {
                    window: w,
                    delta: (0.0, 0.0).into(),
                    history,
                    active: [false; 2],
                });
            }
            let Some(mut f) = gv.window_gestures.flick.take() else {
                return true;
            };
            f.delta += delta;
            f.history.push(time, f.delta);
            if let Some((_, x, y)) = &mut gv.overview.flick {
                x.jump(f.delta.x);
                y.jump(f.delta.y);
                if delta != (0.0, 0.0).into() {
                    gv.queue_redraw_all();
                }
            }
            for (i, v) in present.into_iter().enumerate() {
                if let Some(v) = v {
                    f.active[i] = v != 0.0;
                }
            }
            if f.active.iter().any(|v| *v) {
                gv.window_gestures.flick = Some(f);
            } else {
                let v = f.history.velocity(time);
                if let Some((_, x, y)) = &mut gv.overview.flick {
                    let now = gv.start_time.elapsed();
                    x.advance(now);
                    y.advance(now);
                    x.set(0.0);
                    y.set(0.0);
                    x.velocity = v.x;
                    y.velocity = v.y;
                    if x.is_active() || y.is_active() {
                        gv.queue_redraw_all();
                    }
                }
                if f.delta.y < -12.0 && v.y < -150.0 && f.delta.y.abs() > f.delta.x.abs() {
                    if let Some(w) = f.window.filter(|w| w.alive()) {
                        if let Some(t) = w.toplevel() {
                            t.send_close();
                        }
                    }
                }
            }
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{
        input::pointer::MotionEvent,
        output::{Mode, PhysicalProperties, Subpixel},
        utils::SERIAL_COUNTER,
    };
    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rectangle::new((x, y).into(), (w, h).into())
    }
    #[test]
    fn projected_throw_selects_halves_quarters_maximize_and_dock() {
        let output = r(200, 100, 1600, 900);
        let p = (1000.0, 550.0).into();
        for (v, expected) in [
            ((5000.0, 0.0), Landing::Snap(Zone::Right)),
            ((-5000.0, 0.0), Landing::Snap(Zone::Left)),
            ((0.0, -3000.0), Landing::Snap(Zone::Maximized)),
            ((5000.0, -3000.0), Landing::Snap(Zone::TopRight)),
            ((-5000.0, 3000.0), Landing::Snap(Zone::BottomLeft)),
            ((0.0, 3000.0), Landing::Minimize),
        ] {
            assert_eq!(landing(p, v.into(), output), Some(expected));
        }
        assert_eq!(landing(p, (0.0, 0.0).into(), output), None);
        assert_eq!(
            landing((1000.0, 995.0).into(), (0.0, 0.0).into(), output),
            Some(Landing::Minimize)
        );
    }
    #[test]
    fn resize_and_pinch_choose_nearest_edges_with_offset_geometry() {
        let rect = r(100, 200, 800, 600);
        assert_eq!(nearest_edges(rect, (899.0, 201.0).into()), Edge::TOP_RIGHT);
        assert_eq!(
            nearest_edges(rect, (101.0, 799.0).into()),
            Edge::BOTTOM_LEFT
        );
        for (p, e) in [
            ((102.0, 500.0), Edge::LEFT),
            ((898.0, 500.0), Edge::RIGHT),
            ((500.0, 202.0), Edge::TOP),
            ((500.0, 798.0), Edge::BOTTOM),
        ] {
            assert_eq!(nearest_edge(rect, p.into()), e);
        }
    }
    #[test]
    fn scroll_resize_anchors_every_opposite_edge_even_at_minimum_size() {
        let rect = r(100, 200, 800, 600);
        for e in [
            Edge::TOP_LEFT,
            Edge::TOP_RIGHT,
            Edge::BOTTOM_LEFT,
            Edge::BOTTOM_RIGHT,
        ] {
            for d in [(35.5, 25.5), (-45.0, -55.0), (2000.0, 2000.0)] {
                let out = resized(rect, e, d.into());
                if e.contains(Edge::LEFT) {
                    assert_eq!(out.loc.x + out.size.w, 900);
                } else {
                    assert_eq!(out.loc.x, 100);
                }
                if e.contains(Edge::TOP) {
                    assert_eq!(out.loc.y + out.size.h, 800);
                } else {
                    assert_eq!(out.loc.y, 200);
                }
            }
        }
        assert_eq!(
            resized(rect, Edge::RIGHT, (30.0, 100.0).into()),
            r(100, 200, 830, 600)
        );
    }
    #[test]
    fn centre_and_pinch_anchors_stay_fixed() {
        let rect = r(100, 200, 800, 600);
        let out = anchored_size(rect, (1000, 800).into(), (0.5, 0.5).into());
        assert_eq!(out, r(0, 100, 1000, 800));
        let out = anchored_size(rect, (1000, 800).into(), (1.0, 0.5).into());
        assert_eq!(out.loc.x + out.size.w, 900);
        assert_eq!(out.loc.y + out.size.h / 2, 500);
    }
    #[test]
    fn pressure_requires_clamped_motion_and_resets_on_leaving() {
        let output = r(200, 100, 1600, 900);
        let corner = (200.0, 100.0).into();
        let mut p = Pressure::default();
        for _ in 0..100 {
            assert!(!p.push(corner, corner, output));
        }
        for _ in 0..4 {
            assert!(!p.push(corner, (180.0, 100.0).into(), output));
        }
        assert!(p.push(corner, (180.0, 100.0).into(), output));
        assert!(!p.push(corner, (0.0, 0.0).into(), output));
        assert!(!p.push((300.0, 200.0).into(), (0.0, 0.0).into(), output));
        assert!(p.push(corner, (100.0, 100.0).into(), output));
        let mut p = Pressure::default();
        p.push(corner, (120.0, 100.0).into(), output);
        p.push(corner, (201.0, 100.0).into(), output);
        assert!(!p.push(corner, (180.0, 100.0).into(), output));
    }
    #[test]
    fn tiling_drop_selects_centre_or_nearest_edge_and_reparents() {
        use crate::tiling::{drop_edge, Tree};
        let area = r(0, 0, 1600, 1000);
        let rect = r(200, 100, 400, 400);
        assert_eq!(drop_edge(rect, (400.0, 300.0).into()), None);
        for (p, d) in [
            ((205.0, 300.0), Direction::Left),
            ((595.0, 300.0), Direction::Right),
            ((400.0, 105.0), Direction::Up),
            ((400.0, 495.0), Direction::Down),
        ] {
            assert_eq!(drop_edge(rect, p.into()), Some(d));
        }
        for d in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            let mut tree = Tree::default();
            for k in 0..4 {
                tree.insert(k, None, area, 12);
            }
            assert!(tree.drop_at(&0, &3, Some(d)));
            let tiles = tree.layout(area, 12).0;
            assert_eq!(tiles.len(), 4);
            let a = tiles.iter().find(|(k, _)| *k == 0).unwrap().1;
            let b = tiles.iter().find(|(k, _)| *k == 3).unwrap().1;
            match d {
                Direction::Left => assert!(a.loc.x < b.loc.x),
                Direction::Right => assert!(a.loc.x > b.loc.x),
                Direction::Up => assert!(a.loc.y < b.loc.y),
                Direction::Down => assert!(a.loc.y > b.loc.y),
            }
        }
    }
    fn setup() -> (crate::state::tests::Harness, Output) {
        let mut h = crate::state::tests::Harness::new();
        let o = Output::new(
            "gesture-test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        o.change_current_state(
            Some(Mode {
                size: (1600, 1000).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&o, (0, 0));
        h.gv.workspaces.ensure(&o);
        (h, o)
    }
    fn map(h: &mut crate::state::tests::Harness, rect: Rect) -> Window {
        let w = h.window();
        h.gv.map_new_window(w.clone());
        w.toplevel()
            .unwrap()
            .with_pending_state(|s| s.size = Some(rect.size));
        h.gv.space.map_element(w.clone(), rect.loc, true);
        w
    }
    fn point(gv: &mut Gavno, p: Pos) {
        let pointer = gv.seat.get_pointer().unwrap();
        pointer.motion(
            gv,
            None,
            &MotionEvent {
                location: p,
                serial: SERIAL_COUNTER.next_serial(),
                time: 1,
            },
        );
    }
    #[test]
    fn drag_targets_pointer_window_tracks_directly_and_resting_begin_is_idle() {
        let (mut h, _) = setup();
        let a = map(&mut h, r(100, 100, 400, 300));
        let b = map(&mut h, r(900, 100, 400, 300));
        h.gv.focus_window(Some(&b));
        point(&mut h.gv, (200.0, 200.0).into());
        h.gv.window_animations.cancel(&a);
        h.gv.window_animations.cancel(&b);
        h.gv.begin_window_drag(0);
        assert!(!h.gv.is_animating());
        h.gv.update_window_drag((30.0, 20.0).into(), 10_000);
        assert_eq!(h.gv.space.element_location(&a), Some((130, 120).into()));
        assert_eq!(h.gv.space.element_location(&b), Some((900, 100).into()));
        assert!(!h.gv.window_animations.is_animating());
        h.gv.end_window_drag(300_000, false);
        assert!(!h.gv.window_animations.is_animating());
    }
    #[test]
    fn workspace_switch_cancels_drag_and_stale_updates_cannot_remap_windows() {
        let (mut h, o) = setup();
        let w = map(&mut h, r(100, 100, 400, 300));
        point(&mut h.gv, (200.0, 200.0).into());
        h.gv.begin_window_drag(0);
        h.gv.update_window_drag((30.0, 20.0).into(), 10_000);
        h.gv.switch_workspace(&o, 1);
        assert!(h.gv.window_gestures.drag.is_none());
        assert!(!h.gv.update_window_drag((30.0, 20.0).into(), 20_000));
        assert!(!h.gv.end_window_drag(30_000, false));
        assert!(h.gv.space.element_location(&w).is_none());
        assert_eq!(h.gv.workspaces.membership(&w), Some((o, 0)));
    }
    #[test]
    fn drag_validates_mapping_minimization_and_active_membership() {
        for invalid in 0..3 {
            let (mut h, o) = setup();
            let w = map(&mut h, r(100, 100, 400, 300));
            point(&mut h.gv, (200.0, 200.0).into());
            h.gv.begin_window_drag(0);
            match invalid {
                0 => h.gv.space.unmap_elem(&w),
                1 => crate::window_mode::with_mode_state(&w, |s| s.minimized = true),
                _ => h.gv.workspaces.ensure(&o).list.active = 1,
            }
            let before = h.gv.space.element_location(&w);
            h.gv.update_window_drag((50.0, 0.0).into(), 10_000);
            assert!(h.gv.window_gestures.drag.is_none());
            assert_eq!(h.gv.space.element_location(&w), before);
            assert!(!h.gv.end_window_drag(20_000, false));
        }
    }
    #[test]
    fn cross_output_drag_lands_and_snaps_on_destination_before_or_after_transfer() {
        for transfer in [false, true] {
            for snap in [false, true] {
                let (mut h, left) = setup();
                let right = Output::new("right".into(), left.physical_properties());
                right.change_current_state(Some(Mode { size: (1600, 1000).into(), refresh: 60000 }), None, None, Some((1600, 0).into()));
                h.gv.space.map_output(&right, (1600, 0));
                h.gv.workspaces.ensure(&right);
                let w = map(&mut h, r(200, 200, 400, 300));
                point(&mut h.gv, (400.0, 350.0).into());
                h.gv.begin_window_drag(0);
                let dx = if snap { 2795.0 } else { 1600.0 };
                h.gv.update_window_drag((dx, 0.0).into(), 10_000);
                if transfer { h.gv.refresh_workspaces(); }
                h.gv.end_window_drag(300_000, false);
                if snap {
                    assert_eq!(mode_state(&w).mode, WindowMode::Snapped(Zone::Right));
                    assert_eq!(h.gv.space.element_location(&w), Some(Zone::Right.rect(h.gv.work_area(&right), h.gv.config.gap).loc));
                } else {
                    assert_eq!(h.gv.space.element_location(&w), Some((1800, 200).into()));
                    assert_eq!(mode_state(&w).mode, WindowMode::Floating);
                }
            }
        }
    }
    #[test]
    fn four_finger_down_throw_minimizes_and_preserves_workspace_membership() {
        let (mut h, o) = setup();
        let w = map(&mut h, r(500, 250, 400, 300));
        point(&mut h.gv, (700.0, 400.0).into());
        h.gv.begin_window_drag(0);
        h.gv.update_window_drag((0.0, 180.0).into(), 30_000);
        h.gv.update_window_drag((0.0, 180.0).into(), 60_000);
        h.gv.end_window_drag(60_000, false);
        assert!(mode_state(&w).minimized);
        assert!(h.gv.space.element_location(&w).is_none());
        assert_eq!(h.gv.workspaces.membership(&w), Some((o, 0)));
    }
    #[test]
    fn pinch_idle_then_direct_scale_and_centre_resize() {
        let (mut h, _) = setup();
        let w = map(&mut h, r(200, 200, 400, 300));
        point(&mut h.gv, (595.0, 350.0).into());
        h.gv.window_animations.cancel(&w);
        h.gv.begin_window_pinch(false);
        assert!(!h.gv.is_animating());
        h.gv.update_window_pinch(1.5);
        assert_eq!(h.gv.interactive_rect(&w), Some(r(0, 125, 600, 450)));
        assert!(!h.gv.window_animations.is_animating());
        h.gv.end_window_pinch(true);
        assert_eq!(h.gv.interactive_rect(&w), Some(r(200, 200, 400, 300)));
        h.gv.begin_window_pinch(true);
        h.gv.update_window_pinch(1.5);
        assert_eq!(h.gv.interactive_rect(&w), Some(r(100, 125, 600, 450)));
    }
    #[test]
    fn tile_pinch_grows_share_from_either_side_and_cancel_restores_the_tree() {
        let (mut h, o) = setup();
        let a = map(&mut h, r(200, 200, 400, 300));
        let b = map(&mut h, r(800, 200, 400, 300));
        h.gv.toggle_workspace_tiling(&o);
        let area = h.gv.work_area(&o);
        let gap = h.gv.config.gap;
        for w in [&a, &b] {
            let before = h.gv.workspaces.get(&o).unwrap().list.items[0]
                .tree
                .layout(area, gap)
                .0;
            let rect = before.iter().find(|(n, _)| n == w).unwrap().1;
            point(
                &mut h.gv,
                (
                    if w == &a { (rect.loc.x + rect.size.w - 5) as f64 } else { (rect.loc.x + 5) as f64 },
                    rect.loc.y as f64 + rect.size.h as f64 / 2.0,
                )
                    .into(),
            );
            h.gv.begin_window_pinch(false);
            h.gv.update_window_pinch(1.1);
            assert!(h.gv.interactive_rect(w).unwrap().size.w > rect.size.w);
            h.gv.end_window_pinch(true);
            assert_eq!(
                h.gv.workspaces.get(&o).unwrap().list.items[0]
                    .tree
                    .layout(area, gap)
                    .0,
                before
            );
        }
    }
    #[test]
    fn precision_scroll_ownership_is_latched_and_opposite_edge_stays_fixed() {
        use smithay::input::keyboard::ModifiersState;
        let (mut h, _) = setup();
        let w = map(&mut h, r(200, 200, 400, 300));
        point(&mut h.gv, (590.0, 350.0).into());
        let keyboard = h.gv.seat.get_keyboard().unwrap();
        assert!(!h
            .gv
            .precision_scroll((10.0, 0.0).into(), [Some(10.0), None]));
        keyboard.set_modifier_state(ModifiersState {
            logo: true,
            ..Default::default()
        });
        assert!(!h
            .gv
            .precision_scroll((10.0, 0.0).into(), [Some(10.0), None]));
        assert_eq!(h.gv.interactive_rect(&w), Some(r(200, 200, 400, 300)));
        assert!(!h.gv.precision_scroll((0.0, 0.0).into(), [Some(0.0), None]));
        assert!(h
            .gv
            .precision_scroll((10.0, 0.0).into(), [Some(10.0), None]));
        keyboard.set_modifier_state(ModifiersState::default());
        assert!(h
            .gv
            .precision_scroll((15.0, 0.0).into(), [Some(15.0), None]));
        assert_eq!(h.gv.interactive_rect(&w), Some(r(200, 200, 425, 300)));
        assert!(h.gv.precision_scroll((0.0, 0.0).into(), [Some(0.0), None]));
        assert!(h.gv.window_gestures.scroll.is_none());
        assert!(!h.gv.window_animations.is_animating());
    }
    #[test]
    fn resize_mode_keys_motion_and_exit_are_button_free_and_idle() {
        use smithay::input::keyboard::keysyms;
        let (mut h, _) = setup();
        let w = map(&mut h, r(200, 200, 400, 300));
        point(&mut h.gv, (599.0, 350.0).into());
        h.gv.enter_resize_mode();
        assert!(h.gv.overlay.resize_mode.active());
        assert!(!h.gv.window_animations.is_animating());
        h.gv.resize_mode_key(keysyms::KEY_Right, false);
        h.gv.resize_mode_key(keysyms::KEY_Right, true);
        assert_eq!(h.gv.interactive_rect(&w), Some(r(200, 200, 460, 300)));
        h.gv.resize_mode_key(keysyms::KEY_Tab, false);
        h.gv.resize_mode_key(keysyms::KEY_Down, false);
        assert_eq!(h.gv.interactive_rect(&w), Some(r(200, 200, 460, 310)));
        assert!(h.gv.resize_mode_motion((609.0, 355.0).into()));
        assert_eq!(h.gv.interactive_rect(&w), Some(r(200, 200, 470, 315)));
        assert!(!h.gv.resize_mode_motion((400.0, 100.0).into()));
        assert!(!h.gv.overlay.resize_mode.active());
        h.gv.enter_resize_mode();
        h.gv.resize_mode_key(keysyms::KEY_Return, false);
        assert!(!h.gv.overlay.resize_mode.active());
    }
    #[test]
    fn frontmost_content_blocks_lower_resize_bands() {
        let (mut h, _) = setup();
        let _lower = map(&mut h, r(200, 200, 400, 300));
        let upper = map(&mut h, r(100, 100, 400, 300));
        assert!(h.gv.resize_band_at((195.0, 300.0).into()).is_none());
        assert_eq!(
            h.gv.resize_band_at((100.0, 300.0).into()).map(|v| v.0),
            Some(upper)
        );
    }
    #[test]
    fn client_chosen_commit_size_preserves_anchor() {
        let (mut h, _) = setup();
        let w = map(&mut h, r(100, 200, 800, 600));
        h.gv.apply_interactive_resize(&w, r(200, 300, 700, 500), (1.0, 1.0).into());
        // The bufferless test client has geometry 0x0, deliberately different
        // from the configure: its bottom-right still remains at (900,800).
        resize_grab::handle_commit(&mut h.gv.space, w.toplevel().unwrap().wl_surface());
        let loc = h.gv.space.element_location(&w).unwrap();
        let actual = w.geometry().size;
        assert_eq!((loc.x + actual.w, loc.y + actual.h), (900, 800));
    }
}

#[cfg(test)]
mod feature_tests {
    use super::*;
    #[test]
    fn disabled_snap_and_dock_do_not_claim_window_throw_targets() {
        let mut h = crate::state::tests::Harness::new();
        let output = Rectangle::new((0, 0).into(), (1600, 900).into());
        let p = (800.0, 450.0).into();
        assert!(matches!(h.gv.landing_for_features(p, (5000.0, 0.0).into(), output), Some(Landing::Snap(_))));
        h.gv.config.modules.snap = false;
        h.gv.config.modules.dock = false;
        h.gv.features = crate::module::Features::new(&h.gv.config);
        assert_eq!(h.gv.landing_for_features(p, (5000.0, 0.0).into(), output), None);
        assert_eq!(h.gv.landing_for_features(p, (0.0, 5000.0).into(), output), None);
    }
}
