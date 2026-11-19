//! Small pipewire-pulse client. Queries and writes run on a worker; no audio is played.
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn child_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("LC_ALL", "C").stdin(Stdio::null());
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe system calls run between fork and exec.
    // Kernel cleanup must work even when process::exit skips destructors or
    // the audio worker cannot service Shutdown because a query is blocked.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The compositor may have exited before the signal was installed.
            if libc::getppid() != parent {
                libc::_exit(127);
            }
            Ok(())
        });
    }
    command
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub volume: u8,
    pub muted: bool,
    /// Identifiers a desktop entry may be known by (stream endpoints only).
    pub app_ids: Vec<String>,
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
/// Symbolic icon of an output or input device from its pulse properties.
fn device_icon(props: &Value, name: &str, source: bool) -> String {
    let form = field(props, "device.form_factor");
    let bus = field(props, "device.bus");
    let hint = field(props, "device.icon_name");
    let name = name.to_ascii_lowercase();
    let bluetooth = bus == "bluetooth" || name.starts_with("bluez_");
    if source {
        return match form.as_str() {
            "headset" | "headphone" | "hands-free" if bluetooth => "bluetooth-symbolic",
            _ => "audio-input-microphone-symbolic",
        }
        .into();
    }
    match form.as_str() {
        "headphone" | "headset" | "hands-free" | "hifi" if !hint.contains("speaker") => {
            "audio-headphones-symbolic"
        }
        "speaker" | "computer" | "tv" | "portable" => "audio-speakers-symbolic",
        _ if bluetooth => "bluetooth-symbolic",
        _ if name.contains("hdmi") || name.contains("displayport") || hint.contains("video-display") => {
            "video-display-symbolic"
        }
        _ if hint.contains("headphone") || hint.contains("headset") => "audio-headphones-symbolic",
        _ => "audio-speakers-symbolic",
    }
    .into()
}
/// Resolves missing stream icons through the desktop entries. Runs on the
/// audio worker; reading the entries never touches the compositor thread.
pub(super) fn resolve_icons(
    entries: &mut crate::desktop_entry::DesktopEntries,
    streams: &mut [Endpoint],
) {
    for stream in streams {
        if !stream.icon.is_empty() {
            continue;
        }
        let entry = stream
            .app_ids
            .iter()
            .find_map(|id| entries.resolve(id).filter(|e| !e.icon.is_empty()));
        stream.icon = match entry {
            Some(entry) => entry.icon,
            // Many themes ship an icon named after the binary.
            None => stream
                .app_ids
                .first()
                .map(|id| id.to_ascii_lowercase())
                .unwrap_or_default(),
        };
    }
}
/// pactl's JSON writer cannot encode non-ASCII text and prints "(null)" for it,
/// which hits every localized PipeWire description ("Встроенное аудио ...").
fn readable(text: String) -> String {
    if text == "(null)" { String::new() } else { text }
}

/// A device label that survives pactl's JSON: the description, else the ASCII
/// nick or profile ("Speaker playback" -> "Speaker"), else the node name.
fn device_name(v: &Value, props: &Value) -> String {
    let description = readable(field(v, "description"));
    if !description.is_empty() {
        return description;
    }
    for key in ["node.nick", "device.profile.description"] {
        let text = readable(field(props, key));
        let text = text.trim_end_matches(" playback").trim_end_matches(" capture").trim();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    let node = field(v, "name");
    let part = node.split("__").nth(1).unwrap_or(&node);
    match part.strip_prefix("HDMI") {
        Some(n) if !n.is_empty() => format!("HDMI {n}"),
        _ => part.to_string(),
    }
}

pub fn parse_list(text: &str, streams: bool) -> Result<Vec<Endpoint>, String> {
    parse_devices(text, streams, false)
}
pub fn parse_devices(text: &str, streams: bool, source: bool) -> Result<Vec<Endpoint>, String> {
    let list: Value =
        serde_json::from_str(text).map_err(|_| "Invalid audio response".to_string())?;
    let list = list.as_array().ok_or("Invalid audio list")?;
    Ok(list
        .iter()
        .filter_map(|v| {
            let index = v["index"].as_u64()?;
            let props = &v["properties"];
            let name = if streams {
                ["application.name", "media.name", "application.process.binary"]
                    .iter()
                    .map(|key| readable(field(props, key)))
                    .find(|n| !n.is_empty())
                    .unwrap_or_default()
            } else {
                device_name(v, props)
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
            let icon = if streams {
                field(props, "application.icon_name")
            } else {
                device_icon(props, &field(v, "name"), source)
            };
            let app_ids = if streams {
                [
                    "pipewire.access.portal.app_id",
                    "application.id",
                    "application.process.binary",
                    "application.name",
                ]
                .iter()
                .map(|key| field(props, key))
                .filter(|id| !id.is_empty())
                .collect()
            } else {
                Vec::new()
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
                icon,
                volume: level,
                muted: v["mute"].as_bool().unwrap_or(false),
                app_ids,
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
    entries: crate::desktop_entry::DesktopEntries,
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
                            match child_command("pactl")
                                .arg("subscribe")
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
        Self { subscription, entries: Default::default() }
    }
}
fn query(args: &[&str]) -> Result<String, String> {
    let result = child_command("pactl")
        .args(args)
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
        let sources = parse_devices(&query(&["--format=json", "list", "sources"])?, false, true)?;
        let mut streams = parse_list(&query(&["--format=json", "list", "sink-inputs"])?, true)?;
        resolve_icons(&mut self.entries, &mut streams);
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

#[cfg(test)]
mod null_name_tests {
    use super::*;
    #[test]
    fn localized_descriptions_fall_back_to_ascii_names() {
        let text = r#"[{"index":67,"name":"alsa_output.platform-sound.HiFi__HDMI2__sink","description":"(null)","properties":{"node.nick":"HDMI playback"},"volume":{}},
            {"index":68,"name":"alsa_output.platform-sound.HiFi__Speaker__sink","description":"(null)","properties":{},"volume":{}},
            {"index":69,"name":"alsa_output.x.HiFi__HDMI2__sink","description":"(null)","properties":{},"volume":{}}]"#;
        let names: Vec<_> = parse_devices(text, false, false).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["HDMI", "Speaker", "HDMI 2"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::process::ExitStatusExt, time::{Duration, Instant}};

    #[test]
    fn children_die_when_compositor_exits_during_query() {
        const TEST: &str = "panel_controls::audio::tests::children_die_when_compositor_exits_during_query";
        const MODE: &str = "GAVNO_AUDIO_PARENT_DEATH_TEST";
        match std::env::var(MODE).as_deref() {
            Ok("parent") => {
                let (tx, rx) = std::sync::mpsc::channel();
                // Fake subscription and query run on separate worker threads.
                // Both stay blocked; neither can handle a shutdown command.
                for _ in 0..2 {
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let mut child = child_command("/bin/sleep")
                            .arg("60")
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn().unwrap();
                        tx.send(child.id()).unwrap();
                        let _ = child.wait();
                    });
                }
                let a = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let b = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                println!("FAKE_AUDIO_CHILDREN={a},{b}");
                std::io::stdout().flush().unwrap();
                // Match the greeter handoff: skip every destructor.
                std::process::exit(0);
            }
            Ok("monitor") => {
                // Isolate subreaper state from the parallel test runner. This
                // lets the monitor reap the orphaned fake children and inspect
                // their actual exit signals instead of polling kill(pid, 0).
                // SAFETY: prctl affects only this isolated test process.
                assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
                let parent = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(MODE, "parent")
                    .output().unwrap();
                assert!(parent.status.success(), "{}", String::from_utf8_lossy(&parent.stderr));
                let stdout = String::from_utf8(parent.stdout).unwrap();
                let children = stdout.lines()
                    .find_map(|line| line.split_once("FAKE_AUDIO_CHILDREN=").map(|(_, ids)| ids))
                    .expect("fake compositor reported both child PIDs");
                struct Reap(Vec<libc::pid_t>);
                impl Drop for Reap {
                    fn drop(&mut self) {
                        for &pid in &self.0 {
                            // SAFETY: these are this monitor's unreaped children.
                            unsafe {
                                libc::kill(pid, libc::SIGKILL);
                                libc::waitpid(pid, std::ptr::null_mut(), 0);
                            }
                        }
                    }
                }
                let mut pending = Reap(children.split(',').map(|pid| pid.parse().unwrap()).collect());
                assert_eq!(pending.0.len(), 2);
                let deadline = Instant::now() + Duration::from_secs(5);
                while !pending.0.is_empty() && Instant::now() < deadline {
                    for pid in pending.0.clone() {
                        let mut status = 0;
                        // SAFETY: status is writable and pid is an adopted child.
                        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                        assert!(result >= 0, "waitpid: {}", std::io::Error::last_os_error());
                        if result == pid {
                            pending.0.retain(|p| *p != pid);
                            assert_eq!(std::process::ExitStatus::from_raw(status).signal(), Some(libc::SIGKILL));
                        }
                    }
                    if !pending.0.is_empty() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                assert!(pending.0.is_empty(), "fake audio children survived compositor exit");
            }
            _ => {
                let monitor = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(MODE, "monitor")
                    .output().unwrap();
                assert!(monitor.status.success(), "{}\n{}",
                    String::from_utf8_lossy(&monitor.stdout), String::from_utf8_lossy(&monitor.stderr));
            }
        }
    }
}
