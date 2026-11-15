use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixListener,
    process::{Command, Stdio},
    thread,
};

#[test]
fn stdio_forwards_tools_and_recovers_when_ds_is_unavailable() {
    let root = std::env::temp_dir().join(format!("ds-agent-mcp-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let path = root.join("ipc.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        for method in ["screen", "focus", "close", "move", "raise"] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], method);
            assert_eq!(request["id"], 1);
            if method != "screen" {
                assert_eq!(request["params"]["id"], 7);
            }
            if method == "move" {
                assert_eq!(request["params"], json!({"id":7,"x":-10,"y":20,"h":600}));
            }
            let result = if method == "screen" {
                json!({"outputs":[],"windows":[{"id":7,"app_id":"foot","title":"hello","x":0,"y":0,"w":800,"h":600,"focused":true}],"focused":7})
            } else {
                json!({"ok":true})
            };
            writeln!(stream, "{}", json!({"id":request["id"],"result":result})).unwrap();
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_ds-agent"))
        .arg("mcp")
        .env("DS_SOCK", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut exchange = |request: Value| -> Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        serde_json::from_str(&line).unwrap()
    };
    assert_eq!(
        exchange(json!({"jsonrpc":"2.0","id":0,"method":"initialize"}))["result"]
            ["protocolVersion"],
        "2025-06-18"
    );
    for (id, name) in ["screen", "focus", "close", "move", "raise"]
        .into_iter()
        .enumerate()
    {
        let args = match name {
            "screen" => json!({}),
            "move" => json!({"id":7,"x":-10,"y":20,"h":600}),
            _ => json!({"id":7}),
        };
        let reply = exchange(
            json!({"jsonrpc":"2.0","id":id+1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        );
        assert_eq!(reply["result"]["isError"], false);
        if name == "screen" {
            assert_eq!(
                reply["result"]["content"][0]["text"],
                "w7 foot \"hello\" 800x600+0+0 focused"
            );
        }
    }
    server.join().unwrap(); // No listener remains; the next call must fail as a tool result.
    let reply =
        exchange(json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"screen"}}));
    assert_eq!(reply["result"]["isError"], true);
    assert!(reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("is ds running?"));
    assert_eq!(
        exchange(json!({"jsonrpc":"2.0","id":10,"method":"tools/list"}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    drop(exchange);
    drop(input);
    assert!(child.wait().unwrap().success());
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.is_empty(), "{stderr}");
    fs::remove_dir_all(root).unwrap();
}
