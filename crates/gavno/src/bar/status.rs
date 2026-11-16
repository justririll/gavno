//! Blocking status I/O lives here. Only changed snapshots cross into calloop.
use super::Battery;
use crate::Gavno;
use smithay::reexports::calloop::{channel, LoopHandle};
use std::{
    path::Path,
    process::Command as Process,
    sync::mpsc::{self, Sender},
    time::{Duration, Instant},
};
use zbus::{
    blocking::{proxy::Builder, Connection, MessageIterator, Proxy},
    zvariant::OwnedObjectPath,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    pub level: f64,
    pub muted: bool,
}
pub fn parse_wpctl(text: &str) -> Option<Volume> {
    let mut words = text.split_whitespace();
    if words.next()? != "Volume:" {
        return None;
    }
    let level: f64 = words.next()?.parse().ok()?;
    if !level.is_finite() || level < 0.0 {
        return None;
    }
    let tail: Vec<_> = words.collect();
    if !tail.is_empty() && tail != ["[MUTED]"] {
        return None;
    }
    Some(Volume {
        level: level.min(1.0),
        muted: !tail.is_empty(),
    })
}
pub fn strength_icon(strength: u8) -> &'static str {
    match strength {
        80.. => "network-wireless-signal-excellent-symbolic",
        55..=79 => "network-wireless-signal-good-symbolic",
        30..=54 => "network-wireless-signal-ok-symbolic",
        1..=29 => "network-wireless-signal-weak-symbolic",
        0 => "network-wireless-signal-none-symbolic",
    }
}
pub fn battery_icons(battery: &Battery) -> [String; 3] {
    let suffix = if battery.charging { "-charging" } else { "" };
    let level = (battery.percent.min(100) / 10) * 10;
    let fallback = match battery.percent {
        0..=5 => "empty",
        6..=15 => "caution",
        16..=35 => "low",
        36..=85 => "good",
        _ => "full",
    };
    [
        format!("battery-level-{level}{suffix}-symbolic"),
        format!("battery-{fallback}{suffix}-symbolic"),
        format!("battery-{fallback}-symbolic"),
    ]
}
pub fn volume_icon(volume: Option<Volume>) -> &'static str {
    match volume {
        Some(v) if v.muted || v.level == 0.0 => "audio-volume-muted-symbolic",
        Some(v) if v.level <= 0.33 => "audio-volume-low-symbolic",
        Some(v) if v.level <= 0.66 => "audio-volume-medium-symbolic",
        Some(_) => "audio-volume-high-symbolic",
        None => "audio-volume-muted-symbolic",
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Network {
    Wifi {
        ssid: String,
        strength: u8,
    },
    Wired,
    #[default]
    Offline,
}
impl Network {
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Wifi { strength, .. } => strength_icon(*strength),
            Self::Wired => "network-wired-symbolic",
            Self::Offline => "network-wireless-offline-symbolic",
        }
    }
    pub fn label(&self) -> &str {
        match self {
            Self::Wifi { ssid, .. } => ssid,
            Self::Wired => "Wired",
            Self::Offline => "Offline",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Brightness {
    pub name: String,
    pub value: u32,
    pub max: u32,
}
impl Brightness {
    /// Lowest value GAVNO sets: 5 % of max. Panels go fully dark near 0, which
    /// leaves the user with a black screen and no way to see the controls.
    pub fn min_value(&self) -> u32 {
        ((self.max as f64 * 0.05).round() as u32).max(1).min(self.max)
    }
    pub fn level(&self) -> f64 {
        self.value as f64 / self.max as f64
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub volume: Option<Volume>,
    pub brightness: Option<Brightness>,
    pub battery: Option<Battery>,
    pub network: Network,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    VolumeUp,
    VolumeDown,
    Mute,
    MicMute,
    BrightnessUp,
    BrightnessDown,
}
#[derive(Debug)]
pub enum Command {
    Visible(bool),
    Media(Media),
    Volume(f64),
    Brightness(f64),
    Network(Option<Network>),
}
pub enum Event {
    Status(Snapshot),
    Mic(Volume),
}

/// A trailing update is retained even when input stops inside the throttle window.
#[derive(Default)]
pub struct VolumeThrottle {
    last: Option<Duration>,
    pending: Option<f64>,
}
impl VolumeThrottle {
    pub fn push(&mut self, value: f64) {
        self.pending = Some(value.clamp(0.0, 1.0));
    }
    pub fn take(&mut self, now: Duration) -> Option<f64> {
        if self
            .last
            .is_some_and(|t| now.saturating_sub(t) < Duration::from_millis(50))
        {
            return None;
        }
        let value = self.pending.take()?;
        self.last = Some(now);
        Some(value)
    }
    fn wait(&self, now: Duration) -> Option<Duration> {
        self.pending.map(|_| {
            self.last
                .map(|t| (t + Duration::from_millis(50)).saturating_sub(now))
                .unwrap_or_default()
        })
    }
}
fn read_volume(source: bool) -> Option<Volume> {
    let output = Process::new("wpctl")
        .args([
            "get-volume",
            if source {
                "@DEFAULT_AUDIO_SOURCE@"
            } else {
                "@DEFAULT_AUDIO_SINK@"
            },
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_wpctl(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}
/// Raising the volume unmutes, as in other desktops. The laptop's speaker
/// sink also reports muted once its hardware level reaches zero, and stayed
/// silent until mute was toggled by hand.
fn unmute_sink(volume: Option<Volume>) {
    if volume.is_some_and(|v| v.muted) {
        run_wpctl(&["set-mute", "@DEFAULT_AUDIO_SINK@", "0"]);
    }
}
fn run_wpctl(args: &[&str]) -> bool {
    match Process::new("wpctl").args(args).output() {
        Ok(o) if o.status.success() => true,
        result => {
            tracing::warn!(?result, "audio control unavailable");
            false
        }
    }
}
pub fn read_brightness(root: &Path) -> Option<Brightness> {
    let mut entries: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries.into_iter().find_map(|p| {
        let value = std::fs::read_to_string(p.join("brightness"))
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        let max = std::fs::read_to_string(p.join("max_brightness"))
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        if max == 0 {
            return None;
        }
        Some(Brightness {
            name: p.file_name()?.to_str()?.into(),
            value: value.min(max),
            max,
        })
    })
}
fn set_brightness(b: &Brightness, value: u32) -> zbus::Result<()> {
    let connection = Connection::system()?;
    Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1/session/auto",
        "org.freedesktop.login1.Session",
    )?
    .call("SetBrightness", &("backlight", b.name.as_str(), value))
}
fn fallback_network() -> Network {
    let connected = std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_name() != "lo"
                && std::fs::read_to_string(e.path().join("operstate"))
                    .is_ok_and(|s| s.trim() == "up")
        });
    if connected {
        Network::Wired
    } else {
        Network::Offline
    }
}
fn refresh(snapshot: &mut Snapshot, hardware: bool, fallback: bool) {
    snapshot.volume = read_volume(false);
    if hardware {
        snapshot.brightness = read_brightness(Path::new("/sys/class/backlight"));
    }
    snapshot.battery = super::read_battery(Path::new("/sys/class/power_supply"));
    if fallback {
        snapshot.network = fallback_network();
    }
}
pub fn spawn(handle: &LoopHandle<'static, Gavno>, hardware: bool) -> Option<Sender<Command>> {
    let _startup = crate::perf::Startup::new("dbus.status.spawn.begin", "dbus.status.spawn.end");
    let (events, source) = channel::channel();
    if let Err(error) = handle.insert_source(source, |event, _, gv| {
        let _stall = crate::perf::Stall::new("dbus.status");
        if let channel::Event::Msg(event) = event {
            gv.bar_status(event);
        }
    }) {
        tracing::warn!(%error,"cannot register status channel");
        return None;
    }
    let (commands, receive) = mpsc::channel();
    let network_commands = commands.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("bar-network".into())
        .spawn(move || {
            if let Err(error) = network_worker(&network_commands) {
                tracing::warn!(%error,"NetworkManager unavailable; using link state");
                let _ = network_commands.send(Command::Network(None));
            }
        })
    {
        tracing::warn!(%error,"cannot start network worker");
        let _ = commands.send(Command::Network(None));
    }
    let worker = std::thread::Builder::new()
        .name("bar-status".into())
        .spawn(move || {
            let start = Instant::now();
            let mut snapshot = Snapshot::default();
            let mut previous = Snapshot::default();
            let mut visible = false;
            let mut fallback = false;
            let mut refresh_at = None;
            let mut battery_at = Instant::now() + Duration::from_secs(60);
            let mut throttle = VolumeThrottle::default();
            let mut mic = None;
            refresh(&mut snapshot, hardware, false);
            if snapshot != previous {
                let _ = events.send(Event::Status(snapshot.clone()));
                previous = snapshot.clone();
            }
            loop {
                let now = start.elapsed();
                if let Some(value) = throttle.take(now) {
                    let applied = run_wpctl(&[
                        "set-volume",
                        "-l",
                        "1.0",
                        "@DEFAULT_AUDIO_SINK@",
                        &format!("{:.0}%", value * 100.0),
                    ]);
                    if value > 0.0 {
                        unmute_sink(snapshot.volume);
                    }
                    snapshot.volume = read_volume(false);
                    // Failed writes reconcile an optimistic slider value. This
                    // response is driven by input, never by an idle refresh.
                    if !applied {
                        let _ = events.send(Event::Status(snapshot.clone()));
                    }
                }
                if visible && refresh_at.is_some_and(|t| Instant::now() >= t) {
                    refresh(&mut snapshot, hardware, fallback);
                    refresh_at = Some(Instant::now() + Duration::from_secs(30));
                }
                // Low battery warnings must also fire while the bar is hidden
                // (fullscreen); only the battery is read then, once a minute.
                if !visible && battery_at <= Instant::now() {
                    snapshot.battery = super::read_battery(Path::new("/sys/class/power_supply"));
                    battery_at = Instant::now() + Duration::from_secs(60);
                }
                if snapshot != previous {
                    if events.send(Event::Status(snapshot.clone())).is_err() {
                        break;
                    }
                    previous = snapshot.clone();
                }
                let wait = throttle
                    .wait(start.elapsed())
                    .into_iter()
                    .chain(
                        refresh_at
                            .filter(|_| visible)
                            .map(|t| t.saturating_duration_since(Instant::now())),
                    )
                    .chain((!visible).then(|| battery_at.saturating_duration_since(Instant::now())))
                    .min();
                let command = match wait {
                    Some(d) => match receive.recv_timeout(d) {
                        Ok(c) => c,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    },
                    None => match receive.recv() {
                        Ok(c) => c,
                        Err(_) => break,
                    },
                };
                match command {
                    Command::Visible(v) => {
                        visible = v;
                        refresh_at = v.then(|| Instant::now() + Duration::from_secs(30));
                        if v {
                            refresh(&mut snapshot, hardware, fallback);
                        }
                    }
                    Command::Network(n) => {
                        fallback = n.is_none();
                        snapshot.network = n.unwrap_or_else(fallback_network);
                    }
                    Command::Volume(value) => throttle.push(value),
                    Command::Brightness(level) => {
                        if let Some(b) = snapshot.brightness.as_mut() {
                            let value = ((level.clamp(0.0, 1.0) * b.max as f64).round() as u32)
                                .clamp(b.min_value(), b.max);
                            match set_brightness(b, value) {
                                Ok(()) => b.value = value,
                                Err(error) => {
                                    tracing::warn!(%error,"brightness control unavailable");
                                    let _ = events.send(Event::Status(snapshot.clone()));
                                }
                            }
                        }
                    }
                    Command::Media(action) => match action {
                        Media::VolumeUp | Media::VolumeDown => {
                            run_wpctl(&[
                                "set-volume",
                                "-l",
                                "1.0",
                                "@DEFAULT_AUDIO_SINK@",
                                if action == Media::VolumeUp {
                                    "5%+"
                                } else {
                                    "5%-"
                                },
                            ]);
                            if action == Media::VolumeUp {
                                unmute_sink(snapshot.volume);
                            }
                            snapshot.volume = read_volume(false);
                        }
                        Media::Mute => {
                            run_wpctl(&["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]);
                            snapshot.volume = read_volume(false);
                        }
                        Media::MicMute => {
                            run_wpctl(&["set-mute", "@DEFAULT_AUDIO_SOURCE@", "toggle"]);
                            let value = read_volume(true);
                            if value != mic {
                                if let Some(v) = value {
                                    let _ = events.send(Event::Mic(v));
                                }
                                mic = value;
                            }
                        }
                        Media::BrightnessUp | Media::BrightnessDown => {
                            if let Some(b) = snapshot.brightness.as_mut() {
                                let step = ((b.max as f64 * 0.05).round() as u32).max(1);
                                let value = if action == Media::BrightnessUp {
                                    b.value.saturating_add(step).min(b.max)
                                } else {
                                    b.value.saturating_sub(step).max(b.min_value())
                                };
                                match set_brightness(b, value) {
                                    Ok(()) => b.value = value,
                                    Err(error) => {
                                        tracing::warn!(%error,"brightness control unavailable");
                                        let _ = events.send(Event::Status(snapshot.clone()));
                                    }
                                }
                            }
                        }
                    },
                }
            }
        });
    if let Err(error) = worker {
        tracing::warn!(%error,"cannot start status worker");
        return None;
    }
    Some(commands)
}
const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
fn proxy<'a>(
    connection: &'a Connection,
    path: &'a str,
    interface: &'a str,
) -> zbus::Result<Proxy<'a>> {
    Builder::new(connection)
        .destination(NM)?
        .path(path)?
        .interface(interface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
}
fn network_snapshot(connection: &Connection) -> zbus::Result<(Network, Vec<String>)> {
    let manager = proxy(connection, NM_PATH, NM)?;
    let path: OwnedObjectPath = manager.get_property("PrimaryConnection")?;
    let mut paths = vec![NM_PATH.into()];
    if path.as_str() == "/" {
        return Ok((Network::Offline, paths));
    }
    paths.push(path.to_string());
    let active = proxy(
        connection,
        path.as_str(),
        "org.freedesktop.NetworkManager.Connection.Active",
    )?;
    let kind: String = active.get_property("Type")?;
    if kind == "802-3-ethernet" {
        return Ok((Network::Wired, paths));
    }
    if kind != "802-11-wireless" {
        return Ok((Network::Offline, paths));
    }
    let devices: Vec<OwnedObjectPath> = active.get_property("Devices")?;
    for device in devices {
        paths.push(device.to_string());
        let wireless = proxy(
            connection,
            device.as_str(),
            "org.freedesktop.NetworkManager.Device.Wireless",
        )?;
        let Ok(ap) = wireless.get_property::<OwnedObjectPath>("ActiveAccessPoint") else {
            continue;
        };
        if ap.as_str() == "/" {
            continue;
        }
        paths.push(ap.to_string());
        let access = proxy(
            connection,
            ap.as_str(),
            "org.freedesktop.NetworkManager.AccessPoint",
        )?;
        let ssid: Vec<u8> = access.get_property("Ssid")?;
        let strength: u8 = access.get_property("Strength")?;
        return Ok((
            Network::Wifi {
                ssid: String::from_utf8_lossy(&ssid)
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect(),
                strength,
            },
            paths,
        ));
    }
    Ok((Network::Offline, paths))
}
fn network_worker(commands: &Sender<Command>) -> zbus::Result<()> {
    let connection = Connection::system()?;
    // arg0namespace matches NM PropertiesChanged interface names AND the
    // bus's NameOwnerChanged(NM, ...). This follows service restarts and new
    // AP/connection paths with one blocking subscription, without polling.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .arg0ns(NM)?
        .build();
    let signals = MessageIterator::for_match_rule(rule, &connection, Some(64))?;
    let (mut previous, mut paths) = match network_snapshot(&connection) {
        Ok((network, paths)) => (Some(network), paths),
        Err(error) => {
            tracing::warn!(%error,"NetworkManager unavailable; using link state");
            (None, vec![NM_PATH.to_string()])
        }
    };
    if commands.send(Command::Network(previous.clone())).is_err() {
        return Ok(());
    }
    for message in signals {
        let message = message?;
        let header = message.header();
        let owner_changed = header
            .interface()
            .is_some_and(|v| v.as_str() == "org.freedesktop.DBus")
            && header
                .member()
                .is_some_and(|v| v.as_str() == "NameOwnerChanged")
            && header
                .sender()
                .is_some_and(|v| v.as_str() == "org.freedesktop.DBus")
            && message
                .body()
                .deserialize::<(String, String, String)>()
                .is_ok_and(|v| v.0 == NM);
        let property_changed = header
            .interface()
            .is_some_and(|v| v.as_str() == "org.freedesktop.DBus.Properties")
            && header
                .member()
                .is_some_and(|v| v.as_str() == "PropertiesChanged")
            && header
                .path()
                .is_some_and(|p| paths.iter().any(|s| s == p.as_str()));
        if !owner_changed && !property_changed {
            continue;
        }
        let network = match network_snapshot(&connection) {
            Ok((network, new_paths)) => {
                paths = new_paths;
                Some(network)
            }
            Err(error) => {
                if previous.is_some() {
                    tracing::warn!(%error,"NetworkManager lost; using link state");
                }
                None
            }
        };
        if network != previous {
            if commands.send(Command::Network(network.clone())).is_err() {
                break;
            }
            previous = network;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wpctl_parser() {
        assert_eq!(
            parse_wpctl("Volume: 0.45\n"),
            Some(Volume {
                level: 0.45,
                muted: false
            })
        );
        assert_eq!(
            parse_wpctl("Volume: 0.45 [MUTED]"),
            Some(Volume {
                level: 0.45,
                muted: true
            })
        );
        for bad in [
            "",
            "Volume: NaN",
            "Volume: inf",
            "Volume: -1",
            "noise 0.5",
            "Volume: 0.4 junk",
        ] {
            assert_eq!(parse_wpctl(bad), None);
        }
        assert_eq!(parse_wpctl("Volume: 1.5").unwrap().level, 1.0);
    }
    #[test]
    fn network_levels() {
        for (value, level) in [
            (0, "none"),
            (1, "weak"),
            (29, "weak"),
            (30, "ok"),
            (54, "ok"),
            (55, "good"),
            (79, "good"),
            (80, "excellent"),
            (100, "excellent"),
        ] {
            assert_eq!(
                strength_icon(value),
                format!("network-wireless-signal-{level}-symbolic")
            );
        }
    }
    #[test]
    fn battery_names() {
        assert_eq!(
            battery_icons(&Battery {
                percent: 15,
                charging: false
            }),
            [
                "battery-level-10-symbolic",
                "battery-caution-symbolic",
                "battery-caution-symbolic"
            ]
        );
        assert_eq!(
            battery_icons(&Battery {
                percent: 100,
                charging: true
            })[0],
            "battery-level-100-charging-symbolic"
        );
        assert_eq!(
            battery_icons(&Battery {
                percent: 79,
                charging: true
            })[1],
            "battery-good-charging-symbolic"
        );
    }
    #[test]
    fn throttle_last_value_wins() {
        let mut t = VolumeThrottle::default();
        t.push(0.1);
        assert_eq!(t.take(Duration::ZERO), Some(0.1));
        t.push(0.2);
        t.push(0.7);
        assert_eq!(t.take(Duration::from_millis(49)), None);
        assert_eq!(t.take(Duration::from_millis(50)), Some(0.7));
        assert_eq!(t.take(Duration::from_secs(2)), None);
        assert_eq!(t.wait(Duration::from_secs(2)), None);
    }
    #[test]
    fn brightness_never_goes_dark() {
        let b = Brightness { name: "bl".into(), value: 10, max: 2047 };
        assert_eq!(b.min_value(), 102);
        let tiny = Brightness { name: "bl".into(), value: 1, max: 3 };
        assert_eq!(tiny.min_value(), 1);
    }

    #[test]
    fn brightness_fixture() {
        let root = std::path::PathBuf::from(format!("/tmp/gavno-backlight-{}", std::process::id()));
        let path = root.join("dp_aux_backlight");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("brightness"), "40\n").unwrap();
        std::fs::write(path.join("max_brightness"), "100\n").unwrap();
        assert_eq!(read_brightness(&root).unwrap().level(), 0.4);
        std::fs::remove_dir_all(root).unwrap();
    }
}
