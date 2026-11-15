//! Output/input backends. `udev` drives real hardware from a TTY; `winit`
//! runs ds as a window inside another session for development.

pub mod udev;
pub mod winit;

use smithay::backend::{allocator::dmabuf::Dmabuf, renderer::ImportDma};

use crate::Ds;

pub enum Backend {
    /// Before a backend is initialized.
    None,
    Winit(winit::WinitData),
    Udev(udev::UdevData),
}

pub fn queue_redraw_all(ds: &mut Ds) {
    match &mut ds.backend {
        Backend::None => {}
        Backend::Winit(data) => data.queue_redraw(),
        Backend::Udev(_) => udev::queue_redraw_all(ds),
    }
}

/// Imports a client dmabuf into the renderer to check it is usable.
pub fn import_dmabuf(ds: &mut Ds, dmabuf: &Dmabuf) -> bool {
    match &mut ds.backend {
        Backend::None => false,
        Backend::Winit(data) => data.backend.renderer().import_dmabuf(dmabuf, None).is_ok(),
        Backend::Udev(data) => data.import_dmabuf(dmabuf),
    }
}

pub fn switch_vt(ds: &mut Ds, vt: i32) {
    if let Backend::Udev(data) = &mut ds.backend {
        data.switch_vt(vt);
    }
}
