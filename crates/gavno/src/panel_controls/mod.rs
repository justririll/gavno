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
/// Which worker owns the data of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Wifi,
    Bluetooth,
    Audio,
}
/// Pages of the quick settings panel. `Main` is the overview; the others
/// slide in from it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Main,
    Wifi,
    Bluetooth,
    Audio,
    Power,
}
impl Page {
    fn kind(self) -> Option<Kind> {
        match self {
            Self::Wifi => Some(Kind::Wifi),
            Self::Bluetooth => Some(Kind::Bluetooth),
            Self::Audio => Some(Kind::Audio),
            Self::Main | Self::Power => None,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Main => "Quick settings",
            Self::Wifi => "Wi-Fi",
            Self::Bluetooth => "Bluetooth",
            Self::Audio => "Sound",
            Self::Power => "Power",
        }
    }
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
    Tty,
    WifiPower,
    Disconnect,
    Network(String),
    BluetoothPower,
    Search,
    Device(String),
    /// A volume slider: dragged and adjusted, activating toggles its mute.
    Slider(audio::Target),
    /// The icon of a slider.
    Mute(audio::Target),
    Output(String),
    Brightness,
    NightLight,
    DoNotDisturb,
    CodeMode,
    ChargeLimit,
    Open(Page),
    Back,
    Settings,
    Lock,
    Power(crate::topbar::PowerAction),
    Media(crate::media::Command),
    MediaInfo,
}
/// Everything the compositor owns that the panel shows or toggles.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub battery: Option<crate::topbar::Battery>,
    /// Battery power draw in milliwatts, oldest first, for the graph.
    pub power_history: Vec<u32>,
    pub profile: Option<String>,
    /// Backlight or software dimming, 0.0..=1.0.
    pub brightness: Option<f64>,
    pub dnd: bool,
    pub night_light: bool,
    pub code_mode: bool,
    /// Code mode does not exist in the Classic shell.
    pub code_available: bool,
    pub tty: bool,
    pub player: bool,
}
/// Work only the compositor can do, requested by the panel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Effect {
    NightLight,
    /// Toggle the battery charge limit between 80 % and 100 %.
    ChargeLimit,
    DoNotDisturb,
    CodeMode,
    TtyMode,
    Brightness(f64),
    Settings,
    Lock,
    Power(crate::topbar::PowerAction),
    Media(crate::media::Command),
    MusicPanel,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    Tile,
    /// The on/off switch beside a detail page's title.
    Switch,
    Entry,
    Network,
    Device,
    Slider,
    AppSlider,
    Button,
    Media,
}
#[derive(Clone)]
struct Row {
    kind: RowKind,
    label: String,
    detail: String,
    icon: String,
    action: Action,
    level: Option<u8>,
    muted: bool,
    section: &'static str,
    on: bool,
    chevron: Option<Page>,
    strength: Option<u8>,
    secured: bool,
    battery: Option<u8>,
}
impl Row {
    fn new(kind: RowKind, label: &str, action: Action) -> Self {
        Self {
            kind,
            label: label.into(),
            detail: String::new(),
            icon: String::new(),
            action,
            level: None,
            muted: false,
            section: "",
            on: false,
            chevron: None,
            strength: None,
            secured: false,
            battery: None,
        }
    }
    fn icon(mut self, icon: &str) -> Self {
        self.icon = icon.into();
        self
    }
    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
    fn on(mut self, on: bool) -> Self {
        self.on = on;
        self
    }
    fn section(mut self, section: &'static str) -> Self {
        self.section = section;
        self
    }
}
/// What a drag on a slider changes.
type Drag = (Action, Rect);
#[derive(Default)]
struct Anim {
    opened: Option<std::time::Instant>,
    slide: Option<(Page, std::time::Instant)>,
}
use crate::ui::style::{OPEN_TIME, SLIDE_TIME};
#[derive(Default)]
pub struct State {
    pub wifi: wifi::Snapshot,
    pub bluetooth: bluetooth::Snapshot,
    pub audio: audio::Snapshot,
    errors: [Option<String>; 3],
    workers: [Option<worker::Worker>; 3],
    started: bool,
    pub event_token: Option<smithay::reexports::calloop::RegistrationToken>,
    pub open: Option<Output>,
    page: Page,
    selected: usize,
    offset: usize,
    password: Option<(String, Secret)>,
    view: ui::View,
    drag: Option<Drag>,
    /// Set by the first key press after opening; shows the key hint footer.
    keyboard: bool,
    /// The pointer is over a target; the selection ring follows it.
    hover: bool,
    pub code_mode: bool,
    anim: Anim,
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
    /// Where the open panel was last drawn, output-local.
    pub fn panel_rect(&self) -> Option<crate::topbar::geometry::Rect> {
        (self.open.is_some() && self.view.panel.size.w > 0).then_some(self.view.panel)
    }
    /// True while the opening or a page slide still needs frames.
    pub fn animating(&self) -> bool {
        self.open.is_some() && (self.anim.opened.is_some() || self.anim.slide.is_some())
    }
    /// Opens the panel on `page`, or slides to it when it is already open.
    fn show(&mut self, page: Page, output: &Output) {
        if self.open.is_some() {
            self.open = Some(output.clone());
            self.go(page);
            return;
        }
        self.open = Some(output.clone());
        self.page = page;
        self.selected = 0;
        self.offset = 0;
        self.keyboard = false;
        self.anim = Anim {
            opened: Some(std::time::Instant::now()),
            slide: None,
        };
        self.send(Kind::Audio, worker::Command::Visible(true));
        if let Some(kind) = page.kind().filter(|k| *k != Kind::Audio) {
            self.send(kind, worker::Command::Visible(true));
        }
    }
    /// Switches page with the slide animation. Workers only see the pages
    /// they own, so an invisible Wi-Fi page never scans.
    fn go(&mut self, page: Page) {
        if self.open.is_none() || self.page == page {
            return;
        }
        let old = self.page.kind();
        if old.is_some() && old != page.kind() {
            self.send(old.unwrap(), worker::Command::Visible(false));
        }
        if let Some(kind) = page.kind().filter(|k| Some(*k) != old) {
            self.send(kind, worker::Command::Visible(true));
        }
        self.anim.slide = Some((self.page, std::time::Instant::now()));
        self.page = page;
        self.selected = 0;
        self.offset = 0;
        self.password = None;
        self.drag = None;
    }
    pub fn close(&mut self) {
        if self.open.take().is_some() {
            if let Some(kind) = self.page.kind() {
                self.send(kind, worker::Command::Visible(false));
            }
        }
        self.page = Page::Main;
        self.password = None;
        self.drag = None;
        self.hover = false;
        self.anim = Anim::default();
        self.view.hits.clear();
    }
    fn rows(&self, page: Page, info: &Info) -> Vec<Row> {
        match page {
            Page::Main => self.main_rows(info),
            Page::Wifi => self.wifi_rows(),
            Page::Bluetooth => self.bluetooth_rows(),
            Page::Audio => self.audio_rows(),
            Page::Power => crate::topbar::POWER
                .iter()
                .map(|(action, icon, label)| {
                    Row::new(RowKind::Entry, label, Action::Power(*action)).icon(icon)
                })
                .collect(),
        }
    }
    fn main_rows(&self, info: &Info) -> Vec<Row> {
        let wifi = &self.wifi;
        let bluetooth = &self.bluetooth;
        let mut rows = Vec::new();
        let mut tile = |label: &str, detail: String, icon: &str, action, on, chevron| {
            let mut row = Row::new(RowKind::Tile, label, action).icon(icon).detail(detail).on(on);
            row.chevron = chevron;
            rows.push(row);
        };
        tile(
            "Wi-Fi",
            if !wifi.hardware {
                "Blocked".into()
            } else if !wifi.enabled {
                "Off".into()
            } else {
                wifi.networks
                    .iter()
                    .find(|n| n.active)
                    .map(|n| n.label())
                    .unwrap_or_else(|| "Not connected".into())
            },
            wifi.icon(),
            Action::WifiPower,
            wifi.enabled && wifi.hardware,
            Some(Page::Wifi),
        );
        tile(
            "Bluetooth",
            if bluetooth.blocked {
                "Blocked".into()
            } else if !bluetooth.powered {
                "Off".into()
            } else {
                bluetooth
                    .devices
                    .iter()
                    .find(|d| d.connected)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "On".into())
            },
            bluetooth.icon(),
            Action::BluetoothPower,
            bluetooth.powered,
            Some(Page::Bluetooth),
        );
        let on_off = |on: bool| if on { "On" } else { "Off" }.to_string();
        tile(
            "Night light",
            on_off(info.night_light),
            "night-light-symbolic",
            Action::NightLight,
            info.night_light,
            None,
        );
        tile(
            "Do not disturb",
            on_off(info.dnd),
            "notifications-disabled-symbolic",
            Action::DoNotDisturb,
            info.dnd,
            None,
        );
        if let Some(limit) = info.battery.as_ref().and_then(|b| b.charge_limit) {
            let on = limit < 100;
            tile(
                "Charge limit",
                if on { format!("{limit}%") } else { "Off".into() },
                "battery-level-80-plugged-in-symbolic",
                Action::ChargeLimit,
                on,
                None,
            );
        }
        if info.code_available {
            tile(
                "Code mode",
                on_off(info.code_mode),
                "utilities-terminal-symbolic",
                Action::CodeMode,
                info.code_mode,
                None,
            );
            tile(
                "TTY",
                on_off(info.tty),
                "utilities-terminal-symbolic",
                Action::Tty,
                info.tty,
                None,
            );
        }
        let output = self.audio.output.as_ref();
        let volume = output.map(|e| (e.volume as f64 / 100.0, e.muted));
        let mut slider = Row::new(
            RowKind::Slider,
            "Volume",
            Action::Slider(audio::Target::Output),
        )
        .icon(crate::topbar::status::volume_icon(volume.map(|(level, muted)| {
            crate::topbar::status::Volume { level, muted }
        })));
        slider.level = Some(output.map(|e| e.volume).unwrap_or(0));
        slider.muted = output.is_none_or(|e| e.muted);
        slider.chevron = Some(Page::Audio);
        rows.push(slider);
        if let Some(level) = info.brightness {
            let mut slider = Row::new(RowKind::Slider, "Brightness", Action::Brightness)
                .icon("display-brightness-symbolic");
            slider.level = Some((level.clamp(0.0, 1.0) * 100.0).round() as u8);
            rows.push(slider);
        }
        if info.player {
            // Media sits first in the focus order, as it does on screen.
            rows.insert(0, Row::new(RowKind::Media, "Media", Action::MediaInfo));
        }
        for (label, icon, action) in [
            ("Settings", "preferences-system-symbolic", Action::Settings),
            ("Lock", "system-lock-screen-symbolic", Action::Lock),
            ("Power", "system-shutdown-symbolic", Action::Open(Page::Power)),
        ] {
            rows.push(Row::new(RowKind::Button, label, action).icon(icon));
        }
        rows
    }
    fn wifi_rows(&self) -> Vec<Row> {
        let wifi = &self.wifi;
        let mut rows = vec![Row::new(RowKind::Switch, "Wi-Fi", Action::WifiPower)
            .icon(wifi.icon())
            .detail(if !wifi.hardware {
                "Blocked by hardware rfkill"
            } else if wifi.enabled {
                "On"
            } else {
                "Off"
            })
            .on(wifi.enabled && wifi.hardware)];
        if !wifi.enabled {
            return rows;
        }
        let network = |n: &wifi::Network, section: &'static str| {
            let mut row = Row::new(RowKind::Network, &n.label(), Action::Network(n.path.clone()))
                .section(section)
                .on(n.active);
            row.strength = Some(n.strength);
            row.secured = n.secured;
            row
        };
        rows.extend(wifi.networks.iter().filter(|n| n.active).map(|n| network(n, "")));
        let saved = wifi.networks.iter().filter(|n| !n.active && n.saved.is_some());
        rows.extend(saved.map(|n| network(n, "Saved")));
        let nearby = wifi.networks.iter().filter(|n| !n.active && n.saved.is_none());
        rows.extend(nearby.map(|n| network(n, "Nearby")));
        rows
    }
    fn bluetooth_rows(&self) -> Vec<Row> {
        let bt = &self.bluetooth;
        let mut rows = vec![
            Row::new(RowKind::Switch, "Bluetooth", Action::BluetoothPower)
                .icon(bt.icon())
                .detail(if bt.blocked {
                    "Blocked by rfkill"
                } else if bt.powered {
                    "On"
                } else {
                    "Off"
                })
                .on(bt.powered),
            Row::new(RowKind::Button, "Search", Action::Search)
                .icon("system-search-symbolic")
                .detail(if bt.discovering {
                    "Searching for up to 30 seconds"
                } else {
                    "Find nearby devices"
                }),
        ];
        let device = |d: &bluetooth::Device, section: &'static str| {
            let mut row = Row::new(RowKind::Device, &d.name, Action::Device(d.path.clone()))
                .icon(&d.icon)
                .section(section)
                .on(d.connected);
            row.battery = d.battery;
            row
        };
        let groups: [(&'static str, fn(&bluetooth::Device) -> bool); 3] = [
            ("Connected", |d| d.connected),
            ("Paired", |d| d.paired && !d.connected),
            ("Found", |d| !d.paired),
        ];
        for (section, belongs) in groups {
            rows.extend(bt.devices.iter().filter(|d| belongs(d)).map(|d| device(d, section)));
        }
        rows
    }
    fn audio_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let slider = |e: &audio::Endpoint, target, icon: &str, section| {
            let mut row = Row::new(RowKind::Slider, &e.name, Action::Slider(target))
                .icon(icon)
                .section(section);
            row.level = Some(e.volume);
            row.muted = e.muted;
            row
        };
        if let Some(e) = &self.audio.output {
            let mut row = slider(e, audio::Target::Output, if e.muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" }, "Output");
            row.label = "Volume".into();
            rows.push(row);
        }
        for e in &self.audio.sinks {
            let current = self.audio.output.as_ref().is_some_and(|o| o.id == e.id);
            let icon = if e.icon.is_empty() { "audio-speakers-symbolic" } else { &e.icon };
            let mut row = Row::new(RowKind::Entry, &e.name, Action::Output(e.id.clone()))
                .icon(icon)
                .on(current);
            row.section = "Output";
            rows.push(row);
        }
        if let Some(e) = &self.audio.input {
            let icon = if e.muted { "microphone-sensitivity-muted-symbolic" } else { "audio-input-microphone-symbolic" };
            rows.push(slider(e, audio::Target::Input, icon, "Input"));
        }
        for e in &self.audio.streams {
            let icon = if e.icon.is_empty() { "application-x-executable-symbolic" } else { &e.icon };
            let mut row = Row::new(RowKind::AppSlider, &e.name, Action::Slider(audio::Target::Stream(e.id.clone())))
                .icon(icon)
                .section("Apps");
            row.level = Some(e.volume);
            row.muted = e.muted;
            rows.push(row);
        }
        rows
    }
    fn apply_event(&mut self, event: worker::Event) -> bool {
        let before = (
            self.wifi.icon(),
            self.bluetooth.icon(),
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
            self.audio.output.as_ref().map(|e| (e.volume, e.muted)),
        );
        // The overview shows all three services; a detail page only its own.
        let visible = self.open.is_some()
            && (self.page == Page::Main || self.page.kind() == Some(event.kind));
        before != after || (visible && (changed || old_error != self.errors[index(event.kind)]))
    }
    fn activate(&mut self, action: Action) -> Option<Effect> {
        match action {
            Action::WifiPower => self.send(Kind::Wifi, worker::Command::Power(!self.wifi.enabled)),
            Action::Disconnect => self.send(Kind::Wifi, worker::Command::Disconnect),
            Action::Network(path) => {
                if let Some(n) = self.wifi.networks.iter().find(|n| n.path == path) {
                    if n.active {
                        self.send(Kind::Wifi, worker::Command::Disconnect);
                    } else if n.enterprise && n.saved.is_none() {
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
            Action::Slider(target) | Action::Mute(target) => {
                self.send(Kind::Audio, worker::Command::Mute(target))
            }
            Action::Output(id) => self.send(Kind::Audio, worker::Command::Output(id)),
            Action::Brightness => {}
            Action::NightLight => return Some(Effect::NightLight),
            Action::DoNotDisturb => return Some(Effect::DoNotDisturb),
            Action::CodeMode => return Some(Effect::CodeMode),
            Action::ChargeLimit => return Some(Effect::ChargeLimit),
            Action::Tty => return Some(Effect::TtyMode),
            Action::Open(page) => self.go(page),
            Action::Back => self.go(Page::Main),
            Action::Settings => return Some(Effect::Settings),
            Action::Lock => return Some(Effect::Lock),
            Action::Power(action) => return Some(Effect::Power(action)),
            Action::Media(command) => return Some(Effect::Media(command)),
            Action::MediaInfo => return Some(Effect::MusicPanel),
        }
        None
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
    /// Index of the row a hit belongs to: the keyboard selection follows the pointer.
    fn row_of(&self, action: &Action, info: &Info) -> Option<usize> {
        let normal = match action {
            Action::Mute(t) => Action::Slider(t.clone()),
            other => other.clone(),
        };
        self.rows(self.page, info).iter().position(|row| {
            row.action == normal
                || matches!(action, Action::Open(p) if row.chevron == Some(*p))
        })
    }
    fn key(&mut self, sym: u32, ch: Option<char>, info: &Info) -> Option<Effect> {
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
            return None;
        }
        self.keyboard = true;
        if sym == keysyms::KEY_Escape {
            self.close();
            return None;
        }
        self.open.as_ref()?;
        let rows = self.rows(self.page, info);
        let last = rows.len().saturating_sub(1);
        self.selected = self.selected.min(last);
        let row = rows.get(self.selected);
        match sym {
            keysyms::KEY_Down | keysyms::KEY_j | keysyms::KEY_Tab => {
                self.selected = (self.selected + 1).min(last)
            }
            keysyms::KEY_Up | keysyms::KEY_k | keysyms::KEY_ISO_Left_Tab => {
                self.selected = self.selected.saturating_sub(1)
            }
            keysyms::KEY_BackSpace => self.go(Page::Main),
            keysyms::KEY_Left | keysyms::KEY_h | keysyms::KEY_Right | keysyms::KEY_l => {
                let right = matches!(sym, keysyms::KEY_Right | keysyms::KEY_l);
                let step = |value: u8| {
                    if right {
                        value.saturating_add(5).min(100)
                    } else {
                        value.saturating_sub(5)
                    }
                };
                match row {
                    Some(Row { action: Action::Slider(target), level: Some(value), .. }) => {
                        self.adjust(target.clone(), step(*value));
                    }
                    Some(Row { action: Action::Brightness, level: Some(value), .. }) => {
                        return Some(Effect::Brightness(step(*value) as f64 / 100.0));
                    }
                    Some(Row { chevron: Some(page), .. }) if right => self.go(*page),
                    _ if right => self.selected = (self.selected + 1).min(last),
                    _ => self.selected = self.selected.saturating_sub(1),
                }
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                return self.activate(row?.action.clone());
            }
            // Space opens the details of a tile or slider, otherwise it acts like Enter.
            keysyms::KEY_space => {
                let row = row?;
                return match row.chevron {
                    Some(page) => {
                        self.go(page);
                        None
                    }
                    None => self.activate(row.action.clone()),
                };
            }
            _ => {}
        }
        None
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
        if fake {
            // The legacy status worker is off in fixture mode; give the bar
            // and the panel a battery, a profile and a backlight to show.
            self.bar_status(crate::topbar::status::Event::Status(fake::status()));
        }
    }
    /// Opens the panel on `page`, slides there when open, or closes it when
    /// that page is already showing.
    pub fn toggle_panel_controls(&mut self, page: Page, output: &Output) {
        self.start_panel_controls();
        let here = self.topbar.controls.open.as_ref() == Some(output);
        if here && self.topbar.controls.page == page {
            self.topbar.controls.close();
        } else if here {
            self.topbar.controls.go(page);
        } else {
            self.topbar.close();
            self.close_music();
            self.dock.close_menu();
            self.topbar.controls.show(page, output);
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
            .is_some_and(|output| {
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
    fn panel_info(&self) -> Info {
        self.topbar.quick_info(self.classic_mode(), self.features.on(crate::module::Feature::Media))
    }
    /// Runs what the panel asked of the compositor.
    fn panel_effect(&mut self, output: &Output, effect: Effect) {
        match effect {
            Effect::NightLight => {
                self.config.display.night_light = !self.config.display.night_light;
                self.topbar.set_night_light(self.config.display.night_light);
                self.refresh_theme_clock();
            }
            Effect::DoNotDisturb => self.bar_action(output, crate::topbar::BarAction::DoNotDisturb),
            Effect::CodeMode => self.set_code_mode(!self.code_mode),
            Effect::ChargeLimit => self.toggle_charge_limit(),
            Effect::TtyMode => self.set_tty_mode(!self.tty.active()),
            Effect::Brightness(level) => self.panel_set_brightness(level),
            Effect::Settings => {
                self.topbar.controls.close();
                let command = self.config.classic.settings_command.clone();
                self.launch_shell(&command, None);
            }
            Effect::Lock => self.bar_action(
                output,
                crate::topbar::BarAction::Power(crate::topbar::PowerAction::Lock),
            ),
            Effect::Power(action) => {
                self.bar_action(output, crate::topbar::BarAction::Power(action))
            }
            Effect::Media(command) => self.media_command(command),
            Effect::MusicPanel => {
                self.topbar.controls.close();
                self.bar_action(output, crate::topbar::BarAction::MusicPanel);
            }
        }
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
        let output = self.topbar.controls.open.clone();
        let info = self.panel_info();
        let effect = self.topbar.controls.key(sym.raw(), ch, &info);
        if let (Some(effect), Some(output)) = (effect, output) {
            self.panel_effect(&output, effect);
        }
        self.cancel_shell_tap();
        self.sync_bar_hints();
        self.queue_redraw_all();
    }
    pub fn panel_controls_button(&mut self, point: Point<f64, Logical>, pressed: bool) -> bool {
        let Some(output) = self.topbar.controls.open.clone() else {
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
            let info = self.panel_info();
            if let Some(selected) = self.topbar.controls.row_of(&action, &info) {
                self.topbar.controls.selected = selected;
            }
            if slider {
                self.topbar.controls.drag = Some((action, area));
                self.panel_controls_motion(point);
            } else if let Some(effect) = self.topbar.controls.activate(action) {
                self.panel_effect(&output, effect);
            }
        } else if !self.topbar.controls.view.panel.to_f64().contains(local) {
            // Outside closes, including on the status cluster, which toggles.
            self.topbar.controls.close();
        }
        self.sync_bar_hints();
        self.queue_redraw_all();
        true
    }
    pub fn panel_controls_motion(&mut self, point: Point<f64, Logical>) -> bool {
        let Some(output) = self.topbar.controls.open.clone() else {
            return false;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return true;
        };
        let local = point - geo.loc.to_f64();
        if let Some((action, area)) = self.topbar.controls.drag.clone() {
            let level = ((local.x - area.loc.x as f64) / area.size.w.max(1) as f64)
                .clamp(0.0, 1.0)
                * 100.0;
            match action {
                Action::Slider(target) => self.topbar.controls.adjust(target, level.round() as u8),
                Action::Brightness => self.panel_effect(&output, Effect::Brightness(level.round() / 100.0)),
                _ => {}
            }
            self.queue_redraw_all();
            return true;
        }
        // The selection follows the pointer, redrawing only on a change.
        let hovered = self
            .topbar
            .controls
            .view
            .hits
            .iter()
            .rev()
            .find(|(r, _, _)| r.to_f64().contains(local))
            .map(|(_, action, _)| action.clone());
        let over = hovered.is_some();
        let mut changed = over != self.topbar.controls.hover;
        self.topbar.controls.hover = over;
        if let Some(action) = hovered {
            let info = self.panel_info();
            if let Some(row) = self.topbar.controls.row_of(&action, &info) {
                if row != self.topbar.controls.selected {
                    self.topbar.controls.selected = row;
                    changed = true;
                }
            }
        }
        if changed {
            self.queue_redraw_all();
        }
        true
    }
    pub fn panel_controls_axis(&mut self, delta: f64) -> bool {
        if let Some(output) = self.topbar.controls.open.as_ref() {
            if let Some(geo) = self.space.output_geometry(output) {
                let local = self.pointer_location() - geo.loc.to_f64();
                if self.topbar.action(output, local) == Some(crate::topbar::BarAction::PowerMenu)
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
            self.queue_redraw_all();
        }
        true
    }
}
pub fn fake_enabled() -> bool {
    std::env::var("GAVNO_PANEL_FAKE").as_deref() == Ok("1")
}
