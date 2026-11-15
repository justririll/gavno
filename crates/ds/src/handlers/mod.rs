mod compositor;
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

use crate::Ds;

impl SeatHandler for Ds {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Ds> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.queue_redraw_all();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}
delegate_seat!(Ds);

impl SelectionHandler for Ds {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Ds {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Ds {}
impl ServerDndGrabHandler for Ds {}
delegate_data_device!(Ds);

impl PrimarySelectionHandler for Ds {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(Ds);

impl OutputHandler for Ds {}
delegate_output!(Ds);

impl DmabufHandler for Ds {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if crate::backend::import_dmabuf(self, &dmabuf) {
            let _ = notifier.successful::<Ds>();
        } else {
            notifier.failed();
        }
    }
}
delegate_dmabuf!(Ds);

delegate_viewporter!(Ds);
delegate_presentation!(Ds);

impl FractionalScaleHandler for Ds {
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
delegate_fractional_scale!(Ds);
