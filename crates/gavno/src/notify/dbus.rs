//! Blocking worker owns the bus connection and signals. Its zbus executor serves
//! methods independently; neither direction can block the compositor on D-Bus.
use super::{Notice, Request, Signal};
use smithay::reexports::calloop::channel;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU32, Ordering},
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
struct Registry {
    live: HashMap<u32, String>,
    // Closed popup IDs retain their history action while the sender survives.
    history: VecDeque<(u32, String, bool)>,
}
type Live = Arc<Mutex<Registry>>;
struct Server {
    events: channel::Sender<Request>,
    next: AtomicU32,
    live: Live,
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
        let string_hint = |key| -> zbus::fdo::Result<String> {
            hints
                .get(key)
                .map(<&str>::try_from)
                .transpose()
                .map(|s| s.unwrap_or_default().to_owned())
                .map_err(|_| zbus::fdo::Error::InvalidArgs(format!("{key} must be a string")))
        };
        let desktop = string_hint("desktop-entry")?;
        let image = string_hint("image-path")?;
        let mut live = self
            .live
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("notification registry unavailable".into()))?;
        let id = if replaces_id != 0 && live.live.contains_key(&replaces_id) {
            replaces_id
        } else {
            self.next
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| {
                    zbus::fdo::Error::LimitsExceeded("notification IDs exhausted".into())
                })?
        };
        let sender = header
            .sender()
            .map(|s| s.as_str().to_owned())
            .unwrap_or_default();
        let n = Notice {
            id,
            app: app_name,
            icon: icon_path(if image.is_empty() { app_icon } else { image }),
            desktop,
            summary,
            body,
            actions: actions
                .chunks_exact(2)
                .map(|a| (a[0].clone(), a[1].clone()))
                .collect(),
            urgency,
            timeout: expire_timeout,
        };
        self.events
            .send(Request::Notify(n))
            .map_err(|_| zbus::fdo::Error::Failed("notification channel closed".into()))?;
        live.live.insert(id, sender.clone());
        let has_default = actions.chunks_exact(2).any(|a| a[0] == "default");
        live.history.retain(|h| h.0 != id);
        live.history.push_front((id, sender, has_default));
        live.history.truncate(50);
        Ok(id)
    }
    async fn close_notification(
        &self,
        id: u32,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let exists = self
            .live
            .lock()
            .map_err(|_| zbus::fdo::Error::Failed("notification registry unavailable".into()))?
            .live
            .remove(&id)
            .is_some();
        if exists {
            let _ = self.events.send(Request::Close(id));
            connection
                .emit_signal(None::<&str>, PATH, NAME, "NotificationClosed", &(id, 3u32))
                .await?;
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
) -> zbus::Result<(Connection, Live)> {
    let live = Live::default();
    let server = Server {
        events,
        next: AtomicU32::new(1),
        live: live.clone(),
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
    if let Signal::Closed(id, reason) = signal {
        let exists = live
            .lock()
            .map_err(|_| zbus::Error::Failure("notification registry unavailable".into()))?
            .live
            .remove(&id)
            .is_some();
        if exists {
            connection.emit_signal(
                None::<&str>,
                PATH,
                NAME,
                "NotificationClosed",
                &(id, reason),
            )?;
        }
        return Ok(());
    }
    let (id, key, owner) = {
        let registry = live
            .lock()
            .map_err(|_| zbus::Error::Failure("notification registry unavailable".into()))?;
        match signal {
            Signal::Action(id, key) => (id, key, registry.live.get(&id).cloned()),
            Signal::HistoryAction(id) => (
                id,
                "default".into(),
                registry
                    .history
                    .iter()
                    .find(|h| h.0 == id && h.2)
                    .map(|h| h.1.clone()),
            ),
            Signal::Closed(_, _) => return Ok(()),
        }
    };
    if let Some(owner) = owner {
        let bus = zbus::blocking::fdo::DBusProxy::new(connection)?;
        if let Ok(name) = zbus::names::BusName::try_from(owner.as_str()) {
            if bus.name_has_owner(name)? {
                connection.emit_signal(None::<&str>, PATH, NAME, "ActionInvoked", &(id, key))?;
            }
        }
    }
    Ok(())
}

pub fn spawn(events: channel::Sender<Request>) -> Option<mpsc::Sender<Signal>> {
    let (tx, rx) = mpsc::channel();
    match std::thread::Builder::new().name("notifications-dbus".into()).spawn(move || {
        let result = (|| -> zbus::Result<()> {
            let (connection, live) = connect(events.clone(), None)?;
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
        let (events, _source) = channel::channel();
        let (server, registry) = connect(events.clone(), Some(address)).unwrap();
        assert!(connect(events, Some(address)).is_err());
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
        assert_eq!(send(id, vec!["default", "Open"]).unwrap(), id);
        assert!(send(0, vec!["odd"]).is_err());
        let mut actions = proxy.receive_signal("ActionInvoked").unwrap();
        let mut closed = proxy.receive_signal("NotificationClosed").unwrap();
        dispatch(&server, &registry, Signal::Action(id, "default".into())).unwrap();
        assert_eq!(
            actions
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, String)>()
                .unwrap(),
            (id, "default".into())
        );
        proxy.call::<_, _, ()>("CloseNotification", &(id,)).unwrap();
        assert_eq!(
            closed
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, u32)>()
                .unwrap(),
            (id, 3)
        );
        dispatch(&server, &registry, Signal::HistoryAction(id)).unwrap();
        assert_eq!(
            actions
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, String)>()
                .unwrap(),
            (id, "default".into())
        );
        let next = send(id, vec![]).unwrap();
        assert_ne!(next, id);
        dispatch(&server, &registry, Signal::Closed(next, 1)).unwrap();
        assert_eq!(
            closed
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, u32)>()
                .unwrap(),
            (next, 1)
        );
        let dismissed = send(0, vec![]).unwrap();
        dispatch(&server, &registry, Signal::Closed(dismissed, 2)).unwrap();
        assert_eq!(
            closed
                .next()
                .unwrap()
                .body()
                .deserialize::<(u32, u32)>()
                .unwrap(),
            (dismissed, 2)
        );
    }
}
