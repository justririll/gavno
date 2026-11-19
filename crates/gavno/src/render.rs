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
                utils::{Relocate, RelocateRenderElement, RescaleRenderElement},
                AsRenderElements, Kind, RenderElementStates,
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

use crate::{cursor::Cursor, module::{Feature, Features}, Gavno};

/// Safe fallback for trusted screens and failed theme initialization.
pub const CLEAR_COLOR: [f32; 4] = [0.055, 0.063, 0.086, 1.0];

render_elements! {
    pub GavnoRenderElement<=GlesRenderer>;
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Memory = MemoryRenderBufferRenderElement<GlesRenderer>,
    Desktop = crate::classic::cache::CachedElement,
    Color = crate::hdr::surface::ManagedElement,
    Window = crate::decor::WindowElement,
    Solid = smithay::backend::renderer::element::solid::SolidColorRenderElement,
    Shader = smithay::backend::renderer::gles::element::PixelShaderElement,
    Thumbnail = crate::overview::ThumbnailElement,
    Plain = RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
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
    /// KMS cursor planes sit above the primary plane, including its dimmer.
    pub dimmed: bool,
    /// Suppresses both software elements and KMS cursor-plane candidates.
    pub hidden: bool,
    cursor: Cursor,
    buffers: Vec<(Image, u32, MemoryRenderBuffer)>,
}

impl CursorCache {
    pub fn animated(&self, name: &str) -> bool {
        self.cursor.animated(name)
    }
    pub fn new() -> Self {
        Self {
            dimmed: false,
            hidden: false,
            cursor: Cursor::load(),
            buffers: Vec::new(),
        }
    }

    /// Buffer and hotspot (logical px) for an integer `scale`.
    fn get(&mut self, name: &str, scale: u32, time: Duration) -> (MemoryRenderBuffer, Point<f64, Logical>) {
        let image = self.cursor.image(name, scale, time);
        // The theme may lack the size for `scale` (or fall back to the 24px
        // arrow): derive the buffer scale from the image actually chosen so
        // the cursor keeps its logical size.
        let base = self.cursor.base_size().max(1) as f64;
        let scale = ((image.size.max(image.width) as f64 / base).round() as u32).clamp(1, scale.max(1));
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
    features: &mut Features,
    osd: &mut crate::topbar::osd::Osd,
    launcher: &mut crate::launcher::Launcher,
    classic: &mut crate::classic::Classic,
    polkit: &mut crate::polkit::Polkit,
    background: &mut crate::ui::background::Background,
    mut stall: Option<&mut crate::perf::Stall<'_>>,
) -> Vec<GavnoRenderElement> {
    if let Some(stall) = stall.as_deref_mut() {
        stall.shm_bytes(crate::render_sanitize::take_output_shm_bytes(output));
    }
    let accent = features.call(Feature::Theme, crate::ui::background::FIXED_ACCENT,
        || background.accent_for(output, &config.theme, time));
    topbar.set_accent(accent);
    let mut descriptions = std::collections::HashMap::new();
    if features.on(Feature::ColorManagement) && crate::handlers::color_management::any_tagged() {
        // The walks below hold each surface's lock: read the locked states,
        // never call with_states on the same surface (that deadlocks).
        let mut collect = |surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
                           states: &smithay::wayland::compositor::SurfaceData| {
            if let Some(description) = crate::handlers::color_management::states_description(states) {
                descriptions.insert(crate::handlers::color_management::surface_id(surface), description);
            }
        };
        for window in space.elements() { window.with_surfaces(|surface, states| collect(surface, states)); }
        for layer in layer_map_for_output(output).layers() { layer.with_surfaces(|surface, states| collect(surface, states)); }
        if let CursorImageStatus::Surface(surface) = cursor_status { with_surfaces_surface_tree(surface, |s, states| collect(s, states)); }
        if let Some(surface) = locks.surface(output) { with_surfaces_surface_tree(surface, |s, states| collect(s, states)); }
    }
    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.state"); }
    let elements = output_elements_inner(renderer, space, output, pointer_pos, cursor_status, cursor, time, greeter, lock, overlay, window_decor, animations, locks, workspaces, overview, dock, topbar, config, features, osd, launcher, classic, polkit, background, stall.as_deref_mut());
    if descriptions.is_empty() { crate::hdr::surface::reset(output); return elements; }
    let hdr = crate::handlers::color_management::is_hdr(output);
    if !hdr { descriptions.retain(|_, d| d.tf != crate::color::Transfer::Srgb || d.primaries != crate::color::SRGB); }
    if descriptions.is_empty() { crate::hdr::surface::reset(output); return elements; }
    let white = if hdr { crate::handlers::color_management::output_description(output).luminances[2] as f32 } else { config.sdr_white_nits };
    let converted = features.call(Feature::ColorManagement, Ok(Default::default()), || {
        crate::hdr::surface::convert(renderer, output, &elements, &descriptions, hdr, white)
    });
    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.color"); }
    match converted {
        Ok(converted) => {
            let mut converted = converted.into_iter();
            elements.into_iter().map(|element| converted.next().flatten()
                .map(GavnoRenderElement::Color).unwrap_or(element)).collect()
        },
        Err(error) => {
            features.failed(Feature::ColorManagement, format!("surface color conversion failed: {error}"));
            elements
        }
    }
}

/// Display-only pass, in front of the cursor and all trusted UI. Captures use
/// `output_elements` directly and therefore retain the original pixel values.
pub fn dim_output(elements: &mut Vec<GavnoRenderElement>, brightness: &mut crate::brightness::State, output: &Output, time: Duration) {
    if let Some(element) = brightness.element(output, time) {
        elements.insert(0, GavnoRenderElement::Solid(element));
    }
}

fn output_elements_inner(
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
    features: &mut Features,
    osd: &mut crate::topbar::osd::Osd,
    launcher: &mut crate::launcher::Launcher,
    classic: &mut crate::classic::Classic,
    polkit: &mut crate::polkit::Polkit,
    background: &mut crate::ui::background::Background,
    mut stall: Option<&mut crate::perf::Stall<'_>>,
) -> Vec<GavnoRenderElement> {
    let mut elements: Vec<GavnoRenderElement> = Vec::new();
    let Some(output_geo) = space.output_geometry(output) else {
        return elements;
    };
    let scale = Scale::from(output.current_scale().fractional_scale());
    topbar.shell_height = output_geo.size.h;
    let cursor_kind = if cursor.dimmed { Kind::Unspecified } else { Kind::Cursor };

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
                    renderer, loc, &buffer, None, None, None, cursor_kind,
                ) { elements.push(GavnoRenderElement::Memory(cursor)); }
            }
            if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.cursor"); }
            elements.extend(osd.elements(renderer, output, output_geo.size, time, &config.dock.icon_theme, topbar.accent()));
            let name = output.name();
            if lock.backdrop_wanted(&name) {
                // The first lock frame captures the desktop once; the blur happens
                // on a worker and arrives as a small texture a frame or two later.
                lock.set_capturing(true);
                let open = crate::handlers::session_lock::Locks::default();
                let desktop = output_elements_inner(renderer, space, output, pointer_pos, cursor_status, cursor, time, None, lock, overlay, window_decor, animations, &open, workspaces, overview, dock, topbar, config, features, osd, launcher, classic, polkit, background, None);
                lock.set_capturing(false);
                let physical = (
                    (output_geo.size.w as f64 * scale.x).round() as i32,
                    (output_geo.size.h as f64 * scale.x).round() as i32,
                );
                match capture_elements(renderer, &desktop, physical, scale.x) {
                    Some(rgba) => lock.start_backdrop(&name, rgba, physical.0 as usize, physical.1 as usize),
                    None => lock.start_backdrop(&name, Vec::new(), 0, 0),
                }
            }
            let battery = topbar.status().battery.as_ref().map(|b| (b.percent, b.charging));
            let lock_elements = lock.elements(renderer, &name, output_geo.size, scale.x, time, &config.theme, battery);
            let media_elements = features.call(Feature::Media, Vec::new(), || {
                lock.media_elements(renderer, output_geo.size, scale.x, &topbar.now_playing.borrow().snapshot, &config.dock.icon_theme)
            });
            // Media is in front of the trusted lock background, beneath its cursor.
            elements.extend(media_elements);
            elements.extend(lock_elements);
            // Desktop pixels are permitted only after local PAM success. Input
            // remains blocked until the fade completes on the event loop.
            if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.shell"); }
            if !lock.desktop_visible() { return elements; }
        } else {
            elements.extend(osd.elements(renderer, output, output_geo.size, time, &config.dock.icon_theme, topbar.accent()));
            if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.shell"); }
            if let Some(surface) = locks.surface(output) {
                elements.extend(render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<_>>(
                    renderer, surface, (0, 0), scale, 1.0, Kind::Unspecified,
                ).into_iter().map(GavnoRenderElement::Surface));
            }
            if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.space"); }
            return elements;
        }
    }

    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.lock"); }
    if !cursor.hidden && !locks.locked && output_geo.to_f64().contains(pointer_pos) {
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
                        cursor_kind,
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
                    MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &buffer, None, None, None, cursor_kind)
                {
                    elements.push(GavnoRenderElement::Memory(elem));
                }
            }
        }
    }

    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.cursor"); }
    if !locks.locked { elements.extend(osd.elements(renderer, output, output_geo.size, time, &config.dock.icon_theme, topbar.accent())); }

    if let Some(greeter) = greeter {
        let size = output_geo.size;
        elements.extend(greeter.safe_call(Vec::new(), |g| { g.set_location(&config.theme); g.elements(renderer, size, scale.x, time) }));
        if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.shell"); }
        return elements;
    }

    // The screenshot overlay freezes the output: nothing below it renders.
    // Its toast floats above the live desktop.
    if !locks.locked && !lock.shell_hidden() {
        let shot = overlay.screenshot.elements(renderer, output, output_geo.size, scale.x, time, topbar.accent());
        elements.extend(shot.elements);
        if shot.frozen {
            if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.shell"); }
            return elements;
        }
    }

    // The polkit dialog is modal: above every window and shell panel, beneath
    // only the cursor and OSD. It waits hidden while the session is locked.
    if !locks.locked && !lock.shell_hidden() && !polkit.queue.is_empty() {
        let here = match &polkit.output {
            Some(shown) => shown == output || space.output_geometry(shown).is_none(),
            None => true,
        };
        if here {
            let crate::polkit::Polkit { queue, view, .. } = polkit;
            let dialog = &queue[0];
            let accent = topbar.accent();
            let theme = &config.dock.icon_theme;
            elements.extend(features.call(Feature::Polkit, Vec::new(), || {
                view.elements(renderer, dialog, output_geo.size, scale.x, accent, theme)
            }));
        }
    }

    if !locks.locked && !lock.shell_hidden() {
        let accent = topbar.accent();
        let media_on = features.on(Feature::Media);
        elements.extend(features.call(Feature::Bar, Vec::new(), || topbar.panel_elements(renderer, output, output_geo.size, scale.x, accent, &config.dock.icon_theme, classic.progress.value > 0.5, media_on)));
    }

    if !locks.locked && !lock.shell_hidden() {
        elements.extend(features.call(Feature::Media, Vec::new(), || {
            topbar.now_playing.borrow_mut().music.session.as_mut()
                .map(|s| s.elements(renderer, output, output_geo.size, scale.x, topbar.accent())).unwrap_or_default()
        }));
    }

    let p = features.call(Feature::Classic, 0.0, || {
        classic.advance(time);
        classic.progress.value.clamp(0.0, 1.0)
    });
    topbar.shell_progress = p;
    topbar.shell_height = output_geo.size.h;
    dock.shell_progress = p;
    window_decor.classic = features.on(Feature::Classic) && classic.settled == crate::classic::ShellMode::Classic;
    window_decor.pointer = pointer_pos;
    let [r, g, b, _] = topbar.accent();
    window_decor.style = crate::decor::FocusStyle { dim: config.appearance.dim_inactive as f32, border: config.appearance.focus_border, accent: [r, g, b] };
    elements.extend(features.call(Feature::ResizeMode, Vec::new(), || overlay.resize_mode.elements(renderer, output, output_geo, scale.x)));
    if !locks.locked && !lock.shell_hidden() {
        elements.extend(features.call(Feature::Notifications, Vec::new(), || {
            topbar.notifications.borrow_mut().elements(renderer, output, output_geo.size, scale.x, time, &config.dock.icon_theme, topbar.shell_progress, topbar.accent())
        }));
    }
    let accent = topbar.accent();
    elements.extend(overlay.keyboard.elements(renderer, output, output_geo, space, scale.x, accent));
    elements.extend(features.call(Feature::Launcher, Vec::new(), || {
        launcher.elements(renderer, output, output_geo.size, scale.x, time, accent, &config.dock.icon_theme)
    }));
    if !locks.locked && !lock.shell_hidden() {
        elements.extend(overlay.clipboard.picker_elements(renderer, output, output_geo.size, scale.x, time, accent));
    }
    elements.extend(features.call(Feature::Switcher, Vec::new(), || crate::overlay::overlay_elements(overlay, renderer, output, output_geo, scale.x, time)));
    let shell_visible = !space.elements().any(|w| crate::window_mode::mode_state(w).mode == crate::window_mode::WindowMode::Fullscreen
        && space.element_location(w).is_some_and(|p| output_geo.contains(p)));
    let zone = layer_map_for_output(output).non_exclusive_zone();
    let work_area = crate::classic::reserve(smithay::utils::Rectangle::new(zone.loc + output_geo.loc, zone.size),
        if features.on(Feature::Classic) { classic.settled } else { crate::classic::ShellMode::Gavno },
        features.on(Feature::Bar) && config.bar.enabled, features.on(Feature::Dock) && config.dock.enabled, config.dock.autohide);
    features.call(Feature::Classic, (), || classic.desktop.append_overlay(renderer, output, &mut elements, scale.x));
    let focused = classic.taskbar.focused.clone();
    features.call(Feature::Classic, (), || classic.start.append(renderer, output, output_geo.size, time, config.classic.align, accent, &config.dock.icon_theme, &mut elements));
    features.call(Feature::Classic, (), || classic.taskbar.append(renderer, output, output_geo.size, p, config.classic.align, focused.as_ref(), topbar.status(), accent, &config.dock.icon_theme, shell_visible, &mut elements));
    elements.extend(features.read(Feature::Bar, Vec::new(), || topbar.elements(renderer, output, output_geo.size.w, workspaces.get(output), time, shell_visible || (p > 0.0 && topbar.menu.as_ref() == Some(output)), features)));
    // Above the dock and every window, beneath the bar and its popovers.
    elements.extend(overlay.dropdown.elements(renderer, window_decor, output, output_geo, work_area, &config.dropdown, time));
    elements.extend(features.call(Feature::Dock, Vec::new(), || dock.elements(renderer, output, output_geo.size, pointer_pos - output_geo.loc.to_f64(), time, shell_visible)));
    // Workspace ownership and transitions are core; overview only adds its zoom.
    let mut scene = if features.on(Feature::Overview) {
        features.call(Feature::Overview, None, || {
            overview.work_area = Some(work_area);
            overview.scene(workspaces, space, output, time)
        })
    } else {
        crate::workspace::workspace_scene(workspaces, space, output, time)
    };
    features.call(Feature::Dock, (), || dock.minimize_scene(space, output, time, &mut scene));
    elements.extend(features.call(Feature::Overview, Vec::new(), || overview.chrome(renderer, workspaces, space, output, accent)));
    let accent = topbar.accent();
    let overview_visible = features.on(Feature::Overview) && overview.visible();
    elements.extend(features.call(Feature::Tiling, Vec::new(), || {
        let mut line = Vec::new();
        if let Some((hover_output, split)) = &workspaces.splitter_hover {
            if hover_output == output && !overview_visible {
                let mut rect = split.line();
                rect.loc -= output_geo.loc;
                workspaces.splitter_canvas.begin(renderer, scale.x);
                workspaces.splitter_canvas.rect(rect, 1.0, accent, 0.0, [0.0; 4]);
                line.extend(workspaces.splitter_canvas.finish());
            }
        }
        line
    }));
    features.call(Feature::Animations, (), || animations.advance(time, space));
    let mut static_animations = crate::anim::WindowAnimations::default();
    let animations = if features.on(Feature::Animations) { animations } else { &mut static_animations };
    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.shell"); }
    let decorated = features.read(Feature::Decorations, None, || Some(window_decor.elements(renderer, space, output, animations, time, scene.as_deref(), features, stall.as_deref_mut())));
    elements.extend(decorated.unwrap_or_else(|| {
        plain_elements(renderer, space, output, scene.as_deref(), stall.as_deref_mut())
    }));
    features.call(Feature::Classic, (), || classic.desktop.append(renderer, output, &mut elements, p, scale.x, accent, &config.dock.icon_theme));
    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.desktop"); }
    elements.extend(background_elements(renderer, output, smithay::wayland::shell::wlr_layer::Layer::Bottom));
    background.record_visibility(output, &elements, output_geo.size, scale.x);
    elements.extend(background_elements(renderer, output, smithay::wayland::shell::wlr_layer::Layer::Background));
    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.layers.back"); }
    if !crate::ui::background::has_wallpaper(output, output_geo.size) {
        let sky = features.call(Feature::Theme, None, || {
            background.elements(renderer, output, output_geo.size, scale.x, &config.theme, time)
        });
        elements.push(sky.unwrap_or_else(|| background.solid(output, output_geo.size, config.theme.solid())));
    }
    if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.background"); }
    elements
}

/// Core surface rendering survives decoration failure, including workspace,
/// overview and dock scene transforms. It needs no decoration shaders or caches.
pub(crate) fn plain_elements(
    renderer: &mut GlesRenderer, space: &Space<Window>, output: &Output,
    scene: Option<&[crate::overview::SceneWindow]>,
    mut stall: Option<&mut crate::perf::Stall<'_>>,
) -> Vec<GavnoRenderElement> {
    let fallback;
    let scene = if let Some(scene) = scene { scene } else {
        let origin = space.output_geometry(output).unwrap_or_default().loc;
        fallback = space.elements().filter_map(|window| {
            let location = space.element_location(window)?;
            Some(crate::overview::SceneWindow { window: window.clone(), location,
                rect: smithay::utils::Rectangle::new((location - origin).to_f64(), window.geometry().size.to_f64()) })
        }).collect::<Vec<_>>();
        &fallback
    };
    let scale = Scale::from(output.current_scale().fractional_scale());
    let layers = layer_map_for_output(output);
    let mut result = Vec::new();
    for upper in [true, false] {
        if !upper {
            for v in scene.iter().rev() {
                let geometry = v.window.geometry();
                let ratio = Scale::from((v.rect.size.w / geometry.size.w.max(1) as f64,
                    v.rect.size.h / geometry.size.h.max(1) as f64));
                let origin = Point::<f64, Logical>::from((-geometry.loc.x as f64, -geometry.loc.y as f64)).to_physical(scale).to_i32_round();
                for element in v.window.render_elements::<WaylandSurfaceRenderElement<_>>(renderer, origin, scale, 1.0) {
                    let element = RescaleRenderElement::from_element(element, (0, 0).into(), ratio);
                    let element = RelocateRenderElement::from_element(element,
                        v.rect.loc.to_physical(scale).to_i32_round(), Relocate::Relative);
                    result.push(GavnoRenderElement::Plain(element));
                }
            }
        }
        if !upper {
            if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.space"); }
        }
        for layer in layers.layers().rev().filter(|l| upper && matches!(l.layer(), smithay::wayland::shell::wlr_layer::Layer::Top | smithay::wayland::shell::wlr_layer::Layer::Overlay)) {
            if let Some(geometry) = layers.layer_geometry(layer) {
                result.extend(layer.render_elements::<WaylandSurfaceRenderElement<_>>(
                    renderer, geometry.loc.to_physical_precise_round(scale), scale, 1.0,
                ).into_iter().map(GavnoRenderElement::Surface));
            }
        }
        if upper {
            if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.layers.front"); }
        }
    }
    result
}

fn background_elements(renderer: &mut GlesRenderer, output: &Output, selected: smithay::wayland::shell::wlr_layer::Layer) -> Vec<GavnoRenderElement> {
    let layers = layer_map_for_output(output);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let mut result = Vec::new();
    for layer in layers.layers().rev().filter(|l| l.layer() == selected) {
        if let Some(geometry) = layers.layer_geometry(layer) {
            result.extend(layer.render_elements::<WaylandSurfaceRenderElement<_>>(
                renderer, geometry.loc.to_physical_precise_round(scale), scale, 1.0,
            ).into_iter().map(GavnoRenderElement::Surface));
        }
    }
    result
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
                crate::handlers::color_management::surface_output(surface_states, &primary);
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

    if let Some(window) = gv.overlay.dropdown.rendered(output) {
        window.with_surfaces(|surface, surface_states| {
            update_surface_primary_scanout_output(surface, output, surface_states, states, default_primary_scanout_output_compare);
            if let Some(primary) = surface_primary_scanout_output(surface, surface_states) {
                let scale = primary.current_scale().fractional_scale();
                with_fractional_scale(surface_states, |fs| fs.set_preferred_scale(scale));
            }
        });
        window.send_frame(output, time, throttle, surface_primary_scanout_output);
        if let Some(feedback) = feedback {
            window.send_dmabuf_feedback(output, surface_primary_scanout_output, |surface, _| {
                select_dmabuf_feedback(surface, states, &feedback.render, &feedback.scanout)
            });
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
                crate::handlers::color_management::surface_output(surface_states, &primary);
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
