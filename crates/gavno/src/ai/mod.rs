//! Local AI in the launcher (`design/ai.md`). The compositor only builds a
//! small context, sends it to the `gavno-ai` helper over D-Bus from a worker
//! thread, and shows the validated answer as a preview row. The model never
//! runs without the user pressing Enter on the "Ask AI" row, and the
//! compositor thread never waits for it.
pub(crate) mod names;

use crate::{desktop_entry::shell_quote, ipc::window_id, Gavno};
use gavno_ai::action::{
    Action, AppInfo, Context, Interpretation, MusicCommand, MusicInfo, SetKey, SetValue, WindowInfo,
    WorkspaceInfo,
};
use smithay::{
    desktop::Window,
    reexports::calloop::{channel, RegistrationToken},
    utils::IsAlive,
};
use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Apps sent with each request.
const MAX_APPS: usize = 60;
const MAX_WINDOWS: usize = 30;

/// What the launcher's AI row shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Idle,
    /// Waiting for the model; the number identifies the request.
    Asking(u64),
    /// A validated answer waits for the second Enter.
    Preview(Interpretation),
    Error(String),
}

/// Requests for the D-Bus worker thread.
#[derive(Debug, PartialEq)]
pub enum Request {
    /// `context` is the JSON of an `Interpret` request, to warm the prompt prefix.
    Prewarm { context: String },
    Interpret { seq: u64, query: String, context: String },
    Name { output: String, ws: u64, sig: u64, windows: String },
}

pub enum Reply {
    Interpreted { seq: u64, result: Result<Interpretation, String> },
    Named { output: String, ws: u64, sig: u64, result: Result<String, String> },
}

#[derive(Default)]
pub struct Ai {
    /// Present once the first request started the worker thread.
    pub(crate) worker: Option<mpsc::Sender<Request>>,
    reply_source: Option<RegistrationToken>,
    seq: u64,
    /// The last time the model answered; the helper stops it after the idle timeout.
    last_used: Option<Instant>,
    pub(super) stable: HashMap<(String, u64), (u64, Instant)>,
    pub(super) requested: HashSet<((String, u64), u64)>,
    pub(super) refine_timer: Option<RegistrationToken>,
    #[cfg(test)]
    pub(crate) executed: Vec<Action>,
}

impl Ai {
    /// Whether the model probably still runs, so refinement may use it.
    pub(crate) fn model_recent(&self, idle_minutes: u32) -> bool {
        self.last_used.is_some_and(|t| t.elapsed() < Duration::from_secs(idle_minutes as u64 * 60))
    }
}

/// Whether a launcher query should offer the "Ask AI" row: at least two words
/// and no strong match among the applications.
pub(crate) fn is_candidate(query: &str, best_score: Option<u8>) -> bool {
    query.split_whitespace().count() >= 2 && best_score.is_none_or(|s| s >= 2)
}

/// Blocking D-Bus client; lives on its own thread.
struct Client {
    proxy: Option<zbus::blocking::Proxy<'static>>,
}

impl Client {
    /// The proxy is rebuilt after a failure (the helper may have exited and restarts on demand).
    fn proxy(&mut self) -> Result<&zbus::blocking::Proxy<'static>, String> {
        if self.proxy.is_none() {
            let bus = zbus::blocking::Connection::session().map_err(|e| format!("no session bus: {e}"))?;
            self.proxy = Some(
                zbus::blocking::Proxy::new(&bus, "org.gavno.AI1", "/org/gavno/AI1", "org.gavno.AI1")
                    .map_err(|e| format!("AI helper unavailable: {e}"))?,
            );
        }
        Ok(self.proxy.as_ref().unwrap())
    }

    fn call<B: serde::Serialize + zbus::zvariant::DynamicType>(&mut self, method: &str, args: &B) -> Result<String, String> {
        let result = self.proxy()?.call::<_, _, String>(method, args);
        result.map_err(|e| {
            self.proxy = None;
            format!("AI helper unavailable: {e}")
        })
    }

    /// A call whose reply is empty.
    fn notify<B: serde::Serialize + zbus::zvariant::DynamicType>(&mut self, method: &str, args: &B) -> Result<(), String> {
        let result = self.proxy()?.call::<_, _, ()>(method, args);
        result.map_err(|e| {
            self.proxy = None;
            format!("AI helper unavailable: {e}")
        })
    }
}

fn parse(text: Result<String, String>) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(&text?).map_err(|e| format!("bad AI answer: {e}"))?;
    match value.get("error").and_then(|e| e.as_str()) {
        Some(error) => Err(error.to_owned()),
        None => Ok(value),
    }
}

fn worker_main(requests: mpsc::Receiver<Request>, replies: channel::Sender<Reply>) {
    let mut client = Client { proxy: None };
    while let Ok(request) = requests.recv() {
        match request {
            Request::Prewarm { context } => {
                let _ = client.notify("Prewarm", &(context,));
            }
            Request::Interpret { seq, query, context } => {
                let result = parse(client.call("Interpret", &(query, context)))
                    .and_then(|v| serde_json::from_value::<Interpretation>(v).map_err(|e| format!("bad AI answer: {e}")));
                if replies.send(Reply::Interpreted { seq, result }).is_err() {
                    break;
                }
            }
            Request::Name { output, ws, sig, windows } => {
                let result = parse(client.call("NameWorkspace", &(windows,)))
                    .and_then(|v| v["name"].as_str().map(str::to_owned).ok_or_else(|| "no name".to_owned()));
                if replies.send(Reply::Named { output, ws, sig, result }).is_err() {
                    break;
                }
            }
        }
    }
}

impl Gavno {
    /// Sends `request` to the worker, starting it on first use.
    pub(crate) fn ai_send(&mut self, request: Request) -> bool {
        if self.ai.worker.is_none() {
            let (reply_tx, reply_rx) = channel::channel::<Reply>();
            let source = self.loop_handle.insert_source(reply_rx, |event, _, gv| {
                let _stall = crate::perf::Stall::new("ai.reply");
                if let channel::Event::Msg(reply) = event {
                    gv.guard(crate::module::Feature::Launcher, (), |gv| gv.ai_reply(reply));
                }
            });
            let Ok(source) = source else { return false };
            let (tx, rx) = mpsc::channel();
            if std::thread::Builder::new()
                .name("gavno-ai-client".into())
                .spawn(move || worker_main(rx, reply_tx))
                .is_err()
            {
                self.loop_handle.remove(source);
                return false;
            }
            self.ai.reply_source = Some(source);
            self.ai.worker = Some(tx);
        }
        self.ai.worker.as_ref().is_some_and(|w| w.send(request).is_ok())
    }

    /// Stops the worker and forgets pending work (AI turned off or crashed state reset).
    pub(crate) fn ai_config_changed(&mut self) {
        self.launcher.ai_enabled = self.config.ai.enabled;
        if self.config.ai.enabled {
            return;
        }
        self.ai.worker = None;
        if let Some(token) = self.ai.reply_source.take() {
            self.loop_handle.remove(token);
        }
        if let Some(token) = self.ai.refine_timer.take() {
            self.loop_handle.remove(token);
        }
        self.ai.last_used = None;
        self.launcher.ai_phase = Phase::Idle;
        self.launcher.refresh_ai_row();
    }

    /// The first keystroke that makes a query a command candidate starts the model.
    pub(crate) fn ai_after_query_change(&mut self) {
        if self.launcher.ask_visible() && !self.launcher.prewarm_sent {
            self.launcher.prewarm_sent = true;
            let context = serde_json::to_string(&self.ai_context()).unwrap_or_default();
            self.ai_send(Request::Prewarm { context });
        }
    }

    /// Enter on the "Ask AI" row: ask, run the previewed actions, or retry.
    pub(crate) fn ai_enter(&mut self) {
        match self.launcher.ai_phase.clone() {
            Phase::Asking(_) => {}
            Phase::Preview(interpretation) => {
                self.launcher.dismiss();
                self.ai_execute(interpretation.actions);
                self.queue_redraw_all();
            }
            Phase::Idle | Phase::Error(_) => self.ai_ask(),
        }
    }

    fn ai_ask(&mut self) {
        let query = self.launcher.begin_ask();
        if query.is_empty() {
            return;
        }
        self.ai.seq += 1;
        let seq = self.ai.seq;
        let context = serde_json::to_string(&self.ai_context()).unwrap_or_default();
        self.launcher.ai_phase = Phase::Asking(seq);
        if !self.ai_send(Request::Interpret { seq, query, context }) {
            self.launcher.ai_phase = Phase::Error("The AI helper could not be started".into());
        }
        self.queue_redraw_all();
    }

    pub(crate) fn ai_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Interpreted { seq, result } => {
                if self.launcher.ai_phase != Phase::Asking(seq) || !self.launcher.open {
                    return;
                }
                self.launcher.ai_phase = match result {
                    Ok(interpretation) => {
                        self.ai.last_used = Some(Instant::now());
                        Phase::Preview(interpretation)
                    }
                    Err(error) => Phase::Error(error),
                };
                self.queue_redraw_all();
            }
            Reply::Named { output, ws, sig, result } => match result {
                Ok(name) => {
                    self.ai.last_used = Some(Instant::now());
                    self.ai_named(&output, ws, sig, name);
                }
                Err(error) => {
                    if error.contains("not running") {
                        self.ai.last_used = None;
                    }
                }
            },
        }
    }

    /// What the model may refer to: the active output's windows and
    /// workspaces, installed apps and the music state. Titles are cut short.
    fn ai_context(&mut self) -> Context {
        let mut ctx = Context { home: std::env::var("HOME").unwrap_or_default(), ..Default::default() };
        if let Some(owner) = self.active_output().and_then(|o| self.workspaces.get(&o)) {
            for (i, ws) in owner.list.items.iter().enumerate().take(10) {
                let index = i as u32 + 1;
                ctx.workspaces.push(WorkspaceInfo { index, name: ws.name.clone().unwrap_or_default() });
                for w in &ws.windows {
                    let Some(id) = window_id(&w.window) else { continue };
                    if ctx.windows.len() >= MAX_WINDOWS {
                        break;
                    }
                    let (title, app_id) = crate::overlay::window_labels(&w.window);
                    ctx.windows.push(WindowInfo { id, app_id, title: title.chars().take(60).collect(), workspace: index });
                }
            }
        }
        if ctx.workspaces.is_empty() {
            ctx.workspaces.push(WorkspaceInfo { index: 1, name: String::new() });
        }
        ctx.apps = self
            .launcher
            .app_choices(&self.config.dock.pinned, MAX_APPS)
            .into_iter()
            .map(|(id, name)| AppInfo { id, name })
            .collect();
        let music = self.now_playing.borrow().music_now().0;
        ctx.music = music.map(|p| MusicInfo {
            state: format!("{:?}", p.playback).to_lowercase(),
            title: p.label().chars().take(60).collect(),
        });
        ctx
    }

    fn ai_window(&self, id: u64) -> Option<Window> {
        self.workspaces
            .all()
            .map(|o| &o.window)
            .find(|w| w.alive() && window_id(w) == Some(id))
            .cloned()
    }

    /// Runs validated actions through the same functions the keybinds use.
    pub(crate) fn ai_execute(&mut self, actions: Vec<Action>) {
        for action in actions {
            #[cfg(test)]
            self.ai.executed.push(action.clone());
            self.ai_run(action);
        }
    }

    fn ai_run(&mut self, action: Action) {
        match action {
            Action::Launch { desktop_id } => {
                if let Some(command) = self.launcher.launch_command(&desktop_id, &self.config.terminal) {
                    self.launch_shell_at(Instant::now(), &command, Some(&desktop_id));
                }
            }
            Action::Focus { window } => {
                if let Some(window) = self.ai_window(window) {
                    self.focus_window(Some(&window));
                }
            }
            Action::MoveToWorkspace { window, workspace } => {
                if let Some(window) = self.ai_window(window) {
                    self.move_to_workspace(&window, workspace.saturating_sub(1) as usize);
                }
            }
            Action::SwitchWorkspace { workspace } => {
                if let Some(output) = self.active_output() {
                    self.switch_workspace(&output, workspace.saturating_sub(1) as usize);
                }
            }
            Action::NameWorkspace { workspace, name } => {
                if let Some(output) = self.active_output() {
                    let index = workspace.saturating_sub(1) as usize;
                    if let Some(ws) = self.workspaces.ensure(&output).list.items.get_mut(index) {
                        ws.name = Some(name);
                        ws.user_named = true;
                    }
                    self.refresh_shell();
                }
            }
            Action::Music { command, query } => self.ai_music(command, query),
            Action::OpenFolder { path } => {
                let command = format!("{} {}", self.config.file_manager, shell_quote(&path));
                self.launch_shell_at(Instant::now(), &command, None);
            }
            Action::Set { key, value } => match (key, value) {
                (SetKey::Brightness, SetValue::Number(percent)) => {
                    self.panel_set_brightness((percent / 100.0).clamp(0.05, 1.0));
                }
                (SetKey::NightLight, SetValue::Bool(on)) => {
                    self.config.display.night_light = on;
                    self.refresh_theme_clock();
                    self.queue_redraw_all();
                }
                (SetKey::Dnd, SetValue::Bool(on)) => {
                    self.guard(crate::module::Feature::Notifications, (), |gv| {
                        gv.notifications.borrow_mut().dnd = on;
                        gv.refresh_notifications();
                    });
                }
                _ => {}
            },
        }
    }

    fn ai_music(&mut self, command: MusicCommand, query: Option<String>) {
        let playing = self
            .now_playing
            .borrow()
            .music_now()
            .0
            .is_some_and(|p| p.playback == crate::media::Playback::Playing);
        match (command, query) {
            (MusicCommand::Play, Some(query)) => {
                if let Some(output) = self.active_output() {
                    self.music_search_for(&output, &query);
                }
            }
            (MusicCommand::Play, None) if !playing => self.music_command(crate::media::Command::PlayPause),
            (MusicCommand::Pause, _) if playing => self.music_command(crate::media::Command::PlayPause),
            (MusicCommand::Next, _) => self.music_command(crate::media::Command::Next),
            (MusicCommand::Previous, _) => self.music_command(crate::media::Command::Previous),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
