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
        crate::window_mode::track_floating_content(surface.wl_surface());
        let window = Window::new_wayland_window(surface);
        self.guard(crate::module::Feature::Ipc, (), |gv| gv.ipc.assign_id(&window));
        self.unmapped.push(window);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unmapped
            .retain(|w| w.toplevel().is_some_and(|t| t.wl_surface() != surface.wl_surface()));
        if self.overlay.dropdown.window().and_then(|w| w.toplevel()).is_some_and(|t| t.wl_surface() == surface.wl_surface()) {
            self.dropdown_closed();
        }
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.unmap_window(&window);
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
            if self.tiled(&window) {
                if let Some(grab) = crate::grabs::tiling_grab::SplitterGrab::for_tile(self,start_data,&window,edges.into()) {
                    pointer.set_grab(self,grab,serial,Focus::Clear);
                }
                return;
            }
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

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) { self.minimize_window(&window); }
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

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        use smithay::desktop::{PopupKeyboardGrab, PopupPointerGrab, PopupUngrabStrategy};
        use smithay::input::pointer::Focus;
        let Some(seat) = Seat::<Gavno>::from_resource(&seat) else { return };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        // Menus of a locked or greeter session never get grabs.
        if self.protocols.locks.locked || self.lock.internal() || self.greeter.is_some() {
            return;
        }
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else { return };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
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

/// Include the final tile/state in the handshake instead of resizing a client
/// immediately after its first buffer. Floating clients still choose their size.
pub(crate) fn prepare_initial_configure(gv: &Gavno, window: &Window) {
    let Some(toplevel) = window.toplevel() else { return };
    use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
    let classic = gv.features.on(crate::module::Feature::Classic)
        && gv.classic.settled == crate::classic::ShellMode::Classic;
    // Negotiate the same style as sync_classic_decorations before sizing. Its
    // idle callback must not change decoration/size after the first configure.
    let ssd = classic && gv.features.on(crate::module::Feature::Decorations) && gv.greeter.is_none()
        && !crate::handlers::xdg_decoration::wants_client_side(toplevel.wl_surface());
    toplevel.with_pending_state(|state| {
        if state.decoration_mode.is_some() { state.decoration_mode = Some(if ssd { Mode::ServerSide } else { Mode::ClientSide }); }
    });
    gv.prepare_classic_window(window);
    let tile = gv.initial_tile_rect(window);
    if tile.is_some() {
        crate::window_mode::with_mode_state(window, |s| s.initially_tiled = true);
    }
    let output = gv.classic_initial_output(window).or_else(|| gv.active_output());
    let bounds = output.as_ref().map(|output| gv.work_area(output).size);
    let fixed = output.as_ref().and_then(|output| {
        let area = gv.work_area(output);
        let rect = match crate::window_mode::mode_state(window).mode {
            WindowMode::Maximized => crate::snap::Zone::Maximized.rect(area, if classic { 0 } else { gv.config.gap }),
            WindowMode::Snapped(zone) => zone.rect(area, gv.config.gap),
            // Fullscreen already carries the requested output size; floating
            // clients choose their natural size unless a tiling slot exists.
            WindowMode::Fullscreen | WindowMode::Floating => return None,
        };
        Some(gv.classic_client_rect(window, rect))
    });
    let activate = !gv.protocols.locks.locked && gv.greeter.is_none() && !gv.polkit_visible()
        && !crate::handlers::xdg_activation::is_urgent(window);
    toplevel.with_pending_state(|state| {
        state.bounds = bounds;
        if let Some(rect) = fixed { state.size = Some(rect.size); }
        if activate { state.states.set(xdg_toplevel::State::Activated); }
        if let Some(rect) = tile {
            state.size = Some(rect.size);
            for flag in [xdg_toplevel::State::TiledLeft, xdg_toplevel::State::TiledRight,
                xdg_toplevel::State::TiledTop, xdg_toplevel::State::TiledBottom] {
                state.states.set(flag);
            }
        }
    });
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(gv: &mut Gavno, surface: &WlSurface) {
    // Handle toplevel commits: the initial configure goes out on the first
    // commit, while the window is still unmapped.
    // Windows on hidden workspaces are neither unmapped nor in the Space but
    // still need the configure handshake when they remap.
    let window = gv
        .unmapped
        .iter()
        .chain(gv.space.elements())
        .chain(gv.workspaces.all().map(|o| &o.window))
        .chain(gv.workspaces.stashed().map(|o| &o.window))
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        .cloned();
    if let Some(window) = window {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent && gv.is_dropdown_window(&window) {
            gv.prepare_dropdown_configure(&window);
            window.toplevel().unwrap().send_configure();
        } else if !initial_configure_sent && !gv.delay_classic_configure(&window) {
            prepare_initial_configure(gv, &window);
            window.toplevel().unwrap().send_configure();
        }
        if !gv.is_dropdown_window(&window) { gv.finish_natural_float(&window, surface); }
    }

    // Handle popup commits.
    gv.popups.commit(surface);
    if let Some(popup) = gv.popups.find_popup(surface) {
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

        // Constrain to the parent window's output, not the pointer's.
        let Some(output) = self.output_for_window(window).or_else(|| self.active_output()) else {
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

impl Gavno {
    /// Takes a toplevel off the desktop: close animation, workspace and tiling
    /// membership, and keyboard focus. Used when the toplevel is destroyed and
    /// when it unmaps itself by committing a null buffer.
    pub fn unmap_window(&mut self, window: &Window) {
        self.remember_classic_window(window);
        self.forget_classic_window(window);
        let was_focused = self.focused_window().as_ref() == Some(window);
        if self.space.element_location(window).is_some() {
            self.features.call(crate::module::Feature::Animations, (), || {
                self.window_animations.close(window, self.start_time.elapsed())
            });
        }
        self.space.unmap_elem(window);
        self.workspace_forget(window);
        if was_focused {
            self.focus_topmost();
        }
    }
}
