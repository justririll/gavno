pub mod tiling_grab;
pub mod move_grab;
pub use move_grab::MoveSurfaceGrab;

pub mod resize_grab;
pub use resize_grab::ResizeSurfaceGrab;

impl crate::Gavno {
    /// End window manipulation before any desktop mapping or transform changes.
    /// Client click grabs remain intact so their outstanding releases can arrive.
    pub(crate) fn end_window_grab(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let window_grab = pointer.with_grab(|_, grab| {
            grab.is::<MoveSurfaceGrab>() || grab.is::<ResizeSurfaceGrab>() || grab.is::<tiling_grab::SplitterGrab>()
        }).unwrap_or(false);
        if window_grab {
            pointer.unset_grab(
                self,
                smithay::utils::SERIAL_COUNTER.next_serial(),
                self.clock.now().as_millis() as u32,
            );
        }
    }
}
