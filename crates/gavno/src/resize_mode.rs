//! Generous resize targets and button-free keyboard/touchpad resize mode.
use crate::{
    grabs::{resize_grab::ResizeEdge as Edge, ResizeSurfaceGrab},
    topbar::Canvas,
    window_gesture::{self, nearest_edge},
    window_mode::{mode_state, WindowMode},
    Gavno,
};
use smithay::{
    desktop::Window,
    input::{
        keyboard::keysyms,
        pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData},
    },
    output::Output,
    utils::{IsAlive, Logical, Point, Rectangle},
};
type Rect = Rectangle<i32, Logical>;
type Pos = Point<f64, Logical>;

/// Corners are 28 px squares, centred 2 px inside the geometric corner.
/// Straight bands extend 12 px outside and 4 px into client content.
pub fn band_at(r: Rect, p: Pos) -> Option<Edge> {
    let x = p.x - r.loc.x as f64;
    let y = p.y - r.loc.y as f64;
    let w = r.size.w as f64;
    let h = r.size.h as f64;
    if x < -12.0 || y < -12.0 || x >= w + 12.0 || y >= h + 12.0 {
        return None;
    }
    let left = x < 16.0;
    let right = x >= w - 16.0;
    let top = y < 16.0;
    let bottom = y >= h - 16.0;
    if (left || right) && (top || bottom) {
        return Some(
            (if left { Edge::LEFT } else { Edge::RIGHT })
                | (if top { Edge::TOP } else { Edge::BOTTOM }),
        );
    }
    if x < 4.0 {
        Some(Edge::LEFT)
    } else if x >= w - 4.0 {
        Some(Edge::RIGHT)
    } else if y < 4.0 {
        Some(Edge::TOP)
    } else if y >= h - 4.0 {
        Some(Edge::BOTTOM)
    } else {
        None
    }
}
pub fn cycle(edge: Edge, backwards: bool) -> Edge {
    let all = [
        Edge::RIGHT,
        Edge::BOTTOM_RIGHT,
        Edge::BOTTOM,
        Edge::BOTTOM_LEFT,
        Edge::LEFT,
        Edge::TOP_LEFT,
        Edge::TOP,
        Edge::TOP_RIGHT,
    ];
    let i = all.iter().position(|e| *e == edge).unwrap_or(0);
    all[(i + if backwards { 7 } else { 1 }) % 8]
}
pub fn step(sym: u32, shift: bool) -> Option<Pos> {
    let n = if shift { 50.0 } else { 10.0 };
    Some(
        match sym {
            keysyms::KEY_Left => (-n, 0.0),
            keysyms::KEY_Right => (n, 0.0),
            keysyms::KEY_Up => (0.0, -n),
            keysyms::KEY_Down => (0.0, n),
            _ => return None,
        }
        .into(),
    )
}
pub fn cursor(edge: Edge) -> CursorIcon {
    if edge == Edge::TOP_LEFT || edge == Edge::BOTTOM_RIGHT {
        CursorIcon::NwseResize
    } else if edge == Edge::TOP_RIGHT || edge == Edge::BOTTOM_LEFT {
        CursorIcon::NeswResize
    } else if edge.intersects(Edge::LEFT | Edge::RIGHT) {
        CursorIcon::EwResize
    } else {
        CursorIcon::NsResize
    }
}
fn label(edge: Edge) -> &'static str {
    match edge {
        Edge::LEFT => "Left",
        Edge::RIGHT => "Right",
        Edge::TOP => "Top",
        Edge::BOTTOM => "Bottom",
        Edge::TOP_LEFT => "Top left",
        Edge::TOP_RIGHT => "Top right",
        Edge::BOTTOM_LEFT => "Bottom left",
        _ => "Bottom right",
    }
}
struct Session {
    window: Window,
    output: Output,
    edge: Edge,
    pointer: Pos,
    residual: Pos,
    rect: Rect,
}
#[derive(Default)]
pub struct ResizeMode {
    pub(crate) band_hover: bool,
    session: Option<Session>,
    canvas: Canvas,
}
impl ResizeMode {
    pub fn active(&self) -> bool {
        self.session.is_some()
    }
    pub fn elements(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        output: &Output,
        geo: Rect,
        scale: f64,
    ) -> Vec<crate::render::GavnoRenderElement> {
        let Some(s) = &self.session else {
            return Vec::new();
        };
        if &s.output != output || !s.window.alive() {
            return Vec::new();
        }
        let width = (geo.size.w - 24).clamp(1, 300);
        let height = 76;
        let x = (geo.size.w - width) / 2;
        let y = (geo.size.h - height) / 2;
        self.canvas.begin(renderer, scale);
        let panel = Rectangle::new((x, y).into(), (width, height).into());
        self.canvas.shadow(panel, 16.0);
        self.canvas
            .glass(panel, 16.0, crate::ui::glass::tone::THICK, crate::topbar::GLASS, 1.0, crate::topbar::LINE);
        self.canvas.label(
            renderer,
            &format!("{} × {}", s.rect.size.w, s.rect.size.h),
            x + 22,
            y + 14,
            width - 44,
            20.0,
        );
        self.canvas.label(
            renderer,
            &format!("{} · Tab changes edge", label(s.edge)),
            x + 22,
            y + 43,
            width - 44,
            12.0,
        );
        self.canvas.finish()
    }
}
impl Gavno {
    pub fn resize_band_at(&self, p: Pos) -> Option<(Window, Edge, Rect)> {
        self.features.read(crate::module::Feature::ResizeMode, Default::default(), || self.resize_band_at_feature(p))
    }

    fn resize_band_at_feature(&self, p: Pos) -> Option<(Window, Edge, Rect)> {
        use smithay::wayland::shell::wlr_layer::Layer;
        if self.greeter.is_some()
            || self.protocols.locks.locked
            || self.shell_contains(p)
            || self.desktop_transform_at(p)
            || self.layer_hit(p, &[Layer::Overlay, Layer::Top]).is_some()
        {
            return None;
        }
        // Frontmost content blocks every lower window's outside band.
        for w in self.space.elements().rev() {
            let Some(r) = self.interactive_rect(w) else {
                continue;
            };
            if mode_state(w).mode != WindowMode::Fullscreen && !self.tiled(w) {
                if let Some(edge) = band_at(r, p) {
                    return Some((w.clone(), edge, r));
                }
            }
            if r.to_f64().contains(p) {
                return None;
            }
        }
        None
    }
    pub fn refresh_resize_cursor(&mut self) {
        self.guard(crate::module::Feature::ResizeMode, Default::default(), |gv| gv.refresh_resize_cursor_feature())
    }

    fn refresh_resize_cursor_feature(&mut self) {
        if self.pointer_hidden { return; }
        if self.greeter.is_some() || self.protocols.locks.locked {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if pointer.is_grabbed() {
            return;
        }
        let edge = self
            .overlay
            .resize_mode
            .session
            .as_ref()
            .map(|s| s.edge)
            .or_else(|| {
                self.resize_band_at(pointer.current_location())
                    .map(|(_, e, _)| e)
            });
        if let Some(edge) = edge {
            self.set_cursor_status(CursorImageStatus::Named(cursor(edge)));
            self.overlay.resize_mode.band_hover = true;
        } else if self.overlay.resize_mode.band_hover {
            self.overlay.resize_mode.band_hover = false;
            if self.workspaces.splitter_hover.is_none() {
                self.set_cursor_status(CursorImageStatus::default_named());
                self.queue_redraw_all();
            }
        }
    }
    pub fn start_band_resize(
        &mut self,
        button: u32,
        serial: smithay::utils::Serial,
        time: u32,
    ) -> bool {
        self.guard(crate::module::Feature::ResizeMode, Default::default(), |gv| gv.start_band_resize_feature(button, serial, time))
    }

    fn start_band_resize_feature(
        &mut self,
        button: u32,
        serial: smithay::utils::Serial,
        time: u32,
    ) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        if pointer.is_grabbed() {
            return false;
        }
        let Some((w, edge, r)) = self.resize_band_at(pointer.current_location()) else {
            return false;
        };
        self.prepare_interactive_resize(&w);
        self.focus_window(Some(&w));
        let start = GrabStartData {
            focus: None,
            button,
            location: pointer.current_location(),
        };
        let grab = ResizeSurfaceGrab::start(start, w, edge, r);
        pointer.set_grab(self, grab, serial, Focus::Clear);
        self.pointer_buttons.push(button);
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                button,
                state: smithay::backend::input::ButtonState::Pressed,
                serial,
                time,
            },
        );
        pointer.frame(self);
        true
    }
    pub fn enter_resize_mode(&mut self) {
        self.guard(crate::module::Feature::ResizeMode, Default::default(), |gv| gv.enter_resize_mode_feature())
    }

    fn enter_resize_mode_feature(&mut self) {
        self.exit_resize_mode();
        let Some(w) = self.focused_window() else {
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
        self.prepare_interactive_resize(&w);
        let Some(rect) = self.interactive_rect(&w) else {
            return;
        };
        let pointer = self.pointer_location();
        let edge = band_at(rect, pointer).unwrap_or_else(|| nearest_edge(rect, pointer));
        self.overlay.resize_mode.session = Some(Session {
            window: w,
            output,
            edge,
            pointer,
            residual: (0.0, 0.0).into(),
            rect,
        });
        self.clear_desktop_pointer();
        self.refresh_resize_cursor();
        self.sync_bar_hints();
        self.queue_redraw_all();
    }
    pub fn exit_resize_mode(&mut self) -> bool {
        self.guard(crate::module::Feature::ResizeMode, Default::default(), |gv| gv.exit_resize_mode_feature())
    }

    fn exit_resize_mode_feature(&mut self) -> bool {
        let Some(s) = self.overlay.resize_mode.session.take() else {
            return false;
        };
        self.finish_interactive_resize(&s.window);
        self.refresh_resize_cursor();
        self.sync_bar_hints();
        self.queue_redraw_all();
        true
    }
    #[cfg(test)]
    pub fn resize_mode_key(&mut self, sym: u32, shift: bool) -> bool {
        self.guard(crate::module::Feature::ResizeMode, Default::default(), |gv| gv.resize_mode_key_feature(sym, shift, false))
    }

    pub fn resize_mode_key_with_ctrl(&mut self, sym: u32, shift: bool, ctrl: bool) -> bool {
        self.guard(crate::module::Feature::ResizeMode, false, |gv| gv.resize_mode_key_feature(sym, shift, ctrl))
    }

    fn resize_mode_key_feature(&mut self, sym: u32, shift: bool, ctrl: bool) -> bool {
        if !(self.features.on(crate::module::Feature::ResizeMode) && self.overlay.resize_mode.active()) {
            return false;
        }
        let sym = if self.keyboard_first() { crate::input::vim_arrow(sym).unwrap_or(sym) } else { sym };
        if matches!(
            sym,
            keysyms::KEY_Escape | keysyms::KEY_Return | keysyms::KEY_KP_Enter
        ) {
            self.exit_resize_mode();
            return true;
        }
        if let Some(mut s) = self.overlay.resize_mode.session.take() {
            if matches!(sym, keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab) {
                s.edge = cycle(s.edge, shift);
                s.residual = (0.0, 0.0).into();
                self.queue_redraw_all();
            }
            if let Some(d) = step(sym, shift) {
                if ctrl {
                    if !self.tiled(&s.window) && mode_state(&s.window).mode == WindowMode::Floating {
                        self.prepare_interactive(&s.window);
                        if let Some(mut rect) = self.interactive_rect(&s.window) {
                            let area = self.work_area(&s.output);
                            rect.loc.x = (rect.loc.x + d.x as i32).clamp(area.loc.x, area.loc.x + (area.size.w - rect.size.w).max(0));
                            rect.loc.y = (rect.loc.y + d.y as i32).clamp(area.loc.y, area.loc.y + (area.size.h - rect.size.h).max(0));
                            self.space.map_element(s.window.clone(), rect.loc, false);
                            self.save_workspace_location(&s.window, rect.loc);
                            self.queue_redraw_all();
                        }
                    }
                } else { self.resize_edges_by(&s.window, s.edge, d); }
            }
            s.rect = self.interactive_rect(&s.window).unwrap_or(s.rect);
            self.overlay.resize_mode.session = Some(s);
            self.refresh_resize_cursor();
        }
        true
    }
    pub fn resize_mode_motion(&mut self, pos: Pos) -> bool {
        self.guard(crate::module::Feature::ResizeMode, Default::default(), |gv| gv.resize_mode_motion_feature(pos))
    }

    fn resize_mode_motion_feature(&mut self, pos: Pos) -> bool {
        let Some(mut s) = self.overlay.resize_mode.session.take() else {
            return false;
        };
        if !s.window.alive()
            || self.space.element_location(&s.window).is_none()
            || self.focused_window().as_ref() != Some(&s.window)
        {
            self.finish_interactive_resize(&s.window);
            self.queue_redraw_all();
            return false;
        }
        let delta = pos - s.pointer + s.residual;
        // Test the predicted rectangle: dragging its active boundary must not exit
        // merely because that boundary has not received a client buffer yet.
        let predicted = window_gesture::resized(s.rect, s.edge, delta);
        let bounds = Rectangle::new(
            (
                predicted.loc.x.saturating_sub(12),
                predicted.loc.y.saturating_sub(12),
            )
                .into(),
            (
                predicted.size.w.saturating_add(24),
                predicted.size.h.saturating_add(24),
            )
                .into(),
        );
        if !bounds.to_f64().contains(pos) {
            self.finish_interactive_resize(&s.window);
            self.queue_redraw_all();
            return false;
        }
        let applied: Pos = (delta.x.round(), delta.y.round()).into();
        self.resize_edges_by(&s.window, s.edge, applied);
        s.residual = delta - applied;
        s.pointer = pos;
        s.rect = self.interactive_rect(&s.window).unwrap_or(s.rect);
        self.overlay.resize_mode.session = Some(s);
        true
    }
    pub fn validate_resize_mode(&mut self) {
        self.guard(crate::module::Feature::ResizeMode, Default::default(), |gv| gv.validate_resize_mode_feature())
    }

    fn validate_resize_mode_feature(&mut self) {
        if self.overlay.resize_mode.session.as_ref().is_some_and(|s| {
            !s.window.alive()
                || self.space.element_location(&s.window).is_none()
                || self.focused_window().as_ref() != Some(&s.window)
                || self.protocols.locks.locked
                || (self.features.on(crate::module::Feature::Overview) && self.features.read(crate::module::Feature::Overview, false, || self.overview.visible()))
        }) {
            self.exit_resize_mode();
        }
    }
    pub fn pressure_motion(&mut self, current: Pos, proposed: Pos, clamped: Pos) {
        self.guard(crate::module::Feature::WindowGestures, Default::default(), |gv| gv.pressure_motion_feature(current, proposed, clamped))
    }

    fn pressure_motion_feature(&mut self, current: Pos, proposed: Pos, clamped: Pos) {
        if self.greeter.is_some() || self.protocols.locks.locked || self.window_gesture_locked() {
            return;
        }
        let geo = self.space.outputs().find_map(|o| {
            self.space
                .output_geometry(o)
                .filter(|r| r.to_f64().contains(current))
                .map(|r| (o.clone(), r))
        });
        if let Some((output, geo)) = geo {
            let bottom = (geo.loc.y + geo.size.h - 1) as f64;
            if current.y >= bottom && proposed.y > clamped.y {
                self.push_dock_edge(&output);
            }
            let rejected = proposed - clamped;
            let pressure_pos = if rejected != (0.0, 0.0).into() {
                current + rejected
            } else {
                (
                    current.x + (proposed.x - current.x).max(0.0),
                    current.y + (proposed.y - current.y).max(0.0),
                )
                    .into()
            };
            if self
                .window_gestures
                .pressure
                .push(current, pressure_pos, geo)
                && !self.features.read(crate::module::Feature::Overview, false, || self.overview.open())
            {
                self.toggle_overview();
            }
        } else {
            self.window_gestures.pressure = Default::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect() -> Rect {
        Rectangle::new((100, 200).into(), (800, 600).into())
    }
    #[test]
    fn resize_bands_have_exact_outside_inside_and_corner_targets() {
        let r = rect();
        assert_eq!(band_at(r, (88.0, 500.0).into()), Some(Edge::LEFT));
        assert_eq!(band_at(r, (87.99, 500.0).into()), None);
        assert_eq!(band_at(r, (103.99, 500.0).into()), Some(Edge::LEFT));
        assert_eq!(band_at(r, (104.0, 500.0).into()), None);
        assert_eq!(band_at(r, (911.99, 500.0).into()), Some(Edge::RIGHT));
        assert_eq!(band_at(r, (912.0, 500.0).into()), None);
        assert_eq!(band_at(r, (500.0, 188.0).into()), Some(Edge::TOP));
        assert_eq!(band_at(r, (500.0, 203.99).into()), Some(Edge::TOP));
        assert_eq!(band_at(r, (500.0, 204.0).into()), None);
        assert_eq!(band_at(r, (115.99, 215.99).into()), Some(Edge::TOP_LEFT));
        assert_eq!(band_at(r, (116.0, 216.0).into()), None);
        assert_eq!(band_at(r, (884.0, 784.0).into()), Some(Edge::BOTTOM_RIGHT));
        assert_eq!(band_at(r, (88.0, 811.99).into()), Some(Edge::BOTTOM_LEFT));
        assert_eq!(band_at(r, (87.0, 187.0).into()), None);
    }
    #[test]
    fn cycles_all_eight_edges_and_reverses() {
        let mut e = Edge::RIGHT;
        let mut seen = Vec::new();
        for _ in 0..8 {
            assert!(!seen.contains(&e));
            seen.push(e);
            let next = cycle(e, false);
            assert_eq!(cycle(next, true), e);
            e = next;
        }
        assert_eq!(e, Edge::RIGHT);
    }
    #[test]
    fn arrow_steps_are_ten_or_fifty_and_keep_the_opposite_edge_fixed() {
        assert_eq!(step(keysyms::KEY_Left, false), Some((-10.0, 0.0).into()));
        assert_eq!(step(keysyms::KEY_Up, true), Some((0.0, -50.0).into()));
        assert_eq!(step(keysyms::KEY_Return, false), None);
        assert_eq!(
            window_gesture::resized(rect(), Edge::LEFT, step(keysyms::KEY_Left, true).unwrap()),
            Rectangle::new((50, 200).into(), (850, 600).into())
        );
        assert_eq!(cursor(Edge::BOTTOM_RIGHT), CursorIcon::NwseResize);
        assert_eq!(cursor(Edge::RIGHT), CursorIcon::EwResize);
    }
}

impl Gavno {
    pub(crate) fn reset_resize_mode(&mut self) {
        if let Some(session) = self.overlay.resize_mode.session.take() {
            self.finish_interactive_resize(&session.window);
        }
        self.overlay.resize_mode = Default::default();
        self.set_cursor_status(smithay::input::pointer::CursorImageStatus::default_named());
    }
}
