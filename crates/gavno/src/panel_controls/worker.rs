//! Blocking workers sleep on commands and service signals, never on periodic ticks.
use super::{audio, bluetooth, wifi, Kind, Secret};
use smithay::reexports::calloop::channel::Sender as EventSender;
use std::{
    sync::mpsc::{self, Sender},
    time::{Duration, Instant},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Snapshot {
    Wifi(wifi::Snapshot),
    Bluetooth(bluetooth::Snapshot),
    Audio(audio::Snapshot),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: Kind,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
}
// Deliberately no Debug: commands may own a password.
pub enum Command {
    Shutdown,
    AudioDisconnected,
    Visible(bool),
    Refresh,
    Power(bool),
    Connect(wifi::Network, Secret),
    Disconnect,
    Search,
    Device(bluetooth::Device),
    Volume(audio::Target, u8),
    Mute(audio::Target),
    Output(String),
}
pub struct Worker {
    pub sender: Sender<Command>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
    }
}
impl Worker {
    pub fn send(&self, command: Command) {
        let _ = self.sender.send(command);
    }
}
fn signals(
    connection: zbus::blocking::Connection,
    service: &'static str,
    sender: Sender<Command>,
) -> zbus::Result<()> {
    // Sender matching also handles PropertiesChanged and ObjectManager changes.
    let rules = [format!("type='signal',sender='{service}'"),format!("type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{service}'")];
    for rule in rules {
        let signals =
            zbus::blocking::MessageIterator::for_match_rule(rule.as_str(), &connection, Some(128))?;
        let sender = sender.clone();
        std::thread::Builder::new()
            .name(format!("panel-signals-{service}"))
            .spawn(move || {
                for message in signals {
                    if message.is_err() || sender.send(Command::Refresh).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    }
    Ok(())
}
fn emit(
    events: &EventSender<Event>,
    kind: Kind,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    previous: &mut Option<Event>,
) {
    let event = Event {
        kind,
        snapshot,
        error,
    };
    if previous.as_ref() != Some(&event) {
        let _ = events.send(event.clone());
        *previous = Some(event);
    }
}
pub fn wifi_loop(
    mut provider: Box<dyn wifi::Provider>,
    receive: mpsc::Receiver<Command>,
    events: EventSender<Event>,
) {
    let mut previous = None;
    let mut error = None;
    let mut open = false;
    loop {
        let snapshot = provider.snapshot().map(Snapshot::Wifi);
        match snapshot {
            Ok(s) => emit(&events, Kind::Wifi, Some(s), error.take(), &mut previous),
            Err(e) => emit(&events, Kind::Wifi, None, Some(e), &mut previous),
        }
        let Ok(command) = receive.recv() else {
            break;
        };
        if matches!(command, Command::Shutdown) {
            break;
        }
        let result = match command {
            Command::Visible(value) => {
                let opening = value && !open;
                open = value;
                if opening {
                    provider.scan()
                } else {
                    Ok(())
                }
            }
            Command::Power(v) => provider.power(v),
            Command::Connect(n, p) if open => provider.connect(&n, p),
            Command::Disconnect if open => provider.disconnect(),
            _ => Ok(()),
        };
        error = result.err();
    }
}
pub fn bluetooth_loop(
    provider: Box<dyn bluetooth::Provider>,
    receive: mpsc::Receiver<Command>,
    events: EventSender<Event>,
) {
    bluetooth_loop_limit(provider, receive, events, Duration::from_secs(30));
}
pub(super) fn bluetooth_loop_limit(
    mut provider: Box<dyn bluetooth::Provider>,
    receive: mpsc::Receiver<Command>,
    events: EventSender<Event>,
    limit: Duration,
) {
    let mut previous = None;
    let mut error = None;
    let mut open = false;
    let mut deadline: Option<Instant> = None;
    loop {
        if deadline.is_some_and(|at| Instant::now() >= at) {
            deadline = None;
            error = provider.discovery(false).err();
        }
        match provider.snapshot() {
            Ok(s) => emit(
                &events,
                Kind::Bluetooth,
                Some(Snapshot::Bluetooth(s)),
                error.take(),
                &mut previous,
            ),
            Err(e) => emit(&events, Kind::Bluetooth, None, Some(e), &mut previous),
        }
        let command = if let Some(at) = deadline {
            match receive.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(c) => c,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    deadline = None;
                    error = provider.discovery(false).err();
                    continue;
                }
                Err(_) => break,
            }
        } else {
            let Ok(c) = receive.recv() else {
                break;
            };
            c
        };
        if matches!(command, Command::Shutdown) {
            break;
        }
        let result = match command {
            Command::Visible(v) => {
                open = v;
                if !v && deadline.take().is_some() {
                    provider.discovery(false)
                } else {
                    Ok(())
                }
            }
            Command::Power(v) => {
                if !v && deadline.take().is_some() {
                    let _ = provider.discovery(false);
                }
                provider.power(v)
            }
            Command::Search if open => {
                if deadline.is_none() {
                    match provider.discovery(true) {
                        Ok(()) => {
                            deadline = Some(Instant::now() + limit.min(Duration::from_secs(30)));
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(())
                }
            }
            Command::Device(d) if open => {
                if deadline.take().is_some() {
                    let _ = provider.discovery(false);
                }
                provider.toggle(&d)
            }
            _ => Ok(()),
        };
        error = result.err();
    }
    if deadline.is_some() {
        let _ = provider.discovery(false);
    }
}
pub fn audio_loop(
    mut provider: Box<dyn audio::Provider>,
    receive: mpsc::Receiver<Command>,
    events: EventSender<Event>,
) {
    let mut pending = None;
    let mut previous = None;
    let mut error = None;
    loop {
        match provider.snapshot() {
            Ok(s) => emit(
                &events,
                Kind::Audio,
                Some(Snapshot::Audio(s)),
                error.take(),
                &mut previous,
            ),
            Err(e) => emit(&events, Kind::Audio, None, Some(e), &mut previous),
        }
        let Some(mut command) = pending.take().or_else(|| receive.recv().ok()) else {
            break;
        };
        if let Command::Volume(target, value) = &mut command {
            while let Ok(next) = receive.try_recv() {
                match next {
                    Command::Volume(next_target, next_value) if next_target == *target => {
                        *value = next_value
                    }
                    Command::Refresh => {}
                    other => {
                        pending = Some(other);
                        break;
                    }
                }
            }
        }
        if matches!(command, Command::Shutdown) {
            break;
        }
        error = match command {
            Command::Visible(true) => {
                provider.opened();
                Ok(())
            }
            Command::AudioDisconnected => {
                Err("Audio updates disconnected; reopen the mixer to reconnect".into())
            }
            Command::Volume(t, v) => provider.volume(&t, v),
            Command::Mute(t) => provider.mute(&t),
            Command::Output(id) => provider.output(&id),
            _ => Ok(()),
        }
        .err();
    }
}
pub fn spawn(kind: Kind, events: EventSender<Event>, fake: bool) -> Option<Worker> {
    let (sender, receive) = mpsc::channel();
    let signal_sender = sender.clone();
    std::thread::Builder::new()
        .name(format!("panel-{kind:?}"))
        .spawn(move || {
            if fake {
                super::fake::run(kind, receive, events);
                return;
            }
            match kind {
                Kind::Wifi | Kind::Bluetooth => {
                    let connection = match zbus::blocking::Connection::system() {
                        Ok(c) => c,
                        Err(_) => {
                            let _ = events.send(Event {
                                kind,
                                snapshot: None,
                                error: Some("System D-Bus unavailable".into()),
                            });
                            return;
                        }
                    };
                    let service = if kind == Kind::Wifi {
                        "org.freedesktop.NetworkManager"
                    } else {
                        "org.bluez"
                    };
                    if signals(connection.clone(), service, signal_sender).is_err() {
                        let _ = connection.clone().close();
                        let _ = events.send(Event {
                            kind,
                            snapshot: None,
                            error: Some("Cannot subscribe to device updates".into()),
                        });
                        return;
                    }
                    if kind == Kind::Wifi {
                        wifi_loop(
                            Box::new(wifi::Dbus::new(connection.clone())),
                            receive,
                            events,
                        );
                    } else {
                        bluetooth_loop(
                            Box::new(bluetooth::Dbus::new(connection.clone())),
                            receive,
                            events,
                        );
                    }
                    let _ = connection.close();
                }
                Kind::Audio => {
                    audio_loop(Box::new(audio::Pactl::new(signal_sender)), receive, events);
                }
            }
        })
        .ok()?;
    Some(Worker { sender })
}
