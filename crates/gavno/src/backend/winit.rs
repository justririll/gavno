//! Nested backend: GAVNO in a window of the host session. For development only.

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer, ImportDma, ImportMemWl},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::Transform,
    wayland::dmabuf::DmabufFeedbackBuilder,
};

use super::Backend;
use crate::{render, Gavno};

pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    damage_tracker: OutputDamageTracker,
    redraw_queued: bool,
}

impl WinitData {
    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn queue_redraw(&mut self) {
        if !self.redraw_queued {
            self.redraw_queued = true;
            self.backend.window().request_redraw();
        }
    }
}

pub fn init(event_loop: &mut EventLoop<'static, Gavno>, gv: &mut Gavno) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init::<GlesRenderer>()?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "GAVNO".into(),
            model: "nested".into(),
        },
    );
    let _global = output.create_global::<Gavno>(&gv.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    gv.space.map_output(&output, (0, 0));

    gv.shm_state.update_formats(backend.renderer().shm_formats());
    let formats = backend.renderer().dmabuf_formats();
    // Announce the GPU's render node: with main_device 0 clients cannot find
    // the GPU, so GTK falls back to llvmpipe and Vulkan to lavapipe, whose
    // buffers the host's EGL then fails to import.
    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .ok()
        .and_then(|device| device.try_get_render_node().ok().flatten());
    let feedback = render_node.and_then(|node| DmabufFeedbackBuilder::new(node.dev_id(), formats.clone()).build().ok());
    match feedback {
        Some(feedback) => {
            let _ = gv
                .dmabuf_state
                .create_global_with_default_feedback::<Gavno>(&gv.display_handle, &feedback);
        }
        None => {
            tracing::warn!("no render node for the nested renderer; dmabuf without feedback");
            let _ = gv.dmabuf_state.create_global::<Gavno>(&gv.display_handle, formats);
        }
    }

    let damage_tracker = OutputDamageTracker::from_output(&output);
    gv.backend = Backend::Winit(WinitData {
        backend,
        output,
        damage_tracker,
        redraw_queued: true,
    });

    event_loop.handle().insert_source(winit, |event, _, gv| match event {
        WinitEvent::Resized { size, .. } => {
            if let Backend::Winit(data) = &mut gv.backend {
                data.output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    None,
                    None,
                );
            }
            gv.arrange_layers_all();
        }
        WinitEvent::Input(event) => gv.process_input_event(event),
        WinitEvent::Redraw => render(gv),
        WinitEvent::CloseRequested => gv.quit(),
        _ => {}
    })?;
    Ok(())
}

fn render(gv: &mut Gavno) {
    let pointer_pos = gv.pointer_location();
    let time = gv.start_time.elapsed();
    gv.cursor.dimmed = gv.media.brightness.dimmed();
    let Backend::Winit(data) = &mut gv.backend else {
        return;
    };
    data.redraw_queued = false;
    let output = data.output.clone();

    let age = data.backend.buffer_age().unwrap_or(0);
    let result = {
        let Ok((renderer, mut framebuffer)) = data.backend.bind() else {
            return;
        };
        let mut elements = render::output_elements(
            renderer,
            &gv.space,
            &output,
            pointer_pos,
            &mut gv.cursor_status,
            &mut gv.cursor,
            time,
            gv.greeter.as_mut(),
        &mut gv.lock,
            &mut gv.overlay,
            &mut gv.window_decor,
            &mut gv.window_animations,
            &gv.protocols.locks,
        &mut gv.workspaces,
        &mut gv.overview,
        &mut gv.dock,
        &mut gv.topbar,
        &gv.config,
        &mut gv.features,
        &mut gv.media.osd,
        &mut gv.launcher,
        &mut gv.classic,
        &mut gv.polkit,
        );
        render::dim_output(&mut elements, &mut gv.media.brightness, &output, time);
        data.damage_tracker
            .render_output(renderer, &mut framebuffer, age, &elements, render::CLEAR_COLOR)
            .map(|r| (r.damage.cloned(), r.states))
    };
    match result {
        Ok((damage, states)) => {
            let mut submitted = true;
            if let Some(damage) = damage {
                if let Err(err) = data.backend.submit(Some(&damage)) {
                    submitted = false;
                    tracing::warn!(?err, "winit submit failed");
                }
            }
            if submitted {
                gv.protocols.locks.submitted(&output);
                gv.protocols.locks.presented(&output);
            }
            render::post_render(gv, &output, &states, None);
            if gv.is_animating() {
                gv.queue_redraw_all();
            }
        }
        Err(err) => tracing::warn!(?err, "winit render failed"),
    }
}

/// Renders the nested output offscreen; see `udev::capture_output`.
pub fn capture_output(gv: &mut Gavno, output: &Output, cursor: bool) -> Option<(u32, u32, Vec<u8>)> {
    // Hide the cursor through its status rather than a fake pointer
    // position: the dock and bar react to the pointer while building
    // elements, and a far-away pointer would make them retract.
    let saved = (!cursor).then(|| {
        std::mem::replace(&mut gv.cursor_status, smithay::input::pointer::CursorImageStatus::Hidden)
    });
    let result = capture_output_with_cursor_state(gv, output);
    if let Some(status) = saved {
        gv.cursor_status = status;
    }
    result
}

fn capture_output_with_cursor_state(gv: &mut Gavno, output: &Output) -> Option<(u32, u32, Vec<u8>)> {
    let pointer_pos = gv.pointer_location();
    let time = gv.start_time.elapsed();
    let Backend::Winit(data) = &mut gv.backend else { return None };
    if &data.output != output {
        return None;
    }
    let mode = output.current_mode()?;
    let scale = output.current_scale().fractional_scale();
    let renderer = data.backend.renderer();
    let elements = render::output_elements(
        renderer,
        &gv.space,
        output,
        pointer_pos,
        &mut gv.cursor_status,
        &mut gv.cursor,
        time,
        gv.greeter.as_mut(),
        &mut gv.lock,
        &mut gv.overlay,
        &mut gv.window_decor,
        &mut gv.window_animations,
        &gv.protocols.locks,
        &mut gv.workspaces,
        &mut gv.overview,
        &mut gv.dock,
        &mut gv.topbar,
        &gv.config,
        &mut gv.features,
        &mut gv.media.osd,
        &mut gv.launcher,
        &mut gv.classic,
        &mut gv.polkit,
    );
    let bytes = render::capture_elements(renderer, &elements, (mode.size.w, mode.size.h), scale)?;
    Some((mode.size.w as u32, mode.size.h as u32, bytes))
}
