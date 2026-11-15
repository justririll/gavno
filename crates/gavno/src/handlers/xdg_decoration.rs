use crate::Gavno;
use smithay::{
    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    wayland::shell::xdg::{decoration::XdgDecorationHandler, ToplevelSurface},
};

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
    fn request_mode(&mut self, surface: ToplevelSurface, _mode: Mode) {
        configure(surface);
    }
    fn unset_mode(&mut self, surface: ToplevelSurface) {
        configure(surface);
    }
}
smithay::delegate_xdg_decoration!(Gavno);
