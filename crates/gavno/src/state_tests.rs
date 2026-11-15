//! In-process state tests: no display backend, renderer, or public Wayland socket.
use super::*;
use std::{os::unix::net::UnixStream, time::Duration};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    shm: Option<wl_shm::WlShm>,
    pointer_positions: Vec<(f64, f64)>,
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
                "wl_seat" => {
                    let seat: wl_seat::WlSeat = registry.bind(name, version.min(8), qh, ());
                    seat.get_pointer(qh, ());
                }
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "xdg_wm_base" => state.shell = Some(registry.bind(name, version.min(6), qh, ())),
                _ => {}
            }
        }
    }
}
wayland_client::delegate_noop!(Client: ignore wl_seat::WlSeat);
impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.pointer_positions.push((surface_x, surface_y)),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(Client: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Client: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Client: ignore xdg_wm_base::XdgWmBase);
wayland_client::delegate_noop!(Client: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(Client: ignore wl_buffer::WlBuffer);
impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        _: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}
wayland_client::delegate_noop!(Client: ignore xdg_toplevel::XdgToplevel);

pub(crate) struct Harness {
    pub gv: Gavno,
    event_loop: EventLoop<'static, Gavno>,
    connection: Connection,
    queue: EventQueue<Client>,
    client: Client,
    surfaces: Vec<(Window, wl_surface::WlSurface)>,
}
impl Harness {
    pub fn new() -> Self {
        let mut event_loop = EventLoop::try_new().unwrap();
        let display = Display::new().unwrap();
        let mut gv = Gavno::new(
            &mut event_loop,
            display,
            Config::build(true, None, |_| None).0,
        );
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
            surfaces: Vec::new(),
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
        let surface = self
            .client
            .compositor
            .as_ref()
            .unwrap()
            .create_surface(&qh, ());
        let shell_surface = self
            .client
            .shell
            .as_ref()
            .unwrap()
            .get_xdg_surface(&surface, &qh, ());
        let _toplevel = shell_surface.get_toplevel(&qh, ());
        self.exchange();
        let window = self.gv.unmapped.pop().unwrap();
        self.surfaces.push((window.clone(), surface));
        window
    }
    pub fn pointer_positions(&mut self) -> Vec<(f64, f64)> {
        self.exchange();
        self.exchange();
        std::mem::take(&mut self.client.pointer_positions)
    }
    /// Commit a real shm buffer and acknowledge the latest compositor configure.
    pub fn commit_size(&mut self, window: &Window, width: i32, height: i32) {
        use std::os::fd::AsFd;
        self.exchange();
        self.exchange();
        let path = std::env::temp_dir().join(format!(
            "gavno-test-{}-{:?}",
            std::process::id(),
            smithay::utils::SERIAL_COUNTER.next_serial()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let length = width * height * 4;
        file.set_len(length as u64).unwrap();
        let qh = self.queue.handle();
        let pool = self
            .client
            .shm
            .as_ref()
            .unwrap()
            .create_pool(file.as_fd(), length, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        let surface = &self.surfaces.iter().find(|(w, _)| w == window).unwrap().1;
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        pool.destroy();
        self.exchange();
        self.exchange();
        assert_eq!(window.geometry().size, (width, height).into());
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
    h.gv.overlay
        .preview(output, crate::snap::Zone::Left, from, to, Duration::ZERO);
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
    h.gv.window_animations.entries[0]
        .motion
        .retarget(from.to_f64(), to.to_f64(), Duration::ZERO);
    h.gv.resize_direction(crate::snap::Direction::Right);
    let motion = h.gv.window_animations.motion(&window).unwrap();
    assert!(!motion.active());
    assert_eq!(motion.rect(to.to_f64()), to.to_f64());

    h.gv.window_animations.entries[0]
        .motion
        .retarget(from.to_f64(), to.to_f64(), Duration::ZERO);
    h.gv.configure_slot(&window, to, crate::window_mode::WindowMode::Floating);
    let motion = h.gv.window_animations.motion(&window).unwrap();
    assert!(!motion.active());
    assert_eq!(motion.rect(to.to_f64()), to.to_f64());
}
