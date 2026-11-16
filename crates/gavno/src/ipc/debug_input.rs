//! Opt-in live-test input. No sources or recurring work exist until a request.
mod events;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, path::PathBuf, rc::Rc, time::{Duration, Instant}};
use serde_json::{json, Value};
use smithay::{
    backend::input::{AxisSource, ButtonState, InputEvent, KeyState},
    output::Output,
    reexports::calloop::{timer::{TimeoutAction, Timer}, RegistrationToken},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};
use crate::Gavno;
use super::{protocol, transport};
use events::{Backend, Gesture};

type Completion = Box<dyn FnOnce(Result<Value, String>)>;
#[derive(Default)]
pub struct State {
    pub(super) enabled: bool,
    gesture_active: bool,
    screenshot: Option<Capture>,
}
struct Capture {
    output: Output,
    path: PathBuf,
    timeout: RegistrationToken,
    complete: Completion,
}

pub fn init(gv: &mut Gavno) {
    gv.ipc.debug_input.enabled = std::env::var("GAVNO_DEBUG_INPUT").as_deref() == Ok("1");
    if gv.ipc.debug_input.enabled {
        tracing::warn!("GAVNO_DEBUG_INPUT=1: debug input IPC is enabled");
    }
}

pub fn handles(method: &str) -> bool {
    matches!(method, "inject_gesture" | "inject_key" | "inject_pointer" | "inject_button" | "inject_scroll" | "screenshot" | "state")
}
fn access(gv: &Gavno, method: &str) -> Result<(), String> {
    if !gv.ipc.debug_input.enabled { return Err(format!("unknown method: {method}")); }
    if gv.protocols.locks.locked { return Err("session locked".into()); }
    if gv.greeter.is_some() { return Err("greeter running".into()); }
    if !gv.features.on(crate::module::Feature::Ipc) { return Err("IPC feature disabled".into()); }
    Ok(())
}
fn number(params: &Value, name: &str, default: Option<f64>) -> Result<f64, String> {
    let value = match params.get(name) {
        Some(v) => v.as_f64(),
        None => default,
    };
    value.filter(|v| v.is_finite()).ok_or_else(|| format!("{name} must be a finite number"))
}
fn integer(params: &Value, name: &str, default: u32, min: u32, max: u32) -> Result<u32, String> {
    let value = match params.get(name) {
        Some(v) => v.as_u64().and_then(|v| u32::try_from(v).ok()),
        None => Some(default),
    };
    value.filter(|v| (min..=max).contains(v)).ok_or_else(|| format!("{name} must be an integer in {min}..={max}"))
}
fn keycode(params: &Value) -> Result<u32, String> {
    let key = &params["key"];
    if let Some(code) = key.as_u64().filter(|v| *v <= 0x2ff) { return Ok(code as u32); }
    let code = match key.as_str().ok_or("key must be a key name or evdev keycode (0..=767)")? {
        "super" => 125, "ctrl" => 29, "alt" => 56, "shift" => 42,
        "return" => 28, "escape" => 1, "tab" => 15, "space" => 57,
        "backspace" => 14, "left" => 105, "right" => 106, "up" => 103, "down" => 108,
        name if name.len() == 1 && name.as_bytes()[0].is_ascii_lowercase() => {
            const LETTERS: [u32; 26] = [30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,22,47,17,45,21,44];
            LETTERS[(name.as_bytes()[0] - b'a') as usize]
        }
        "0" => 11,
        name if name.len() == 1 && (b'1'..=b'9').contains(&name.as_bytes()[0]) => u32::from(name.as_bytes()[0] - b'1') + 2,
        name if name.starts_with('f') => match name[1..].parse::<u32>() {
            Ok(n @ 1..=10) if name == format!("f{n}") => n + 58,
            Ok(11) => 87, Ok(12) => 88,
            _ => return Err("unknown key name".into()),
        },
        _ => return Err("unknown key name".into()),
    };
    Ok(code)
}

pub fn dispatch(gv: &mut Gavno, method: &str, params: &Value) -> Result<Value, String> {
    access(gv, method)?;
    let time = gv.start_time.elapsed().as_micros() as u64;
    match method {
        "state" => return Ok(state(gv)),
        "inject_key" => {
            let code = keycode(params)?;
            let states: &[KeyState] = match params["state"].as_str() {
                Some("press") => &[KeyState::Pressed],
                Some("release") => &[KeyState::Released],
                Some("tap") => &[KeyState::Pressed, KeyState::Released],
                _ => return Err("state must be press, release or tap".into()),
            };
            for &state in states { gv.process_input_event::<Backend>(InputEvent::Keyboard { event: events::Key { time, code, state } }); }
        }
        "inject_pointer" => {
            let x = number(params, "x", None)?;
            let y = number(params, "y", None)?;
            let origin = gv.space.outputs().next().and_then(|o| gv.space.output_geometry(o)).ok_or("no output")?.loc;
            gv.process_input_event::<Backend>(InputEvent::PointerMotionAbsolute { event: events::Position {
                time, x: x - f64::from(origin.x), y: y - f64::from(origin.y),
            } });
        }
        "inject_button" => {
            let code = match params["button"].as_str() {
                Some("left") => 0x110, Some("right") => 0x111, Some("middle") => 0x112,
                _ => return Err("button must be left, right or middle".into()),
            };
            let states: &[ButtonState] = match params["state"].as_str() {
                Some("press") => &[ButtonState::Pressed],
                Some("release") => &[ButtonState::Released],
                Some("click") => &[ButtonState::Pressed, ButtonState::Released],
                _ => return Err("state must be press, release or click".into()),
            };
            for &state in states { gv.process_input_event::<Backend>(InputEvent::PointerButton { event: events::Button { time, code, state } }); }
        }
        "inject_scroll" => {
            let dx = number(params, "dx", Some(0.0))?;
            let dy = number(params, "dy", Some(0.0))?;
            let source = match params["source"].as_str() {
                Some("finger") => AxisSource::Finger, Some("wheel") => AxisSource::Wheel,
                _ => return Err("source must be finger or wheel".into()),
            };
            gv.process_input_event::<Backend>(InputEvent::PointerAxis { event: events::Scroll { time, dx, dy, source } });
        }
        _ => return Err(format!("{method} requires asynchronous IPC")),
    }
    Ok(json!({"ok": true}))
}

fn state(gv: &Gavno) -> Value {
    let focused = gv.focused_window().and_then(|w| w.toplevel().map(|t| with_states(t.wl_surface(), |s| {
        let attrs = s.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap();
        json!({"title": attrs.title, "app_id": attrs.app_id})
    })));
    let outputs: Vec<_> = gv.space.outputs().map(|o| json!({"output": o.name(),
        "workspace": gv.workspaces.get(o).map(|w| w.list.active).unwrap_or(0)})).collect();
    let pos = gv.pointer_location();
    let mods = gv.seat.get_keyboard().unwrap().modifier_state();
    json!({"outputs_powered_on": !gv.power.is_off(), "rendered_frames": gv.power.rendered_frames,
        "power_frame_timer_firings": gv.power.frame_timer_firings, "shell_mode": gv.classic.mode, "shell_progress": gv.shell_progress(), "settled": gv.classic.settled,
        "overview_open": gv.overview.open(), "outputs": outputs, "focused": focused,
        "launcher_open": gv.launcher.open, "start_open": gv.classic.start.open,
        "music_open": gv.music_open(), "polkit_open": gv.polkit_visible(),
        "pointer": {"x": pos.x, "y": pos.y},
        "modifiers": {"super": mods.logo, "ctrl": mods.ctrl, "alt": mods.alt, "shift": mods.shift,
            "caps_lock": mods.caps_lock, "num_lock": mods.num_lock}})
}

#[derive(Clone, Copy)]
enum Kind { Swipe, Pinch, Hold }
#[derive(Clone, Copy)]
struct GestureSpec { kind: Kind, event: Gesture, duration: Duration, steps: u32 }
impl GestureSpec {
    fn parse(params: &Value) -> Result<Self, String> {
        let kind = match params["kind"].as_str() {
            Some("swipe") => Kind::Swipe, Some("pinch") => Kind::Pinch, Some("hold") => Kind::Hold,
            _ => return Err("kind must be swipe, pinch or hold".into()),
        };
        let duration = integer(params, "duration_ms", 250, 1, 60_000)?;
        let steps = integer(params, "steps", (duration / 8).max(1), 1, 10_000)?;
        let cancelled = match params.get("cancel") {
            None => false, Some(v) => v.as_bool().ok_or("cancel must be a boolean")?,
        };
        let scale = number(params, "scale", Some(1.0))?;
        if scale <= 0.0 { return Err("scale must be positive".into()); }
        Ok(Self { kind, duration: Duration::from_millis(u64::from(duration)), steps,
            event: Gesture { time: 0, fingers: integer(params, "fingers", 3, 2, 5)?,
                dx: number(params, "dx", Some(0.0))?, dy: number(params, "dy", Some(0.0))?, scale,
                rotation: number(params, "rotation", Some(0.0))?, cancelled } })
    }
    fn begin(&self, gv: &mut Gavno) {
        let event = self.event;
        gv.process_input_event::<Backend>(match self.kind {
            Kind::Swipe => InputEvent::GestureSwipeBegin { event },
            Kind::Pinch => InputEvent::GesturePinchBegin { event },
            Kind::Hold => InputEvent::GestureHoldBegin { event },
        });
    }
    fn update(&self, gv: &mut Gavno, step: u32) {
        let n = f64::from(self.steps);
        let event = Gesture { time: self.event.time + (self.duration.as_micros() * u128::from(step) / u128::from(self.steps)) as u64,
            dx: self.event.dx / n, dy: self.event.dy / n,
            scale: 1.0 + (self.event.scale - 1.0) * f64::from(step) / n,
            rotation: self.event.rotation / n, ..self.event };
        // The Wayland/libinput hold protocol has no update event.
        match self.kind {
            Kind::Swipe => gv.process_input_event::<Backend>(InputEvent::GestureSwipeUpdate { event }),
            Kind::Pinch => gv.process_input_event::<Backend>(InputEvent::GesturePinchUpdate { event }),
            Kind::Hold => {},
        }
    }
    fn end(&self, gv: &mut Gavno) {
        let event = Gesture { time: self.event.time + self.duration.as_micros() as u64, ..self.event };
        gv.process_input_event::<Backend>(match self.kind {
            Kind::Swipe => InputEvent::GestureSwipeEnd { event },
            Kind::Pinch => InputEvent::GesturePinchEnd { event },
            Kind::Hold => InputEvent::GestureHoldEnd { event },
        });
    }
}

fn start_gesture(gv: &mut Gavno, mut spec: GestureSpec, complete: Completion) {
    if gv.ipc.debug_input.gesture_active { complete(Err("injected gesture already pending".into())); return; }
    let start = Instant::now();
    let first = start + spec.duration / spec.steps;
    let pending = Rc::new(RefCell::new(Some(complete)));
    let callback = pending.clone();
    let mut step = 0;
    // Register before begin so registration failure has no input side effects.
    // Scheduled event timestamps preserve hardware-style velocity even when
    // rendering delays delivery of one timer callback.
    spec.event.time = gv.start_time.elapsed().as_micros() as u64;
    let begin = spec;
    let registration = gv.loop_handle.insert_source(Timer::from_deadline(first), move |_, _, gv| {
        if let Err(error) = access(gv, "inject_gesture") {
            // Do not send even an end event to a lock/greeter surface. Discard
            // synthetic interactions so they cannot resume after unlocking.
            gv.gestures = Default::default();
            gv.cancel_window_gestures();
            for o in &mut gv.workspaces.outputs { o.held = false; o.position.set(o.list.active as f64); }
            gv.overview.held = false;
            gv.overview.progress.set(0.0);
            gv.classic.held = false;
            gv.classic.progress.set(gv.classic.mode.progress());
            gv.ipc.debug_input.gesture_active = false;
            if let Some(complete) = callback.borrow_mut().take() { complete(Err(error)); }
            return TimeoutAction::Drop;
        }
        step += 1;
        spec.update(gv, step);
        if step == spec.steps {
            spec.end(gv);
            gv.ipc.debug_input.gesture_active = false;
            if let Some(complete) = callback.borrow_mut().take() { complete(Ok(json!({"ok": true, "cancelled": spec.event.cancelled}))); }
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToInstant(start + spec.duration.mul_f64(f64::from(step + 1) / f64::from(spec.steps)))
        }
    });
    match registration {
        Ok(_) => {
            gv.ipc.debug_input.gesture_active = true;
            begin.begin(gv);
        }
        Err(error) => if let Some(complete) = pending.borrow_mut().take() { complete(Err(format!("cannot register gesture timer: {error}"))); },
    }
}

pub fn request(line: &[u8], gv: &mut Gavno, reply: transport::Reply<Gavno>) {
    enum Deferred { Gesture(GestureSpec), Screenshot(PathBuf) }
    let mut deferred = None;
    let response = protocol::reply(line, |method, params| {
        access(gv, method)?;
        deferred = Some(match method {
            "inject_gesture" => Deferred::Gesture(GestureSpec::parse(params)?),
            "screenshot" => Deferred::Screenshot(match params.get("path") {
                Some(v) => PathBuf::from(v.as_str().filter(|v| !v.is_empty()).ok_or("path must be a nonempty string")?),
                None => crate::debug::shot_path(),
            }),
            _ => return dispatch(gv, method, params),
        });
        Ok(Value::Null)
    });
    let Some(deferred) = deferred else { reply.send(response); return; };
    let id = serde_json::from_slice::<Value>(line).unwrap()["id"].clone();
    let complete = Box::new(move |result| reply.send(protocol::response(id, result)));
    match deferred {
        Deferred::Gesture(spec) => start_gesture(gv, spec, complete),
        Deferred::Screenshot(path) => start_capture(gv, path, complete),
    }
}

fn start_capture(gv: &mut Gavno, path: PathBuf, complete: Completion) {
    if gv.ipc.debug_input.screenshot.is_some() { complete(Err("screenshot already pending".into())); return; }
    let Some(output) = gv.space.outputs().next().cloned() else { complete(Err("no output".into())); return; };
    let timeout = gv.loop_handle.insert_source(Timer::from_duration(Duration::from_secs(5)), |_, _, gv| {
        if let Some(pending) = gv.ipc.debug_input.screenshot.take() {
            gv.protocols.locks.after_frame = None;
            let error = access(gv, "screenshot").err().unwrap_or_else(|| "timed out waiting for a rendered frame".into());
            (pending.complete)(Err(error));
        }
        TimeoutAction::Drop
    });
    let timeout = match timeout { Ok(t) => t, Err(e) => { complete(Err(e.to_string())); return; } };
    let handle = gv.loop_handle.clone();
    let rendered = output.clone();
    gv.protocols.locks.after_frame = Some((output.clone(), Box::new(move || {
        // The notification runs inside backend rendering; read pixels only once
        // its mutable renderer borrow has ended, on the same calloop turn.
        handle.insert_idle(move |gv| frame_drawn(gv, &rendered));
    })));
    gv.ipc.debug_input.screenshot = Some(Capture { output, path, timeout, complete });
    gv.queue_redraw_all();
}

/// Both backends notify here after drawing; capture uses the SIGUSR1 encoder.
pub fn frame_drawn(gv: &mut Gavno, output: &Output) {
    if !gv.ipc.debug_input.screenshot.as_ref().is_some_and(|s| &s.output == output) { return; }
    let pending = gv.ipc.debug_input.screenshot.take().unwrap();
    gv.loop_handle.remove(pending.timeout);
    gv.protocols.locks.after_frame = None;
    let result = access(gv, "screenshot").and_then(|()| {
        let (w, h, rgba) = crate::backend::screenshot(gv).ok_or("screenshot failed")?;
        if w < 2 || h < 2 { return Err("output is too small".into()); }
        crate::debug::write_png_half(&pending.path, w, h, &rgba).map_err(|e| e.to_string())?;
        Ok(json!({"path": pending.path, "width": w / 2, "height": h / 2}))
    });
    (pending.complete)(result);
}
