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
        space::SpaceRenderElements,
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

use crate::{cursor::Cursor, Gavno};

/// Background behind all windows until the wallpaper module exists.
pub const CLEAR_COLOR: [f32; 4] = [0.055, 0.063, 0.086, 1.0];

render_elements! {
    pub GavnoRenderElement<=GlesRenderer>;
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Memory = MemoryRenderBufferRenderElement<GlesRenderer>,
    Window = crate::decor::WindowElement,
    Shader = smithay::backend::renderer::gles::element::PixelShaderElement,
    Thumbnail = crate::overview::ThumbnailElement,
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
    fn get(&mut self, name: &str, scale: u32, time: Duration) -> (MemoryRenderBuffer, Point<f64, Logical>) {
        let image = self.cursor.image(name, scale, time);
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
    greeter: Option<&mut crate::greeter::Greeter>,
    lock: &mut crate::lock::Lock,
    overlay: &mut crate::overlay::Overlay,
    window_decor: &mut crate::decor::WindowDecor,
    animations: &mut crate::anim::WindowAnimations,
    locks: &crate::handlers::session_lock::Locks,
    workspaces: &mut crate::workspace::Workspaces,
    overview: &mut crate::overview::Overview,
    dock: &mut crate::dock::Dock,
    topbar: &mut crate::topbar::TopBar,
    config: &crate::config::Config,
) -> Vec<GavnoRenderElement> {
    let mut elements: Vec<GavnoRenderElement> = Vec::new();
    let Some(output_geo) = space.output_geometry(output) else {
        return elements;
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Locked sessions expose only locker surfaces; the normal clear color is
    // the fail-closed fallback when the locker has no buffer or has crashed.
    if locks.locked {
        if lock.internal() {
            // Use only the compositor's trusted cursor while clients have no input.
            if output_geo.to_f64().contains(pointer_pos) {
                let int_scale = output.current_scale().integer_scale().max(1) as u32;
                let (buffer, hotspot) = cursor.get("default", int_scale, time);
                let loc = (pointer_pos - output_geo.loc.to_f64() - hotspot).to_physical(scale);
                if let Ok(cursor) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer, loc, &buffer, None, None, None, Kind::Cursor,
                ) { elements.push(GavnoRenderElement::Memory(cursor)); }
            }
            elements.extend(topbar.osd.elements(renderer, output, output_geo.size, time, &config.dock.icon_theme, topbar.accent()));
            elements.extend(lock.elements(renderer, output_geo.size, scale.x, time));
            // Desktop pixels are permitted only after local PAM success. Input
            // remains blocked until the fade completes on the event loop.
            if !lock.desktop_visible() { return elements; }
        } else {
            elements.extend(topbar.osd.elements(renderer, output, output_geo.size, time, &config.dock.icon_theme, topbar.accent()));
            if let Some(surface) = locks.surface(output) {
                elements.extend(render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<_>>(
                    renderer, surface, (0, 0), scale, 1.0, Kind::Unspecified,
                ).into_iter().map(GavnoRenderElement::Surface));
            }
            return elements;
        }
    }

    if !locks.locked && output_geo.to_f64().contains(pointer_pos) {
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
                    .map(GavnoRenderElement::Surface),
                );
            }
            CursorImageStatus::Named(icon) => {
                let int_scale = output.current_scale().integer_scale().max(1) as u32;
                let (buffer, hotspot) = cursor.get(icon.name(), int_scale, time);
                let loc = (rel - hotspot).to_physical(scale);
                if let Ok(elem) =
                    MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &buffer, None, None, None, Kind::Cursor)
                {
                    elements.push(GavnoRenderElement::Memory(elem));
                }
            }
        }
    }

    if !locks.locked { elements.extend(topbar.osd.elements(renderer, output, output_geo.size, time, &config.dock.icon_theme, topbar.accent())); }

    if let Some(greeter) = greeter {
        let size = output_geo.size;
        elements.extend(greeter.elements(renderer, size, scale.x, time));
        return elements;
    }

    elements.extend(crate::overlay::overlay_elements(overlay, renderer, output, output_geo, scale.x, time));
    let shell_visible = !space.elements().any(|w| crate::window_mode::mode_state(w).mode == crate::window_mode::WindowMode::Fullscreen
        && space.element_location(w).is_some_and(|p| output_geo.contains(p)));
    let zone = layer_map_for_output(output).non_exclusive_zone();
    let work_area = crate::dock::reserve(smithay::utils::Rectangle::new(zone.loc + output_geo.loc, zone.size), config.bar.enabled, config.dock.enabled, config.dock.autohide);
    elements.extend(topbar.elements(renderer, output, output_geo.size.w, workspaces.get(output), time, shell_visible));
    elements.extend(dock.elements(renderer, output, output_geo.size, pointer_pos - output_geo.loc.to_f64(), time, shell_visible));
    overview.work_area = Some(work_area);
    let mut scene = overview.scene(workspaces, space, output, time);
    dock.minimize_scene(space, output, time, &mut scene);
    elements.extend(overview.chrome(renderer, workspaces, space, output));
    if let Some((hover_output, split)) = &workspaces.splitter_hover {
        if hover_output == output && !overview.visible() {
            let mut rect = split.line();
            rect.loc -= output_geo.loc;
            workspaces.splitter_canvas.begin(renderer, scale.x);
            workspaces.splitter_canvas.rect(rect, 1.0, topbar.accent(), 0.0, [0.0; 4]);
            elements.extend(workspaces.splitter_canvas.finish());
        }
    }
    elements.extend(window_decor.elements(renderer, space, output, animations, time, scene.as_deref()));
    elements
}

/// After a frame was drawn (or found to be empty): pick each surface's
/// primary output, send frame callbacks and dmabuf feedback.
pub fn post_render(
    gv: &Gavno,
    output: &Output,
    states: &RenderElementStates,
    feedback: Option<&SurfaceDmabufFeedback>,
) {
    let time = gv.clock.now();
    let throttle = Some(Duration::from_secs(1));

    if gv.protocols.locks.locked {
        if let Some(surface) = gv.protocols.locks.surface(output) {
            with_surfaces_surface_tree(surface, |surface, surface_states| {
                update_surface_primary_scanout_output(surface, output, surface_states, states, default_primary_scanout_output_compare);
                with_fractional_scale(surface_states, |fs| fs.set_preferred_scale(output.current_scale().fractional_scale()));
            });
            send_frames_surface_tree(surface, output, time, throttle, surface_primary_scanout_output);
        }
        return;
    }

    for window in gv.space.elements() {
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
        if gv.space.outputs_for_element(window).contains(output) {
            window.send_frame(output, time, throttle, surface_primary_scanout_output);
            if let Some(feedback) = feedback {
                window.send_dmabuf_feedback(output, surface_primary_scanout_output, |surface, _| {
                    select_dmabuf_feedback(surface, states, &feedback.render, &feedback.scanout)
                });
            }
        }
    }

    layers_post_render(output, states, feedback, time, throttle);

    if let CursorImageStatus::Surface(surface) = &gv.cursor_status {
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

/// Renders `elements` into an offscreen texture of `size` physical pixels and
/// returns tightly packed RGBA bytes. Used by screenshots and screencopy.
pub fn capture_elements(
    renderer: &mut GlesRenderer,
    elements: &[GavnoRenderElement],
    size: (i32, i32),
    scale: f64,
) -> Option<Vec<u8>> {
    use smithay::backend::renderer::{damage::OutputDamageTracker, gles::GlesTexture, Bind, ExportMem, Offscreen};
    use smithay::utils::{Buffer as BufferCoord, Rectangle, Size, Transform};

    let buffer_size: Size<i32, BufferCoord> = size.into();
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
    let mapping = {
        let mut fb = renderer.bind(&mut texture).ok()?;
        let mut damage = OutputDamageTracker::new(size, scale, Transform::Normal);
        damage.render_output(renderer, &mut fb, 0, elements, CLEAR_COLOR).ok()?;
        renderer
            .copy_framebuffer(&fb, Rectangle::from_size(buffer_size), Fourcc::Abgr8888)
            .ok()?
    };
    Some(renderer.map_texture(&mapping).ok()?.to_vec())
}
