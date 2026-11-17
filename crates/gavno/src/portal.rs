//! `org.freedesktop.impl.portal.Settings` on the session bus: GAVNO is its own
//! xdg-desktop-portal Settings backend, so apps learn the colour scheme and accent colour
//! (libadwaita, Firefox, Qt 6). A named worker thread owns the connection and blocks on
//! the update channel, so an idle session costs nothing and the compositor never waits
//! on D-Bus.
use crate::Gavno;
use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use zbus::{
    blocking::{connection::Builder, Connection},
    fdo::{RequestNameFlags, RequestNameReply},
    zvariant::{OwnedValue, Value},
};

const NAME: &str = "org.freedesktop.impl.portal.desktop.gavno";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.Settings";
const APPEARANCE: &str = "org.freedesktop.appearance";
/// Apps restyle every window on an accent change, so keep them rare.
const ACCENT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// OKLab distance below which an accent change is not worth a restyle.
const ACCENT_DISTANCE: f64 = 0.02;

/// What the compositor tells the worker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Update {
    /// `color-scheme` value: 1 prefer dark, 2 prefer light, 0 no preference.
    ColorScheme(u32),
    /// Current accent as sRGB 0..1 (the theme palette is sRGB-encoded).
    Accent([f32; 3]),
}

/// `[theme] app_color_scheme` to the portal's `color-scheme` value.
pub fn color_scheme_value(setting: &str) -> u32 {
    match setting {
        "dark" => 1,
        "light" => 2,
        _ => 0,
    }
}

/// Compositor-side end: forwards changes, dropping repeats before they reach the channel.
pub struct Handle {
    tx: mpsc::Sender<Update>,
    scheme: u32,
    accent: [f32; 3],
}
impl Handle {
    pub fn color_scheme(&mut self, value: u32) {
        if value != self.scheme {
            self.scheme = value;
            let _ = self.tx.send(Update::ColorScheme(value));
        }
    }
    pub fn accent(&mut self, color: [f32; 4]) {
        let rgb = [color[0], color[1], color[2]];
        if rgb != self.accent {
            self.accent = rgb;
            let _ = self.tx.send(Update::Accent(rgb));
        }
    }
}

#[derive(Clone, Copy)]
struct Values {
    scheme: u32,
    accent: [f32; 3],
}

/// Accepts an accent only when it moved visibly and the last emission is old enough.
/// A visible change that comes too soon is held and released when the interval ends,
/// so the last change of a twilight transition still reaches apps.
#[derive(Default)]
struct AccentGate {
    last: Option<([f32; 3], Duration)>,
    held: Option<[f32; 3]>,
}
impl AccentGate {
    fn accept(&mut self, rgb: [f32; 3], now: Duration) -> bool {
        if let Some((old, at)) = self.last {
            let moved = crate::ui::theme::oklab(old)
                .into_iter()
                .zip(crate::ui::theme::oklab(rgb))
                .map(|(a, b)| (a - b).powi(2))
                .sum::<f64>()
                .sqrt();
            if moved <= ACCENT_DISTANCE {
                self.held = None;
                return false;
            }
            if now.saturating_sub(at) < ACCENT_INTERVAL {
                self.held = Some(rgb);
                return false;
            }
        }
        self.last = Some((rgb, now));
        self.held = None;
        true
    }
    /// When the held change may be emitted; `None` when nothing is held.
    fn due(&self) -> Option<Duration> {
        self.held.and(self.last).map(|(_, at)| at + ACCENT_INTERVAL)
    }
    /// The held change, once its interval has passed.
    fn release(&mut self, now: Duration) -> Option<[f32; 3]> {
        let rgb = self.held?;
        self.accept(rgb, now).then_some(rgb)
    }
}

fn owned(value: impl Into<Value<'static>>) -> OwnedValue {
    // Only file descriptors are refused, and none appear here.
    OwnedValue::try_from(value.into()).expect("plain value")
}

impl Values {
    fn appearance(&self) -> [(&'static str, OwnedValue); 3] {
        let [r, g, b] = self.accent.map(f64::from);
        [
            ("color-scheme", owned(self.scheme)),
            ("accent-color", owned((r, g, b))),
            ("contrast", owned(0u32)),
        ]
    }
}

/// Namespace filter of `ReadAll`: empty matches all, a trailing `*` is a prefix glob.
fn matches(patterns: &[String], namespace: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => namespace.starts_with(prefix),
            None => p == namespace,
        })
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
enum PortalError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotFound(String),
}

struct Server(Arc<Mutex<Values>>);
impl Server {
    fn values(&self) -> MutexGuard<'_, Values> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
impl Server {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    fn read_all(&self, namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedValue>> {
        let mut all = HashMap::new();
        if matches(&namespaces, APPEARANCE) {
            let entries = self.values().appearance();
            all.insert(
                APPEARANCE.to_owned(),
                entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            );
        }
        all
    }

    fn read(&self, namespace: String, key: String) -> Result<OwnedValue, PortalError> {
        let entries = self.values().appearance();
        (namespace == APPEARANCE)
            .then(|| entries.into_iter().find(|(k, _)| *k == key).map(|(_, v)| v))
            .flatten()
            .ok_or_else(|| PortalError::NotFound(format!("{namespace} {key}")))
    }
}

fn connect(values: Arc<Mutex<Values>>, address: Option<&str>) -> zbus::Result<Connection> {
    let builder = if let Some(address) = address {
        Builder::address(address)?
    } else {
        Builder::session()?
    };
    let connection = builder.serve_at(PATH, Server(values))?.build()?;
    // DoNotQueue alone neither replaces the owner nor allows later replacement.
    if connection.request_name_with_flags(NAME, RequestNameFlags::DoNotQueue.into())?
        != RequestNameReply::PrimaryOwner
    {
        return Err(zbus::Error::NameTaken);
    }
    Ok(connection)
}

fn emit(connection: &Connection, key: &str, value: Value<'_>) {
    let result = connection.emit_signal(
        None::<&str>,
        PATH,
        INTERFACE,
        "SettingChanged",
        &(APPEARANCE, key, value),
    );
    if let Err(error) = result {
        tracing::debug!(%error, key, "cannot emit portal SettingChanged");
    }
}

fn set_accent(connection: &Connection, values: &Mutex<Values>, rgb: [f32; 3]) {
    values.lock().unwrap_or_else(|e| e.into_inner()).accent = rgb;
    let [r, g, b] = rgb.map(f64::from);
    emit(connection, "accent-color", Value::from((r, g, b)));
}

/// Applies updates until the compositor side is dropped. `clock` is injectable for tests.
/// The thread only wakes on its own while an accent change is held back.
fn serve(
    connection: &Connection,
    values: &Mutex<Values>,
    updates: mpsc::Receiver<Update>,
    clock: impl Fn() -> Duration,
) {
    let mut gate = AccentGate::default();
    loop {
        let update = match gate.due() {
            None => updates.recv().ok(),
            Some(due) => match updates.recv_timeout(due.saturating_sub(clock())) {
                Ok(update) => Some(update),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(rgb) = gate.release(clock()) {
                        set_accent(connection, values, rgb);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            },
        };
        let Some(update) = update else { break };
        match update {
            Update::ColorScheme(scheme) => {
                values.lock().unwrap_or_else(|e| e.into_inner()).scheme = scheme;
                emit(connection, "color-scheme", Value::from(scheme));
            }
            Update::Accent(rgb) => {
                if gate.accept(rgb, clock()) {
                    set_accent(connection, values, rgb);
                }
            }
        }
    }
}

/// Starts the service; `None` when the worker thread cannot be created.
pub fn start(gv: &mut Gavno) {
    let scheme = color_scheme_value(&gv.config.theme.app_color_scheme);
    let accent = gv.topbar.accent();
    let accent = [accent[0], accent[1], accent[2]];
    let values = Arc::new(Mutex::new(Values { scheme, accent }));
    let (tx, rx) = mpsc::channel();
    let base = Instant::now();
    let spawned = std::thread::Builder::new()
        .name("portal-dbus".into())
        .spawn(move || match connect(values.clone(), None) {
            Ok(connection) => serve(&connection, &values, rx, || base.elapsed()),
            Err(error) => {
                tracing::info!(%error, "Settings portal backend inactive (session bus unavailable or name owned)");
            }
        });
    match spawned {
        Ok(_) => gv.portal = Some(Handle { tx, scheme, accent }),
        Err(error) => tracing::info!(%error, "cannot start Settings portal backend"),
    }
}

#[cfg(test)]
mod tests;
