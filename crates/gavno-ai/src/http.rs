//! A tiny blocking HTTP/1.1 client for llama-server on 127.0.0.1.
//!
//! Only what the service needs: one request per connection, `Connection:
//! close`, `Content-Length` or chunked bodies, and hard time limits. A timeout
//! only closes the connection: the server is never signalled (NPU rules in
//! `design/ai.md`).
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let end = find(data, b"\r\n").ok_or("truncated chunked body")?;
        let size = std::str::from_utf8(&data[..end])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok())
            .ok_or("bad chunk size")?;
        data = &data[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err("truncated chunk".into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// Sends `method path` with an optional JSON body and reads the whole answer
/// within `total`.
pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    total: Duration,
) -> Result<Response, String> {
    let deadline = Instant::now() + total;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(1500).min(total))
        .map_err(|e| format!("cannot connect to the model server: {e}"))?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.map_or(0, <[u8]>::len)
    );
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body.unwrap_or_default()))
        .map_err(|e| format!("cannot send to the model server: {e}"))?;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("the model server did not answer in time".into());
        }
        stream.set_read_timeout(Some(left)).ok();
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.len() > 4 << 20 {
                    return Err("the model server answer is too large".into());
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err("the model server did not answer in time".into());
            }
            Err(e) => return Err(format!("cannot read from the model server: {e}")),
        }
    }
    let split = find(&data, b"\r\n\r\n").ok_or("malformed answer from the model server")?;
    let head = String::from_utf8_lossy(&data[..split]).to_lowercase();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("malformed status line")?;
    let rest = &data[split + 4..];
    let body = if head.contains("transfer-encoding: chunked") { dechunk(rest)? } else { rest.to_vec() };
    Ok(Response { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunk_joins_chunks() {
        assert_eq!(dechunk(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap(), b"abcde");
        assert!(dechunk(b"5\r\nab").is_err());
    }
}
