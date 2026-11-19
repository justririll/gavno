//! Webcam finger pointer. Mod+Shift+M starts the `gavno-cam` helper with one
//! end of a private socketpair as its stdin; the helper tracks the index
//! finger and sends pointer messages (see `gavno_cam_proto`). Turning the
//! mode off terminates the helper. While it is off nothing exists: no
//! process, no event source, no descriptor, no timer.
//!
//! Messages enter the same synthetic input path as the debug IPC (without its
//! environment gate: the gate here is the private socket to our own child).

use std::{
    cell::Cell,
    io::Read,
    os::unix::net::UnixStream,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

use gavno_cam_proto::{LineReader, Message};
use smithay::{
    backend::input::{AxisSource, ButtonState},
    reexports::calloop::{
        channel,
        generic::Generic,
        timer::{TimeoutAction, Timer},
        Interest, Mode, PostAction, RegistrationToken,
    },
};

use crate::{ipc::debug_input, Gavno};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
/// Messages handled per wakeup; a flooding child cannot starve the loop.
const READ_BUDGET: usize = 8;

pub struct Finger {
    child: Option<Child>,
    token: Option<RegistrationToken>,
    hand: bool,
    left: bool,
    right: bool,
}

/// Helper path: `$GAVNO_CAM_BIN`, a sibling of this executable, then `PATH`.
fn helper() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("GAVNO_CAM_BIN") {
        return path.into();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("gavno-cam")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| "gavno-cam".into())
}

/// Helpers that were told to stop (or are exiting) and not yet reaped. A new
/// helper must not open the camera while an old one may still hold it.
static TERMINATING: AtomicUsize = AtomicUsize::new(0);
/// A restart is waiting for the old helper to go away.
static RESTART_PENDING: AtomicBool = AtomicBool::new(false);
/// A restart waits at most this many polls of `RESTART_POLL`.
const RESTART_POLLS: u32 = 60;
const RESTART_POLL: Duration = Duration::from_millis(50);

struct Reaped;
impl Drop for Reaped {
    fn drop(&mut self) {
        TERMINATING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// SIGTERM now (unless the child is already exiting on its own); a short-lived
/// thread reaps the child and kills it if it lingers, so the compositor thread
/// never waits. A failing exit status is sent to `report`.
fn terminate(mut child: Child, signal: bool, report: Option<channel::Sender<String>>) {
    if signal {
        // SAFETY: signalling our own child.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    }
    TERMINATING.fetch_add(1, Ordering::SeqCst);
    let reaper = std::thread::Builder::new().name("gavno-cam-reap".into()).spawn({
        let id = child.id();
        move || {
            let _guard = Reaped;
            for _ in 0..40 {
                match child.try_wait() {
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Ok(Some(status)) => {
                        if let (Some(report), Some(code)) = (report, status.code()) {
                            if code != 0 {
                                let _ = report.send(format!("gavno-cam exited with status {code}"));
                            }
                        }
                        return;
                    }
                    Err(_) => return,
                }
            }
            tracing::warn!(pid = id, "gavno-cam ignored SIGTERM; killing it");
            let _ = child.kill();
            let _ = child.wait();
        }
    });
    if let Err(error) = reaper {
        TERMINATING.fetch_sub(1, Ordering::SeqCst);
        tracing::warn!(%error, "cannot start the gavno-cam reaper");
    }
}

impl Gavno {
    pub fn finger_pointer_on(&self) -> bool {
        self.finger.is_some()
    }

    /// Why the mode may not run right now, if it may not.
    fn finger_blocked(&self) -> Option<&'static str> {
        if self.greeter.is_some() { Some("greeter running") }
        else if self.protocols.locks.locked || self.lock.internal() { Some("session locked") }
        else { None }
    }

    pub fn toggle_finger_pointer(&mut self) {
        if self.finger.is_none() && RESTART_PENDING.swap(false, Ordering::SeqCst) {
            // Toggled off again while the restart was still waiting.
            return;
        }
        if self.finger.is_some() {
            self.stop_finger_pointer();
        } else if let Err(error) = self.start_finger_pointer() {
            tracing::warn!(%error, "finger pointer not started");
        }
    }

    pub fn start_finger_pointer(&mut self) -> Result<(), String> {
        if self.finger.is_some() { return Ok(()); }
        if !self.config.pointer.camera { return Err("disabled by [pointer] camera = false".into()); }
        if let Some(reason) = self.finger_blocked() { return Err(reason.into()); }
        if self.polkit_visible() { return Err("authorization dialog visible".into()); }
        if TERMINATING.load(Ordering::SeqCst) > 0 {
            return self.restart_finger_when_free();
        }
        self.start_finger_pointer_now()
    }

    /// `start_finger_pointer` without the wait for a dying helper.
    fn start_finger_pointer_now(&mut self) -> Result<(), String> {
        if self.finger.is_some() { return Ok(()); }
        if let Some(reason) = self.finger_blocked() { return Err(reason.into()); }
        let output = self.active_output().ok_or("no output")?;
        let size = output.current_mode().map(|m| m.size).ok_or("output has no mode")?;
        let mut command = Command::new(helper());
        let pointer = &self.config.pointer;
        command
            .env("GAVNO_CAM_FD", "0")
            .env("GAVNO_CAM_OUTPUT", format!("{}x{}", size.w, size.h))
            .env("GAVNO_CAM_BOX", pointer.active_box.to_string())
            .env("GAVNO_CAM_MIN_CUTOFF", pointer.min_cutoff.to_string())
            .env("GAVNO_CAM_BETA", pointer.beta.to_string());
        self.spawn_finger_helper(command)
    }

    /// The previous helper may still hold the camera: poll with a one-shot
    /// timer (bounded) and start once it is reaped.
    fn restart_finger_when_free(&mut self) -> Result<(), String> {
        if RESTART_PENDING.swap(true, Ordering::SeqCst) { return Ok(()); }
        let mut polls = 0;
        let timer = self.loop_handle.insert_source(Timer::from_duration(RESTART_POLL), move |_, _, gv| {
            if !RESTART_PENDING.load(Ordering::SeqCst) { return TimeoutAction::Drop; }
            polls += 1;
            if TERMINATING.load(Ordering::SeqCst) > 0 && polls < RESTART_POLLS {
                return TimeoutAction::ToDuration(RESTART_POLL);
            }
            RESTART_PENDING.store(false, Ordering::SeqCst);
            // `TERMINATING` may still be set after the bound: start anyway.
            if gv.finger.is_none() {
                if let Err(error) = gv.start_finger_pointer_now() {
                    tracing::warn!(%error, "finger pointer not started");
                }
            }
            TimeoutAction::Drop
        });
        if let Err(error) = timer {
            RESTART_PENDING.store(false, Ordering::SeqCst);
            return Err(format!("cannot schedule restart: {error}"));
        }
        Ok(())
    }

    /// Starts `command` with a fresh socketpair as stdin and watches the other end.
    fn spawn_finger_helper(&mut self, mut command: Command) -> Result<(), String> {
        let (parent, child_end) = UnixStream::pair().map_err(|e| e.to_string())?;
        parent.set_nonblocking(true).map_err(|e| e.to_string())?;
        // posix_spawn file actions place the socket on fd 0; no pre_exec fork.
        command.stdin(Stdio::from(std::os::fd::OwnedFd::from(child_end))).stdout(Stdio::null());
        let child = command.spawn().map_err(|e| format!("cannot start gavno-cam: {e}"))?;
        let mut reader = LineReader::default();
        let token = self.loop_handle.insert_source(Generic::new(parent, Interest::READ, Mode::Level), move |_, stream, gv| {
            let mut stream = &**stream;
            let mut chunk = [0u8; 512];
            let mut messages = Vec::new();
            let mut closed = false;
            for _ in 0..READ_BUDGET {
                match stream.read(&mut chunk) {
                    Ok(0) => { closed = true; break; }
                    Ok(n) => reader.feed(&chunk[..n], |m| messages.push(m)),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => { closed = true; break; }
                }
            }
            for message in messages {
                if gv.finger_message(message) { return Ok(PostAction::Remove); }
            }
            if closed {
                // The helper exited by itself (no hand for 60 s, camera error).
                // This source is dropped by the return value; do not remove it twice.
                gv.finger_helper_exited();
                return Ok(PostAction::Remove);
            }
            Ok(PostAction::Continue)
        });
        match token {
            Ok(token) => {
                self.finger = Some(Finger { child: Some(child), token: Some(token), hand: false, left: false, right: false });
                self.set_camera_indicator(Some(false));
                Ok(())
            }
            Err(error) => {
                terminate(child, true, None);
                Err(format!("cannot watch gavno-cam: {error}"))
            }
        }
    }

    /// The helper closed its socket: end the mode and, when it failed, say so.
    fn finger_helper_exited(&mut self) {
        let Some(finger) = self.finger.as_mut() else { return };
        finger.token = None;
        let child = finger.child.take();
        self.stop_finger_pointer();
        let Some(child) = child else { return };
        let (tx, rx) = channel::channel::<String>();
        let token = Rc::new(Cell::new(None));
        let slot = token.clone();
        let inserted = self.loop_handle.insert_source(rx, move |event, _, gv| {
            match event {
                channel::Event::Msg(error) => {
                    tracing::warn!(%error, "finger pointer helper failed");
                    gv.camera_notification("Camera unavailable", "The finger pointer could not use the camera.");
                    if let Some(token) = slot.take() { gv.loop_handle.remove(token); }
                }
                channel::Event::Closed => {
                    if let Some(token) = slot.take() { gv.loop_handle.remove(token); }
                }
            }
        });
        match inserted {
            Ok(t) => { token.set(Some(t)); terminate(child, false, Some(tx)); }
            Err(_) => terminate(child, false, None),
        }
    }

    /// Ends the mode: releases held buttons, drops the socket and stops the helper.
    pub fn stop_finger_pointer(&mut self) {
        let Some(mut finger) = self.finger.take() else { return };
        if let Some(token) = finger.token.take() { self.loop_handle.remove(token); }
        if self.finger_blocked().is_none() {
            if finger.left { debug_input::button(self, BTN_LEFT, ButtonState::Released); }
            if finger.right { debug_input::button(self, BTN_RIGHT, ButtonState::Released); }
        }
        if let Some(child) = finger.child.take() { terminate(child, true, None); }
        self.set_camera_indicator(None);
    }

    /// The bar and the Classic taskbar show the camera icon while it runs.
    fn set_camera_indicator(&mut self, state: Option<bool>) {
        self.topbar.camera = state;
        self.classic.taskbar.camera = state;
        self.queue_redraw_all();
    }

    /// A reload that disables `[pointer] camera` stops a running helper.
    pub(crate) fn finger_config_changed(&mut self) {
        if !self.config.pointer.camera { self.stop_finger_pointer(); }
    }

    /// Applies one message. Returns true when it ended the mode, in which case
    /// the calling source callback must return `PostAction::Remove`.
    pub(crate) fn finger_message(&mut self, message: Message) -> bool {
        let Some(finger) = self.finger.as_ref() else { return false };
        if self.finger_blocked().is_some() {
            // Stopping from inside the source's own callback: clear the token
            // first and let the callback's `Remove` drop the source.
            if let Some(finger) = self.finger.as_mut() { finger.token = None; }
            self.stop_finger_pointer();
            return true;
        }
        // An authorization dialog must not be clicked by a stray hand; only
        // releases of buttons already held still pass.
        let modal = self.polkit_visible();
        match message {
            Message::Hand { present } => {
                if finger.hand != present {
                    self.finger.as_mut().unwrap().hand = present;
                    self.set_camera_indicator(Some(present));
                }
            }
            Message::Motion { x, y } if !modal => {
                let Some(geo) = self.active_output().and_then(|o| self.space.output_geometry(&o)) else { return false };
                let (x, y) = (f64::from(geo.loc.x) + f64::from(x) * f64::from(geo.size.w), f64::from(geo.loc.y) + f64::from(y) * f64::from(geo.size.h));
                debug_input::pointer_to(self, x, y);
            }
            Message::Button { right, pressed } => {
                let held = if right { finger.right } else { finger.left };
                if pressed == held || (pressed && modal) { return false; }
                let finger = self.finger.as_mut().unwrap();
                *(if right { &mut finger.right } else { &mut finger.left }) = pressed;
                let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
                debug_input::button(self, if right { BTN_RIGHT } else { BTN_LEFT }, state);
            }
            Message::Scroll { dy } if !modal => debug_input::scroll(self, 0.0, f64::from(dy), AxisSource::Wheel),
            Message::Motion { .. } | Message::Scroll { .. } => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::Harness;
    use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};

    /// A harness with one 1600x900 output at (100, 50).
    fn harness() -> Harness {
        let mut h = Harness::new();
        let output = Output::new("finger-test".into(), PhysicalProperties {
            size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "test".into(), model: "test".into(),
        });
        output.change_current_state(Some(Mode { size: (1600, 900).into(), refresh: 60_000 }), None, None, None);
        h.gv.space.map_output(&output, (100, 50));
        h.gv.workspaces.ensure(&output);
        h
    }
    fn dispatch(h: &mut Harness) {
        h.event_loop.dispatch(Some(Duration::from_millis(1)), &mut h.gv).unwrap();
    }

    fn fake(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn nothing_exists_while_the_mode_is_off() {
        let h = harness();
        assert!(h.gv.finger.is_none());
        assert!(h.gv.topbar.camera.is_none());
    }

    #[test]
    fn motion_buttons_and_scroll_reach_the_pointer_and_stop_releases() {
        let mut h = harness();
        h.gv.spawn_finger_helper(fake("sleep 30")).unwrap();
        assert_eq!(h.gv.topbar.camera, Some(false));
        let geo = h.gv.space.output_geometry(&h.gv.active_output().unwrap()).unwrap();
        h.gv.finger_message(Message::Hand { present: true });
        assert_eq!(h.gv.topbar.camera, Some(true));
        h.gv.finger_message(Message::Motion { x: 0.25, y: 0.5 });
        let pos = h.gv.pointer_location();
        assert!((pos.x - (f64::from(geo.loc.x) + f64::from(geo.size.w) * 0.25)).abs() < 1.0, "{pos:?}");
        assert!((pos.y - (f64::from(geo.loc.y) + f64::from(geo.size.h) * 0.5)).abs() < 1.0, "{pos:?}");
        h.gv.finger_message(Message::Button { right: false, pressed: true });
        assert!(h.gv.pointer_buttons.contains(&BTN_LEFT));
        // A duplicate press is ignored, scrolling does not disturb the button.
        h.gv.finger_message(Message::Button { right: false, pressed: true });
        h.gv.finger_message(Message::Scroll { dy: 12.0 });
        assert!(h.gv.pointer_buttons.contains(&BTN_LEFT));
        h.gv.stop_finger_pointer();
        assert!(h.gv.finger.is_none());
        assert!(h.gv.topbar.camera.is_none());
        assert!(!h.gv.pointer_buttons.contains(&BTN_LEFT), "stop must release the held button");
    }

    #[test]
    fn locked_session_stops_the_mode_and_refuses_to_start() {
        let mut h = harness();
        h.gv.spawn_finger_helper(fake("sleep 30")).unwrap();
        h.gv.protocols.locks.locked = true;
        h.gv.finger_message(Message::Motion { x: 0.1, y: 0.1 });
        assert!(h.gv.finger.is_none(), "a message while locked stops the helper");
        assert_eq!(h.gv.start_finger_pointer(), Err("session locked".into()));
        h.gv.protocols.locks.locked = false;
    }

    #[test]
    fn mod_shift_m_toggles_and_plain_mod_m_still_minimizes() {
        use smithay::input::keyboard::{keysyms, Keysym, ModifiersState};
        let mut h = harness();
        for mod_key in [crate::config::ModKey::Super, crate::config::ModKey::Alt] {
            h.gv.config.mod_key = mod_key;
            let mut mods = ModifiersState::default();
            match mod_key { crate::config::ModKey::Super => mods.logo = true, crate::config::ModKey::Alt => mods.alt = true }
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_m), &mods), Some(crate::input::Action::Minimize));
            mods.shift = true;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_M), &mods), Some(crate::input::Action::ToggleFingerPointer));
            h.gv.config.pointer.camera = false;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_M), &mods), None, "a disabled module frees the chord");
            h.gv.config.pointer.camera = true;
        }
    }

    #[test]
    fn config_can_disable_the_mode() {
        let mut h = harness();
        h.gv.config.pointer.camera = false;
        assert!(h.gv.start_finger_pointer().is_err());
        h.gv.config.pointer.camera = true;
        h.gv.spawn_finger_helper(fake("sleep 30")).unwrap();
        h.gv.config.pointer.camera = false;
        h.gv.finger_config_changed();
        assert!(h.gv.finger.is_none());
    }

    #[test]
    fn a_helper_that_exits_ends_the_mode_through_the_event_loop() {
        let mut h = harness();
        h.gv.spawn_finger_helper(fake("printf 'H 1\\n' >&0; exit 0")).unwrap();
        for _ in 0..200 {
            dispatch(&mut h);
            if h.gv.finger.is_none() { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(h.gv.finger.is_none(), "EOF on the socket must clear the mode");
        assert!(h.gv.topbar.camera.is_none());
    }

    #[test]
    fn messages_from_the_socket_are_applied() {
        let mut h = harness();
        h.gv.spawn_finger_helper(fake("printf 'H 1\\nM 0.5 0.5\\nB r 1\\n' >&0; sleep 30")).unwrap();
        for _ in 0..200 {
            dispatch(&mut h);
            if h.gv.pointer_buttons.contains(&BTN_RIGHT) { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(h.gv.topbar.camera, Some(true));
        assert!(h.gv.pointer_buttons.contains(&BTN_RIGHT));
        h.gv.stop_finger_pointer();
    }
}
