//! Small pipewire-pulse client. Queries and writes run on a worker; no audio is played.
use serde_json::Value;
use std::process::{Command, Stdio};
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub volume: u8,
    pub muted: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub output: Option<Endpoint>,
    pub input: Option<Endpoint>,
    pub sinks: Vec<Endpoint>,
    pub streams: Vec<Endpoint>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Output,
    Input,
    Stream(String),
}
fn field(v: &Value, key: &str) -> String {
    v[key]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect()
}
pub fn parse_list(text: &str, streams: bool) -> Result<Vec<Endpoint>, String> {
    let list: Value =
        serde_json::from_str(text).map_err(|_| "Invalid audio response".to_string())?;
    let list = list.as_array().ok_or("Invalid audio list")?;
    Ok(list
        .iter()
        .filter_map(|v| {
            let index = v["index"].as_u64()?;
            let props = &v["properties"];
            let name = if streams {
                let app = field(props, "application.name");
                if app.is_empty() {
                    field(props, "media.name")
                } else {
                    app
                }
            } else {
                field(v, "description")
            };
            let levels: Vec<_> = v["volume"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|c| c["value"].as_u64())
                .map(|v| v.min(65536))
                .collect();
            let level = if levels.is_empty() {
                0
            } else {
                (levels.iter().sum::<u64>() / levels.len() as u64 * 100 / 65536).min(100) as u8
            };
            Some(Endpoint {
                id: if streams {
                    index.to_string()
                } else {
                    field(v, "name")
                },
                name: if name.is_empty() {
                    format!("Audio {index}")
                } else {
                    name
                },
                icon: field(props, "application.icon_name"),
                volume: level,
                muted: v["mute"].as_bool().unwrap_or(false),
            })
        })
        .collect())
}
pub trait Provider: Send {
    fn snapshot(&mut self) -> Result<Snapshot, String>;
    fn opened(&mut self) {}
    fn volume(&mut self, target: &Target, value: u8) -> Result<(), String>;
    fn mute(&mut self, target: &Target) -> Result<(), String>;
    fn output(&mut self, id: &str) -> Result<(), String>;
}
pub struct Pactl {
    subscription: Subscription,
}
enum SubscriptionCommand {
    Start,
    Stop,
}
struct Subscription(std::sync::mpsc::Sender<SubscriptionCommand>);
impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.0.send(SubscriptionCommand::Stop);
    }
}
impl Pactl {
    pub fn new(events: std::sync::mpsc::Sender<super::worker::Command>) -> Self {
        use std::{
            io::{BufRead, BufReader},
            sync::mpsc,
        };
        let (send, receive) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("panel-pulse-subscription".into())
            .spawn(move || {
                let mut child: Option<std::process::Child> = None;
                while let Ok(command) = receive.recv() {
                    match command {
                        SubscriptionCommand::Start => {
                            if child
                                .as_mut()
                                .is_some_and(|c| c.try_wait().is_ok_and(|status| status.is_none()))
                            {
                                continue;
                            }
                            if let Some(mut old) = child.take() {
                                let _ = old.kill();
                                let _ = old.wait();
                            }
                            match Command::new("pactl")
                                .arg("subscribe")
                                .env("LC_ALL", "C")
                                .stdin(Stdio::null())
                                .stdout(Stdio::piped())
                                .stderr(Stdio::null())
                                .spawn()
                            {
                                Ok(mut process) => {
                                    if let Some(stdout) = process.stdout.take() {
                                        let events = events.clone();
                                        let _ = std::thread::Builder::new()
                                            .name("panel-pulse-events".into())
                                            .spawn(move || {
                                                for line in BufReader::new(stdout).lines() {
                                                    let Ok(line) = line else {
                                                        break;
                                                    };
                                                    if [
                                                        " on sink ",
                                                        " on source ",
                                                        " on sink-input ",
                                                        " on server ",
                                                        " on card ",
                                                    ]
                                                    .iter()
                                                    .any(|s| line.contains(s))
                                                        && events
                                                            .send(super::worker::Command::Refresh)
                                                            .is_err()
                                                    {
                                                        return;
                                                    }
                                                }
                                                let _ = events.send(
                                                    super::worker::Command::AudioDisconnected,
                                                );
                                            });
                                    }
                                    child = Some(process);
                                }
                                Err(_) => {
                                    let _ = events.send(super::worker::Command::AudioDisconnected);
                                }
                            }
                        }
                        SubscriptionCommand::Stop => break,
                    }
                }
                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            });
        let subscription = Subscription(send);
        let _ = subscription.0.send(SubscriptionCommand::Start);
        Self { subscription }
    }
}
fn query(args: &[&str]) -> Result<String, String> {
    let result = Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "pactl is unavailable".to_string())?;
    if !result.status.success() {
        return Err("Audio server unavailable or operation denied".into());
    }
    String::from_utf8(result.stdout).map_err(|_| "Invalid audio response".into())
}
fn object(target: &Target) -> (&str, &str) {
    match target {
        Target::Output => ("sink", "@DEFAULT_SINK@"),
        Target::Input => ("source", "@DEFAULT_SOURCE@"),
        Target::Stream(id) => ("sink-input", id),
    }
}
impl Provider for Pactl {
    fn opened(&mut self) {
        let _ = self.subscription.0.send(SubscriptionCommand::Start);
    }
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        let info: Value = serde_json::from_str(&query(&["--format=json", "info"])?)
            .map_err(|_| "Invalid audio server info".to_string())?;
        let sinks = parse_list(&query(&["--format=json", "list", "sinks"])?, false)?;
        let sources = parse_list(&query(&["--format=json", "list", "sources"])?, false)?;
        let streams = parse_list(&query(&["--format=json", "list", "sink-inputs"])?, true)?;
        Ok(Snapshot {
            output: sinks
                .iter()
                .find(|s| s.id == field(&info, "default_sink_name"))
                .cloned(),
            input: sources
                .into_iter()
                .find(|s| s.id == field(&info, "default_source_name")),
            sinks,
            streams,
        })
    }
    fn volume(&mut self, target: &Target, value: u8) -> Result<(), String> {
        let (kind, id) = object(target);
        query(&[
            &format!("set-{kind}-volume"),
            id,
            &format!("{}%", value.min(100)),
        ])
        .map(|_| ())
    }
    fn mute(&mut self, target: &Target) -> Result<(), String> {
        let (kind, id) = object(target);
        query(&[&format!("set-{kind}-mute"), id, "toggle"]).map(|_| ())
    }
    fn output(&mut self, id: &str) -> Result<(), String> {
        query(&["set-default-sink", id])?;
        for stream in parse_list(&query(&["--format=json", "list", "sink-inputs"])?, true)? {
            query(&["move-sink-input", &stream.id, id])?;
        }
        Ok(())
    }
}
