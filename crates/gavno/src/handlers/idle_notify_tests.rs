//! Test Smithay's actual one-shot timers without a compositor or renderer.
use smithay::{
    input::{Seat, SeatHandler, SeatState},
    reexports::{
        calloop::EventLoop,
        wayland_server::{protocol::wl_surface::WlSurface, Display},
    },
    wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState},
};
use std::{os::unix::net::UnixStream, sync::Arc, time::Duration};
use wayland_client::{
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

struct Server {
    compositor: smithay::wayland::compositor::CompositorState,
    idle: IdleNotifierState<Self>,
    seats: SeatState<Self>,
    seat: Seat<Self>,
}
impl smithay::wayland::compositor::CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut smithay::wayland::compositor::CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a smithay::wayland::compositor::CompositorClientState {
        &client
            .get_data::<crate::state::ClientState>()
            .unwrap()
            .compositor_state
    }
    fn commit(&mut self, _: &WlSurface) {}
}
smithay::delegate_compositor!(Server);
impl IdleNotifierHandler for Server {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle
    }
}
impl SeatHandler for Server {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }
}
smithay::delegate_idle_notify!(Server);
smithay::delegate_seat!(Server);

#[derive(Default)]
struct Client {
    notifier: Option<ExtIdleNotifierV1>,
    seat: Option<wl_seat::WlSeat>,
    events: Vec<(u32, &'static str)>,
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
            match interface.as_str() {
                "ext_idle_notifier_v1" => {
                    state.notifier = Some(registry.bind(name, version.min(2), qh, ()))
                }
                "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<ExtIdleNotificationV1, u32> for Client {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            ext_idle_notification_v1::Event::Idled => "idle",
            ext_idle_notification_v1::Event::Resumed => "resumed",
            _ => return,
        };
        state.events.push((*id, event));
    }
}
wayland_client::delegate_noop!(Client: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(Client: ignore ExtIdleNotifierV1);

struct Harness {
    display: Display<Server>,
    server: Server,
    event_loop: EventLoop<'static, Server>,
    connection: Connection,
    queue: EventQueue<Client>,
    client: Client,
}
impl Harness {
    fn new() -> Self {
        let display = Display::new().unwrap();
        let event_loop = EventLoop::try_new().unwrap();
        let mut seats = SeatState::new();
        let seat = seats.new_wl_seat(&display.handle(), "test");
        let server = Server {
            compositor: smithay::wayland::compositor::CompositorState::new::<Server>(
                &display.handle(),
            ),
            idle: IdleNotifierState::new(&display.handle(), event_loop.handle()),
            seats,
            seat,
        };
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        client_socket.set_nonblocking(true).unwrap();
        display
            .handle()
            .insert_client(
                server_socket,
                Arc::new(crate::state::ClientState::default()),
            )
            .unwrap();
        let connection = Connection::from_socket(client_socket).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut harness = Self {
            display,
            server,
            event_loop,
            connection,
            queue,
            client: Client::default(),
        };
        harness.exchange();
        // Bind the globals announced by the registry events.
        harness.exchange();
        assert!(harness.client.notifier.is_some());
        assert!(harness.client.seat.is_some());
        harness
    }
    fn exchange(&mut self) {
        self.connection.flush().unwrap();
        self.display.dispatch_clients(&mut self.server).unwrap();
        self.read_events();
    }
    fn read_events(&mut self) {
        self.display.flush_clients().unwrap();
        self.queue.dispatch_pending(&mut self.client).unwrap();
        if let Some(read) = self.queue.prepare_read() {
            let _ = read.read();
        }
        self.queue.dispatch_pending(&mut self.client).unwrap();
    }
    fn notification(&mut self, id: u32, timeout: u32, input_only: bool) -> ExtIdleNotificationV1 {
        let notifier = self.client.notifier.as_ref().unwrap();
        let seat = self.client.seat.as_ref().unwrap();
        let notification = if input_only {
            notifier.get_input_idle_notification(timeout, seat, &self.queue.handle(), id)
        } else {
            notifier.get_idle_notification(timeout, seat, &self.queue.handle(), id)
        };
        self.exchange();
        notification
    }
    fn timeout(&mut self) {
        self.event_loop
            .dispatch(Some(Duration::from_millis(40)), &mut self.server)
            .unwrap();
        self.read_events();
    }
    fn activity(&mut self) {
        self.server.idle.notify_activity(&self.server.seat);
        self.read_events();
    }
}
#[test]
fn idle_is_one_shot_and_activity_resumes_then_rearms() {
    let mut h = Harness::new();
    let _notification = h.notification(1, 5, false);
    h.timeout();
    assert_eq!(h.client.events, [(1, "idle")]);
    h.timeout();
    assert_eq!(h.client.events, [(1, "idle")]);
    h.activity();
    assert_eq!(h.client.events, [(1, "idle"), (1, "resumed")]);
    h.timeout();
    assert_eq!(h.client.events.last(), Some(&(1, "idle")));
    assert_eq!(h.client.events.len(), 3);
}
#[test]
fn inhibitors_cancel_timers_and_resume_idle_clients() {
    let mut h = Harness::new();
    let _notification = h.notification(1, 5, false);
    h.server.idle.set_is_inhibited(true);
    h.timeout();
    h.activity();
    h.timeout();
    assert!(h.client.events.is_empty());
    h.server.idle.set_is_inhibited(false);
    h.timeout();
    assert_eq!(h.client.events, [(1, "idle")]);
    h.server.idle.set_is_inhibited(true);
    h.read_events();
    assert_eq!(h.client.events, [(1, "idle"), (1, "resumed")]);
}
#[test]
fn input_idle_ignores_inhibitors_and_destroy_cancels_timer() {
    let mut h = Harness::new();
    h.server.idle.set_is_inhibited(true);
    let ordinary = h.notification(1, 5, false);
    let input = h.notification(2, 5, true);
    h.timeout();
    assert_eq!(h.client.events, [(2, "idle")]);
    h.activity();
    input.destroy();
    ordinary.destroy();
    h.exchange();
    let count = h.client.events.len();
    h.timeout();
    assert_eq!(h.client.events.len(), count);
}
