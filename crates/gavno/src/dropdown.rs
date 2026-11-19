//! Quake-style dropdown terminal on Mod+` (the key left of 1, matched by its
//! physical keycode so it also works as "ё" on the Russian layout).
//!
//! The first toggle spawns `[dropdown] command`; the toplevel that appears with
//! the app id [`APP_ID`] becomes the dropdown. It never enters the `Space`, the
//! workspaces, tiling or any window list: this module keeps the `Window`,
//! draws it above the windows (below the bar and its popovers), hit-tests it
//! and feeds it keyboard focus. Hiding slides it away but keeps the process,
//! so the next toggle is instant. Nothing here runs on a timer: frames are
//! requested only while the 160 ms slide runs.

use std::time::{Duration, Instant};

use smithay::{
    desktop::{find_popup_root_surface, Window, WindowSurfaceType},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{get_parent, with_states},
        shell::xdg::XdgToplevelSurfaceData,
    },
};

use crate::Gavno;

/// The app id the configured terminal must use.
pub const APP_ID: &str = "gavno-dropdown";
const SLIDE: Duration = Duration::from_millis(160);
/// A spawned terminal that never shows its window is forgotten after this long,
/// so the next toggle starts it again.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What a toggle asks the compositor to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// No terminal yet: start the configured command.
    Spawn,
    /// The terminal is still starting; the intent was flipped, nothing to do.
    Wait,
    /// Slide the hidden terminal in and focus it.
    Show,
    /// Slide the focused terminal away and restore the previous focus.
    Hide,
    /// Visible but another window has focus (`hide_on_focus_loss = false`).
    Focus,
}

/// Time-based ease-out slide between 0 (hidden above the top edge) and 1.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Slide {
    value: f64,
    from: f64,
    target: f64,
    /// Set by the first `advance` after `set`, so the clock is the renderer's.
    start: Option<Duration>,
    running: bool,
}
impl Default for Slide {
    fn default() -> Self {
        Self { value: 0.0, from: 0.0, target: 0.0, start: None, running: false }
    }
}
impl Slide {
    fn set(&mut self, target: f64) {
        if self.target == target && (self.running || self.value == target) { return; }
        self.from = self.value;
        self.target = target;
        self.start = None;
        self.running = self.value != target;
    }
    fn jump(&mut self, value: f64) {
        *self = Self { value, from: value, target: value, start: None, running: false };
    }
    fn advance(&mut self, now: Duration) {
        if !self.running { return; }
        let start = *self.start.get_or_insert(now);
        let t = (now.saturating_sub(start).as_secs_f64() / SLIDE.as_secs_f64()).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.value = self.from + (self.target - self.from) * eased;
        if t >= 1.0 {
            self.value = self.target;
            self.running = false;
        }
    }
}

/// The dropdown rectangle for a work area, in the area's coordinates: `width`
/// and `height` are fractions of it, centred horizontally, flush with the top.
pub fn geometry(work: Rectangle<i32, Logical>, width: f64, height: f64) -> Rectangle<i32, Logical> {
    let w = ((work.size.w as f64 * width).round() as i32).clamp(1, work.size.w.max(1));
    let h = ((work.size.h as f64 * height).round() as i32).clamp(1, work.size.h.max(1));
    Rectangle::new((work.loc.x + (work.size.w - w) / 2, work.loc.y).into(), (w, h).into())
}

/// Where the rectangle sits while the slide is at `value` (0: fully above the
/// top edge of the work area, 1: in place).
pub fn displayed(rect: Rectangle<i32, Logical>, value: f64) -> Rectangle<i32, Logical> {
    let hidden = ((1.0 - value.clamp(0.0, 1.0)) * rect.size.h as f64).round() as i32;
    Rectangle::new((rect.loc.x, rect.loc.y - hidden).into(), rect.size)
}

#[derive(Default)]
pub struct Dropdown {
    window: Option<Window>,
    /// The command was started and its window has not mapped yet.
    spawned_at: Option<Instant>,
    /// Show the window as soon as it maps.
    show_on_map: bool,
    /// Logically visible (or sliding in).
    shown: bool,
    slide: Slide,
    /// The output it is shown on: the active one at the last show.
    output: Option<Output>,
    /// Focus to give back when the dropdown is hidden.
    previous: Option<Window>,
}

impl Dropdown {
    pub fn is_animating(&self) -> bool {
        self.slide.running
    }
    /// Anything of it is on screen, including the slide away.
    pub fn visible(&self) -> bool {
        self.window.is_some() && (self.shown || self.slide.value > 0.0)
    }
    pub fn shown(&self) -> bool {
        self.window.is_some() && self.shown
    }
    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }
    pub fn output(&self) -> Option<&Output> {
        self.output.as_ref()
    }
    /// The window that gets frame callbacks on `output`, if it is drawn there.
    pub fn rendered(&self, output: &Output) -> Option<&Window> {
        (self.visible() && self.output.as_ref() == Some(output)).then_some(self.window.as_ref()).flatten()
    }
    pub(crate) fn progress(&self) -> f64 {
        self.slide.value
    }

    /// The toggle state machine. `focused`: the dropdown has keyboard focus.
    pub fn toggle(&mut self, focused: bool, now: Instant) -> Step {
        if self.window.is_none() {
            if self.spawned_at.is_some_and(|at| now.duration_since(at) < SPAWN_TIMEOUT) {
                self.show_on_map = !self.show_on_map;
                return Step::Wait;
            }
            self.spawned_at = Some(now);
            self.show_on_map = true;
            return Step::Spawn;
        }
        match (self.shown, focused) {
            (false, _) => Step::Show,
            (true, true) => Step::Hide,
            (true, false) => Step::Focus,
        }
    }

    /// The spawned terminal mapped; returns whether to show it right away.
    fn adopt(&mut self, window: Window) -> bool {
        self.window = Some(window);
        self.spawned_at = None;
        self.shown = false;
        self.slide.jump(0.0);
        std::mem::take(&mut self.show_on_map)
    }
    /// The terminal went away (exited or unmapped): back to the initial state.
    fn forget(&mut self) {
        self.window = None;
        self.spawned_at = None;
        self.show_on_map = false;
        self.shown = false;
        self.slide.jump(0.0);
        self.previous = None;
    }
    fn show(&mut self, output: Output) {
        self.output = Some(output);
        self.shown = true;
        self.slide.set(1.0);
    }
    fn hide(&mut self) {
        self.shown = false;
        self.slide.set(0.0);
    }
    /// Drop the window without any animation (locked session).
    fn hide_now(&mut self) {
        self.shown = false;
        self.slide.jump(0.0);
    }

    /// Advances the slide to the renderer's clock; cheap at rest.
    pub fn advance(&mut self, now: Duration) {
        self.slide.advance(now);
    }

    /// Render elements on `output` (empty while hidden or on another output).
    /// `work` is the output's work area in global coordinates.
    pub fn elements(
        &mut self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        decor: &mut crate::decor::WindowDecor,
        output: &Output,
        output_geo: Rectangle<i32, Logical>,
        work: Rectangle<i32, Logical>,
        config: &crate::config::DropdownConfig,
        time: Duration,
    ) -> Vec<crate::render::GavnoRenderElement> {
        self.advance(time);
        let Some(window) = self.rendered(output).cloned() else {
            decor.forget_dropdown(output);
            return Vec::new();
        };
        let mut rect = displayed(geometry(work, config.width, config.height), self.slide.value);
        rect.loc -= output_geo.loc;
        decor.dropdown_elements(renderer, output, &window, rect)
    }
}

impl Gavno {
    /// The configured size on `output`, in global logical coordinates.
    pub(crate) fn dropdown_rect(&self, output: &Output) -> Rectangle<i32, Logical> {
        geometry(self.work_area(output), self.config.dropdown.width, self.config.dropdown.height)
    }

    /// Should this new toplevel become the dropdown? Only the first window
    /// with the app id does; a second one is an ordinary window.
    pub(crate) fn is_dropdown_window(&self, window: &Window) -> bool {
        if self.overlay.dropdown.window.as_ref().is_some_and(|current| current != window && current.alive()) {
            return false;
        }
        window.toplevel().is_some_and(|toplevel| {
            with_states(toplevel.wl_surface(), |states| {
                states.data_map.get::<XdgToplevelSurfaceData>()
                    .is_some_and(|data| data.lock().unwrap().app_id.as_deref() == Some(APP_ID))
            })
        })
    }

    /// Is `surface` the dropdown toplevel, one of its subsurfaces or a popup?
    pub(crate) fn dropdown_owns(&self, surface: &WlSurface) -> bool {
        let Some(toplevel) = self.overlay.dropdown.window.as_ref().and_then(|w| w.toplevel()) else { return false };
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) { root = parent; }
        if let Some(popup) = self.popups.find_popup(&root) {
            let Ok(popup_root) = find_popup_root_surface(&popup) else { return false };
            root = popup_root;
        }
        toplevel.wl_surface() == &root
    }

    pub(crate) fn dropdown_focused(&self) -> bool {
        self.seat.get_keyboard().and_then(|keyboard| keyboard.current_focus())
            .is_some_and(|focus| self.dropdown_owns(&focus))
    }

    /// Fixes the size, bounds and states of the dropdown for `output`.
    fn configure_dropdown(&self, window: &Window, output: &Output, active: bool) {
        let Some(toplevel) = window.toplevel() else { return };
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
        let rect = self.dropdown_rect(output);
        toplevel.with_pending_state(|state| {
            state.size = Some(rect.size);
            state.bounds = Some(self.work_area(output).size);
            if state.decoration_mode.is_some() { state.decoration_mode = Some(Mode::ClientSide); }
            for flag in [xdg_toplevel::State::TiledLeft, xdg_toplevel::State::TiledRight,
                xdg_toplevel::State::TiledTop, xdg_toplevel::State::TiledBottom] {
                state.states.set(flag);
            }
            if active { state.states.set(xdg_toplevel::State::Activated); }
            else { state.states.unset(xdg_toplevel::State::Activated); }
        });
        toplevel.send_pending_configure();
    }

    /// The first configure of a not yet mapped dropdown terminal.
    pub(crate) fn prepare_dropdown_configure(&self, window: &Window) {
        let Some(output) = self.active_output() else { return };
        self.configure_dropdown(window, &output, false);
    }

    fn dropdown_refused(&self) -> bool {
        self.protocols.locks.locked || self.greeter.is_some() || self.lock.internal()
    }

    pub fn toggle_dropdown(&mut self) {
        if self.dropdown_refused() { return; }
        let focused = self.dropdown_focused();
        match self.overlay.dropdown.toggle(focused, Instant::now()) {
            Step::Spawn => crate::state::spawn_shell(&self.config.dropdown.command),
            Step::Wait => {}
            Step::Show | Step::Focus => self.show_dropdown(),
            Step::Hide => self.hide_dropdown(true),
        }
    }

    /// The spawned terminal committed its first buffer.
    pub(crate) fn adopt_dropdown(&mut self, window: Window) {
        window.on_commit();
        if self.overlay.dropdown.adopt(window) && !self.dropdown_refused() { self.show_dropdown(); }
    }

    /// The dropdown toplevel is gone (the process exited or the toplevel unmapped).
    pub(crate) fn dropdown_closed(&mut self) {
        let was_focused = self.dropdown_focused();
        let previous = self.overlay.dropdown.previous.take();
        self.overlay.dropdown.forget();
        if was_focused { self.restore_dropdown_focus(previous); }
        self.queue_redraw_all();
    }

    fn show_dropdown(&mut self) {
        let Some(window) = self.overlay.dropdown.window.clone() else { return };
        let Some(output) = self.active_output() else { return };
        if !self.dropdown_focused() {
            self.overlay.dropdown.previous = self.focused_window();
        }
        self.configure_dropdown(&window, &output, true);
        self.overlay.dropdown.show(output);
        self.focus_dropdown();
        self.queue_redraw_all();
    }

    /// Gives the dropdown keyboard focus; other windows lose their activation.
    pub(crate) fn focus_dropdown(&mut self) {
        let Some(window) = self.overlay.dropdown.window.clone() else { return };
        let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) else { return };
        self.classic.desktop.focused = false;
        for other in self.space.elements() {
            other.set_activated(false);
            if let Some(toplevel) = other.toplevel() { toplevel.send_pending_configure(); }
        }
        window.set_activated(true);
        if let Some(toplevel) = window.toplevel() { toplevel.send_pending_configure(); }
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
        self.queue_redraw_all();
    }

    /// Slides the dropdown away. `restore`: it has focus now and the previous
    /// window gets it back; when focus already moved elsewhere nothing changes.
    pub(crate) fn hide_dropdown(&mut self, restore: bool) {
        let Some(window) = self.overlay.dropdown.window.clone() else { return };
        let previous = self.overlay.dropdown.previous.take();
        let had_focus = self.dropdown_focused();
        self.overlay.dropdown.hide();
        window.set_activated(false);
        if let Some(toplevel) = window.toplevel() { toplevel.send_pending_configure(); }
        if restore && had_focus { self.restore_dropdown_focus(previous); }
        self.queue_redraw_all();
    }

    fn restore_dropdown_focus(&mut self, previous: Option<Window>) {
        match previous {
            Some(window) if window.alive() && (self.space.element_location(&window).is_some() || self.workspaces.membership(&window).is_some()) => {
                self.focus_window(Some(&window));
            }
            Some(_) => self.focus_topmost(),
            None => self.focus_window(None),
        }
    }

    /// Dropdown pointer target at `pos` (global): surface and its origin.
    pub(crate) fn dropdown_surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let dropdown = &self.overlay.dropdown;
        if !dropdown.shown() || dropdown.progress() <= 0.0 || self.dropdown_refused() { return None; }
        let (window, output) = (dropdown.window()?, dropdown.output()?);
        let rect = displayed(self.dropdown_rect(output), dropdown.progress());
        let origin = rect.loc - window.geometry().loc;
        let (surface, offset) = window.surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL)?;
        Some((surface, (origin + offset).to_f64()))
    }

    /// Runs once per loop iteration: lock handling and focus-loss hiding.
    pub(crate) fn refresh_dropdown(&mut self) {
        let dropdown = &self.overlay.dropdown;
        if dropdown.window.is_none() { return; }
        if !dropdown.window.as_ref().is_some_and(|w| w.alive()) {
            self.dropdown_closed();
            return;
        }
        if !self.overlay.dropdown.visible() { return; }
        if self.dropdown_refused() {
            self.overlay.dropdown.hide_now();
            self.queue_redraw_all();
            return;
        }
        if self.overlay.dropdown.shown() && self.config.dropdown.hide_on_focus_loss && !self.dropdown_focused() {
            self.hide_dropdown(false);
        }
    }

    /// Output settings or the config changed: refit a shown dropdown.
    pub(crate) fn dropdown_config_changed(&mut self) {
        let (Some(window), Some(output)) = (self.overlay.dropdown.window.clone(), self.overlay.dropdown.output.clone()) else { return };
        if !self.space.outputs().any(|o| o == &output) { return; }
        self.configure_dropdown(&window, &output, self.overlay.dropdown.shown());
        self.queue_redraw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::Harness;
    use smithay::{
        output::{Mode, PhysicalProperties, Subpixel},
        wayland::shell::xdg::ToplevelSurface,
    };

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }
    fn output(h: &mut Harness, name: &str, x: i32, size: (i32, i32), scale: f64) -> Output {
        let output = Output::new(name.into(), PhysicalProperties {
            size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "test".into(), model: "test".into(),
        });
        output.change_current_state(Some(Mode { size: size.into(), refresh: 60000 }), None,
            Some(smithay::output::Scale::Fractional(scale)), Some((x, 0).into()));
        h.gv.space.map_output(&output, (x, 0));
        output
    }
    fn set_app_id(window: &Window, app_id: &str) {
        with_states(window.toplevel().unwrap().wl_surface(), |states| {
            states.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap().app_id = Some(app_id.into());
        });
    }
    /// A mapped dropdown terminal, committed through the real commit path.
    fn spawned(h: &mut Harness) -> Window {
        let window = h.window();
        set_app_id(&window, APP_ID);
        h.gv.unmapped.push(window.clone());
        h.commit_size(&window, 400, 200);
        window
    }
    fn plain_window(h: &mut Harness) -> Window {
        let window = h.window();
        h.gv.map_new_window(window.clone());
        h.commit_size(&window, 300, 200);
        window
    }
    fn focus_of(h: &Harness) -> Option<WlSurface> {
        h.gv.seat.get_keyboard().unwrap().current_focus()
    }
    fn surface(window: &Window) -> WlSurface {
        window.toplevel().unwrap().wl_surface().clone()
    }
    fn quiet(h: &mut Harness) {
        h.gv.config.modules.animations = false;
        h.gv.features = crate::module::Features::new(&h.gv.config);
    }

    #[test]
    fn toggle_state_machine() {
        let now = Instant::now();
        let mut d = Dropdown::default();
        assert_eq!(d.toggle(false, now), Step::Spawn);
        // Pressing again while it starts cancels, and again re-arms the show.
        assert_eq!(d.toggle(false, now), Step::Wait);
        assert!(!d.show_on_map);
        assert_eq!(d.toggle(false, now), Step::Wait);
        assert!(d.show_on_map);
        // A terminal that never maps is started again after the timeout.
        assert_eq!(d.toggle(false, now + SPAWN_TIMEOUT), Step::Spawn);
    }

    #[test]
    fn slide_runs_160ms_with_ease_out_and_stops() {
        let mut s = Slide::default();
        s.set(1.0);
        assert!(s.running);
        s.advance(Duration::from_millis(1000));
        assert_eq!(s.value, 0.0);
        s.advance(Duration::from_millis(1080));
        assert!(s.value > 0.5 && s.value < 1.0, "ease-out is past halfway at half time: {}", s.value);
        assert!(s.running);
        s.advance(Duration::from_millis(1160));
        assert_eq!((s.value, s.running), (1.0, false));
        s.set(0.0);
        s.advance(Duration::from_millis(2000));
        s.advance(Duration::from_millis(3000));
        assert_eq!((s.value, s.running), (0.0, false));
        // Setting the current target again never restarts the clock.
        s.set(0.0);
        assert!(!s.running);
    }

    #[test]
    fn geometry_follows_work_area_and_scale_free_fractions() {
        let work = rect(100, 40, 1600, 900);
        assert_eq!(geometry(work, 1.0, 0.45), rect(100, 40, 1600, 405));
        assert_eq!(geometry(work, 0.5, 0.5), rect(500, 40, 800, 450));
        // A work area at an offset (second output) keeps its origin.
        assert_eq!(geometry(rect(1920, 0, 1280, 720), 1.0, 0.45), rect(1920, 0, 1280, 324));
        assert_eq!(geometry(rect(0, 0, 10, 10), 0.0, 0.0).size, (1, 1).into());
        let full = geometry(work, 1.0, 0.45);
        assert_eq!(displayed(full, 1.0), full);
        assert_eq!(displayed(full, 0.0).loc.y, 40 - 405);
        assert_eq!(displayed(full, 0.5).loc.y, 40 - 203);
    }

    #[test]
    fn size_per_output_and_scale() {
        let mut h = Harness::new();
        quiet(&mut h);
        let left = output(&mut h, "left", 0, (1600, 900), 1.0);
        let right = output(&mut h, "right", 1600, (2880, 1620), 1.35);
        h.gv.config.dropdown.height = 0.5;
        h.gv.arrange_layers_all();
        let a = h.gv.dropdown_rect(&left);
        let b = h.gv.dropdown_rect(&right);
        assert_eq!(a, geometry(h.gv.work_area(&left), 1.0, 0.5));
        assert_eq!(b, geometry(h.gv.work_area(&right), 1.0, 0.5));
        assert_eq!(a.loc.x, 0);
        assert_eq!(b.loc.x, 1600);
        assert_eq!(b.size.w, h.gv.work_area(&right).size.w);
        // The first configure uses the active output (the pointer is on the first one).
        let window = h.window();
        set_app_id(&window, APP_ID);
        h.gv.prepare_dropdown_configure(&window);
        let pending = window.toplevel().unwrap().with_pending_state(|s| s.size);
        assert_eq!(pending, Some(h.gv.dropdown_rect(&left).size));
    }

    #[test]
    fn adopted_window_stays_out_of_space_workspaces_and_lists() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        let other = plain_window(&mut h);
        let window = spawned(&mut h);
        assert!(h.gv.unmapped.is_empty());
        assert_eq!(h.gv.overlay.dropdown.window(), Some(&window));
        assert!(h.gv.space.element_location(&window).is_none());
        assert!(h.gv.workspaces.membership(&window).is_none());
        assert!(h.gv.workspaces.all().all(|owned| owned.window != window));
        assert!(h.gv.space.elements().all(|w| w != &window));
        assert!(h.gv.window_for_surface(&surface(&window)).is_none());
        assert!(!h.gv.mru.contains(&window));
        assert!(h.gv.space.elements().any(|w| w == &other));
        // Mapped hidden: nothing animates, nothing is drawn, no input target.
        assert!(!h.gv.overlay.dropdown.visible());
        assert!(!h.gv.is_animating());
        assert!(h.gv.dropdown_surface_under((50.0, 50.0).into()).is_none());
        // Tiling a workspace never picks it up either.
        h.gv.reflow_tiling(true);
        assert!(h.gv.space.element_location(&window).is_none());
    }

    #[test]
    fn toggle_shows_focuses_and_hide_restores_the_previous_focus() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        let main = plain_window(&mut h);
        let dropdown = spawned(&mut h);
        h.gv.focus_window(Some(&main));
        assert_eq!(focus_of(&h), Some(surface(&main)));

        h.gv.toggle_dropdown();
        assert!(h.gv.overlay.dropdown.shown());
        assert_eq!(focus_of(&h), Some(surface(&dropdown)));
        assert!(h.gv.is_animating(), "the slide asks for frames");
        // The first frame starts the clock, a later one finishes it.
        h.gv.overlay.dropdown.advance(Duration::from_secs(5));
        h.gv.overlay.dropdown.advance(Duration::from_secs(6));
        assert_eq!(h.gv.overlay.dropdown.progress(), 1.0);
        assert!(!h.gv.is_animating(), "zero idle cost once shown");
        // Pointer hit testing: inside the dropdown, then below it.
        let rect = h.gv.dropdown_rect(&h.gv.active_output().unwrap());
        let inside = Point::from((rect.loc.x as f64 + 20.0, rect.loc.y as f64 + 20.0));
        let (under, _) = h.gv.dropdown_surface_under(inside).expect("dropdown under pointer");
        assert_eq!(under, surface(&dropdown));
        assert_eq!(h.gv.surface_under(inside).map(|(s, _)| s), Some(surface(&dropdown)));
        let below = Point::from((inside.x, (rect.loc.y + rect.size.h) as f64 + 10.0));
        assert!(h.gv.dropdown_surface_under(below).is_none());

        h.gv.toggle_dropdown();
        assert!(!h.gv.overlay.dropdown.shown());
        assert_eq!(focus_of(&h), Some(surface(&main)), "focus returns to the previous window");
        assert!(h.gv.overlay.dropdown.visible(), "still sliding away");
        h.gv.overlay.dropdown.advance(Duration::from_secs(7));
        h.gv.overlay.dropdown.advance(Duration::from_secs(8));
        assert!(!h.gv.overlay.dropdown.visible());
        assert!(!h.gv.is_animating());
        assert!(h.gv.overlay.dropdown.window().is_some(), "the process stays alive");
        assert!(h.gv.dropdown_surface_under(inside).is_none());
        // Instant reopen.
        h.gv.toggle_dropdown();
        assert_eq!(focus_of(&h), Some(surface(&dropdown)));
    }

    #[test]
    fn hide_without_a_previous_window_clears_focus() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        let dropdown = spawned(&mut h);
        h.gv.toggle_dropdown();
        assert_eq!(focus_of(&h), Some(surface(&dropdown)));
        h.gv.toggle_dropdown();
        assert_eq!(focus_of(&h), None);
    }

    #[test]
    fn focus_loss_hides_without_stealing_focus_back() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        let main = plain_window(&mut h);
        let other = plain_window(&mut h);
        let dropdown = spawned(&mut h);
        h.gv.focus_window(Some(&main));
        h.gv.toggle_dropdown();
        assert_eq!(focus_of(&h), Some(surface(&dropdown)));
        h.gv.focus_window(Some(&other));
        h.gv.on_loop_idle();
        assert!(!h.gv.overlay.dropdown.shown());
        assert_eq!(focus_of(&h), Some(surface(&other)));
        // With the option off the dropdown stays; the toggle refocuses it first.
        h.gv.config.dropdown.hide_on_focus_loss = false;
        h.gv.toggle_dropdown();
        h.gv.focus_window(Some(&other));
        h.gv.on_loop_idle();
        assert!(h.gv.overlay.dropdown.shown());
        assert_eq!(focus_of(&h), Some(surface(&other)));
        h.gv.toggle_dropdown();
        assert!(h.gv.overlay.dropdown.shown());
        assert_eq!(focus_of(&h), Some(surface(&dropdown)));
        h.gv.toggle_dropdown();
        assert!(!h.gv.overlay.dropdown.shown());
        assert_eq!(focus_of(&h), Some(surface(&other)));
    }

    #[test]
    fn locked_session_hides_and_refuses_the_toggle() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        spawned(&mut h);
        h.gv.toggle_dropdown();
        assert!(h.gv.overlay.dropdown.shown());
        h.gv.protocols.locks.locked = true;
        h.gv.on_loop_idle();
        assert!(!h.gv.overlay.dropdown.visible());
        assert!(h.gv.dropdown_surface_under((10.0, 10.0).into()).is_none());
        h.gv.toggle_dropdown();
        assert!(!h.gv.overlay.dropdown.visible());
        h.gv.protocols.locks.locked = false;
        h.gv.toggle_dropdown();
        assert!(h.gv.overlay.dropdown.shown());
    }

    #[test]
    fn first_toggle_spawns_once_and_shows_when_the_window_maps() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        // A harmless command: the compositor only starts it.
        h.gv.config.dropdown.command = "true".into();
        h.gv.toggle_dropdown();
        assert!(h.gv.overlay.dropdown.spawned_at.is_some());
        assert!(h.gv.overlay.dropdown.show_on_map);
        h.gv.toggle_dropdown();
        assert!(!h.gv.overlay.dropdown.show_on_map, "a second press while starting cancels the show");
        h.gv.toggle_dropdown();
        let window = spawned(&mut h);
        assert!(h.gv.overlay.dropdown.shown());
        assert_eq!(focus_of(&h), Some(surface(&window)));
    }

    #[test]
    fn exit_clears_the_state_and_the_next_toggle_spawns_again() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        let main = plain_window(&mut h);
        let dropdown = spawned(&mut h);
        h.gv.focus_window(Some(&main));
        h.gv.toggle_dropdown();
        assert_eq!(focus_of(&h), Some(surface(&dropdown)));
        h.gv.dropdown_closed();
        assert!(h.gv.overlay.dropdown.window().is_none());
        assert!(!h.gv.overlay.dropdown.visible());
        assert!(!h.gv.is_animating());
        assert_eq!(focus_of(&h), Some(surface(&main)), "focus returns when the terminal exits");
        h.gv.config.dropdown.command = "true".into();
        h.gv.toggle_dropdown();
        assert!(h.gv.overlay.dropdown.spawned_at.is_some(), "respawn after exit");
    }

    #[test]
    fn toplevel_destroy_path_forgets_the_window() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        let dropdown = spawned(&mut h);
        let toplevel: ToplevelSurface = dropdown.toplevel().unwrap().clone();
        use smithay::wayland::shell::xdg::XdgShellHandler;
        h.gv.toplevel_destroyed(toplevel);
        assert!(h.gv.overlay.dropdown.window().is_none());
    }

    #[test]
    fn second_window_with_the_app_id_is_an_ordinary_window() {
        let mut h = Harness::new();
        quiet(&mut h);
        output(&mut h, "o", 0, (1600, 900), 1.0);
        let first = spawned(&mut h);
        let second = h.window();
        set_app_id(&second, APP_ID);
        h.gv.unmapped.push(second.clone());
        h.commit_size(&second, 100, 100);
        assert_eq!(h.gv.overlay.dropdown.window(), Some(&first));
        assert!(h.gv.space.elements().any(|w| w == &second), "the extra window is mapped normally");
    }

    #[test]
    fn config_reload_refits_a_shown_dropdown() {
        let mut h = Harness::new();
        quiet(&mut h);
        let output = output(&mut h, "o", 0, (1600, 900), 1.0);
        let window = spawned(&mut h);
        h.gv.toggle_dropdown();
        h.commit_size(&window, 1600, 400);
        h.gv.config.dropdown.height = 0.6;
        h.gv.dropdown_config_changed();
        let size = window.toplevel().unwrap().with_pending_state(|s| s.size);
        assert_eq!(size, Some(h.gv.dropdown_rect(&output).size));
    }
}
