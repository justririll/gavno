//! Building the element list for one output, shared by all backends, plus
//! the per-frame bookkeeping after a frame was drawn.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                default_primary_scanout_output_compare,
                utils::select_dmabuf_feedback,
                Kind, RenderElementStates,
            },
            gles::GlesRenderer,
        },
    },
    desktop::{
        space::{space_render_elements, SpaceRenderElements},
        utils::{
            send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
            surface_primary_scanout_output, update_surface_primary_scanout_output,
            with_surfaces_surface_tree, OutputPresentationFeedback,
        },
        layer_map_for_output, Space, Window,
    },
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    render_elements,
    utils::{IsAlive, Logical, Point, Scale, Transform},
    wayland::{
        compositor::with_states,
        dmabuf::DmabufFeedback,
        fractional_scale::with_fractional_scale,
    },
};
use xcursor::parser::Image;

use crate::{cursor::Cursor, Ds};

/// Background behind all windows until the wallpaper module exists.
pub const CLEAR_COLOR: [f32; 4] = [0.055, 0.063, 0.086, 1.0];

render_elements! {
    pub DsRenderElement<=GlesRenderer>;
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Memory = MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// Dmabuf feedback for one output: what to prefer for composition and what
/// can go straight to a scan-out plane.
#[derive(Debug, Clone)]
pub struct SurfaceDmabufFeedback {
    pub render: DmabufFeedback,
    pub scanout: DmabufFeedback,
}

/// The compositor cursor, uploaded once per (frame, scale).
pub struct CursorCache {
    cursor: Cursor,
    buffers: Vec<(Image, u32, MemoryRenderBuffer)>,
}

impl CursorCache {
    pub fn new() -> Self {
        Self {
            cursor: Cursor::load(),
            buffers: Vec::new(),
        }
    }

    /// Buffer and hotspot (logical px) for an integer `scale`.
    fn get(&mut self, scale: u32, time: Duration) -> (MemoryRenderBuffer, Point<f64, Logical>) {
        let image = self.cursor.image(scale, time);
        let hotspot = Point::from((image.xhot as f64 / scale as f64, image.yhot as f64 / scale as f64));
        if let Some((_, _, buf)) = self.buffers.iter().find(|(i, s, _)| *s == scale && i == &image) {
            return (buf.clone(), hotspot);
        }
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            scale as i32,
            Transform::Normal,
            None,
        );
        self.buffers.push((image, scale, buffer.clone()));
        (buffer, hotspot)
    }
}

/// Everything visible on `output`, front to back.
pub fn output_elements(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    output: &Output,
    pointer_pos: Point<f64, Logical>,
    cursor_status: &mut CursorImageStatus,
    cursor: &mut CursorCache,
    time: Duration,
) -> Vec<DsRenderElement> {
    let mut elements: Vec<DsRenderElement> = Vec::new();
    let Some(output_geo) = space.output_geometry(output) else {
        return elements;
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    if output_geo.to_f64().contains(pointer_pos) {
        if let CursorImageStatus::Surface(surface) = cursor_status {
            if !surface.alive() {
                *cursor_status = CursorImageStatus::default_named();
            }
        }
        let rel = pointer_pos - output_geo.loc.to_f64();
        match cursor_status {
            CursorImageStatus::Hidden => {}
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<std::sync::Mutex<CursorImageAttributes>>()
                        .map(|a| a.lock().unwrap().hotspot)
                        .unwrap_or_default()
                });
                let loc = (rel - hotspot.to_f64()).to_physical(scale).to_i32_round();
                elements.extend(
                    render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<_>>(
                        renderer,
                        surface,
                        loc,
                        scale,
                        1.0,
                        Kind::Cursor,
                    )
                    .into_iter()
                    .map(DsRenderElement::Surface),
                );
            }
            // Named shapes all draw the default arrow until cursor-shape lands.
            CursorImageStatus::Named(_) => {
                let int_scale = output.current_scale().integer_scale().max(1) as u32;
                let (buffer, hotspot) = cursor.get(int_scale, time);
                let loc = (rel - hotspot).to_physical(scale);
                if let Ok(elem) =
                    MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &buffer, None, None, None, Kind::Cursor)
                {
                    elements.push(DsRenderElement::Memory(elem));
                }
            }
        }
    }

    if let Ok(space_elements) = space_render_elements(renderer, [space], output, 1.0) {
        elements.extend(space_elements.into_iter().map(DsRenderElement::Space));
    }
    elements
}

/// After a frame was drawn (or found to be empty): pick each surface's
/// primary output, send frame callbacks and dmabuf feedback.
pub fn post_render(
    ds: &Ds,
    output: &Output,
    states: &RenderElementStates,
    feedback: Option<&SurfaceDmabufFeedback>,
) {
    let time = ds.clock.now();
    let throttle = Some(Duration::from_secs(1));

    for window in ds.space.elements() {
        window.with_surfaces(|surface, surface_states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_states,
                states,
                default_primary_scanout_output_compare,
            );
            if let Some(primary) = surface_primary_scanout_output(surface, surface_states) {
                let scale = primary.current_scale().fractional_scale();
                with_fractional_scale(surface_states, |fs| fs.set_preferred_scale(scale));
            }
        });
        if ds.space.outputs_for_element(window).contains(output) {
            window.send_frame(output, time, throttle, surface_primary_scanout_output);
            if let Some(feedback) = feedback {
                window.send_dmabuf_feedback(output, surface_primary_scanout_output, |surface, _| {
                    select_dmabuf_feedback(surface, states, &feedback.render, &feedback.scanout)
                });
            }
        }
    }

    layers_post_render(output, states, feedback, time, throttle);

    if let CursorImageStatus::Surface(surface) = &ds.cursor_status {
        if surface.alive() {
            with_surfaces_surface_tree(surface, |surface, surface_states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_states,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
            send_frames_surface_tree(surface, output, time, throttle, surface_primary_scanout_output);
        }
    }
}

/// `post_render` for layer surfaces (they are not in the space's element list).
fn layers_post_render(
    output: &Output,
    states: &RenderElementStates,
    feedback: Option<&SurfaceDmabufFeedback>,
    time: smithay::utils::Time<smithay::utils::Monotonic>,
    throttle: Option<Duration>,
) {
    let map = layer_map_for_output(output);
    for layer in map.layers() {
        layer.with_surfaces(|surface, surface_states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_states,
                states,
                default_primary_scanout_output_compare,
            );
            if let Some(primary) = surface_primary_scanout_output(surface, surface_states) {
                let scale = primary.current_scale().fractional_scale();
                with_fractional_scale(surface_states, |fs| fs.set_preferred_scale(scale));
            }
        });
        layer.send_frame(output, time, throttle, surface_primary_scanout_output);
        if let Some(feedback) = feedback {
            layer.send_dmabuf_feedback(output, surface_primary_scanout_output, |surface, _| {
                select_dmabuf_feedback(surface, states, &feedback.render, &feedback.scanout)
            });
        }
    }
}

/// Collects presentation feedback of everything shown on `output`.
pub fn take_presentation_feedback(
    space: &Space<Window>,
    output: &Output,
    states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut feedback = OutputPresentationFeedback::new(output);
    for window in space.elements() {
        if space.outputs_for_element(window).contains(output) {
            window.take_presentation_feedback(&mut feedback, surface_primary_scanout_output, |surface, _| {
                surface_presentation_feedback_flags_from_states(surface, states)
            });
        }
    }
    for layer in layer_map_for_output(output).layers() {
        layer.take_presentation_feedback(&mut feedback, surface_primary_scanout_output, |surface, _| {
            surface_presentation_feedback_flags_from_states(surface, states)
        });
    }
    feedback
}
