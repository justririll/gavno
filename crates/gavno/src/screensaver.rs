//! `org.freedesktop.ScreenSaver` on the session bus: apps that cannot use the Wayland
//! idle-inhibit protocol (SDL, Steam, Electron, Firefox, X11 clients) keep the screen on
//! through cookies. A named worker thread owns the connection and blocks on the bus, so
//! an idle session costs nothing and the compositor never waits on D-Bus.
use crate::Gavno;
use smithay::reexports::calloop::{channel, LoopHandle};
use std::{
    any::Any,
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};
use zbus::{
    blocking::{connection::Builder, Connection},
    fdo::{RequestNameFlags, RequestNameReply},
};

const NAME: &str = "org.freedesktop.ScreenSaver";
const PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];
const MAX_PER_SENDER: usize = 64;
const MAX_TOTAL: usize = 1024;

/// What the worker tells the compositor.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Number of live cookies, sent whenever it changes.
    Count(u32),
    /// `SimulateUserActivity` was called.
    Activity,
}

struct Cookie {
    sender: String,
    application: String,
    reason: String,
}
#[derive(Default)]
struct SenderState {
    cookies: usize,
    /// Keeps the per-sender `NameOwnerChanged` subscription alive; dropping cancels it.
    watch: Option<Box<dyn Any + Send>>,
}
#[derive(Default)]
struct Registry {
    next: u32,
    cookies: HashMap<u32, Cookie>,
    senders: HashMap<String, SenderState>,
}
#[derive(Clone)]
struct Shared {
    events: channel::Sender<Event>,
    registry: Arc<Mutex<Registry>>,
}

fn bounded(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

impl Shared {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        // A poisoned registry is still consistent: every mutation is a single step.
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn report(&self, before: usize, registry: &Registry, first: Option<&Cookie>) {
        let after = registry.cookies.len();
        if before == after {
            return;
        }
        if before == 0 {
            if let Some(c) = first {
                tracing::info!("idle inhibited over D-Bus by {}: {}", c.application, c.reason);
            }
        } else if after == 0 {
            tracing::info!("D-Bus idle inhibit released");
        }
        let _ = self.events.send(Event::Count(after as u32));
    }

    /// Drops every cookie of `sender`, e.g. when it left the bus.
    fn release_sender(&self, sender: &str, own_watch: bool) {
        let mut registry = self.registry();
        let before = registry.cookies.len();
        registry.cookies.retain(|_, c| c.sender != sender);
        if let Some(state) = registry.senders.remove(sender) {
            if own_watch {
                // The watch task is the caller; cancelling it from inside is pointless.
                std::mem::forget(state.watch);
            }
        }
        self.report(before, &registry, None);
    }
}

struct Server(Shared);

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl Server {
    async fn inhibit(
        &self,
        application_name: String,
        reason_for_inhibit: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<u32> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("no sender".into()))?
            .to_string();
        let cookie = {
            let mut registry = self.0.registry();
            let before = registry.cookies.len();
            let per_sender = registry.senders.get(&sender).map_or(0, |s| s.cookies);
            if per_sender >= MAX_PER_SENDER || before >= MAX_TOTAL {
                return Err(zbus::fdo::Error::LimitsExceeded("too many inhibits".into()));
            }
            let id = loop {
                registry.next = registry.next.wrapping_add(1);
                if registry.next != 0 && !registry.cookies.contains_key(&registry.next) {
                    break registry.next;
                }
            };
            let entry = Cookie {
                sender: sender.clone(),
                application: bounded(&application_name, 128),
                reason: bounded(&reason_for_inhibit, 128),
            };
            tracing::debug!(id, app = %entry.application, "ScreenSaver.Inhibit");
            registry.cookies.insert(id, entry);
            registry.senders.entry(sender.clone()).or_default().cookies += 1;
            let first = registry.cookies.get(&id);
            self.0.report(before, &registry, first);
            id
        };
        let needs_watch = self.0.registry().senders.get(&sender).is_some_and(|s| {
            s.cookies == 1 && s.watch.is_none()
        });
        if needs_watch {
            if let Err(error) = self.watch(connection, &sender).await {
                tracing::debug!(%error, "cannot watch inhibiting client");
                self.0.release_sender(&sender, false);
                return Err(zbus::fdo::Error::Failed("cannot watch client".into()));
            }
        }
        Ok(cookie)
    }

    fn un_inhibit(&self, cookie: u32, #[zbus(header)] header: zbus::message::Header<'_>) {
        let Some(sender) = header.sender() else {
            return;
        };
        let mut registry = self.0.registry();
        let before = registry.cookies.len();
        if registry
            .cookies
            .get(&cookie)
            .is_none_or(|c| c.sender != sender.as_str())
        {
            tracing::debug!(cookie, "ScreenSaver.UnInhibit ignored");
            return;
        }
        registry.cookies.remove(&cookie);
        if let Some(state) = registry.senders.get_mut(sender.as_str()) {
            state.cookies -= 1;
            if state.cookies == 0 {
                // Dropping the watch also removes its bus match rule.
                registry.senders.remove(sender.as_str());
            }
        }
        tracing::debug!(cookie, "ScreenSaver.UnInhibit");
        self.0.report(before, &registry, None);
    }

    fn simulate_user_activity(&self) {
        let _ = self.0.events.send(Event::Activity);
    }
}

impl Server {
    /// Releases the sender's cookies once its unique name leaves the bus. The match rule
    /// exists only while the sender holds cookies, so unrelated bus traffic never wakes us.
    async fn watch(&self, connection: &zbus::Connection, sender: &str) -> zbus::Result<()> {
        use futures_lite::StreamExt;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, sender)?
            .build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, connection, Some(4)).await?;
        let owned = zbus::fdo::DBusProxy::new(connection)
            .await?
            .name_has_owner(zbus::names::BusName::try_from(sender)?)
            .await?;
        if !owned {
            // Already gone before the rule was in place.
            return Err(zbus::Error::Failure("sender left the bus".into()));
        }
        let shared = self.0.clone();
        let who = sender.to_owned();
        let task = connection.executor().spawn(
            async move {
                while let Some(Ok(message)) = stream.next().await {
                    let gone = message
                        .body()
                        .deserialize::<(String, String, String)>()
                        .is_ok_and(|(_, _, new_owner)| new_owner.is_empty());
                    if gone {
                        shared.release_sender(&who, true);
                        return;
                    }
                }
            },
            "screensaver-watch",
        );
        let mut registry = self.0.registry();
        match registry.senders.get_mut(sender) {
            Some(state) => state.watch = Some(Box::new(task)),
            // Released meanwhile; dropping the task cancels the subscription.
            None => drop(task),
        }
        Ok(())
    }
}

fn connect(events: channel::Sender<Event>, address: Option<&str>) -> zbus::Result<Connection> {
    let shared = Shared {
        events,
        registry: Arc::default(),
    };
    let builder = if let Some(address) = address {
        Builder::address(address)?
    } else {
        Builder::session()?
    };
    let mut builder = builder;
    for path in PATHS {
        builder = builder.serve_at(path, Server(shared.clone()))?;
    }
    let connection = builder.build()?;
    // DoNotQueue alone neither replaces the owner nor allows later replacement.
    if connection.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into())?
        != RequestNameReply::PrimaryOwner
    {
        return Err(zbus::Error::NameTaken);
    }
    Ok(connection)
}

fn spawn(events: channel::Sender<Event>) {
    let spawned = std::thread::Builder::new()
        .name("screensaver-dbus".into())
        .spawn(move || match connect(events, None) {
            Ok(_connection) => loop {
                // The zbus executor serves calls; this thread only keeps the name alive.
                std::thread::park();
            },
            Err(error) => {
                tracing::info!(%error, "ScreenSaver service inactive (session bus unavailable or name owned)");
            }
        });
    if let Err(error) = spawned {
        tracing::info!(%error, "cannot start ScreenSaver service");
    }
}

fn channel(handle: &LoopHandle<'static, Gavno>) -> Option<channel::Sender<Event>> {
    let (tx, rx) = channel::channel();
    handle
        .insert_source(rx, |event, _, gv| {
            let _stall = crate::perf::Stall::new("dbus.screensaver");
            let channel::Event::Msg(event) = event else {
                return;
            };
            match event {
                Event::Count(count) => gv.protocols.idle_policy.external_inhibits = count,
                Event::Activity => gv.protocols.idle_policy.activity(),
            }
            gv.refresh_idle();
        })
        .map_err(|error| tracing::warn!(%error, "cannot register ScreenSaver channel"))
        .ok()?;
    Some(tx)
}

/// Real TTY session only: a nested instance must not take the host session's name.
pub fn start(gv: &mut Gavno) {
    if let Some(events) = channel(&gv.loop_handle) {
        spawn(events);
    }
}

#[cfg(test)]
mod tests;
