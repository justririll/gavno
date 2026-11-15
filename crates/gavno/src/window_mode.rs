//! Per-window floating / maximized / fullscreen bookkeeping.
//!
//! The state lives in the window's user data. The pure part (what to
//! remember and what to restore) is kept free of Smithay types beyond
//! `Rectangle` so it can be unit tested.

use std::sync::Mutex;

use smithay::{
    desktop::Window,
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Rectangle},
};

use crate::Gavno;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowMode {
    #[default]
    Floating,
    Maximized,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModeState {
    pub mode: WindowMode,
    /// Floating geometry to go back to. `None` while floating, or when the
    /// window was never floating (it asked for maximize before its first map).
    pub restore: Option<Rectangle<i32, Logical>>,
}

impl ModeState {
    /// Switches to `mode`. `current` is the window's present geometry; it is
    /// remembered only when leaving the floating mode, so maximized ->
    /// fullscreen -> unfullscreen still returns to the floating geometry.
    pub fn enter(&mut self, mode: WindowMode, current: Option<Rectangle<i32, Logical>>) {
        if mode == WindowMode::Floating {
            self.mode = mode;
            return;
        }
        if self.mode == WindowMode::Floating {
            self.restore = current;
        }
        self.mode = mode;
    }

    /// Back to floating; returns the geometry to restore, if known.
    pub fn leave(&mut self) -> Option<Rectangle<i32, Logical>> {
        self.mode = WindowMode::Floating;
        self.restore.take()
    }
}

/// The window's mode state (default: floating).
pub fn mode_state(window: &Window) -> ModeState {
    window
        .user_data()
        .get::<Mutex<ModeState>>()
        .map(|m| *m.lock().unwrap())
        .unwrap_or_default()
}

pub fn with_mode_state<T>(window: &Window, f: impl FnOnce(&mut ModeState) -> T) -> T {
    window.user_data().insert_if_missing_threadsafe(|| Mutex::new(ModeState::default()));
    let mut guard = window.user_data().get::<Mutex<ModeState>>().unwrap().lock().unwrap();
    f(&mut guard)
}

impl Gavno {
    /// The output a window is on, else the active one.
    fn output_for_window(&self, window: &Window) -> Option<Output> {
        self.space
            .outputs_for_element(window)
            .into_iter()
            .next()
            .or_else(|| self.active_output())
    }

    /// Moves `window` into `mode` on `output` (default: where it is now) and
    /// sends the matching configure. Maximized fills the work area,
    /// fullscreen the whole output, floating restores the remembered geometry.
    pub fn set_window_mode(&mut self, window: &Window, mode: WindowMode, output: Option<Output>) {
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        let mapped = self.space.element_location(window).is_some();
        let output = output.or_else(|| self.output_for_window(window));

        let target: Option<Rectangle<i32, Logical>> = match mode {
            WindowMode::Floating => None,
            WindowMode::Maximized => output.as_ref().map(|o| self.work_area(o)),
            WindowMode::Fullscreen => output.as_ref().and_then(|o| self.space.output_geometry(o)),
        };

        let current = self
            .space
            .element_location(window)
            .map(|loc| Rectangle::new(loc, window.geometry().size));
        let restore = with_mode_state(window, |s| match mode {
            WindowMode::Floating => s.leave(),
            _ => {
                s.enter(mode, current);
                None
            }
        });

        toplevel.with_pending_state(|state| {
            match mode {
                WindowMode::Floating => {
                    state.states.unset(xdg_toplevel::State::Maximized);
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.fullscreen_output = None;
                    // `None` lets the client pick its own size again.
                    state.size = restore.map(|r| r.size);
                }
                WindowMode::Maximized => {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.fullscreen_output = None;
                    state.size = target.map(|t| t.size);
                }
                WindowMode::Fullscreen => {
                    state.states.unset(xdg_toplevel::State::Maximized);
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.size = target.map(|t| t.size);
                }
            }
        });

        if mapped {
            let loc = match mode {
                WindowMode::Floating => restore.map(|r| r.loc),
                _ => target.map(|t| t.loc),
            };
            if let Some(loc) = loc {
                self.space.map_element(window.clone(), loc, false);
            }
            toplevel.send_pending_configure();
        } else if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        self.queue_redraw_all();
    }

    /// Toggles `mode` for the focused window.
    pub fn toggle_window_mode(&mut self, mode: WindowMode) {
        let Some(window) = self.focused_window() else {
            return;
        };
        let next = if mode_state(&window).mode == mode { WindowMode::Floating } else { mode };
        self.set_window_mode(&window, next, None);
    }

    /// Re-applies the size of maximized/fullscreen windows after the work
    /// area or an output changed (a panel appeared, the output was resized).
    pub fn refit_fixed_windows(&mut self) {
        let fixed: Vec<(Window, WindowMode)> = self
            .space
            .elements()
            .map(|w| (w.clone(), mode_state(w).mode))
            .filter(|(_, m)| *m != WindowMode::Floating)
            .collect();
        for (window, mode) in fixed {
            self.set_window_mode(&window, mode, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn maximize_then_restore() {
        let mut s = ModeState::default();
        s.enter(WindowMode::Maximized, Some(rect(10, 20, 300, 200)));
        assert_eq!(s.mode, WindowMode::Maximized);
        assert_eq!(s.leave(), Some(rect(10, 20, 300, 200)));
        assert_eq!(s.mode, WindowMode::Floating);
        assert_eq!(s.leave(), None);
    }

    #[test]
    fn maximized_to_fullscreen_keeps_original_geometry() {
        let mut s = ModeState::default();
        s.enter(WindowMode::Maximized, Some(rect(1, 2, 3, 4)));
        s.enter(WindowMode::Fullscreen, Some(rect(0, 0, 1920, 1080)));
        assert_eq!(s.mode, WindowMode::Fullscreen);
        assert_eq!(s.leave(), Some(rect(1, 2, 3, 4)));
    }

    #[test]
    fn reapplying_same_mode_keeps_restore() {
        let mut s = ModeState::default();
        s.enter(WindowMode::Maximized, Some(rect(5, 5, 50, 50)));
        s.enter(WindowMode::Maximized, Some(rect(0, 0, 1920, 1080)));
        assert_eq!(s.leave(), Some(rect(5, 5, 50, 50)));
    }

    #[test]
    fn never_floating_has_nothing_to_restore() {
        let mut s = ModeState::default();
        s.enter(WindowMode::Fullscreen, None);
        assert_eq!(s.leave(), None);
    }
}
