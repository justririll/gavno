use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::{Duration, Instant},
};
fn once(method: &str, params: &Value) -> Result<Value, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    let mut stream = UnixStream::connect(PathBuf::from(runtime).join("gavno/ipc.sock"))
        .map_err(|e| format!("Cannot connect to GAVNO: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    writeln!(
        stream,
        "{}",
        json!({"id":1,"method":method,"params":params})
    )
    .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(1024 * 1024)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if !line.ends_with('\n') {
        return Err("Incomplete IPC response".into());
    }
    let response: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if response["id"] != 1 {
        return Err("Unexpected IPC response ID".into());
    }
    if let Some(error) = response.get("error") {
        return Err(error.as_str().unwrap_or("IPC error").into());
    }
    response
        .get("result")
        .cloned()
        .ok_or("Missing IPC result".into())
}
/// DRM requests may race a page flip. Retry on this one transient error only.
pub fn call(method: &str, params: &Value) -> Result<Value, String> {
    let start = Instant::now();
    loop {
        match once(method, params) {
            Err(e)
                if e == "output is presenting; retry"
                    && start.elapsed() < Duration::from_secs(3) =>
            {
                std::thread::sleep(Duration::from_millis(30))
            }
            result => return result,
        }
    }
}
pub async fn request(method: &'static str, params: Value) -> Result<Value, String> {
    crate::work::run(move || call(method, &params)).await?
}

pub fn unavailable(error: &str) -> bool {
    error.starts_with("Cannot connect to GAVNO:") || error == "XDG_RUNTIME_DIR is not set"
}
