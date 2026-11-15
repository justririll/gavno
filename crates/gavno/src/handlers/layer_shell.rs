//! wlr-layer-shell: panels, launchers, wallpapers and overlays.
//!
//! Layer surfaces live in the per-output `LayerMap`; the `Space` already
//! renders them around the windows, so this file handles protocol events,
//! the work area, hit testing and keyboard focus.

use smithay::{
    delegate_layer_shell,
    desktop::{layer_map_for_output, LayerSurface, WindowSurfaceType},
    output::Output,
    reexports::wayland_server::protocol::{wl_output, wl_surface::WlSurface},
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        shell::wlr_layer::{
            KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface,
            LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
        },
    },
};

use crate::Gavno;

/// Marks a layer surface that was already given its exclusive keyboard focus.
struct ExclusiveFocusGranted;

impl WlrLayerShellHandler for Gavno {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.active_output());
        let Some(output) = output else {
            // No output to put it on (the protocol says to close it).
            surface.send_close();
            return;
        };
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface.clone(), namespace)) {
            tracing::warn!(?err, "failed to map layer surface");
            surface.send_close();
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let found = self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map.layers().find(|l| l.layer_surface() == &surface).cloned();
            layer.map(|l| (o.clone(), l))
        });
        if let Some((output, layer)) = found {
            layer_map_for_output(&output).unmap_layer(&layer);
            self.refit_fixed_windows();
        }
        let had_focus = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|f| &f == surface.wl_surface());
        if had_focus {
            self.focus_topmost();
        }
        self.queue_redraw_all();
    }
}
delegate_layer_shell!(Gavno);

/// Asks every layer surface on `output` to go away (the output is leaving).
pub fn close_layers(output: &Output) {
    for layer in layer_map_for_output(output).layers() {
        layer.layer_surface().send_close();
    }
}

/// Where a pointer hit a layer surface.
pub struct LayerHit {
    pub layer: LayerSurface,
    /// The surface (or subsurface/popup) under the pointer.
    pub surface: WlSurface,
    /// Global location of that surface's origin.
    pub surface_loc: Point<f64, Logical>,
}

impl Gavno {
    /// Output geometry minus the exclusive zones of its layer surfaces:
    /// where maximized and newly mapped windows go.
    pub fn work_area(&self, output: &Output) -> Rectangle<i32, Logical> {
        let geo = self.space.output_geometry(output).unwrap_or_default();
        let zone = layer_map_for_output(output).non_exclusive_zone();
        Rectangle::new(zone.loc + geo.loc, zone.size)
    }

    /// Re-arranges the layers of every output (outputs changed).
    pub fn arrange_layers_all(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in outputs {
            layer_map_for_output(&output).arrange();
        }
        self.refit_fixed_windows();
        self.queue_redraw_all();
    }

    /// The topmost layer surface among `layers` (front to back order) under `pos`.
    pub fn layer_hit(&self, pos: Point<f64, Logical>, layers: &[Layer]) -> Option<LayerHit> {
        let output = self
            .space
            .outputs()
            .find(|o| self.space.output_geometry(o).is_some_and(|g| g.to_f64().contains(pos)))?;
        let output_geo = self.space.output_geometry(output)?;
        let rel = pos - output_geo.loc.to_f64();
        let map = layer_map_for_output(output);
        let layer = layers.iter().find_map(|l| map.layer_under(*l, rel))?;
        let layer_loc = map.layer_geometry(layer)?.loc;
        let (surface, loc) = layer.surface_under(rel - layer_loc.to_f64(), WindowSurfaceType::ALL)?;
        Some(LayerHit {
            layer: layer.clone(),
            surface,
            surface_loc: (loc + layer_loc + output_geo.loc).to_f64(),
        })
    }

    /// A layer surface with exclusive keyboard interactivity that currently
    /// holds the keyboard focus (launchers): clicks elsewhere must not steal it.
    pub fn exclusive_layer_focused(&self) -> bool {
        let Some(focus) = self.seat.get_keyboard().and_then(|k| k.current_focus()) else {
            return false;
        };
        self.layer_for_surface(&focus)
            .is_some_and(|l| is_exclusive(&l) && matches!(l.layer(), Layer::Top | Layer::Overlay))
    }

    pub fn layer_for_surface(&self, surface: &WlSurface) -> Option<LayerSurface> {
        self.space.outputs().find_map(|o| {
            layer_map_for_output(o)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .cloned()
        })
    }

    /// Gives keyboard focus to a layer surface that accepts it (click on it).
    pub fn focus_layer(&mut self, layer: &LayerSurface) {
        if self.protocols.locks.locked { return; }
        if !layer.can_receive_keyboard_focus() {
            return;
        }
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(layer.wl_surface().clone()), SERIAL_COUNTER.next_serial());
    }
}

fn is_exclusive(layer: &LayerSurface) -> bool {
    layer.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
}

/// Called on every `wl_surface.commit`: initial configure, re-arranging and
/// exclusive keyboard focus for layer surfaces.
pub fn handle_commit(gv: &mut Gavno, surface: &WlSurface) {
    let Some(output) = gv
        .space
        .outputs()
        .find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
        .cloned()
    else {
        return;
    };

    let initial_configure_sent = with_states(surface, |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .unwrap()
            .lock()
            .unwrap()
            .initial_configure_sent
    });

    let layer = {
        let mut map = layer_map_for_output(&output);
        // Arrange first so the initial configure respects the requested size.
        map.arrange();
        let layer = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL).cloned();
        if !initial_configure_sent {
            if let Some(layer) = &layer {
                layer.layer_surface().send_configure();
            }
        }
        layer
    };
    gv.refit_fixed_windows();

    // A mapped Top/Overlay surface with exclusive interactivity takes the
    // keyboard focus once, when it first shows up.
    if let Some(layer) = layer {
        let mapped = smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| s.buffer().is_some())
            .unwrap_or(false);
        let eligible = mapped && is_exclusive(&layer) && matches!(layer.layer(), Layer::Top | Layer::Overlay);
        if eligible {
            let first_time = with_states(surface, |states| {
                states.data_map.insert_if_missing_threadsafe(|| ExclusiveFocusGranted)
            });
            if first_time {
                gv.focus_layer(&layer);
            }
        }
    }
}

