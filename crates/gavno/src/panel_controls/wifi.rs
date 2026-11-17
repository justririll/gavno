//! NetworkManager access. This module is called exclusively by the panel worker.
use super::Secret;
use std::collections::HashMap;
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};
const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
type Settings = HashMap<String, HashMap<String, OwnedValue>>;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Network {
    pub path: String,
    pub ssid: Vec<u8>,
    pub strength: u8,
    pub secured: bool,
    pub enterprise: bool,
    pub key_management: String,
    pub saved: Option<String>,
    pub active: bool,
}
impl Network {
    pub fn label(&self) -> String {
        String::from_utf8_lossy(&self.ssid)
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub enabled: bool,
    pub hardware: bool,
    pub device: String,
    pub networks: Vec<Network>,
    pub failure: Option<String>,
}
impl Snapshot {
    pub fn icon(&self) -> &'static str {
        if !self.enabled || !self.hardware {
            "network-wireless-disabled-symbolic"
        } else if let Some(n) = self.networks.iter().find(|n| n.active) {
            crate::topbar::status::strength_icon(n.strength)
        } else {
            "network-wireless-offline-symbolic"
        }
    }
}
pub fn deduplicate(mut networks: Vec<Network>) -> Vec<Network> {
    networks.retain(|n| !n.ssid.is_empty());
    networks.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.strength.cmp(&a.strength))
            .then(a.ssid.cmp(&b.ssid))
    });
    let mut seen = std::collections::HashSet::new();
    networks.retain(|n| seen.insert(n.ssid.clone()));
    networks.sort_by(|a, b| b.strength.cmp(&a.strength).then(a.ssid.cmp(&b.ssid)));
    networks
}
pub trait Provider: Send {
    fn snapshot(&mut self) -> Result<Snapshot, String>;
    fn power(&mut self, enabled: bool) -> Result<(), String>;
    fn scan(&mut self) -> Result<(), String>;
    fn connect(&mut self, network: &Network, password: Secret) -> Result<(), String>;
    fn disconnect(&mut self) -> Result<(), String>;
}
pub struct Dbus {
    pub connection: Connection,
    device: String,
    failure: Option<String>,
}
fn proxy<'a>(c: &'a Connection, path: &'a str, interface: &'a str) -> zbus::Result<Proxy<'a>> {
    zbus::blocking::proxy::Builder::new(c)
        .destination(NM)?
        .path(path)?
        .interface(interface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
}
impl Dbus {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            device: String::new(),
            failure: None,
        }
    }
    fn read(&mut self) -> zbus::Result<Snapshot> {
        let manager = proxy(&self.connection, ROOT, NM)?;
        let mut result = Snapshot {
            enabled: manager.get_property("WirelessEnabled")?,
            hardware: manager.get_property("WirelessHardwareEnabled")?,
            ..Default::default()
        };
        let devices: Vec<OwnedObjectPath> = manager.call("GetDevices", &())?;
        for device in devices {
            let p = proxy(
                &self.connection,
                device.as_str(),
                "org.freedesktop.NetworkManager.Device",
            )?;
            if p.get_property::<u32>("DeviceType")? != 2 {
                continue;
            }
            result.device = device.to_string();
            if p.get_property::<u32>("State").unwrap_or(0) == 100 {
                self.failure = None;
            }
            if p.get_property::<u32>("State").unwrap_or(0) == 120 {
                self.failure =
                    Some("Wi-Fi connection failed; check password, profile and signal".into());
            }
            break;
        }
        result.failure = self.failure.clone();
        self.device = result.device.clone();
        if self.device.is_empty() {
            return Ok(result);
        }
        let wireless = proxy(&self.connection, &self.device, WIRELESS)?;
        let active: OwnedObjectPath = wireless.get_property("ActiveAccessPoint")?;
        let aps: Vec<OwnedObjectPath> = wireless.call("GetAllAccessPoints", &())?;
        let settings = proxy(
            &self.connection,
            "/org/freedesktop/NetworkManager/Settings",
            "org.freedesktop.NetworkManager.Settings",
        )?;
        let profiles: Vec<OwnedObjectPath> = settings.call("ListConnections", &())?;
        let mut saved = HashMap::new();
        for path in profiles {
            let p = proxy(
                &self.connection,
                path.as_str(),
                "org.freedesktop.NetworkManager.Settings.Connection",
            )?;
            if let Ok(s) = p.call::<_, _, Settings>("GetSettings", &()) {
                if let Some(ssid) = s
                    .get("802-11-wireless")
                    .and_then(|v| v.get("ssid"))
                    .and_then(|v| Vec::<u8>::try_from(v.try_clone().ok()?).ok())
                {
                    saved.insert(ssid, path.to_string());
                }
            }
        }
        for path in aps {
            let ap = proxy(
                &self.connection,
                path.as_str(),
                "org.freedesktop.NetworkManager.AccessPoint",
            )?;
            let Ok(ssid) = ap.get_property::<Vec<u8>>("Ssid") else {
                continue;
            };
            let flags: u32 = ap.get_property("Flags")?;
            let security =
                ap.get_property::<u32>("WpaFlags")? | ap.get_property::<u32>("RsnFlags")?;
            result.networks.push(Network {
                saved: saved.get(&ssid).cloned(),
                ssid,
                strength: ap.get_property::<u8>("Strength")?.min(100),
                secured: flags & 1 != 0 || security != 0,
                enterprise: security & 0x200 != 0 && security & 0x100 == 0,
                key_management: if security & 0x100 != 0 {
                    "wpa-psk"
                } else if security & 0x400 != 0 {
                    "sae"
                } else if security & 0x800 != 0 {
                    "owe"
                } else {
                    "none"
                }
                .into(),
                active: path == active,
                path: path.to_string(),
            });
        }
        result.networks = deduplicate(result.networks);
        Ok(result)
    }
}
impl Provider for Dbus {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.read().map_err(|_| "NetworkManager unavailable".into())
    }
    fn power(&mut self, enabled: bool) -> Result<(), String> {
        self.failure = None;
        let run = || -> zbus::Result<()> {
            proxy(&self.connection, ROOT, NM)?.set_property("WirelessEnabled", enabled)?;
            Ok(())
        };
        run().map_err(|_| "Cannot change Wi-Fi radio; check hardware rfkill and permissions".into())
    }
    fn scan(&mut self) -> Result<(), String> {
        if self.device.is_empty() {
            return Err("No Wi-Fi adapter".into());
        }
        proxy(&self.connection, &self.device, WIRELESS)
            .and_then(|p| {
                p.call::<_, _, ()>("RequestScan", &(HashMap::<String, Value<'_>>::new(),))
            })
            .map_err(|_| "Scan unavailable; showing cached networks".into())
    }
    fn disconnect(&mut self) -> Result<(), String> {
        self.failure = None;
        proxy(
            &self.connection,
            &self.device,
            "org.freedesktop.NetworkManager.Device",
        )
        .and_then(|p| p.call::<_, _, ()>("Disconnect", &()))
        .map_err(|_| "Cannot disconnect Wi-Fi".into())
    }
    fn connect(&mut self, n: &Network, password: Secret) -> Result<(), String> {
        self.failure = None;
        // Never format D-Bus errors: a remote peer may echo connection secrets.
        let run = || -> zbus::Result<()> {
            let manager = proxy(&self.connection, ROOT, NM)?;
            let device = OwnedObjectPath::try_from(self.device.as_str())?;
            let ap = OwnedObjectPath::try_from(n.path.as_str())?;
            if let Some(saved) = &n.saved {
                let _: OwnedObjectPath = manager.call(
                    "ActivateConnection",
                    &(OwnedObjectPath::try_from(saved.as_str())?, device, ap),
                )?;
            } else {
                let mut settings: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::new();
                let label = n.label();
                settings.insert(
                    "connection",
                    HashMap::from([
                        ("id", Value::from(label.as_str())),
                        ("type", Value::from("802-11-wireless")),
                    ]),
                );
                settings.insert(
                    "802-11-wireless",
                    HashMap::from([
                        ("ssid", Value::from(n.ssid.clone())),
                        ("mode", Value::from("infrastructure")),
                    ]),
                );
                if n.secured && n.key_management == "owe" {
                    settings.insert(
                        "802-11-wireless-security",
                        HashMap::from([("key-mgmt", Value::from("owe"))]),
                    );
                } else if n.secured {
                    settings.insert(
                        "802-11-wireless-security",
                        HashMap::from([
                            ("key-mgmt", Value::from(n.key_management.as_str())),
                            (
                                if n.key_management == "none" {
                                    "wep-key0"
                                } else {
                                    "psk"
                                },
                                Value::from(password.0.as_str()),
                            ),
                        ]),
                    );
                }
                let _: (OwnedObjectPath, OwnedObjectPath) =
                    manager.call("AddAndActivateConnection", &(settings, device, ap))?;
            }
            Ok(())
        };
        if n.enterprise && n.saved.is_none() {
            return Err("Enterprise Wi-Fi requires a saved profile".into());
        }
        run().map_err(|_| "Connection failed; check password and saved profile".into())
    }
}
