//! Blocking D-Bus workers sleep on signals/commands, never on polling timers.
use crate::Gavno;
use smithay::reexports::calloop::{channel, LoopHandle};
use std::sync::mpsc::{self, Sender};
use zbus::{
    blocking::{Connection, MessageIterator, Proxy},
    zvariant::{OwnedFd, OwnedObjectPath},
};

const DEST: &str = "org.freedesktop.login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const PATH: &str = "/org/freedesktop/login1";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signal {
    Lock,
    Unlock,
    Sleep(bool),
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Hint(bool),
    Secured,
    Resume,
    Power(crate::greeter::Hit),
}

pub(super) fn spawn(handle: &LoopHandle<'static, Gavno>) -> Option<Sender<Command>> {
    let (events, source) = channel::channel();
    if let Err(error) = handle.insert_source(source, |event, _, gv| {
        if let channel::Event::Msg(signal) = event {
            gv.logind_signal(signal);
        }
    }) {
        tracing::error!(%error, "cannot register logind channel");
        return None;
    }
    let (commands, receive) = mpsc::channel();
    let worker = std::thread::Builder::new().name("lock-logind".into()).spawn(move || {
        let result = (|| -> zbus::Result<()> {
            let connection = Connection::system()?;
            let manager = Proxy::new(&connection, DEST, PATH, MANAGER)?;
            let session_path: OwnedObjectPath = manager.call("GetSessionByPID", &(std::process::id(),))?;
            // The bus verifies the well-known sender; paths/interfaces are checked below.
            let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(DEST)?.build();
            let signals = MessageIterator::for_match_rule(rule, &connection, Some(64))?;
            let command_connection = connection.clone();
            let command_path = session_path.clone();
            // Acquire before consuming any queued sleep signal.
            let inhibitor = match inhibit(&manager) {
                Ok(fd) => Some(fd),
                Err(error) => { tracing::error!(%error, "cannot acquire sleep delay inhibitor"); None }
            };
            std::thread::Builder::new().name("lock-logind-control".into()).spawn(move || {
                let result = (|| -> zbus::Result<()> {
                    let manager = Proxy::new(&command_connection, DEST, PATH, MANAGER)?;
                    let session = Proxy::new(&command_connection, DEST, command_path.as_str(), SESSION)?;
                    let mut inhibitor = inhibitor;
                    for command in receive {
                        match command {
                            Command::Hint(locked) => {
                                if let Err(error) = session.call::<_, _, ()>("SetLockedHint", &(locked,)) {
                                    tracing::warn!(%error, "cannot set logind LockedHint");
                                }
                            }
                            Command::Power(hit) => {
                                let connection = command_connection.clone();
                                // Suspend can wait for the delay inhibitor. Keep its
                                // method call off the thread that releases that inhibitor.
                                if let Err(error) = std::thread::Builder::new().name("lock-power".into()).spawn(move || {
                                    let method = match hit {
                                        crate::greeter::Hit::Suspend => "Suspend",
                                        crate::greeter::Hit::Restart => "Reboot",
                                        crate::greeter::Hit::PowerOff => "PowerOff",
                                        _ => return,
                                    };
                                    let result = Proxy::new(&connection, DEST, PATH, MANAGER)
                                        .and_then(|manager| manager.call::<_, _, ()>(method, &(true,)));
                                    if let Err(error) = result { tracing::warn!(%error, "logind power request failed"); }
                                }) { tracing::error!(%error, "cannot start power worker"); }
                            }
                            Command::Secured => { inhibitor.take(); }
                            Command::Resume => {
                                if inhibitor.is_none() {
                                    match inhibit(&manager) {
                                        Ok(fd) => inhibitor = Some(fd),
                                        Err(error) => tracing::error!(%error, "cannot reacquire sleep inhibitor"),
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result { tracing::error!(%error, "logind control stopped"); }
            }).map_err(|e| zbus::Error::Failure(e.to_string()))?;
            for message in signals {
                let message = message?;
                let header = message.header();
                let path = header.path().map(|v| v.as_str());
                let interface = header.interface().map(|v| v.as_str());
                let member = header.member().map(|v| v.as_str());
                let signal = match (path, interface, member) {
                    (Some(p), Some(SESSION), Some("Lock")) if p == session_path.as_str() => Some(Signal::Lock),
                    (Some(p), Some(SESSION), Some("Unlock")) if p == session_path.as_str() => Some(Signal::Unlock),
                    (Some(PATH), Some(MANAGER), Some("PrepareForSleep")) => {
                        message.body().deserialize::<bool>().ok().map(Signal::Sleep)
                    }
                    _ => None,
                };
                if let Some(signal) = signal { if events.send(signal).is_err() { break; } }
            }
            Ok(())
        })();
        if let Err(error) = result { tracing::error!(%error, "logind lock integration unavailable"); }
    });
    if let Err(error) = worker {
        tracing::error!(%error, "cannot start logind worker");
        return None;
    }
    Some(commands)
}
fn inhibit(manager: &Proxy<'_>) -> zbus::Result<OwnedFd> {
    manager.call(
        "Inhibit",
        &(
            "sleep",
            "GAVNO",
            "Present the lock screen before sleeping",
            "delay",
        ),
    )
}
