use crate::Gavno;
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::idle_inhibit::IdleInhibitHandler,
};
impl IdleInhibitHandler for Gavno {
    fn inhibit(&mut self, surface: WlSurface) {
        // Keep duplicates: one surface may own several independent inhibitors.
        self.protocols.idle_policy.inhibitors.push(surface);
        self.refresh_idle();
    }
    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some(index) = self
            .protocols
            .idle_policy
            .inhibitors
            .iter()
            .position(|s| s == &surface)
        {
            self.protocols.idle_policy.inhibitors.swap_remove(index);
        }
        self.refresh_idle();
    }
}
smithay::delegate_idle_inhibit!(Gavno);
