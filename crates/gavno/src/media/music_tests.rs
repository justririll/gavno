//! Contract tests use a private session bus and never contact Spotify.
use super::*;
use crate::state::tests::Harness;
use smithay::{
    backend::{
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::gles::GlesRenderer,
    },
    output::{Mode, PhysicalProperties, Subpixel},
};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Instant,
};

struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn bus() -> Option<(Bus, String)> {
    bus_with_data(None)
}
fn bus_with_data(data: Option<&std::path::Path>) -> Option<(Bus, String)> {
    let mut command = Command::new("dbus-daemon");
    if let Some(data) = data {
        command.env("XDG_DATA_HOME", data);
    }
    let Ok(child) = command
        .env_remove("XDG_RUNTIME_DIR")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
    else {
        eprintln!("skipping music bus test: dbus-daemon unavailable");
        return None;
    };
    let mut bus = Bus(child);
    let mut address = String::new();
    BufReader::new(bus.0.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    Some((bus, address.trim().to_owned()))
}
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.gavno.Music1.Error")]
enum Error {
    Failed(String),
    RateLimited(String),
    LoggedOut(String),
}
#[derive(Default)]
struct FakeData {
    calls: Vec<(String, String, String)>,
    state: State,
    sender: String,
    slow_complete: bool,
    slow_reply: Option<async_channel::Sender<()>>,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<FakeData>>);
impl Fake {
    fn calls(&self, name: &str) -> Vec<(String, String)> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(n, _, _)| n == name)
            .map(|(_, a, b)| (a.clone(), b.clone()))
            .collect()
    }
    fn record(&self, name: &str, a: &str, b: &str) {
        self.0
            .lock()
            .unwrap()
            .calls
            .push((name.into(), a.into(), b.into()));
    }
}
fn item(kind: &str, title: &str) -> Item {
    Item {
        uri: format!("spotify:{kind}:test"),
        kind: kind.into(),
        title: title.into(),
        subtitle: "Artist".into(),
        cover_url: Some("https://test.invalid/cover".into()),
        duration_ms: 200000,
        ..Default::default()
    }
}
fn page(title: &str) -> Page {
    Page {
        title: title.into(),
        items: vec![
            item("section", "Recommended"),
            item("track", "Song"),
            item("album", "Album"),
        ],
        next: Some("next-page".into()),
        play_uri: Some("spotify:playlist:context".into()),
        ..Default::default()
    }
}
#[zbus::interface(name = "org.gavno.Music1")]
impl Fake {
    fn state(&self, #[zbus(header)] header: zbus::message::Header<'_>) -> String {
        let mut data = self.0.lock().unwrap();
        data.sender = header.sender().unwrap().to_string();
        serde_json::to_string(&data.state).unwrap()
    }
    async fn browse(&self, view: &str, arg: &str) -> Result<String, Error> {
        self.record("Browse", view, arg);
        match arg {
            "limited" => return Err(Error::RateLimited("Try again later".into())),
            "failed" => return Err(Error::Failed("Cannot browse".into())),
            "loggedout" => return Err(Error::LoggedOut("Please log in".into())),
            "slow" => {
                let (tx, rx) = async_channel::bounded(1);
                {
                    let mut data = self.0.lock().unwrap();
                    data.slow_complete = false;
                    data.slow_reply = Some(tx);
                }
                let _ = rx.recv().await;
                self.0.lock().unwrap().slow_complete = true;
            }
            _ => (),
        }
        let mut result = page(if view == "entity" {
            "Album tracks"
        } else {
            view
        });
        if view == "more" {
            result.items = vec![item("track", "More music")];
            result.next = None;
        }
        Ok(serde_json::to_string(&result).unwrap())
    }
    fn play(&self, uri: &str, context_uri: &str) {
        self.record("Play", uri, context_uri);
    }
    fn control(&self, command: &str) {
        self.record("Control", command, "");
    }
    fn seek(&self, position_ms: u32) {
        self.record("Seek", &position_ms.to_string(), "");
    }
    fn set_volume(&self, volume: f64) {
        self.record("SetVolume", &volume.to_string(), "");
    }
    fn login(&self) {
        self.record("Login", "", "");
    }
}
fn setup(address: &str) -> (Harness, Output) {
    let mut h = Harness::new();
    let output = Output::new(
        "music-test".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    );
    output.change_current_state(
        Some(Mode {
            size: (1280, 800).into(),
            refresh: 60000,
        }),
        None,
        None,
        None,
    );
    h.gv.space.map_output(&output, (0, 0));
    h.gv.workspaces.ensure(&output);
    h.gv.now_playing.borrow_mut().music.bus_address = Some(address.into());
    (h, output)
}
fn fake(address: &str, logged_in: bool) -> (Fake, zbus::blocking::Connection) {
    let fake = Fake::default();
    fake.0.lock().unwrap().state = State {
        logged_in,
        volume: 0.5,
        item: Some(item("track", "Header")),
        ..Default::default()
    };
    let connection = zbus::blocking::connection::Builder::address(address)
        .unwrap()
        .name("org.gavno.Music")
        .unwrap()
        .serve_at("/org/gavno/Music", fake.clone())
        .unwrap()
        .build()
        .unwrap();
    (fake, connection)
}
fn pump(h: &mut Harness, mut done: impl FnMut(&Gavno) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(&h.gv) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for music worker"
        );
        h.event_loop
            .dispatch(Some(Duration::from_millis(10)), &mut h.gv)
            .unwrap();
    }
}
fn key(gv: &mut Gavno, sym: u32) {
    gv.music_key(Keysym::from(sym), None, &Default::default());
}
fn text(gv: &mut Gavno, value: &str) {
    for ch in value.chars() {
        gv.music_key(Keysym::from(keysyms::KEY_a), Some(ch), &Default::default());
    }
}
fn renderer() -> GlesRenderer {
    let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
    unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap()
}
fn draw(h: &mut Harness, renderer: &mut GlesRenderer, output: &Output) {
    let mut media = h.gv.now_playing.borrow_mut();
    let s = media.music.session.as_mut().unwrap();
    let elements = s.elements(renderer, output, (1280, 800).into(), 1.0);
    assert!(!elements.is_empty());
    if let Some(directory) = std::env::var_os("GAVNO_MUSIC_TEST_ARTIFACTS") {
        let bytes = crate::render::capture_elements(renderer, &elements, (1280, 800), 1.0).unwrap();
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let mut encoder = png::Encoder::new(
            std::fs::File::create(directory.join("music-panel.png")).unwrap(),
            1280,
            800,
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&bytes)
            .unwrap();
    }
}
fn click(h: &mut Harness, hit: Hit) {
    let area =
        h.gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .view
            .hits
            .iter()
            .find(|(_, h)| *h == hit)
            .unwrap()
            .0;
    assert!(h.gv.music_button(
        (area.loc.x as f64 + 1.0, area.loc.y as f64 + 1.0).into(),
        true
    ));
}
#[test]
fn private_daemon_browsing_search_navigation_signals_and_controls() {
    let Some((_bus, address)) = bus() else { return };
    let (fake, connection) = fake(&address, true);
    let (mut h, output) = setup(&address);
    assert!(!h.gv.music_open());
    h.gv.toggle_music(&output);
    pump(&mut h, |gv| {
        gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .is_some_and(|s| !s.loading && s.state.logged_in)
    });
    assert_eq!(fake.calls("Browse"), [("home".into(), "".into())]);
    let mut renderer = renderer();
    draw(&mut h, &mut renderer, &output);
    click(&mut h, Hit::Row(1));
    pump(&mut h, |_| !fake.calls("Play").is_empty());
    assert_eq!(
        fake.calls("Play"),
        [(
            "spotify:track:test".into(),
            "spotify:playlist:context".into()
        )]
    );
    click(&mut h, Hit::Row(2));
    pump(&mut h, |gv| {
        gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .page
            .title
            == "Album tracks"
    });
    key(&mut h.gv, keysyms::KEY_Left); // Plain Left is consumed without navigating.
    key(&mut h.gv, keysyms::KEY_BackSpace);
    assert_eq!(
        h.gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .page
            .title,
        "home"
    );
    text(&mut h.gv, "привет");
    assert!(fake.calls("Browse").iter().all(|(v, _)| v != "search"));
    assert!(h
        .gv
        .now_playing
        .borrow()
        .music
        .session
        .as_ref()
        .unwrap()
        .debounce
        .is_some());
    pump(&mut h, |_| {
        fake.calls("Browse").iter().any(|(v, _)| v == "search")
    });
    pump(&mut h, |gv| {
        !gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .loading
    });
    assert_eq!(
        fake.calls("Browse")
            .iter()
            .filter(|(v, _)| v == "search")
            .collect::<Vec<_>>(),
        [&("search".into(), "привет".into())]
    );
    for _ in 0..6 {
        key(&mut h.gv, keysyms::KEY_BackSpace);
    }
    assert_eq!(
        h.gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .page
            .title,
        "home"
    );
    // Load more appends instead of replacing the context and initial rows.
    draw(&mut h, &mut renderer, &output);
    click(&mut h, Hit::More);
    pump(&mut h, |gv| {
        gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .page
            .items
            .len()
            == 4
    });
    // Cover decoding occurs on the worker and the compositor uploads when drawing.
    let path = std::env::temp_dir().join(format!("gavno-music-cover-{}.png", std::process::id()));
    {
        let mut encoder = png::Encoder::new(std::fs::File::create(&path).unwrap(), 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[220, 20, 80, 255])
            .unwrap();
    }
    connection
        .emit_signal(
            None::<&str>,
            "/org/gavno/Music",
            "org.gavno.Music1",
            "CoverReady",
            &("https://test.invalid/cover", path.to_str().unwrap()),
        )
        .unwrap();
    pump(&mut h, |gv| {
        !gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .view
            .covers
            .is_empty()
    });
    assert_eq!(
        h.gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .page
            .items[1]
            .cover_path
            .as_deref(),
        path.to_str()
    );
    let state = State {
        logged_in: true,
        playing: true,
        item: Some(item("track", "Changed header")),
        position_ms: 1000,
        position_at_ms: epoch_ms() - 2000,
        volume: 0.7,
        ..Default::default()
    };
    connection
        .emit_signal(
            None::<&str>,
            "/org/gavno/Music",
            "org.gavno.Music1",
            "StateChanged",
            &(serde_json::to_string(&state).unwrap(),),
        )
        .unwrap();
    pump(&mut h, |gv| {
        gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .state
            .playing
    });
    {
        let media = h.gv.now_playing.borrow();
        let s = media.music.session.as_ref().unwrap();
        assert_eq!(s.state.item.as_ref().unwrap().title, "Changed header");
        assert!(s.state.position() >= 3000);
        assert!(s.tick.is_some());
    }
    draw(&mut h, &mut renderer, &output);
    click(&mut h, Hit::Control("toggle"));
    click(&mut h, Hit::Seek);
    click(&mut h, Hit::Volume);
    pump(&mut h, |_| !fake.calls("SetVolume").is_empty());
    assert_eq!(fake.calls("Control")[0].0, "toggle");
    assert!(!fake.calls("Seek").is_empty());
    // Static render frames reuse handles rather than re-laying out the panel.
    assert!(!h
        .gv
        .now_playing
        .borrow()
        .music
        .session
        .as_ref()
        .unwrap()
        .view
        .covers
        .is_empty());
    let (source, tick, worker) = {
        let media = h.gv.now_playing.borrow();
        let s = media.music.session.as_ref().unwrap();
        (s.source.unwrap(), s.tick.unwrap(), s.worker.clone())
    };
    h.gv.close_music();
    assert!(h.gv.now_playing.borrow().music.session.is_none());
    assert!(worker.is_closed());
    assert!(h.gv.loop_handle.update(&source).is_err());
    assert!(h.gv.loop_handle.update(&tick).is_err());
    let sender = fake.0.lock().unwrap().sender.clone();
    let bus_proxy = zbus::blocking::fdo::DBusProxy::new(&connection).unwrap();
    pump(&mut h, |_| {
        !bus_proxy
            .name_has_owner(zbus::names::BusName::try_from(sender.as_str()).unwrap())
            .unwrap()
    });
    std::fs::remove_file(path).unwrap();
}
#[test]
fn cached_tabs_errors_login_stale_replies_and_close_cleanup() {
    let Some((_bus, address)) = bus() else { return };
    let (fake, connection) = fake(&address, false);
    let (mut h, output) = setup(&address);
    h.gv.toggle_music(&output);
    pump(&mut h, |gv| {
        !gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .loading
    });
    let mut renderer = renderer();
    draw(&mut h, &mut renderer, &output);
    click(&mut h, Hit::Login);
    pump(&mut h, |_| !fake.calls("Login").is_empty());
    for query in ["limited", "failed", "loggedout"] {
        {
            let mut media = h.gv.now_playing.borrow_mut();
            let s = media.music.session.as_mut().unwrap();
            s.browse("search", query.into());
        }
        pump(&mut h, |gv| {
            !gv.now_playing
                .borrow()
                .music
                .session
                .as_ref()
                .unwrap()
                .loading
        });
        assert!(!h
            .gv
            .now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .error
            .is_empty());
    }
    {
        let mut media = h.gv.now_playing.borrow_mut();
        media
            .music
            .session
            .as_mut()
            .unwrap()
            .browse("search", "slow".into());
    }
    pump(&mut h, |_| {
        fake.calls("Browse").iter().any(|(_, q)| q == "slow")
    });
    h.gv.now_playing
        .borrow()
        .music
        .session
        .as_ref()
        .unwrap()
        .send(worker::Command::Control("next".into()));
    pump(&mut h, |_| !fake.calls("Control").is_empty());
    assert!(
        !fake.0.lock().unwrap().slow_complete,
        "control waited for Browse"
    );
    // Returning to a cached tab invalidates a reply already in flight.
    h.gv.music_key(
        Keysym::from(keysyms::KEY_1),
        None,
        &ModifiersState {
            ctrl: true,
            ..Default::default()
        },
    );
    fake.0
        .lock()
        .unwrap()
        .slow_reply
        .take()
        .unwrap()
        .try_send(())
        .unwrap();
    pump(&mut h, |_| fake.0.lock().unwrap().slow_complete);
    h.event_loop
        .dispatch(Some(Duration::from_millis(100)), &mut h.gv)
        .unwrap();
    assert_eq!(
        h.gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .page
            .title,
        "home"
    );
    for sym in [
        keysyms::KEY_2,
        keysyms::KEY_3,
        keysyms::KEY_1,
        keysyms::KEY_2,
    ] {
        h.gv.music_key(
            Keysym::from(sym),
            None,
            &ModifiersState {
                ctrl: true,
                ..Default::default()
            },
        );
        pump(&mut h, |gv| {
            !gv.now_playing
                .borrow()
                .music
                .session
                .as_ref()
                .unwrap()
                .loading
        });
    }
    assert_eq!(
        fake.calls("Browse")
            .iter()
            .filter(|(v, _)| v == "library")
            .count(),
        1
    );
    text(&mut h.gv, "pending search");
    assert!(h
        .gv
        .now_playing
        .borrow()
        .music
        .session
        .as_ref()
        .unwrap()
        .debounce
        .is_some());
    let debounce =
        h.gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .debounce
            .unwrap();
    h.gv.close_music();
    assert!(h.gv.now_playing.borrow().music.session.is_none());
    assert!(h.gv.loop_handle.update(&debounce).is_err());
    let before = fake.calls("Browse").len();
    h.event_loop
        .dispatch(Some(Duration::from_millis(400)), &mut h.gv)
        .unwrap();
    assert_eq!(fake.calls("Browse").len(), before);
    // No timer or panel allocation is created by the closed-session idle gate.
    assert_eq!(
        crate::classic::cache::performance::count(|| {
            for _ in 0..100 {
                h.gv.refresh_music_gate();
            }
        }),
        0
    );
    drop(connection);
}
#[test]
fn missing_daemon_config_gate_lock_and_feature_recovery() {
    let Some((_bus, address)) = bus() else { return };
    let (mut h, output) = setup(&address);
    h.gv.config.media.music_panel = false;
    h.gv.toggle_music(&output);
    assert!(!h.gv.music_open());
    h.gv.config.media.music_panel = true;
    h.gv.protocols.locks.locked = true;
    h.gv.toggle_music(&output);
    assert!(!h.gv.music_open());
    h.gv.protocols.locks.locked = false;
    h.gv.toggle_music(&output);
    pump(&mut h, |gv| {
        !gv.now_playing
            .borrow()
            .music
            .session
            .as_ref()
            .unwrap()
            .error
            .is_empty()
    });
    assert!(h.gv.features.on(Feature::Media));
    h.gv.protocols.locks.locked = true;
    h.gv.refresh_music_gate();
    assert!(!h.gv.music_open());
    h.gv.protocols.locks.locked = false;
    h.gv.toggle_music(&output);
    text(&mut h.gv, "timer");
    h.gv.features.inject_panic(Feature::Media);
    key(&mut h.gv, keysyms::KEY_a);
    assert!(!h.gv.music_open());
    assert!(h.gv.now_playing.borrow().music.session.is_none());
    assert!(!h.gv.features.on(Feature::Media));
}
#[test]
fn panel_geometry_is_clamped_and_position_is_bounded() {
    for size in [(1280, 800), (640, 480), (320, 240), (100, 80)] {
        let size: smithay::utils::Size<i32, Logical> = size.into();
        assert!(smithay::utils::Rectangle::from_size(size).contains_rect(ui::panel_rect(size)));
    }
    let state = State {
        playing: true,
        position_ms: 900,
        position_at_ms: 0,
        item: Some(Item {
            duration_ms: 1000,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(state.position(), 1000);
}

#[test]
fn discovers_activatable_service_without_starting_it() {
    let directory =
        std::env::temp_dir().join(format!("gavno-music-activation-{}", std::process::id()));
    let services = directory.join("dbus-1/services");
    std::fs::create_dir_all(&services).unwrap();
    std::fs::write(
        services.join("org.gavno.Music.service"),
        "[D-BUS Service]\nName=org.gavno.Music\nExec=/bin/false\n",
    )
    .unwrap();
    let Some((_bus, address)) = bus_with_data(Some(&directory)) else {
        std::fs::remove_dir_all(directory).unwrap();
        return;
    };
    let connection = zbus::blocking::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .unwrap();
    let proxy = zbus::blocking::fdo::DBusProxy::new(&connection).unwrap();
    assert!(crate::media::mpris::discover_music(&proxy));
    assert!(!proxy
        .name_has_owner(zbus::names::BusName::try_from("org.gavno.Music").unwrap())
        .unwrap());
    std::fs::remove_dir_all(directory).unwrap();
}
