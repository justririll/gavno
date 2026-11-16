//! Blocking worker owns the bus connection and signals. Its zbus executor serves
//! methods independently; neither direction can block the compositor on D-Bus.
use super::{Notice, Request, Signal};
use smithay::reexports::calloop::channel;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
};
use zbus::{
    blocking::{connection::Builder, Connection},
    fdo::{RequestNameFlags, RequestNameReply},
    zvariant::OwnedValue,
};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
#[derive(Default)]
pub(super) struct Registry {
    pub(super) live: HashMap<u32, (String, u64)>,
}
pub(super) type Live = Arc<Mutex<Registry>>;
struct Server {
    events: channel::Sender<Request>,
    next: AtomicU32,
    generation: AtomicU64,
    live: Live,
    enabled: Arc<AtomicBool>,
}
fn bounded(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// The spec permits an absolute path, a local file URI or an icon theme name.
fn icon_path(value: String) -> String {
    let Some(uri) = value.strip_prefix("file://") else {
        return value;
    };
    let path = uri
        .strip_prefix("localhost/")
        .map(|s| format!("/{s}"))
        .unwrap_or_else(|| uri.to_owned());
    if !path.starts_with('/') {
        return value;
    }
    let mut decoded = Vec::new();
    let mut bytes = path.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let (Some(a), Some(b)) = (bytes.next(), bytes.next()) else {
                return value;
            };
            let (Some(a), Some(b)) = ((a as char).to_digit(16), (b as char).to_digit(16)) else {
                return value;
            };
            decoded.push((a * 16 + b) as u8);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).unwrap_or(value)
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    fn get_capabilities(&self) -> Vec<&str> {
        vec!["body", "actions", "icon-static", "persistence"]
    }
    fn get_server_information(&self) -> (&str, &str, &str, &str) {
        ("GAVNO", "gavno", env!("CARGO_PKG_VERSION"), "1.2")
    }
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<u32> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err(zbus::fdo::Error::Failed(
                "notifications are disabled".into(),
            ));
        }
        if actions.len() % 2 != 0 || expire_timeout < -1 {
            return Err(zbus::fdo::Error::InvalidArgs(
                "invalid actions or expire_timeout".into(),
            ));
        }
        let urgency = hints
            .get("urgency")
            .map(u8::try_from)
            .transpose()
            .map_err(|_| zbus::fdo::Error::InvalidArgs("urgency must be a byte".into()))?
            .unwrap_or(1);
        if urgency > 2 {
            return Err(zbus::fdo::Error::InvalidArgs(
                "urgency must be 0, 1 or 2".into(),
            ));
        }
        let string_hint = |key, max| -> zbus::fdo::Result<String> {
            hints
                .get(key)
                .map(<&str>::try_from)
                .transpose()
                .map(|s| bounded(s.unwrap_or_default(), max))
                .map_err(|_| zbus::fdo::Error::InvalidArgs(format!("{key} must be a string")))
        };
        let desktop = string_hint("desktop-entry", 256)?;
        let image = string_hint("image-path", 1024)?;
        let mut live = self
            .live
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("notification registry unavailable".into()))?;
        // Serialize allocation and channel delivery with registry updates.
        if !self.enabled.load(Ordering::Acquire) {
            return Err(zbus::fdo::Error::Failed(
                "notifications are disabled".into(),
            ));
        }
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            let mut candidate = self.next.load(Ordering::Relaxed).max(1);
            let start = candidate;
            loop {
                if !live.live.contains_key(&candidate) {
                    self.next
                        .store(candidate.wrapping_add(1).max(1), Ordering::Relaxed);
                    break candidate;
                }
                candidate = candidate.wrapping_add(1).max(1);
                if candidate == start {
                    return Err(zbus::fdo::Error::LimitsExceeded(
                        "notification IDs exhausted".into(),
                    ));
                }
            }
        };
        let gen = self
            .generation
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| {
                zbus::fdo::Error::LimitsExceeded("notification generations exhausted".into())
            })?;
        let sender = header
            .sender()
            .map(|s| s.as_str().to_owned())
            .unwrap_or_default();
        let n = Notice {
            id,
            gen,
            app: bounded(&app_name, 128),
            icon: icon_path(if image.is_empty() {
                bounded(&app_icon, 1024)
            } else {
                image
            }),
            desktop,
            summary: bounded(&summary, 512),
            body: bounded(&body, 4096),
            actions: actions
                .chunks_exact(2)
                .take(8)
                .map(|a| (bounded(&a[0], 128), bounded(&a[1], 128)))
                .collect(),
            urgency,
            timeout: expire_timeout,
        };
        self.events
            .send(Request::Notify(n))
            .map_err(|_| zbus::fdo::Error::Failed("notification channel closed".into()))?;
        live.live.insert(id, (sender, gen));
        Ok(id)
    }
    async fn close_notification(
        &self,
        id: u32,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let removed = self
            .live
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("notification registry unavailable".into()))?
            .live
            .remove(&id);
        if let Some((_, gen)) = removed {
            let _ = self.events.send(Request::Close(id, gen));
            connection
                .emit_signal(None::<&str>, PATH, NAME, "NotificationClosed", &(id, 3u32))
                .await?;
        } else {
            return Err(zbus::fdo::Error::Failed("no such notification".into()));
        }
        Ok(())
    }
    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn action_invoked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

fn connect(
    events: channel::Sender<Request>,
    address: Option<&str>,
    live: Live,
    enabled: Arc<AtomicBool>,
) -> zbus::Result<(Connection, Live)> {
    let server = Server {
        events,
        next: AtomicU32::new(1),
        generation: AtomicU64::new(1),
        live: live.clone(),
        enabled,
    };
    let builder = if let Some(address) = address {
        Builder::address(address)?
    } else {
        Builder::session()?
    };
    let connection = builder.serve_at(PATH, server)?.build()?;
    // DoNotQueue alone neither replaces the owner nor allows later replacement.
    if connection.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into())?
        != RequestNameReply::PrimaryOwner
    {
        return Err(zbus::Error::NameTaken);
    }
    Ok((connection, live))
}

fn dispatch(connection: &Connection, live: &Live, signal: Signal) -> zbus::Result<()> {
    let payload = {
        let mut registry = live
            .lock()
            .map_err(|_| zbus::Error::Failure("notification registry unavailable".into()))?;
        match signal {
            Signal::Closed(id, gen, reason) => {
                if !registry
                    .live
                    .get(&id)
                    .is_some_and(|(_, current)| *current == gen)
                {
                    return Ok(());
                }
                registry.live.remove(&id);
                (id, Some(reason), None)
            }
            Signal::Action(id, gen, key) => {
                let Some((owner, current)) = registry.live.get(&id) else {
                    return Ok(());
                };
                if *current != gen {
                    return Ok(());
                }
                (id, None, Some((owner.clone(), key)))
            }
        }
    };
    let (id, reason, action) = payload;
    if let Some(reason) = reason {
        connection.emit_signal(
            None::<&str>,
            PATH,
            NAME,
            "NotificationClosed",
            &(id, reason),
        )?;
    } else if let Some((owner, key)) = action {
        let bus = zbus::blocking::fdo::DBusProxy::new(connection)?;
        if let Ok(name) = zbus::names::BusName::try_from(owner.as_str()) {
            if bus.name_has_owner(name)? {
                connection.emit_signal(None::<&str>, PATH, NAME, "ActionInvoked", &(id, key))?;
            }
        }
    }
    Ok(())
}

pub(super) fn spawn(
    events: channel::Sender<Request>,
    live: Live,
    enabled: Arc<AtomicBool>,
) -> Option<mpsc::Sender<Signal>> {
    let (tx, rx) = mpsc::channel();
    match std::thread::Builder::new().name("notifications-dbus".into()).spawn(move || {
        let result = (|| -> zbus::Result<()> {
            let (connection, live) = connect(events.clone(), None, live, enabled)?;
            if events.send(Request::Active).is_err() { return Ok(()); }
            // A failed signal (peer gone, bus hiccup) must not end the
            // server: dropping the connection would release the name.
            for signal in rx {
                if let Err(error) = dispatch(&connection, &live, signal) {
                    tracing::warn!(%error, "cannot emit notification signal");
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "notification server inactive (session bus unavailable or name already owned)");
        }
    }) {
        Ok(_) => Some(tx),
        Err(error) => { tracing::warn!(%error, "cannot start notification server"); None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
    };
    struct Bus(Child);
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn local_file_icons_and_theme_names() {
        assert_eq!(
            icon_path("file:///tmp/app%20icon.png".into()),
            "/tmp/app icon.png"
        );
        assert_eq!(
            icon_path("file://localhost/tmp/icon.png".into()),
            "/tmp/icon.png"
        );
        assert_eq!(icon_path("firefox".into()), "firefox");
        assert_eq!(icon_path("/tmp/icon.png".into()), "/tmp/icon.png");
    }
    #[test]
    fn private_bus_methods_replacement_and_name_conflict() {
        let Ok(child) = Command::new("dbus-daemon")
            .env_remove("XDG_RUNTIME_DIR")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
        else {
            return;
        };
        let mut bus = Bus(child);
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim();
        let (events, source) = channel::channel();
        let enabled = Arc::new(AtomicBool::new(true));
        let (server, registry) = connect(
            events.clone(),
            Some(address),
            Live::default(),
            enabled.clone(),
        )
        .unwrap();
        assert!(connect(events, Some(address), Live::default(), enabled.clone()).is_err());
        let mut event_loop =
            smithay::reexports::calloop::EventLoop::<Vec<Request>>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(source, |event, _, requests| {
                if let channel::Event::Msg(request) = event {
                    requests.push(request);
                }
            })
            .unwrap();
        let mut requests = Vec::new();
        let client = Builder::address(address).unwrap().build().unwrap();
        let proxy = zbus::blocking::Proxy::new(&client, NAME, PATH, NAME).unwrap();
        let caps: Vec<String> = proxy.call("GetCapabilities", &()).unwrap();
        assert_eq!(caps, ["body", "actions", "icon-static", "persistence"]);
        let info: (String, String, String, String) =
            proxy.call("GetServerInformation", &()).unwrap();
        assert_eq!(info.3, "1.2");
        let send = |replace, actions: Vec<&str>| {
            proxy.call::<_, _, u32>(
                "Notify",
                &(
                    "App",
                    replace,
                    "",
                    "Title",
                    "Body",
                    actions,
                    HashMap::<String, OwnedValue>::new(),
                    0i32,
                ),
            )
        };
        let id = send(0u32, vec![]).unwrap();
        assert_ne!(id, 0);
        let generation = |id| registry.lock().unwrap().live[&id].1;
        let old_gen = generation(id);
        assert_eq!(send(id, vec!["default", "Open"]).unwrap(), id);
        let gen = generation(id);
        assert!(gen > old_gen);
        assert!(send(0, vec!["odd"]).is_err());
        assert!(proxy
            .call::<_, _, ()>("CloseNotification", &(999u32,))
            .unwrap_err()
            .to_string()
            .contains("no such notification"));
        let mut actions = proxy.receive_signal("ActionInvoked").unwrap();
        let mut closed = proxy.receive_signal("NotificationClosed").unwrap();
        dispatch(&server, &registry, Signal::Closed(id, old_gen, 4)).unwrap();
        assert_eq!(generation(id), gen);
        dispatch(
            &server,
            &registry,
            Signal::Action(id, old_gen, "stale".into()),
        )
        .unwrap();
        dispatch(
            &server,
            &registry,
            Signal::Action(id, gen, "default".into()),
        )
        .unwrap();
        assert_eq!(
            actions
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, String)>()
                .unwrap(),
            (id, "default".into())
        );
        dispatch(&server, &registry, Signal::Closed(id, gen, 1)).unwrap();
        assert_eq!(
            closed
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, u32)>()
                .unwrap(),
            (id, 1)
        );
        assert!(!registry.lock().unwrap().live.contains_key(&id));
        dispatch(&server, &registry, Signal::Action(id, gen, "closed".into())).unwrap();

        // An unknown replacement ID is used verbatim; the allocator skips it.
        assert_eq!(send(2, vec![]).unwrap(), 2);
        let fresh = send(0, vec![]).unwrap();
        assert_eq!(fresh, 3);
        let fresh_gen = generation(fresh);
        dispatch(
            &server,
            &registry,
            Signal::Action(fresh, fresh_gen, "sentinel".into()),
        )
        .unwrap();
        assert_eq!(
            actions
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, String)>()
                .unwrap(),
            (fresh, "sentinel".into())
        );
        proxy
            .call::<_, _, ()>("CloseNotification", &(fresh,))
            .unwrap();
        assert_eq!(
            closed
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, u32)>()
                .unwrap(),
            (fresh, 3)
        );
        assert_eq!(send(fresh, vec![]).unwrap(), fresh);

        let oversized = "界".repeat(2000);
        let mut hints = HashMap::<String, OwnedValue>::new();
        hints.insert(
            "desktop-entry".into(),
            zbus::zvariant::Str::from(oversized.as_str()).into(),
        );
        hints.insert(
            "image-path".into(),
            zbus::zvariant::Str::from(oversized.as_str()).into(),
        );
        let long_id: u32 = proxy
            .call(
                "Notify",
                &(
                    &oversized,
                    0u32,
                    &oversized,
                    &oversized,
                    &oversized,
                    vec![oversized.as_str(); 20],
                    hints,
                    0i32,
                ),
            )
            .unwrap();
        event_loop
            .dispatch(std::time::Duration::ZERO, &mut requests)
            .unwrap();
        let n = requests
            .iter()
            .find_map(|r| match r {
                Request::Notify(n) if n.id == long_id => Some(n),
                _ => None,
            })
            .unwrap();
        assert_eq!(n.app.len(), 126);
        assert_eq!(n.summary.len(), 510);
        assert_eq!(n.body.len(), 4095);
        assert_eq!(n.body, "界".repeat(1365));
        assert_eq!(n.icon.len(), 1023);
        assert_eq!(n.desktop.len(), 255);
        assert_eq!(n.actions.len(), 8);
        assert!(n
            .actions
            .iter()
            .all(|(key, label)| key.len() == 126 && label.len() == 126));

        enabled.store(false, Ordering::Release);
        let live_before = registry.lock().unwrap().live.clone();
        assert!(send(0, vec![])
            .unwrap_err()
            .to_string()
            .contains("notifications are disabled"));
        assert_eq!(registry.lock().unwrap().live, live_before);
        let count = requests.len();
        event_loop
            .dispatch(std::time::Duration::ZERO, &mut requests)
            .unwrap();
        assert_eq!(requests.len(), count);
        enabled.store(true, Ordering::Release);
        assert_eq!(send(0, vec![]).unwrap(), long_id + 1);
    }
}
