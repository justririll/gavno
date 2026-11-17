//! Blocking bus calls and image decoding live here, never on the compositor.
use super::{active_player, Command, Metadata, Playback, Player, Snapshot};
use smithay::reexports::calloop::channel;
use std::{collections::HashMap, path::PathBuf, sync::mpsc};
use zbus::{
    blocking::{Connection, MessageIterator, Proxy},
    zvariant::OwnedValue,
};
const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
type Properties = HashMap<String, OwnedValue>;

pub enum Request {
    Command(Command),
    MusicCommand(Command),
    Signal(zbus::Message),
    Disconnected,
}
fn string(values: &Properties, key: &str) -> String {
    values
        .get(key)
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect()
}
pub(super) fn metadata(values: &Properties) -> Metadata {
    Metadata {
        title: string(values, "xesam:title"),
        artists: values
            .get("xesam:artist")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Vec::<String>::try_from(v).ok())
            .unwrap_or_default()
            .into_iter()
            .take(32)
            .map(|s| s.chars().filter(|c| !c.is_control()).take(256).collect())
            .collect(),
        art_url: string(values, "mpris:artUrl"),
        length: values
            .get("mpris:length")
            .and_then(|v| i64::try_from(v).ok())
            .filter(|n| *n >= 0),
    }
}
/// Accept local file URIs only, including non-UTF-8 Unix filenames.
pub(super) fn file_url(url: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let path = url.strip_prefix("file://")?;
    let path = path.strip_prefix("localhost").unwrap_or(path);
    if !path.starts_with('/') || path.contains(['?', '#']) {
        return None;
    }
    let mut bytes = path.bytes();
    let mut decoded = Vec::new();
    while let Some(b) = bytes.next() {
        let b = if b == b'%' {
            let a = (bytes.next()? as char).to_digit(16)?;
            let b = (bytes.next()? as char).to_digit(16)?;
            (a * 16 + b) as u8
        } else {
            b
        };
        if b == 0 {
            return None;
        }
        decoded.push(b);
    }
    Some(std::ffi::OsString::from_vec(decoded).into())
}
fn properties(connection: &Connection, owner: &str, interface: &str) -> zbus::Result<Properties> {
    Proxy::new(connection, owner, PATH, "org.freedesktop.DBus.Properties")?
        .call("GetAll", &(interface,))
}
fn apply_properties(player: &mut Player, interface: &str, props: &Properties) {
    if interface == ROOT {
        if props.contains_key("Identity") {
            player.identity = string(props, "Identity");
        }
        if props.contains_key("DesktopEntry") {
            player.desktop = string(props, "DesktopEntry");
        }
        return;
    }
    if props.contains_key("PlaybackStatus") {
        player.playback = match string(props, "PlaybackStatus").as_str() {
            "Playing" => Playback::Playing,
            "Paused" => Playback::Paused,
            _ => Playback::Stopped,
        };
    }
    if let Some(value) = props.get("Metadata") {
        let meta = value
            .try_clone()
            .ok()
            .and_then(|v| Properties::try_from(v).ok())
            .unwrap_or_default();
        player.metadata = metadata(&meta);
    }
    for (key, flag) in [
        ("CanGoNext", &mut player.can_next),
        ("CanGoPrevious", &mut player.can_previous),
        ("CanPlay", &mut player.can_play),
        ("CanPause", &mut player.can_pause),
    ] {
        if let Some(value) = props.get(key) {
            *flag = bool::try_from(value).unwrap_or(false);
        }
    }
}
fn read_player(connection: &Connection, name: &str, owner: &str) -> zbus::Result<Player> {
    let root = properties(connection, owner, ROOT).unwrap_or_default();
    let props = properties(connection, owner, PLAYER)?;
    let mut player = Player {
        name: name.into(),
        ..Default::default()
    };
    apply_properties(&mut player, ROOT, &root);
    apply_properties(&mut player, PLAYER, &props);
    Ok(player)
}
#[derive(Default)]
struct Registry {
    players: Vec<Player>,
    owners: HashMap<String, String>,
    serial: u64,
}
impl Registry {
    fn update(&mut self, mut player: Player) {
        self.serial = self.serial.saturating_add(1);
        let old = self.players.iter().position(|p| p.name == player.name);
        player.started = old
            .map(|i| &self.players[i])
            .filter(|p| p.playback == Playback::Playing && player.playback == Playback::Playing)
            .map(|p| p.started)
            .unwrap_or(self.serial);
        player.updated = self.serial;
        if let Some(i) = old {
            self.players[i] = player;
        } else {
            self.players.push(player);
        }
    }
    fn remove(&mut self, name: &str) {
        self.owners.remove(name);
        self.players.retain(|p| p.name != name);
    }
}
fn subscribe(connection: &Connection, tx: &mpsc::Sender<Request>, rule: &str) -> zbus::Result<()> {
    let mut stream = MessageIterator::for_match_rule(rule, connection, Some(256))?;
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("mpris-signals".into())
        .spawn(move || {
            for message in &mut stream {
                let Ok(message) = message else {
                    break;
                };
                if tx.send(Request::Signal(message)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Request::Disconnected);
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    Ok(())
}
fn relevant(message: &zbus::Message) -> bool {
    let Ok((interface, changed, invalidated)) = message
        .body()
        .deserialize::<(String, Properties, Vec<String>)>()
    else {
        return false;
    };
    let keys: &[&str] = if interface == PLAYER {
        &[
            "PlaybackStatus",
            "Metadata",
            "CanGoNext",
            "CanGoPrevious",
            "CanPlay",
            "CanPause",
        ]
    } else if interface == ROOT {
        &["Identity", "DesktopEntry"]
    } else {
        return false;
    };
    keys.iter()
        .any(|k| changed.contains_key(*k) || invalidated.iter().any(|v| v == k))
}
/// Called once by the MPRIS worker. This never activates a name.
pub(super) fn discover_music(bus: &zbus::blocking::fdo::DBusProxy<'_>) -> bool {
    bus.list_activatable_names().map(|names| names.iter().any(|n| n.as_str() == "org.gavno.Music")).unwrap_or(false)
        || bus.list_names().map(|names| names.iter().any(|n| n.as_str() == "org.gavno.Music")).unwrap_or(false)
}

fn run(
    connection: Connection,
    events: channel::Sender<Snapshot>,
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Request>,
    theme: &str,
) -> zbus::Result<()> {
    subscribe(&connection, &tx, "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0namespace='org.mpris.MediaPlayer2'")?;
    subscribe(&connection, &tx, "type='signal',path='/org/mpris/MediaPlayer2',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged'")?;
    drop(tx);
    let bus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
    let mut registry = Registry::default();
    let mut names = bus.list_names()?;
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for name in names.iter().filter(|n| n.starts_with(PREFIX)) {
        if let Ok(owner) = bus.get_name_owner(name.as_ref()) {
            registry.owners.insert(name.to_string(), owner.to_string());
            if let Ok(player) = read_player(&connection, name, &owner) {
                registry.update(player);
            }
        }
    }
    let mut previous = None;
    let mut previous_music = None;
    let mut music_available = None;
    let mut previous_available = false;
    let mut icons = crate::icons::Icons::default();
    let mut entries = crate::desktop_entry::DesktopEntries::default();
    // Image keys are cached independently of changing playback/capabilities.
    let mut image_key = (String::new(), String::new());
    let mut image = None;
    let mut music_image_key = (String::new(), String::new());
    let mut music_image = None;
    loop {
        let mut selected = active_player(&registry.players).cloned();
        // Selection history stays on this thread. Repeated identical property
        // signals must not wake the compositor just because their serial changed.
        if let Some(player) = selected.as_mut() {
            player.started = 0;
            player.updated = 0;
        }
        let mut music_player = registry
            .players
            .iter()
            .find(|p| p.name == super::MUSIC_PLAYER)
            .cloned();
        if let Some(player) = &mut music_player {
            player.started = 0;
            player.updated = 0;
        }
        if music_available.is_none() {
            music_available = Some(discover_music(&bus));
        }
        let available = music_available.unwrap_or(false);
        if selected != previous || music_player != previous_music || available != previous_available
        {
            let key = selected
                .as_ref()
                .map(|p| (p.metadata.art_url.clone(), p.desktop.clone()))
                .unwrap_or_default();
            if key != image_key {
                image = file_url(&key.0)
                    .and_then(|p| p.to_str().and_then(|p| icons.get(p, 96, theme)))
                    .or_else(|| {
                        entries
                            .resolve(&key.1)
                            .and_then(|e| icons.get(&e.icon, 96, theme))
                    })
                    .or_else(|| icons.get(&key.1, 96, theme));
                image_key = key;
            }
            let key = music_player
                .as_ref()
                .map(|p| (p.metadata.art_url.clone(), p.desktop.clone()))
                .unwrap_or_default();
            if key != music_image_key {
                use super::music::worker::{decode_cover, CoverKind};
                let path = file_url(&key.0);
                let path = path.as_deref().and_then(|p| p.to_str());
                music_image = path
                    .and_then(|p| decode_cover(p, CoverKind::Hero))
                    .or_else(|| {
                        // Rounded, tinted covers need a PNG; other art (SVG) still shows,
                        // square and with a neutral tint, through the icon loader. Every
                        // view draws covers at the hero size, so load it at that size.
                        let buffer = icons.get(path?, CoverKind::Hero.size(), theme)?;
                        Some(super::ui::Cover {
                            buffer,
                            tint: super::ui::NEUTRAL_TINT,
                        })
                    })
                    .map(std::sync::Arc::new);
                music_image_key = key;
            }
            if events
                .send(Snapshot {
                    player: selected.clone(),
                    image: image.clone(),
                    music_available: available,
                    music_player: music_player.clone(),
                    music_image: music_image.clone(),
                })
                .is_err()
            {
                break;
            }
            previous = selected;
            previous_music = music_player;
            previous_available = available;
        }
        match rx.recv() {
            Ok(request @ (Request::Command(_) | Request::MusicCommand(_))) => {
                let (command, player) = match request {
                    Request::Command(command) => (command, active_player(&registry.players)),
                    Request::MusicCommand(command) => (
                        command,
                        registry
                            .players
                            .iter()
                            .find(|p| p.name == super::MUSIC_PLAYER),
                    ),
                    _ => unreachable!(),
                };
                if let Some(player) = player.filter(|p| p.allows(command)) {
                    if let Some(owner) = registry.owners.get(&player.name) {
                        let method = match command {
                            Command::PlayPause => "PlayPause",
                            Command::Next => "Next",
                            Command::Previous => "Previous",
                        };
                        let result = Proxy::new(&connection, owner.as_str(), PATH, PLAYER)
                            .and_then(|p| p.call::<_, _, ()>(method, &()));
                        if let Err(error) = result {
                            tracing::debug!(%error, %method, "media command failed");
                        }
                    }
                }
            }
            Ok(Request::Signal(message)) => {
                let header = message.header();
                if header
                    .interface()
                    .is_some_and(|s| s.as_str() == "org.freedesktop.DBus")
                {
                    let Ok((name, _, owner)) =
                        message.body().deserialize::<(String, String, String)>()
                    else {
                        continue;
                    };
                    if !name.starts_with(PREFIX) {
                        continue;
                    }
                    // Discovery can race the initial list. Ignore queued ownership
                    // events that no longer describe the bus's current owner.
                    let current = zbus::names::BusName::try_from(name.as_str())
                        .ok()
                        .and_then(|name| bus.get_name_owner(name).ok())
                        .map(|owner| owner.to_string())
                        .unwrap_or_default();
                    if current != owner || registry.owners.get(&name) == Some(&owner) {
                        continue;
                    }
                    registry.remove(&name);
                    if !owner.is_empty() {
                        registry.owners.insert(name.clone(), owner.clone());
                        if let Ok(player) = read_player(&connection, &name, &owner) {
                            registry.update(player);
                        }
                    }
                } else if relevant(&message) {
                    let Some(sender) = header.sender() else {
                        continue;
                    };
                    let names: Vec<_> = registry
                        .owners
                        .iter()
                        .filter(|(_, owner)| owner.as_str() == sender.as_str())
                        .map(|(name, _)| name.clone())
                        .collect();
                    let Ok((interface, mut changed, invalidated)) =
                        message
                            .body()
                            .deserialize::<(String, Properties, Vec<String>)>()
                    else {
                        continue;
                    };
                    // Apply changed values in signal order so a later GetAll cannot
                    // hide a rapid Playing -> Paused -> Playing transition.
                    if !invalidated.is_empty() {
                        if let Ok(fresh) = properties(&connection, sender, &interface) {
                            for (key, value) in fresh {
                                if invalidated.contains(&key) {
                                    changed.insert(key, value);
                                }
                            }
                        }
                    }
                    for name in names {
                        if let Some(mut player) =
                            registry.players.iter().find(|p| p.name == name).cloned()
                        {
                            apply_properties(&mut player, &interface, &changed);
                            registry.update(player);
                        } else if let Ok(player) = read_player(&connection, &name, sender) {
                            registry.update(player);
                        }
                    }
                }
            }
            Ok(Request::Disconnected) | Err(_) => {
                let _ = events.send(Snapshot::default());
                // Explicit shutdown wakes the signal iterators too. Their
                // connection clones must not keep idle threads alive.
                let _ = connection.clone().close();
                break;
            }
        }
    }
    Ok(())
}
pub(super) fn spawn(
    events: channel::Sender<Snapshot>,
    theme: String,
) -> Option<mpsc::Sender<Request>> {
    let (tx, rx) = mpsc::channel();
    let signals = tx.clone();
    match std::thread::Builder::new()
        .name("mpris-dbus".into())
        .spawn(move || {
            if let Err(error) =
                Connection::session().and_then(|c| run(c, events, signals, rx, &theme))
            {
                tracing::warn!(%error, "MPRIS worker inactive");
            }
        }) {
        Ok(_) => Some(tx),
        Err(error) => {
            tracing::warn!(%error, "cannot start MPRIS worker");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Str, Value};
    #[test]
    fn parses_arrays_missing_and_bad_metadata() {
        assert_eq!(metadata(&Properties::new()), Metadata::default());
        let mut values = Properties::new();
        values.insert("xesam:title".into(), Str::from("Track").into());
        values.insert(
            "xesam:artist".into(),
            Value::from(vec!["A", "B"]).try_into().unwrap(),
        );
        values.insert("mpris:length".into(), 123i64.into());
        assert_eq!(metadata(&values).artists, ["A", "B"]);
        assert_eq!(metadata(&values).title, "Track");
        assert_eq!(metadata(&values).length, Some(123));
        values.insert("xesam:artist".into(), Str::from("wrong type").into());
        values.insert("xesam:title".into(), true.into());
        values.insert("mpris:length".into(), (-1i64).into());
        assert_eq!(metadata(&values), Metadata::default());
        values.insert(
            "xesam:artist".into(),
            Value::from(vec![1u32]).try_into().unwrap(),
        );
        values.insert("mpris:length".into(), Str::from("bad").into());
        assert_eq!(metadata(&values), Metadata::default());
    }
    #[test]
    fn local_file_decoding_rejects_remote_and_malformed_urls() {
        assert_eq!(
            file_url("file:///tmp/a%20b%23c.png"),
            Some("/tmp/a b#c.png".into())
        );
        assert_eq!(
            file_url("file://localhost/tmp/%D0%B0.png"),
            Some("/tmp/а.png".into())
        );
        assert_eq!(file_url("file:///tmp/a+b.png"), Some("/tmp/a+b.png".into()));
        for url in [
            "https://host/a.png",
            "http://host/a",
            "file://host/a",
            "file:///a%",
            "file:///a%zz",
            "file:///a%00",
            "file:///a?b",
            "/tmp/a",
        ] {
            assert!(file_url(url).is_none(), "{url}");
        }
    }
    #[test]
    fn property_updates_preserve_start_order_until_playback_restarts() {
        let mut r = Registry::default();
        let p = Player {
            name: "a".into(),
            playback: Playback::Playing,
            ..Default::default()
        };
        r.update(p.clone());
        let start = r.players[0].started;
        r.update(p.clone());
        assert_eq!(r.players[0].started, start);
        r.update(Player {
            playback: Playback::Paused,
            ..p.clone()
        });
        r.update(p);
        assert!(r.players[0].started > start);
        r.remove("a");
        assert!(active_player(&r.players).is_none());
    }
    struct TestRoot;
    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl TestRoot {
        #[zbus(property)]
        fn identity(&self) -> &str {
            "Test player"
        }
        #[zbus(property)]
        fn desktop_entry(&self) -> &str {
            "test-player"
        }
    }
    struct TestPlayer {
        status: std::sync::Arc<std::sync::Mutex<String>>,
        commands: mpsc::Sender<Command>,
        art_url: String,
    }
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl TestPlayer {
        fn play_pause(&self) {
            let _ = self.commands.send(Command::PlayPause);
        }
        fn next(&self) {
            let _ = self.commands.send(Command::Next);
        }
        fn previous(&self) {
            let _ = self.commands.send(Command::Previous);
        }
        #[zbus(property)]
        fn playback_status(&self) -> String {
            self.status.lock().unwrap().clone()
        }
        #[zbus(property)]
        fn metadata(&self) -> Properties {
            let mut values = Properties::new();
            values.insert("xesam:title".into(), Str::from("Bus track").into());
            values.insert(
                "mpris:artUrl".into(),
                Str::from(self.art_url.as_str()).into(),
            );
            values.insert(
                "xesam:artist".into(),
                Value::from(vec!["First", "Second"]).try_into().unwrap(),
            );
            values
        }
        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }
    }
    struct Bus(std::process::Child);
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn wait_for(
        event_loop: &mut smithay::reexports::calloop::EventLoop<'_, Vec<Snapshot>>,
        snapshots: &mut Vec<Snapshot>,
        predicate: impl Fn(&Snapshot) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            event_loop
                .dispatch(std::time::Duration::from_millis(20), snapshots)
                .unwrap();
            if snapshots.iter().any(&predicate) {
                snapshots.clear();
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "MPRIS event deadline exceeded"
            );
        }
    }
    #[test]
    fn private_bus_discovers_updates_controls_and_removes_players() {
        use std::{
            io::{BufRead, BufReader},
            process::{Command as Process, Stdio},
            sync::{Arc, Mutex},
        };
        let Ok(child) = Process::new("dbus-daemon")
            .env_remove("XDG_RUNTIME_DIR")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
        else {
            eprintln!("skipping MPRIS bus test: dbus-daemon unavailable");
            return;
        };
        let mut bus = Bus(child);
        let mut address = String::new();
        let Some(stdout) = bus.0.stdout.take() else {
            return;
        };
        if BufReader::new(stdout).read_line(&mut address).is_err() || address.trim().is_empty() {
            eprintln!("skipping MPRIS bus test: private bus unavailable");
            return;
        }
        let address = address.trim();
        let art = std::env::temp_dir().join(format!("gavno mpris art {}.svg", std::process::id()));
        std::fs::write(&art, br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40"><rect width="40" height="40" fill="#ff0000"/></svg>"##).unwrap();
        let art_url = format!("file://{}", art.to_string_lossy().replace(' ', "%20"));
        let status = Arc::new(Mutex::new("Paused".into()));
        let (command_tx, commands) = mpsc::channel();
        let server = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .serve_at(PATH, TestRoot)
            .unwrap()
            .serve_at(
                PATH,
                TestPlayer {
                    status: status.clone(),
                    commands: command_tx,
                    art_url,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        let name = "org.mpris.MediaPlayer2.gavno_test";
        server.request_name(name).unwrap();
        let worker_connection = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .build()
            .unwrap();
        let (events, source) = channel::channel();
        let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(source, |event, _, snapshots: &mut Vec<Snapshot>| {
                if let channel::Event::Msg(snapshot) = event {
                    snapshots.push(snapshot);
                }
            })
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let signal_tx = tx.clone();
        let worker =
            std::thread::spawn(move || run(worker_connection, events, signal_tx, rx, "hicolor"));
        let mut snapshots = Vec::new();
        wait_for(&mut event_loop, &mut snapshots, |s| {
            s.image.is_some()
                && s.player.as_ref().is_some_and(|p| {
                    p.identity == "Test player"
                        && p.desktop == "test-player"
                        && p.playback == Playback::Paused
                        && p.artist() == "First, Second"
                })
        });
        for command in [Command::PlayPause, Command::Next, Command::Previous] {
            tx.send(Request::Command(command)).unwrap();
            assert_eq!(
                commands
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap(),
                command
            );
        }
        *status.lock().unwrap() = "Playing".into();
        let mut changed = Properties::new();
        changed.insert("PlaybackStatus".into(), Str::from("Playing").into());
        server
            .emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER, changed, Vec::<String>::new()),
            )
            .unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| {
            s.player
                .as_ref()
                .is_some_and(|p| p.playback == Playback::Playing)
        });
        // Spotify remains visible and independently controllable while another
        // player owns the active capsule. Its cover reuses the worker's cache.
        let (music_tx, music_commands) = mpsc::channel();
        let music_server = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .serve_at(PATH, TestRoot)
            .unwrap()
            .serve_at(
                PATH,
                TestPlayer {
                    status: Arc::new(Mutex::new("Paused".into())),
                    commands: music_tx,
                    art_url: format!("file://{}", art.to_string_lossy().replace(' ', "%20")),
                },
            )
            .unwrap()
            .build()
            .unwrap();
        music_server
            .request_name(super::super::MUSIC_PLAYER)
            .unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| {
            s.music_image.is_some()
                && s.music_player
                    .as_ref()
                    .is_some_and(|p| p.name == super::super::MUSIC_PLAYER)
                && s.player.as_ref().is_some_and(|p| p.name == name)
        });
        for command in [Command::Previous, Command::PlayPause, Command::Next] {
            tx.send(Request::MusicCommand(command)).unwrap();
            assert_eq!(
                music_commands
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap(),
                command
            );
            assert!(
                commands.try_recv().is_err(),
                "Spotify commands must not reach the active player"
            );
        }
        let mut changed = Properties::new();
        changed.insert(
            "Metadata".into(),
            Value::from(Properties::from([(
                "xesam:title".into(),
                Str::from("Spotify update").into(),
            )]))
            .try_into()
            .unwrap(),
        );
        music_server
            .emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER, changed, Vec::<String>::new()),
            )
            .unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| {
            s.music_player
                .as_ref()
                .is_some_and(|p| p.title() == "Spotify update")
                && s.player.as_ref().is_some_and(|p| p.name == name)
        });
        music_server
            .release_name(super::super::MUSIC_PLAYER)
            .unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| {
            s.music_player.is_none()
        });
        // Invalidated properties are fetched on the worker as well.
        *status.lock().unwrap() = "Paused".into();
        server
            .emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER, Properties::new(), vec!["PlaybackStatus"]),
            )
            .unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| {
            s.player
                .as_ref()
                .is_some_and(|p| p.playback == Playback::Paused)
        });
        server
            .emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(
                    PLAYER,
                    HashMap::from([("Position", OwnedValue::from(123i64))]),
                    Vec::<String>::new(),
                ),
            )
            .unwrap();
        event_loop
            .dispatch(std::time::Duration::from_millis(50), &mut snapshots)
            .unwrap();
        assert!(
            snapshots.is_empty(),
            "position changes must not publish UI updates"
        );
        let remote = HashMap::<String, OwnedValue>::from([
            ("xesam:title".into(), Str::from("Remote art").into()),
            (
                "mpris:artUrl".into(),
                Str::from("https://example.invalid/art.png").into(),
            ),
        ]);
        server
            .emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(
                    PLAYER,
                    HashMap::from([(
                        "Metadata",
                        OwnedValue::try_from(Value::from(remote)).unwrap(),
                    )]),
                    Vec::<String>::new(),
                ),
            )
            .unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| {
            s.image.is_none() && s.player.as_ref().is_some_and(|p| p.title() == "Remote art")
        });
        server.release_name(name).unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| s.player.is_none());
        // Reappearance exercises NameOwnerChanged discovery after the initial list.
        server.request_name(name).unwrap();
        wait_for(&mut event_loop, &mut snapshots, |s| s.player.is_some());
        drop(bus);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_file(art).unwrap();
    }
}
