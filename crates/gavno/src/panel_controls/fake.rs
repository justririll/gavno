//! Explicit development fixtures. Never access host buses, commands or devices.
use super::{
    audio, bluetooth, wifi,
    worker::{Command, Event, Snapshot},
    Kind,
};
use smithay::reexports::calloop::channel::Sender;
use std::sync::mpsc::Receiver;
/// Battery, backlight and power profile for the bar and the panel. The legacy
/// status worker never starts in fixture mode. `GAVNO_PANEL_FAKE_BATTERY=none`
/// shows a machine without a battery.
pub fn status() -> crate::topbar::status::Snapshot {
    let battery = match std::env::var("GAVNO_PANEL_FAKE_BATTERY").as_deref() {
        Ok("none") => None,
        Ok("low") => Some(crate::topbar::Battery { percent: 12, charging: false, minutes: Some(34), milliwatts: Some(6200), charge_limit: Some(80) }),
        _ => Some(crate::topbar::Battery { percent: 64, charging: true, minutes: Some(95), milliwatts: Some(18400), charge_limit: Some(80) }),
    };
    crate::topbar::status::Snapshot {
        volume: None,
        brightness: Some(crate::topbar::status::Brightness { name: "fake-panel".into(), value: 70, max: 100 }),
        battery,
        network: crate::topbar::status::Network::Offline,
        profile: Some("balanced".into()),
        power_history: vec![4200, 3900, 5100, 7800, 12600, 9400, 6100, 4800, 4500, 6200],
    }
}
pub fn snapshots() -> (wifi::Snapshot, bluetooth::Snapshot, audio::Snapshot) {
    let wifi = wifi::Snapshot {
        enabled: true,
        hardware: true,
        device: "fake-wlan".into(),
        failure: None,
        networks: vec![
            wifi::Network {
                path: "/fake/home".into(),
                ssid: b"Home network".to_vec(),
                strength: 94,
                secured: true,
                saved: Some("/fake/profile".into()),
                active: true,
                ..Default::default()
            },
            wifi::Network {
                path: "/fake/studio".into(),
                ssid: b"Studio".to_vec(),
                strength: 72,
                secured: true,
                ..Default::default()
            },
            wifi::Network {
                path: "/fake/guest".into(),
                ssid: b"Guest Wi-Fi".to_vec(),
                strength: 46,
                ..Default::default()
            },
        ],
    };
    let bluetooth = bluetooth::Snapshot {
        adapter: "/fake/adapter".into(),
        powered: true,
        blocked: false,
        discovering: false,
        devices: vec![
            bluetooth::Device {
                path: "/fake/headphones".into(),
                name: "Wireless headphones".into(),
                icon: "audio-headphones-symbolic".into(),
                paired: true,
                connected: true,
                battery: Some(82),
            },
            bluetooth::Device {
                path: "/fake/keyboard".into(),
                name: "Keyboard".into(),
                icon: "input-keyboard-symbolic".into(),
                paired: true,
                ..Default::default()
            },
            bluetooth::Device {
                path: "/fake/mouse".into(),
                name: "Travel mouse".into(),
                icon: "input-mouse-symbolic".into(),
                paired: true,
                battery: Some(41),
                ..Default::default()
            },
            bluetooth::Device {
                path: "/fake/speaker".into(),
                name: "Portable speaker".into(),
                icon: "audio-speakers-symbolic".into(),
                ..Default::default()
            },
        ],
    };
    let output = audio::Endpoint {
        id: "speakers".into(),
        name: "Built-in speakers".into(),
        volume: 45,
        ..Default::default()
    };
    let audio = audio::Snapshot {
        output: Some(output.clone()),
        input: Some(audio::Endpoint {
            id: "mic".into(),
            name: "Built-in microphone".into(),
            volume: 65,
            ..Default::default()
        }),
        sinks: vec![
            audio::Endpoint { icon: "audio-speakers-symbolic".into(), ..output.clone() },
            audio::Endpoint {
                id: "headphones".into(),
                name: "Wireless headphones".into(),
                icon: "audio-headphones-symbolic".into(),
                volume: 55,
                ..Default::default()
            },
        ],
        streams: vec![
            audio::Endpoint {
                id: "42".into(),
                name: "Music".into(),
                icon: "haruna".into(),
                volume: 70,
                ..Default::default()
            },
            audio::Endpoint {
                id: "44".into(),
                name: "Unknown player".into(),
                volume: 100,
                muted: true,
                ..Default::default()
            },
            audio::Endpoint {
                id: "43".into(),
                name: "Browser".into(),
                icon: "firefox".into(),
                volume: 30,
                ..Default::default()
            },
        ],
    };
    (wifi, bluetooth, audio)
}
pub fn run(kind: Kind, receive: Receiver<Command>, events: Sender<Event>) {
    let (mut wifi, mut bluetooth, mut audio) = snapshots();
    loop {
        let snapshot = match kind {
            Kind::Wifi => Snapshot::Wifi(wifi.clone()),
            Kind::Bluetooth => Snapshot::Bluetooth(bluetooth.clone()),
            Kind::Audio => Snapshot::Audio(audio.clone()),
        };
        if events
            .send(Event {
                kind,
                snapshot: Some(snapshot),
                error: None,
            })
            .is_err()
        {
            return;
        }
        let Ok(command) = receive.recv() else {
            return;
        };
        if matches!(command, Command::Shutdown) {
            return;
        }
        match command {
            Command::Power(v) if kind == Kind::Wifi => wifi.enabled = v,
            Command::Power(v) if kind == Kind::Bluetooth => bluetooth.powered = v,
            Command::Connect(n, _) => {
                for ap in &mut wifi.networks {
                    ap.active = ap.path == n.path;
                }
            }
            Command::Disconnect => {
                for ap in &mut wifi.networks {
                    ap.active = false;
                }
            }
            Command::Device(d) => {
                if let Some(device) = bluetooth.devices.iter_mut().find(|a| a.path == d.path) {
                    device.paired = true;
                    device.connected = !device.connected;
                }
            }
            Command::Search => bluetooth.discovering = true,
            Command::Visible(false) => bluetooth.discovering = false,
            Command::Volume(t, v) => {
                if let Some(e) = endpoint(&mut audio, &t) {
                    e.volume = v;
                }
            }
            Command::Mute(t) => {
                if let Some(e) = endpoint(&mut audio, &t) {
                    e.muted = !e.muted;
                }
            }
            Command::Output(id) => audio.output = audio.sinks.iter().find(|e| e.id == id).cloned(),
            _ => {}
        }
    }
}
fn endpoint<'a>(s: &'a mut audio::Snapshot, t: &audio::Target) -> Option<&'a mut audio::Endpoint> {
    match t {
        audio::Target::Output => s.output.as_mut(),
        audio::Target::Input => s.input.as_mut(),
        audio::Target::Stream(id) => s.streams.iter_mut().find(|e| &e.id == id),
    }
}
