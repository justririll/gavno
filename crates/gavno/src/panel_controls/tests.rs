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
fn state(kind: Kind) -> State {
    let (wifi, bluetooth, audio) = fake::snapshots();
    State {
        wifi,
        bluetooth,
        audio,
        open: Some((kind, output())),
        ..Default::default()
    }
}
#[test]
fn legacy_status_cannot_overwrite_panel_volume_or_resurrect_a_removed_output() {
    use crate::topbar::status;
    let mut h = crate::state::tests::Harness::new();
    let legacy_volume = Some(status::Volume { level: 0.5, muted: false });
    let legacy = status::Snapshot {
        volume: legacy_volume,
        brightness: Some(status::Brightness { name: "fake-panel".into(), value: 70, max: 100 }),
        battery: Some(crate::topbar::Battery { percent: 42, charging: false }),
        network: status::Network::Wired,
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
    let mut s = state(Kind::Wifi).wifi;
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
    let mut s = state(Kind::Wifi);
    s.activate(Action::Network("/fake/studio".into()));
    for ch in "private-password🔑".chars() {
        s.key(0, Some(ch));
    }
    let debug = format!("{:?}", s.password);
    assert!(!debug.contains("private"));
    assert!(!debug.contains("🔑"));
    assert!(debug.contains("REDACTED"));
    s.key(keysyms::KEY_BackSpace, None);
    assert_eq!(
        s.password.as_ref().unwrap().1 .0.as_str(),
        "private-password"
    );
    s.key(keysyms::KEY_Escape, None);
    assert!(s.password.is_none());
    assert!(s.open.is_some());
    s.key(keysyms::KEY_Escape, None);
    assert!(s.open.is_none());
}
#[test]
fn keyboard_navigation_slider_steps_and_mute_use_worker_commands() {
    let mut s = state(Kind::Audio);
    let (tx, rx) = mpsc::channel();
    s.workers[2] = Some(worker::Worker { sender: tx });
    s.key(keysyms::KEY_l, None);
    assert!(matches!(
        rx.try_recv(),
        Ok(worker::Command::Volume(audio::Target::Output, 50))
    ));
    s.key(keysyms::KEY_h, None);
    assert!(matches!(
        rx.try_recv(),
        Ok(worker::Command::Volume(audio::Target::Output, 45))
    ));
    s.key(keysyms::KEY_j, None);
    assert_eq!(s.selected, 1);
    s.key(keysyms::KEY_space, None);
    assert!(matches!(
        rx.try_recv(),
        Ok(worker::Command::Mute(audio::Target::Input))
    ));
    s.key(keysyms::KEY_Up, None);
    assert_eq!(s.selected, 0);
    s.key(keysyms::KEY_Return, None);
    assert!(matches!(
        rx.try_recv(),
        Ok(worker::Command::Mute(audio::Target::Output))
    ));
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
    // Each query requires one command; there is no timeout or periodic refresh path.
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "snapshot")
            .count(),
        8
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
#[test]
fn popover_geometry_and_hit_testing_fit_small_and_fractional_outputs() {
    use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
    let mut renderer = unsafe {
        smithay::backend::renderer::gles::GlesRenderer::new(EGLContext::new(&display).unwrap())
    }
    .unwrap();
    for size in [(640, 480), (2133, 1200)] {
        for scale in [1.0, 1.35] {
            for kind in [Kind::Wifi, Kind::Bluetooth, Kind::Audio] {
                for classic in [false, true] {
                    let mut s = state(kind);
                    let output = s.open.as_ref().unwrap().1.clone();
                    let (panel, _) = ui::geometry(size.into(), classic, s.rows(kind).len());
                    assert!(
                        panel.loc.x >= 0
                            && panel.loc.y >= 0
                            && panel.loc.x + panel.size.w <= size.0
                            && panel.loc.y + panel.size.h <= size.1
                    );
                    s.elements(
                        &mut renderer,
                        &output,
                        size.into(),
                        scale,
                        [0.95, 0.64, 0.42, 1.0],
                        "Adwaita",
                        classic,
                    );
                    assert!(!s.view.hits.is_empty());
                    for (area, _, _) in &s.view.hits {
                        assert!(panel.contains_rect(*area));
                        assert!(area
                            .to_f64()
                            .contains((area.loc.x as f64 + 0.5, area.loc.y as f64 + 0.5)));
                    }
                    for _ in 0..20 {
                        s.key(keysyms::KEY_j, None);
                    }
                    s.elements(
                        &mut renderer,
                        &output,
                        size.into(),
                        scale,
                        [0.95, 0.64, 0.42, 1.0],
                        "Adwaita",
                        classic,
                    );
                    assert!(
                        s.selected
                            < s.offset + ui::geometry(size.into(), classic, s.rows(kind).len()).1
                    );
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
            for (key, kind) in [
                (keysyms::KEY_n, Kind::Wifi),
                (keysyms::KEY_b, Kind::Bluetooth),
                (keysyms::KEY_v, Kind::Audio),
            ] {
                assert_eq!(
                    h.gv.bind_for(Keysym::from(key), &mods),
                    Some(Action::PanelControls(kind))
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
    let mut s = state(Kind::Wifi);
    let (tx, rx) = mpsc::channel();
    s.workers[2] = Some(worker::Worker { sender: tx });
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
