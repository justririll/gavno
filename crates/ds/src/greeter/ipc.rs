//! greetd IPC on a worker thread, so PAM delays (pam_faildelay) never block
//! the compositor. Without `$GREETD_SOCK` it runs a demo backend that
//! accepts the password "ds", for testing the greeter on its own.
//!
//! Wire format: u32 length (native endian) + JSON.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::mpsc,
    time::Duration,
};

use serde_json::{json, Value};
use smithay::reexports::calloop::{channel, LoopHandle};

use crate::Ds;

/// A password that is wiped from memory when dropped.
pub struct Secret(String);

impl Secret {
    pub fn new() -> Self {
        Secret(String::new())
    }
    pub fn push(&mut self, c: char) {
        self.0.push(c);
    }
    pub fn pop(&mut self) {
        self.0.pop();
    }
    pub fn clear(&mut self) {
        wipe(&mut self.0);
    }
    pub fn len(&self) -> usize {
        self.0.chars().count()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn take(&mut self) -> Secret {
        Secret(std::mem::take(&mut self.0))
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

fn wipe(s: &mut String) {
    // Safety: zero bytes are valid UTF-8 and the string is cleared right after.
    unsafe { s.as_mut_vec().fill(0) };
    s.clear();
}

pub enum Request {
    Login { user: String, password: Secret },
    Start { cmd: Vec<String>, env: Vec<String> },
}

#[derive(Debug)]
pub enum Reply {
    Authenticated,
    Denied(String),
    Started,
    Failed(String),
}

pub struct Ipc {
    tx: mpsc::Sender<Request>,
    pub demo: bool,
}

impl Ipc {
    pub fn spawn(loop_handle: &LoopHandle<'static, Ds>) -> Ipc {
        let sock = std::env::var("GREETD_SOCK").ok();
        let demo = sock.is_none();
        let (tx, rx) = mpsc::channel::<Request>();
        let (reply_tx, reply_rx) = channel::channel::<Reply>();
        loop_handle
            .insert_source(reply_rx, |event, _, ds| {
                if let channel::Event::Msg(reply) = event {
                    if let Some(greeter) = ds.greeter.as_mut() {
                        greeter.on_reply(reply);
                        if greeter.quit_requested {
                            ds.quit();
                        }
                    }
                    ds.queue_redraw_all();
                }
            })
            .expect("greeter ipc channel");

        std::thread::Builder::new()
            .name("greetd-ipc".into())
            .spawn(move || {
                let mut conn = sock.and_then(|s| match UnixStream::connect(&s) {
                    Ok(c) => Some(c),
                    Err(err) => {
                        tracing::error!(?err, "cannot connect to greetd");
                        None
                    }
                });
                for req in rx {
                    let reply = match conn.as_mut() {
                        Some(c) => handle(c, req),
                        None => demo_handle(req),
                    };
                    if reply_tx.send(reply).is_err() {
                        break;
                    }
                }
            })
            .expect("greeter ipc thread");
        Ipc { tx, demo }
    }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }
}

fn demo_handle(req: Request) -> Reply {
    match req {
        Request::Login { password, .. } => {
            std::thread::sleep(Duration::from_millis(700));
            if password.0 == "ds" {
                Reply::Authenticated
            } else {
                Reply::Denied("Wrong password".into())
            }
        }
        Request::Start { .. } => Reply::Started,
    }
}

fn handle(conn: &mut UnixStream, req: Request) -> Reply {
    match req {
        Request::Login { user, password } => match login(conn, &user, &password) {
            Ok(r) => r,
            Err(err) => Reply::Failed(format!("greetd: {err}")),
        },
        Request::Start { cmd, env } => {
            match roundtrip(conn, &json!({"type": "start_session", "cmd": cmd, "env": env})) {
                Ok(v) if v["type"] == "success" => Reply::Started,
                Ok(v) => Reply::Failed(describe(&v)),
                Err(err) => Reply::Failed(format!("greetd: {err}")),
            }
        }
    }
}

fn login(conn: &mut UnixStream, user: &str, password: &Secret) -> std::io::Result<Reply> {
    let mut resp = roundtrip(conn, &json!({"type": "create_session", "username": user}))?;
    let mut password_sent = false;
    loop {
        match resp["type"].as_str() {
            Some("success") => return Ok(Reply::Authenticated),
            Some("error") => {
                let _ = roundtrip(conn, &json!({"type": "cancel_session"}));
                let msg = if resp["error_type"] == "auth_error" {
                    "Wrong password".to_owned()
                } else {
                    describe(&resp)
                };
                return Ok(Reply::Denied(msg));
            }
            Some("auth_message") => {
                let kind = resp["auth_message_type"].as_str().unwrap_or("");
                match kind {
                    "secret" | "visible" if !password_sent => {
                        password_sent = true;
                        resp = send_secret(conn, &password.0)?;
                    }
                    "secret" | "visible" => {
                        // A second prompt (e.g. an expired password) cannot be
                        // answered from here.
                        let _ = roundtrip(conn, &json!({"type": "cancel_session"}));
                        let text = resp["auth_message"].as_str().unwrap_or("Login needs another answer").trim();
                        return Ok(Reply::Denied(text.to_owned()));
                    }
                    _ => {
                        resp = roundtrip(conn, &json!({"type": "post_auth_message_response"}))?;
                    }
                }
            }
            _ => {
                let _ = roundtrip(conn, &json!({"type": "cancel_session"}));
                return Ok(Reply::Failed(describe(&resp)));
            }
        }
    }
}

fn describe(v: &Value) -> String {
    v["description"].as_str().unwrap_or("Login failed").to_owned()
}

/// Sends the password without building an intermediate JSON value, and wipes
/// the serialized message afterwards.
fn send_secret(conn: &mut UnixStream, password: &str) -> std::io::Result<Value> {
    let mut buf = Vec::with_capacity(96 + password.len() * 2);
    buf.extend_from_slice(br#"{"type":"post_auth_message_response","response":"#);
    serde_json::to_writer(&mut buf, password).map_err(std::io::Error::other)?;
    buf.push(b'}');
    let result = write_frame(conn, &buf);
    buf.fill(0);
    result?;
    read_frame(conn)
}

fn roundtrip(conn: &mut UnixStream, msg: &Value) -> std::io::Result<Value> {
    let bytes = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    write_frame(conn, &bytes)?;
    read_frame(conn)
}

fn write_frame(conn: &mut UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    conn.write_all(&(bytes.len() as u32).to_ne_bytes())?;
    conn.write_all(bytes)
}

fn read_frame(conn: &mut UnixStream) -> std::io::Result<Value> {
    let mut len = [0u8; 4];
    conn.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_ne_bytes(len) as usize];
    conn.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(std::io::Error::other)
}
