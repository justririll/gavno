//! The polkit AuthenticationAgent object, served on the system bus by one
//! worker thread that blocks until the feature stops.
use super::{helper, Begin, Outcome, Request};
use smithay::reexports::calloop::channel;
use std::{collections::HashMap, sync::mpsc};
use zbus::zvariant::{OwnedObjectPath, Value};

pub const PATH: &str = "/org/gavno/PolkitAgent";
pub const NAME: &str = "org.freedesktop.PolicyKit1";
pub const AUTHORITY: &str = "/org/freedesktop/PolicyKit1/Authority";
pub const AUTHORITY_INTERFACE: &str = "org.freedesktop.PolicyKit1.Authority";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum PolkitError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
}

fn bounded(value: String, max: usize) -> String {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

struct Agent {
    events: channel::Sender<Request>,
    own_uid: u32,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        _action_id: String,
        message: String,
        icon_name: String,
        _details: HashMap<String, String>,
        cookie: String,
        identities: Vec<helper::Identity>,
    ) -> Result<(), PolkitError> {
        let users: Vec<(u32, String)> = helper::allowed_uids(&identities)
            .into_iter()
            .filter_map(|uid| helper::user_name(uid).map(|name| (uid, name)))
            .collect();
        let uids: Vec<u32> = users.iter().map(|(uid, _)| *uid).collect();
        let selected = helper::preferred_user(&uids, self.own_uid)
            .ok_or_else(|| PolkitError::Failed("No supported user to authenticate as".into()))?;
        let (reply, outcome) = async_channel::bounded(1);
        self.events
            .send(Request::Begin(Begin {
                cookie: bounded(cookie, 512),
                message: bounded(message, 2048),
                icon: bounded(icon_name, 256),
                users: users.into_iter().map(|(_, name)| name).collect(),
                selected,
                reply,
            }))
            .map_err(|_| PolkitError::Failed("Polkit agent stopped".into()))?;
        match outcome.recv().await {
            Ok(Outcome::Success) => Ok(()),
            Ok(Outcome::Failed(message)) => Err(PolkitError::Failed(message)),
            Ok(Outcome::Cancelled) | Err(_) => Err(PolkitError::Cancelled("Authentication cancelled".into())),
        }
    }

    fn cancel_authentication(&self, cookie: String) -> Result<(), PolkitError> {
        self.events
            .send(Request::Cancel(cookie))
            .map_err(|_| PolkitError::Failed("Polkit agent stopped".into()))
    }
}

fn session_id(connection: &zbus::blocking::Connection) -> zbus::Result<String> {
    if let Some(id) = std::env::var("XDG_SESSION_ID").ok().filter(|s| !s.trim().is_empty()) {
        return Ok(id);
    }
    let manager = zbus::blocking::Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let path: OwnedObjectPath = manager.call("GetSessionByPID", &(std::process::id(),))?;
    let session = zbus::blocking::Proxy::new(connection, "org.freedesktop.login1", path, "org.freedesktop.login1.Session")?;
    session.get_property::<String>("Id")
}

/// Where the agent connects and which session it serves. Tests use a private
/// bus and a fixed session id.
#[derive(Default, Clone)]
pub struct Target {
    pub address: Option<String>,
    pub session: Option<String>,
    /// Serve only this process (like `pkttyagent --process`), to test against
    /// a real polkitd while the session already has an agent.
    pub process: Option<u32>,
}

/// Field 22 of /proc/<pid>/stat, which polkit uses to identify a process.
pub(super) fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// Starts the worker. Dropping the returned sender unregisters the agent and
/// ends the thread.
pub(super) fn spawn(events: channel::Sender<Request>, target: Target) -> Option<mpsc::Sender<()>> {
    let (stop, stopped) = mpsc::channel::<()>();
    let spawned = std::thread::Builder::new().name("polkit-agent".into()).spawn(move || {
        let result = (|| -> zbus::Result<()> {
            let builder = match &target.address {
                Some(address) => zbus::blocking::connection::Builder::address(address.as_str())?,
                None => zbus::blocking::connection::Builder::system()?,
            };
            // SAFETY: getuid has no preconditions.
            let agent = Agent { events, own_uid: unsafe { libc::getuid() } };
            let connection = builder.serve_at(PATH, agent)?.build()?;
            let (kind, properties, session) = match (target.process, target.session) {
                (Some(pid), _) => {
                    let start = process_start_time(pid).ok_or_else(|| zbus::Error::Failure(format!("no process {pid}")))?;
                    ("unix-process", HashMap::from([("pid", Value::from(pid)), ("start-time", Value::from(start))]), format!("process {pid}"))
                }
                (None, Some(session)) => ("unix-session", HashMap::from([("session-id", Value::from(session.clone()))]), session),
                (None, None) => {
                    let session = session_id(&connection)?;
                    ("unix-session", HashMap::from([("session-id", Value::from(session.clone()))]), session)
                }
            };
            let subject = (kind, properties);
            let locale = std::env::var("LANG").unwrap_or_else(|_| "C".into());
            // polkitd declares the agent path as a string, not an object path.
            connection.call_method(Some(NAME), AUTHORITY, Some(AUTHORITY_INTERFACE), "RegisterAuthenticationAgent", &(&subject, locale.as_str(), PATH))?;
            tracing::info!(%session, "built-in polkit agent registered");
            let _ = stopped.recv();
            let _ = connection.call_method(Some(NAME), AUTHORITY, Some(AUTHORITY_INTERFACE), "UnregisterAuthenticationAgent", &(&subject, PATH));
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "built-in polkit agent inactive (no polkitd, or another agent owns this session)");
        }
    });
    match spawned {
        Ok(_) => Some(stop),
        Err(error) => {
            tracing::warn!(%error, "cannot start the polkit agent thread");
            None
        }
    }
}
