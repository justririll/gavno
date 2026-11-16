//! Decorations are outside client geometry, so CSD windows keep their own titlebars.
use crate::{
    module::Feature,
    topbar::geometry::{rect, Rect},
    window_mode::{mode_state, WindowMode},
    Gavno,
};
use smithay::{
    desktop::Window,
    input::pointer::{ButtonEvent, Focus, GrabStartData},
    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    utils::{Logical, Point, Serial},
};

pub const HEIGHT: i32 = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Title,
    Minimize,
    Maximize,
    Close,
}
pub fn ssd(window: &Window) -> bool {
    window
        .toplevel()
        .is_some_and(|t| t.with_pending_state(|s| s.decoration_mode) == Some(Mode::ServerSide))
}
pub fn title_rect(content: Rect) -> Rect {
    rect(
        content.loc.x,
        content.loc.y - HEIGHT,
        content.size.w,
        HEIGHT,
    )
}
pub fn part(r: Rect, p: Point<f64, Logical>) -> Option<Part> {
    if !r.to_f64().contains(p) {
        return None;
    }
    let right = r.loc.x + r.size.w;
    Some(if p.x >= f64::from(right - 46) {
        Part::Close
    } else if p.x >= f64::from(right - 92) {
        Part::Maximize
    } else if p.x >= f64::from(right - 138) {
        Part::Minimize
    } else {
        Part::Title
    })
}
impl Gavno {
    /// Only clients that negotiated xdg-decoration are asked to change style.
    /// GTK/CSD clients without that protocol retain their client decorations.
    pub fn sync_classic_decorations(&mut self) -> bool {
        let classic = self.features.on(Feature::Classic)
            && self.features.on(Feature::Decorations)
            && self.classic.settled == super::ShellMode::Classic
            && self.greeter.is_none();
        let desired = if classic {
            Mode::ServerSide
        } else {
            Mode::ClientSide
        };
        let mut changed = false;
        for w in self
            .space
            .elements()
            .chain(self.unmapped.iter())
            .chain(self.workspaces.all().map(|v| &v.window))
        {
            let Some(t) = w.toplevel() else { continue };
            let update = t.with_pending_state(|state| {
                if state.decoration_mode.is_some_and(|mode| mode != desired) {
                    state.decoration_mode = Some(desired);
                    true
                } else {
                    false
                }
            });
            if update {
                changed = true;
                // Fixed windows send decoration and size together in the refit.
                if mode_state(w).mode == WindowMode::Floating && t.is_initial_configure_sent() {
                    t.send_pending_configure();
                }
            }
        }
        changed
    }
    pub fn classic_client_rect(&self, window: &Window, mut slot: Rect) -> Rect {
        if self.features.on(Feature::Classic)
            && self.classic.settled == super::ShellMode::Classic
            && self.features.on(Feature::Decorations)
            && ssd(window)
        {
            let height = HEIGHT.min(slot.size.h.saturating_sub(1));
            slot.loc.y += height;
            slot.size.h -= height;
        }
        slot
    }

    pub fn classic_decor_hit(
        &self,
        p: Point<f64, Logical>,
    ) -> Option<(Window, Part, Point<i32, Logical>)> {
        if self.classic.settled != super::ShellMode::Classic
            || !self.features.on(Feature::Decorations)
            || self.desktop_transform_at(p)
        {
            return None;
        }
        for w in self.space.elements().rev() {
            let Some(loc) = self.space.element_location(w) else {
                continue;
            };
            let content = smithay::utils::Rectangle::new(loc, w.geometry().size);
            if ssd(w) && mode_state(w).mode != WindowMode::Fullscreen {
                if let Some(hit) = part(title_rect(content), p) {
                    return Some((w.clone(), hit, loc));
                }
            }
            if content.to_f64().contains(p) {
                return None;
            }
        }
        None
    }
    pub fn classic_decor_button(
        &mut self,
        button: u32,
        pressed: bool,
        serial: Serial,
        time: u32,
    ) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        if pointer.is_grabbed() {
            return false;
        }
        let p = pointer.current_location();
        let Some((w, part, loc)) = self.classic_decor_hit(p) else {
            return false;
        };
        if !pressed || button != 0x110 {
            return true;
        }
        self.guard(Feature::Classic, (), |gv| {
            gv.focus_window(Some(&w));
            match part {
                Part::Minimize => gv.minimize_window(&w),
                Part::Maximize => gv.toggle_window_mode(WindowMode::Maximized),
                Part::Close => {
                    if let Some(t) = w.toplevel() {
                        t.send_close();
                    }
                }
                Part::Title => {
                    let now = gv.start_time.elapsed();
                    let double = gv
                        .classic
                        .title_click
                        .as_ref()
                        .is_some_and(|(last, t, point)| {
                            last == &w
                                && now.saturating_sub(*t).as_millis() < 400
                                && (p.x - point.x).hypot(p.y - point.y) < 6.0
                        });
                    gv.classic.title_click = Some((w.clone(), now, p));
                    if double {
                        gv.classic.title_click = None;
                        gv.toggle_window_mode(WindowMode::Maximized);
                    } else {
                        let start = GrabStartData {
                            focus: None,
                            button,
                            location: p,
                        };
                        let grab = crate::grabs::MoveSurfaceGrab::start(start, w, loc);
                        pointer.set_grab(gv, grab, serial, Focus::Clear);
                        gv.pointer_buttons.push(button);
                        pointer.button(
                            gv,
                            &ButtonEvent {
                                button,
                                state: smithay::backend::input::ButtonState::Pressed,
                                serial,
                                time,
                            },
                        );
                        pointer.frame(gv);
                        return;
                    }
                }
            }
            gv.suppressed_buttons.push(button);
            gv.queue_redraw_all();
        });
        true
    }
}
