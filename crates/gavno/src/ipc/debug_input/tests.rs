//! Exercise actual JSON IPC, timers, XKB and the production input handlers.
use super::*;
use std::{io::{Read, Write}, os::unix::net::{UnixListener, UnixStream}, sync::atomic::{AtomicU64, Ordering}};
use smithay::output::{Mode, PhysicalProperties, Subpixel};

struct Live {
    h: crate::state::tests::Harness,
    output: Output,
    client: UnixStream,
    listener: transport::Listener,
    root: PathBuf,
}
impl Live {
    fn new(enabled: bool, overview: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut h = crate::state::tests::Harness::new();
        h.gv.ipc.debug_input.enabled = enabled;
        h.gv.config.mod_key = crate::config::ModKey::Super;
        h.gv.config.modules.overview = overview;
        h.gv.features = crate::module::Features::new(&h.gv.config);
        let output = Output::new("debug-test".into(), PhysicalProperties {
            size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "test".into(), model: "test".into(),
        });
        output.change_current_state(Some(Mode { size: (1600, 900).into(), refresh: 60_000 }), None, None, None);
        h.gv.space.map_output(&output, (100, 50));
        h.gv.workspaces.ensure(&output);
        let root = std::env::temp_dir().join(format!("gavno-debug-input-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ipc.sock");
        let listener = transport::listen_async(h.gv.loop_handle.clone(), UnixListener::bind(&path).unwrap(), super::super::request_async).unwrap();
        let client = UnixStream::connect(path).unwrap();
        client.set_nonblocking(true).unwrap();
        Self { h, output, client, listener, root }
    }
    fn send(&mut self, method: &str, params: Value) {
        writeln!(self.client, "{}", json!({"id": 19, "method": method, "params": params})).unwrap();
    }
    fn receive(&mut self) -> Value {
        let start = Instant::now();
        let mut response = Vec::new();
        while !response.contains(&b'\n') {
            assert!(start.elapsed() < Duration::from_secs(6), "IPC response timed out");
            self.h.event_loop.dispatch(Duration::from_millis(1), &mut self.h.gv).unwrap();
            let mut bytes = [0; 8192];
            match self.client.read(&mut bytes) {
                Ok(0) => panic!("IPC closed before reply"),
                Ok(n) => response.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                Err(e) => panic!("IPC read: {e}"),
            }
        }
        let response: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["id"], 19);
        response
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.send(method, params);
        let reply = self.receive();
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
    fn key(&mut self, key: Value, state: &str) { self.call("inject_key", json!({"key":key,"state":state})); }
    fn swipe(&mut self, dx: f64, dy: f64, cancel: bool) -> Value {
        self.call("inject_gesture", json!({"kind":"swipe","fingers":3,"dx":dx,"dy":dy,"duration_ms":64,"steps":8,"cancel":cancel}))
    }
    fn settle(&mut self) {
        let gv = &mut self.h.gv;
        gv.classic.progress.jump(gv.classic.mode.progress());
        gv.settle_classic();
        let o = gv.workspaces.ensure(&self.output);
        o.position.jump(o.list.active as f64);
        gv.overview.progress.jump(0.0);
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        self.h.gv.loop_handle.remove(self.listener.token);
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn injected_swipe_uses_timers_and_switches_workspace_only_on_end() {
    let mut live = Live::new(true, true);
    let window = live.h.window();
    live.h.gv.map_new_window(window);
    live.send("inject_gesture", json!({"kind":"swipe","fingers":3,"dx":-240,"duration_ms":64,"steps":8}));
    let start = Instant::now();
    while !live.h.gv.ipc.debug_input.gesture_active {
        assert!(start.elapsed() < Duration::from_secs(1));
        live.h.event_loop.dispatch(Duration::from_millis(1), &mut live.h.gv).unwrap();
    }
    assert_eq!(live.h.gv.workspaces.get(&live.output).unwrap().list.active, 0);
    assert_eq!(live.client.read(&mut [0; 1]).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(live.receive()["result"]["cancelled"], false);
    assert!(start.elapsed() >= Duration::from_millis(60));
    assert!(live.h.gv.workspaces.get(&live.output).unwrap().list.active > 0);
    assert!(!live.h.gv.ipc.debug_input.gesture_active);
}

#[test]
fn held_super_routes_vertical_swipe_to_classic_and_back() {
    let mut live = Live::new(true, false);
    live.swipe(0.0, -340.0, false);
    assert_eq!(live.h.gv.classic.mode, crate::classic::ShellMode::Gavno);
    live.key(json!("super"), "press");
    assert_eq!(live.call("state", json!({}))["modifiers"]["super"], true);
    live.swipe(0.0, -340.0, false);
    live.key(json!("super"), "release");
    assert_eq!(live.h.gv.classic.mode, crate::classic::ShellMode::Classic);
    assert!(!live.h.gv.launcher.open && !live.h.gv.classic.start.open);
    live.settle();
    live.swipe(0.0, 340.0, false);
    assert_eq!(live.h.gv.classic.mode, crate::classic::ShellMode::Classic);
    live.settle();
    live.key(json!(125), "press");
    live.swipe(0.0, 340.0, false);
    live.key(json!(125), "release");
    assert_eq!(live.h.gv.classic.mode, crate::classic::ShellMode::Gavno);
    assert_eq!(live.call("state", json!({}))["modifiers"]["super"], false);
}

#[test]
fn injected_super_tap_opens_launcher_and_classic_start_and_keys_reach_clients() {
    let mut live = Live::new(true, false);
    let window = live.h.window();
    live.h.gv.map_new_window(window.clone());
    live.h.gv.focus_window(Some(&window));
    live.h.keyboard_keys();
    live.key(json!("a"), "tap");
    assert_eq!(live.h.keyboard_keys(), vec![30, 30]);
    live.key(json!("super"), "tap");
    assert!(live.h.gv.launcher.open);
    assert!(!live.h.gv.seat.get_keyboard().unwrap().modifier_state().logo);
    live.h.gv.close_launcher();
    live.h.gv.set_shell_mode(crate::classic::ShellMode::Classic);
    live.settle();
    live.key(json!("super"), "tap");
    assert!(live.h.gv.classic.start.open);
}

#[test]
fn pointer_coordinates_and_click_focus_the_window_under_the_pointer() {
    let mut live = Live::new(true, true);
    let a = live.h.window(); let b = live.h.window();
    for (window, x) in [(&a, 200), (&b, 800)] {
        live.h.gv.map_new_window(window.clone());
        live.h.commit_size(window, 400, 300);
        live.h.gv.space.map_element(window.clone(), (x, 200), false);
    }
    live.h.gv.window_animations.entries.clear();
    live.h.gv.focus_window(Some(&b));
    live.call("inject_pointer", json!({"x":300.0,"y":300.0}));
    live.call("inject_button", json!({"button":"left","state":"click"}));
    assert_eq!(live.h.gv.focused_window(), Some(a));
    assert_eq!(live.h.pointer_buttons(), vec![0x110, 0x110]);
    let state = live.call("state", json!({}));
    assert_eq!(state["pointer"], json!({"x":300.0,"y":300.0}));
    assert_eq!(state["focused"]["title"], Value::Null);
    assert_eq!(state["outputs"][0]["workspace"], 0);
    assert!(live.h.gv.pointer_buttons.is_empty());
    for source in ["wheel", "finger"] { live.call("inject_scroll", json!({"dx":0,"dy":12,"source":source})); }
}

#[test]
fn debug_methods_are_unknown_without_opt_in_and_refused_while_locked_or_greeter() {
    let mut live = Live::new(false, true);
    let methods = ["inject_gesture", "inject_key", "inject_pointer", "inject_button", "inject_scroll", "screenshot", "state"];
    for method in methods {
        live.send(method, json!({}));
        assert_eq!(live.receive()["error"], format!("unknown method: {method}"));
    }
    live.h.gv.ipc.debug_input.enabled = true;
    live.h.gv.protocols.locks.locked = true;
    for method in methods {
        live.send(method, json!({}));
        assert_eq!(live.receive()["error"], "session locked");
    }
    live.h.gv.protocols.locks.locked = false;
    let (tx, _) = std::sync::mpsc::channel();
    live.h.gv.greeter = Some(crate::greeter::Greeter::for_lock(tx, crate::greeter::User { name: "test".into(), display: "Test".into() }));
    for method in methods {
        live.send(method, json!({}));
        assert_eq!(live.receive()["error"], "greeter running");
    }
    assert!(!live.h.gv.ipc.debug_input.gesture_active);
}

#[test]
fn cancellation_and_lock_mid_gesture_leave_no_synthetic_interaction() {
    let mut live = Live::new(true, false);
    assert_eq!(live.swipe(-400.0, 0.0, true)["cancelled"], true);
    assert_eq!(live.h.gv.workspaces.get(&live.output).unwrap().list.active, 0);
    live.settle();
    live.key(json!("super"), "press");
    live.swipe(0.0, -340.0, true);
    live.key(json!("super"), "release");
    assert_eq!(live.h.gv.classic.mode, crate::classic::ShellMode::Gavno);
    live.send("inject_gesture", json!({"kind":"swipe","fingers":3,"dx":-400,"duration_ms":80}));
    while !live.h.gv.ipc.debug_input.gesture_active {
        live.h.event_loop.dispatch(Duration::from_millis(1), &mut live.h.gv).unwrap();
    }
    live.h.gv.protocols.locks.locked = true;
    assert_eq!(live.receive()["error"], "session locked");
    assert!(!live.h.gv.ipc.debug_input.gesture_active);
    assert!(!live.h.gv.workspaces.get(&live.output).unwrap().held);
}

#[test]
fn invalid_parameters_have_no_input_side_effects_and_hold_and_pinch_end() {
    let mut live = Live::new(true, false);
    for (method, params) in [
        ("inject_gesture", json!({"kind":"swipe","fingers":1})),
        ("inject_gesture", json!({"kind":"pinch","scale":0})),
        ("inject_gesture", json!({"kind":"swipe","steps":0})),
        ("inject_gesture", json!({"kind":"swipe","duration_ms":60001})),
        ("inject_gesture", json!({"kind":"swipe","cancel":1})),
        ("inject_key", json!({"key":-1,"state":"tap"})),
        ("inject_key", json!({"key":768,"state":"tap"})),
        ("inject_key", json!({"key":"super","state":"click"})),
        ("inject_pointer", json!({"x":null,"y":0})),
        ("inject_button", json!({"button":"left","state":"tap"})),
        ("inject_scroll", json!({"source":"unknown"})),
        ("screenshot", json!({"path":""})),
    ] {
        live.send(method, params);
        assert!(live.receive()["error"].is_string());
    }
    assert!(!live.h.gv.ipc.debug_input.gesture_active);
    assert!(!live.h.gv.seat.get_keyboard().unwrap().modifier_state().logo);
    for kind in ["hold", "pinch"] {
        live.call("inject_gesture", json!({"kind":kind,"fingers":2,"scale":1.2,"rotation":30,"dx":4,"dy":2,"duration_ms":16,"steps":2}));
    }
    for name in ["super","ctrl","alt","shift","return","escape","tab","space","backspace","left","right","up","down","a","z","0","9","f1","f10","f11","f12"] {
        assert!(keycode(&json!({"key":name})).is_ok(), "{name}");
    }
}

#[test]
fn pending_screenshot_is_refused_if_locked_before_the_next_frame() {
    let mut live = Live::new(true, true);
    live.send("screenshot", json!({"path":live.root.join("shot.png")}));
    while live.h.gv.ipc.debug_input.screenshot.is_none() {
        live.h.event_loop.dispatch(Duration::from_millis(1), &mut live.h.gv).unwrap();
    }
    assert_eq!(live.client.read(&mut [0; 1]).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    live.h.gv.protocols.locks.locked = true;
    live.h.gv.protocols.locks.submitted(&live.output);
    assert_eq!(live.receive()["error"], "session locked");
    assert!(!live.root.join("shot.png").exists());
    assert!(live.h.gv.ipc.debug_input.screenshot.is_none());
}
