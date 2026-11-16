//! Output/input backends. `udev` drives real hardware from a TTY; `winit`
//! runs GAVNO as a window inside another session for development.

pub mod udev;
pub mod winit;

use smithay::backend::{allocator::dmabuf::Dmabuf, renderer::ImportDma};

use crate::Gavno;

pub enum Backend {
    /// Before a backend is initialized.
    None,
    Winit(winit::WinitData),
    Udev(udev::UdevData),
}

pub fn queue_redraw_all(gv: &mut Gavno) {
    if gv.power.is_off() {
        gv.schedule_power_frames();
        return;
    }
    match &mut gv.backend {
        Backend::None => {}
        Backend::Winit(data) => data.queue_redraw(),
        Backend::Udev(_) => udev::queue_redraw_all(gv),
    }
}

/// Imports a client dmabuf into the renderer to check it is usable.
pub fn import_dmabuf(gv: &mut Gavno, dmabuf: &Dmabuf) -> bool {
    match &mut gv.backend {
        Backend::None => false,
        Backend::Winit(data) => data.backend.renderer().import_dmabuf(dmabuf, None).is_ok(),
        Backend::Udev(data) => data.import_dmabuf(dmabuf),
    }
}

pub fn switch_vt(gv: &mut Gavno, vt: i32) {
    if let Backend::Udev(data) = &mut gv.backend {
        data.switch_vt(vt);
    }
}

/// Renders the first output offscreen and reads it back as RGBA.
pub fn screenshot(gv: &mut Gavno) -> Option<(u32, u32, Vec<u8>)> {
    match &gv.backend {
        Backend::Udev(_) => udev::screenshot(gv),
        Backend::Winit(data) => {
            let output = data.output().clone();
            winit::capture_output(gv, &output, true)
        }
        Backend::None => None,
    }
}

/// Offscreen capture of `output` on whichever backend is running.
pub fn capture_output(gv: &mut Gavno, output: &smithay::output::Output, cursor: bool) -> Option<(u32, u32, Vec<u8>)> {
    match &gv.backend {
        Backend::Udev(_) => udev::capture_output(gv, output, cursor),
        Backend::Winit(_) => winit::capture_output(gv, output, cursor),
        Backend::None => None,
    }
}
