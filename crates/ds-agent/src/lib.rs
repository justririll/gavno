//! Minimal MCP over newline-delimited JSON-RPC, with one ds connection per tool call.
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use serde_json::{json, Value};

const TOOLS: [&str; 5] = ["screen", "focus", "close", "move", "raise"];
const MAX_REPLY: u64 = 8 * 1024 * 1024;

pub fn tools_list() -> Value {
    let tools: Vec<_> = TOOLS.iter().map(|&name| {
        let mut properties = json!({});
        let mut required = vec![];
        if name != "screen" {
            properties["id"] = json!({"type": "integer", "minimum": 1, "description": "Stable numeric window id from screen"});
            required.push("id");
        }
        if name == "move" {
            for key in ["x", "y", "w", "h"] {
                properties[key] = json!({"type": "integer", "minimum": i32::MIN, "maximum": i32::MAX});
                if matches!(key, "w" | "h") { properties[key]["minimum"] = json!(1); }
            }
            required.extend(["x", "y"]);
        }
        let description = match name {
            "screen" => "List outputs and windows, topmost first, with stable ids and focus",
            "focus" => "Raise and focus a window",
            "close" => "Ask a window to close",
            "move" => "Move a window in logical pixels; optionally configure width and/or height",
            "raise" => "Raise a window without changing keyboard focus",
            _ => unreachable!(),
        };
        json!({"name": name, "description": description,
            "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}})
    }).collect();
    json!({"tools": tools})
}

fn error(id: Value, code: i32, message: impl AsRef<str>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.as_ref()}})
}

fn tool_result(result: Result<String, String>) -> Value {
    match result {
        Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
        Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
    }
}

fn validate_tool(name: &str, params: &Value) -> Result<(), String> {
    if !TOOLS.contains(&name) {
        return Err(format!("unknown tool: {name}"));
    }
    let object = params.as_object().ok_or("arguments must be an object")?;
    if name != "screen"
        && !params
            .get("id")
            .and_then(Value::as_u64)
            .is_some_and(|id| id > 0)
    {
        return Err("id must be a positive integer".into());
    }
    for key in object.keys() {
        if (name == "screen")
            || (key != "id" && !(name == "move" && ["x", "y", "w", "h"].contains(&key.as_str())))
        {
            return Err(format!("unexpected argument: {key}"));
        }
    }
    if name == "move" {
        for key in ["x", "y", "w", "h"] {
            if matches!(key, "w" | "h") && params.get(key).is_none() {
                continue;
            }
            let n = params
                .get(key)
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| format!("{key} must be a 32-bit integer"))?;
            if matches!(key, "w" | "h") && n <= 0 {
                return Err(format!("{key} must be positive"));
            }
        }
    }
    Ok(())
}

/// None means a notification: stdout must stay silent.
pub fn handle_request(
    line: &str,
    mut call: impl FnMut(&str, &Value) -> Result<Value, String>,
) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => return Some(error(Value::Null, -32700, format!("parse error: {e}"))),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if !request.is_object()
        || request["jsonrpc"] != "2.0"
        || !request["method"].is_string()
        || !(id.is_null() || id.is_string() || id.is_number())
    {
        return Some(error(Value::Null, -32600, "invalid JSON-RPC request"));
    }
    if request.get("id").is_none() {
        return None;
    }
    let result = match request["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion": "2025-06-18", "serverInfo": {"name": "ds-agent", "version": env!("CARGO_PKG_VERSION")}, "capabilities": {"tools": {}}})
        }
        "tools/list" => tools_list(),
        "tools/call" => {
            let params = &request["params"];
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "tools/call requires params.name"));
            };
            let empty = json!({});
            let arguments = params.get("arguments").unwrap_or(&empty);
            tool_result(
                validate_tool(name, arguments)
                    .and_then(|()| call(name, arguments))
                    .map(|result| {
                        if name == "screen" {
                            format_screen(&result)
                        } else {
                            result.to_string()
                        }
                    }),
            )
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

pub fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    mut call: impl FnMut(&str, &Value) -> Result<Value, String>,
) -> io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        if let Some(reply) = handle_request(&line, &mut call) {
            serde_json::to_writer(&mut output, &reply)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

pub fn socket_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("DS_SOCK").filter(|s| !s.is_empty()) {
        return Ok(path.into());
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .map(|runtime| PathBuf::from(runtime).join("ds/ipc.sock"))
        .ok_or_else(|| {
            "cannot find ds IPC socket: set DS_SOCK or XDG_RUNTIME_DIR; is ds running?".into()
        })
}

pub fn forward(method: &str, params: &Value) -> Result<Value, String> {
    let path = socket_path()?;
    let connect = || -> Result<Value, String> {
        let mut stream = UnixStream::connect(&path).map_err(|e| {
            format!(
                "cannot connect to ds at {}: {e}; is ds running?",
                path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| e.to_string())?;
        let request = json!({"id": 1, "method": method, "params": params});
        serde_json::to_writer(&mut stream, &request)
            .map_err(|e| format!("ds IPC write failed: {e}"))?;
        stream
            .write_all(b"\n")
            .map_err(|e| format!("ds IPC write failed: {e}"))?;
        let mut response = Vec::new();
        BufReader::new(stream)
            .take(MAX_REPLY + 1)
            .read_until(b'\n', &mut response)
            .map_err(|e| format!("ds IPC read failed: {e}"))?;
        if response.len() as u64 > MAX_REPLY || response.last() != Some(&b'\n') {
            return Err("ds IPC disconnected or returned an oversized/incomplete reply".into());
        }
        let response: Value =
            serde_json::from_slice(&response).map_err(|e| format!("invalid ds IPC reply: {e}"))?;
        if response["id"] != 1 {
            return Err("ds IPC returned a mismatched reply id".into());
        }
        if let Some(error) = response.get("error") {
            return Err(format!("ds: {}", error.as_str().unwrap_or("IPC error")));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "ds IPC reply has no result".into())
    };
    connect()
}

fn token(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "._-/".contains(c)) {
        s.into()
    } else {
        json!(s).to_string()
    }
}

pub fn format_screen(screen: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(outputs) = screen["outputs"].as_array() {
        for output in outputs {
            lines.push(format!(
                "output {} {}x{}{:+}{:+} scale={}",
                token(output["name"].as_str().unwrap_or("?")),
                output["w"],
                output["h"],
                output["x"].as_i64().unwrap_or(0),
                output["y"].as_i64().unwrap_or(0),
                output["scale"]
            ));
        }
    }
    if let Some(windows) = screen["windows"].as_array() {
        for window in windows {
            let mut line = format!(
                "w{} {} {} {}x{}{:+}{:+}",
                window["id"],
                token(window["app_id"].as_str().unwrap_or("")),
                json!(window["title"].as_str().unwrap_or("")),
                window["w"],
                window["h"],
                window["x"].as_i64().unwrap_or(0),
                window["y"].as_i64().unwrap_or(0)
            );
            if let Some(output) = window["output"].as_str() {
                line.push_str(&format!(" out={}", token(output)));
            }
            if window["focused"] == true
                || screen["focused"]
                    .as_u64()
                    .is_some_and(|id| window["id"].as_u64() == Some(id))
            {
                line.push_str(" focused");
            }
            lines.push(line);
        }
        if windows.is_empty() {
            lines.push("no windows".into());
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parsing_and_initialization() {
        let no_call = |_: &str, _: &Value| panic!("not a tool call");
        let reply = handle_request(
            r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}"#,
            no_call,
        )
        .unwrap();
        assert_eq!(reply["id"], "init");
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply["result"]["serverInfo"]["name"], "ds-agent");
        assert_eq!(reply["result"]["capabilities"], json!({"tools":{}}));
        assert!(handle_request(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            no_call
        )
        .is_none());
        for (line, code) in [
            ("{", -32700),
            ("[]", -32600),
            (r#"{"jsonrpc":"2.0","id":1,"method":"unknown"}"#, -32601),
        ] {
            assert_eq!(
                handle_request(line, no_call).unwrap()["error"]["code"],
                code
            );
        }
    }

    #[test]
    fn list_has_all_tools_and_geometry_schema() {
        let list = tools_list();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|t| t["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            TOOLS
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["inputSchema"]["properties"].is_object());
            assert!(tool["inputSchema"]["required"].is_array());
        }
        assert_eq!(tools[3]["inputSchema"]["required"], json!(["id", "x", "y"]));
        assert_eq!(tools[3]["inputSchema"]["properties"]["w"]["minimum"], 1);
        assert_eq!(tools[0]["inputSchema"]["required"], json!([]));
    }

    #[test]
    fn compact_screen_preserves_order_and_escapes_titles() {
        let screen = json!({"outputs":[{"name":"eDP-1","x":0,"y":0,"w":1920,"h":1080,"scale":1.75}], "focused":3,
            "windows":[{"id":3,"app_id":"foot","title":"~/Projects","x":413,"y":180,"w":820,"h":560,"output":"eDP-1"},
                {"id":1,"app_id":"app with spaces","title":"a\n\"b","x":-200,"y":0,"w":640,"h":480,"focused":false}]});
        assert_eq!(format_screen(&screen), "output eDP-1 1920x1080+0+0 scale=1.75\nw3 foot \"~/Projects\" 820x560+413+180 out=eDP-1 focused\nw1 \"app with spaces\" \"a\\n\\\"b\" 640x480-200+0");
        assert_eq!(
            format_screen(&json!({"outputs":[],"windows":[],"focused":null})),
            "no windows"
        );
    }

    #[test]
    fn tool_errors_do_not_stop_stdio_server() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"screen\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n");
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, |_, _| {
            Err("is ds running?".into())
        })
        .unwrap();
        let replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["result"]["isError"], true);
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn validates_calls_before_forwarding() {
        for (name, arguments) in [
            ("move", json!({"id":1,"x":0})),
            ("move", json!({"id":1,"x":0,"y":0,"h":-1})),
            ("focus", json!({"id":"w1"})),
            ("screen", json!({"extra":true})),
            ("other", json!({})),
        ] {
            let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}});
            assert_eq!(
                handle_request(&request.to_string(), |_, _| panic!("invalid call")).unwrap()
                    ["result"]["isError"],
                true
            );
        }
        for name in TOOLS {
            let args = match name {
                "screen" => json!({}),
                "move" => json!({"id":3,"x":-1,"y":2,"w":100}),
                _ => json!({"id":3}),
            };
            let request = json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":name,"arguments":args}});
            let reply = handle_request(&request.to_string(), |method, params| {
                assert_eq!(method, name);
                assert_eq!(params, &args);
                Ok(json!({"ok":true}))
            })
            .unwrap();
            assert_eq!(reply["result"]["isError"], false);
        }
    }
}
