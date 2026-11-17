//! BlueZ access, with a temporary Just Works agent for outgoing pairing only.
use std::collections::HashMap;
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue},
};
const BLUEZ: &str = "org.bluez";
type Properties = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Properties>>;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    pub path: String,
    pub name: String,
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    pub battery: Option<u8>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub adapter: String,
    pub powered: bool,
    pub blocked: bool,
    pub discovering: bool,
    pub devices: Vec<Device>,
}
impl Snapshot {
    pub fn icon(&self) -> &'static str {
        if !self.powered {
            "bluetooth-disabled-symbolic"
        } else if self.devices.iter().any(|d| d.connected) {
            "bluetooth-active-symbolic"
        } else {
            "bluetooth-symbolic"
        }
    }
    pub fn battery(&self) -> Option<u8> {
        self.devices
            .iter()
            .find(|d| d.connected && d.battery.is_some())
            .and_then(|d| d.battery)
    }
}
fn string(p: &Properties, key: &str) -> String {
    p.get(key)
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}
fn boolean(p: &Properties, key: &str) -> bool {
    p.get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}
pub fn map_objects(objects: Objects) -> Snapshot {
    let mut result = Snapshot::default();
    let mut entries: Vec<_> = objects.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, interfaces) in &entries {
        if let Some(p) = interfaces.get("org.bluez.Adapter1") {
            result.adapter = path.to_string();
            result.powered = boolean(p, "Powered");
            result.discovering = boolean(p, "Discovering");
            break;
        }
    }
    for (path, interfaces) in entries {
        if let Some(p) = interfaces.get("org.bluez.Device1") {
            if !path.as_str().starts_with(&format!("{}/", result.adapter)) {
                continue;
            }
            let raw = string(p, "Icon");
            let icon = if raw.contains("audio") {
                "audio-headphones-symbolic"
            } else if raw.contains("input") {
                "input-keyboard-symbolic"
            } else if raw.contains("phone") {
                "phone-symbolic"
            } else {
                "bluetooth-symbolic"
            };
            let name = string(p, "Alias");
            result.devices.push(Device {
                path: path.to_string(),
                name: if name.is_empty() {
                    string(p, "Address")
                } else {
                    name
                },
                icon: icon.into(),
                paired: boolean(p, "Paired"),
                connected: boolean(p, "Connected"),
                battery: interfaces
                    .get("org.bluez.Battery1")
                    .and_then(|p| p.get("Percentage"))
                    .and_then(|v| u8::try_from(v).ok())
                    .map(|n| n.min(100)),
            });
        }
    }
    result
        .devices
        .sort_by(|a, b| b.paired.cmp(&a.paired).then(a.name.cmp(&b.name)));
    result
}
pub fn rfkill_blocked(root: &std::path::Path) -> bool {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let path = entry.path();
            std::fs::read_to_string(path.join("type")).is_ok_and(|s| s.trim() == "bluetooth")
                && ["soft", "hard"].iter().any(|key| {
                    std::fs::read_to_string(path.join(key)).is_ok_and(|s| s.trim() == "1")
                })
        })
}
pub trait Provider: Send {
    fn snapshot(&mut self) -> Result<Snapshot, String>;
    fn power(&mut self, enabled: bool) -> Result<(), String>;
    fn discovery(&mut self, enabled: bool) -> Result<(), String>;
    fn toggle(&mut self, device: &Device) -> Result<(), String>;
}
pub struct Dbus {
    pub connection: Connection,
    adapter: String,
    discovery: Option<Discovery>,
}
// The independent lease stops discovery even if a snapshot or pairing call stalls.
struct Discovery {
    cancel: std::sync::mpsc::Sender<()>,
    finished: std::sync::mpsc::Receiver<Result<(), String>>,
}
impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
    }
}
fn proxy<'a>(c: &'a Connection, path: &'a str, interface: &'a str) -> zbus::Result<Proxy<'a>> {
    zbus::blocking::proxy::Builder::new(c)
        .destination(BLUEZ)?
        .path(path)?
        .interface(interface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
}
struct Agent;
#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}
    fn cancel(&self) {}
    fn request_pin_code(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::NotSupported(
            "Passkey entry is not supported".into(),
        ))
    }
    fn request_passkey(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<u32> {
        Err(zbus::fdo::Error::NotSupported(
            "Passkey entry is not supported".into(),
        ))
    }
    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: &str) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "Passkey entry is not supported".into(),
        ))
    }
    fn display_passkey(
        &self,
        _device: OwnedObjectPath,
        _passkey: u32,
        _entered: u16,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "Passkey entry is not supported".into(),
        ))
    }
    fn request_confirmation(
        &self,
        _device: OwnedObjectPath,
        _passkey: u32,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "Passkey confirmation is not supported".into(),
        ))
    }
    fn request_authorization(&self, _device: OwnedObjectPath) {}
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: &str) {}
}
impl Dbus {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            adapter: String::new(),
            discovery: None,
        }
    }
}
impl Provider for Dbus {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        let objects = proxy(&self.connection, "/", "org.freedesktop.DBus.ObjectManager")
            .and_then(|p| p.call::<_, _, Objects>("GetManagedObjects", &()))
            .map_err(|_| "Bluetooth service unavailable".to_string())?;
        let mut snapshot = map_objects(objects);
        snapshot.blocked = rfkill_blocked(std::path::Path::new("/sys/class/rfkill"));
        self.adapter = snapshot.adapter.clone();
        Ok(snapshot)
    }
    fn power(&mut self, enabled: bool) -> Result<(), String> {
        if enabled && rfkill_blocked(std::path::Path::new("/sys/class/rfkill")) {
            return Err(
                "Bluetooth is blocked by rfkill; release the hardware or software radio block"
                    .into(),
            );
        }
        if self.adapter.is_empty() {
            return Err("No Bluetooth adapter".into());
        }
        let result = proxy(&self.connection, &self.adapter, "org.bluez.Adapter1")
            .and_then(|p| p.set_property("Powered", enabled).map_err(Into::into));
        result.map_err(|_| "Cannot power Bluetooth; check hardware rfkill and permissions".into())
    }
    fn discovery(&mut self, enabled: bool) -> Result<(), String> {
        if !enabled {
            return if let Some(lease) = self.discovery.take() {
                let _ = lease.cancel.send(());
                lease
                    .finished
                    .recv()
                    .unwrap_or_else(|_| Err("Cannot stop Bluetooth discovery".into()))
            } else {
                Ok(())
            };
        }
        if self.discovery.is_some() {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        proxy(&self.connection, &self.adapter, "org.bluez.Adapter1")
            .and_then(|p| p.call::<_, _, ()>("StartDiscovery", &()))
            .map_err(|_| "Bluetooth discovery unavailable; check radio and rfkill".to_string())?;
        let (cancel, receive) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let connection = self.connection.clone();
        let adapter = self.adapter.clone();
        if std::thread::Builder::new()
            .name("panel-bluetooth-discovery".into())
            .spawn(move || {
                let _ = receive
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
                let result = proxy(&connection, &adapter, "org.bluez.Adapter1")
                    .and_then(|p| p.call::<_, _, ()>("StopDiscovery", &()))
                    .map_err(|_| "Cannot stop Bluetooth discovery".into());
                let _ = done.send(result);
            })
            .is_err()
        {
            let _ = proxy(&self.connection, &self.adapter, "org.bluez.Adapter1")
                .and_then(|p| p.call::<_, _, ()>("StopDiscovery", &()));
            return Err("Cannot start Bluetooth discovery worker".into());
        }
        self.discovery = Some(Discovery { cancel, finished });
        Ok(())
    }
    fn toggle(&mut self, d: &Device) -> Result<(), String> {
        let run = || -> zbus::Result<()> {
            let p = proxy(&self.connection, &d.path, "org.bluez.Device1")?;
            if d.connected {
                return p.call("Disconnect", &());
            }
            if !d.paired {
                const PATH: &str = "/org/gavno/PanelAgent";
                let manager = proxy(&self.connection, "/org/bluez", "org.bluez.AgentManager1")?;
                self.connection.object_server().at(PATH, Agent)?;
                let path = OwnedObjectPath::try_from(PATH)?;
                let registration =
                    manager.call::<_, _, ()>("RegisterAgent", &(path.clone(), "NoInputNoOutput"));
                let paired = registration.and_then(|()| p.call::<_, _, ()>("Pair", &()));
                let _ = manager.call::<_, _, ()>("UnregisterAgent", &(path,));
                let _ = self.connection.object_server().remove::<Agent, _>(PATH);
                paired?;
                p.set_property("Trusted", true)?;
            }
            p.call("Connect", &())
        };
        run().map_err(|_| {
            "Bluetooth connection failed. Devices requiring a passkey are unsupported".into()
        })
    }
}
