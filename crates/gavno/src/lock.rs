//! Built-in session lock. The protocol gate is set before any fallible UI work.
mod auth;
pub(crate) mod backdrop;
mod logind;

use crate::{
    greeter::{
        ipc::{Reply, Secret},
        Greeter,
    },
    render::GavnoRenderElement,
    Gavno,
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    input::keyboard::{Keysym, ModifiersState},
    reexports::calloop::{
        channel,
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{Logical, Point, Size},
};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Lock {
    internal: bool,
    backdrop: backdrop::Backdrop,
    /// True only while the renderer builds the desktop scene for the backdrop.
    capturing: bool,
    media_canvas: crate::topbar::Canvas,
    media_icons: crate::icons::Icons,
    authenticated: bool,
    failed: bool,
    ui_panics: u8,
    recover_ui: bool,
    #[cfg(test)]
    pub(crate) panic_key: bool,
    ui: Option<Greeter>,
    backoff: Backoff,
    blocked_until: Option<Instant>,
    checking: bool,
    generation: u64,
    minute_timer: Option<RegistrationToken>,
    idle_timer: Option<RegistrationToken>,
    idle_generation: u64,
    retry_timer: Option<RegistrationToken>,
    logind: Option<mpsc::Sender<logind::Command>>,
    hinted: bool,
    sleeping: bool,
    released: bool,
}
#[derive(Default)]
struct Backoff(u32);
impl Backoff {
    fn next(&mut self) -> Duration {
        let seconds = (1u64 << self.0.min(5)).min(30);
        self.0 = self.0.saturating_add(1);
        Duration::from_secs(seconds)
    }
}
impl Lock {
    #[cfg(test)]
    pub(crate) fn idle_timer_armed(&self) -> bool {
        self.idle_timer.is_some()
    }
    pub fn internal(&self) -> bool {
        self.internal
    }
    /// The internal lock is hiding the shell. False while the one-off desktop
    /// capture for the backdrop is built, which never reaches the screen.
    pub fn shell_hidden(&self) -> bool {
        self.internal && !self.capturing
    }
    pub fn set_capturing(&mut self, on: bool) {
        self.capturing = on;
    }
    pub fn backdrop_wanted(&self, output: &str) -> bool {
        self.internal && !self.failed && !self.authenticated && self.backdrop.wants(output)
    }
    pub fn start_backdrop(&mut self, output: &str, rgba: Vec<u8>, width: usize, height: usize) {
        self.backdrop.start(output, rgba, width, height);
    }
    pub fn desktop_visible(&self) -> bool {
        self.internal && self.authenticated && !self.failed
    }
    fn accepts_input(&self) -> bool {
        self.internal
            && !self.failed
            && !self.authenticated
            && !self.checking
            && self
                .blocked_until
                .is_none_or(|until| Instant::now() >= until)
    }
    /// Discard a failed UI before the event loop recreates it behind the closed gate.
    fn ui_call<T>(&mut self, f: impl FnOnce(&mut Greeter) -> T) -> Option<T> {
        if self.failed {
            return None;
        }
        let ui = self.ui.as_mut()?;
        match catch_unwind(AssertUnwindSafe(|| f(ui))) {
            Ok(value) => Some(value),
            Err(payload) => {
                self.ui_panics = self.ui_panics.saturating_add(1);
                self.recover_ui = self.ui_panics == 1;
                self.generation = self.generation.wrapping_add(1);
                self.checking = false;
                self.failed = true;
                self.authenticated = false;
                self.ui = None;
                tracing::error!(message = %crate::module::panic_message(payload.as_ref()), "lock UI failed; retaining solid-color lock");
                None
            }
        }
    }
    pub fn is_animating(&mut self) -> bool {
        self.internal && self.ui_call(|ui| ui.is_animating()).unwrap_or(false)
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &str,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        theme: &crate::config::ThemeConfig,
        battery: Option<(u8, bool)>,
    ) -> Vec<GavnoRenderElement> {
        let backdrop = self.backdrop.buffer(output);
        let elements = self
            .ui_call(|ui| {
                ui.set_location(theme);
                ui.set_backdrop(backdrop);
                ui.set_battery(battery);
                ui.elements(renderer, size, scale, now)
            })
            .unwrap_or_default();
        if elements.is_empty() && !self.failed {
            // Shader initialization failure is reported without a panic. Stop
            // requesting animation frames and retain the same closed gate.
            self.ui_panics = self.ui_panics.saturating_add(1);
            self.recover_ui = self.ui_panics == 1;
            self.failed = true;
            self.authenticated = false;
            self.ui = None;
        }
        elements
    }
    pub(crate) fn clear_media(&mut self) {
        self.media_canvas = Default::default();
        self.media_icons = Default::default();
    }
    pub(crate) fn media_elements(
        &mut self, renderer: &mut GlesRenderer, size: Size<i32, Logical>, scale: f64,
        snapshot: &crate::media::Snapshot, theme: &str,
    ) -> Vec<GavnoRenderElement> {
        if self.failed || self.authenticated || self.ui.is_none() { return Vec::new(); }
        let Some(player) = &snapshot.player else { return Vec::new(); };
        let Some((line, buttons)) = crate::media::ui::lock_geometry(size) else { return Vec::new(); };
        let c = &mut self.media_canvas;
        c.begin(renderer, scale);
        // Title and artist, centred above the buttons and cut with an ellipsis.
        let (title_style, _) = crate::media::ui::capsule_styles(player.playback);
        let artist = player.artist();
        let title_style = crate::ui::text::TextStyleSpec { size: 14.0, ..title_style };
        let label = if artist.is_empty() { player.title().to_owned() } else { format!("{} · {artist}", player.title()) };
        let label = crate::topbar::ellipsize(&mut c.text, &label, title_style, line.size.w);
        let width = (c.text.measure(&label, title_style).ceil() as i32).min(line.size.w);
        c.styled_label(renderer, &label, line.loc.x + (line.size.w - width) / 2, line.loc.y, width + 2, title_style);
        for (command, area) in buttons {
            crate::media::ui::transport(
                c,
                renderer,
                &mut self.media_icons,
                theme,
                command,
                player.playback == crate::media::Playback::Playing,
                player.allows(command),
                area,
                false,
            );
        }
        c.finish()
    }
    pub fn key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        if self.accepts_input() {
            #[cfg(test)]
            let inject = std::mem::take(&mut self.panic_key);
            self.ui_call(|ui| {
                #[cfg(test)]
                if inject { panic!("injected lock key panic"); }
                ui.key(sym, ch, mods)
            });
            self.checking = self.ui.as_ref().is_some_and(Greeter::lock_checking);
        }
    }
    pub fn modifiers(&mut self, caps: bool, layout: &str) {
        self.ui_call(|ui| ui.set_modifiers(caps, layout));
    }
    pub fn pointer(&mut self, pos: Point<f64, Logical>, click: bool) -> bool {
        if !self.accepts_input() {
            return false;
        }
        if click {
            if let Some(Some(hit)) = self.ui_call(|ui| ui.hit(pos)) {
                self.command(logind::Command::Power(hit));
                return true;
            }
            false
        } else {
            self.ui_call(|ui| ui.pointer_motion(pos)).unwrap_or(false)
        }
    }
    fn command(&self, command: logind::Command) {
        if let Some(tx) = &self.logind {
            let _ = tx.send(command);
        }
    }
}

pub fn start(gv: &mut Gavno) {
    let _startup = crate::perf::Startup::new("dbus.logind.spawn.begin", "dbus.logind.spawn.end");
    gv.lock.logind = logind::spawn(&gv.loop_handle);
    gv.lock
        .command(logind::Command::Hint(gv.protocols.locks.locked));
    gv.refresh_idle();
}
impl Gavno {
    pub fn lock_session(&mut self) -> bool {
        if self.greeter.is_some() || self.protocols.locks.locked {
            return false;
        }
        // Set the security gate before constructing shaders/fonts or spawning PAM.
        self.protocols.locks =
            crate::handlers::session_lock::Locks::internal(self.space.outputs().cloned());
        self.refresh_lock_idle(false, false);
        self.lock.internal = true;
        self.start_lock_backdrop();
        // The camera never runs behind the lock screen.
        self.stop_finger_pointer();
        self.lock.authenticated = false;
        self.lock.failed = false;
        self.lock.ui_panics = 0;
        self.lock.recover_ui = false;
        self.lock.backoff = Backoff::default();
        self.lock.blocked_until = None;
        self.lock.checking = false;
        self.lock.generation = self.lock.generation.wrapping_add(1);
        self.secure_lock_input();
        self.switcher_finish(false);
        self.exit_resize_mode();
        self.cancel_window_gestures();
        self.window_gestures = Default::default();
        self.close_overview();
        self.gestures = Default::default();
        self.overview.drag = None;
        self.overview.flick = None;
        self.guard(crate::module::Feature::Bar, (), |gv| gv.topbar.close());
        self.guard(crate::module::Feature::Dock, (), |gv| gv.dock.close_menu());
        self.create_lock_ui();
        self.update_lock_modifiers();
        self.schedule_lock_minute();
        self.refresh_internal_lock();
        self.queue_redraw_all();
        true
    }
    /// One desktop capture per output is taken by the next frame; the blur
    /// runs on a worker and arrives over this channel.
    fn start_lock_backdrop(&mut self) {
        if let Some(token) = self.lock.backdrop.source.take() {
            self.loop_handle.remove(token);
        }
        let (tx, rx) = channel::channel::<backdrop::Blurred>();
        match self.loop_handle.insert_source(rx, |event, _, gv| {
            if let channel::Event::Msg(blurred) = event {
                if gv.lock.internal && gv.lock.backdrop.accept(blurred) {
                    gv.queue_redraw_all();
                }
            }
        }) {
            Ok(token) => {
                self.lock.backdrop.request(tx);
                self.lock.backdrop.source = Some(token);
            }
            Err(error) => tracing::warn!(%error, "cannot register the lock backdrop channel"),
        }
    }
    pub(crate) fn update_lock_modifiers(&mut self) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            let caps = keyboard.modifier_state().caps_lock;
            let layout = keyboard.with_xkb_state(self, |ctx| {
                let xkb = ctx.xkb().lock().unwrap_or_else(|e| e.into_inner());
                xkb.layout_name(xkb.active_layout()).to_owned()
            });
            self.lock.modifiers(caps, &layout);
        }
    }
    fn restart_lock_ui(&mut self) {
        self.lock.authenticated = false;
        self.lock.checking = false;
        self.lock.ui = None;
        self.lock.generation = self.lock.generation.wrapping_add(1);
        self.create_lock_ui();
        self.update_lock_modifiers();
    }
    fn create_lock_ui(&mut self) {
        let generation = self.lock.generation;
        let (password_tx, password_rx) = mpsc::channel::<Secret>();
        let (reply_tx, reply_rx) = channel::channel::<bool>();
        if self
            .loop_handle
            .insert_source(reply_rx, move |event, _, gv| {
                if let channel::Event::Msg(success) = event {
                    gv.lock_auth_reply(generation, success);
                }
            })
            .is_err()
        {
            self.lock.failed = true;
            return;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            let user = auth::current_user()?;
            let username = user.name.clone();
            std::thread::Builder::new()
                .name("lock-pam".into())
                .spawn(move || {
                    for password in password_rx {
                        let success = catch_unwind(AssertUnwindSafe(|| {
                            auth::authenticate(&username, password)
                        }))
                        .unwrap_or(false);
                        if reply_tx.send(success).is_err() {
                            break;
                        }
                    }
                })
                .ok()?;
            Some(Greeter::for_lock(password_tx, user))
        }));
        match result {
            Ok(Some(ui)) => self.lock.ui = Some(ui),
            _ => {
                self.lock.failed = true;
                tracing::error!("cannot initialize lock UI; retaining solid-color lock");
            }
        }
    }
    fn lock_auth_reply(&mut self, generation: u64, success: bool) {
        if self.lock.internal && self.lock.generation == generation {
            self.lock_auth_result(success);
        }
    }
    fn lock_auth_result(&mut self, success: bool) {
        self.lock.checking = false;
        if self.lock.failed {
            return;
        }
        if success && !self.lock.sleeping {
            self.lock.authenticated = true;
            self.lock.ui_call(|ui| ui.on_reply(Reply::Authenticated));
        } else {
            self.lock.ui_call(Greeter::lock_failed);
            let delay = self.lock.backoff.next();
            self.lock.blocked_until = Some(Instant::now() + delay);
            let generation = self.lock.generation;
            self.lock.retry_timer = self
                .loop_handle
                .insert_source(Timer::from_duration(delay), move |_, _, gv| {
                    let _stall = crate::perf::Stall::new("timer.lock_retry");
                    if gv.lock.generation == generation {
                        gv.lock.retry_timer = None;
                        gv.lock.blocked_until = None;
                        gv.queue_redraw_all();
                    }
                    TimeoutAction::Drop
                })
                .ok();
        }
        self.queue_redraw_all();
    }
    pub(crate) fn refresh_internal_lock(&mut self) {
        if self.power.is_off() {
            for output in &self.power.secured_outputs { self.protocols.locks.powered_off(output); }
        }
        if self.lock.internal && std::mem::take(&mut self.lock.recover_ui) {
            self.lock.failed = false;
            self.restart_lock_ui();
            if let Some(ui) = self.lock.ui.as_mut() { ui.settle_intro(); }
        }
        let locked = self.protocols.locks.locked;
        if locked != self.lock.hinted {
            self.lock.hinted = locked;
            self.lock.command(logind::Command::Hint(locked));
        }
        if self.lock.sleeping && locked && self.protocols.locks.secured() && !self.lock.released {
            self.lock.released = true;
            self.lock.command(logind::Command::Secured);
        }
        // Rendering may only request completion. The gate is cleared here, and only after PAM.
        let complete = self.lock.authenticated
            && !self.lock.failed
            && self.lock.ui.as_ref().is_some_and(|ui| ui.quit_requested);
        if self.lock.internal && complete {
            self.lock.internal = false;
            self.lock.authenticated = false;
            self.lock.ui = None;
            if let Some(token) = self.lock.backdrop.source.take() {
                self.loop_handle.remove(token);
            }
            self.lock.backdrop.clear();
            for token in [self.lock.minute_timer.take(), self.lock.retry_timer.take()]
                .into_iter()
                .flatten()
            {
                self.loop_handle.remove(token);
            }
            self.protocols.locks = Default::default();
            self.lock.hinted = false;
            self.lock.command(logind::Command::Hint(false));
            self.focus_topmost();
            self.protocols.idle_policy.activity();
            self.queue_redraw_all();
        }
    }
    pub(crate) fn power_lock_clock(&mut self, on: bool) {
        if let Some(token) = self.lock.minute_timer.take() { self.loop_handle.remove(token); }
        if on && self.lock.internal && !self.lock.failed { self.schedule_lock_minute(); }
    }
    fn schedule_lock_minute(&mut self) {
        if self.power.is_off() { return; }
        if let Some(token) = self.lock.minute_timer.take() {
            self.loop_handle.remove(token);
        }
        let time = crate::ui::theme::LocalTime::now();
        let delay =
            Duration::from_secs(60 - u64::from(time.second) % 60) + Duration::from_millis(20);
        self.lock.minute_timer = self
            .loop_handle
            .insert_source(Timer::from_duration(delay), |_, _, gv| {
                let _stall = crate::perf::Stall::new("timer.lock_clock");
                gv.lock.minute_timer = None;
                if gv.lock.internal && !gv.lock.failed {
                    gv.queue_redraw_all();
                    gv.schedule_lock_minute();
                }
                TimeoutAction::Drop
            })
            .ok();
    }
    /// Shares the existing idle policy's coalesced activity and visible inhibitors.
    pub(crate) fn refresh_lock_idle(&mut self, activity: bool, inhibited: bool) {
        let enabled = self.config.lock.after_idle_secs > 0
            && self.greeter.is_none()
            && !self.protocols.locks.locked
            && !inhibited;
        if !enabled || activity {
            self.lock.idle_generation = self.lock.idle_generation.wrapping_add(1);
            if let Some(token) = self.lock.idle_timer.take() {
                self.loop_handle.remove(token);
            }
        }
        if enabled && self.lock.idle_timer.is_none() {
            let delay = Duration::from_secs(u64::from(self.config.lock.after_idle_secs));
            let generation = self.lock.idle_generation;
            self.lock.idle_timer = self
                .loop_handle
                .insert_source(Timer::from_duration(delay), move |_, _, gv| {
                    let _stall = crate::perf::Stall::new("timer.lock_idle");
                    gv.lock.idle_timer = None;
                    // Evaluate inhibitor changes from this same event-loop batch first.
                    gv.loop_handle.insert_idle(move |gv| {
                        let _stall = crate::perf::Stall::new("idle.lock_idle");
                        gv.refresh_idle();
                        if gv.lock.idle_generation == generation
                            && !gv.protocols.idle.is_inhibited()
                        {
                            gv.lock_session();
                        }
                    });
                    TimeoutAction::Drop
                })
                .ok();
        }
    }
    fn logind_signal(&mut self, signal: logind::Signal) {
        match signal {
            logind::Signal::Lock => {
                if self.lock.internal && self.lock.authenticated {
                    self.restart_lock_ui();
                }
                self.lock_session();
                self.queue_redraw_all();
            }
            logind::Signal::Unlock => {
                tracing::warn!("ignoring logind Unlock signal; local authentication is required")
            }
            logind::Signal::Sleep(true) => {
                self.lock.sleeping = true;
                self.lock.released = false;
                if self.lock.internal && !self.lock.failed {
                    // Invalidate in-flight PAM replies as well as a successful fade.
                    self.restart_lock_ui();
                }
                self.lock_session();
                // A previous secure frame may have been followed by a successful
                // fade exposing the desktop. Require fresh presentation for sleep.
                self.protocols
                    .locks
                    .require_secure_frame(self.space.outputs().cloned());
                self.refresh_internal_lock();
                self.queue_redraw_all();
            }
            logind::Signal::Sleep(false) => {
                self.lock.sleeping = false;
                self.lock.released = false;
                self.lock.command(logind::Command::Resume);
                self.power_on_monitors("session resume");
            }
        }
    }
    pub(crate) fn lock_pointer(&mut self, click: bool) {
        let pos = self.pointer_location();
        let geometry = self.space.outputs().filter_map(|o| self.space.output_geometry(o))
            .find(|geo| geo.to_f64().contains(pos));
        if click && !self.lock.failed && !self.lock.authenticated && self.lock.ui.is_some() {
            if let Some(geo) = geometry {
                let local = pos - geo.loc.to_f64();
                let command = self.features.read(crate::module::Feature::Media, None, || {
                    self.now_playing.borrow().snapshot.player.as_ref()?;
                    crate::media::ui::lock_geometry(geo.size)?.1.into_iter()
                        .find(|(_, area)| area.to_f64().contains(local)).map(|(command, _)| command)
                });
                if let Some(command) = command { self.media_command(command); return; }
            }
        }
        let origin = geometry.map(|geo| geo.loc);
        if let Some(origin) = origin {
            if self.lock.pointer(pos - origin.to_f64(), click) || !click {
                self.queue_redraw_all();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backoff_doubles_and_caps_without_overflow() {
        let mut delay = Backoff::default();
        assert_eq!(
            (0..8).map(|_| delay.next().as_secs()).collect::<Vec<_>>(),
            [1, 2, 4, 8, 16, 30, 30, 30]
        );
        delay.0 = u32::MAX;
        assert_eq!(delay.next().as_secs(), 30);
    }
    #[test]
    fn internal_external_exclusion_and_logind_unlock() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.protocols.locks.locked = true;
        assert!(!h.gv.lock_session());
        assert!(!h.gv.lock.internal);
        h.gv.protocols.locks.locked = false;
        h.gv.logind_signal(logind::Signal::Lock);
        assert!(h.gv.protocols.locks.locked);
        assert!(h.gv.lock.internal);
        h.gv.logind_signal(logind::Signal::Unlock);
        assert!(h.gv.protocols.locks.locked);
        assert!(!h.gv.lock.authenticated);
        assert!(!h.gv.lock_session());
    }
    fn output() -> smithay::output::Output {
        smithay::output::Output::new(
            "test".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        )
    }
    #[test]
    fn media_buttons_send_commands_without_changing_the_lock_gate() {
        let mut h = crate::state::tests::Harness::new();
        let output = output();
        output.change_current_state(Some(smithay::output::Mode {
            size: (1646, 926).into(), refresh: 120000,
        }), None, None, None);
        h.gv.space.map_output(&output, (0, 0));
        let (tx, rx) = mpsc::channel();
        h.gv.now_playing.borrow_mut().worker = Some(tx);
        h.gv.media_update(crate::media::Snapshot {
            player: Some(crate::media::Player {
                playback: crate::media::Playback::Playing,
                can_next: true, can_previous: true, can_pause: true, ..Default::default()
            }), ..Default::default()
        });
        assert!(h.gv.lock_session());
        assert!(h.gv.lock.ui.is_some());
        let generation = h.gv.lock.generation;
        let pointer = h.gv.seat.get_pointer().unwrap();
        for (command, area) in crate::media::ui::lock_geometry((1646, 926).into()).unwrap().1 {
            pointer.motion(&mut h.gv, None, &smithay::input::pointer::MotionEvent {
                location: (area.loc.x as f64 + 10.0, area.loc.y as f64 + 10.0).into(),
                serial: smithay::utils::SERIAL_COUNTER.next_serial(), time: 1,
            });
            h.gv.lock_pointer(true);
            assert!(matches!(rx.try_recv(), Ok(crate::media::mpris::Request::Command(c)) if c == command));
            assert!(h.gv.protocols.locks.locked);
            assert!(!h.gv.lock.authenticated);
            assert!(!h.gv.lock.checking);
            assert_eq!(h.gv.lock.generation, generation);
        }
    }
    /// Renders the lock screen headlessly in a day and a night state. Set
    /// GAVNO_LOCK_TEST_ARTIFACTS to keep the PNGs.
    #[test]
    fn lock_screen_renders_with_backdrop_battery_and_media() {
        let mut renderer = crate::media::ui::artifacts::renderer();
        let size: Size<i32, Logical> = (1280, 800).into();
        // A busy fake desktop: a wallpaper gradient and two bright windows.
        let (w, h) = (1280usize, 800usize);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let mut px = [40 + (x * 90 / w) as u8, 60 + (y * 80 / h) as u8, 120, 255];
                if (100..700).contains(&x) && (120..560).contains(&y) { px = [235, 235, 240, 255]; }
                if (640..1180).contains(&x) && (300..720).contains(&y) { px = [30, 30, 34, 255]; }
                rgba.extend(px);
            }
        }
        let now = crate::ui::theme::LocalTime::now();
        let utc_hour = f64::from(now.hour) - now.utc_offset as f64 / 3600.0;
        for (name, noon_offset_hours) in [("day", 0.0), ("night", 12.0)] {
            let longitude = ((12.0 - utc_hour - noon_offset_hours) * 15.0 + 540.0).rem_euclid(360.0) - 180.0;
            let mut config = crate::config::ThemeConfig::default();
            config.latitude = Some(10.0);
            config.longitude = Some(longitude);
            let (tx, _rx) = mpsc::channel();
            let mut lock = Lock::default();
            lock.internal = true;
            let mut ui = Greeter::for_lock(tx, crate::greeter::User { name: "kirill".into(), display: "Kirill".into() });
            ui.settle_intro();
            lock.ui = Some(ui);
            let (btx, _brx) = channel::channel();
            lock.backdrop.request(btx);
            let (bw, bh, argb) = backdrop::blur(&rgba, w, h).unwrap();
            assert!(lock.backdrop.accept(backdrop::Blurred { epoch: lock.backdrop.epoch(), output: "t".into(), width: bw as i32, height: bh as i32, argb }));
            let mut elements = lock.media_elements(
                &mut renderer, size, 1.0,
                &crate::media::Snapshot { player: Some(crate::media::ui::artifacts::player()), ..Default::default() },
                "Adwaita",
            );
            assert!(!elements.is_empty(), "media controls must be drawn when music plays");
            elements.extend(lock.elements(&mut renderer, "t", size, 1.0, Duration::from_secs(1), &config, Some((73, false))));
            assert!(lock.backdrop.buffer("t").is_some());
            if let Some(dir) = std::env::var_os("GAVNO_LOCK_TEST_ARTIFACTS") {
                let dir = std::path::PathBuf::from(dir);
                std::fs::create_dir_all(&dir).unwrap();
                let bytes = crate::render::capture_elements(&mut renderer, &elements, (1280, 800), 1.0).unwrap();
                let mut encoder = png::Encoder::new(std::fs::File::create(dir.join(format!("lock-{name}.png"))).unwrap(), 1280, 800);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header().unwrap().write_image_data(&bytes).unwrap();
            }
        }
    }
    #[test]
    fn sleep_waits_for_presented_frames_and_resume_rearms_inhibitor() {
        let mut h = crate::state::tests::Harness::new();
        let output = output();
        h.gv.space.map_output(&output, (0, 0));
        let (tx, rx) = mpsc::channel();
        h.gv.lock.logind = Some(tx);
        h.gv.logind_signal(logind::Signal::Sleep(true));
        assert!(h.gv.protocols.locks.locked);
        assert_eq!(rx.try_recv().unwrap(), logind::Command::Hint(true));
        h.gv.protocols.locks.presented(&output);
        h.gv.refresh_internal_lock();
        assert!(rx.try_recv().is_err());
        h.gv.protocols.locks.submitted(&output);
        h.gv.refresh_internal_lock();
        assert!(rx.try_recv().is_err());
        h.gv.protocols.locks.presented(&output);
        h.gv.refresh_internal_lock();
        assert_eq!(rx.try_recv().unwrap(), logind::Command::Secured);
        h.gv.refresh_internal_lock();
        assert!(rx.try_recv().is_err());
        h.gv.logind_signal(logind::Signal::Sleep(false));
        assert_eq!(rx.try_recv().unwrap(), logind::Command::Resume);
        assert!(h.gv.protocols.locks.locked);
    }
    #[test]
    fn suspend_invalidates_pending_auth_and_previously_presented_frames() {
        let mut h = crate::state::tests::Harness::new();
        let output = output();
        h.gv.space.map_output(&output, (0, 0));
        h.gv.lock_session();
        let old_generation = h.gv.lock.generation;
        h.gv.protocols.locks.submitted(&output);
        h.gv.protocols.locks.presented(&output);
        assert!(h.gv.protocols.locks.secured());
        let (tx, rx) = mpsc::channel();
        h.gv.lock.logind = Some(tx);
        h.gv.logind_signal(logind::Signal::Sleep(true));
        assert_ne!(h.gv.lock.generation, old_generation);
        assert!(!h.gv.protocols.locks.secured());
        assert!(rx.try_recv().is_err());
        h.gv.protocols.locks.presented(&output);
        h.gv.refresh_internal_lock();
        assert!(rx.try_recv().is_err());
        h.gv.logind_signal(logind::Signal::Sleep(false));
        assert_eq!(rx.try_recv().unwrap(), logind::Command::Resume);
        h.gv.lock_auth_reply(old_generation, true);
        h.gv.refresh_internal_lock();
        assert!(!h.gv.lock.authenticated);
        assert!(h.gv.protocols.locks.locked);
    }
    #[test]
    fn power_menu_lock_uses_the_internal_gate() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.bar_action(
            &output(),
            crate::topbar::BarAction::Power(crate::topbar::PowerAction::Lock),
        );
        assert!(h.gv.protocols.locks.locked);
        assert!(h.gv.lock.internal());
    }
    #[test]
    fn idle_reuses_coalesced_activity_and_stops_while_inhibited_or_locked() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.refresh_lock_idle(false, false);
        assert!(h.gv.lock.idle_timer.is_some());
        let generation = h.gv.lock.idle_generation;
        h.gv.refresh_lock_idle(false, false);
        assert_eq!(h.gv.lock.idle_generation, generation);
        h.gv.refresh_lock_idle(true, false);
        assert_ne!(h.gv.lock.idle_generation, generation);
        h.gv.refresh_lock_idle(false, true);
        assert!(h.gv.lock.idle_timer.is_none());
        h.gv.refresh_lock_idle(false, false);
        assert!(h.gv.lock.idle_timer.is_some());
        h.gv.config.lock.after_idle_secs = 0;
        h.gv.refresh_lock_idle(false, false);
        assert!(h.gv.lock.idle_timer.is_none());
        h.gv.config.lock.after_idle_secs = 1;
        h.gv.refresh_lock_idle(false, false);
        assert!(h.gv.lock.idle_timer.is_some());
        h.gv.lock_session();
        assert!(h.gv.lock.idle_timer.is_none());
    }
    #[test]
    fn ui_panic_and_worker_failure_cannot_unlock() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.lock_session();
        h.gv.lock.ui_call::<()>(|_| panic!("mock UI failure"));
        assert!(h.gv.lock.failed);
        h.gv.lock_auth_result(true);
        h.gv.refresh_internal_lock();
        assert!(h.gv.protocols.locks.locked);
        assert!(!h.gv.lock.is_animating());
    }
    #[test]
    fn backoff_blocks_input_and_auth_success_alone_keeps_gate_closed() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.lock_session();
        h.gv.lock_auth_result(false);
        assert!(!h.gv.lock.accepts_input());
        h.gv.lock.blocked_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(h.gv.lock.accepts_input());
        h.gv.lock_auth_result(true);
        h.gv.refresh_internal_lock();
        assert!(h.gv.protocols.locks.locked);
        assert!(h.gv.lock.desktop_visible());
    }
    #[test]
    fn powered_off_lock_confirms_only_successful_drm_power_down_and_resume_wakes() {
        let mut h = crate::state::tests::Harness::new();
        let output = output();
        h.gv.space.map_output(&output, (0, 0));
        let (tx, rx) = mpsc::channel();
        h.gv.lock.logind = Some(tx);
        h.gv.power_off_monitors("test");
        h.gv.logind_signal(logind::Signal::Sleep(true));
        assert!(h.gv.power.is_off());
        assert!(h.gv.lock.minute_timer.is_none());
        assert_eq!(rx.try_recv().unwrap(), logind::Command::Hint(true));
        assert!(rx.try_recv().is_err(), "a failed/unknown power-off cannot confirm security");
        // Simulate the successful clear recorded by the DRM backend.
        h.gv.power.secured_outputs.push(output);
        h.gv.refresh_internal_lock();
        assert_eq!(rx.try_recv().unwrap(), logind::Command::Secured);
        h.gv.logind_signal(logind::Signal::Sleep(false));
        assert_eq!(rx.try_recv().unwrap(), logind::Command::Resume);
        assert!(!h.gv.power.is_off());
        assert!(h.gv.lock.minute_timer.is_some());
        assert!(h.gv.protocols.locks.locked);
    }

}

#[cfg(test)]
#[path = "lock/protocol_tests.rs"]
mod protocol_tests;
