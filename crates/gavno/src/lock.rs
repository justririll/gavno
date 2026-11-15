//! Built-in session lock. The protocol gate is set before any fallible UI work.
mod auth;
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
    authenticated: bool,
    failed: bool,
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
    pub fn internal(&self) -> bool {
        self.internal
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
    /// A UI panic permanently poisons this lock instance. Its caller still sees locked=true.
    fn ui_call<T>(&mut self, f: impl FnOnce(&mut Greeter) -> T) -> Option<T> {
        if self.failed {
            return None;
        }
        let ui = self.ui.as_mut()?;
        match catch_unwind(AssertUnwindSafe(|| f(ui))) {
            Ok(value) => Some(value),
            Err(_) => {
                self.failed = true;
                self.authenticated = false;
                self.ui = None;
                tracing::error!("lock UI failed; retaining solid-color lock");
                None
            }
        }
    }
    pub fn is_animating(&self) -> bool {
        self.internal && !self.failed && self.ui.as_ref().is_some_and(Greeter::is_animating)
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
    ) -> Vec<GavnoRenderElement> {
        let elements = self
            .ui_call(|ui| ui.elements(renderer, size, scale, now))
            .unwrap_or_default();
        if elements.is_empty() {
            // Shader initialization failure is reported without a panic. Stop
            // requesting animation frames and retain the same closed gate.
            self.failed = true;
            self.authenticated = false;
            self.ui = None;
        }
        elements
    }
    pub fn key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        if self.accepts_input() {
            self.ui_call(|ui| ui.key(sym, ch, mods));
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
        self.lock.authenticated = false;
        self.lock.failed = false;
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
        self.topbar.menu = None;
        self.dock.close_menu();
        self.create_lock_ui();
        self.update_lock_modifiers();
        self.schedule_lock_minute();
        self.refresh_internal_lock();
        self.queue_redraw_all();
        true
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
    fn schedule_lock_minute(&mut self) {
        if let Some(token) = self.lock.minute_timer.take() {
            self.loop_handle.remove(token);
        }
        let time = crate::ui::theme::LocalTime::now();
        let delay =
            Duration::from_secs(60 - u64::from(time.second) % 60) + Duration::from_millis(20);
        self.lock.minute_timer = self
            .loop_handle
            .insert_source(Timer::from_duration(delay), |_, _, gv| {
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
                    gv.lock.idle_timer = None;
                    // Evaluate inhibitor changes from this same event-loop batch first.
                    gv.loop_handle.insert_idle(move |gv| {
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
            }
        }
    }
    pub(crate) fn lock_pointer(&mut self, click: bool) {
        let pos = self.pointer_location();
        let origin = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .find(|geo| geo.to_f64().contains(pos))
            .map(|geo| geo.loc);
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
}

#[cfg(test)]
#[path = "lock/protocol_tests.rs"]
mod protocol_tests;
