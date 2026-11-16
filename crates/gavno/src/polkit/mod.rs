//! Built-in polkit authentication agent: a D-Bus worker receives requests and
//! the compositor draws the password dialog itself, so no resident GTK agent
//! process is needed and the dialog shows in the next frame.
mod dbus;
pub mod helper;
pub mod ui;
#[cfg(test)]
pub(crate) mod tests;

use crate::{module::Feature, Gavno};
use smithay::{
    input::keyboard::{keysyms, Keysym, ModifiersState},
    output::Output,
    reexports::calloop::channel,
    utils::{Logical, Point},
};
use std::{collections::VecDeque, path::PathBuf, sync::{mpsc, Arc}};
use zeroize::Zeroizing;

pub use dbus::Target;

/// Wrong passwords allowed before the request fails.
pub const MAX_ATTEMPTS: u8 = 3;
/// Longest password the dialog accepts, in characters.
const MAX_PASSWORD: usize = 256;

pub enum Outcome {
    Success,
    Cancelled,
    Failed(String),
}

pub struct Begin {
    pub cookie: String,
    pub message: String,
    pub icon: String,
    pub users: Vec<String>,
    pub selected: usize,
    pub reply: async_channel::Sender<Outcome>,
}

pub enum Request {
    Begin(Begin),
    Cancel(String),
}

pub struct Dialog {
    pub begin: Begin,
    pub password: Zeroizing<String>,
    pub error: Option<String>,
    pub attempts: u8,
    /// Set while the PAM helper runs.
    pub helper: Option<Arc<helper::Helper>>,
    serial: u64,
}

impl Dialog {
    pub fn busy(&self) -> bool {
        self.helper.is_some()
    }
    pub fn user(&self) -> &str {
        self.begin.users.get(self.begin.selected).map(String::as_str).unwrap_or("")
    }
    fn finish(self, outcome: Outcome) {
        if let Some(helper) = &self.helper {
            helper.cancel();
        }
        let _ = self.begin.reply.try_send(outcome);
    }
}

#[derive(Default)]
pub struct Polkit {
    /// The front request is the visible dialog; the rest wait in order.
    pub queue: VecDeque<Dialog>,
    /// Output the dialog is shown on.
    pub output: Option<Output>,
    stop: Option<mpsc::Sender<()>>,
    started: bool,
    serial: u64,
    /// Tests run a fake PAM helper.
    pub(crate) helper_override: Option<PathBuf>,
    pub view: ui::View,
}

impl Gavno {
    /// Registers the built-in agent for this session.
    pub fn start_polkit(&mut self, target: Target) {
        if self.polkit.started || self.greeter.is_some() || !self.features.on(Feature::Polkit) {
            return;
        }
        let (tx, rx) = channel::channel();
        if let Err(error) = self.loop_handle.insert_source(rx, |event, _, gv| {
            let _stall = crate::perf::Stall::new("dbus.polkit");
            if let channel::Event::Msg(request) = event {
                gv.guard(Feature::Polkit, (), |gv| gv.polkit_request(request));
            }
        }) {
            tracing::warn!(%error, "cannot register the polkit channel");
            return;
        }
        self.polkit.started = true;
        let _startup = crate::perf::Startup::new("dbus.polkit.spawn.begin", "dbus.polkit.spawn.end");
        self.polkit.stop = dbus::spawn(tx, target);
    }

    pub fn polkit_visible(&self) -> bool {
        self.features.on(Feature::Polkit)
            && !self.polkit.queue.is_empty()
            && !self.protocols.locks.locked
            && !self.lock.internal()
            && self.greeter.is_none()
    }

    fn polkit_request(&mut self, request: Request) {
        match request {
            Request::Begin(begin) => {
                if self.polkit.queue.iter().any(|d| d.begin.cookie == begin.cookie) {
                    let _ = begin.reply.try_send(Outcome::Failed("Duplicate authentication cookie".into()));
                    return;
                }
                self.polkit.serial += 1;
                let dialog = Dialog {
                    begin,
                    password: Zeroizing::new(String::new()),
                    error: None,
                    attempts: 0,
                    helper: None,
                    serial: self.polkit.serial,
                };
                if self.polkit.queue.is_empty() {
                    self.polkit.output = self.active_output();
                }
                self.polkit.queue.push_back(dialog);
            }
            Request::Cancel(cookie) => {
                if let Some(index) = self.polkit.queue.iter().position(|d| d.begin.cookie == cookie) {
                    if let Some(dialog) = self.polkit.queue.remove(index) {
                        dialog.finish(Outcome::Cancelled);
                    }
                    self.polkit_front_changed();
                }
            }
        }
        self.queue_redraw_all();
    }

    fn polkit_front_changed(&mut self) {
        if !self.polkit.queue.is_empty() {
            self.polkit.output = self.active_output();
        }
    }

    fn polkit_finish(&mut self, outcome: Outcome) {
        if let Some(dialog) = self.polkit.queue.pop_front() {
            dialog.finish(outcome);
        }
        self.polkit_front_changed();
        self.queue_redraw_all();
    }

    /// Keys while the dialog is shown. Every key is consumed.
    pub fn polkit_key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        self.guard(Feature::Polkit, (), |gv| {
            let Some(dialog) = gv.polkit.queue.front_mut() else { return };
            match sym.raw() {
                keysyms::KEY_Escape => {
                    gv.polkit_finish(Outcome::Cancelled);
                    return;
                }
                _ if dialog.busy() => return,
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                    gv.polkit_submit();
                    return;
                }
                keysyms::KEY_BackSpace if mods.ctrl => dialog.password.clear(),
                keysyms::KEY_BackSpace => {
                    dialog.password.pop();
                }
                keysyms::KEY_u | keysyms::KEY_U if mods.ctrl => dialog.password.clear(),
                keysyms::KEY_Tab | keysyms::KEY_Down => dialog.cycle_user(1),
                keysyms::KEY_ISO_Left_Tab | keysyms::KEY_Up => dialog.cycle_user(-1),
                _ => match ch {
                    Some(c) if !c.is_control() && !mods.ctrl && !mods.alt && !mods.logo => {
                        if dialog.password.chars().count() < MAX_PASSWORD {
                            dialog.password.push(c);
                            dialog.error = None;
                        }
                    }
                    _ => return,
                },
            }
            gv.queue_redraw_all();
        });
    }

    /// Clicks while the dialog is shown; the dialog is modal, so every click is
    /// consumed. Returns false when no dialog is visible.
    pub fn polkit_button(&mut self, pos: Point<f64, Logical>, pressed: bool) -> bool {
        if !self.polkit_visible() {
            return false;
        }
        if !pressed {
            return true;
        }
        self.guard(Feature::Polkit, (), |gv| {
            let local = gv
                .polkit
                .output
                .as_ref()
                .and_then(|o| gv.space.output_geometry(o))
                .map(|g| pos - g.loc.to_f64())
                .unwrap_or(pos);
            let Some(hit) = gv.polkit.view.hit(local) else { return };
            let Some(dialog) = gv.polkit.queue.front_mut() else { return };
            match hit {
                ui::Hit::Cancel => gv.polkit_finish(Outcome::Cancelled),
                ui::Hit::Submit if !dialog.busy() => gv.polkit_submit(),
                ui::Hit::User(step) if !dialog.busy() => {
                    dialog.cycle_user(step);
                    gv.queue_redraw_all();
                }
                _ => {}
            }
        });
        true
    }

    fn polkit_submit(&mut self) {
        let path = self.polkit.helper_override.clone().or_else(helper::helper_path);
        let Some(dialog) = self.polkit.queue.front_mut() else { return };
        if dialog.busy() || dialog.password.is_empty() {
            return;
        }
        let Some(path) = path else {
            self.polkit_finish(Outcome::Failed("polkit-agent-helper-1 is not installed".into()));
            return;
        };
        let worker = Arc::new(helper::Helper::default());
        dialog.helper = Some(worker.clone());
        dialog.error = None;
        let serial = dialog.serial;
        let user = dialog.user().to_owned();
        let cookie = dialog.begin.cookie.clone();
        let password = Zeroizing::new(std::mem::take(&mut *dialog.password));
        let (tx, rx) = channel::channel();
        if self
            .loop_handle
            .insert_source(rx, move |event, _, gv| {
                if let channel::Event::Msg(result) = event {
                    gv.guard(Feature::Polkit, (), |gv| gv.polkit_result(serial, result));
                }
            })
            .is_err()
        {
            self.polkit_finish(Outcome::Failed("cannot wait for the polkit helper".into()));
            return;
        }
        // PAM may take seconds (fail delays); never block the compositor.
        let spawned = std::thread::Builder::new().name("polkit-helper".into()).spawn(move || {
            let _ = tx.send(worker.converse(&path, &user, &cookie, &password));
        });
        if spawned.is_err() {
            self.polkit_finish(Outcome::Failed("cannot start the polkit helper".into()));
            return;
        }
        self.queue_redraw_all();
    }

    fn polkit_result(&mut self, serial: u64, result: Result<bool, String>) {
        let Some(dialog) = self.polkit.queue.front_mut().filter(|d| d.serial == serial) else {
            return;
        };
        dialog.helper = None;
        match result {
            Ok(true) => self.polkit_finish(Outcome::Success),
            Ok(false) => {
                dialog.attempts += 1;
                dialog.password.clear();
                if dialog.attempts >= MAX_ATTEMPTS {
                    self.polkit_finish(Outcome::Failed("Too many failed attempts".into()));
                } else {
                    dialog.error = Some("Wrong password, try again".into());
                    self.queue_redraw_all();
                }
            }
            Err(error) => {
                tracing::warn!(%error, "polkit helper failed");
                self.polkit_finish(Outcome::Failed(error));
            }
        }
    }

    /// Feature reset: cancel every request, free the keyboard and stop the
    /// worker (it unregisters the agent).
    pub(crate) fn reset_polkit(&mut self) {
        for dialog in self.polkit.queue.drain(..) {
            dialog.finish(Outcome::Cancelled);
        }
        self.polkit.stop = None;
        self.polkit.started = false;
        self.polkit.output = None;
        self.polkit.view = Default::default();
        self.queue_redraw_all();
    }
}

impl Dialog {
    fn cycle_user(&mut self, step: i32) {
        let count = self.begin.users.len() as i32;
        if count > 1 {
            self.begin.selected = (self.begin.selected as i32 + step).rem_euclid(count) as usize;
            self.password.clear();
            self.error = None;
        }
    }
}
