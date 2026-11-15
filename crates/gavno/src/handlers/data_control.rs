use crate::Gavno;
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
impl DataControlHandler for Gavno {
    fn data_control_state(&self) -> &DataControlState {
        &self.protocols.data_control
    }
}
smithay::delegate_data_control!(Gavno);
