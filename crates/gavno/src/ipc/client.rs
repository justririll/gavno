//! One-shot, newline-framed IPC client; no compositor initialization is needed.
use std::{io::{self, BufRead, BufReader, Read, Write}, os::unix::net::UnixStream, path::PathBuf, time::Duration};
use serde_json::{json, Value};

pub(super) fn socket_path() -> io::Result<PathBuf> {
    for name in ["GAVNO_SOCKET", "GAVNO_SOCK"] {
        if let Some(path) = std::env::var_os(name).filter(|v| !v.is_empty()) { return Ok(path.into()); }
    }
    std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())
        .map(|p| PathBuf::from(p).join("gavno/ipc.sock"))
        .ok_or_else(|| io::Error::other("set GAVNO_SOCKET, GAVNO_SOCK or XDG_RUNTIME_DIR to find the IPC socket"))
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn std::error::Error>> {
    let method = args.next().ok_or("usage: gavno msg <method> [json]")?;
    let params: Value = serde_json::from_str(&args.next().unwrap_or_else(|| "{}".into()))?;
    if !params.is_object() { return Err("params must be a JSON object".into()); }
    if args.next().is_some() { return Err("usage: gavno msg <method> [json]".into()); }
    let mut socket = UnixStream::connect(socket_path()?)?;
    // Maximum injected duration is 60 seconds; allow time for rendering too.
    socket.set_read_timeout(Some(Duration::from_secs(70)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    serde_json::to_writer(&mut socket, &json!({"id": 1, "method": method, "params": params}))?;
    socket.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(socket).take(1024 * 1024 + 1).read_line(&mut line)?;
    if !line.ends_with('\n') || line.len() > 1024 * 1024 { return Err("incomplete or oversized IPC reply".into()); }
    let reply: Value = serde_json::from_str(&line)?;
    if reply["id"] != 1 || (reply.get("error").is_none() && reply.get("result").is_none()) {
        return Err("invalid IPC reply".into());
    }
    println!("{reply}");
    if let Some(error) = reply.get("error") { return Err(format!("IPC error: {error}").into()); }
    Ok(())
}
