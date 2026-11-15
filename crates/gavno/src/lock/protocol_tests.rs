//! Exercise real ext-session-lock requests over an in-process socket.
use super::*;
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use std::{os::unix::net::UnixStream, sync::Arc};
use wayland_client::{protocol::wl_registry, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
#[derive(Default)]
struct Client {
    manager: Option<ExtSessionLockManagerV1>,
    events: Vec<&'static str>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "ext_session_lock_manager_v1" {
                state.manager = Some(registry.bind(name, version.min(1), qh, ()));
            }
        }
    }
}
impl Dispatch<ExtSessionLockV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => state.events.push("locked"),
            ext_session_lock_v1::Event::Finished => state.events.push("finished"),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Client: ignore ExtSessionLockManagerV1);
struct Harness {
    gv: Gavno,
    event_loop: EventLoop<'static, Gavno>,
    conn: Connection,
    queue: EventQueue<Client>,
    client: Client,
}
impl Harness {
    fn new() -> Self {
        let mut event_loop = EventLoop::try_new().unwrap();
        let mut gv = Gavno::new(
            &mut event_loop,
            Display::new().unwrap(),
            crate::config::Config::build(true, None, |_| None).0,
        );
        let (server, client) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        gv.display_handle
            .insert_client(server, Arc::new(crate::state::ClientState::default()))
            .unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let queue = conn.new_event_queue();
        conn.display().get_registry(&queue.handle(), ());
        let mut h = Self {
            gv,
            event_loop,
            conn,
            queue,
            client: Default::default(),
        };
        h.exchange();
        h.exchange();
        h
    }
    fn exchange(&mut self) {
        self.conn.flush().unwrap();
        self.event_loop
            .dispatch(Some(Duration::ZERO), &mut self.gv)
            .unwrap();
        self.gv.display_handle.flush_clients().unwrap();
        self.queue.dispatch_pending(&mut self.client).unwrap();
        if let Some(read) = self.queue.prepare_read() {
            let _ = read.read();
        }
        self.queue.dispatch_pending(&mut self.client).unwrap();
    }
}
#[test]
fn external_and_internal_locks_are_mutually_exclusive() {
    let mut h = Harness::new();
    let external = h
        .client
        .manager
        .as_ref()
        .unwrap()
        .lock(&h.queue.handle(), ());
    h.exchange();
    assert_eq!(h.client.events, ["locked"]);
    assert!(!h.gv.lock_session());
    assert!(!h.gv.lock.internal());
    external.unlock_and_destroy();
    h.exchange();
    assert!(!h.gv.protocols.locks.locked);
    assert!(h.gv.lock_session());
    let rejected = h
        .client
        .manager
        .as_ref()
        .unwrap()
        .lock(&h.queue.handle(), ());
    h.exchange();
    assert_eq!(h.client.events, ["locked", "finished"]);
    rejected.destroy();
    h.exchange();
    assert!(h.gv.protocols.locks.locked);
    assert!(h.gv.lock.internal());
    // Even a direct handler invocation must not clear the internal gate.
    smithay::wayland::session_lock::SessionLockHandler::unlock(&mut h.gv);
    assert!(h.gv.protocols.locks.locked);
}
#[test]
fn idle_timer_locks_without_a_periodic_source() {
    let mut h = Harness::new();
    h.gv.config.lock.after_idle_secs = 1;
    h.gv.refresh_idle();
    h.event_loop
        .dispatch(Some(Duration::from_millis(1100)), &mut h.gv)
        .unwrap();
    assert!(h.gv.protocols.locks.locked);
    assert!(h.gv.lock.idle_timer.is_none());
}
