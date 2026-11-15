//! In-process state tests: no display backend, renderer, or public Wayland socket.
use super::*;
use std::{os::unix::net::UnixStream, time::Duration};
use wayland_client::{
    protocol::{wl_compositor, wl_registry, wl_surface},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shell: Option<xdg_wm_base::XdgWmBase>,
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
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()))
                }
                "xdg_wm_base" => state.shell = Some(registry.bind(name, version.min(6), qh, ())),
                _ => {}
            }
        }
    }
}
wayland_client::delegate_noop!(Client: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Client: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Client: ignore xdg_wm_base::XdgWmBase);
wayland_client::delegate_noop!(Client: ignore xdg_surface::XdgSurface);
wayland_client::delegate_noop!(Client: ignore xdg_toplevel::XdgToplevel);

pub(crate) struct Harness {
    pub gv: Gavno,
    event_loop: EventLoop<'static, Gavno>,
    connection: Connection,
    queue: EventQueue<Client>,
    client: Client,
}
impl Harness {
    pub fn new() -> Self {
        let mut event_loop = EventLoop::try_new().unwrap();
        let display = Display::new().unwrap();
        let mut gv = Gavno::new(&mut event_loop, display, Config::build(true, None, |_| None).0);
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        client_socket.set_nonblocking(true).unwrap();
        gv.display_handle
            .insert_client(server_socket, Arc::new(ClientState::default()))
            .unwrap();
        let connection = Connection::from_socket(client_socket).unwrap();
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut h = Self {
            gv,
            event_loop,
            connection,
            queue,
            client: Client::default(),
        };
        h.exchange();
        h.exchange();
        h
    }

    fn exchange(&mut self) {
        self.connection.flush().unwrap();
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

    pub fn window(&mut self) -> Window {
        let qh = self.queue.handle();
        let surface = self.client.compositor.as_ref().unwrap().create_surface(&qh, ());
        let shell_surface = self.client.shell.as_ref().unwrap()
            .get_xdg_surface(&surface, &qh, ());
        let _toplevel = shell_surface.get_toplevel(&qh, ());
        self.exchange();
        self.gv.unmapped.pop().unwrap()
    }
}

#[test]
fn locked_sessions_do_not_schedule_hidden_window_animations() {
    let mut h = Harness::new();
    let window = h.window();
    h.gv.map_new_window(window);
    assert!(h.gv.is_animating());
    h.gv.protocols.locks.locked = true;
    assert!(!h.gv.is_animating());
    let window = h.window();
    h.gv.map_new_window(window);
    assert!(h.gv.window_animations.is_animating());
    assert!(!h.gv.is_animating());
    h.gv.protocols.locks.locked = false;
    assert!(h.gv.is_animating());
}

#[test]
fn locked_sessions_do_not_schedule_hidden_overlay_animations() {
    let mut h = Harness::new();
    let output = Output::new(
        "test".into(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    );
    let from = Rectangle::new((0, 0).into(), (100, 100).into());
    let to = Rectangle::new((0, 0).into(), (200, 200).into());
    h.gv.overlay.preview(
        output, crate::snap::Zone::Left, from, to, Duration::ZERO,
    );
    assert!(h.gv.overlay.is_animating());
    assert!(h.gv.is_animating());
    h.gv.protocols.locks.locked = true;
    assert!(!h.gv.is_animating());
    h.gv.protocols.locks.locked = false;
    assert!(h.gv.is_animating());
}

#[test]
fn keyboard_resize_and_slot_changes_cancel_geometry_overrides() {
    let mut h = Harness::new();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    let from = Rectangle::new((10, 20).into(), (100, 100).into());
    let to = Rectangle::new((30, 40).into(), (200, 200).into());
    h.gv.window_animations.entries[0].motion.retarget(
        from.to_f64(),
        to.to_f64(),
        Duration::ZERO,
    );
    h.gv.resize_direction(crate::snap::Direction::Right);
    let motion = h.gv.window_animations.motion(&window).unwrap();
    assert!(!motion.active());
    assert_eq!(motion.rect(to.to_f64()), to.to_f64());

    h.gv.window_animations.entries[0].motion.retarget(
        from.to_f64(),
        to.to_f64(),
        Duration::ZERO,
    );
    h.gv.configure_slot(&window, to, crate::window_mode::WindowMode::Floating);
    let motion = h.gv.window_animations.motion(&window).unwrap();
    assert!(!motion.active());
    assert_eq!(motion.rect(to.to_f64()), to.to_f64());
}
