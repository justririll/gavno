use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    delegate_compositor, delegate_shm,
    reexports::wayland_server::{
        protocol::{wl_buffer, wl_surface::WlSurface},
        Client,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler, CompositorState},
        shm::{ShmHandler, ShmState},
    },
};

use super::{layer_shell, xdg_shell};
use crate::{grabs::resize_grab, state::ClientState, Gavno};

impl CompositorHandler for Gavno {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) { root = parent; }
        let mut hidden_commit = self.window_for_surface(&root).is_some_and(|w| self.space.element_location(&w).is_none());

        if !is_sync_subsurface(surface) {
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
                let has_buffer = with_renderer_surface_state(&root, |s| s.buffer().is_some()).unwrap_or(false);
                hidden_commit = self.space.element_location(&window).is_none();
                if !hidden_commit { crate::anim::surface_commit(self, &window, has_buffer); }
            }

            // First buffer of a new toplevel: place and show it.
            if let Some(idx) = self
                .unmapped
                .iter()
                .position(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
            {
                let has_buffer = with_renderer_surface_state(&root, |s| s.buffer().is_some()).unwrap_or(false);
                let window = self.unmapped[idx].clone();
                window.on_commit();
                if has_buffer {
                    self.unmapped.remove(idx);
                    self.map_new_window(window);
                }
            }
        }

        xdg_shell::handle_commit(self, surface);
        layer_shell::handle_commit(self, surface);
        resize_grab::handle_commit(&mut self.space, surface);
        if let Some(w)=self.window_for_surface(surface).filter(|w| resize_grab::directed_finished(w) && !self.window_animations.motion(w).is_some_and(|m|m.closing)) {
            resize_grab::settle_directed(self, &w);
        }
        if !hidden_commit || self.overview.visible() { self.queue_redraw_all(); }
    }
}

impl BufferHandler for Gavno {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Gavno {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(Gavno);
delegate_shm!(Gavno);
