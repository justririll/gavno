//! Nested backend: ds in a window of the host session. For development only.

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer, ImportDma, ImportMemWl},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::Transform,
    wayland::dmabuf::DmabufFeedbackBuilder,
};

use super::Backend;
use crate::{render, Ds};

pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    damage_tracker: OutputDamageTracker,
    redraw_queued: bool,
}

impl WinitData {
    pub fn queue_redraw(&mut self) {
        if !self.redraw_queued {
            self.redraw_queued = true;
            self.backend.window().request_redraw();
        }
    }
}

pub fn init(event_loop: &mut EventLoop<'static, Ds>, ds: &mut Ds) -> Result<(), Box<dyn std::error::Error>> {
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
            make: "ds".into(),
            model: "nested".into(),
        },
    );
    let _global = output.create_global::<Ds>(&ds.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    ds.space.map_output(&output, (0, 0));

    ds.shm_state.update_formats(backend.renderer().shm_formats());
    let formats = backend.renderer().dmabuf_formats();
    if let Ok(feedback) = DmabufFeedbackBuilder::new(0, formats.clone()).build() {
        let _ = ds
            .dmabuf_state
            .create_global_with_default_feedback::<Ds>(&ds.display_handle, &feedback);
    } else {
        let _ = ds.dmabuf_state.create_global::<Ds>(&ds.display_handle, formats);
    }

    let damage_tracker = OutputDamageTracker::from_output(&output);
    ds.backend = Backend::Winit(WinitData {
        backend,
        output,
        damage_tracker,
        redraw_queued: true,
    });

    event_loop.handle().insert_source(winit, |event, _, ds| match event {
        WinitEvent::Resized { size, .. } => {
            if let Backend::Winit(data) = &mut ds.backend {
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
            ds.queue_redraw_all();
        }
        WinitEvent::Input(event) => ds.process_input_event(event),
        WinitEvent::Redraw => render(ds),
        WinitEvent::CloseRequested => ds.quit(),
        _ => {}
    })?;
    Ok(())
}

fn render(ds: &mut Ds) {
    let pointer_pos = ds.pointer_location();
    let time = ds.start_time.elapsed();
    let Backend::Winit(data) = &mut ds.backend else {
        return;
    };
    data.redraw_queued = false;
    let output = data.output.clone();

    let age = data.backend.buffer_age().unwrap_or(0);
    let result = {
        let Ok((renderer, mut framebuffer)) = data.backend.bind() else {
            return;
        };
        let elements = render::output_elements(
            renderer,
            &ds.space,
            &output,
            pointer_pos,
            &mut ds.cursor_status,
            &mut ds.cursor,
            time,
        );
        data.damage_tracker
            .render_output(renderer, &mut framebuffer, age, &elements, render::CLEAR_COLOR)
            .map(|r| (r.damage.cloned(), r.states))
    };
    match result {
        Ok((damage, states)) => {
            if let Some(damage) = damage {
                if let Err(err) = data.backend.submit(Some(&damage)) {
                    tracing::warn!(?err, "winit submit failed");
                }
            }
            render::post_render(ds, &output, &states, None);
        }
        Err(err) => tracing::warn!(?err, "winit render failed"),
    }
}
