// Exercise the production listener/client sources without starting a compositor.
#[path = "../src/ipc/protocol.rs"]
mod protocol;
#[path = "../src/ipc/transport.rs"]
mod transport;

use serde_json::{json, Value};
use smithay::reexports::calloop::EventLoop;
use std::{
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    time::{Duration, Instant},
};

fn receive<D>(
    event_loop: &mut EventLoop<D>,
    data: &mut D,
    client: &mut UnixStream,
    count: usize,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = Vec::new();
    while bytes.iter().filter(|&&b| b == b'\n').count() < count {
        assert!(Instant::now() < deadline, "timed out receiving IPC replies");
        event_loop.dispatch(Duration::from_millis(1), data).unwrap();
        let mut chunk = [0; 64 * 1024];
        loop {
            match client.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
    }
    bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[test]
fn listener_handles_fragmentation_pipelining_errors_and_multiple_clients() {
    let root = std::env::temp_dir().join(format!("ds-socket-test-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let path = root.join("ipc.sock");
    let mut event_loop = EventLoop::<Vec<String>>::try_new().unwrap();
    transport::listen(
        event_loop.handle(),
        UnixListener::bind(&path).unwrap(),
        |line, calls| {
            protocol::reply(line, |method, params| {
                calls.push(method.to_string());
                match method {
                    "screen" => Ok(json!({"outputs": [], "windows": [], "focused": null})),
                    "focus" => Err(format!("unknown window id: {}", params["id"])),
                    _ => Err(format!("unknown method: {method}")),
                }
            })
        },
    )
    .unwrap();
    let mut calls = Vec::new();
    let mut client = UnixStream::connect(&path).unwrap();
    client.set_nonblocking(true).unwrap();
    event_loop.dispatch(Duration::ZERO, &mut calls).unwrap();
    client.write_all(br#"{"id":1,"method":"scr"#).unwrap();
    event_loop
        .dispatch(Duration::from_millis(1), &mut calls)
        .unwrap();
    assert!(calls.is_empty(), "incomplete messages must not dispatch");
    client.write_all(b"een\"}\n{broken}\n{\"id\":2,\"method\":\"focus\",\"params\":{\"id\":99}}\n{\"id\":3,\"method\":\"other\"}\n").unwrap();
    let replies = receive(&mut event_loop, &mut calls, &mut client, 4);
    assert_eq!(
        replies[0],
        json!({"id":1,"result":{"outputs":[],"windows":[],"focused":null}})
    );
    assert!(replies[1]["id"].is_null());
    assert!(replies[1]["error"]
        .as_str()
        .unwrap()
        .starts_with("invalid JSON"));
    assert_eq!(replies[2], json!({"id":2,"error":"unknown window id: 99"}));
    assert_eq!(replies[3], json!({"id":3,"error":"unknown method: other"}));
    let mut second = UnixStream::connect(&path).unwrap();
    second.set_nonblocking(true).unwrap();
    second
        .write_all(b"{\"id\":4,\"method\":\"screen\",\"params\":{}}\n")
        .unwrap();
    second.shutdown(Shutdown::Write).unwrap();
    assert_eq!(
        receive(&mut event_loop, &mut calls, &mut second, 1)[0]["id"],
        4
    );
    client
        .write_all(b"{\"id\":5,\"method\":\"screen\"}\n")
        .unwrap();
    assert_eq!(
        receive(&mut event_loop, &mut calls, &mut client, 1)[0]["id"],
        5
    );
    drop(event_loop);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn backpressure_retains_all_replies_after_peer_half_closes() {
    let mut event_loop = EventLoop::<()>::try_new().unwrap();
    let (server, mut client) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    transport::add_client(event_loop.handle(), server, |line, _| {
        protocol::reply(line, |_, _| {
            Ok(json!({"payload": "x".repeat(2 * 1024 * 1024)}))
        })
    })
    .unwrap();
    client
        .write_all(b"{\"id\":1,\"method\":\"screen\"}\n{\"id\":2,\"method\":\"screen\"}\n")
        .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    // Fill the server's send buffer while the peer is not reading.
    event_loop
        .dispatch(Duration::from_millis(1), &mut ())
        .unwrap();
    let replies = receive(&mut event_loop, &mut (), &mut client, 2);
    assert_eq!(
        replies
            .iter()
            .map(|r| r["id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    for reply in replies {
        assert_eq!(
            reply["result"]["payload"].as_str().unwrap().len(),
            2 * 1024 * 1024
        );
    }
}
