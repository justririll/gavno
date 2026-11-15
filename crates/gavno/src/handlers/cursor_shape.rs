//! Smithay validates the requesting client's pointer focus and serial.
use crate::Gavno;
impl smithay::wayland::tablet_manager::TabletSeatHandler for Gavno {}
smithay::delegate_cursor_shape!(Gavno);
