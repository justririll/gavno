use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

struct Bus(Child, String);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn bus() -> Option<Bus> {
    let mut child = Command::new("dbus-daemon")
        .env_remove("XDG_RUNTIME_DIR")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut address = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut address).ok()?;
    Some(Bus(child, address.trim().to_owned()))
}

type Tree = HashMap<String, HashMap<String, OwnedValue>>;
const RED: [f32; 3] = [0.9, 0.2, 0.2];
const BLUE: [f32; 3] = [0.2, 0.2, 0.9];

struct Fixture {
    _bus: Bus,
    client: Connection,
    updates: mpsc::Sender<Update>,
    seconds: Arc<AtomicU64>,
    signals: mpsc::Receiver<zbus::Message>,
}
impl Fixture {
    fn read_all(&self, patterns: &[&str]) -> Tree {
        self.call("ReadAll", &(patterns.to_vec(),)).unwrap().body().deserialize().unwrap()
    }
    fn call(&self, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
        self.client.call_method(Some(NAME), PATH, Some(INTERFACE), method, body)
    }
    fn read(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue> {
        self.call("Read", &(namespace, key))?.body().deserialize()
    }
    /// Next `SettingChanged` key and value, or `None` after a short wait.
    fn signal(&mut self) -> Option<(String, OwnedValue)> {
        let message = self.signals.recv_timeout(Duration::from_millis(700)).ok()?;
        let (namespace, key, value): (String, String, OwnedValue) = message.body().deserialize().ok()?;
        assert_eq!(namespace, APPEARANCE);
        Some((key, value))
    }
}
fn fixture() -> Option<Fixture> {
    let bus = bus()?;
    let values = Arc::new(Mutex::new(Values { scheme: 1, accent: RED }));
    let server = connect(values.clone(), Some(&bus.1)).unwrap();
    let (updates, rx) = mpsc::channel();
    let seconds = Arc::new(AtomicU64::new(0));
    let clock = seconds.clone();
    std::thread::spawn(move || {
        serve(&server, &values, rx, || Duration::from_secs(clock.load(Ordering::SeqCst)))
    });
    let client = Builder::address(bus.1.as_str()).unwrap().build().unwrap();
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(INTERFACE)
        .unwrap()
        .member("SettingChanged")
        .unwrap()
        .build();
    let iterator = zbus::blocking::MessageIterator::for_match_rule(rule, &client, Some(8)).unwrap();
    let (forward, signals) = mpsc::channel();
    std::thread::spawn(move || {
        for message in iterator.flatten() {
            if forward.send(message).is_err() {
                break;
            }
        }
    });
    Some(Fixture { _bus: bus, client, updates, seconds, signals })
}

fn rgb(value: &OwnedValue) -> (f64, f64, f64) {
    value.try_clone().unwrap().try_into().unwrap()
}

#[test]
fn read_all_filters_namespaces() {
    let Some(f) = fixture() else { return };
    for patterns in [&[][..], &[APPEARANCE], &["org.freedesktop.*"], &["*"]] {
        let tree = f.read_all(patterns);
        let appearance = &tree[APPEARANCE];
        assert_eq!(u32::try_from(appearance["color-scheme"].try_clone().unwrap()).unwrap(), 1);
        assert_eq!(u32::try_from(appearance["contrast"].try_clone().unwrap()).unwrap(), 0);
        assert!((rgb(&appearance["accent-color"]).0 - f64::from(RED[0])).abs() < 1e-6);
    }
    assert!(f.read_all(&["org.gnome.*"]).is_empty());
    assert!(f.read_all(&["org.freedesktop.appearanc"]).is_empty());
}

#[test]
fn read_returns_keys_and_not_found() {
    let Some(f) = fixture() else { return };
    assert_eq!(u32::try_from(f.read(APPEARANCE, "color-scheme").unwrap()).unwrap(), 1);
    assert_eq!(u32::try_from(f.read(APPEARANCE, "contrast").unwrap()).unwrap(), 0);
    assert_eq!(rgb(&f.read(APPEARANCE, "accent-color").unwrap()).2 as f32, RED[2]);
    for (namespace, key) in [(APPEARANCE, "nope"), ("org.gnome.desktop", "color-scheme")] {
        let error = f.read(namespace, key).unwrap_err();
        assert!(error.to_string().contains("org.freedesktop.portal.Error.NotFound"), "{error}");
    }
    let version: u32 = f
        .client
        .call_method(Some(NAME), PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(INTERFACE, "version"))
        .unwrap()
        .body()
        .deserialize::<OwnedValue>()
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(version, 2);
}

#[test]
fn color_scheme_change_is_emitted() {
    let Some(mut f) = fixture() else { return };
    f.updates.send(Update::ColorScheme(2)).unwrap();
    let (key, value) = f.signal().expect("signal");
    assert_eq!(key, "color-scheme");
    assert_eq!(u32::try_from(value).unwrap(), 2);
    assert_eq!(u32::try_from(f.read(APPEARANCE, "color-scheme").unwrap()).unwrap(), 2);
}

#[test]
fn accent_is_rate_limited() {
    let Some(mut f) = fixture() else { return };
    // The first accent is emitted at once.
    f.updates.send(Update::Accent(RED)).unwrap();
    assert_eq!(f.signal().expect("first").0, "accent-color");
    // Big move, but too soon: dropped.
    f.seconds.store(60, Ordering::SeqCst);
    f.updates.send(Update::Accent(BLUE)).unwrap();
    assert!(f.signal().is_none());
    assert!((rgb(&f.read(APPEARANCE, "accent-color").unwrap()).2 - f64::from(RED[2])).abs() < 1e-6);
    // Old enough but imperceptible: dropped.
    f.seconds.store(2000, Ordering::SeqCst);
    f.updates.send(Update::Accent([0.9, 0.2, 0.205])).unwrap();
    assert!(f.signal().is_none());
    // Old enough and visible: emitted with the new value.
    f.updates.send(Update::Accent(BLUE)).unwrap();
    let (key, value) = f.signal().expect("big change");
    assert_eq!(key, "accent-color");
    assert!((rgb(&value).2 - f64::from(BLUE[2])).abs() < 1e-6);
}

#[test]
fn gate_and_handle_logic() {
    let mut gate = AccentGate::default();
    assert!(gate.accept(RED, Duration::ZERO));
    assert_eq!(gate.due(), None);
    assert!(!gate.accept(BLUE, ACCENT_INTERVAL - Duration::from_secs(1)));
    assert!(gate.accept(BLUE, ACCENT_INTERVAL));
    assert_eq!(gate.due(), None);
    // A visible change that comes too soon is held until the interval ends.
    let later = ACCENT_INTERVAL + Duration::from_secs(60);
    assert!(!gate.accept(RED, later));
    assert_eq!(gate.due(), Some(ACCENT_INTERVAL * 2));
    assert_eq!(gate.release(ACCENT_INTERVAL * 2 - Duration::from_secs(1)), None);
    assert_eq!(gate.release(ACCENT_INTERVAL * 2), Some(RED));
    assert_eq!(gate.due(), None);
    // Moving back to the emitted colour cancels a held change.
    assert!(!gate.accept(BLUE, ACCENT_INTERVAL * 2 + Duration::from_secs(1)));
    assert!(gate.due().is_some());
    assert!(!gate.accept(RED, ACCENT_INTERVAL * 2 + Duration::from_secs(2)));
    assert_eq!(gate.due(), None);
    let (tx, rx) = mpsc::channel();
    let mut handle = Handle { tx, scheme: 1, accent: RED };
    handle.color_scheme(1);
    handle.accent([0.9, 0.2, 0.2, 1.0]);
    assert!(rx.try_recv().is_err());
    handle.color_scheme(2);
    handle.accent([0.1, 0.2, 0.3, 1.0]);
    assert_eq!(rx.try_recv(), Ok(Update::ColorScheme(2)));
    assert_eq!(rx.try_recv(), Ok(Update::Accent([0.1, 0.2, 0.3])));
    assert_eq!([color_scheme_value("dark"), color_scheme_value("light"), color_scheme_value("none")], [1, 2, 0]);
}

#[test]
fn taken_name_is_reported_not_replaced() {
    let Some(f) = fixture() else { return };
    let values = Arc::new(Mutex::new(Values { scheme: 1, accent: RED }));
    assert!(connect(values, Some(&f._bus.1)).is_err());
}
