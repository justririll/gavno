//! greetd IPC on a worker thread, so PAM delays (pam_faildelay) never block
//! the compositor. Without `$GREETD_SOCK` it runs a demo backend that
//! accepts the password "gavno", for testing the greeter on its own.
//!
//! Wire format: u32 length (native endian) + JSON.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{mpsc, Arc, atomic::{AtomicU64, Ordering}},
    time::Duration,
};

use serde_json::{json, Value};
use zeroize::Zeroize;
use smithay::reexports::calloop::{channel, LoopHandle};

use crate::Gavno;

/// A password that is wiped from memory when dropped.
pub struct Secret(String);

impl Secret {
    pub fn new() -> Self {
        Secret(String::with_capacity(2048))
    }
    pub fn push(&mut self, c: char) {
        // Reserve once: editing never leaves old password allocations behind.
        if self.0.len() + c.len_utf8() <= 2048 { self.0.push(c); }
    }
    pub fn pop(&mut self) {
        if let Some((end, _)) = self.0.char_indices().next_back() {
            // Safety: only the removed UTF-8 character is wiped before truncation.
            unsafe { self.0.as_mut_vec()[end..].zeroize(); }
            self.0.truncate(end);
        }
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
        std::mem::replace(self, Self::new())
    }
    pub(crate) fn as_str(&self) -> &str { &self.0 }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

fn wipe(s: &mut String) {
    // zeroize wipes the allocation (including spare capacity) before clearing.
    s.zeroize();
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
    tx: mpsc::Sender<(u64, Request)>,
    epoch: Arc<AtomicU64>,
    pub demo: bool,
}

impl Ipc {
    pub fn spawn(loop_handle: &LoopHandle<'static, Gavno>) -> Ipc {
        let sock = std::env::var("GREETD_SOCK").ok();
        let demo = sock.is_none();
        let (tx, rx) = mpsc::channel::<(u64, Request)>();
        let epoch = Arc::new(AtomicU64::new(0));
        let replies = epoch.clone();
        let (reply_tx, reply_rx) = channel::channel::<(u64, Reply)>();
        loop_handle
            .insert_source(reply_rx, move |event, _, gv| {
                if let channel::Event::Msg((generation, reply)) = event {
                    if generation != replies.load(Ordering::Relaxed) { return; }
                    if let Some(greeter) = gv.greeter.as_mut() {
                        greeter.safe_call((), |g| g.on_reply(reply));
                        if greeter.quit_requested {
                            if greeter.hands_off_to_gavno() {
                                crate::backend::udev::keep_panel_on_exit();
                            }
                            gv.quit();
                        }
                    }
                    gv.queue_redraw_all();
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
                for (generation, req) in rx {
                    let reply = match conn.as_mut() {
                        Some(c) => handle(c, req),
                        None => demo_handle(req),
                    };
                    if reply_tx.send((generation, reply)).is_err() {
                        break;
                    }
                }
            })
            .expect("greeter ipc thread");
        Ipc { tx, demo, epoch }
    }

    pub(crate) fn invalidate(&self) { self.epoch.fetch_add(1, Ordering::Relaxed); }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send((self.epoch.load(Ordering::Relaxed), req));
    }
}

fn demo_handle(req: Request) -> Reply {
    match req {
        Request::Login { password, .. } => {
            std::thread::sleep(Duration::from_millis(700));
            if password.0 == "gavno" {
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

#[cfg(test)]
mod secret_tests {
    use super::*;
    #[test]
    fn edits_and_clear_wipe_the_live_allocation() {
        let mut secret = Secret::new();
        secret.push('a'); secret.push('🔒');
        let pointer = secret.0.as_ptr();
        secret.pop();
        assert_eq!(secret.as_str(), "a");
        // The allocation is still live; these bytes were initialized and then wiped.
        assert_eq!(unsafe { std::slice::from_raw_parts(pointer.add(1), 4) }, [0; 4]);
        secret.clear();
        assert_eq!(unsafe { *pointer }, 0);
        assert!(secret.is_empty());
    }
    #[test]
    fn take_transfers_ownership_and_growth_never_reallocates() {
        let mut secret = Secret::new();
        let pointer = secret.0.as_ptr();
        for _ in 0..1024 { secret.push('🔒'); }
        assert_eq!(secret.0.as_ptr(), pointer); assert_eq!(secret.len(), 512);
        let mut owned = secret.take();
        assert!(secret.is_empty()); assert_eq!(owned.0.as_ptr(), pointer);
        owned.clear();
        assert!(unsafe { std::slice::from_raw_parts(pointer, 2048) }.iter().all(|byte| *byte == 0));
    }
}
