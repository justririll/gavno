use super::*;
use smithay::reexports::calloop::EventLoop;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Bus(Child, String);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn bus() -> Option<Bus> {
    let mut child = Command::new("dbus-daemon")
        .env_remove("XDG_RUNTIME_DIR")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut address = String::new();
    BufReader::new(child.stdout.take()?).read_line(&mut address).ok()?;
    Some(Bus(child, address.trim().to_owned()))
}
/// Drains the channel until the latest count equals `want` or a second passes.
fn wait_count(
    event_loop: &mut EventLoop<Vec<Event>>,
    events: &mut Vec<Event>,
    want: u32,
) -> bool {
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        event_loop
            .dispatch(Some(Duration::from_millis(20)), events)
            .unwrap();
        let last = events.iter().rev().find_map(|e| match e {
            Event::Count(n) => Some(*n),
            _ => None,
        });
        if last == Some(want) {
            return true;
        }
    }
    false
}
fn client(address: &str) -> Connection {
    Builder::address(address).unwrap().build().unwrap()
}
fn call(c: &Connection, path: &str, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> zbus::Result<zbus::Message> {
    c.call_method(Some(NAME), path, Some(NAME), method, body)
}
fn inhibit(c: &Connection, path: &str) -> zbus::Result<u32> {
    call(c, path, "Inhibit", &("app", "playing"))?
        .body()
        .deserialize()
}

struct Fixture {
    _bus: Bus,
    address: String,
    _server: Connection,
    event_loop: EventLoop<'static, Vec<Event>>,
    events: Vec<Event>,
}
fn fixture() -> Option<Fixture> {
    let bus = bus()?;
    let address = bus.1.clone();
    let (tx, rx) = channel::channel();
    let server = connect(tx, Some(&address)).unwrap();
    let event_loop = EventLoop::try_new().unwrap();
    event_loop
        .handle()
        .insert_source(rx, |event, _, events: &mut Vec<Event>| {
            if let channel::Event::Msg(event) = event {
                events.push(event);
            }
        })
        .unwrap();
    Some(Fixture {
        _bus: bus,
        address,
        _server: server,
        event_loop,
        events: Vec::new(),
    })
}

#[test]
fn cookies_are_distinct_nonzero_and_both_paths_work() {
    let Some(mut f) = fixture() else { return };
    let c = client(&f.address);
    let a = inhibit(&c, PATHS[0]).unwrap();
    let b = inhibit(&c, PATHS[1]).unwrap();
    assert!(a != 0 && b != 0 && a != b);
    assert!(wait_count(&mut f.event_loop, &mut f.events, 2));
}

#[test]
fn uninhibit_needs_the_owner() {
    let Some(mut f) = fixture() else { return };
    let owner = client(&f.address);
    let other = client(&f.address);
    let cookie = inhibit(&owner, PATHS[0]).unwrap();
    assert!(wait_count(&mut f.event_loop, &mut f.events, 1));
    call(&other, PATHS[0], "UnInhibit", &(cookie,)).unwrap();
    call(&other, PATHS[0], "UnInhibit", &(9999u32,)).unwrap();
    call(&owner, PATHS[1], "SimulateUserActivity", &()).unwrap();
    f.events.clear();
    // Activity arrives after the ignored foreign calls, and no Count(0) came first.
    let until = Instant::now() + Duration::from_secs(2);
    while !f.events.contains(&Event::Activity) && Instant::now() < until {
        f.event_loop
            .dispatch(Some(Duration::from_millis(20)), &mut f.events)
            .unwrap();
    }
    assert!(f.events.contains(&Event::Activity));
    assert!(!f.events.contains(&Event::Count(0)));
    call(&owner, PATHS[0], "UnInhibit", &(cookie,)).unwrap();
    assert!(wait_count(&mut f.event_loop, &mut f.events, 0));
}

#[test]
fn disconnect_releases_cookies() {
    let Some(mut f) = fixture() else { return };
    let c = client(&f.address);
    inhibit(&c, PATHS[0]).unwrap();
    inhibit(&c, PATHS[1]).unwrap();
    assert!(wait_count(&mut f.event_loop, &mut f.events, 2));
    drop(c);
    assert!(wait_count(&mut f.event_loop, &mut f.events, 0));
}

#[test]
fn per_sender_limit_is_enforced() {
    let Some(mut f) = fixture() else { return };
    let c = client(&f.address);
    for _ in 0..MAX_PER_SENDER {
        inhibit(&c, PATHS[0]).unwrap();
    }
    let error = inhibit(&c, PATHS[0]).unwrap_err();
    assert!(
        error.to_string().contains("LimitsExceeded"),
        "unexpected error: {error}"
    );
    // Another sender is unaffected.
    inhibit(&client(&f.address), PATHS[0]).unwrap();
    assert!(wait_count(&mut f.event_loop, &mut f.events, MAX_PER_SENDER as u32 + 1));
}

#[test]
fn taken_name_is_reported_not_replaced() {
    let Some(f) = fixture() else { return };
    let (tx, _rx) = channel::channel();
    assert!(connect(tx, Some(&f.address)).is_err());
}
