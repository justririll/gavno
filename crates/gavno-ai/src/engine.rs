//! The service logic without D-Bus: unit control, the model call, validation
//! and the idle clock. `UnitControl` and the server address are injected, so
//! tests run against a fake llama-server and a fake unit.
use crate::{
    action::{self, Context},
    http, prompt,
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

/// Starts and stops the model server. Production: systemd over D-Bus.
pub trait UnitControl: Send + Sync {
    fn start(&self) -> Result<(), String>;
    fn stop(&self) -> Result<(), String>;
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub addr: SocketAddr,
    pub idle: Duration,
    /// How long a request waits for a cold model to answer `/health`.
    pub ready_timeout: Duration,
    pub request_timeout: Duration,
    pub max_tokens: u32,
}

impl Settings {
    pub fn new(port: u16, idle: Duration) -> Self {
        Self {
            addr: ([127, 0, 0, 1], port).into(),
            idle,
            ready_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            max_tokens: 128,
        }
    }
}

struct State {
    last_activity: Instant,
    in_flight: u32,
    last_latency_ms: u64,
    closing: bool,
}

pub struct Engine {
    unit: Box<dyn UnitControl>,
    settings: Settings,
    state: Mutex<State>,
    wake: Condvar,
}

/// Counts a request as in flight, so the idle stop never interrupts it.
struct Flight<'a>(&'a Engine, bool);

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap();
        s.in_flight -= 1;
        if self.1 {
            s.last_activity = Instant::now();
        }
        self.0.wake.notify_all();
    }
}

impl Engine {
    pub fn new(unit: Box<dyn UnitControl>, settings: Settings) -> Self {
        Self {
            unit,
            settings,
            state: Mutex::new(State { last_activity: Instant::now(), in_flight: 0, last_latency_ms: 0, closing: false }),
            wake: Condvar::new(),
        }
    }

    fn enter(&self, counts_as_use: bool) -> Result<Flight<'_>, String> {
        let mut s = self.state.lock().unwrap();
        if s.closing {
            return Err("the AI helper is shutting down, try again".into());
        }
        s.in_flight += 1;
        if counts_as_use {
            s.last_activity = Instant::now();
        }
        Ok(Flight(self, counts_as_use))
    }

    /// Whether llama-server answers `/health` with 200 (the model is loaded).
    pub fn healthy(&self) -> bool {
        http::request(self.settings.addr, "GET", "/health", None, Duration::from_millis(800))
            .is_ok_and(|r| r.status == 200)
    }

    /// Starts the unit without waiting for the model, then evaluates the
    /// stable prompt prefix (system prompt, apps, windows) once, so the first
    /// real request only has to read the query. Never blocks the caller.
    pub fn prewarm(self: &Arc<Self>, context: &str) -> Result<(), String> {
        let flight = self.enter(true)?;
        drop(flight);
        let ctx: Option<Context> = serde_json::from_str(context).ok();
        let engine = self.clone();
        std::thread::spawn(move || {
            let Ok(_flight) = engine.enter(true) else { return };
            if let Err(error) = engine.ensure_ready() {
                eprintln!("gavno-ai: prewarm: {error}");
                return;
            }
            if let Some(ctx) = ctx {
                let user = prompt::user_message(&ctx, "");
                let mut body = prompt::chat_body(prompt::SYSTEM, &user, &prompt::actions_schema(), 1);
                body["cache_prompt"] = json!(true);
                let _ = http::request(
                    engine.settings.addr,
                    "POST",
                    "/v1/chat/completions",
                    Some(body.to_string().as_bytes()),
                    engine.settings.request_timeout,
                );
            }
        });
        Ok(())
    }

    fn ensure_ready(&self) -> Result<(), String> {
        if self.healthy() {
            return Ok(());
        }
        self.unit.start().map_err(|e| format!("cannot start the model: {e}"))?;
        let deadline = Instant::now() + self.settings.ready_timeout;
        while Instant::now() < deadline {
            if self.healthy() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("the model did not become ready in time".into())
    }

    fn chat(&self, system: &str, user: &str, schema: &Value) -> Result<String, String> {
        let body = prompt::chat_body(system, user, schema, self.settings.max_tokens);
        let started = Instant::now();
        let response = http::request(
            self.settings.addr,
            "POST",
            "/v1/chat/completions",
            Some(body.to_string().as_bytes()),
            self.settings.request_timeout,
        )?;
        if response.status != 200 {
            return Err(format!("the model server answered {}", response.status));
        }
        let value: Value = serde_json::from_slice(&response.body).map_err(|e| format!("bad server answer ({e})"))?;
        let text = value["choices"][0]["message"]["content"]
            .as_str()
            .ok_or("the server answer has no text")?
            .to_owned();
        self.state.lock().unwrap().last_latency_ms = started.elapsed().as_millis() as u64;
        Ok(text)
    }

    /// `Interpret`: returns `{"actions":[..],"summary":[..]}`.
    pub fn interpret(&self, query: &str, context: &str) -> Result<Value, String> {
        let _flight = self.enter(true)?;
        let ctx: Context = serde_json::from_str(context).map_err(|e| format!("bad context ({e})"))?;
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 300 {
            return Err("the request must be 1-300 characters".into());
        }
        self.ensure_ready()?;
        let text = self.chat(prompt::SYSTEM, &prompt::user_message(&ctx, query), &prompt::actions_schema())?;
        let result = action::interpret(&text, &ctx)?;
        Ok(json!(result))
    }

    /// `NameWorkspace`: refines a workspace name, only while the model already
    /// runs and without counting as use, so naming never keeps a model alive.
    pub fn name_workspace(&self, windows: &str) -> Result<Value, String> {
        let _flight = self.enter(false)?;
        let windows: Vec<(String, String)> = serde_json::from_str::<Vec<Value>>(windows)
            .map_err(|e| format!("bad windows ({e})"))?
            .iter()
            .map(|w| {
                let field = |k: &str| w[k].as_str().unwrap_or_default().chars().take(60).collect::<String>();
                (field("app_id"), field("title"))
            })
            .collect();
        if windows.is_empty() {
            return Err("no windows to name".into());
        }
        if !self.healthy() {
            return Err("the model is not running".into());
        }
        let text = self.chat(prompt::NAME_SYSTEM, &prompt::name_message(&windows), &prompt::name_schema())?;
        Ok(json!({"name": action::workspace_name(&text)?}))
    }

    pub fn status(&self) -> (bool, u64) {
        (self.healthy(), self.state.lock().unwrap().last_latency_ms)
    }

    /// Blocks until the helper has been idle for `settings.idle` with no
    /// request in flight, then stops the unit (systemd sends SIGINT) and
    /// returns. Sleeps on a condition variable: no polling.
    pub fn run_until_idle(&self) {
        let mut s = self.state.lock().unwrap();
        loop {
            if s.in_flight > 0 {
                s = self.wake.wait(s).unwrap();
                continue;
            }
            let due = s.last_activity + self.settings.idle;
            let now = Instant::now();
            if now >= due {
                s.closing = true;
                break;
            }
            s = self.wake.wait_timeout(s, due - now).unwrap().0;
        }
        drop(s);
        if let Err(error) = self.unit.stop() {
            eprintln!("gavno-ai: cannot stop the model unit: {error}");
        }
    }
}
