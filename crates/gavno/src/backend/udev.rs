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
        renderer::{element::RenderElementStates, gles::GlesRenderer, ImportDma, ImportMemWl},
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
    hdr::{compose::Composition, kms::ConnectorColor, mode::{self, Candidate}},
    Gavno,
};

/// The initial and fallback SDR formats. HDR explicitly tests XR30 separately.
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

impl Drop for UdevData {
    fn drop(&mut self) {
        // Drop the GPU (and restore connector properties) before libseat closes
        // the session. Struct fields otherwise drop in declaration order.
        self.gpu.take();
    }
}

struct Gpu {
    node: DrmNode,
    render_node: DrmNode,
    token: RegistrationToken,
    renderer: GlesRenderer,
    manager: GbmDrmOutputManager,
    allocator: GbmAllocator<DrmDeviceFd>,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
}

struct Surface {
    output: Output,
    global: GlobalId,
    drm_output: GbmDrmOutput,
    feedback: Option<SurfaceDmabufFeedback>,
    redraw: RedrawState,
    color: Result<ConnectorColor, String>,
    hdr: Option<Composition>,
    hdr_requested: Option<bool>,
    sdr_format: Fourcc,
    white_nits: f32,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // Restore while the manager still owns the active DRM device.
        for surface in self.surfaces.values_mut() {
            if let Ok(color) = &mut surface.color {
                if let Err(error) = color.restore() { warn!(%error, "failed to restore SDR at GPU shutdown"); }
            }
        }
    }
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
        allocator.clone(),
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
        allocator,
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
    let mut gpu = data.gpu.take().unwrap();
    let global = data.dmabuf_global.take();
    for (_, mut surface) in gpu.surfaces.drain() {
        if let Ok(color) = &mut surface.color { let _ = color.restore(); }
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

    let modes: Vec<_> = connector.modes().iter().map(|m| {
        let wl = WlMode::from(*m);
        Candidate { width: m.size().0, height: m.size().1, refresh: wl.refresh,
            preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED) }
    }).collect();
    let requested = gv.config.output_mode;
    if requested.is_some() && mode::select(&modes, requested).is_none() {
        warn!(name, ?requested, "requested output mode unavailable; using preferred mode");
    }
    if modes.is_empty() {
        warn!(name, "connector has no modes");
        return;
    }
    let hdr_requested = gv.config.output_hdr;
    let white_nits = gv.config.sdr_white_nits;

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
    output.change_current_state(
        None,
        Some(Transform::Normal),
        Some(OutputScale::Fractional(scale)),
        Some((x, 0).into()),
    );
    gv.space.map_output(&output, (x, 0));

    let Backend::Udev(data) = &mut gv.backend else { return };
    let primary = data.primary_gpu;
    let Some(gpu) = data.gpu.as_mut() else { return };
    let initialized = mode::try_modes(&modes, requested, |index| {
        let wl_mode = WlMode::from(connector.modes()[index]);
        output.change_current_state(Some(wl_mode), None, None, None);
        match gpu.manager.initialize_output::<_, GavnoRenderElement>(
            crtc,
            connector.modes()[index],
            &[connector.handle()],
            &output,
            None,
            &mut gpu.renderer,
            &DrmOutputRenderElements::default(),
        ) {
            Ok(drm_output) => Some(drm_output),
            Err(err) => {
                warn!(?err, name, width = wl_mode.size.w, height = wl_mode.size.h,
                    refresh = wl_mode.refresh, "output mode rejected; trying fallback");
                None
            }
        }
    });
    let Some((index, drm_output)) = initialized else {
        error!(name, "failed to initialize any advertised output mode");
        gv.space.unmap_output(&output);
        gv.display_handle.remove_global::<Gavno>(global);
        return;
    };
    let wl_mode = WlMode::from(connector.modes()[index]);
    output.set_preferred(wl_mode);
    info!(name, width = wl_mode.size.w, height = wl_mode.size.h,
        refresh_hz = f64::from(wl_mode.refresh) / 1000.0, scale, "selected output mode");
    let feedback = surface_dmabuf_feedback(&drm_output, &gpu.renderer, primary, gpu.render_node);
    let color = if gpu.manager.device().is_atomic() {
        ConnectorColor::probe(gpu.manager.device().device_fd().clone(), connector.handle())
    } else { Err("HDR requires atomic modesetting".into()) };
    if let Err(reason) = &color { debug!(name, reason, "HDR unavailable on connector"); }
    let sdr_format = drm_output.format();

    gpu.surfaces.insert(
        crtc,
        Surface {
            output: output.clone(),
            global,
            drm_output,
            feedback,
            redraw: RedrawState::Idle,
            color,
            hdr: None,
            hdr_requested: Some(hdr_requested),
            sdr_format,
            white_nits,
        },
    );
    gv.output_added_hook();
    queue_redraw(gv, crtc);
}

fn connector_disconnected(gv: &mut Gavno, connector: connector::Info, crtc: crtc::Handle) {
    let Some(mut surface) = udev(gv).and_then(|d| d.gpu.as_mut()).and_then(|g| g.surfaces.remove(&crtc)) else {
        return;
    };
    if let RedrawState::WaitingForTimer { token, .. } = surface.redraw {
        gv.loop_handle.remove(token);
    }
    if let Ok(color) = &mut surface.color { let _ = color.restore(); }
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

/// Requests are applied only after an outstanding page flip/timer completes.
/// This keeps format changes out of the in-flight frame's lifetime.
pub fn set_hdr(gv: &mut Gavno, output: &str, enabled: bool) -> Result<(), String> {
    let Some(gpu) = udev(gv).and_then(|d| d.gpu.as_mut()) else { return Err("HDR requires the DRM backend".into()); };
    let (_, surface) = gpu.surfaces.iter_mut().find(|(_, s)| s.output.name() == output).ok_or("unknown output")?;
    if enabled { surface.color.as_ref().map_err(Clone::clone)?; }
    surface.hdr_requested = Some(enabled);
    gv.queue_redraw_all();
    Ok(())
}

pub fn hdr_status(backend: &Backend, output: &Output) -> Option<crate::hdr::Status> {
    let Backend::Udev(data) = backend else { return None };
    let surface = data.gpu.as_ref()?.surfaces.values().find(|s| s.output == *output)?;
    Some(crate::hdr::Status {
        enabled: surface.hdr.is_some(), pending: surface.hdr_requested,
        supported: surface.color.is_ok(), sdr_white_nits: surface.white_nits,
    })
}

pub fn toggle_hdr(gv: &mut Gavno) {
    let Some(output) = gv.active_output() else { return };
    let enabled = udev(gv).and_then(|d| d.gpu.as_ref()).and_then(|g| g.surfaces.values()
        .find(|s| s.output == output)).map(|s| !s.hdr_requested.unwrap_or(s.hdr.is_some()));
    if let Some(enabled) = enabled {
        if let Err(error) = set_hdr(gv, &output.name(), enabled) { warn!(%error, "HDR toggle unavailable"); }
    }
}

pub fn adjust_sdr_white(gv: &mut Gavno, delta: f32) {
    let Some(output) = gv.active_output() else { return };
    let Some(gpu) = udev(gv).and_then(|d| d.gpu.as_mut()) else { return };
    let Some((_, surface)) = gpu.surfaces.iter_mut().find(|(_, s)| s.output == output) else { return };
    if let Some(hdr) = &mut surface.hdr {
        hdr.adjust_white(delta);
        surface.white_nits = hdr.white_nits;
        info!(output = output.name(), nits = surface.white_nits, "HDR SDR white adjusted");
        gv.queue_redraw_all();
    }
}

fn set_scanout_format(surface: &mut Surface, renderer: &GlesRenderer,
    allocator: &GbmAllocator<DrmDeviceFd>, format: Fourcc) -> Result<(), String> {
    let modifiers: Vec<_> = renderer.egl_context().dmabuf_render_formats().iter()
        .filter(|f| f.code == format).map(|f| f.modifier).collect();
    surface.drm_output.with_compositor(|c| c.set_format(allocator.clone(), format, modifiers))
        .map_err(|e| format!("scanout format {format:?}: {e:?}"))
}

fn change_hdr(surface: &mut Surface, renderer: &mut GlesRenderer,
    allocator: &GbmAllocator<DrmDeviceFd>, enabled: bool) -> Result<(), String> {
    if enabled == surface.hdr.is_some() {
        if let Ok(color) = &mut surface.color {
            if enabled { color.enable() } else { color.restore() }.map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    if enabled {
        surface.color.as_ref().map_err(Clone::clone)?;
        // Prepare all GL resources before touching connector properties.
        let composition = Composition::new(renderer, &surface.output, surface.white_nits)?;
        set_scanout_format(surface, renderer, allocator, Fourcc::Xrgb2101010)?;
        if let Err(error) = surface.color.as_mut().map_err(|e| e.clone())?.enable() {
            if let Err(restore) = set_scanout_format(surface, renderer, allocator, surface.sdr_format) {
                warn!(%restore, "using 10-bit SDR swapchain after failed HDR enable");
            }
            return Err(format!("HDR connector commit: {error}"));
        }
        surface.hdr = Some(composition);
    } else {
        // Do not feed SDR pixels to a PQ connector if restoring it failed.
        surface.color.as_mut().map_err(|e| e.clone())?.restore().map_err(|e| e.to_string())?;
        surface.hdr = None;
        if let Err(error) = set_scanout_format(surface, renderer, allocator, surface.sdr_format) {
            warn!(%error, "using 10-bit SDR swapchain after HDR disable");
        }
    }
    surface.drm_output.reset_buffers();
    info!(output = surface.output.name(), enabled, nits = surface.white_nits, "HDR output state changed");
    Ok(())
}

fn draw_frame(surface: &mut Surface, renderer: &mut GlesRenderer,
    elements: &[GavnoRenderElement]) -> Result<(bool, RenderElementStates), String> {
    use smithay::backend::drm::compositor::PrimaryPlaneElement;
    if let Some(hdr) = &mut surface.hdr {
        let (element, states) = hdr.render(renderer, &surface.output, elements, render::CLEAR_COLOR)?;
        // All pixels, including the cursor, must pass through PQ encoding.
        let elements = [element];
        let result = surface.drm_output.render_frame(renderer, &elements, [0.0, 0.0, 0.0, 1.0], FrameFlags::empty())
            .map_err(|e| format!("HDR scanout render: {e:?}"))?;
        if result.needs_sync() {
            if let PrimaryPlaneElement::Swapchain(element) = &result.primary_element {
                element.sync.wait().map_err(|e| format!("HDR scanout fence: {e:?}"))?;
            }
        }
        Ok((result.is_empty, states))
    } else {
        surface.drm_output.render_frame(renderer, elements, render::CLEAR_COLOR, FrameFlags::DEFAULT)
            .map(|r| (r.is_empty, r.states)).map_err(|e| format!("SDR render: {e:?}"))
    }
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
    if let Some(enabled) = surface.hdr_requested.take() {
        if let Err(error) = change_hdr(surface, &mut gpu.renderer, &gpu.allocator, enabled) {
            warn!(output = surface.output.name(), %error, "HDR change failed; retaining SDR or last working mode");
        }
    }
    // Failed acquisition/reactivation commits must not send SDR pixels to an
    // unknown (possibly PQ) connector. Fall back to explicitly established SDR.
    if surface.color.as_ref().is_ok_and(|color| !color.is_current(surface.hdr.is_some())) {
        if let Err(error) = change_hdr(surface, &mut gpu.renderer, &gpu.allocator, false) {
            warn!(%error, "connector color state unknown; deferring presentation");
            surface.hdr_requested = Some(false);
            surface.redraw = RedrawState::Idle;
            return;
        }
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
    );
    let mut result = draw_frame(surface, &mut gpu.renderer, &elements);
    if result.is_err() && surface.hdr.is_some() {
        warn!(output = output.name(), error = ?result.as_ref().err(), "HDR render failed; falling back to SDR");
        if change_hdr(surface, &mut gpu.renderer, &gpu.allocator, false).is_ok() {
            result = draw_frame(surface, &mut gpu.renderer, &elements);
        }
    }

    let (is_empty, states) = match result {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, output = output.name(), "render failed");
            surface.redraw = RedrawState::Idle;
            return;
        }
    };

    let mut retry_sdr = false;
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
                if surface.hdr.is_some() {
                    retry_sdr = change_hdr(surface, &mut gpu.renderer, &gpu.allocator, false).is_ok();
                }
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
    if !is_empty {
        crate::handlers::screencopy::frame_drawn(gv, &output);
    }
    if retry_sdr || gv.is_animating() {
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
        for surface in gpu.surfaces.values_mut() {
            if let Ok(color) = &mut surface.color {
                if let Err(error) = color.restore() { warn!(%error, "failed to restore SDR before session pause"); }
            }
        }
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
            if let Ok(color) = &mut surface.color {
                color.invalidate();
            }
            // Reconcile SDR as well as HDR before presenting after a VT switch.
            surface.hdr_requested = Some(surface.hdr_requested.unwrap_or(surface.hdr.is_some()));
        }
        let node = gpu.node;
        // Connectors may have changed while we were away.
        device_changed(gv, node);
    }
    gv.queue_redraw_all();
}

/// Capture the original sRGB scene, independently of HDR/PQ scanout.
pub fn screenshot(gv: &mut Gavno) -> Option<(u32, u32, Vec<u8>)> {
    let Backend::Udev(data) = &gv.backend else { return None };
    let output = data.gpu.as_ref()?.surfaces.values().next()?.output.clone();
    capture_output(gv, &output, true)
}

/// Renders `output` offscreen (the SDR composition, also in HDR mode) and
/// returns its physical size and tightly packed RGBA pixels.
pub fn capture_output(gv: &mut Gavno, output: &Output, cursor: bool) -> Option<(u32, u32, Vec<u8>)> {
    // A pointer position outside every output leaves the cursor out of the frame.
    let pointer_pos = if cursor { gv.pointer_location() } else { (-1.0e6, -1.0e6).into() };
    let time = gv.start_time.elapsed();
    let Backend::Udev(data) = &mut gv.backend else { return None };
    let gpu = data.gpu.as_mut()?;
    let output = output.clone();
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
    );
    let bytes = render::capture_elements(&mut gpu.renderer, &elements, (mode.size.w, mode.size.h), scale)?;
    Some((mode.size.w as u32, mode.size.h as u32, bytes))
}
