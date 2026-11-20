//! TTY mode: a terminals-only look on the GAVNO shell, layered like code mode
//! (a session flag, not a shell mode). A permanent status line on top shows the
//! workspaces as tabs, the focused title and the system status; below it every
//! workspace tiles its windows like tmux panes with 1 px dividers. There is no
//! mouse, dock, decoration or wallpaper. Classic never enters it.
//!
//! Nothing here costs anything while the screen is settled: the only timers
//! are the minute-aligned clock tick, a one-shot that ends the power-off
//! animation, and the springs and fades while they move.

use std::time::Duration;

use smithay::{
    output::Output,
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
};

use crate::Gavno;

pub(crate) mod draw;

/// CRT power-on and power-off, in total.
pub const CRT_TIME: Duration = Duration::from_millis(200);
/// The dip through black after a console switch.
pub const FADE_TIME: Duration = Duration::from_millis(120);
/// Height of the status line on top, in logical pixels.
pub const BAR_HEIGHT: i32 = 28;
/// A terminal is never started twice for one console within this time.
const SPAWN_GUARD: Duration = Duration::from_secs(2);

/// Opacity of the black cover after a console switch: opaque at once, gone
/// after `FADE_TIME`. The content switches immediately, so the dip adds no
/// latency.
pub(crate) fn fade_cover(start: Duration, now: Duration) -> f32 {
    let t = (now.saturating_sub(start).as_secs_f64() / FADE_TIME.as_secs_f64()).clamp(0.0, 1.0);
    (1.0 - t).powi(2) as f32
}

/// CRT shape at animation position `p` (0 = dark, 1 = fully on): the height
/// of the picture as a fraction of the screen and the brightness of the line.
pub(crate) fn crt_shape(p: f64) -> (f64, f32) {
    const LINE_ONLY: f64 = 0.3;
    let p = p.clamp(0.0, 1.0);
    if p < LINE_ONLY {
        return (0.0, 1.0);
    }
    let q = (p - LINE_ONLY) / (1.0 - LINE_ONLY);
    (crate::ui::style::ease_out(q), (1.0 - q * 1.6).clamp(0.0, 1.0) as f32)
}

pub struct Tty {
    /// The mode itself; false while Classic owns the shell.
    pub(crate) on: bool,
    /// Tiling of every workspace before TTY mode forced it.
    pub(crate) saved: Vec<(Output, u64, bool)>,
    /// The power-off animation runs; the mode ends when it is over.
    pub(crate) closing: bool,
    /// Start of the running CRT animation and whether it powers on.
    crt: Option<(Duration, bool)>,
    crt_timer: Option<RegistrationToken>,
    /// Start of the console switch fade.
    fade: Option<Duration>,
    /// The clock of the status line wakes once per minute.
    clock_timer: Option<RegistrationToken>,
    last_spawn: Option<Duration>,
    /// Forces the animation decision in tests.
    #[cfg(test)]
    pub(crate) animate: Option<bool>,
    view: draw::View,
}

impl Default for Tty {
    fn default() -> Self {
        Self {
            on: false,
            saved: Vec::new(),
            closing: false,
            crt: None,
            crt_timer: None,
            fade: None,
            clock_timer: None,
            last_spawn: None,
            #[cfg(test)]
            animate: None,
            view: draw::View::default(),
        }
    }
}

impl Tty {
    /// The mode is on and not on its way out.
    pub(crate) fn active(&self) -> bool {
        self.on && !self.closing
    }
    /// The status line shows, so the status worker should run.
    pub(crate) fn status_wanted(&self) -> bool {
        self.on
    }
    pub(crate) fn animating(&self, now: Duration) -> bool {
        self.on
            && (self.crt.is_some_and(|(start, _)| now < start + CRT_TIME)
                || self.fade.is_some_and(|start| now < start + FADE_TIME)
                || self.view.tabs_active())
    }
    /// Timers currently armed, for the zero-idle-cost checks.
    #[cfg(test)]
    pub(crate) fn timers_armed(&self) -> bool {
        self.clock_timer.is_some() || self.crt_timer.is_some()
    }
    /// The SPAWN_GUARD check, recording the spawn when it is allowed.
    fn may_spawn(&mut self, now: Duration) -> bool {
        if self.last_spawn.is_some_and(|at| now.saturating_sub(at) < SPAWN_GUARD) {
            return false;
        }
        self.last_spawn = Some(now);
        true
    }
}

impl Gavno {
    /// What the user asked for: on, and not already leaving.
    pub(crate) fn tty_requested(&self) -> bool {
        self.tty.active()
    }

    /// Keyboard-first shell modes hide the pointer until it moves again.
    pub(crate) fn keyboard_first(&self) -> bool {
        self.code_mode || self.tty.on
    }

    fn tty_animates(&self) -> bool {
        #[cfg(test)]
        if let Some(animate) = self.tty.animate {
            return animate;
        }
        !matches!(self.backend, crate::backend::Backend::None)
            && self.features.on(crate::module::Feature::Animations)
            && self.space.outputs().next().is_some()
    }

    /// Turns TTY mode on or off. In Classic the request is only remembered and
    /// applied when the shell returns. Leaving plays the power-off animation
    /// first, so the mode ends 200 ms after the request.
    pub fn set_tty_mode(&mut self, enable: bool) {
        if !self.tty.on && (self.classic_mode() || self.classic.windows.in_classic()) {
            self.classic.windows.set_tty_resume(enable);
            return;
        }
        let now = self.start_time.elapsed();
        if enable {
            if self.tty.closing {
                self.cancel_tty_leave();
                self.tty.crt = Some((now, true));
                self.queue_redraw_all();
                return;
            }
            if self.tty.on {
                return;
            }
            self.apply_tty_mode(true);
            if self.tty_animates() {
                self.tty.crt = Some((now, true));
            }
            self.tty_console_entered(now);
            self.queue_redraw_all();
        } else {
            if !self.tty.on || self.tty.closing {
                return;
            }
            if !self.tty_animates() {
                self.apply_tty_mode(false);
                return;
            }
            self.tty.closing = true;
            self.tty.crt = Some((now, false));
            self.tty.crt_timer = self
                .loop_handle
                .insert_source(Timer::from_duration(CRT_TIME), |_, _, gv| {
                    let _stall = crate::perf::Stall::new("timer.tty_power_off");
                    gv.tty.crt_timer = None;
                    gv.finish_tty_leave();
                    TimeoutAction::Drop
                })
                .ok();
            if self.tty.crt_timer.is_none() {
                self.finish_tty_leave();
                return;
            }
            self.queue_redraw_all();
        }
    }

    fn cancel_tty_leave(&mut self) {
        self.tty.closing = false;
        if let Some(token) = self.tty.crt_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Ends the mode at once: the end of the power-off animation, and Classic.
    pub(crate) fn finish_tty_leave(&mut self) {
        self.cancel_tty_leave();
        self.tty.crt = None;
        self.apply_tty_mode(false);
        self.queue_redraw_all();
    }

    /// The tiling a workspace had before a forced-tiling mode touched it.
    pub(crate) fn tiling_before_forced(&self, output: &Output, id: u64, current: bool) -> bool {
        let find = |saved: &[(Output, u64, bool)]| {
            saved.iter().find(|(o, i, _)| o == output && *i == id).map(|(_, _, tiling)| *tiling)
        };
        find(&self.tty.saved).or_else(|| find(&self.code_mode_saved)).unwrap_or(current)
    }

    /// The layout and session part of the switch, without animation.
    fn apply_tty_mode(&mut self, enable: bool) {
        if self.tty.on == enable {
            return;
        }
        self.tty.on = enable;
        if enable {
            let mut saved = Vec::new();
            for owner in &self.workspaces.outputs {
                for workspace in &owner.list.items {
                    saved.push((owner.output.clone(), workspace.id, self.tiling_before_forced(&owner.output, workspace.id, workspace.tiling)));
                }
            }
            self.tty.saved = saved;
            self.hide_pointer();
            for owner in &mut self.workspaces.outputs {
                for workspace in &mut owner.list.items {
                    workspace.tiling = true;
                }
            }
        } else {
            let saved = std::mem::take(&mut self.tty.saved);
            let code = self.code_mode;
            for owner in &mut self.workspaces.outputs {
                let output = owner.output.clone();
                let fallback = owner.list.default_tiling;
                for workspace in &mut owner.list.items {
                    workspace.tiling = code
                        || saved
                            .iter()
                            .find(|(o, id, _)| *o == output && *id == workspace.id)
                            .map(|(_, _, tiling)| *tiling)
                            .unwrap_or(fallback);
                }
            }
            self.tty_reset_view();
            if !self.code_mode {
                self.show_pointer();
            }
        }
        self.dock.close_menu();
        self.sync_tty_flags();
        self.sync_bar_hints();
        self.reflow_tiling(false);
        self.refit_fixed_windows();
        self.refresh_tty();
        self.persist_mode();
        self.queue_redraw_all();
    }

    /// Drops every timer and animation of the mode.
    fn tty_reset_view(&mut self) {
        if let Some(token) = self.tty.clock_timer.take() {
            self.loop_handle.remove(token);
        }
        self.tty.fade = None;
    }

    /// Pushes the mode into the parts that restyle themselves.
    pub(crate) fn sync_tty_flags(&mut self) {
        crate::ui::text::set_mono_family(&self.config.tty.font);
        self.launcher.tty = self.tty.on;
        self.notifications.borrow_mut().set_tty(self.tty.on);
    }

    /// Keeps workspaces created during TTY mode tiling. Called from the tiling
    /// reflow, so it needs no callback of its own.
    pub(crate) fn enforce_tty_tiling(&mut self) -> bool {
        if !self.tty.on {
            return false;
        }
        let mut changed = false;
        for owner in &mut self.workspaces.outputs {
            for workspace in &mut owner.list.items {
                if !workspace.tiling {
                    workspace.tiling = true;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Gap between panes and the screen edge: the 1 px divider of a console.
    pub(crate) fn tile_gap(&self) -> i32 {
        if self.tty.on { 1 } else { self.config.gap }
    }

    /// Classic took the shell over: leave TTY mode without animation and
    /// remember it for the return.
    pub(crate) fn tty_suspend_for_classic(&mut self) -> Option<Vec<(Output, u64, bool)>> {
        if !self.tty.on {
            return None;
        }
        self.cancel_tty_leave();
        self.tty.crt = None;
        self.tty.on = false;
        self.tty_reset_view();
        self.sync_tty_flags();
        Some(std::mem::take(&mut self.tty.saved))
    }

    /// Alt+F<n>: console `n` (1-based). A console that does not exist yet
    /// means the last one, the trailing empty console.
    pub fn tty_switch_console(&mut self, number: usize) {
        if !self.tty.active() {
            return;
        }
        let Some(output) = self.active_output() else { return };
        let count = self.workspaces.ensure(&output).list.items.len();
        let index = number.saturating_sub(1).min(count.saturating_sub(1));
        self.switch_workspace(&output, index);
    }

    /// A console was switched to (also by Mod+n and the like): the dip
    /// through black and the terminal of an empty console.
    pub(crate) fn tty_console_switched(&mut self) {
        if !self.tty.on {
            return;
        }
        let now = self.start_time.elapsed();
        self.tty_console_entered(now);
        if self.tty_animates() {
            self.tty.fade = Some(now);
        }
        self.queue_redraw_all();
    }

    fn tty_console_entered(&mut self, now: Duration) {
        self.tty_autostart(now);
    }

    /// Starts the configured terminal on an empty console. It happens only on
    /// a switch into the console, never from a refresh, so a terminal that
    /// fails or is closed again is not started in a loop.
    fn tty_autostart(&mut self, now: Duration) {
        if !self.config.tty.autostart_terminal || self.protocols.locks.locked || self.greeter.is_some() {
            return;
        }
        if !self.active_console_empty() || !self.tty.may_spawn(now) {
            return;
        }
        let command = self.tty_terminal();
        self.launch_shell(&command, None);
    }

    /// The terminal a console starts: `footclient` against a running foot
    /// server (instant, native Wayland, no client-side decorations), else the
    /// configured `terminal`.
    pub(crate) fn tty_terminal(&self) -> String {
        let socket = std::env::var_os("XDG_RUNTIME_DIR").zip(std::env::var_os("WAYLAND_DISPLAY")).map(|(dir, display)| {
            std::path::Path::new(&dir).join(format!("foot-{}.sock", display.to_string_lossy()))
        });
        foot_client_command(socket.as_deref().is_some_and(|p| p.exists()), &self.config.terminal)
    }

    /// The active workspace of the active output shows no window.
    pub(crate) fn active_console_empty(&self) -> bool {
        self.active_output()
            .and_then(|output| self.workspaces.get(&output))
            .is_none_or(|owner| draw::console_empty(&owner.list.items[owner.list.active]))
    }

    /// Per refresh pass: the clock tick and the pointer focus, which stays on
    /// the focused window so that scrolling reaches it without a mouse.
    pub(crate) fn refresh_tty(&mut self) {
        let wanted = self.tty.on && !self.tty.closing && !self.power.is_off();
        match (wanted, self.tty.clock_timer.is_some()) {
            (true, false) => {
                let second = crate::ui::theme::LocalTime::now().second as u64;
                let wait = Duration::from_secs(60 - second.min(59)) + Duration::from_millis(20);
                self.tty.clock_timer = self
                    .loop_handle
                    .insert_source(Timer::from_duration(wait), |_, _, gv| {
                        let _stall = crate::perf::Stall::new("timer.tty_clock");
                        gv.tty.clock_timer = None;
                        gv.queue_redraw_all();
                        TimeoutAction::Drop
                    })
                    .ok();
            }
            (false, true) => {
                if let Some(token) = self.tty.clock_timer.take() {
                    self.loop_handle.remove(token);
                }
            }
            _ => {}
        }
        self.tty_sync_pointer_focus();
    }

    /// Keeps the invisible pointer on the focused window: with no mouse events
    /// delivered, scroll events still reach the window you type into.
    fn tty_sync_pointer_focus(&mut self) {
        if !self.tty.active() || self.protocols.locks.locked || self.greeter.is_some() || self.lock.internal() {
            return;
        }
        let Some(window) = self.focused_window() else { return };
        let Some(toplevel) = window.toplevel() else { return };
        let surface = toplevel.wl_surface().clone();
        let Some(pointer) = self.seat.get_pointer() else { return };
        if pointer.current_focus().as_ref() == Some(&surface) {
            return;
        }
        let Some(location) = self.space.element_location(&window) else { return };
        let geometry = window.geometry();
        let origin = (location - geometry.loc).to_f64();
        let centre = location.to_f64() + smithay::utils::Point::from((geometry.size.w as f64 / 2.0, geometry.size.h as f64 / 2.0));
        pointer.motion(
            self,
            Some((surface, origin)),
            &smithay::input::pointer::MotionEvent {
                location: centre,
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
    }

    /// Gesture-driven shell transitions also end TTY mode.
    pub(crate) fn sync_tty(&mut self) {
        if self.tty.on && self.classic.settled == crate::classic::ShellMode::Classic {
            self.finish_tty_leave();
            self.classic.windows.set_tty_resume(false);
        }
    }

    /// Whether anything of TTY mode still needs frames.
    pub(crate) fn tty_animating(&self) -> bool {
        self.tty.animating(self.start_time.elapsed())
    }
}

/// The console terminal: footclient when a foot server answers, else `fallback`.
pub(crate) fn foot_client_command(server: bool, fallback: &str) -> String {
    if server {
        "footclient -o csd.preferred=none -o pad=8x6".to_owned()
    } else {
        fallback.to_owned()
    }
}

#[cfg(test)]
mod tests;
