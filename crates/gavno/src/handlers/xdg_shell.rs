use smithay::{
    delegate_xdg_shell,
    desktop::{find_popup_root_surface, get_popup_toplevel_coords, PopupKind, Window},
    input::{
        pointer::{Focus, GrabStartData as PointerGrabStartData},
        Seat,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            protocol::{wl_output, wl_seat, wl_surface::WlSurface},
            Resource,
        },
    },
    output::Output,
    utils::{Rectangle, Serial},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::{
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
    window_mode::WindowMode,
    Gavno,
};

impl XdgShellHandler for Gavno {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Mapped on the first buffer commit, see `CompositorHandler::commit`.
        let window = Window::new_wayland_window(surface);
        self.ipc.assign_id(&window);
        self.unmapped.push(window);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unmapped
            .retain(|w| w.toplevel().is_some_and(|t| t.wl_surface() != surface.wl_surface()));
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            let was_focused = self.focused_window().as_ref() == Some(&window);
            if self.space.element_location(&window).is_some() { self.window_animations.close(&window, self.start_time.elapsed()); }
            self.space.unmap_elem(&window);
            self.workspace_forget(&window);
            self.dispatch_modules(|m, gv| {
                m.window_unmapped(gv, &window);
                crate::module::Handled::No
            });
            if was_focused {
                self.focus_topmost();
            }
        }
        self.queue_redraw_all();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        if self.protocols.locks.locked { return; }
        let Some(seat) = Seat::from_resource(&seat) else { return; };

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let Some(window) = self.window_for_surface(wl_surface) else {
                return;
            };
            if crate::window_mode::mode_state(&window).mode == WindowMode::Fullscreen {
                return;
            }
            let Some(initial_window_location) = self.space.element_location(&window) else {
                return;
            };

            let grab = MoveSurfaceGrab::start(start_data, window, initial_window_location);

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        if self.protocols.locks.locked { return; }
        let Some(seat) = Seat::from_resource(&seat) else { return; };

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let Some(window) = self.window_for_surface(wl_surface) else {
                return;
            };
            if crate::window_mode::mode_state(&window).mode != WindowMode::Floating {
                return;
            }
            let Some(initial_window_location) = self.space.element_location(&window) else {
                return;
            };
            let initial_window_size = window.geometry().size;

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });

            surface.send_pending_configure();

            let grab = ResizeSurfaceGrab::start(
                start_data,
                window,
                edges.into(),
                Rectangle::new(initial_window_location, initial_window_size),
            );

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.request_window_mode(&surface, WindowMode::Maximized, None);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.request_window_mode(&surface, WindowMode::Floating, None);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, wl_output: Option<wl_output::WlOutput>) {
        let output = wl_output.as_ref().and_then(Output::from_resource);
        self.request_window_mode(&surface, WindowMode::Fullscreen, output);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.request_window_mode(&surface, WindowMode::Floating, None);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // TODO popup grabs
    }
}

// Xdg Shell
delegate_xdg_shell!(Gavno);

pub fn check_grab(
    seat: &Seat<Gavno>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<Gavno>> {
    let pointer = seat.get_pointer()?;

    // Check that this surface has a click grab.
    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    // If the focus was for a different surface, ignore the request.
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(gv: &mut Gavno, surface: &WlSurface) {
    let popups = &mut gv.popups;
    // Handle toplevel commits: the initial configure goes out on the first
    // commit, while the window is still unmapped.
    if let Some(window) = gv
        .unmapped
        .iter()
        .chain(gv.space.elements())
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent {
            window.toplevel().unwrap().send_configure();
        }
    }

    // Handle popup commits.
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // NOTE: This should never fail as the initial configure is always
                    // allowed.
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

impl Gavno {
    /// A client asked for a mode change. Windows that are not mapped yet are
    /// tracked too, so their first configure and placement honour it.
    fn request_window_mode(&mut self, surface: &ToplevelSurface, mode: WindowMode, output: Option<Output>) {
        let window = self
            .window_for_surface(surface.wl_surface())
            .or_else(|| {
                self.unmapped
                    .iter()
                    .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface.wl_surface()))
                    .cloned()
            });
        match window {
            Some(window) => self.set_window_mode(&window, mode, output),
            // Unknown surface: the protocol still wants a configure back.
            None => {
                surface.send_pending_configure();
            }
        }
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };

        let Some(output) = self.active_output() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
