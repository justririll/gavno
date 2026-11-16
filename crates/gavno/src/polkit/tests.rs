use super::{helper::tests::{FakeHelper, ACCEPTS_SECRET}, *};
use crate::state::tests::Harness;
use smithay::input::keyboard::{keysyms, Keysym, ModifiersState};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
use zbus::zvariant::OwnedValue;

/// Queues a dialog as if polkitd had asked; the receiver gets its outcome.
pub(crate) fn push_dialog(gv: &mut Gavno, cookie: &str, users: &[&str]) -> async_channel::Receiver<Outcome> {
    let (reply, outcome) = async_channel::bounded(1);
    gv.polkit_request(Request::Begin(Begin {
        cookie: cookie.into(),
        message: "Authentication is needed to run a test as the super user".into(),
        icon: String::new(),
        users: users.iter().map(|u| u.to_string()).collect(),
        selected: 0,
        reply,
    }));
    outcome
}

fn key(gv: &mut Gavno, raw: u32, ch: Option<char>) {
    gv.polkit_key(Keysym::from(raw), ch, &ModifiersState::default());
}

fn type_text(gv: &mut Gavno, text: &str) {
    for c in text.chars() {
        key(gv, keysyms::KEY_a, Some(c));
    }
}

/// Runs the event loop until `done` holds.
fn pump(h: &mut Harness, mut done: impl FnMut(&mut Gavno) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(&mut h.gv) {
        assert!(Instant::now() < deadline, "timed out waiting for the polkit agent");
        h.event_loop.dispatch(Some(Duration::from_millis(10)), &mut h.gv).unwrap();
    }
}

fn outcome(receiver: &async_channel::Receiver<Outcome>) -> Option<&'static str> {
    receiver.try_recv().ok().map(|o| match o {
        Outcome::Success => "success",
        Outcome::Cancelled => "cancelled",
        Outcome::Failed(_) => "failed",
    })
}

#[test]
fn dialog_types_submits_retries_and_cancels() {
    let mut h = Harness::new();
    let fake = FakeHelper::new(ACCEPTS_SECRET);
    h.gv.polkit.helper_override = Some(fake.0.clone());

    let first = push_dialog(&mut h.gv, "one", &["admin"]);
    let second = push_dialog(&mut h.gv, "two", &["admin", "root"]);
    let duplicate = push_dialog(&mut h.gv, "one", &["admin"]);
    assert_eq!(outcome(&duplicate), Some("failed"));
    assert_eq!(h.gv.polkit.queue.len(), 2);
    assert!(h.gv.polkit_visible());

    // Typing, editing and an empty submit.
    key(&mut h.gv, keysyms::KEY_Return, None);
    assert!(!h.gv.polkit.queue[0].busy());
    type_text(&mut h.gv, "xyz");
    key(&mut h.gv, keysyms::KEY_BackSpace, None);
    assert_eq!(h.gv.polkit.queue[0].password.as_str(), "xy");
    h.gv.polkit_key(Keysym::from(keysyms::KEY_u), Some('u'), &ModifiersState { ctrl: true, ..Default::default() });
    assert!(h.gv.polkit.queue[0].password.is_empty());

    // Wrong password: retry with a cleared field and an error line.
    type_text(&mut h.gv, "wrong");
    key(&mut h.gv, keysyms::KEY_Return, None);
    assert!(h.gv.polkit.queue[0].busy());
    assert!(h.gv.polkit.queue[0].password.is_empty());
    type_text(&mut h.gv, "ignored while busy");
    assert!(h.gv.polkit.queue[0].password.is_empty());
    pump(&mut h, |gv| !gv.polkit.queue[0].busy());
    assert_eq!(h.gv.polkit.queue[0].attempts, 1);
    assert!(h.gv.polkit.queue[0].error.is_some());
    assert_eq!(outcome(&first), None);

    // Right password finishes the front request; the next one shows.
    type_text(&mut h.gv, "secret with spaces");
    key(&mut h.gv, keysyms::KEY_KP_Enter, None);
    pump(&mut h, |gv| gv.polkit.queue.len() == 1);
    assert_eq!(outcome(&first), Some("success"));
    assert_eq!(h.gv.polkit.queue[0].begin.cookie, "two");

    // User selection wraps and clears the field.
    type_text(&mut h.gv, "abc");
    key(&mut h.gv, keysyms::KEY_Tab, None);
    assert_eq!(h.gv.polkit.queue[0].user(), "root");
    assert!(h.gv.polkit.queue[0].password.is_empty());
    key(&mut h.gv, keysyms::KEY_Up, None);
    assert_eq!(h.gv.polkit.queue[0].user(), "admin");

    // Hidden while locked; the request keeps waiting.
    h.gv.protocols.locks.locked = true;
    assert!(!h.gv.polkit_visible());
    h.gv.protocols.locks.locked = false;
    assert!(h.gv.polkit_visible());

    key(&mut h.gv, keysyms::KEY_Escape, None);
    assert_eq!(outcome(&second), Some("cancelled"));
    assert!(h.gv.polkit.queue.is_empty());
    assert!(!h.gv.polkit_visible());
}

#[test]
fn three_wrong_passwords_fail_and_cancel_closes_by_cookie() {
    let mut h = Harness::new();
    let fake = FakeHelper::new(ACCEPTS_SECRET);
    h.gv.polkit.helper_override = Some(fake.0.clone());
    let failing = push_dialog(&mut h.gv, "fail", &["admin"]);
    for attempt in 1..=MAX_ATTEMPTS {
        type_text(&mut h.gv, "nope");
        key(&mut h.gv, keysyms::KEY_Return, None);
        pump(&mut h, |gv| gv.polkit.queue.front().is_none_or(|d| !d.busy()));
        if attempt < MAX_ATTEMPTS {
            assert_eq!(h.gv.polkit.queue[0].attempts, attempt);
        }
    }
    assert_eq!(outcome(&failing), Some("failed"));
    assert!(h.gv.polkit.queue.is_empty());

    let waiting = push_dialog(&mut h.gv, "a", &["admin"]);
    let cancelled = push_dialog(&mut h.gv, "b", &["admin"]);
    h.gv.polkit_request(Request::Cancel("b".into()));
    assert_eq!(outcome(&cancelled), Some("cancelled"));
    h.gv.polkit_request(Request::Cancel("unknown".into()));
    assert_eq!(h.gv.polkit.queue.len(), 1);
    // Cancelling a running helper kills it and answers at once.
    type_text(&mut h.gv, "secret with spaces");
    key(&mut h.gv, keysyms::KEY_Return, None);
    h.gv.polkit_request(Request::Cancel("a".into()));
    assert_eq!(outcome(&waiting), Some("cancelled"));
    assert!(h.gv.polkit.queue.is_empty());
    // The stale helper result is ignored.
    h.event_loop.dispatch(Some(Duration::from_millis(200)), &mut h.gv).unwrap();
    assert!(h.gv.polkit.queue.is_empty());
}

#[test]
fn feature_panic_cancels_requests_and_frees_the_keyboard() {
    let mut h = Harness::new();
    let pending = push_dialog(&mut h.gv, "panic", &["admin"]);
    type_text(&mut h.gv, "secret");
    h.gv.features.inject_panic(Feature::Polkit);
    key(&mut h.gv, keysyms::KEY_a, Some('a'));
    h.gv.run_feature_resets();
    assert_eq!(outcome(&pending), Some("cancelled"));
    assert!(h.gv.polkit.queue.is_empty());
    assert!(!h.gv.polkit_visible());
}

struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Default, Clone)]
struct FakeAuthority {
    /// (sender, session id, object path) of each registration.
    registered: std::sync::Arc<Mutex<Vec<(String, String, String)>>>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl FakeAuthority {
    fn register_authentication_agent(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        subject: (String, HashMap<String, OwnedValue>),
        _locale: String,
        path: String,
    ) {
        let session = subject.1.get("session-id").and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        self.registered.lock().unwrap().push((sender, session, path));
    }
    fn unregister_authentication_agent(&self, _subject: (String, HashMap<String, OwnedValue>), _path: String) {}
}

#[test]
fn dbus_agent_registers_and_answers_begin_authentication() {
    let Ok(child) = Command::new("dbus-daemon")
        .env_remove("XDG_RUNTIME_DIR")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
    else {
        eprintln!("skipping polkit bus test: dbus-daemon unavailable");
        return;
    };
    let mut bus = Bus(child);
    let mut address = String::new();
    BufReader::new(bus.0.stdout.take().unwrap()).read_line(&mut address).unwrap();
    let address = address.trim().to_owned();
    let authority = FakeAuthority::default();
    let _polkitd = zbus::blocking::connection::Builder::address(address.as_str())
        .unwrap()
        .name(dbus::NAME)
        .unwrap()
        .serve_at(dbus::AUTHORITY, authority.clone())
        .unwrap()
        .build()
        .unwrap();

    let mut h = Harness::new();
    let fake = FakeHelper::new(ACCEPTS_SECRET);
    h.gv.polkit.helper_override = Some(fake.0.clone());
    h.gv.start_polkit(Target { address: Some(address.clone()), session: Some("c7".into()), process: None });
    let deadline = Instant::now() + Duration::from_secs(10);
    while authority.registered.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "agent did not register");
        std::thread::sleep(Duration::from_millis(10));
    }
    let (agent, session, path) = authority.registered.lock().unwrap()[0].clone();
    assert_eq!((session.as_str(), path.as_str()), ("c7", dbus::PATH));

    let call = |cookie: &'static str| {
        let address = address.clone();
        let agent = agent.clone();
        std::thread::spawn(move || {
            let client = zbus::blocking::connection::Builder::address(address.as_str()).unwrap().build().unwrap();
            let uid = unsafe { libc::getuid() };
            let identities: Vec<(&str, HashMap<&str, zbus::zvariant::Value>)> =
                vec![("unix-user", HashMap::from([("uid", zbus::zvariant::Value::from(uid))]))];
            let result = client.call_method(
                Some(agent.as_str()),
                dbus::PATH,
                Some("org.freedesktop.PolicyKit1.AuthenticationAgent"),
                "BeginAuthentication",
                &("org.test.action", "Test needs a password", "", HashMap::<&str, &str>::new(), cookie, identities),
            );
            match result {
                Ok(_) => "ok".to_owned(),
                Err(zbus::Error::MethodError(name, _, _)) => name.as_str().to_owned(),
                Err(error) => error.to_string(),
            }
        })
    };

    let success = call("ok-cookie");
    pump(&mut h, |gv| !gv.polkit.queue.is_empty());
    assert_eq!(h.gv.polkit.queue[0].user(), helper::user_name(unsafe { libc::getuid() }).unwrap());
    type_text(&mut h.gv, "secret with spaces");
    key(&mut h.gv, keysyms::KEY_Return, None);
    pump(&mut h, |gv| gv.polkit.queue.is_empty());
    assert_eq!(success.join().unwrap(), "ok");

    let escaped = call("esc-cookie");
    pump(&mut h, |gv| !gv.polkit.queue.is_empty());
    key(&mut h.gv, keysyms::KEY_Escape, None);
    assert_eq!(escaped.join().unwrap(), "org.freedesktop.PolicyKit1.Error.Cancelled");

    let remote = call("remote-cookie");
    pump(&mut h, |gv| !gv.polkit.queue.is_empty());
    let client = zbus::blocking::connection::Builder::address(address.as_str()).unwrap().build().unwrap();
    client
        .call_method(Some(agent.as_str()), dbus::PATH, Some("org.freedesktop.PolicyKit1.AuthenticationAgent"), "CancelAuthentication", &("remote-cookie",))
        .unwrap();
    pump(&mut h, |gv| gv.polkit.queue.is_empty());
    assert_eq!(remote.join().unwrap(), "org.freedesktop.PolicyKit1.Error.Cancelled");

    // Stopping the feature unregisters and ends the worker.
    h.gv.reset_polkit();
}

#[test]
fn process_start_time_parses_stat_with_spaces_in_comm() {
    let own = dbus::process_start_time(std::process::id()).unwrap();
    assert!(own > 0);
    assert_eq!(dbus::process_start_time(u32::MAX), None);
}
