//! TTY backend: libseat session, libinput, one DRM/KMS device, GLES renderer.
//!
//! Single-GPU on purpose: the Snapdragon X Elite has one DRM device (msm) that
//! does both display and rendering. Multi-GPU can come later if ever needed.
//!
//! Redraws are on demand. Each output has a small state machine:
//! `Idle` -> (something changed) -> `Queued` -> render ->
//!   - frame had damage: `WaitingForVBlank` until the page flip completes;
//!   - frame was empty: `WaitingForTimer` for one refresh period, so clients
//!     that only ask for frame callbacks are paced at the refresh rate.
//! Changes during a wait set `queued` and are drawn right after it. With
//! nothing changing, no timer is armed and GAVNO sleeps.

use std::{collections::HashMap, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            dmabuf::Dmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, NodeType,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{gles::GlesRenderer, ImportDma, ImportMemWl},
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{primary_gpu, UdevBackend, UdevEvent},
    },
    desktop::utils::OutputPresentationFeedback,
    output::{Mode as WlMode, Output, PhysicalProperties, Scale as OutputScale},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop, RegistrationToken,
        },
        drm::control::{connector, crtc, ModeTypeFlags},
        input::{self as libinput, Libinput},
        rustix::fs::OFlags,
        wayland_protocols::wp::{
            linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1,
            presentation_time::server::wp_presentation_feedback,
        },
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Monotonic, Time, Transform},
    wayland::{
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal},
        presentation::Refresh,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, warn};

use super::Backend;
use crate::{
    render::{self, GavnoRenderElement, SurfaceDmabufFeedback},
    Gavno,
};

/// 8-bit formats only: msm scans these out everywhere, 10-bit is not worth
/// the risk on the panel yet.
const SUPPORTED_FORMATS: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Abgr8888, Fourcc::Xrgb8888, Fourcc::Xbgr8888];

type GbmDrmOutput = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;
type GbmDrmOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

pub struct UdevData {
    pub session: LibSeatSession,
    libinput: Libinput,
    primary_gpu: DrmNode,
    gpu: Option<Gpu>,
    dmabuf_global: Option<DmabufGlobal>,
    /// DRM devices found at startup while our session was not the active one
    /// yet (no DRM master then); opened on the first activation.
    pending_devices: Vec<(DrmNode, std::path::PathBuf)>,
}

struct Gpu {
    node: DrmNode,
    render_node: DrmNode,
    token: RegistrationToken,
    renderer: GlesRenderer,
    manager: GbmDrmOutputManager,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
}

struct Surface {
    output: Output,
    global: GlobalId,
    drm_output: GbmDrmOutput,
    feedback: Option<SurfaceDmabufFeedback>,
    redraw: RedrawState,
}

#[derive(Debug)]
enum RedrawState {
    Idle,
    Queued,
    WaitingForVBlank { queued: bool },
    WaitingForTimer { token: RegistrationToken, queued: bool },
}

impl UdevData {
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        let Some(gpu) = self.gpu.as_mut() else {
            return false;
        };
        let ok = gpu.renderer.import_dmabuf(dmabuf, None).is_ok();
        if ok {
            dmabuf.set_node(gpu.render_node);
        }
        ok
    }

    pub fn switch_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            warn!(?err, vt, "VT switch failed");
        }
    }
}

pub fn init(event_loop: &mut EventLoop<'static, Gavno>, gv: &mut Gavno) -> Result<(), Box<dyn std::error::Error>> {
    let (session, notifier) = LibSeatSession::new().map_err(|err| format!("libseat session: {err}"))?;
    info!(seat = session.seat(), "session opened");

    let primary_gpu = primary_gpu(session.seat())
        .ok()
        .flatten()
        .and_then(|path| DrmNode::from_path(path).ok())
        .ok_or("no primary GPU found")?;
    info!(?primary_gpu, "primary GPU");

    // Input.
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&session.seat())
        .map_err(|_| "libinput: failed to assign seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput.clone());
    event_loop
        .handle()
        .insert_source(libinput_backend, |mut event, _, gv| {
            if let InputEvent::DeviceAdded { device } = &mut event {
                configure_input_device(device);
            }
            gv.process_input_event(event);
        })?;

    // Session pause/resume (VT switches).
    event_loop.handle().insert_source(notifier, |event, _, gv| match event {
        SessionEvent::PauseSession => on_session_pause(gv),
        SessionEvent::ActivateSession => on_session_activate(gv),
    })?;

    let udev_backend = UdevBackend::new(session.seat())?;
    let active = session.is_active();
    let devices: Vec<(DrmNode, std::path::PathBuf)> = udev_backend
        .device_list()
        .filter_map(|(id, path)| Some((DrmNode::from_dev_id(id).ok()?, path.to_path_buf())))
        .collect();
    gv.backend = Backend::Udev(UdevData {
        session,
        libinput,
        primary_gpu,
        gpu: None,
        dmabuf_global: None,
        pending_devices: Vec::new(),
    });

    if active {
        for (node, path) in devices {
            device_added(gv, node, &path);
        }
        if !matches!(&gv.backend, Backend::Udev(UdevData { gpu: Some(_), .. })) {
            return Err("the primary GPU could not be initialized".into());
        }
    } else {
        info!("session is not active yet; waiting for our VT before opening the GPU");
        if let Some(data) = udev(gv) {
            data.pending_devices = devices;
        }
    }

    event_loop.handle().insert_source(udev_backend, |event, _, gv| match event {
        UdevEvent::Added { device_id, path } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_added(gv, node, &path);
            }
        }
        UdevEvent::Changed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_changed(gv, node);
            }
        }
        UdevEvent::Removed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_removed(gv, node);
            }
        }
    })?;

    Ok(())
}

/// Touchpad-first defaults (design/input.md): tap to click, tap-and-drag,
/// clickfinger buttons, natural scrolling, disable while typing.
fn configure_input_device(device: &mut libinput::Device) {
    if device.config_tap_finger_count() > 0 {
        let _ = device.config_tap_set_enabled(true);
        let _ = device.config_tap_set_drag_enabled(true);
        let _ = device.config_tap_set_drag_lock_enabled(false);
        let _ = device.config_dwt_set_enabled(true);
        let _ = device.config_scroll_set_natural_scroll_enabled(std::env::var("GAVNO_NATURAL_SCROLL").as_deref() != Ok("0"));
        let methods = device.config_click_methods();
        if methods.contains(&libinput::ClickMethod::Clickfinger) {
            let _ = device.config_click_set_method(libinput::ClickMethod::Clickfinger);
        }
    }
    debug!(name = device.name(), "input device configured");
}

fn udev(gv: &mut Gavno) -> Option<&mut UdevData> {
    match &mut gv.backend {
        Backend::Udev(data) => Some(data),
        _ => None,
    }
}

/// The surface for `crtc`, borrowing only `gv.backend`.
fn surface_mut(backend: &mut Backend, crtc: crtc::Handle) -> Option<&mut Surface> {
    match backend {
        Backend::Udev(data) => data.gpu.as_mut()?.surfaces.get_mut(&crtc),
        _ => None,
    }
}

fn device_added(gv: &mut Gavno, node: DrmNode, path: &Path) {
    let Some(data) = udev(gv) else { return };
    // One device: the primary GPU (card node). Everything else is ignored.
    let primary = data.primary_gpu;
    let same_device = node == primary
        || node.node_with_type(NodeType::Primary).and_then(Result::ok) == Some(primary);
    if !same_device || data.gpu.is_some() || node.ty() != NodeType::Primary {
        return;
    }

    match open_gpu(gv, node, path) {
        Ok(()) => device_changed(gv, node),
        Err(err) => error!(?err, ?node, "failed to open GPU"),
    }
}

fn open_gpu(gv: &mut Gavno, node: DrmNode, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let Some(data) = udev(gv) else { return Ok(()) };
    let fd = data
        .session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, notifier) = DrmDevice::new(fd.clone(), true)?;
    let gbm = GbmDevice::new(fd)?;

    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_device = EGLDevice::device_for_display(&display)?;
    let render_node = egl_device.try_get_render_node().ok().flatten().unwrap_or(node);
    let context = EGLContext::new(&display)?;
    let renderer = unsafe { GlesRenderer::new(context)? };
    info!(?render_node, "GLES renderer ready");

    let token = gv.loop_handle.insert_source(notifier, move |event, metadata, gv| match event {
        DrmEvent::VBlank(crtc) => on_vblank(gv, crtc, metadata),
        DrmEvent::Error(err) => error!(?err, "DRM error"),
    })?;

    let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node));
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
    let manager = DrmOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        SUPPORTED_FORMATS.iter().copied(),
        render_formats,
    );

    // Client buffers: shm formats and a dmabuf global with feedback.
    gv.shm_state.update_formats(renderer.shm_formats());
    let dmabuf_formats = renderer.dmabuf_formats();
    let default_feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats).build()?;
    let global = gv
        .dmabuf_state
        .create_global_with_default_feedback::<Gavno>(&gv.display_handle, &default_feedback);

    let Some(data) = udev(gv) else { return Ok(()) };
    data.dmabuf_global = Some(global);
    data.gpu = Some(Gpu {
        node,
        render_node,
        token,
        renderer,
        manager,
        scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
    });
    Ok(())
}

fn device_changed(gv: &mut Gavno, node: DrmNode) {
    let Some(gpu) = udev(gv).and_then(|d| d.gpu.as_mut()) else { return };
    if gpu.node != node {
        return;
    }
    let events: Vec<DrmScanEvent> = match gpu.scanner.scan_connectors(gpu.manager.device()) {
        Ok(result) => result.into_iter().collect(),
        Err(err) => {
            warn!(?err, "connector scan failed");
            return;
        }
    };
    for event in events {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => connector_connected(gv, connector, crtc),
            DrmScanEvent::Disconnected {
                connector,
                crtc: Some(crtc),
            } => connector_disconnected(gv, connector, crtc),
            _ => {}
        }
    }
}

fn device_removed(gv: &mut Gavno, node: DrmNode) {
    let Backend::Udev(data) = &mut gv.backend else { return };
    if data.gpu.as_ref().map(|g| g.node) != Some(node) {
        return;
    }
    let gpu = data.gpu.take().unwrap();
    let global = data.dmabuf_global.take();
    for (_, surface) in gpu.surfaces {
        crate::handlers::layer_shell::close_layers(&surface.output);
        gv.space.unmap_output(&surface.output);
        gv.display_handle.remove_global::<Gavno>(surface.global);
    }
    gv.loop_handle.remove(gpu.token);
    if let Some(global) = global {
        gv.dmabuf_state.destroy_global::<Gavno>(&gv.display_handle, global);
    }
    warn!(?node, "GPU removed");
}

fn connector_connected(gv: &mut Gavno, connector: connector::Info, crtc: crtc::Handle) {
    let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
    let internal = matches!(
        connector.interface(),
        connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS | connector::Interface::DSI
    );
    let scale = if internal { gv.config.internal_scale } else { 1.0 };

    let Some(mode) = connector
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .copied()
    else {
        warn!(name, "connector has no modes");
        return;
    };
    let wl_mode = WlMode::from(mode);
    info!(name, ?wl_mode, scale, "output connected");

    // Outputs are placed left to right in connection order.
    let x = gv
        .space
        .outputs()
        .filter_map(|o| gv.space.output_geometry(o))
        .map(|g| g.loc.x + g.size.w)
        .max()
        .unwrap_or(0);

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make: "Unknown".into(),
            model: name.clone(),
        },
    );
    let global = output.create_global::<Gavno>(&gv.display_handle);
    output.set_preferred(wl_mode);
    output.change_current_state(
        Some(wl_mode),
        Some(Transform::Normal),
        Some(OutputScale::Fractional(scale)),
        Some((x, 0).into()),
    );
    gv.space.map_output(&output, (x, 0));

    let Backend::Udev(data) = &mut gv.backend else { return };
    let primary = data.primary_gpu;
    let Some(gpu) = data.gpu.as_mut() else { return };
    let drm_output = match gpu.manager.initialize_output::<_, GavnoRenderElement>(
        crtc,
        mode,
        &[connector.handle()],
        &output,
        None,
        &mut gpu.renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(o) => o,
        Err(err) => {
            error!(?err, name, "failed to initialize output");
            gv.space.unmap_output(&output);
            gv.display_handle.remove_global::<Gavno>(global);
            return;
        }
    };
    let feedback = surface_dmabuf_feedback(&drm_output, &gpu.renderer, primary, gpu.render_node);

    gpu.surfaces.insert(
        crtc,
        Surface {
            output: output.clone(),
            global,
            drm_output,
            feedback,
            redraw: RedrawState::Idle,
        },
    );

    gv.dispatch_modules(|m, gv| {
        m.output_added(gv, &output);
        crate::module::Handled::No
    });
    gv.output_added_hook();
    queue_redraw(gv, crtc);
}

fn connector_disconnected(gv: &mut Gavno, connector: connector::Info, crtc: crtc::Handle) {
    let Some(surface) = udev(gv).and_then(|d| d.gpu.as_mut()).and_then(|g| g.surfaces.remove(&crtc)) else {
        return;
    };
    if let RedrawState::WaitingForTimer { token, .. } = surface.redraw {
        gv.loop_handle.remove(token);
    }
    info!(output = surface.output.name(), ?connector, "output disconnected");
    crate::handlers::layer_shell::close_layers(&surface.output);
    gv.space.unmap_output(&surface.output);
    gv.display_handle.remove_global::<Gavno>(surface.global);
    gv.queue_redraw_all();
}

/// Scan-out tranche first (formats the primary plane can show directly),
/// then everything the renderer can sample.
fn surface_dmabuf_feedback(
    drm_output: &GbmDrmOutput,
    renderer: &GlesRenderer,
    primary: DrmNode,
    render_node: DrmNode,
) -> Option<SurfaceDmabufFeedback> {
    let render_formats = renderer.dmabuf_formats();
    let (scanout_formats, device_id) = drm_output.with_compositor(|compositor| {
        let surface = compositor.surface();
        let planes = surface.planes().clone();
        let formats = surface
            .plane_info()
            .formats
            .iter()
            .copied()
            .chain(planes.overlay.into_iter().flat_map(|p| p.formats))
            .filter(|f| render_formats.contains(f))
            .collect::<Vec<_>>();
        (formats, surface.device_fd().dev_id().ok())
    });
    let builder = DmabufFeedbackBuilder::new(render_node.dev_id(), render_formats.clone());
    let render = builder.clone().build().ok()?;
    let scanout = builder
        .add_preference_tranche(
            device_id.unwrap_or_else(|| primary.dev_id()),
            Some(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout),
            scanout_formats,
        )
        .build()
        .ok()?;
    Some(SurfaceDmabufFeedback { render, scanout })
}

pub fn queue_redraw_all(gv: &mut Gavno) {
    let crtcs: Vec<crtc::Handle> = udev(gv)
        .and_then(|d| d.gpu.as_ref())
        .map(|g| g.surfaces.keys().copied().collect())
        .unwrap_or_default();
    for crtc in crtcs {
        queue_redraw(gv, crtc);
    }
}

fn queue_redraw(gv: &mut Gavno, crtc: crtc::Handle) {
    let Some(surface) = surface_mut(&mut gv.backend, crtc) else {
        return;
    };
    match &mut surface.redraw {
        RedrawState::Idle => {
            surface.redraw = RedrawState::Queued;
            gv.loop_handle.insert_idle(move |gv| render_surface(gv, crtc));
        }
        RedrawState::Queued => {}
        RedrawState::WaitingForVBlank { queued } | RedrawState::WaitingForTimer { queued, .. } => {
            *queued = true;
        }
    }
}

fn render_surface(gv: &mut Gavno, crtc: crtc::Handle) {
    let session_active = udev(gv).is_some_and(|d| d.session.is_active());
    let pointer_pos = gv.pointer_location();
    let time = gv.start_time.elapsed();

    let Backend::Udev(data) = &mut gv.backend else { return };
    let Some(gpu) = data.gpu.as_mut() else { return };
    let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };
    if !matches!(surface.redraw, RedrawState::Queued) {
        return;
    }
    if !session_active {
        // Resumed by `on_session_activate`.
        surface.redraw = RedrawState::Idle;
        return;
    }
    let output = surface.output.clone();

    let elements = render::output_elements(
        &mut gpu.renderer,
        &gv.space,
        &output,
        pointer_pos,
        &mut gv.cursor_status,
        &mut gv.cursor,
        time,
        gv.greeter.as_mut(),
        &mut gv.overlay,
        &mut gv.window_decor,
        &mut gv.window_animations,
        &gv.protocols.locks,
        &mut gv.workspaces,
        &mut gv.overview,
        &mut gv.dock,
        &mut gv.topbar,
        &gv.config,
    );
    let result = surface
        .drm_output
        .render_frame(&mut gpu.renderer, &elements, render::CLEAR_COLOR, FrameFlags::DEFAULT)
        .map(|r| (r.is_empty, r.states));

    let (is_empty, states) = match result {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, output = output.name(), "render failed");
            surface.redraw = RedrawState::Idle;
            return;
        }
    };

    if !is_empty {
        let feedback = render::take_presentation_feedback(&gv.space, &output, &states);
        match surface.drm_output.queue_frame(Some(feedback)) {
            Ok(()) => {
                surface.redraw = RedrawState::WaitingForVBlank { queued: false };
                gv.protocols.locks.submitted(&output);
            },
            Err(err) => {
                warn!(?err, output = output.name(), "queue_frame failed");
                surface.redraw = RedrawState::Idle;
            }
        }
    } else {
        gv.protocols.locks.submitted(&output);
        gv.protocols.locks.presented(&output);
        // Nothing changed on screen. Pace frame callbacks with a timer of
        // one refresh period instead of spinning.
        let refresh = output.current_mode().map(|m| m.refresh).unwrap_or(60_000).max(1);
        let period = Duration::from_micros(1_000_000_000 / refresh as u64);
        let token = gv
            .loop_handle
            .insert_source(Timer::from_duration(period), move |_, _, gv| {
                on_timer(gv, crtc);
                TimeoutAction::Drop
            })
            .ok();
        surface.redraw = match token {
            Some(token) => RedrawState::WaitingForTimer { token, queued: false },
            None => RedrawState::Idle,
        };
    }

    let feedback = surface.feedback.clone();
    render::post_render(gv, &output, &states, feedback.as_ref());
    if gv.is_animating() {
        queue_redraw(gv, crtc);
    }
}

fn on_timer(gv: &mut Gavno, crtc: crtc::Handle) {
    let Some(surface) = surface_mut(&mut gv.backend, crtc) else {
        return;
    };
    if let RedrawState::WaitingForTimer { queued, .. } = surface.redraw {
        surface.redraw = RedrawState::Idle;
        if queued {
            queue_redraw(gv, crtc);
        }
    }
}

fn on_vblank(gv: &mut Gavno, crtc: crtc::Handle, metadata: &mut Option<DrmEventMetadata>) {
    let Some(surface) = surface_mut(&mut gv.backend, crtc) else {
        return;
    };

    let refresh = surface
        .output
        .current_mode()
        .map(|m| m.refresh)
        .unwrap_or(60_000)
        .max(1);
    let period = Duration::from_micros(1_000_000_000 / refresh as u64);
    let (time, flags): (Time<Monotonic>, _) = match metadata.as_ref().map(|m| m.time) {
        Some(DrmEventTime::Monotonic(tp)) if !tp.is_zero() => (
            tp.into(),
            wp_presentation_feedback::Kind::Vsync
                | wp_presentation_feedback::Kind::HwClock
                | wp_presentation_feedback::Kind::HwCompletion,
        ),
        _ => (gv.clock.now(), wp_presentation_feedback::Kind::Vsync),
    };
    let seq = metadata.as_ref().map(|m| m.sequence).unwrap_or(0);

    let lock_output = surface.output.clone();
    let mut presented = false;
    match surface.drm_output.frame_submitted() {
        Ok(Some(Some(mut feedback))) => {
            presented = true;
            feedback.presented(time, Refresh::fixed(period), seq as u64, flags);
        }
        Ok(_) => { presented = true; }
        Err(err) => warn!(?err, "frame_submitted failed"),
    }

    let queued = matches!(surface.redraw, RedrawState::WaitingForVBlank { queued: true });
    surface.redraw = RedrawState::Idle;
    if presented { gv.protocols.locks.presented(&lock_output); }
    if queued {
        queue_redraw(gv, crtc);
    }
}

fn on_session_pause(gv: &mut Gavno) {
    info!("session paused");
    let Backend::Udev(data) = &mut gv.backend else { return };
    data.libinput.suspend();
    if let Some(gpu) = data.gpu.as_mut() {
        gpu.manager.pause();
        for surface in gpu.surfaces.values_mut() {
            if let RedrawState::WaitingForTimer { token, .. } = surface.redraw {
                gv.loop_handle.remove(token);
            }
            surface.redraw = RedrawState::Idle;
        }
    }
}

fn on_session_activate(gv: &mut Gavno) {
    info!("session activated");
    let Some(data) = udev(gv) else { return };
    // Started on an inactive VT, libinput failed to open its devices, and
    // resume() on a never-suspended context is a no-op. Suspend first so
    // resume() re-enumerates the seat.
    if !data.pending_devices.is_empty() {
        data.libinput.suspend();
    }
    if data.libinput.resume().is_err() {
        warn!("failed to resume libinput");
    }
    let pending = std::mem::take(&mut data.pending_devices);
    if !pending.is_empty() {
        for (node, path) in pending {
            device_added(gv, node, &path);
        }
        if !matches!(&gv.backend, Backend::Udev(UdevData { gpu: Some(_), .. })) {
            error!("the primary GPU could not be initialized");
            gv.quit();
        }
        gv.queue_redraw_all();
        return;
    }
    let Some(data) = udev(gv) else { return };
    if let Some(gpu) = data.gpu.as_mut() {
        if let Err(err) = gpu.manager.activate(false) {
            error!(?err, "failed to reactivate the DRM device");
        }
        for surface in gpu.surfaces.values_mut() {
            surface.drm_output.reset_buffers();
        }
        let node = gpu.node;
        // Connectors may have changed while we were away.
        device_changed(gv, node);
    }
    gv.queue_redraw_all();
}

pub fn screenshot(gv: &mut Gavno) -> Option<(u32, u32, Vec<u8>)> {
    use smithay::backend::renderer::{damage::OutputDamageTracker, Bind, ExportMem, Offscreen};
    use smithay::backend::renderer::gles::GlesTexture;
    use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};

    let pointer_pos = gv.pointer_location();
    let time = gv.start_time.elapsed();
    let Backend::Udev(data) = &mut gv.backend else { return None };
    let gpu = data.gpu.as_mut()?;
    let output = gpu.surfaces.values().next()?.output.clone();
    let mode = output.current_mode()?;
    let scale = output.current_scale().fractional_scale();
    let elements = render::output_elements(
        &mut gpu.renderer,
        &gv.space,
        &output,
        pointer_pos,
        &mut gv.cursor_status,
        &mut gv.cursor,
        time,
        gv.greeter.as_mut(),
        &mut gv.overlay,
        &mut gv.window_decor,
        &mut gv.window_animations,
        &gv.protocols.locks,
        &mut gv.workspaces,
        &mut gv.overview,
        &mut gv.dock,
        &mut gv.topbar,
        &gv.config,
    );
    let size: Size<i32, BufferCoord> = (mode.size.w, mode.size.h).into();
    let mut texture: GlesTexture = gpu.renderer.create_buffer(Fourcc::Abgr8888, size).ok()?;
    let mapping = {
        let mut fb = gpu.renderer.bind(&mut texture).ok()?;
        let mut damage = OutputDamageTracker::new(mode.size, scale, Transform::Normal);
        damage
            .render_output(&mut gpu.renderer, &mut fb, 0, &elements, render::CLEAR_COLOR)
            .ok()?;
        gpu.renderer
            .copy_framebuffer(&fb, Rectangle::from_size(size), Fourcc::Abgr8888)
            .ok()?
    };
    let bytes = gpu.renderer.map_texture(&mapping).ok()?.to_vec();
    Some((mode.size.w as u32, mode.size.h as u32, bytes))
}
