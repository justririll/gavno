use crate::Gavno;
use smithay::{
    reexports::{wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode, wayland_server::protocol::wl_surface::WlSurface},
    wayland::{compositor::with_states, shell::xdg::{decoration::XdgDecorationHandler, ToplevelSurface}},
};
use std::sync::Mutex;

/// The mode a client last asked for with `set_mode`; `None` after `unset_mode`.
#[derive(Default)]
struct Requested(Mutex<Option<Mode>>);

/// A client that asked to draw its own title bar (VS Code's custom title bar,
/// Firefox's tabs in the title bar) keeps it in Classic instead of getting a
/// second one from GAVNO.
pub(crate) fn wants_client_side(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states.data_map.get::<Requested>().is_some_and(|r| *r.0.lock().unwrap_or_else(|e| e.into_inner()) == Some(Mode::ClientSide))
    })
}
fn remember(surface: &ToplevelSurface, mode: Option<Mode>) {
    with_states(surface.wl_surface(), |states| {
        states.data_map.insert_if_missing_threadsafe(Requested::default);
        if let Some(r) = states.data_map.get::<Requested>() {
            *r.0.lock().unwrap_or_else(|e| e.into_inner()) = mode;
        }
    });
}

// Change this policy when compositor titlebars are available.
const DECORATION_MODE: Mode = Mode::ClientSide;
fn configure(surface: ToplevelSurface) {
    surface.with_pending_state(|state| state.decoration_mode = Some(DECORATION_MODE));
    if surface.is_initial_configure_sent() {
        surface.send_pending_configure();
    }
}
impl XdgDecorationHandler for Gavno {
    fn new_decoration(&mut self, surface: ToplevelSurface) {
        configure(surface);
    }
    fn request_mode(&mut self, surface: ToplevelSurface, mode: Mode) {
        remember(&surface, Some(mode));
        self.answer_decoration_request(surface);
    }
    fn unset_mode(&mut self, surface: ToplevelSurface) {
        remember(&surface, None);
        self.answer_decoration_request(surface);
    }
}
impl Gavno {
    /// Applies the Classic policy to the new request and always answers it.
    fn answer_decoration_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.decoration_mode.get_or_insert(DECORATION_MODE);
        });
        if !surface.is_initial_configure_sent() {
            return;
        }
        if self.sync_classic_decorations() {
            self.refit_fixed_windows();
        }
        surface.send_pending_configure();
    }
}
smithay::delegate_xdg_decoration!(Gavno);
