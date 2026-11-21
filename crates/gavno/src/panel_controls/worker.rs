//! Blocking workers sleep on commands and service signals, never on periodic ticks.
use super::{audio, bluetooth, wifi, Kind, Secret};
use smithay::reexports::calloop::channel::Sender as EventSender;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        Arc,
    },
    time::{Duration, Instant},
};
/// Minimum spacing of signal-driven snapshots while the panel is closed.
const CLOSED_REFRESH: Duration = Duration::from_millis(500);
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
    /// Mirrors the last `Visible` command so signal threads can ignore noisy
    /// access-point updates while nobody looks at the list.
    pub visible: Arc<AtomicBool>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
    }
}
impl Worker {
    pub fn new(sender: Sender<Command>) -> Self {
        Self {
            sender,
            visible: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn send(&self, command: Command) {
        if let Command::Visible(value) = &command {
            self.visible.store(*value, Ordering::Relaxed);
        }
        let _ = self.sender.send(command);
    }
}
fn signals(
    connection: zbus::blocking::Connection,
    service: &'static str,
    sender: Sender<Command>,
    visible: Arc<AtomicBool>,
) -> zbus::Result<()> {
    // Sender matching also handles PropertiesChanged and ObjectManager changes.
    let rules = [format!("type='signal',sender='{service}'"),format!("type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{service}'")];
    for rule in rules {
        let signals =
            zbus::blocking::MessageIterator::for_match_rule(rule.as_str(), &connection, Some(128))?;
        let sender = sender.clone();
        let visible = visible.clone();
        std::thread::Builder::new()
            .name(format!("panel-signals-{service}"))
            .spawn(move || {
                for message in signals {
                    let Ok(message) = message else {
                        break;
                    };
                    // Access-point strength churns constantly; the bar only needs the
                    // active one, which NetworkManager also reports on the device.
                    let noisy = message
                        .header()
                        .path()
                        .is_some_and(|p| p.as_str().contains("/AccessPoint/"));
                    if (noisy || (service == "org.freedesktop.NetworkManager" && crate::topbar::status::nm_noise(&message))) && !visible.load(Ordering::Relaxed) {
                        continue;
                    }
                    if sender.send(Command::Refresh).is_err() {
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
/// Takes the next command plus everything already queued, so a burst of
/// signals costs one snapshot. `None` means the channel closed or a timeout
/// fired (`timed_out` is set in that case).
fn next_batch(
    receive: &mpsc::Receiver<Command>,
    wait_until: Option<Instant>,
    timed_out: &mut bool,
) -> Option<VecDeque<Command>> {
    *timed_out = false;
    let first = match wait_until {
        Some(at) => match receive.recv_timeout(at.saturating_duration_since(Instant::now())) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                *timed_out = true;
                return None;
            }
            Err(_) => return None,
        },
        None => receive.recv().ok()?,
    };
    let mut batch = VecDeque::from([first]);
    while let Ok(next) = receive.try_recv() {
        batch.push_back(next);
    }
    Some(batch)
}
/// Decides when a snapshot is due. Real commands and an open panel refresh at
/// once; signal-only bursts while closed are spaced by `CLOSED_REFRESH` using a
/// one-shot deadline that exists only while a refresh is being held back.
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    due: Option<Instant>,
}
impl Throttle {
    /// True when a snapshot should be taken now.
    fn request(&mut self, urgent: bool) -> bool {
        let now = Instant::now();
        let ready_at = self.last.map(|t| t + CLOSED_REFRESH);
        match ready_at {
            Some(at) if !urgent && now < at => {
                self.due.get_or_insert(at);
                false
            }
            _ => {
                self.due = None;
                true
            }
        }
    }
    fn taken(&mut self) {
        self.last = Some(Instant::now());
        self.due = None;
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
    let mut throttle = Throttle::default();
    let mut snapshot_due = true;
    loop {
        if snapshot_due {
            let snapshot = if open {
                provider.snapshot()
            } else {
                provider.bar_snapshot()
            }
            .map(Snapshot::Wifi);
            throttle.taken();
            match snapshot {
                Ok(s) => emit(&events, Kind::Wifi, Some(s), error.take(), &mut previous),
                Err(e) => emit(&events, Kind::Wifi, None, Some(e), &mut previous),
            }
        }
        let mut timed_out = false;
        let Some(batch) = next_batch(&receive, throttle.due, &mut timed_out) else {
            if timed_out {
                snapshot_due = true;
                continue;
            }
            break;
        };
        let mut acted = false;
        for command in batch {
            let result = match command {
                Command::Shutdown => return,
                Command::Refresh => continue,
                Command::Visible(value) => {
                    acted = true;
                    let opening = value && !open;
                    open = value;
                    if opening {
                        provider.scan()
                    } else {
                        Ok(())
                    }
                }
                Command::Power(v) => {
                    acted = true;
                    provider.power(v)
                }
                Command::Connect(n, p) if open => {
                    acted = true;
                    provider.connect(&n, p)
                }
                Command::Disconnect if open => {
                    acted = true;
                    provider.disconnect()
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                error = Some(e);
            }
        }
        snapshot_due = throttle.request(acted || open);
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
    let mut throttle = Throttle::default();
    let mut snapshot_due = true;
    loop {
        if deadline.is_some_and(|at| Instant::now() >= at) {
            deadline = None;
            error = provider.discovery(false).err();
            snapshot_due = true;
        }
        if snapshot_due {
            throttle.taken();
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
        }
        let wake = match (deadline, throttle.due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mut timed_out = false;
        let Some(batch) = next_batch(&receive, wake, &mut timed_out) else {
            if timed_out {
                // Either the discovery limit or a held-back refresh is due.
                snapshot_due = throttle.due.is_some_and(|at| Instant::now() >= at)
                    || deadline.is_some_and(|at| Instant::now() >= at);
                throttle.due = None;
                continue;
            }
            break;
        };
        let mut acted = false;
        for command in batch {
            let result = match command {
                Command::Shutdown => {
                    if deadline.is_some() {
                        let _ = provider.discovery(false);
                    }
                    return;
                }
                Command::Refresh => continue,
                Command::Visible(v) => {
                    acted = true;
                    open = v;
                    if !v && deadline.take().is_some() {
                        provider.discovery(false)
                    } else {
                        Ok(())
                    }
                }
                Command::Power(v) => {
                    acted = true;
                    if !v && deadline.take().is_some() {
                        let _ = provider.discovery(false);
                    }
                    provider.power(v)
                }
                Command::Search if open => {
                    acted = true;
                    if deadline.is_none() {
                        match provider.discovery(true) {
                            Ok(()) => {
                                deadline =
                                    Some(Instant::now() + limit.min(Duration::from_secs(30)));
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        Ok(())
                    }
                }
                Command::Device(d) if open => {
                    acted = true;
                    if deadline.take().is_some() {
                        let _ = provider.discovery(false);
                    }
                    provider.toggle(&d)
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                error = Some(e);
            }
        }
        snapshot_due = throttle.request(acted || open);
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
    let worker = Worker::new(sender);
    let visible = worker.visible.clone();
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
                    if signals(connection.clone(), service, signal_sender, visible).is_err() {
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
    Some(worker)
}
