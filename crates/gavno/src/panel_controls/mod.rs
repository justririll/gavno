//! Shared Wi-Fi, Bluetooth and audio popovers for both shells.
pub mod audio;
pub mod bluetooth;
mod fake;
#[cfg(test)]
mod tests;
mod ui;
pub mod wifi;
mod worker;
use crate::{topbar::geometry::Rect, Gavno};
use smithay::{
    input::keyboard::{keysyms, Keysym, ModifiersState},
    output::Output,
    utils::{Logical, Point},
};
use zeroize::{Zeroize, Zeroizing};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Wifi,
    Bluetooth,
    Audio,
}
/// Never derive Debug for secret-bearing data, including worker commands.
#[derive(Default)]
pub struct Secret(Zeroizing<String>);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}
impl Secret {
    fn push(&mut self, ch: char) {
        if !ch.is_control() && self.0.len() < 256 {
            self.0.push(ch);
        }
    }
    fn backspace(&mut self) {
        if let Some((index, _)) = self.0.char_indices().next_back() {
            self.0[index..].zeroize();
            self.0.truncate(index);
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    WifiPower,
    Disconnect,
    Network(String),
    BluetoothPower,
    Search,
    Device(String),
    Slider(audio::Target),
    Output(String),
}
#[derive(Clone)]
struct Row {
    label: String,
    detail: String,
    icon: String,
    action: Action,
    level: Option<u8>,
    muted: bool,
}
#[derive(Default)]
pub struct State {
    pub wifi: wifi::Snapshot,
    pub bluetooth: bluetooth::Snapshot,
    pub audio: audio::Snapshot,
    errors: [Option<String>; 3],
    workers: [Option<worker::Worker>; 3],
    started: bool,
    pub event_token: Option<smithay::reexports::calloop::RegistrationToken>,
    pub open: Option<(Kind, Output)>,
    selected: usize,
    offset: usize,
    password: Option<(String, Secret)>,
    view: ui::View,
    drag: Option<(audio::Target, Rect)>,
}
fn index(kind: Kind) -> usize {
    match kind {
        Kind::Wifi => 0,
        Kind::Bluetooth => 1,
        Kind::Audio => 2,
    }
}
impl State {
    fn send(&self, kind: Kind, command: worker::Command) {
        if let Some(w) = &self.workers[index(kind)] {
            w.send(command);
        }
    }
    pub(crate) fn master_step(&mut self, delta: f64) -> bool {
        let Some(output) = self.audio.output.as_ref() else {
            return false;
        };
        if delta != 0.0 {
            if delta < 0.0 && output.muted {
                self.send(Kind::Audio, worker::Command::Mute(audio::Target::Output));
            }
            let value = if delta < 0.0 {
                output.volume.saturating_add(5).min(100)
            } else {
                output.volume.saturating_sub(5)
            };
            self.adjust(audio::Target::Output, value);
        }
        true
    }
    pub fn release(&mut self) {
        self.drag = None;
    }
    pub fn close(&mut self) {
        if let Some((kind, _)) = self.open.take() {
            self.send(kind, worker::Command::Visible(false));
        }
        self.password = None;
        self.drag = None;
        self.view.hits.clear();
    }
    fn rows(&self, kind: Kind) -> Vec<Row> {
        let row = |label: &str, detail: String, icon: &str, action| Row {
            label: label.into(),
            detail,
            icon: icon.into(),
            action,
            level: None,
            muted: false,
        };
        match kind {
            Kind::Wifi => {
                let mut rows = vec![row(
                    "Wi-Fi",
                    if !self.wifi.hardware {
                        "Blocked by hardware rfkill"
                    } else if self.wifi.enabled {
                        "On"
                    } else {
                        "Off"
                    }
                    .into(),
                    self.wifi.icon(),
                    Action::WifiPower,
                )];
                if self.wifi.enabled {
                    if let Some(n) = self.wifi.networks.iter().find(|n| n.active) {
                        rows.push(row(
                            "Disconnect",
                            format!("{} · {}% signal", n.label(), n.strength),
                            "network-wireless-symbolic",
                            Action::Disconnect,
                        ));
                    }
                    for n in &self.wifi.networks {
                        rows.push(row(
                            &n.label(),
                            format!(
                                "{}% signal{}{}{}",
                                n.strength,
                                if n.secured { " · Locked" } else { " · Open" },
                                if n.active { " · Connected" } else { "" },
                                if n.saved.is_some() { " · Saved" } else { "" }
                            ),
                            crate::topbar::status::strength_icon(n.strength),
                            Action::Network(n.path.clone()),
                        ));
                    }
                }
                rows
            }
            Kind::Bluetooth => {
                let mut rows = vec![
                    row(
                        "Bluetooth",
                        if self.bluetooth.blocked {
                            "Blocked by rfkill"
                        } else if self.bluetooth.powered {
                            "On"
                        } else {
                            "Off"
                        }
                        .into(),
                        self.bluetooth.icon(),
                        Action::BluetoothPower,
                    ),
                    row(
                        "Search",
                        if self.bluetooth.discovering {
                            "Searching · up to 30 seconds"
                        } else {
                            "Find nearby devices"
                        }
                        .into(),
                        "system-search-symbolic",
                        Action::Search,
                    ),
                ];
                for d in &self.bluetooth.devices {
                    rows.push(row(
                        &d.name,
                        format!(
                            "{}{}",
                            if d.connected {
                                "Connected"
                            } else if d.paired {
                                "Paired · Connect"
                            } else {
                                "New · Pair and connect"
                            },
                            d.battery
                                .map(|b| format!(" · {b}% battery"))
                                .unwrap_or_default()
                        ),
                        &d.icon,
                        Action::Device(d.path.clone()),
                    ));
                }
                rows
            }
            Kind::Audio => {
                let mut rows = Vec::new();
                for (name, endpoint, target, icon) in [
                    (
                        "Output",
                        self.audio.output.as_ref(),
                        audio::Target::Output,
                        "audio-speakers-symbolic",
                    ),
                    (
                        "Input",
                        self.audio.input.as_ref(),
                        audio::Target::Input,
                        "audio-input-microphone-symbolic",
                    ),
                ] {
                    if let Some(e) = endpoint {
                        rows.push(Row {
                            label: format!("{name} · {}", e.name),
                            detail: String::new(),
                            icon: icon.into(),
                            action: Action::Slider(target),
                            level: Some(e.volume),
                            muted: e.muted,
                        });
                    }
                }
                for e in &self.audio.sinks {
                    rows.push(row(
                        &e.name,
                        if self.audio.output.as_ref().is_some_and(|o| o.id == e.id) {
                            "Default output"
                        } else {
                            "Use output and move streams"
                        }
                        .into(),
                        "audio-card-symbolic",
                        Action::Output(e.id.clone()),
                    ));
                }
                for e in &self.audio.streams {
                    rows.push(Row {
                        label: e.name.clone(),
                        detail: String::new(),
                        icon: if e.icon.is_empty() {
                            "application-x-executable-symbolic".into()
                        } else {
                            e.icon.clone()
                        },
                        action: Action::Slider(audio::Target::Stream(e.id.clone())),
                        level: Some(e.volume),
                        muted: e.muted,
                    });
                }
                rows
            }
        }
    }
    fn apply_event(&mut self, event: worker::Event) -> bool {
        let before = (
            self.wifi.icon(),
            self.bluetooth.icon(),
            self.bluetooth.battery(),
            self.audio.output.as_ref().map(|e| (e.volume, e.muted)),
        );
        let old_error = self.errors[index(event.kind)].clone();
        let changed = match event.snapshot {
            Some(worker::Snapshot::Wifi(s)) => {
                let changed = self.wifi != s;
                self.wifi = s;
                changed
            }
            Some(worker::Snapshot::Bluetooth(s)) => {
                let changed = self.bluetooth != s;
                self.bluetooth = s;
                changed
            }
            Some(worker::Snapshot::Audio(s)) => {
                let changed = self.audio != s;
                self.audio = s;
                changed
            }
            None => false,
        };
        self.errors[index(event.kind)] = event.error.or_else(|| {
            if event.kind == Kind::Wifi {
                self.wifi.failure.clone()
            } else {
                None
            }
        });
        let after = (
            self.wifi.icon(),
            self.bluetooth.icon(),
            self.bluetooth.battery(),
            self.audio.output.as_ref().map(|e| (e.volume, e.muted)),
        );
        before != after
            || (self.open.as_ref().is_some_and(|(k, _)| *k == event.kind)
                && (changed || old_error != self.errors[index(event.kind)]))
    }
    fn activate(&mut self, action: Action) {
        match action {
            Action::WifiPower => self.send(Kind::Wifi, worker::Command::Power(!self.wifi.enabled)),
            Action::Disconnect => self.send(Kind::Wifi, worker::Command::Disconnect),
            Action::Network(path) => {
                if let Some(n) = self.wifi.networks.iter().find(|n| n.path == path) {
                    if n.active {
                        return;
                    }
                    if n.enterprise && n.saved.is_none() {
                        self.errors[0] = Some("Enterprise Wi-Fi requires a saved profile".into());
                    } else if n.secured && n.saved.is_none() && n.key_management != "owe" {
                        self.password = Some((path, Secret::default()));
                    } else {
                        self.send(
                            Kind::Wifi,
                            worker::Command::Connect(n.clone(), Secret::default()),
                        );
                    }
                }
            }
            Action::BluetoothPower => self.send(
                Kind::Bluetooth,
                worker::Command::Power(!self.bluetooth.powered),
            ),
            Action::Search => self.send(Kind::Bluetooth, worker::Command::Search),
            Action::Device(path) => {
                if let Some(d) = self.bluetooth.devices.iter().find(|d| d.path == path) {
                    self.send(Kind::Bluetooth, worker::Command::Device(d.clone()));
                }
            }
            Action::Slider(target) => self.send(Kind::Audio, worker::Command::Mute(target)),
            Action::Output(id) => self.send(Kind::Audio, worker::Command::Output(id)),
        }
    }
    fn adjust(&mut self, target: audio::Target, level: u8) {
        let e = match &target {
            audio::Target::Output => self.audio.output.as_mut(),
            audio::Target::Input => self.audio.input.as_mut(),
            audio::Target::Stream(id) => self.audio.streams.iter_mut().find(|e| &e.id == id),
        };
        if let Some(e) = e {
            if e.volume == level.min(100) {
                return;
            }
            e.volume = level.min(100);
        }
        self.send(Kind::Audio, worker::Command::Volume(target, level));
    }
    fn key(&mut self, sym: u32, ch: Option<char>) {
        if self.password.is_some() {
            match sym {
                keysyms::KEY_Escape => self.password = None,
                keysyms::KEY_BackSpace => {
                    if let Some((_, p)) = &mut self.password {
                        p.backspace();
                    }
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                    if let Some((path, password)) = self.password.take() {
                        if let Some(n) = self.wifi.networks.iter().find(|n| n.path == path) {
                            self.send(Kind::Wifi, worker::Command::Connect(n.clone(), password));
                        }
                    }
                }
                _ => {
                    if let (Some((_, p)), Some(ch)) = (&mut self.password, ch) {
                        p.push(ch);
                    }
                }
            }
            return;
        }
        if sym == keysyms::KEY_Escape {
            self.close();
            return;
        }
        let Some((kind, _)) = &self.open else {
            return;
        };
        let rows = self.rows(*kind);
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        match sym {
            keysyms::KEY_Down | keysyms::KEY_j | keysyms::KEY_Tab => {
                self.selected = (self.selected + 1).min(rows.len().saturating_sub(1))
            }
            keysyms::KEY_Up | keysyms::KEY_k | keysyms::KEY_ISO_Left_Tab => {
                self.selected = self.selected.saturating_sub(1)
            }
            keysyms::KEY_Left | keysyms::KEY_h | keysyms::KEY_Right | keysyms::KEY_l => {
                if let Some(row) = rows.get(self.selected) {
                    if let (Action::Slider(target), Some(value)) = (&row.action, row.level) {
                        self.adjust(
                            target.clone(),
                            if matches!(sym, keysyms::KEY_Left | keysyms::KEY_h) {
                                value.saturating_sub(5)
                            } else {
                                value.saturating_add(5).min(100)
                            },
                        );
                    } else if matches!(sym, keysyms::KEY_Left | keysyms::KEY_h) {
                        self.selected = self.selected.saturating_sub(1);
                    } else {
                        self.selected = (self.selected + 1).min(rows.len().saturating_sub(1));
                    }
                }
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                if let Some(row) = rows.get(self.selected) {
                    self.activate(row.action.clone());
                }
            }
            _ => {}
        }
    }
}
impl Gavno {
    fn panel_controls_event(&mut self, event: worker::Event) {
        let audio = matches!(&event.snapshot, Some(worker::Snapshot::Audio(_)));
        let redraw = self.topbar.controls.apply_event(event);
        if audio {
            let volume = self.topbar.controls.audio.output.as_ref().map(|e| {
                crate::topbar::status::Volume {
                    level: e.volume as f64 / 100.0,
                    muted: e.muted,
                }
            });
            self.update_panel_volume(volume);
        }
        if redraw {
            self.queue_redraw_all();
        }
    }
    pub(crate) fn start_panel_controls(&mut self) {
        if self.topbar.controls.started
            || !self.features.on(crate::module::Feature::Bar)
            || matches!(self.backend, crate::backend::Backend::None)
        {
            return;
        }
        self.topbar.controls.started = true;
        let (tx, rx) = smithay::reexports::calloop::channel::channel::<worker::Event>();
        let token = self.loop_handle.insert_source(rx, |event, _, gv| {
            if let smithay::reexports::calloop::channel::Event::Msg(event) = event {
                gv.panel_controls_event(event);
            }
        });
        match token {
            Ok(token) => self.topbar.controls.event_token = Some(token),
            Err(_) => return,
        }
        let fake = fake_enabled();
        for kind in [Kind::Wifi, Kind::Bluetooth, Kind::Audio] {
            self.topbar.controls.workers[index(kind)] = worker::spawn(kind, tx.clone(), fake);
        }
    }
    pub fn toggle_panel_controls(&mut self, kind: Kind, output: &Output) {
        self.start_panel_controls();
        let was_open = self
            .topbar
            .controls
            .open
            .as_ref()
            .is_some_and(|(k, o)| *k == kind && o == output);
        self.topbar.close();
        self.close_music();
        self.dock.close_menu();
        if !was_open {
            let state = &mut self.topbar.controls;
            state.open = Some((kind, output.clone()));
            state.selected = 0;
            state.offset = 0;
            state.send(kind, worker::Command::Visible(true));
        }
        self.cancel_shell_tap();
        self.sync_bar_hints();
        self.queue_redraw_all();
    }
    pub(crate) fn refresh_panel_controls_gate(&mut self) {
        let close = self
            .topbar
            .controls
            .open
            .as_ref()
            .is_some_and(|(_, output)| {
                !self.config.bar.enabled
                    || !self.features.on(crate::module::Feature::Bar)
                    || self.protocols.locks.locked
                    || self.lock.internal()
                    || self.greeter.is_some()
                    || self.power.is_off()
                    || !self.space.outputs().any(|o| o == output)
                    || !self.shell_visible(output)
            });
        if close {
            self.topbar.controls.close();
            self.queue_redraw_all();
        }
    }
    pub fn panel_controls_open(&self) -> bool {
        self.topbar.controls.open.is_some()
    }
    pub fn panel_controls_key(&mut self, sym: Keysym, ch: Option<char>, _mods: &ModifiersState, code: u32) {
        if matches!(
            sym.raw(),
            keysyms::KEY_XF86AudioRaiseVolume
                | keysyms::KEY_XF86AudioLowerVolume
                | keysyms::KEY_XF86AudioMute
                | keysyms::KEY_XF86AudioMicMute
                | keysyms::KEY_XF86MonBrightnessUp
                | keysyms::KEY_XF86MonBrightnessDown
                | keysyms::KEY_XF86AudioPlay
                | keysyms::KEY_XF86AudioPause
                | keysyms::KEY_XF86AudioNext
                | keysyms::KEY_XF86AudioPrev
        ) {
            self.bar_key(sym.raw(), code, true);
            return;
        }
        self.topbar.controls.key(sym.raw(), ch);
        self.cancel_shell_tap();
        self.sync_bar_hints();
        self.queue_redraw_all();
    }
    pub fn panel_controls_button(&mut self, point: Point<f64, Logical>, pressed: bool) -> bool {
        let Some((_, output)) = self.topbar.controls.open.clone() else {
            return false;
        };
        if !pressed {
            self.topbar.controls.drag = None;
            return true;
        }
        let Some(geo) = self.space.output_geometry(&output) else {
            self.topbar.controls.close();
            return true;
        };
        let local = point - geo.loc.to_f64();
        if self.topbar.controls.view.close.to_f64().contains(local) {
            self.topbar.controls.close();
            self.sync_bar_hints();
            self.queue_redraw_all();
            return true;
        }
        let hit = self
            .topbar
            .controls
            .view
            .hits
            .iter()
            .rev()
            .find(|(r, _, _)| r.to_f64().contains(local))
            .cloned();
        if let Some((area, action, slider)) = hit {
            if let Some((kind, _)) = self.topbar.controls.open.as_ref() {
                if let Some(selected) = self
                    .topbar
                    .controls
                    .rows(*kind)
                    .iter()
                    .position(|row| row.action == action)
                {
                    self.topbar.controls.selected = selected;
                }
            }
            if slider {
                if let Action::Slider(target) = action {
                    self.topbar.controls.drag = Some((target, area));
                    self.panel_controls_motion(point);
                }
            } else {
                self.topbar.controls.activate(action);
            }
        } else if !self.topbar.controls.view.panel.to_f64().contains(local) {
            let action = self.topbar.action(&output, local);
            self.topbar.controls.close();
            if let Some(
                action @ (crate::topbar::BarAction::Network
                | crate::topbar::BarAction::Bluetooth
                | crate::topbar::BarAction::Mixer),
            ) = action
            {
                self.bar_action(&output, action);
            }
        }
        self.sync_bar_hints();
        self.queue_redraw_all();
        true
    }
    pub fn panel_controls_motion(&mut self, point: Point<f64, Logical>) -> bool {
        let Some((_, output)) = self.topbar.controls.open.clone() else {
            return false;
        };
        if let (Some((target, area)), Some(geo)) = (
            self.topbar.controls.drag.clone(),
            self.space.output_geometry(&output),
        ) {
            let level = (((point.x - geo.loc.x as f64 - area.loc.x as f64)
                / area.size.w.max(1) as f64)
                .clamp(0.0, 1.0)
                * 100.0)
                .round() as u8;
            self.topbar.controls.adjust(target, level);
            self.queue_redraw_all();
        }
        true
    }
    pub fn panel_controls_axis(&mut self, delta: f64) -> bool {
        if let Some((_, output)) = self.topbar.controls.open.as_ref() {
            if let Some(geo) = self.space.output_geometry(output) {
                let local = self.pointer_location() - geo.loc.to_f64();
                if self.topbar.action(output, local) == Some(crate::topbar::BarAction::Mixer)
                    && self.topbar.controls.master_step(delta)
                {
                    return true;
                }
            }
        }
        if !self.panel_controls_open() {
            return false;
        }
        if delta != 0.0 {
            let s = &mut self.topbar.controls;
            s.offset = if delta < 0.0 {
                s.offset.saturating_sub(1)
            } else {
                s.offset + 1
            };
            s.selected = s.offset;
            self.queue_redraw_all();
        }
        true
    }
}
pub fn fake_enabled() -> bool {
    std::env::var("GAVNO_PANEL_FAKE").as_deref() == Ok("1")
}
