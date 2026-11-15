use std::{ffi::OsString, sync::Arc, time::Instant};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{keyboard::XkbConfig, pointer::CursorImageStatus, Seat, SeatState},
    output::Output,
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction},
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::DmabufState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        presentation::PresentationState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{wlr_layer::WlrLayerShellState, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};

use crate::{backend::Backend, config::Config, module::Module};

pub struct Gavno {
    pub start_time: Instant,
    pub clock: Clock<Monotonic>,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Gavno>,
    pub loop_signal: LoopSignal,
    pub config: Config,
    pub ipc: crate::ipc::IpcState,

    /// Mapped windows. A toplevel joins the space on its first buffer commit.
    pub space: Space<Window>,
    /// Toplevels that exist but have not committed a buffer yet.
    pub unmapped: Vec<Window>,
    pub mru: Vec<Window>,
    pub overlay: crate::overlay::Overlay,
    pub popups: PopupManager,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Gavno>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub viewporter_state: ViewporterState,
    pub presentation_state: PresentationState,
    pub fractional_scale_state: FractionalScaleManagerState,

    pub seat: Seat<Self>,
    pub cursor_status: CursorImageStatus,
    pub cursor: crate::render::CursorCache,
    /// Keys whose press was a compositor bind; their release is not forwarded.
    pub suppressed_keys: Vec<u32>,

    pub backend: Backend,
    pub modules: Vec<Box<dyn Module>>,
    /// `-c <command>`, run once the first output exists.
    pub startup_cmd: Option<String>,
    /// Set in `--greeter` mode: GAVNO is the login screen.
    pub greeter: Option<crate::greeter::Greeter>,
}

impl Gavno {
    pub fn new(event_loop: &mut EventLoop<'static, Self>, display: Display<Self>, config: Config) -> Self {
        let dh = display.handle();
        let clock = Clock::new();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let dmabuf_state = DmabufState::new();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(&dh, clock.id() as u32);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        let xkb = XkbConfig {
            layout: &config.xkb_layout,
            variant: &config.xkb_variant,
            options: config.xkb_options.clone(),
            ..Default::default()
        };
        if let Err(err) = seat.add_keyboard(xkb, 300, 30) {
            tracing::warn!(?err, "bad xkb config, falling back to us");
            seat.add_keyboard(Default::default(), 300, 30)
                .expect("default xkb keymap must compile");
        }
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        Self {
            start_time: Instant::now(),
            clock,
            socket_name,
            display_handle: dh,
            loop_handle: event_loop.handle(),
            loop_signal: event_loop.get_signal(),
            config,
            ipc: crate::ipc::IpcState::default(),
            space: Space::default(),
            unmapped: Vec::new(),
            mru: Vec::new(),
            overlay: crate::overlay::Overlay::default(),
            popups: PopupManager::default(),
            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            shm_state,
            dmabuf_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            viewporter_state,
            presentation_state,
            fractional_scale_state,
            seat,
            cursor_status: CursorImageStatus::default_named(),
            cursor: crate::render::CursorCache::new(),
            suppressed_keys: Vec::new(),
            backend: Backend::None,
            modules: Vec::new(),
            startup_cmd: None,
            greeter: None,
        }
    }

    fn init_wayland_listener(display: Display<Gavno>, event_loop: &mut EventLoop<'static, Gavno>) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().expect("no free wayland socket name");
        let socket_name = listening_socket.socket_name().to_os_string();
        let handle = event_loop.handle();

        handle
            .insert_source(listening_socket, |client_stream, _, state| {
                if let Err(err) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    tracing::warn!(?err, "failed to add a client");
                }
            })
            .expect("failed to init the wayland listener");

        handle
            .insert_source(Generic::new(display, Interest::READ, Mode::Level), |_, display, state| {
                // Safety: the display is never dropped while the source lives.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            })
            .expect("failed to add the display to the event loop");

        socket_name
    }

    /// Runs once per event loop iteration, after all events were dispatched.
    pub fn on_loop_idle(&mut self) {
        self.space.refresh();
        self.prune_navigation();
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// Something visible changed; draw the affected outputs on their next
    /// vblank. Cheap to call often: repeated calls coalesce.
    pub fn queue_redraw_all(&mut self) {
        crate::backend::queue_redraw_all(self);
    }

    /// True while something animates and needs the next frame drawn.
    pub fn is_animating(&self) -> bool {
        self.greeter.as_ref().is_some_and(|g| g.is_animating()) || self.overlay.is_animating()
    }

    pub fn pointer_location(&self) -> Point<f64, Logical> {
        self.seat.get_pointer().unwrap().current_location()
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        use smithay::wayland::shell::wlr_layer::Layer;
        if let Some(hit) = self.layer_hit(pos, &[Layer::Overlay, Layer::Top]) {
            return Some((hit.surface, hit.surface_loc));
        }
        if let Some(found) = self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        }) {
            return Some(found);
        }
        self.layer_hit(pos, &[Layer::Bottom, Layer::Background])
            .map(|hit| (hit.surface, hit.surface_loc))
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    pub fn focused_window(&self) -> Option<Window> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        self.window_for_surface(&focus)
    }

    /// The output under the pointer, else the first one.
    pub fn active_output(&self) -> Option<Output> {
        let pos = self.pointer_location();
        self.space
            .outputs()
            .find(|o| self.space.output_geometry(o).is_some_and(|g| g.to_f64().contains(pos)))
            .or_else(|| self.space.outputs().next())
            .cloned()
    }

    /// The union of all output rectangles, used to clamp the pointer.
    pub fn outputs_bbox(&self) -> Option<Rectangle<i32, Logical>> {
        self.space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .reduce(|a, b| a.merge(b))
    }

    /// Raises `window`, gives it keyboard focus and marks it activated;
    /// `None` clears focus.
    pub fn focus_window(&mut self, window: Option<&Window>) {
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(window) = window {
            self.space.raise_element(window, true);
            crate::snap::touch_mru(&mut self.mru, window);
        } else {
            for w in self.space.elements() {
                w.set_activated(false);
            }
        }
        for w in self.space.elements() {
            if let Some(toplevel) = w.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        let surface = window.and_then(|w| w.toplevel()).map(|t| t.wl_surface().clone());
        keyboard.set_focus(self, surface, serial);
        self.queue_redraw_all();
    }

    /// Focuses the topmost remaining window, e.g. after the focused one closed.
    pub fn focus_topmost(&mut self) {
        let top = self.space.elements().last().cloned();
        self.focus_window(top.as_ref());
    }

    /// Places a window that just committed its first buffer: centered in the
    /// active output, clamped to it, then focused.
    pub fn map_new_window(&mut self, window: Window) {
        let size = window.geometry().size;
        let output = self.active_output();
        let mode = crate::window_mode::mode_state(&window).mode;
        let loc = output
            .as_ref()
            .map(|o| match mode {
                // Asked for maximize/fullscreen before its first buffer.
                crate::window_mode::WindowMode::Maximized => crate::snap::Zone::Maximized.rect(self.work_area(o), self.config.gap).loc,
                crate::window_mode::WindowMode::Snapped(zone) => zone.rect(self.work_area(o), self.config.gap).loc,
                crate::window_mode::WindowMode::Fullscreen => {
                    self.space.output_geometry(o).unwrap_or_default().loc
                }
                crate::window_mode::WindowMode::Floating => {
                    let area = self.work_area(o);
                    let gap = self.config.gap;
                    let x = area.loc.x + ((area.size.w - size.w) / 2).max(gap);
                    let y = area.loc.y + ((area.size.h - size.h) / 2).max(gap);
                    Point::from((x, y))
                }
            })
            .unwrap_or_default();
        self.space.map_element(window.clone(), loc, true);
        self.focus_window(Some(&window));
        self.dispatch_modules(|m, gv| {
            m.window_mapped(gv, &window);
            crate::module::Handled::No
        });
    }

    /// Called by backends after a new output was mapped.
    pub fn output_added_hook(&mut self) {
        self.arrange_layers_all();
        if let Some(cmd) = self.startup_cmd.take() {
            spawn_shell(&cmd);
        }
        self.queue_redraw_all();
    }

    pub fn close_focused(&mut self) {
        if let Some(toplevel) = self.focused_window().and_then(|w| w.toplevel().cloned()) {
            toplevel.send_close();
        }
    }

    pub fn quit(&mut self) {
        tracing::info!("quitting");
        self.loop_signal.stop();
    }
}

/// Runs `cmd` through `sh -c`, detached from GAVNO. The child is reaped on a
/// helper thread so it never becomes a zombie.
pub fn spawn_shell(cmd: &str) {
    tracing::info!(cmd, "spawning");
    match std::process::Command::new("sh").arg("-c").arg(cmd).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(?err, cmd, "spawn failed"),
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
