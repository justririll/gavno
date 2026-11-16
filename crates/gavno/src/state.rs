use std::{ffi::OsString, sync::Arc, time::Instant};

use smithay::{
    desktop::{PopupManager, Space, Window},
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
        viewporter::ViewporterState,
    },
};

#[cfg(not(test))]
use smithay::wayland::socket::ListeningSocketSource;

use crate::{backend::Backend, config::Config, module::{Feature, Features}};

pub struct Gavno {
    pub protocols: crate::handlers::protocols::Protocols,
    pub satellite: Option<crate::xwayland::Satellite>,
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
    pub window_animations: crate::anim::WindowAnimations,
    pub window_decor: crate::decor::WindowDecor,
    /// Toplevels that exist but have not committed a buffer yet.
    pub unmapped: Vec<Window>,
    pub mru: Vec<Window>,
    pub workspaces: crate::workspace::Workspaces,
    pub overview: crate::overview::Overview,
    pub dock: crate::dock::Dock,
    pub topbar: crate::topbar::TopBar,
    pub gestures: crate::gesture::Gestures,
    pub window_gestures: crate::window_gesture::WindowGestures,
    pub pointer_gestures: smithay::wayland::pointer_gestures::PointerGesturesState,
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
    pub desktop_keys: Vec<u32>,
    /// Buttons whose presses reached Smithay, including window manipulation grabs.
    pub(crate) pointer_buttons: Vec<u32>,
    pub(crate) suppressed_buttons: Vec<u32>,

    pub backend: Backend,
    pub features: Features,
    /// `-c <command>`, run once the first output exists.
    pub startup_cmd: Option<String>,
    /// Set in `--greeter` mode: GAVNO is the login screen.
    pub greeter: Option<crate::greeter::Greeter>,
    pub lock: crate::lock::Lock,
}

impl Gavno {
    pub fn new(event_loop: &mut EventLoop<'static, Self>, display: Display<Self>, config: Config) -> Self {
        let features = Features::new(&config);
        let tiling_default = config.tiling.default && features.on(Feature::Tiling);
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

        let protocols = crate::handlers::protocols::Protocols::new(&dh, event_loop.handle(), &primary_selection_state);

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
            protocols,
            satellite: None,
            start_time: Instant::now(),
            clock,
            socket_name,
            display_handle: dh.clone(),
            loop_handle: event_loop.handle(),
            loop_signal: event_loop.get_signal(),
            config,
            ipc: crate::ipc::IpcState::default(),
            space: Space::default(),
            window_animations: Default::default(),
            window_decor: Default::default(),
            unmapped: Vec::new(),
            mru: Vec::new(),
            workspaces: crate::workspace::Workspaces { default_tiling: tiling_default, ..Default::default() },
            overview: Default::default(),
            dock: Default::default(),
            topbar: Default::default(),
            gestures: Default::default(),
            window_gestures: Default::default(),
            pointer_gestures: smithay::wayland::pointer_gestures::PointerGesturesState::new::<Self>(&dh),
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
            desktop_keys: Vec::new(),
            pointer_buttons: Vec::new(),
            suppressed_buttons: Vec::new(),
            backend: Backend::None,
            features,
            startup_cmd: None,
            greeter: None,
            lock: Default::default(),
        }
    }

    fn init_wayland_listener(display: Display<Gavno>, event_loop: &mut EventLoop<'static, Gavno>) -> OsString {
        #[cfg(not(test))]
        let listening_socket = ListeningSocketSource::new_auto().expect("no free wayland socket name");
        #[cfg(not(test))]
        let socket_name = listening_socket.socket_name().to_os_string();
        // Unit tests use in-process socket pairs without advertising a compositor.
        #[cfg(test)]
        let socket_name = OsString::new();
        let handle = event_loop.handle();

        #[cfg(not(test))]
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
        self.run_feature_resets();
        self.space.refresh();
        self.refresh_workspaces();
        self.refresh_desktop_pointer();
        self.refresh_splitter_cursor();
        self.validate_resize_mode();
        self.refresh_resize_cursor();
        self.prune_navigation();
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        self.update_shortcut_inhibitors(focus.as_ref());
        self.refresh_constraints(focus.as_ref());
        self.refresh_lock();
        self.refresh_internal_lock();
        self.refresh_shell();
        self.finish_dock_launches();
        self.refresh_idle();
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// Something visible changed; draw the affected outputs on their next
    /// vblank. Cheap to call often: repeated calls coalesce.
    pub fn queue_redraw_all(&mut self) {
        crate::backend::queue_redraw_all(self);
    }

    /// True while something animates and needs the next frame drawn.
    pub fn is_animating(&mut self) -> bool {
        if self.protocols.locks.locked { return self.lock.is_animating(); }
        if let Some(g) = self.greeter.as_mut() { return g.safe_call(false, |g| g.is_animating()); }
        let animations = self.guard(Feature::Animations, false, |gv| gv.window_animations.is_animating());
        let overlay = self.guard(Feature::Switcher, false, |gv| gv.overlay.is_animating());
        let overview = self.guard(Feature::Overview, false, |gv| gv.overview.progress.is_active()
            || gv.overview.flick.as_ref().is_some_and(|(_, x, y)| x.is_active() || y.is_active()));
        let dock = self.guard(Feature::Dock, false, |gv| gv.dock.active());
        let bar = self.guard(Feature::Bar, false, |gv| gv.topbar.active());
        animations || overlay || self.workspaces.active() || overview || dock || bar
    }

    pub fn pointer_location(&self) -> Point<f64, Logical> {
        self.seat.get_pointer().unwrap().current_location()
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.protocols.locks.locked { return self.lock_surface_under(pos); }
        if self.greeter.is_some() { return None; }
        if self.greeter.is_none() && self.shell_contains(pos) { return None; }
        use smithay::wayland::shell::wlr_layer::Layer;
        if let Some(hit) = self.layer_hit(pos, &[Layer::Overlay, Layer::Top]) {
            return Some((hit.surface, hit.surface_loc));
        }
        if self.desktop_transform_at(pos) { return None; }
        if self.splitter_at(pos).is_some() || self.resize_band_at(pos).is_some() || (self.features.on(crate::module::Feature::ResizeMode) && self.overlay.resize_mode.active()) { return None; }
        if let Some((_, _, surface, origin)) = self.window_input_at(pos) {
            return Some((surface, origin));
        }
        self.layer_hit(pos, &[Layer::Bottom, Layer::Background])
            .map(|hit| (hit.surface, hit.surface_loc))
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .chain(self.workspaces.all().map(|v| &v.window))
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
        if self.protocols.locks.locked || self.greeter.is_some() { return; }
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(window) = window {
            if let Some((output, index)) = self.workspaces.membership(window) {
                if self.workspaces.get(&output).is_some_and(|o| o.list.active != index) { self.switch_workspace(&output, index); }
            }
            self.restore_minimized(window);
            self.workspace_focus(window);
            crate::handlers::xdg_activation::clear_urgency(window);
            self.space.raise_element(window, true);
            self.raise_tile_overlays();
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
        if surface.is_none() {
            self.guard(Feature::Decorations, (), |gv| gv.window_decor.focus(None));
            self.update_shortcut_inhibitors(None);
            self.refresh_constraints(None);
        }
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
        let urgent = crate::handlers::xdg_activation::is_urgent(&window);
        self.space.map_element(window.clone(), loc, !urgent && !self.protocols.locks.locked);
        if self.greeter.is_none() {
            if let Some(output) = &output {
                let owner = self.workspaces.ensure(output);
                owner.list.items[owner.list.active].windows.push(crate::workspace::OwnedWindow { window: window.clone(), location: loc });
                owner.append_empty();
            }
        }
        self.reflow_tiling(true);
        self.features.call(crate::module::Feature::Animations, (), || self.window_animations.open(&window, self.start_time.elapsed()));
        if !urgent { self.focus_window(Some(&window)); }
    }

    /// Called by backends after a new output was mapped.
    pub fn output_added_hook(&mut self) {
        self.protocols.locks.outputs_added(self.space.outputs().cloned());
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

#[cfg(test)]
#[path = "state_tests.rs"]
pub(crate) mod tests;
