//! The engine against a fake llama-server and a fake unit.
use gavno_ai::engine::{Engine, Settings, UnitControl};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    time::Duration,
};

struct Fake {
    port: u16,
    /// `/health` answers 200 only when set; the fake unit sets it on start.
    up: Arc<AtomicBool>,
    chat_bodies: Arc<Mutex<Vec<Value>>>,
    content: Arc<Mutex<String>>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

struct FakeUnit {
    up: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl UnitControl for FakeUnit {
    fn start(&self) -> Result<(), String> {
        self.starts.fetch_add(1, SeqCst);
        self.up.store(true, SeqCst);
        Ok(())
    }
    fn stop(&self) -> Result<(), String> {
        self.stops.fetch_add(1, SeqCst);
        self.up.store(false, SeqCst);
        Ok(())
    }
}

fn spawn_server() -> (Fake, Box<dyn UnitControl>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let up = Arc::new(AtomicBool::new(false));
    let chat_bodies = Arc::new(Mutex::new(Vec::new()));
    let content = Arc::new(Mutex::new(String::from(r#"{"actions":[]}"#)));
    let (u, b, c) = (up.clone(), chat_bodies.clone(), content.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            let body_start = loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                data.extend_from_slice(&buf[..n]);
                if let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(p + 4);
                }
            };
            let Some(start) = body_start else { continue };
            let head = String::from_utf8_lossy(&data[..start]).to_string();
            let length = head
                .lines()
                .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                .unwrap_or(0);
            while data.len() < start + length {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            let (status, body) = if head.starts_with("GET /health") {
                if u.load(SeqCst) { (200, "{\"status\":\"ok\"}".to_owned()) } else { (503, "{}".to_owned()) }
            } else {
                b.lock().unwrap().push(serde_json::from_slice(&data[start..]).unwrap());
                let content = c.lock().unwrap().clone();
                (200, json!({"choices":[{"message":{"content":content}}]}).to_string())
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let unit = FakeUnit { up: up.clone(), starts: starts.clone(), stops: stops.clone() };
    (Fake { port, up, chat_bodies, content, starts, stops }, Box::new(unit))
}

fn engine(idle_ms: u64) -> (Fake, Arc<Engine>) {
    let (fake, unit) = spawn_server();
    let mut settings = Settings::new(fake.port, Duration::from_millis(idle_ms));
    settings.ready_timeout = Duration::from_secs(5);
    settings.request_timeout = Duration::from_secs(5);
    (fake, Arc::new(Engine::new(unit, settings)))
}

fn context() -> String {
    json!({
        "windows":[{"id":1,"app_id":"firefox","title":"Docs","workspace":1}],
        "workspaces":[{"index":1,"name":""},{"index":2,"name":""}],
        "apps":[{"id":"firefox","name":"Firefox"}],
        "home":"/tmp"
    })
    .to_string()
}

#[test]
fn interpret_starts_the_unit_and_returns_validated_actions() {
    let (fake, engine) = engine(60_000);
    *fake.content.lock().unwrap() =
        r#"{"actions":[{"action":"move_to_workspace","window":1,"workspace":2}]}"#.into();
    let result = engine.interpret("move firefox to the second workspace", &context()).unwrap();
    assert_eq!(result["summary"][0], "Move Firefox to workspace 2");
    assert_eq!(result["actions"][0]["workspace"], 2);
    assert_eq!(fake.starts.load(SeqCst), 1);
    let body = fake.chat_bodies.lock().unwrap()[0].clone();
    assert_eq!(body["temperature"], 0);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert!(body["messages"][1]["content"].as_str().unwrap().contains("Request: move firefox"));
    // A second request finds the model running and does not start it again.
    engine.interpret("again", &context()).unwrap();
    assert_eq!(fake.starts.load(SeqCst), 1);
}

#[test]
fn invalid_model_output_is_an_error_never_a_guess() {
    let (fake, engine) = engine(60_000);
    for bad in [
        r#"{"actions":[{"action":"focus","window":42}]}"#,
        r#"{"actions":[{"action":"shell","command":"id"}]}"#,
        r#"{"actions":[]}"#,
        "garbage",
    ] {
        *fake.content.lock().unwrap() = bad.into();
        let error = engine.interpret("do it", &context()).unwrap_err();
        assert!(!error.is_empty());
    }
    assert!(engine.interpret("", &context()).is_err());
    assert!(engine.interpret("x", "not json").is_err());
}

#[test]
fn naming_never_starts_the_model() {
    let (fake, engine) = engine(60_000);
    let windows = json!([{"app_id":"foot","title":"~"}]).to_string();
    assert_eq!(engine.name_workspace(&windows).unwrap_err(), "the model is not running");
    assert_eq!(fake.starts.load(SeqCst), 0);
    fake.up.store(true, SeqCst);
    *fake.content.lock().unwrap() = r#"{"name":"Terminal work"}"#.into();
    assert_eq!(engine.name_workspace(&windows).unwrap()["name"], "Terminal work");
    *fake.content.lock().unwrap() = r#"{"name":"this name is far too long to keep"}"#.into();
    assert!(engine.name_workspace(&windows).is_err());
}

#[test]
fn prewarm_starts_the_unit_in_the_background() {
    let (fake, engine) = engine(60_000);
    engine.prewarm(&context()).unwrap();
    for _ in 0..200 {
        if fake.starts.load(SeqCst) == 1 && !fake.chat_bodies.lock().unwrap().is_empty() {
            let body = fake.chat_bodies.lock().unwrap()[0].clone();
            assert_eq!(body["max_tokens"], 1, "the warm-up generates nothing");
            let user = body["messages"][1]["content"].as_str().unwrap().to_owned();
            assert!(user.contains("firefox = Firefox") && user.ends_with("Request: "));
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("prewarm did not start the unit and warm the prompt");
}

#[test]
fn idle_helper_stops_the_unit_and_returns() {
    let (fake, engine) = engine(150);
    engine.prewarm("").unwrap();
    let started = std::time::Instant::now();
    engine.run_until_idle();
    assert!(started.elapsed() >= Duration::from_millis(140));
    assert_eq!(fake.stops.load(SeqCst), 1);
    assert!(engine.prewarm("").is_err(), "a closing helper takes no more requests");
}

#[test]
fn unused_helper_still_exits() {
    let (fake, engine) = engine(100);
    engine.run_until_idle();
    assert_eq!(fake.starts.load(SeqCst), 0);
    assert_eq!(fake.stops.load(SeqCst), 1);
}

#[test]
fn a_request_in_flight_delays_the_idle_stop() {
    let (fake, engine) = engine(100);
    *fake.content.lock().unwrap() = r#"{"actions":[{"action":"switch_workspace","workspace":1}]}"#.into();
    let e = engine.clone();
    let worker = std::thread::spawn(move || e.interpret("go", &context()));
    worker.join().unwrap().unwrap();
    let started = std::time::Instant::now();
    engine.run_until_idle();
    // The idle clock restarted when the request finished.
    assert!(started.elapsed() >= Duration::from_millis(90));
    assert_eq!(fake.stops.load(SeqCst), 1);
}
