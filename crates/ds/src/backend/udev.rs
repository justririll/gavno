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
//! nothing changing, no timer is armed and ds sleeps.

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
    render::{self, DsRenderElement, SurfaceDmabufFeedback},
    Ds,
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

pub fn init(event_loop: &mut EventLoop<'static, Ds>, ds: &mut Ds) -> Result<(), Box<dyn std::error::Error>> {
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
        .insert_source(libinput_backend, |mut event, _, ds| {
            if let InputEvent::DeviceAdded { device } = &mut event {
                configure_input_device(device);
            }
            ds.process_input_event(event);
        })?;

    // Session pause/resume (VT switches).
    event_loop.handle().insert_source(notifier, |event, _, ds| match event {
        SessionEvent::PauseSession => on_session_pause(ds),
        SessionEvent::ActivateSession => on_session_activate(ds),
    })?;

    let udev_backend = UdevBackend::new(session.seat())?;
    let active = session.is_active();
    let devices: Vec<(DrmNode, std::path::PathBuf)> = udev_backend
        .device_list()
        .filter_map(|(id, path)| Some((DrmNode::from_dev_id(id).ok()?, path.to_path_buf())))
        .collect();
    ds.backend = Backend::Udev(UdevData {
        session,
        libinput,
        primary_gpu,
        gpu: None,
        dmabuf_global: None,
        pending_devices: Vec::new(),
    });

    if active {
        for (node, path) in devices {
            device_added(ds, node, &path);
        }
        if !matches!(&ds.backend, Backend::Udev(UdevData { gpu: Some(_), .. })) {
            return Err("the primary GPU could not be initialized".into());
        }
    } else {
        info!("session is not active yet; waiting for our VT before opening the GPU");
        if let Some(data) = udev(ds) {
            data.pending_devices = devices;
        }
    }

    event_loop.handle().insert_source(udev_backend, |event, _, ds| match event {
        UdevEvent::Added { device_id, path } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_added(ds, node, &path);
            }
        }
        UdevEvent::Changed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_changed(ds, node);
            }
        }
        UdevEvent::Removed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                device_removed(ds, node);
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
        let _ = device.config_scroll_set_natural_scroll_enabled(true);
        let methods = device.config_click_methods();
        if methods.contains(&libinput::ClickMethod::Clickfinger) {
            let _ = device.config_click_set_method(libinput::ClickMethod::Clickfinger);
        }
    }
    debug!(name = device.name(), "input device configured");
}

fn udev(ds: &mut Ds) -> Option<&mut UdevData> {
    match &mut ds.backend {
        Backend::Udev(data) => Some(data),
        _ => None,
    }
}

/// The surface for `crtc`, borrowing only `ds.backend`.
fn surface_mut(backend: &mut Backend, crtc: crtc::Handle) -> Option<&mut Surface> {
    match backend {
        Backend::Udev(data) => data.gpu.as_mut()?.surfaces.get_mut(&crtc),
        _ => None,
    }
}

fn device_added(ds: &mut Ds, node: DrmNode, path: &Path) {
    let Some(data) = udev(ds) else { return };
    // One device: the primary GPU (card node). Everything else is ignored.
    let primary = data.primary_gpu;
    let same_device = node == primary
        || node.node_with_type(NodeType::Primary).and_then(Result::ok) == Some(primary);
    if !same_device || data.gpu.is_some() || node.ty() != NodeType::Primary {
        return;
    }

    match open_gpu(ds, node, path) {
        Ok(()) => device_changed(ds, node),
        Err(err) => error!(?err, ?node, "failed to open GPU"),
    }
}

fn open_gpu(ds: &mut Ds, node: DrmNode, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let Some(data) = udev(ds) else { return Ok(()) };
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

    let token = ds.loop_handle.insert_source(notifier, move |event, metadata, ds| match event {
        DrmEvent::VBlank(crtc) => on_vblank(ds, crtc, metadata),
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
    ds.shm_state.update_formats(renderer.shm_formats());
    let dmabuf_formats = renderer.dmabuf_formats();
    let default_feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats).build()?;
    let global = ds
        .dmabuf_state
        .create_global_with_default_feedback::<Ds>(&ds.display_handle, &default_feedback);

    let Some(data) = udev(ds) else { return Ok(()) };
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

fn device_changed(ds: &mut Ds, node: DrmNode) {
    let Some(gpu) = udev(ds).and_then(|d| d.gpu.as_mut()) else { return };
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
            } => connector_connected(ds, connector, crtc),
            DrmScanEvent::Disconnected {
                connector,
                crtc: Some(crtc),
            } => connector_disconnected(ds, connector, crtc),
            _ => {}
        }
    }
}

fn device_removed(ds: &mut Ds, node: DrmNode) {
    let Backend::Udev(data) = &mut ds.backend else { return };
    if data.gpu.as_ref().map(|g| g.node) != Some(node) {
        return;
    }
    let gpu = data.gpu.take().unwrap();
    let global = data.dmabuf_global.take();
    for (_, surface) in gpu.surfaces {
        crate::handlers::layer_shell::close_layers(&surface.output);
        ds.space.unmap_output(&surface.output);
        ds.display_handle.remove_global::<Ds>(surface.global);
    }
    ds.loop_handle.remove(gpu.token);
    if let Some(global) = global {
        ds.dmabuf_state.destroy_global::<Ds>(&ds.display_handle, global);
    }
    warn!(?node, "GPU removed");
}

fn connector_connected(ds: &mut Ds, connector: connector::Info, crtc: crtc::Handle) {
    let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
    let internal = matches!(
        connector.interface(),
        connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS | connector::Interface::DSI
    );
    let scale = if internal { ds.config.internal_scale } else { 1.0 };

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
    let x = ds
        .space
        .outputs()
        .filter_map(|o| ds.space.output_geometry(o))
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
    let global = output.create_global::<Ds>(&ds.display_handle);
    output.set_preferred(wl_mode);
    output.change_current_state(
        Some(wl_mode),
        Some(Transform::Normal),
        Some(OutputScale::Fractional(scale)),
        Some((x, 0).into()),
    );
    ds.space.map_output(&output, (x, 0));

    let Backend::Udev(data) = &mut ds.backend else { return };
    let primary = data.primary_gpu;
    let Some(gpu) = data.gpu.as_mut() else { return };
    let drm_output = match gpu.manager.initialize_output::<_, DsRenderElement>(
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
            ds.space.unmap_output(&output);
            ds.display_handle.remove_global::<Ds>(global);
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

    ds.dispatch_modules(|m, ds| {
        m.output_added(ds, &output);
        crate::module::Handled::No
    });
    ds.output_added_hook();
    queue_redraw(ds, crtc);
}

fn connector_disconnected(ds: &mut Ds, connector: connector::Info, crtc: crtc::Handle) {
    let Some(surface) = udev(ds).and_then(|d| d.gpu.as_mut()).and_then(|g| g.surfaces.remove(&crtc)) else {
        return;
    };
    if let RedrawState::WaitingForTimer { token, .. } = surface.redraw {
        ds.loop_handle.remove(token);
    }
    info!(output = surface.output.name(), ?connector, "output disconnected");
    crate::handlers::layer_shell::close_layers(&surface.output);
    ds.space.unmap_output(&surface.output);
    ds.display_handle.remove_global::<Ds>(surface.global);
    ds.queue_redraw_all();
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

pub fn queue_redraw_all(ds: &mut Ds) {
    let crtcs: Vec<crtc::Handle> = udev(ds)
        .and_then(|d| d.gpu.as_ref())
        .map(|g| g.surfaces.keys().copied().collect())
        .unwrap_or_default();
    for crtc in crtcs {
        queue_redraw(ds, crtc);
    }
}

fn queue_redraw(ds: &mut Ds, crtc: crtc::Handle) {
    let Some(surface) = surface_mut(&mut ds.backend, crtc) else {
        return;
    };
    match &mut surface.redraw {
        RedrawState::Idle => {
            surface.redraw = RedrawState::Queued;
            ds.loop_handle.insert_idle(move |ds| render_surface(ds, crtc));
        }
        RedrawState::Queued => {}
        RedrawState::WaitingForVBlank { queued } | RedrawState::WaitingForTimer { queued, .. } => {
            *queued = true;
        }
    }
}

fn render_surface(ds: &mut Ds, crtc: crtc::Handle) {
    let session_active = udev(ds).is_some_and(|d| d.session.is_active());
    let pointer_pos = ds.pointer_location();
    let time = ds.start_time.elapsed();

    let Backend::Udev(data) = &mut ds.backend else { return };
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
        &ds.space,
        &output,
        pointer_pos,
        &mut ds.cursor_status,
        &mut ds.cursor,
        time,
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
        let feedback = render::take_presentation_feedback(&ds.space, &output, &states);
        match surface.drm_output.queue_frame(Some(feedback)) {
            Ok(()) => surface.redraw = RedrawState::WaitingForVBlank { queued: false },
            Err(err) => {
                warn!(?err, output = output.name(), "queue_frame failed");
                surface.redraw = RedrawState::Idle;
            }
        }
    } else {
        // Nothing changed on screen. Pace frame callbacks with a timer of
        // one refresh period instead of spinning.
        let refresh = output.current_mode().map(|m| m.refresh).unwrap_or(60_000).max(1);
        let period = Duration::from_micros(1_000_000_000 / refresh as u64);
        let token = ds
            .loop_handle
            .insert_source(Timer::from_duration(period), move |_, _, ds| {
                on_timer(ds, crtc);
                TimeoutAction::Drop
            })
            .ok();
        surface.redraw = match token {
            Some(token) => RedrawState::WaitingForTimer { token, queued: false },
            None => RedrawState::Idle,
        };
    }

    let feedback = surface.feedback.clone();
    render::post_render(ds, &output, &states, feedback.as_ref());
}

fn on_timer(ds: &mut Ds, crtc: crtc::Handle) {
    let Some(surface) = surface_mut(&mut ds.backend, crtc) else {
        return;
    };
    if let RedrawState::WaitingForTimer { queued, .. } = surface.redraw {
        surface.redraw = RedrawState::Idle;
        if queued {
            queue_redraw(ds, crtc);
        }
    }
}

fn on_vblank(ds: &mut Ds, crtc: crtc::Handle, metadata: &mut Option<DrmEventMetadata>) {
    let Some(surface) = surface_mut(&mut ds.backend, crtc) else {
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
        _ => (ds.clock.now(), wp_presentation_feedback::Kind::Vsync),
    };
    let seq = metadata.as_ref().map(|m| m.sequence).unwrap_or(0);

    match surface.drm_output.frame_submitted() {
        Ok(Some(Some(mut feedback))) => {
            feedback.presented(time, Refresh::fixed(period), seq as u64, flags);
        }
        Ok(_) => {}
        Err(err) => warn!(?err, "frame_submitted failed"),
    }

    let queued = matches!(surface.redraw, RedrawState::WaitingForVBlank { queued: true });
    surface.redraw = RedrawState::Idle;
    if queued {
        queue_redraw(ds, crtc);
    }
}

fn on_session_pause(ds: &mut Ds) {
    info!("session paused");
    let Backend::Udev(data) = &mut ds.backend else { return };
    data.libinput.suspend();
    if let Some(gpu) = data.gpu.as_mut() {
        gpu.manager.pause();
        for surface in gpu.surfaces.values_mut() {
            if let RedrawState::WaitingForTimer { token, .. } = surface.redraw {
                ds.loop_handle.remove(token);
            }
            surface.redraw = RedrawState::Idle;
        }
    }
}

fn on_session_activate(ds: &mut Ds) {
    info!("session activated");
    let Some(data) = udev(ds) else { return };
    if data.libinput.resume().is_err() {
        warn!("failed to resume libinput");
    }
    let pending = std::mem::take(&mut data.pending_devices);
    if !pending.is_empty() {
        for (node, path) in pending {
            device_added(ds, node, &path);
        }
        if !matches!(&ds.backend, Backend::Udev(UdevData { gpu: Some(_), .. })) {
            error!("the primary GPU could not be initialized");
            ds.quit();
        }
        ds.queue_redraw_all();
        return;
    }
    let Some(data) = udev(ds) else { return };
    if let Some(gpu) = data.gpu.as_mut() {
        if let Err(err) = gpu.manager.activate(false) {
            error!(?err, "failed to reactivate the DRM device");
        }
        for surface in gpu.surfaces.values_mut() {
            surface.drm_output.reset_buffers();
        }
        let node = gpu.node;
        // Connectors may have changed while we were away.
        device_changed(ds, node);
    }
    ds.queue_redraw_all();
}
