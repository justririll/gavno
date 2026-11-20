use super::*;
use smithay::reexports::calloop::channel;
use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
fn output() -> Output {
    Output::new(
        "panel-test".into(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    )
}
fn state(page: Page) -> State {
    let (wifi, bluetooth, audio) = fake::snapshots();
    State {
        wifi,
        bluetooth,
        audio,
        open: Some(output()),
        page,
        ..Default::default()
    }
}
fn info() -> Info {
    Info {
        battery: Some(crate::topbar::Battery { percent: 64, charging: true, minutes: Some(95) }),
        profile: Some("balanced".into()),
        brightness: Some(0.7),
        code_available: true,
        ..Default::default()
    }
}
fn probe<const N: usize>(s: &mut State, kinds: [usize; N]) -> [mpsc::Receiver<worker::Command>; N] {
    kinds.map(|i| {
        let (tx, rx) = mpsc::channel();
        s.workers[i] = Some(worker::Worker::new(tx));
        rx
    })
}
#[test]
fn legacy_status_cannot_overwrite_panel_volume_or_resurrect_a_removed_output() {
    use crate::topbar::status;
    let mut h = crate::state::tests::Harness::new();
    let legacy_volume = Some(status::Volume { level: 0.5, muted: false });
    let legacy = status::Snapshot {
        volume: legacy_volume,
        brightness: Some(status::Brightness { name: "fake-panel".into(), value: 70, max: 100 }),
        battery: Some(crate::topbar::Battery { percent: 42, charging: false, minutes: None }),
        network: status::Network::Wired,
        profile: None,
    };
    // Before a successful panel response, legacy volume remains the fallback.
    h.gv.panel_controls_event(worker::Event {
        kind: Kind::Audio, snapshot: None, error: Some("Fake server unavailable".into()),
    });
    h.gv.bar_status(status::Event::Status(legacy.clone()));
    assert_eq!(h.gv.media.status.volume, legacy_volume);

    let mut audio = fake::snapshots().2;
    let output = audio.output.as_mut().unwrap();
    output.volume = 20;
    output.muted = true;
    h.gv.media.osd.show(crate::topbar::osd::Kind::Volume, 0.5, false, h.gv.start_time.elapsed());
    h.gv.panel_controls_event(worker::Event {
        kind: Kind::Audio, snapshot: Some(worker::Snapshot::Audio(audio)), error: None,
    });
    let current = Some(status::Volume { level: 0.2, muted: true });
    assert_eq!(h.gv.topbar.status().volume, current);
    assert!(h.gv.media.osd_timer.is_some(), "panel events reconcile an already visible hotkey OSD");
    // A brightness/network/battery update carries the old worker's cached 50%.
    h.gv.bar_status(status::Event::Status(legacy.clone()));
    assert_eq!(h.gv.media.status.volume, current);
    assert_eq!(h.gv.topbar.status().volume, current);
    assert_eq!(h.gv.media.brightness.backlight, legacy.brightness);
    assert_eq!(h.gv.topbar.status().battery, legacy.battery);
    assert_eq!(h.gv.topbar.status().network, legacy.network);

    h.gv.panel_controls_event(worker::Event {
        kind: Kind::Audio, snapshot: None, error: Some("Fake transient failure".into()),
    });
    h.gv.bar_status(status::Event::Status(legacy.clone()));
    assert_eq!(h.gv.media.status.volume, current);
    // A successful snapshot with no output is authoritative too.
    h.gv.panel_controls_event(worker::Event {
        kind: Kind::Audio, snapshot: Some(worker::Snapshot::Audio(Default::default())), error: None,
    });
    h.gv.bar_status(status::Event::Status(legacy.clone()));
    assert_eq!(h.gv.media.status.volume, None);
    assert_eq!(h.gv.topbar.status().volume, None);

    h.gv.reset_bar();
    h.gv.bar_status(status::Event::Status(legacy));
    assert_eq!(h.gv.media.status.volume, legacy_volume);
}
#[test]
fn wifi_deduplicates_raw_ssids_and_maps_radio_and_signal() {
    let mut s = state(Page::Wifi).wifi;
    let mut duplicate = s.networks[0].clone();
    duplicate.path = "/other".into();
    duplicate.active = false;
    duplicate.strength = 100;
    s.networks.push(duplicate);
    s.networks.push(wifi::Network::default());
    s.networks = wifi::deduplicate(s.networks);
    assert_eq!(s.networks.len(), 3);
    assert!(s.networks[0].active);
    assert_eq!(s.networks[0].strength, 94);
    assert_eq!(s.icon(), "network-wireless-signal-excellent-symbolic");
    s.enabled = false;
    assert_eq!(s.icon(), "network-wireless-disabled-symbolic");
    s.enabled = true;
    s.networks.clear();
    assert_eq!(s.icon(), "network-wireless-offline-symbolic");
}
#[test]
fn bluetooth_maps_properties_battery_and_type() {
    let adapter = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    let device = OwnedObjectPath::try_from("/org/bluez/hci0/dev_01").unwrap();
    let objects = HashMap::from([
        (
            adapter,
            HashMap::from([(
                "org.bluez.Adapter1".into(),
                HashMap::from([("Powered".into(), OwnedValue::from(true))]),
            )]),
        ),
        (
            device,
            HashMap::from([
                (
                    "org.bluez.Device1".into(),
                    HashMap::from([
                        (
                            "Alias".into(),
                            OwnedValue::from(zbus::zvariant::Str::from("Headphones")),
                        ),
                        (
                            "Icon".into(),
                            OwnedValue::from(zbus::zvariant::Str::from("audio-headset")),
                        ),
                        ("Connected".into(), OwnedValue::from(true)),
                        ("Paired".into(), OwnedValue::from(true)),
                    ]),
                ),
                (
                    "org.bluez.Battery1".into(),
                    HashMap::from([("Percentage".into(), OwnedValue::from(82u8))]),
                ),
            ]),
        ),
    ]);
    let s = bluetooth::map_objects(objects);
    assert_eq!(s.devices.len(), 1);
    assert_eq!(s.battery(), Some(82));
    assert_eq!(s.devices[0].icon, "audio-headphones-symbolic");
    assert_eq!(s.icon(), "bluetooth-active-symbolic");
}
#[test]
fn pulse_json_maps_channels_stream_names_mute_and_icons() {
    let text = r#"[{"index":42,"properties":{"application.name":"Browser","application.icon_name":"web-browser"},"volume":{"front-left":{"value":32768},"front-right":{"value":65536}},"mute":true}]"#;
    let rows = audio::parse_list(text, true).unwrap();
    assert_eq!(rows[0].volume, 75);
    assert_eq!(rows[0].name, "Browser");
    assert_eq!(rows[0].id, "42");
    assert!(rows[0].muted);
    assert_eq!(rows[0].icon, "web-browser");
    assert!(audio::parse_list("{}", false).is_err());
    assert!(audio::parse_list("not json", false).is_err());
}
#[test]
fn password_debug_is_redacted_and_escape_clears_buffer() {
    let mut s = state(Page::Wifi);
    let info = info();
    s.activate(Action::Network("/fake/studio".into()));
    for ch in "private-password🔑".chars() {
        s.key(0, Some(ch), &info);
    }
    let debug = format!("{:?}", s.password);
    assert!(!debug.contains("private"));
    assert!(!debug.contains("🔑"));
    assert!(debug.contains("REDACTED"));
    s.key(keysyms::KEY_BackSpace, None, &info);
    assert_eq!(
        s.password.as_ref().unwrap().1 .0.as_str(),
        "private-password"
    );
    // Backspace edits the password; it does not leave the page.
    assert_eq!(s.page, Page::Wifi);
    s.key(keysyms::KEY_Escape, None, &info);
    assert!(s.password.is_none());
    assert!(s.open.is_some());
    s.key(keysyms::KEY_Escape, None, &info);
    assert!(s.open.is_none());
}
#[test]
fn keyboard_navigation_slider_steps_and_mute_use_worker_commands() {
    let mut s = state(Page::Audio);
    let info = info();
    let [rx] = probe(&mut s, [2]);
    s.key(keysyms::KEY_l, None, &info);
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Volume(audio::Target::Output, 50))));
    s.key(keysyms::KEY_h, None, &info);
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Volume(audio::Target::Output, 45))));
    // Output slider, two output devices, input slider, then the apps.
    s.key(keysyms::KEY_j, None, &info);
    assert_eq!(s.selected, 1);
    s.key(keysyms::KEY_Return, None, &info);
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Output(id)) if id == "speakers"));
    s.key(keysyms::KEY_Down, None, &info);
    s.key(keysyms::KEY_Down, None, &info);
    s.key(keysyms::KEY_space, None, &info);
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Mute(audio::Target::Input))));
    s.key(keysyms::KEY_Down, None, &info);
    s.key(keysyms::KEY_Right, None, &info);
    assert!(matches!(
        rx.try_recv(),
        Ok(worker::Command::Volume(audio::Target::Stream(id), 75)) if id == "42"
    ));
    s.key(keysyms::KEY_Up, None, &info);
    s.key(keysyms::KEY_Up, None, &info);
    s.key(keysyms::KEY_Up, None, &info);
    s.key(keysyms::KEY_Up, None, &info);
    assert_eq!(s.selected, 0);
    s.key(keysyms::KEY_Return, None, &info);
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Mute(audio::Target::Output))));
}
#[test]
fn main_page_rows_follow_the_spec_and_hide_what_is_unavailable() {
    let s = state(Page::Main);
    let rows = s.rows(Page::Main, &info());
    let labels: Vec<_> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        ["Wi-Fi", "Bluetooth", "Night light", "Do not disturb", "Code mode", "TTY", "Volume", "Brightness", "Settings", "Lock", "Power"]
    );
    assert_eq!(rows[0].detail, "Home network");
    assert_eq!(rows[1].detail, "Wireless headphones");
    assert!(rows[0].on && rows[1].on && !rows[2].on);
    assert_eq!(rows[0].chevron, Some(Page::Wifi));
    assert_eq!(rows[6].chevron, Some(Page::Audio));
    let none = Info { brightness: None, code_available: false, ..info() };
    let labels: Vec<_> = s.rows(Page::Main, &none).into_iter().map(|r| r.label).collect();
    assert!(!labels.contains(&"Brightness".to_string()) && !labels.contains(&"Code mode".to_string()) && !labels.contains(&"TTY".to_string()));
    let mut off = state(Page::Main);
    off.wifi.enabled = false;
    off.bluetooth.powered = false;
    let rows = off.rows(Page::Main, &info());
    assert_eq!((rows[0].detail.as_str(), rows[1].detail.as_str()), ("Off", "Off"));
}
#[test]
fn tiles_toggle_and_chevrons_slide_to_detail_pages_with_backspace_returning() {
    let mut s = state(Page::Main);
    let info = info();
    let [wifi, bluetooth, audio] = probe(&mut s, [0, 1, 2]);
    assert_eq!(s.key(keysyms::KEY_Return, None, &info), None);
    assert!(matches!(wifi.try_recv(), Ok(worker::Command::Power(false))));
    s.key(keysyms::KEY_j, None, &info);
    s.key(keysyms::KEY_Return, None, &info);
    assert!(matches!(bluetooth.try_recv(), Ok(worker::Command::Power(false))));
    for (key, effect) in [
        (keysyms::KEY_j, Effect::NightLight),
        (keysyms::KEY_j, Effect::DoNotDisturb),
        (keysyms::KEY_j, Effect::CodeMode),
        (keysyms::KEY_j, Effect::TtyMode),
    ] {
        s.key(key, None, &info);
        assert_eq!(s.key(keysyms::KEY_Return, None, &info), Some(effect));
    }
    // Right opens the details of a tile; no animation timer exists while idle.
    s.selected = 0;
    assert!(!s.animating());
    s.key(keysyms::KEY_Right, None, &info);
    assert_eq!(s.page, Page::Wifi);
    assert!(s.animating());
    assert!(matches!(wifi.try_recv(), Ok(worker::Command::Visible(true))));
    assert!(bluetooth.try_recv().is_err());
    s.key(keysyms::KEY_BackSpace, None, &info);
    assert_eq!(s.page, Page::Main);
    assert!(matches!(wifi.try_recv(), Ok(worker::Command::Visible(false))));
    // Space on the volume slider opens the mixer instead of muting.
    s.selected = 6;
    s.key(keysyms::KEY_space, None, &info);
    assert_eq!(s.page, Page::Audio);
    assert!(audio.try_iter().all(|c| !matches!(c, worker::Command::Mute(_))));
    s.key(keysyms::KEY_Escape, None, &info);
    assert!(s.open.is_none());
    assert!(!s.animating());
}
#[test]
fn brightness_and_footer_buttons_ask_the_compositor() {
    let mut s = state(Page::Main);
    let info = info();
    s.selected = 7;
    assert_eq!(s.key(keysyms::KEY_Right, None, &info), Some(Effect::Brightness(0.75)));
    assert_eq!(s.key(keysyms::KEY_h, None, &info), Some(Effect::Brightness(0.65)));
    s.selected = 8;
    assert_eq!(s.key(keysyms::KEY_Return, None, &info), Some(Effect::Settings));
    s.key(keysyms::KEY_j, None, &info);
    assert_eq!(s.key(keysyms::KEY_Return, None, &info), Some(Effect::Lock));
    s.key(keysyms::KEY_j, None, &info);
    assert_eq!(s.key(keysyms::KEY_Return, None, &info), None);
    assert_eq!(s.page, Page::Power);
    let rows = s.rows(Page::Power, &info);
    assert_eq!(rows.len(), 5);
    s.selected = 4;
    assert_eq!(
        s.key(keysyms::KEY_Return, None, &info),
        Some(Effect::Power(crate::topbar::PowerAction::Shutdown))
    );
}
#[test]
fn detail_pages_group_networks_and_devices_into_sections() {
    let mut s = state(Page::Wifi);
    s.wifi.networks[1].saved = Some("/fake/saved".into());
    let rows = s.rows(Page::Wifi, &info());
    let layout: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.section, r.on)).collect();
    assert_eq!(
        layout,
        [
            ("Wi-Fi", "", true),
            ("Home network", "", true),
            ("Studio", "Saved", false),
            ("Guest Wi-Fi", "Nearby", false),
        ]
    );
    // The connected network offers Disconnect; no row repeats the power state.
    let (tx, rx) = mpsc::channel();
    s.workers[0] = Some(worker::Worker::new(tx));
    s.selected = 1;
    s.key(keysyms::KEY_Return, None, &info());
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Disconnect)));
    s.wifi.enabled = false;
    assert_eq!(s.rows(Page::Wifi, &info()).len(), 1);
    let rows = state(Page::Bluetooth).rows(Page::Bluetooth, &info());
    let layout: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.section)).collect();
    assert_eq!(
        layout,
        [
            ("Bluetooth", ""),
            ("Search", ""),
            ("Wireless headphones", "Connected"),
            ("Keyboard", "Paired"),
            ("Travel mouse", "Paired"),
            ("Portable speaker", "Found"),
        ]
    );
    assert_eq!(rows[2].battery, Some(82));
    assert_eq!(rows[4].battery, Some(41));
}
#[test]
fn mixer_lists_output_devices_input_and_apps_with_icons() {
    let rows = state(Page::Audio).rows(Page::Audio, &info());
    let layout: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.section, r.kind, r.on)).collect();
    assert_eq!(layout[0], ("Volume", "Output", RowKind::Slider, false));
    assert_eq!(layout[1], ("Built-in speakers", "Output", RowKind::Entry, true));
    assert_eq!(layout[2], ("Wireless headphones", "Output", RowKind::Entry, false));
    assert_eq!(layout[3], ("Built-in microphone", "Input", RowKind::Slider, false));
    assert!(layout[4..].iter().all(|r| r.1 == "Apps" && r.2 == RowKind::AppSlider));
    assert_eq!(rows[2].icon, "audio-headphones-symbolic");
    assert_eq!(rows[4].icon, "haruna");
    assert_eq!(rows[5].icon, "application-x-executable-symbolic");
    assert!(rows[5].muted);
}
#[test]
fn stream_icons_resolve_through_desktop_entries_and_devices_get_proper_icons() {
    use crate::desktop_entry::{DesktopEntries, DesktopEntry};
    let entry = |id: &str, icon: &str, class: &str| {
        DesktopEntry::parse(
            id,
            &format!("[Desktop Entry]\nType=Application\nName={id}\nExec={id}\nIcon={icon}\nStartupWMClass={class}\n"),
            None,
        )
        .unwrap()
    };
    let mut entries = DesktopEntries::from_entries(vec![
        entry("org.mozilla.firefox", "firefox-brand", "firefox"),
        entry("mpv", "mpv-icon", ""),
    ]);
    let text = r#"[
        {"index":1,"properties":{"application.name":"Firefox","application.process.binary":"firefox"},"volume":{},"mute":false},
        {"index":2,"properties":{"application.name":"mpv Media Player","application.process.binary":"mpv"},"volume":{},"mute":false},
        {"index":3,"properties":{"application.name":"Weird","application.process.binary":"weird-app","application.icon_name":"given"},"volume":{},"mute":false},
        {"index":4,"properties":{"application.name":"Other","application.process.binary":"Other-App"},"volume":{},"mute":false}
    ]"#;
    let mut streams = audio::parse_list(text, true).unwrap();
    audio::resolve_icons(&mut entries, &mut streams);
    let icons: Vec<_> = streams.iter().map(|s| s.icon.as_str()).collect();
    assert_eq!(icons, ["firefox-brand", "mpv-icon", "given", "other-app"]);
    let sinks = r#"[
        {"index":1,"name":"alsa_output.pci.analog","description":"Speakers","properties":{"device.form_factor":"speaker"},"volume":{}},
        {"index":2,"name":"bluez_output.AA.1","description":"Buds","properties":{"device.form_factor":"headset","device.bus":"bluetooth"},"volume":{}},
        {"index":3,"name":"bluez_output.AA.2","description":"Box","properties":{"device.bus":"bluetooth"},"volume":{}},
        {"index":4,"name":"alsa_output.hdmi-stereo","description":"HDMI","properties":{},"volume":{}},
        {"index":5,"name":"alsa_output.usb.analog","description":"Jack","properties":{"device.form_factor":"headphone"},"volume":{}}
    ]"#;
    let icons: Vec<_> = audio::parse_list(sinks, false).unwrap().into_iter().map(|s| s.icon).collect();
    assert_eq!(
        icons,
        [
            "audio-speakers-symbolic",
            "audio-headphones-symbolic",
            "bluetooth-symbolic",
            "video-display-symbolic",
            "audio-headphones-symbolic"
        ]
    );
    let sources = audio::parse_devices(sinks, false, true).unwrap();
    assert!(sources.iter().any(|s| s.icon == "audio-input-microphone-symbolic"));
}
#[test]
fn battery_summary_and_estimates_from_sysfs() {
    let b = |percent, charging, minutes| crate::topbar::Battery { percent, charging, minutes };
    assert_eq!(b(64, false, Some(95)).summary(), "64% · 1 h 35 min left");
    assert_eq!(b(64, true, Some(20)).summary(), "64% · 20 min until full");
    assert_eq!(b(100, true, None).summary(), "100%");
    let root = std::env::temp_dir().join(format!("gavno-battery-minutes-{}", std::process::id()));
    let dir = root.join("BAT0");
    std::fs::create_dir_all(&dir).unwrap();
    for (key, value) in [
        ("type", "Battery"),
        ("capacity", "50"),
        ("status", "Discharging"),
        ("energy_now", "25000000"),
        ("energy_full", "50000000"),
        ("power_now", "10000000"),
    ] {
        std::fs::write(dir.join(key), value).unwrap();
    }
    assert_eq!(crate::topbar::read_battery(&root).unwrap().minutes, Some(150));
    std::fs::write(dir.join("status"), "Charging").unwrap();
    assert_eq!(crate::topbar::read_battery(&root).unwrap().minutes, Some(150));
    std::fs::write(dir.join("power_now"), "0").unwrap();
    assert_eq!(crate::topbar::read_battery(&root).unwrap().minutes, None);
    std::fs::write(dir.join("status"), "Full").unwrap();
    std::fs::write(dir.join("power_now"), "1000").unwrap();
    assert_eq!(crate::topbar::read_battery(&root).unwrap().minutes, None);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn closed_idle_snapshots_never_request_redraw() {
    let mut s = State::default();
    let (wifi, bluetooth, audio) = fake::snapshots();
    for event in [
        worker::Snapshot::Wifi(wifi),
        worker::Snapshot::Bluetooth(bluetooth),
        worker::Snapshot::Audio(audio),
    ] {
        let kind = match &event {
            worker::Snapshot::Wifi(_) => Kind::Wifi,
            worker::Snapshot::Bluetooth(_) => Kind::Bluetooth,
            worker::Snapshot::Audio(_) => Kind::Audio,
        };
        let event = worker::Event {
            kind,
            snapshot: Some(event),
            error: None,
        };
        s.apply_event(event.clone());
        for _ in 0..100 {
            assert!(!s.apply_event(event.clone()));
        }
    }
    let mut wifi = s.wifi.clone();
    wifi.networks[1].strength = 60;
    assert!(!s.apply_event(worker::Event {
        kind: Kind::Wifi,
        snapshot: Some(worker::Snapshot::Wifi(wifi)),
        error: None
    }));
}
struct WifiFake {
    calls: Arc<Mutex<Vec<&'static str>>>,
}
impl wifi::Provider for WifiFake {
    fn snapshot(&mut self) -> Result<wifi::Snapshot, String> {
        self.calls.lock().unwrap().push("snapshot");
        Ok(fake::snapshots().0)
    }
    fn power(&mut self, _: bool) -> Result<(), String> {
        Ok(())
    }
    fn scan(&mut self) -> Result<(), String> {
        self.calls.lock().unwrap().push("scan");
        Ok(())
    }
    fn connect(&mut self, _: &wifi::Network, _: Secret) -> Result<(), String> {
        Ok(())
    }
    fn disconnect(&mut self) -> Result<(), String> {
        Ok(())
    }
}
#[test]
fn wifi_worker_scans_only_on_open_transitions_and_sleeps_when_idle() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fake = WifiFake {
        calls: calls.clone(),
    };
    let (tx, rx) = mpsc::channel();
    let (events, _source) = channel::channel();
    for c in [
        worker::Command::Refresh,
        worker::Command::Visible(false),
        worker::Command::Visible(true),
        worker::Command::Visible(true),
        worker::Command::Refresh,
        worker::Command::Visible(false),
        worker::Command::Refresh,
        worker::Command::Shutdown,
    ] {
        tx.send(c).unwrap();
    }
    worker::wifi_loop(Box::new(fake), rx, events);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "scan")
            .count(),
        1
    );
    // The queued burst ends in Shutdown, so only the initial snapshot is taken.
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "snapshot")
            .count(),
        1
    );
}
#[test]
fn closed_wifi_refreshes_are_coalesced_and_rate_limited() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let (events, _source) = channel::channel();
    let fake = WifiFake {
        calls: calls.clone(),
    };
    let thread = std::thread::spawn(move || worker::wifi_loop(Box::new(fake), rx, events));
    let count = |calls: &Arc<Mutex<Vec<&'static str>>>| {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "snapshot")
            .count()
    };
    // Initial snapshot, then a burst inside the 500 ms window: at most one more,
    // delivered by the one-shot deadline rather than immediately.
    for _ in 0..50 {
        tx.send(worker::Command::Refresh).unwrap();
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(count(&calls), 1);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(count(&calls), 2);
    // Fully idle afterwards: no periodic snapshots.
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(count(&calls), 2);
    tx.send(worker::Command::Shutdown).unwrap();
    thread.join().unwrap();
}
#[test]
fn open_wifi_refresh_burst_costs_one_snapshot() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let (events, _source) = channel::channel();
    tx.send(worker::Command::Visible(true)).unwrap();
    for _ in 0..20 {
        tx.send(worker::Command::Refresh).unwrap();
    }
    let fake = WifiFake {
        calls: calls.clone(),
    };
    let thread = std::thread::spawn(move || worker::wifi_loop(Box::new(fake), rx, events));
    std::thread::sleep(Duration::from_millis(200));
    tx.send(worker::Command::Shutdown).unwrap();
    thread.join().unwrap();
    // Initial snapshot plus one for the whole queued burst.
    assert_eq!(
        calls.lock().unwrap().iter().filter(|c| **c == "snapshot").count(),
        2
    );
}
struct BluetoothFake {
    calls: Arc<Mutex<Vec<bool>>>,
}
impl bluetooth::Provider for BluetoothFake {
    fn snapshot(&mut self) -> Result<bluetooth::Snapshot, String> {
        Ok(fake::snapshots().1)
    }
    fn power(&mut self, _: bool) -> Result<(), String> {
        Ok(())
    }
    fn discovery(&mut self, v: bool) -> Result<(), String> {
        self.calls.lock().unwrap().push(v);
        Ok(())
    }
    fn toggle(&mut self, _: &bluetooth::Device) -> Result<(), String> {
        Ok(())
    }
}
#[test]
fn bluetooth_discovery_requires_open_and_stops_on_close() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let (events, _source) = channel::channel();
    for c in [
        worker::Command::Search,
        worker::Command::Visible(true),
        worker::Command::Search,
        worker::Command::Search,
        worker::Command::Visible(false),
        worker::Command::Search,
        worker::Command::Shutdown,
    ] {
        tx.send(c).unwrap();
    }
    worker::bluetooth_loop(
        Box::new(BluetoothFake {
            calls: calls.clone(),
        }),
        rx,
        events,
    );
    assert_eq!(*calls.lock().unwrap(), [true, false]);
}
fn extras(hints: bool) -> ui::Extras {
    ui::Extras { hints, ..Default::default() }
}
#[test]
fn geometry_and_hits_fit_small_and_fractional_outputs_on_every_page() {
    for size in [(640, 480), (1646, 926), (2133, 1200)] {
        for classic in [false, true] {
            for hints in [false, true] {
                for page in [Page::Main, Page::Wifi, Page::Bluetooth, Page::Audio, Page::Power] {
                    let mut s = state(page);
                    // Many apps so the mixer has to scroll.
                    for i in 0..12 {
                        let mut e = s.audio.streams[0].clone();
                        e.id = format!("{}", 100 + i);
                        s.audio.streams.push(e);
                    }
                    let rows = s.rows(page, &info());
                    let g = ui::geometry(page, &rows, size.into(), classic, 0, extras(hints));
                    let screen = crate::topbar::geometry::rect(0, 0, size.0, size.1);
                    assert!(screen.contains_rect(g.panel), "{page:?} {size:?} {classic}: {:?}", g.panel);
                    if page == Page::Main {
                        assert_eq!(g.panel.size.w, 360.min(size.0 - 16));
                    }
                    let hits = ui::hits(page, &rows, &g);
                    assert!(!hits.is_empty());
                    for (area, _, _) in &hits {
                        assert!(g.panel.contains_rect(*area), "{page:?}: {area:?} outside {:?}", g.panel);
                        assert!(area.to_f64().contains((area.loc.x as f64 + 0.5, area.loc.y as f64 + 0.5)));
                    }
                    // Rows never overlap each other, whatever the scroll position.
                    for (i, a) in g.cells.iter().enumerate() {
                        for b in &g.cells[i + 1..] {
                            assert!(!a.area.overlaps(b.area), "{page:?}: {:?} / {:?}", a.area, b.area);
                        }
                        if g.hints.size.h > 0 {
                            assert!(!a.area.overlaps(g.hints));
                        }
                    }
                }
            }
        }
    }
}
#[test]
fn keyboard_selection_scrolls_the_mixer_without_drawing_outside_the_card() {
    let mut s = state(Page::Audio);
    for i in 0..12 {
        let mut e = s.audio.streams[0].clone();
        e.id = format!("{}", 100 + i);
        s.audio.streams.push(e);
    }
    let rows = s.rows(Page::Audio, &info());
    let size = (1280, 600).into();
    let viewport = ui::viewport(size, false, extras(false));
    let mut offset = 0;
    for selected in 0..rows.len() {
        offset = ui::scroll_to(&rows, selected, offset, viewport);
        let g = ui::geometry(Page::Audio, &rows, size, false, offset, extras(false));
        assert!(g.cells.iter().any(|c| c.row == selected), "row {selected} visible at offset {offset}");
        assert!(g.scrollbar.is_some());
        for cell in &g.cells {
            assert!(g.panel.contains_rect(cell.area));
        }
    }
    for selected in (0..rows.len()).rev() {
        offset = ui::scroll_to(&rows, selected, offset, viewport);
        let g = ui::geometry(Page::Audio, &rows, size, false, offset, extras(false));
        assert!(g.cells.iter().any(|c| c.row == selected));
    }
    // The first row brings its section header back into view.
    assert_eq!(ui::scroll_to(&rows, 0, 5, viewport), 0);
}
#[test]
fn main_page_hits_cover_tiles_chevrons_sliders_mute_and_footer() {
    let mut s = state(Page::Main);
    s.audio.output.as_mut().unwrap().muted = false;
    let rows = s.rows(Page::Main, &info());
    let g = ui::geometry(Page::Main, &rows, (1646, 926).into(), false, 0, extras(false));
    let hits = ui::hits(Page::Main, &rows, &g);
    let top = |point: (f64, f64)| {
        hits.iter().rev().find(|(r, _, _)| r.to_f64().contains(point)).map(|(_, a, slider)| (a.clone(), *slider))
    };
    let center = |r: crate::topbar::geometry::Rect| (r.loc.x as f64 + r.size.w as f64 / 2.0, r.loc.y as f64 + r.size.h as f64 / 2.0);
    let cell = |action: &Action| g.cells.iter().find(|c| &rows[c.row].action == action).copied().unwrap();
    let wifi = cell(&Action::WifiPower);
    assert_eq!(top(center(wifi.area)), Some((Action::WifiPower, false)));
    assert_eq!(top(center(wifi.chevron)), Some((Action::Open(Page::Wifi), false)));
    let volume = cell(&Action::Slider(audio::Target::Output));
    assert_eq!(top(center(volume.track)), Some((Action::Slider(audio::Target::Output), true)));
    assert_eq!(top(center(volume.icon)), Some((Action::Mute(audio::Target::Output), false)));
    assert_eq!(top(center(volume.chevron)), Some((Action::Open(Page::Audio), false)));
    let brightness = cell(&Action::Brightness);
    assert_eq!(top(center(brightness.track)), Some((Action::Brightness, true)));
    for action in [Action::Settings, Action::Lock, Action::Open(Page::Power)] {
        assert_eq!(top(center(cell(&action).area)), Some((action, false)));
    }
    // Tiles are 8 px apart on the grid, sliders and footer below them.
    assert!(volume.area.loc.y > wifi.area.loc.y + wifi.area.size.h);
    // The panel closes outside; a point on the card but between targets does nothing.
    assert_eq!(top((g.panel.loc.x as f64 + 1.0, g.panel.loc.y as f64 + 1.0)), None);
}
#[test]
fn panel_opens_with_a_finite_animation_and_costs_no_frames_when_settled() {
    let mut s = State::default();
    assert!(!s.animating());
    let output = output();
    s.show(Page::Main, &output);
    assert!(s.animating());
    // After the 120 ms opening the next frame is drawn at full size and
    // releases the animation.
    s.anim.opened = Some(std::time::Instant::now() - Duration::from_secs(1));
    assert!(s.animating());
    s.anim.opened = None;
    assert!(!s.animating());
    s.go(Page::Wifi);
    assert!(s.animating());
    s.close();
    assert!(!s.animating());
}
#[test]
fn quick_settings_renders_every_page_in_both_shells() {
    use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
    let mut renderer = unsafe {
        smithay::backend::renderer::gles::GlesRenderer::new(EGLContext::new(&display).unwrap())
    }
    .unwrap();
    let media = crate::media::Snapshot::default();
    for size in [(640, 480), (2133, 1200)] {
        for scale in [1.0, 1.35] {
            for classic in [false, true] {
                for page in [Page::Main, Page::Wifi, Page::Bluetooth, Page::Audio, Page::Power] {
                    let mut s = state(page);
                    s.keyboard = true;
                    s.password = (page == Page::Wifi).then(|| ("/fake/studio".to_string(), Secret::default()));
                    let output = s.open.clone().unwrap();
                    for step in 0..3 {
                        let elements = s.elements(&mut renderer, &output, size.into(), scale, [0.95, 0.64, 0.42, 1.0], "Adwaita", classic, &info(), &media);
                        assert!(!elements.is_empty());
                        assert!(!s.view.hits.is_empty() || s.animating(), "{page:?} step {step}");
                        s.key(keysyms::KEY_j, None, &info());
                        s.anim = Anim::default();
                    }
                    assert!(s.view.panel.size.w > 0);
                }
            }
        }
    }
}
#[test]
fn panel_chords_are_available_in_both_shells_and_code_mode() {
    use crate::input::Action;
    let mut h = crate::state::tests::Harness::new();
    for classic in [false, true] {
        for code in [false, true] {
            h.gv.classic.mode = if classic {
                crate::classic::ShellMode::Classic
            } else {
                crate::classic::ShellMode::Gavno
            };
            h.gv.code_mode = code;
            let mods = smithay::input::keyboard::ModifiersState {
                logo: true,
                shift: true,
                ..Default::default()
            };
            h.gv.config.mod_key = crate::config::ModKey::Super;
            for (key, page) in [
                (keysyms::KEY_n, Page::Wifi),
                (keysyms::KEY_b, Page::Bluetooth),
                (keysyms::KEY_v, Page::Audio),
            ] {
                assert_eq!(
                    h.gv.bind_for(Keysym::from(key), &mods),
                    Some(Action::PanelControls(page))
                );
            }
        }
    }
}
#[test]
fn worker_does_not_emit_duplicate_snapshots() {
    let (tx, rx) = mpsc::channel();
    let (events, source) = channel::channel();
    let mut event_loop = smithay::reexports::calloop::EventLoop::<usize>::try_new().unwrap();
    event_loop
        .handle()
        .insert_source(source, |event, _, count| {
            if let channel::Event::Msg(_) = event {
                *count += 1;
            }
        })
        .unwrap();
    tx.send(worker::Command::Refresh).unwrap();
    tx.send(worker::Command::Shutdown).unwrap();
    worker::wifi_loop(
        Box::new(WifiFake {
            calls: Arc::default(),
        }),
        rx,
        events,
    );
    let mut count = 0;
    event_loop.dispatch(Duration::ZERO, &mut count).unwrap();
    assert_eq!(count, 1);
}
#[test]
fn bluetooth_discovery_expires_without_further_input() {
    struct TimedFake(mpsc::Sender<bool>);
    impl bluetooth::Provider for TimedFake {
        fn snapshot(&mut self) -> Result<bluetooth::Snapshot, String> {
            Ok(bluetooth::Snapshot::default())
        }
        fn power(&mut self, _: bool) -> Result<(), String> {
            Ok(())
        }
        fn discovery(&mut self, value: bool) -> Result<(), String> {
            self.0.send(value).unwrap();
            Ok(())
        }
        fn toggle(&mut self, _: &bluetooth::Device) -> Result<(), String> {
            Ok(())
        }
    }
    let (tx, rx) = mpsc::channel();
    let (calls, results) = mpsc::channel();
    let (events, _source) = channel::channel();
    let thread = std::thread::spawn(move || {
        worker::bluetooth_loop_limit(
            Box::new(TimedFake(calls)),
            rx,
            events,
            Duration::from_millis(20),
        )
    });
    tx.send(worker::Command::Visible(true)).unwrap();
    tx.send(worker::Command::Search).unwrap();
    assert!(results.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(!results.recv_timeout(Duration::from_secs(2)).unwrap());
    tx.send(worker::Command::Shutdown).unwrap();
    thread.join().unwrap();
}
#[test]
fn rfkill_fixture_distinguishes_bluetooth_from_other_radios() {
    let root = std::env::temp_dir().join(format!("gavno-panel-rfkill-{}", std::process::id()));
    let radio = root.join("rfkill0");
    std::fs::create_dir_all(&radio).unwrap();
    std::fs::write(radio.join("type"), "wlan\n").unwrap();
    std::fs::write(radio.join("hard"), "1\n").unwrap();
    assert!(!bluetooth::rfkill_blocked(&root));
    std::fs::write(radio.join("type"), "bluetooth\n").unwrap();
    assert!(bluetooth::rfkill_blocked(&root));
    std::fs::write(radio.join("hard"), "0\n").unwrap();
    assert!(!bluetooth::rfkill_blocked(&root));
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn bar_volume_steps_use_the_same_fake_provider() {
    let mut s = state(Page::Wifi);
    let (tx, rx) = mpsc::channel();
    s.workers[2] = Some(worker::Worker::new(tx));
    assert!(s.master_step(-15.0));
    assert!(matches!(
        rx.try_recv(),
        Ok(worker::Command::Volume(audio::Target::Output, 50))
    ));
    assert!(s.master_step(15.0));
    assert!(matches!(
        rx.try_recv(),
        Ok(worker::Command::Volume(audio::Target::Output, 45))
    ));
    assert!(s.master_step(0.0));
    assert!(rx.try_recv().is_err());
}

#[test]
fn text_and_icons_stay_readable_on_any_accent() {
    // WCAG contrast ratio of the colour used on the accent fill.
    let linear = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    let luminance = |c: [f32; 4]| 0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2]);
    for accent in [
        [0.95, 0.64, 0.42, 1.0],
        [0.74, 0.65, 0.69, 1.0],
        [0.20, 0.45, 0.90, 1.0],
        [0.10, 0.10, 0.30, 1.0],
        [0.95, 0.95, 0.95, 1.0],
        [0.45, 0.80, 0.50, 1.0],
    ] {
        let text = ui::on_accent(accent);
        let (a, b) = (luminance(accent) + 0.05, luminance(text) + 0.05);
        let ratio = a.max(b) / a.min(b);
        assert!(ratio >= 3.0, "{accent:?} on {text:?}: {ratio}");
    }
}
fn bar_test_gavno() -> (crate::state::tests::Harness, Output) {
    use smithay::output::{Mode, PhysicalProperties, Subpixel};
    let mut h = crate::state::tests::Harness::new();
    let output = Output::new(
        "panel-gv".into(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "test".into(), model: "test".into() },
    );
    output.change_current_state(Some(Mode { size: (1646, 926).into(), refresh: 120000 }), None, None, None);
    h.gv.space.map_output(&output, (0, 0));
    h.gv.workspaces.ensure(&output);
    let (wifi, bluetooth, audio) = fake::snapshots();
    h.gv.topbar.controls.wifi = wifi;
    h.gv.topbar.controls.bluetooth = bluetooth;
    h.gv.topbar.controls.audio = audio;
    (h, output)
}
/// Publishes the pointer targets the renderer would have computed.
fn publish_hits(h: &mut crate::state::tests::Harness, output: &Output) {
    let info = h.gv.panel_info();
    let s = &mut h.gv.topbar.controls;
    let rows = s.rows(s.page, &info);
    let size = h.gv.space.output_geometry(output).unwrap().size;
    let g = ui::geometry(s.page, &rows, size, false, s.offset, extras(s.keyboard));
    s.view.hits = ui::hits(s.page, &rows, &g);
    s.view.panel = g.panel;
}
#[test]
fn gavno_routes_chords_keys_clicks_and_effects_through_the_panel() {
    let (mut h, o) = bar_test_gavno();
    let mods = ModifiersState::default();
    let key = |h: &mut crate::state::tests::Harness, sym: u32| {
        h.gv.panel_controls_key(Keysym::from(sym), None, &mods, 40);
    };
    // The chord for a page opens it, the same chord closes it, another slides.
    h.gv.toggle_panel_controls(Page::Wifi, &o);
    assert!(h.gv.panel_controls_open());
    assert_eq!(h.gv.topbar.controls.page, Page::Wifi);
    h.gv.toggle_panel_controls(Page::Audio, &o);
    assert_eq!(h.gv.topbar.controls.page, Page::Audio);
    assert!(h.gv.topbar.controls.animating());
    key(&mut h, keysyms::KEY_BackSpace);
    assert_eq!(h.gv.topbar.controls.page, Page::Main);
    h.gv.toggle_panel_controls(Page::Main, &o);
    assert!(!h.gv.panel_controls_open());
    // The status cluster click is the same toggle, in both shells.
    h.gv.bar_action(&o, crate::topbar::BarAction::PowerMenu);
    assert!(h.gv.panel_controls_open());
    key(&mut h, keysyms::KEY_Escape);
    assert!(!h.gv.panel_controls_open());
    h.gv.bar_action(&o, crate::topbar::BarAction::PowerMenu);

    // Tiles ask the compositor for night light and Do not disturb.
    publish_hits(&mut h, &o);
    let tile = |h: &crate::state::tests::Harness, action: Action| {
        h.gv.topbar.controls.view.hits.iter().rev().find(|(_, a, _)| *a == action).map(|(r, _, _)| { let p = r.to_f64().loc; (p.x + 3.0, p.y + 3.0).into() }).unwrap()
    };
    let night = h.gv.config.display.night_light;
    let point = tile(&h, Action::NightLight);
    assert!(h.gv.panel_controls_button(point, true));
    assert_eq!(h.gv.config.display.night_light, !night);
    let dnd = h.gv.notifications.borrow().dnd;
    let point = tile(&h, Action::DoNotDisturb);
    h.gv.panel_controls_button(point, true);
    assert_eq!(h.gv.notifications.borrow().dnd, !dnd);
    assert!(h.gv.panel_controls_open());
    // The chevron slides to the Wi-Fi page.
    publish_hits(&mut h, &o);
    let point = tile(&h, Action::Open(Page::Wifi));
    h.gv.panel_controls_button(point, true);
    assert_eq!(h.gv.topbar.controls.page, Page::Wifi);
    // A click outside closes without activating anything underneath.
    publish_hits(&mut h, &o);
    assert!(h.gv.panel_controls_button((10.0, 600.0).into(), true));
    assert!(!h.gv.panel_controls_open());
}
#[test]
fn scrolling_the_cluster_steps_volume_by_five_and_sliders_drag() {
    use crate::topbar::geometry::Layout;
    let (mut h, o) = bar_test_gavno();
    let [rx] = probe(&mut h.gv.topbar.controls, [2]);
    let layout = Layout::with_battery(1646, 55, true, 60);
    let status = layout.status;
    h.gv.topbar.test_view(&o, layout);
    let inside = (status.loc.x as f64 + 4.0, 16.0).into();
    assert!(h.gv.bar_axis(&o, inside, -15.0));
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Volume(audio::Target::Output, 50))));
    assert!(h.gv.bar_axis(&o, inside, 15.0));
    assert!(matches!(rx.try_recv(), Ok(worker::Command::Volume(audio::Target::Output, 45))));
    // Dragging the volume slider on the panel follows the pointer along the track.
    h.gv.toggle_panel_controls(Page::Main, &o);
    publish_hits(&mut h, &o);
    let (area, action, slider) = h.gv.topbar.controls.view.hits.iter().rev()
        .find(|(_, a, _)| *a == Action::Slider(audio::Target::Output)).cloned().unwrap();
    assert!(slider);
    let y = area.loc.y as f64 + 10.0;
    assert!(h.gv.panel_controls_button((area.loc.x as f64 + area.size.w as f64 / 2.0, y).into(), true));
    assert!(h.gv.topbar.controls.drag.is_some());
    assert!(rx.try_iter().any(|c| matches!(c, worker::Command::Volume(audio::Target::Output, 50))));
    h.gv.panel_controls_motion((area.loc.x as f64 + area.size.w as f64 * 0.2, y).into());
    assert!(rx.try_iter().any(|c| matches!(c, worker::Command::Volume(audio::Target::Output, 20))));
    h.gv.panel_controls_button((0.0, 0.0).into(), false);
    assert!(h.gv.topbar.controls.drag.is_none());
    let _ = action;
    // Brightness goes through the backlight path without an OSD.
    let (tx, brightness) = std::sync::mpsc::channel();
    h.gv.media.worker = Some(tx);
    h.gv.bar_status(crate::topbar::status::Event::Status(fake::status()));
    h.gv.panel_effect(&o, Effect::Brightness(0.4));
    assert!(matches!(brightness.try_recv(), Ok(crate::topbar::status::Command::Brightness(v)) if v == 0.4));
    assert!(h.gv.media.osd_timer.is_none());
}
