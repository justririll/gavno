//! systemd unit control over the system bus. The polkit rule in
//! `dist/polkit-1/rules.d/50-local-llm.rules` lets wheel start and stop the
//! unit. Stopping always goes through systemd (SIGINT), never a signal sent
//! from here.
use crate::engine::UnitControl;
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::OwnedObjectPath,
};

pub const UNIT: &str = "llama-gavno.service";
pub const PORT: u16 = 8090;

pub struct SystemdUnit {
    pub name: String,
}

impl SystemdUnit {
    fn call(&self, method: &str) -> Result<(), String> {
        let bus = Connection::system().map_err(|e| e.to_string())?;
        let manager = Proxy::new(
            &bus,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .map_err(|e| e.to_string())?;
        let _job: OwnedObjectPath =
            manager.call(method, &(self.name.as_str(), "replace")).map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl UnitControl for SystemdUnit {
    fn start(&self) -> Result<(), String> {
        self.call("StartUnit")
    }
    fn stop(&self) -> Result<(), String> {
        self.call("StopUnit")
    }
}

/// For development against a server started by hand (`GAVNO_AI_UNIT=`).
pub struct NoUnit;

impl UnitControl for NoUnit {
    fn start(&self) -> Result<(), String> {
        Ok(())
    }
    fn stop(&self) -> Result<(), String> {
        Ok(())
    }
}
