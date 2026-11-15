//! Per-window floating / snapped / maximized / fullscreen bookkeeping.
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

use crate::{snap::Zone, Gavno};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowMode {
    #[default]
    Floating,
    Maximized,
    Snapped(Zone),
    Fullscreen,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModeState {
    pub mode: WindowMode,
    /// Minimized windows retain their mode, geometry, and workspace ownership.
    pub minimized: bool,
    /// Explicit floating override on a tiling workspace.
    pub tile_floating: bool,
    /// Original floating geometry, independent of maximize/fullscreen restores.
    pub tile_restore: Option<Rectangle<i32, Logical>>,
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
        .map(|m| *m.lock().unwrap_or_else(|e| e.into_inner()))
        .unwrap_or_default()
}

pub fn with_mode_state<T>(window: &Window, f: impl FnOnce(&mut ModeState) -> T) -> T {
    window
        .user_data()
        .insert_if_missing_threadsafe(|| Mutex::new(ModeState::default()));
    let mut guard = window
        .user_data()
        .get::<Mutex<ModeState>>()
        .unwrap()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

impl Gavno {
    /// Greatest output overlap; unmapped windows use the active output.
    pub(crate) fn output_for_window(&self, window: &Window) -> Option<Output> {
        if self.space.element_location(window).is_none() {
            if let Some((output, _)) = self.workspaces.membership(window) { return Some(output); }
        }
        let rect = self.window_rect(window);
        self.space
            .outputs()
            .filter_map(|o| {
                let intersection = self.space.output_geometry(o)?.intersection(rect?)?;
                Some((
                    o.clone(),
                    i64::from(intersection.size.w) * i64::from(intersection.size.h),
                ))
            })
            .max_by_key(|(_, area)| *area)
            .map(|(o, _)| o)
            .or_else(|| {
                // Mapped windows can be moved under a pointer grab: avoid
                // locking the pointer again if they are temporarily offscreen.
                if rect.is_none() {
                    self.active_output()
                } else {
                    self.space.outputs().next().cloned()
                }
            })
    }

    /// Moves `window` into `mode` on `output` (default: where it is now) and
    /// sends the matching configure. Snap/maximize use the gapped work area,
    /// fullscreen the whole output, floating restores the remembered geometry.
    pub fn set_window_mode(&mut self, window: &Window, mode: WindowMode, output: Option<Output>) {
        if matches!(mode, WindowMode::Snapped(_)) && self.tiling_workspace(window) { return; }
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        crate::grabs::resize_grab::clear_anchor(window);
        let mapped = self.space.element_location(window).is_some();
        let output = output.or_else(|| self.output_for_window(window));

        let target: Option<Rectangle<i32, Logical>> = match mode {
            WindowMode::Floating => None,
            WindowMode::Maximized => output
                .as_ref()
                .map(|o| Zone::Maximized.rect(self.work_area(o), self.config.gap)),
            WindowMode::Snapped(zone) => output
                .as_ref()
                .map(|o| zone.rect(self.work_area(o), self.config.gap)),
            WindowMode::Fullscreen => output.as_ref().and_then(|o| self.space.output_geometry(o)),
        };

        let current = self.space.element_location(window).or_else(|| self.workspaces.location(window)).map(|loc| {
            Rectangle::new(
                loc,
                toplevel
                    .with_pending_state(|s| s.size)
                    .unwrap_or(window.geometry().size),
            )
        });
        let restore = with_mode_state(window, |s| match mode {
            WindowMode::Floating => s.leave(),
            _ => {
                s.enter(mode, current);
                None
            }
        });

        let geometry = if mode == WindowMode::Floating {
            restore
        } else {
            target
        };
        configure_mode(window, mode, geometry.map(|r| r.size));
        if !mapped {
            if let Some(rect) = geometry { self.save_workspace_location(window, rect.loc); }
        }

        if mapped {
            let loc = match mode {
                WindowMode::Floating => restore.map(|r| r.loc),
                _ => target.map(|t| t.loc),
            };
            if let Some(loc) = loc {
                self.space.map_element(window.clone(), loc, false);
            }
            if let Some(from) = current {
                crate::anim::animate_geometry(self, window, from);
            }
            toplevel.send_pending_configure();
        } else if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        self.reflow_tiling(true);
        self.queue_redraw_all();
    }

    /// Assigns a slot without modifying the saved floating geometry.
    pub(crate) fn configure_slot(
        &mut self,
        window: &Window,
        rect: Rectangle<i32, Logical>,
        mode: WindowMode,
    ) {
        crate::grabs::resize_grab::clear_anchor(window);
        self.window_animations.cancel(window);
        configure_mode(window, mode, Some(rect.size));
        self.space.map_element(window.clone(), rect.loc, false);
        if let Some(t) = window.toplevel() {
            t.send_pending_configure();
        }
    }

    /// Toggles `mode` for the focused window.
    pub fn toggle_window_mode(&mut self, mode: WindowMode) {
        let Some(window) = self.focused_window() else {
            return;
        };
        let next = if mode_state(&window).mode == mode {
            WindowMode::Floating
        } else {
            mode
        };
        self.set_window_mode(&window, next, None);
    }

    /// Re-applies the size of maximized/fullscreen windows after the work
    /// area or an output changed (a panel appeared, the output was resized).
    pub fn refit_fixed_windows(&mut self) {
        let mut windows: Vec<_> = self.space.elements().cloned().collect();
        for owned in self.workspaces.all() {
            if !windows.contains(&owned.window) {
                windows.push(owned.window.clone());
            }
        }
        let fixed = windows.into_iter().filter_map(|window| {
            let mode = mode_state(&window).mode;
            (mode != WindowMode::Floating).then_some((window, mode))
        });
        for (window, mode) in fixed {
            self.set_window_mode(&window, mode, None);
        }
        self.reflow_tiling(true);
    }
}

fn configure_mode(
    window: &Window,
    mode: WindowMode,
    size: Option<smithay::utils::Size<i32, Logical>>,
) {
    let Some(toplevel) = window.toplevel() else {
        return;
    };
    toplevel.with_pending_state(|state| {
        for flag in [
            xdg_toplevel::State::Maximized,
            xdg_toplevel::State::Fullscreen,
            xdg_toplevel::State::TiledLeft,
            xdg_toplevel::State::TiledRight,
            xdg_toplevel::State::TiledTop,
            xdg_toplevel::State::TiledBottom,
        ] {
            state.states.unset(flag);
        }
        state.fullscreen_output = None;
        match mode {
            WindowMode::Maximized | WindowMode::Snapped(Zone::Maximized) => {
                state.states.set(xdg_toplevel::State::Maximized);
            }
            WindowMode::Fullscreen => {
                state.states.set(xdg_toplevel::State::Fullscreen);
            }
            WindowMode::Snapped(_) => {
                for flag in [
                    xdg_toplevel::State::TiledLeft,
                    xdg_toplevel::State::TiledRight,
                    xdg_toplevel::State::TiledTop,
                    xdg_toplevel::State::TiledBottom,
                ] {
                    state.states.set(flag);
                }
            }
            WindowMode::Floating => {}
        }
        state.size = size;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn hidden_and_minimized_fixed_windows_refit_without_becoming_visible() {
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new("test".into(), smithay::output::PhysicalProperties {
            size: (0, 0).into(), subpixel: smithay::output::Subpixel::Unknown,
            make: "test".into(), model: "test".into(),
        });
        output.change_current_state(Some(smithay::output::Mode {
            size: (1600, 900).into(), refresh: 60000,
        }), None, None, None);
        h.gv.space.map_output(&output, (0, 0));
        let mut windows = Vec::new();
        for mode in [WindowMode::Maximized, WindowMode::Fullscreen, WindowMode::Snapped(Zone::Left)] {
            let window = h.window();
            h.gv.map_new_window(window.clone());
            h.gv.set_window_mode(&window, mode, Some(output.clone()));
            let restore = mode_state(&window).restore;
            windows.push((window, mode, restore));
        }
        h.gv.minimize_window(&windows[1].0);
        assert!(h.gv.switch_workspace(&output, 1));
        output.change_current_state(Some(smithay::output::Mode {
            size: (1920, 1080).into(), refresh: 60000,
        }), None, None, Some((100, 50).into()));
        h.gv.space.map_output(&output, (100, 50));
        // Change shell reservations as well as output size while all windows are hidden.
        h.gv.config.bar.enabled = false;
        h.gv.config.dock.enabled = false;
        h.gv.arrange_layers_all();
        for (window, mode, restore) in &windows {
            assert!(h.gv.space.element_location(window).is_none());
            let expected = match mode {
                WindowMode::Fullscreen => h.gv.space.output_geometry(&output).unwrap(),
                WindowMode::Maximized => Zone::Maximized.rect(h.gv.work_area(&output), h.gv.config.gap),
                WindowMode::Snapped(zone) => zone.rect(h.gv.work_area(&output), h.gv.config.gap),
                _ => unreachable!(),
            };
            assert_eq!(h.gv.workspaces.location(window), Some(expected.loc));
            assert_eq!(window.toplevel().unwrap().with_pending_state(|s| s.size), Some(expected.size));
            assert_eq!(mode_state(window).restore, *restore);
        }
        assert!(h.gv.switch_workspace(&output, 0));
        assert!(h.gv.space.element_location(&windows[0].0).is_some());
        assert!(h.gv.space.element_location(&windows[1].0).is_none());
        h.gv.focus_window(Some(&windows[1].0));
        assert_eq!(h.gv.space.element_location(&windows[1].0), Some((100, 50).into()));
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
    fn snapped_modes_keep_floating_restore_through_maximize() {
        let mut s = ModeState::default();
        let original = rect(30, 40, 640, 480);
        s.enter(WindowMode::Snapped(Zone::Left), Some(original));
        s.enter(
            WindowMode::Snapped(Zone::BottomLeft),
            Some(rect(0, 0, 800, 450)),
        );
        s.enter(WindowMode::Maximized, Some(rect(0, 0, 800, 900)));
        assert_eq!(s.leave(), Some(original));
        assert_eq!(s.leave(), None);
    }

    #[test]
    fn never_floating_has_nothing_to_restore() {
        let mut s = ModeState::default();
        s.enter(WindowMode::Fullscreen, None);
        assert_eq!(s.leave(), None);
    }
}
