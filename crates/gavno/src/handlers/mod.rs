pub mod protocols;
pub mod screencopy;
mod cursor_shape;
mod data_control;
mod idle_inhibit;
mod idle_notify;
mod keyboard_shortcuts_inhibit;
mod pointer_constraints;
mod relative_pointer;
pub mod session_lock;
pub mod xdg_activation;
mod xdg_decoration;
mod compositor;
pub mod layer_shell;
pub mod xdg_shell;

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_data_device, delegate_dmabuf, delegate_fractional_scale, delegate_output,
    delegate_presentation, delegate_primary_selection, delegate_seat, delegate_viewporter,
    input::{pointer::CursorImageStatus, Seat, SeatHandler, SeatState},
    reexports::wayland_server::{protocol::wl_surface::WlSurface, Resource},
    wayland::{
        compositor::{get_parent, with_states},
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::{with_fractional_scale, FractionalScaleHandler},
        output::OutputHandler,
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            primary_selection::{set_primary_focus, PrimarySelectionHandler, PrimarySelectionState},
            SelectionHandler,
        },
    },
};

use crate::Gavno;

impl SeatHandler for Gavno {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Gavno> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        if self.cursor_status == image { return; }
        self.cursor_status = image;
        self.queue_redraw_all();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let window = focused.and_then(|surface| self.window_for_surface(surface));
        self.guard(crate::module::Feature::Decorations, (), |gv| gv.window_decor.focus(window.as_ref()));
        self.queue_redraw_all();
        if let Some(serial) = seat.get_keyboard().and_then(|keyboard| keyboard.last_enter()) {
            self.record_activation_serial(focused.cloned(), serial);
        }
        self.update_shortcut_inhibitors(focused);
        self.refresh_constraints(focused);
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}
delegate_seat!(Gavno);
smithay::delegate_pointer_gestures!(Gavno);

impl SelectionHandler for Gavno {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Gavno {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Gavno {}
impl ServerDndGrabHandler for Gavno {}
delegate_data_device!(Gavno);

impl PrimarySelectionHandler for Gavno {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(Gavno);

impl OutputHandler for Gavno {}
delegate_output!(Gavno);

impl DmabufHandler for Gavno {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if crate::backend::import_dmabuf(self, &dmabuf) {
            let _ = notifier.successful::<Gavno>();
        } else {
            notifier.failed();
        }
    }
}
delegate_dmabuf!(Gavno);

delegate_viewporter!(Gavno);
delegate_presentation!(Gavno);

impl FractionalScaleHandler for Gavno {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Start with the scale of the output the window is on (or the first
        // output); render feedback refines it per frame afterwards.
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let output = self
            .window_for_surface(&root)
            .and_then(|w| self.space.outputs_for_element(&w).first().cloned())
            .or_else(|| self.active_output());
        if let Some(output) = output {
            let scale = output.current_scale().fractional_scale();
            with_states(&surface, |states| {
                with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
            });
        }
    }
}
delegate_fractional_scale!(Gavno);
