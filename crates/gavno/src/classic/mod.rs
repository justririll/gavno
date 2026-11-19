//! Session-wide shell selection. Windows and workspace ownership stay in the core.
pub mod binds;
pub mod cache;
pub mod decor;
pub mod desktop;
mod desktop_input;
mod desktop_io;
#[cfg(test)]
pub(crate) use desktop_io::tests as desktop_tests;
pub mod start;
pub mod taskbar;
pub mod widgets;
pub(crate) mod windows;

use crate::{
    module::Feature,
    ui::spring::{Spring, SpringParams},
    Gavno,
};
use serde::{Deserialize, Serialize};
use smithay::{
    desktop::Window,
    utils::{Logical, Rectangle},
};
use std::time::Duration;

pub const TASKBAR_HEIGHT: i32 = 48;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellMode {
    #[default]
    Gavno,
    Classic,
}
impl ShellMode {
    pub fn progress(self) -> f64 {
        if self == Self::Classic {
            1.0
        } else {
            0.0
        }
    }
    pub fn other(self) -> Self {
        if self == Self::Classic {
            Self::Gavno
        } else {
            Self::Classic
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    Center,
    #[default]
    Left,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ClassicConfig {
    pub start_in_classic: bool,
    pub open_maximized: bool,
    pub align: Align,
    /// None inherits the dock pins; an explicit empty list means no pins.
    pub pinned: Option<Vec<String>>,
    pub start_pinned: Vec<String>,
    pub settings_command: String,
    pub task_manager: String,
    pub desktop_home: bool,
    pub desktop_trash: bool,
    pub widgets: Vec<String>,
    #[serde(flatten)]
    pub(crate) unknown: toml::Table,
}
impl Default for ClassicConfig {
    fn default() -> Self {
        Self {
            start_in_classic: false,
            open_maximized: true,
            align: Align::Left,
            pinned: None,
            start_pinned: vec![
                "@terminal".into(),
                "@files".into(),
                "firefox".into(),
                "@settings".into(),
            ],
            settings_command: "gavno-settings".into(),
            task_manager: "foot -e btop".into(),
            desktop_home: true,
            desktop_trash: true,
            widgets: vec![
                "clock".into(),
                "system".into(),
                "media".into(),
                "music".into(),
            ],
            unknown: Default::default(),
        }
    }
}

pub struct Classic {
    pub(crate) windows: windows::Windows,
    /// Requested mode; work-area changes are applied only after progress settles.
    pub mode: ShellMode,
    pub settled: ShellMode,
    pub progress: Spring,
    pub held: bool,
    pub switcher_win: bool,
    pub desktop: desktop::Desktop,
    pub taskbar: taskbar::Taskbar,
    pub start: start::Start,
    shown: Vec<Window>,
    pub title_click: Option<(Window, Duration, smithay::utils::Point<f64, Logical>)>,
}
impl Default for Classic {
    fn default() -> Self {
        Self::new(false)
    }
}
impl Classic {
    pub fn new(enabled: bool) -> Self {
        let _startup = crate::perf::Startup::new("classic.new.begin", "classic.new.end");
        let mode = if enabled {
            ShellMode::Classic
        } else {
            ShellMode::Gavno
        };
        Self {
            windows: windows::Windows::new(enabled),
            mode,
            settled: mode,
            progress: Spring::new(mode.progress(), SpringParams::SMOOTH),
            held: false,
            switcher_win: false,
            desktop: Default::default(),
            taskbar: Default::default(),
            start: Default::default(),
            shown: Vec::new(),
            title_click: None,
        }
    }
    pub fn active(&self) -> bool {
        self.progress.is_active() || self.start.active()
    }
    pub fn advance(&mut self, now: Duration) {
        self.progress.advance(now);
    }
    pub fn release(&mut self, origin: ShellMode, velocity: f64, cancelled: bool, now: Duration) {
        let target = if cancelled {
            origin.progress()
        } else {
            crate::gesture::release_target(self.progress.target, velocity, 1) as f64
        };
        self.progress
            .end_follow(if cancelled { 0.0 } else { velocity }, now);
        self.mode = if target == 1.0 {
            ShellMode::Classic
        } else {
            ShellMode::Gavno
        };
        self.progress.set(target);
        self.held = false;
    }
}

pub fn reserve(
    area: Rectangle<i32, Logical>,
    mode: ShellMode,
    bar: bool,
    dock: bool,
    autohide: bool,
) -> Rectangle<i32, Logical> {
    if mode == ShellMode::Classic {
        Rectangle::new(
            area.loc,
            (area.size.w, (area.size.h - TASKBAR_HEIGHT).max(0)).into(),
        )
    } else {
        crate::dock::reserve(area, bar, dock, autohide)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalRoute {
    Overview,
    Mode,
    ShowDesktop,
    Ignore,
}
/// Sign of the vertical swipe delta (positive = fingers down) that moves
/// toward Classic. Swipe up enters Classic and down returns; with the overview
/// enabled, up keeps opening the overview, so Classic moves to swipe down.
pub fn toward_classic_sign(overview: bool) -> f64 {
    if overview {
        1.0
    } else {
        -1.0
    }
}

/// `toward_classic`: the swipe moves in the direction that enters Classic.
/// `super_held`: the mode switch needs Super held, so a plain three-finger
/// swipe never flips the whole shell by accident.
pub fn vertical_route(
    mode: ShellMode,
    toward_classic: bool,
    overview: bool,
    overview_open: bool,
    super_held: bool,
) -> VerticalRoute {
    if overview && overview_open {
        return VerticalRoute::Overview;
    }
    match (mode, toward_classic) {
        (ShellMode::Gavno, true) | (ShellMode::Classic, false) if super_held => VerticalRoute::Mode,
        // Super marks shell gestures: with it held, a swipe the wrong way (or a
        // second swipe while entering Classic) must not minimize everything.
        (ShellMode::Classic, true) if !super_held => VerticalRoute::ShowDesktop,
        (ShellMode::Gavno, false) if overview => VerticalRoute::Overview,
        _ => VerticalRoute::Ignore,
    }
}

impl Gavno {
    pub fn classic_mode(&self) -> bool {
        self.features.on(Feature::Classic) && self.classic.mode == ShellMode::Classic
    }
    pub fn shell_progress(&self) -> f64 {
        self.features.read(Feature::Classic, 0.0, || {
            self.classic.progress.value.clamp(0.0, 1.0)
        })
    }
    pub fn set_shell_mode(&mut self, mode: ShellMode) {
        self.guard(Feature::Classic, (), |gv| {
            gv.close_launcher();
            gv.close_overview();
            // Code mode is left when the transition settles, so its tiles are
            // still tiles when Classic maximizes them.
            gv.topbar.close();
            gv.dock.close_menu();
            gv.classic.start.close();
            gv.classic.desktop.cancel_interaction();
            gv.classic.taskbar.close_menu();
            gv.classic.progress.advance(gv.start_time.elapsed());
            gv.classic.progress.set(mode.progress());
            gv.classic.mode = mode;
            gv.classic.held = false;
            gv.clear_desktop_pointer();
            gv.queue_redraw_all();
        });
    }
    pub fn settle_classic(&mut self) {
        let changed = self.guard(Feature::Classic, false, |gv| {
            gv.classic.advance(gv.start_time.elapsed());
            if gv.classic.held
                || gv.classic.progress.is_active()
                || gv.classic.settled == gv.classic.mode
            {
                return false;
            }
            gv.classic.settled = gv.classic.mode;
            true
        });
        if changed {
            self.switch_classic_windows();
            self.sync_classic_decorations();
            self.refit_classic_transition();
            self.queue_redraw_all();
        }
    }
    pub fn show_classic_desktop(&mut self, restore: bool) {
        self.guard(Feature::Classic, (), |gv| {
            if restore && !gv.classic.shown.is_empty() {
                let windows = std::mem::take(&mut gv.classic.shown);
                // Restore only surviving windows from this invocation; pre-existing
                // minimized windows and windows on other workspaces stay untouched.
                for w in windows
                    .iter()
                    .rev()
                    .filter(|w| smithay::utils::IsAlive::alive(*w))
                {
                    if crate::window_mode::mode_state(w).minimized {
                        gv.restore_minimized(w);
                    }
                }
                if let Some(w) = windows
                    .first()
                    .filter(|w| smithay::utils::IsAlive::alive(*w))
                {
                    gv.focus_window(Some(w));
                }
            } else {
                let windows: Vec<_> = gv.space.elements().rev().cloned().collect();
                if !windows.is_empty() {
                    gv.classic.shown = windows.clone();
                }
                for w in windows {
                    gv.minimize_window(&w);
                }
            }
            gv.classic.start.close();
            gv.queue_redraw_all();
        });
    }
    pub(crate) fn reset_classic(&mut self) {
        self.classic.settled = ShellMode::Gavno;
        self.switch_classic_windows();
        if let Some(t) = self.classic.taskbar.hover_timer.take() {
            self.loop_handle.remove(t);
        }
        self.stop_classic_desktop();
        self.gestures.super_tap.cancel();
        let windows = std::mem::replace(&mut self.classic.windows, windows::Windows::new(false));
        self.classic = Classic::default();
        self.classic.windows = windows;
        self.topbar.shell_progress = 0.0;
        self.dock.shell_progress = 0.0;
        self.window_decor.classic = false;
        self.topbar.close();
        self.dock.close_menu();
        self.loop_handle.insert_idle(|gv| {
            gv.refit_fixed_windows();
            gv.queue_redraw_all();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_switch_needs_super() {
        for overview in [false, true] {
            assert_eq!(
                vertical_route(ShellMode::Gavno, true, overview, false, false),
                VerticalRoute::Ignore
            );
            assert_eq!(
                vertical_route(ShellMode::Classic, false, overview, false, false),
                VerticalRoute::Ignore
            );
            assert_eq!(
                vertical_route(ShellMode::Classic, true, overview, false, false),
                VerticalRoute::ShowDesktop
            );
        }
    }
    #[test]
    fn gesture_routing_table() {
        for overview in [false, true] {
            assert_eq!(
                vertical_route(ShellMode::Gavno, true, overview, false, true),
                VerticalRoute::Mode
            );
            assert_eq!(
                vertical_route(ShellMode::Gavno, false, overview, false, true),
                if overview {
                    VerticalRoute::Overview
                } else {
                    VerticalRoute::Ignore
                }
            );
            assert_eq!(
                vertical_route(ShellMode::Classic, false, overview, false, true),
                VerticalRoute::Mode
            );
            assert_eq!(
                vertical_route(ShellMode::Classic, true, overview, false, true),
                VerticalRoute::Ignore
            );
            if overview {
                for mode in [ShellMode::Classic, ShellMode::Gavno] {
                    for down in [false, true] {
                        assert_eq!(
                            vertical_route(mode, down, true, true, true),
                            VerticalRoute::Overview
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn progress_follows_and_release_projects_velocity_or_cancels() {
        for (origin, delta, velocity, cancelled, target) in [
            (ShellMode::Gavno, 0.4, 2.0, false, ShellMode::Classic),
            (ShellMode::Gavno, 0.4, 0.0, false, ShellMode::Gavno),
            (ShellMode::Classic, -0.4, -2.0, false, ShellMode::Gavno),
            (ShellMode::Classic, -0.7, -2.0, true, ShellMode::Classic),
        ] {
            let mut c = Classic::new(origin == ShellMode::Classic);
            let now = Duration::from_millis(100);
            c.progress
                .follow((origin.progress() + delta).clamp(0.0, 1.0), Duration::ZERO);
            c.progress.advance(now);
            let displayed = c.progress.value;
            c.release(origin, velocity, cancelled, now);
            assert_eq!(c.mode, target);
            assert_eq!(c.progress.value, displayed);
            for i in 1..=400 {
                c.advance(now + Duration::from_millis(i * 8));
            }
            assert_eq!(c.progress.value, target.progress());
            assert!(!c.active());
        }
    }
    #[test]
    fn work_area_keeps_output_origin_and_reserves_only_taskbar() {
        let area = Rectangle::new((100, 200).into(), (1646, 926).into());
        assert_eq!(
            reserve(area, ShellMode::Classic, true, true, false),
            Rectangle::new(area.loc, (1646, 878).into())
        );
        assert_eq!(
            reserve(area, ShellMode::Gavno, true, true, false),
            crate::dock::reserve(area, true, true, false)
        );
        assert_eq!(
            reserve(
                Rectangle::from_size((20, 10).into()),
                ShellMode::Classic,
                false,
                false,
                false
            )
            .size
            .h,
            0
        );
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;
    #[test]
    fn classic_config_parses_and_module_disable_wins_over_startup() {
        assert_eq!(ClassicConfig::default().align, Align::Left);
        let (config, warnings) = crate::config::Config::build(
            false,
            Some(
                r#"
[classic]
start_in_classic = true
align = "left"
pinned = []
start_pinned = ["@terminal", "firefox"]
settings_command = "custom-settings"
task_manager = "foot -e htop"
[modules]
classic = false
"#,
            ),
            |_| None,
        );
        assert!(warnings.is_empty());
        assert_eq!(config.classic.align, Align::Left);
        assert_eq!(config.classic.pinned, Some(Vec::new()));
        assert_eq!(config.classic.task_manager, "foot -e htop");
        let mut h = crate::state::tests::Harness::new();
        h.gv.features = crate::module::Features::new(&config);
        h.gv.set_shell_mode(ShellMode::Classic);
        assert_eq!(h.gv.classic.mode, ShellMode::Gavno);
        assert!(!h.gv.features.on(Feature::Classic));
        let (_, warnings) = crate::config::Config::build(
            false,
            Some("[classic]\nunknown_classic_key = true"),
            |_| None,
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("classic.unknown_classic_key"));
    }
}
