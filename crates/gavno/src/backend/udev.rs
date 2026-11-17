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

use std::{collections::{HashMap, HashSet}, path::Path, time::Duration};

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
use smithay_drm_extras::drm_scanner::{CrtcMapper, DrmScanEvent, DrmScanner, SimpleCrtcMapper};
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
    scanout_blocked: HashSet<String>,
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
    scanner: DrmScanner<InternalFirstMapper>,
    probe_after_present: bool,
    probe_running: bool,
    probe_again: bool,
    surfaces: HashMap<crtc::Handle, Surface>,
    disabled_outputs: HashMap<String, Output>,
}

struct AdvertisedPreferred(Vec<WlMode>);

struct DisabledHdr(bool);

struct Surface {
    output: Output,
    modes: Vec<smithay::reexports::drm::control::Mode>,
    global: GlobalId,
    drm_output: GbmDrmOutput,
    feedback: Option<SurfaceDmabufFeedback>,
    redraw: RedrawState,
    color: Result<ConnectorColor, String>,
    hdr: Option<Composition>,
    hdr_requested: Option<bool>,
    sdr_format: Fourcc,
    white_nits: f32,
    first_frame_queued: bool,
    first_frame_presented: bool,
    /// Consecutive failed page flips; reset by a successful one.
    flip_failures: u32,
    scanout: Scanout,
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
    crate::perf::startup("udev.session.begin");
    let (session, notifier) = LibSeatSession::new().map_err(|err| format!("libseat session: {err}"))?;
    info!(seat = session.seat(), "session opened");
    crate::perf::startup("udev.session.opened");

    let primary_gpu = primary_gpu(session.seat())
        .ok()
        .flatten()
        .and_then(|path| DrmNode::from_path(path).ok())
        .ok_or("no primary GPU found")?;
    info!(?primary_gpu, "primary GPU");
    crate::perf::startup("udev.primary_gpu.found");
    crate::perf::startup("udev.libinput.begin");

    // Input.
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&session.seat())
        .map_err(|_| "libinput: failed to assign seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput.clone());
    event_loop
        .handle()
        .insert_source(libinput_backend, |mut event, _, gv| {
            let _stall = crate::perf::Stall::new("libinput");
            static FIRST_LIBINPUT: std::sync::Once = std::sync::Once::new();
            FIRST_LIBINPUT.call_once(|| crate::perf::startup("udev.libinput.first_event"));
            if let InputEvent::DeviceAdded { device } = &mut event {
                configure_input_device(device);
            }
            gv.process_input_event(event);
        })?;

    crate::perf::startup("udev.libinput.ready");

    // Session pause/resume (VT switches).
    event_loop.handle().insert_source(notifier, |event, _, gv| {
        let _stall = crate::perf::Stall::new("udev.session");
        match event {
            SessionEvent::PauseSession => on_session_pause(gv),
            SessionEvent::ActivateSession => on_session_activate(gv),
        }
    })?;

    let udev_backend = UdevBackend::new(session.seat())?;
    crate::perf::startup("udev.enumeration.ready");
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
        scanout_blocked: HashSet::new(),
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

    event_loop.handle().insert_source(udev_backend, |event, _, gv| {
        let _stall = crate::perf::Stall::new("udev.hotplug");
        match event {
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

    if !data.session.is_active() {
        if !data.pending_devices.iter().any(|(pending, _)| *pending == node) {
            data.pending_devices.push((node, path.to_owned()));
        }
        return;
    }
    match open_gpu(gv, node, path) {
        Ok(()) => {
            scan_cached_connectors(gv, node);
            if udev(gv).and_then(|data| data.gpu.as_ref()).is_some_and(|gpu| gpu.surfaces.is_empty()) {
                device_changed(gv, node);
            }
        },
        Err(err) => error!(?err, ?node, "failed to open GPU"),
    }
}

fn open_gpu(gv: &mut Gavno, node: DrmNode, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    crate::perf::startup("udev.gpu.open.begin");
    let Some(data) = udev(gv) else { return Ok(()) };
    let fd = data
        .session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)?;
    crate::perf::startup("udev.gpu.session_fd_opened");
    crate::perf::startup("udev.drm.master.begin");
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));
    crate::perf::startup("udev.drm.master.end");
    crate::perf::startup("udev.gpu.fd_opened");
    // Preserve the link and mode left by the greeter. Smithay's initial full
    // commit sets the requested timing and resets its claimable cursor/overlays.
    crate::perf::startup("udev.drm.device.begin");
    let (drm, notifier) = {
        let _trace = crate::perf::DrmStartupGuard::new(None);
        DrmDevice::new(fd.clone(), false)?
    };
    crate::perf::startup("udev.drm.ready");
    let gbm = GbmDevice::new(fd)?;

    crate::perf::startup("udev.gbm.ready");
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    crate::perf::startup("udev.egl.display_ready");
    let egl_device = EGLDevice::device_for_display(&display)?;
    let render_node = egl_device.try_get_render_node().ok().flatten().unwrap_or(node);
    let context = EGLContext::new(&display)?;
    crate::perf::startup("udev.egl.context_ready");
    let renderer = unsafe { GlesRenderer::new(context)? };
    info!(?render_node, "GLES renderer ready");
    crate::perf::startup("udev.renderer.ready");

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
        // Preferences are shared across outputs and cannot be changed later.
        // Prefer XR30 so newly configured/hotplugged HDR outputs can also adopt
        // the initial transaction without an intermediate 8-bit swapchain.
        // SDR uses the same 32-bit storage with its normal transfer function.
        std::iter::once(Fourcc::Xrgb2101010).chain(SUPPORTED_FORMATS.iter().copied()),
        render_formats,
    );

    crate::perf::startup("udev.output_manager.ready");

    // Client buffers: shm formats and a dmabuf global with feedback.
    gv.shm_state.update_formats(renderer.shm_formats());
    crate::perf::startup("udev.shm_formats.ready");
    let dmabuf_formats = renderer.dmabuf_formats();
    crate::perf::startup("udev.dmabuf_formats.ready");
    let default_feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats).build()?;
    crate::perf::startup("udev.dmabuf_feedback.ready");
    let global = gv
        .dmabuf_state
        .create_global_with_default_feedback::<Gavno>(&gv.display_handle, &default_feedback);

    crate::perf::startup("udev.dmabuf.ready");
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
        probe_after_present: true,
        probe_running: false,
        probe_again: false,
        surfaces: HashMap::new(),
        disabled_outputs: HashMap::new(),
    });
    Ok(())
}

fn internal_connector(connector: &connector::Info) -> bool {
    matches!(connector.interface(), connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS | connector::Interface::DSI)
}

/// Order CRTC allocation too, before DrmScanner's internal HashMap order can
/// give an external connector the panel's only compatible CRTC.
#[derive(Debug, Default)]
struct InternalFirstMapper(SimpleCrtcMapper);
impl CrtcMapper for InternalFirstMapper {
    fn map<'a>(&mut self, drm: &impl smithay::reexports::drm::control::Device,
        connectors: impl Iterator<Item = &'a connector::Info> + Clone) {
        let mut connectors: Vec<_> = connectors.collect();
        connectors.sort_by_key(|info| !internal_connector(info));
        self.0.map(drm, connectors.into_iter());
    }
    fn crtc_for_connector(&self, connector: &connector::Handle) -> Option<crtc::Handle> {
        self.0.crtc_for_connector(connector)
    }
}

/// DrmScanner always asks for force_probe=true. Override only that ioctl; all
/// CRTC mapping still goes through Smithay, using the kernel's cached modes.
struct CachedConnectors<'a, D>(&'a D);
impl<D: std::os::fd::AsFd> std::os::fd::AsFd for CachedConnectors<'_, D> {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> { self.0.as_fd() }
}
impl<D: smithay::reexports::drm::Device> smithay::reexports::drm::Device for CachedConnectors<'_, D> {}
impl<D: smithay::reexports::drm::control::Device> smithay::reexports::drm::control::Device for CachedConnectors<'_, D> {
    fn get_connector(&self, handle: connector::Handle, _: bool) -> std::io::Result<connector::Info> {
        let name = format!("connector-{}", u32::from(handle));
        crate::perf::startup_output("udev.connector.cached.begin", &name);
        let result = self.0.get_connector(handle, false);
        crate::perf::startup_output("udev.connector.cached.end", &name);
        // Do not let DrmScanner mark a connected output as handled before its
        // modes have been filled in by the probe worker.
        if result.as_ref().is_ok_and(|info| info.state() == connector::State::Connected && info.modes().is_empty()) {
            debug!(connector = u32::from(handle), "connector modes not cached; waiting for probe worker");
            return Err(std::io::Error::other("connector modes not cached"));
        }
        result
    }
}

/// One finite worker per change burst, with at most one rescan coalesced behind
/// it. No polling or idle timers. Never move the scanner off the main loop.
fn device_changed(gv: &mut Gavno, node: DrmNode) {
    use smithay::reexports::{calloop::channel, drm::control::Device};
    let Some(data) = udev(gv) else { return };
    if !data.session.is_active() { return; }
    let Some(gpu) = data.gpu.as_mut().filter(|gpu| gpu.node == node) else { return };
    if gpu.probe_after_present && !gpu.surfaces.is_empty() { return; }
    if gpu.probe_running {
        gpu.probe_again = true;
        return;
    }
    let fd = gpu.manager.device().device_fd().clone();
    let identity = fd.downgrade();
    let (sender, receiver) = channel::channel::<()>();
    let token = match gv.loop_handle.insert_source(receiver, move |event, _, gv| {
        if let channel::Event::Msg(()) = event {
            let Some(data) = udev(gv) else { return };
            let Some(gpu) = data.gpu.as_mut().filter(|gpu| identity == *gpu.manager.device().device_fd()) else { return };
            gpu.probe_running = false;
            let again = std::mem::take(&mut gpu.probe_again);
            scan_cached_connectors(gv, node);
            if again { device_changed(gv, node); }
        }
    }) {
        Ok(token) => token,
        Err(error) => { warn!(%error, "could not register connector probe worker"); return; }
    };
    if let Some(gpu) = udev(gv).and_then(|data| data.gpu.as_mut()) { gpu.probe_running = true; }
    if let Err(error) = std::thread::Builder::new().name("drm-connectors".into()).spawn(move || {
        crate::perf::startup("udev.connectors.probe.begin");
        if let Ok(resources) = fd.resource_handles() {
            for handle in resources.connectors() {
                // Internal panels already have reliable cached modes. External
                // ports can block for AUX/link timeouts, exclusively here.
                if fd.get_connector(*handle, false).is_ok_and(|info| internal_connector(&info)
                    && info.state() == connector::State::Connected && !info.modes().is_empty()) { continue; }
                let name = format!("connector-{}", u32::from(*handle));
                crate::perf::startup_output("udev.connector.probe.begin", &name);
                if let Err(error) = fd.get_connector(*handle, true) {
                    warn!(%error, connector = u32::from(*handle), "connector probe failed");
                }
                crate::perf::startup_output("udev.connector.probe.end", &name);
            }
        }
        crate::perf::startup("udev.connectors.probe.end");
        let _ = sender.send(());
    }) {
        gv.loop_handle.remove(token);
        if let Some(gpu) = udev(gv).and_then(|data| data.gpu.as_mut()) { gpu.probe_running = false; }
        warn!(%error, "could not start connector probe worker");
    }
}

fn scan_cached_connectors(gv: &mut Gavno, node: DrmNode) {
    // While another VT owns the seat, output setup fails with DeviceInactive
    // but the scanner would still record the connector as handled. Leave the
    // scan to on_session_activate, which calls this again.
    if udev(gv).is_some_and(|d| !d.session.is_active()) {
        return;
    }
    let Some(gpu) = udev(gv).and_then(|d| d.gpu.as_mut()) else { return };
    if gpu.node != node {
        return;
    }
    crate::perf::startup("udev.connectors.scan.begin");
    let mut events: Vec<DrmScanEvent> = match gpu.scanner.scan_connectors(&CachedConnectors(gpu.manager.device().device_fd())) {
        Ok(result) => result.into_iter().collect(),
        Err(err) => {
            warn!(?err, "connector scan failed");
            return;
        }
    };
    crate::perf::startup("udev.connectors.scan.end");
    // Try enabled connectors first, so an explicitly disabled panel is only
    // kept as a safety fallback when no configured output can be initialized.
    events.sort_by_key(|event| match event {
        DrmScanEvent::Disconnected { .. } => (false, false),
        DrmScanEvent::Connected { connector, .. } => (
            gv.config.outputs.get(&format!("{}-{}", connector.interface().as_str(), connector.interface_id()))
                .is_some_and(|o| o.enabled == Some(false)),
            !internal_connector(connector),
        ),
    });
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
    if let Some(gpu) = udev(gv).and_then(|data| data.gpu.as_ref()) {
        let active: HashSet<_> = gpu.surfaces.keys().copied().collect();
        if let Err(error) = crate::hdr::startup::clear_unused_outputs(gpu.manager.device(), &active) {
            warn!(%error, "could not disable inherited unused outputs");
        }
    }
}

fn device_removed(gv: &mut Gavno, node: DrmNode) {
    let Backend::Udev(data) = &mut gv.backend else { return };
    data.pending_devices.retain(|(pending, _)| *pending != node);
    if data.gpu.as_ref().map(|g| g.node) != Some(node) {
        return;
    }
    let mut gpu = data.gpu.take().unwrap();
    let global = data.dmabuf_global.take();
    for (_, mut surface) in gpu.surfaces.drain() {
        if let Ok(color) = &mut surface.color { let _ = color.restore(); }
        crate::handlers::layer_shell::close_layers(&surface.output);
        gv.space.unmap_output(&surface.output);
        gv.protocols.screencopy.output_removed(&surface.output);
        crate::handlers::color_management::output_removed(&surface.output);
        gv.display_handle.remove_global::<Gavno>(surface.global);
    }
    gv.loop_handle.remove(gpu.token);
    if let Some(global) = global {
        gv.dmabuf_state.destroy_global::<Gavno>(&gv.display_handle, global);
    }
    warn!(?node, "GPU removed");
}

fn connector_identity(gv: &Gavno, connector: &connector::Info) -> Option<(String, String)> {
    use smithay::reexports::drm::control::Device;
    let Backend::Udev(data) = &gv.backend else { return None };
    let fd = data.gpu.as_ref()?.manager.device().device_fd();
    let props = fd.get_properties(connector.handle()).ok()?;
    for (handle, value) in props {
        if fd.get_property(handle).ok()?.name().to_bytes() == b"EDID" {
            let bytes = fd.get_property_blob(value).ok()?;
            return edid_identity(&bytes);
        }
    }
    None
}

fn edid_identity(bytes: &[u8]) -> Option<(String, String)> {
    let base = bytes.get(..128)?;
    if base[..8] != [0, 255, 255, 255, 255, 255, 255, 0] || base.iter().fold(0_u8, |sum, b| sum.wrapping_add(*b)) != 0 { return None; }
    let vendor = u16::from_be_bytes([base[8], base[9]]);
    let mut make = String::new();
    for shift in [10, 5, 0] {
        let letter = ((vendor >> shift) & 31) as u8;
        if !(1..=26).contains(&letter) { return None; }
        make.push(char::from(b'A' + letter - 1));
    }
    let model = base[54..126].chunks_exact(18).find_map(|descriptor| {
        if descriptor[..5] != [0, 0, 0, 252, 0] { return None; }
        let text = std::str::from_utf8(&descriptor[5..]).ok()?.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
        (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_owned())
    }).unwrap_or_else(|| format!("{}", u16::from_le_bytes([base[10], base[11]])));
    Some((make, model))
}

fn connector_connected(gv: &mut Gavno, connector: connector::Info, crtc: crtc::Handle) {
    let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
    crate::perf::startup_output("udev.output.begin", &name);
    let internal = internal_connector(&connector);
    let (requested, scale, hdr_requested, enabled) = gv.config.output_settings(&name, internal);

    let modes: Vec<_> = connector.modes().iter().map(|m| {
        let wl = WlMode::from(*m);
        Candidate { width: m.size().0, height: m.size().1, refresh: wl.refresh,
            preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED) }
    }).collect();
    if requested.is_some() && mode::select(&modes, requested).is_none() {
        warn!(name, ?requested, "requested output mode unavailable; using preferred mode");
    }
    if modes.is_empty() {
        warn!(name, "connector has no modes");
        return;
    }
    let white_nits = gv.config.sdr_white_nits;

    // Outputs are placed left to right in connection order.
    let x = gv
        .space
        .outputs()
        .filter_map(|o| gv.space.output_geometry(o))
        .map(|g| g.loc.x + g.size.w)
        .max()
        .unwrap_or(0);

    crate::perf::startup_output("udev.output.identity.begin", &name);
    let (make, model) = connector_identity(gv, &connector).unwrap_or_else(|| ("Unknown".into(), name.clone()));
    crate::perf::startup_output("udev.output.identity.ready", &name);
    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make,
            model,
        },
    );
    crate::perf::startup_output("udev.output.wayland_created", &name);
    for (index, timing) in connector.modes().iter().enumerate() {
        let wl = WlMode::from(*timing);
        output.add_mode(wl);
        if modes[index].preferred { output.set_preferred(wl); }
    }
    output.user_data().insert_if_missing(|| AdvertisedPreferred(connector.modes().iter()
        .filter(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).map(|m| WlMode::from(*m)).collect()));
    if !enabled && gv.space.outputs().next().is_some() {
        output.change_current_state(None, Some(Transform::Normal), Some(OutputScale::Fractional(scale)), Some((x, 0).into()));
        if let Some(gpu) = udev(gv).and_then(|d| d.gpu.as_mut()) {
            let color = if gpu.manager.device().is_atomic() {
                ConnectorColor::probe(gpu.manager.device().device_fd().clone(), connector.handle())
            } else { Err("HDR requires atomic modesetting".into()) };
            output.user_data().insert_if_missing(|| DisabledHdr(color.is_ok()));
            crate::perf::startup_output("udev.output.disabled", &name);
            gpu.disabled_outputs.insert(name, output);
        }
        return;
    }
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
    let scanout = Scanout { rejected: data.scanout_blocked.contains(&name), ..Default::default() };
    let Some(gpu) = data.gpu.as_mut() else { return };
    let mut color = if gpu.manager.device().is_atomic() {
        ConnectorColor::probe(gpu.manager.device().device_fd().clone(), connector.handle())
    } else { Err("HDR requires atomic modesetting".into()) };
    if let Err(reason) = &color { debug!(name, reason, "HDR unavailable on connector"); }
    crate::perf::startup_output("udev.output.color_probe.ready", &name);
    let mut initial_hdr = None;
    crate::perf::startup_output("udev.output.initialize.begin", &name);
    let initialized = mode::try_modes(&modes, requested, |index| {
        let wl_mode = WlMode::from(connector.modes()[index]);
        output.change_current_state(Some(wl_mode), None, None, None);
        tracing::info!(target: "gavno::startup", stage = "udev.output.mode_attempt", ms = crate::perf::elapsed_ms(), output = %name, width = wl_mode.size.w, height = wl_mode.size.h, refresh = wl_mode.refresh);
        // Allocate the complete HDR pipeline before any commit. If it cannot
        // be established, initialize SDR without a preliminary SDR modeset.
        let mut black = None;
        let mut composition = None;
        if hdr_requested {
            let prepared = (|| -> Result<_, String> {
                let color = color.as_mut().map_err(|e| e.clone())?;
                let composition = Composition::new(&mut gpu.renderer, &output, white_nits)?;
                let frame = crate::hdr::startup::initialize(gpu.manager.device(), &mut gpu.allocator,
                    &mut gpu.renderer, connector.handle(), crtc, connector.modes()[index], color, &name)?;
                Ok((composition, frame))
            })();
            match prepared {
                Ok((hdr, frame)) => { composition = Some(hdr); black = Some(frame); }
                Err(error) => warn!(%error, output = %name, "initial HDR unavailable; initializing SDR"),
            }
        }
        if composition.is_none() {
            if let Ok(color) = &mut color {
                if let Err(error) = color.restore() {
                    warn!(%error, output = %name, "could not establish SDR connector state");
                    return None;
                }
            }
            if let Err(error) = crate::hdr::startup::clear_inherited_planes(gpu.manager.device(), crtc) {
                warn!(%error, output = %name, "could not clear inherited planes");
                return None;
            }
        }
        crate::perf::startup_output("udev.output.smithay_initialize.begin", &name);
        let initialized = {
            let _trace = crate::perf::DrmStartupGuard::new(Some(&name));
            gpu.manager.initialize_output::<_, GavnoRenderElement>(
                crtc,
                connector.modes()[index],
                &[connector.handle()],
                &output,
                None,
                &mut gpu.renderer,
                &DrmOutputRenderElements::default(),
            )
        };
        match initialized {
            Ok(drm_output) => {
                crate::perf::startup_output("udev.output.smithay_initialize.end", &name);
                if composition.is_some() && drm_output.format() != Fourcc::Xrgb2101010 {
                    // The manager may choose an SDR fallback format. Its initial
                    // black frame is valid in either transfer function.
                    if let Ok(color) = &mut color {
                        if let Err(error) = color.restore() {
                            warn!(%error, "failed to restore SDR after XR30 rejection");
                        }
                    }
                    composition = None;
                }
                initial_hdr = composition;
                drop(black);
                Some(drm_output)
            },
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
        gv.protocols.screencopy.output_removed(&output);
        gv.display_handle.remove_global::<Gavno>(global);
        return;
    };
    crate::perf::startup_output("udev.output.initialize.ready", &name);
    let wl_mode = WlMode::from(connector.modes()[index]);
    // Preferred flags come from the connector, not the selected timing.
    info!(name, width = wl_mode.size.w, height = wl_mode.size.h,
        refresh_hz = f64::from(wl_mode.refresh) / 1000.0, scale, "selected output mode");
    let feedback = surface_dmabuf_feedback(&drm_output, &gpu.renderer, primary, gpu.render_node);
    crate::perf::startup_output("udev.output.feedback.ready", &name);
    let sdr_format = drm_output.format();

    gpu.surfaces.insert(
        crtc,
        Surface {
            output: output.clone(),
            modes: connector.modes().to_vec(),
            global,
            drm_output,
            feedback,
            redraw: RedrawState::Idle,
            color,
            hdr: initial_hdr,
            hdr_requested: None,
            sdr_format,
            white_nits,
            first_frame_queued: false,
            first_frame_presented: false,
            flip_failures: 0,
            scanout,
        },
    );
    if let Some(surface) = gpu.surfaces.get(&crtc) {
        crate::handlers::color_management::output_changed(&output, surface.hdr.is_some(),
            surface.color.as_ref().map(|color| color.caps).unwrap_or_default(), white_nits);
    }
    crate::perf::startup_output("udev.output.hook.begin", &name);
    if gv.power.is_off() { set_power(gv, false); }
    gv.output_added_hook();
    crate::perf::startup_output("udev.output.ready", &name);
    queue_redraw(gv, crtc);
}

fn connector_disconnected(gv: &mut Gavno, connector: connector::Info, crtc: crtc::Handle) {
    if let Some(gpu) = udev(gv).and_then(|d| d.gpu.as_mut()) {
        gpu.disabled_outputs.remove(&format!("{}-{}", connector.interface().as_str(), connector.interface_id()));
    }
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
    gv.protocols.screencopy.output_removed(&surface.output);
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

/// Includes connected outputs intentionally disabled by configuration.
pub fn disabled_outputs(backend: &Backend) -> Vec<Output> {
    let Backend::Udev(data) = backend else { return Vec::new() };
    data.gpu.as_ref().map(|g| g.disabled_outputs.values().cloned().collect()).unwrap_or_default()
}

/// Test the requested timing before publishing the Wayland state. In-flight
/// flips own the previous buffers: callers retry once the flip has completed.
pub fn set_mode(gv: &mut Gavno, output: &Output, mode: WlMode) -> Result<(), String> {
    let Backend::Udev(data) = &mut gv.backend else {
        return if output.current_mode() == Some(mode) { Ok(()) } else { Err("mode changes require the DRM backend".into()) };
    };
    let gpu = data.gpu.as_mut().ok_or("GPU unavailable")?;
    let surface = gpu.surfaces.values_mut().find(|s| s.output == *output).ok_or("unknown output")?;
    if output.current_mode() == Some(mode) { return Ok(()); }
    if matches!(surface.redraw, RedrawState::WaitingForVBlank { .. } | RedrawState::WaitingForTimer { .. }) {
        return Err("output is presenting; retry".into());
    }
    let timing = surface.modes.iter().find(|m| WlMode::from(**m) == mode).copied().ok_or("mode unavailable")?;
    let previous = output.current_mode().and_then(|old| surface.modes.iter().find(|m| WlMode::from(**m) == old)).copied();
    let off = gv.power.is_off();
    if off { gv.power.secured_outputs.retain(|o| o != output); }
    let result = match surface.drm_output.use_mode::<_, GavnoRenderElement>(timing, &mut gpu.renderer, &DrmOutputRenderElements::default()) {
        Ok(()) => Ok(()),
        Err(error) => {
            let rollback = if let Some(previous) = previous {
                surface.drm_output.use_mode::<_, GavnoRenderElement>(previous, &mut gpu.renderer, &DrmOutputRenderElements::default())
                    .map_err(|restore| format!("modeset failed: {error:?}; restore failed: {restore:?}"))
            } else { Ok(()) };
            rollback.and(Err(format!("modeset failed: {error:?}")))
        }
    };
    // Mode negotiation (including rollback) may commit a frame. Clear it even
    // on an error, before returning to the event loop in the sleeping state.
    if off {
        surface.drm_output.with_compositor(|compositor| compositor.clear())
            .map_err(|e| format!("power off after modeset: {e}"))?;
        gv.power.secured_outputs.push(output.clone());
    }
    result?;
    output.change_current_state(Some(mode), None, None, None);
    surface.drm_output.reset_buffers();
    Ok(())
}

/// HDR's offscreen buffer and damage tracker use physical mode and logical
/// scale. Recreate both after a live geometry change before the next frame.
pub fn refresh_output_geometry(gv: &mut Gavno, output: &Output) -> Result<(), String> {
    let Backend::Udev(data) = &mut gv.backend else { return Ok(()) };
    let gpu = data.gpu.as_mut().ok_or("GPU unavailable")?;
    let surface = gpu.surfaces.values_mut().find(|s| s.output == *output).ok_or("unknown output")?;
    if surface.hdr.is_some() {
        let composition = Composition::new(&mut gpu.renderer, output, surface.white_nits)?;
        surface.hdr = Some(composition);
    }
    surface.drm_output.reset_buffers();
    Ok(())
}

pub fn mode_preferred(output: &Output, mode: WlMode) -> bool {
    output.user_data().get::<AdvertisedPreferred>().map_or(output.preferred_mode() == Some(mode), |p| p.0.contains(&mode))
}

pub fn hdr_status(backend: &Backend, output: &Output) -> Option<crate::hdr::Status> {
    if let Some(hdr) = output.user_data().get::<DisabledHdr>() {
        return Some(crate::hdr::Status { enabled: false, pending: None, supported: hdr.0, sdr_white_nits: 203.0 });
    }
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
        crate::handlers::color_management::output_changed(&surface.output, true, surface.color.as_ref().map(|c| c.caps).unwrap_or_default(), surface.white_nits);
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
        crate::handlers::color_management::output_changed(&surface.output, enabled, surface.color.as_ref().map(|c| c.caps).unwrap_or_default(), surface.white_nits);
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
    crate::handlers::color_management::output_changed(&surface.output, enabled, surface.color.as_ref().map(|c| c.caps).unwrap_or_default(), surface.white_nits);
    Ok(())
}

fn draw_frame(surface: &mut Surface, renderer: &mut GlesRenderer, direct_scanout: bool,
    elements: &[GavnoRenderElement], stall: &mut crate::perf::Stall<'_>) -> Result<(bool, RenderElementStates), String> {
    use smithay::backend::drm::compositor::PrimaryPlaneElement;
    use crate::render_sanitize::SanitizedElement;
    let scale = surface.output.current_scale().fractional_scale().into();
    let flags = surface.scanout.frame_flags(direct_scanout, surface.hdr.is_some());
    if let Some(hdr) = &mut surface.hdr {
        let (element, states) = hdr.render(renderer, &surface.output, elements, render::CLEAR_COLOR)?;
        stall.mark("hdr");
        // All pixels, including the cursor, must pass through PQ encoding.
        let element = SanitizedElement::new(element, scale);
        let result = surface.drm_output.render_frame(renderer, element.as_slice(), [0.0, 0.0, 0.0, 1.0], FrameFlags::empty())
            .map_err(|e| format!("HDR scanout render: {e:?}"))?;
        stall.mark("scanout");
        if result.needs_sync() {
            if let PrimaryPlaneElement::Swapchain(element) = &result.primary_element {
                element.sync.wait().map_err(|e| format!("HDR scanout fence: {e:?}"))?;
            }
            stall.mark("fence");
        }
        Ok((result.is_empty, states))
    } else {
        // Borrow the rebuilt scene; only this list allocates, not each wrapper.
        let mut sanitized = Vec::with_capacity(elements.len());
        sanitized.extend(elements.iter().filter_map(|e| SanitizedElement::new(e, scale)));
        let result = surface.drm_output.render_frame(renderer, &sanitized, render::CLEAR_COLOR, flags)
            .map_err(|e| format!("SDR render: {e:?}"))?;
        stall.mark("scanout");
        // Legacy KMS cannot take an in-fence: wait before queueing the frame.
        if result.needs_sync() {
            if let PrimaryPlaneElement::Swapchain(element) = &result.primary_element {
                element.sync.wait().map_err(|e| format!("SDR scanout fence: {e:?}"))?;
            }
            stall.mark("fence");
        }
        Ok((result.is_empty, result.states))
    }
}

/// Clear through DrmCompositor, rather than just DrmSurface, so queued and
/// pending frames cannot re-enable a sleeping CRTC. The next full frame modesets.
pub fn set_power(gv: &mut Gavno, on: bool) {
    gv.power.secured_outputs.clear();
    let Backend::Udev(data) = &mut gv.backend else { return };
    let Some(gpu) = data.gpu.as_mut() else { return };
    for surface in gpu.surfaces.values_mut() {
        if let RedrawState::WaitingForTimer { token, .. } = surface.redraw {
            gv.loop_handle.remove(token);
        }
        surface.redraw = RedrawState::Idle;
        if data.session.is_active() {
            let result = surface.drm_output.with_compositor(|compositor| {
                if on { compositor.reset_state() } else { compositor.clear() }
            });
            if let Err(error) = result {
                warn!(output = surface.output.name(), %error, on, "output power change failed");
            } else if !on {
                gv.power.secured_outputs.push(surface.output.clone());
            }
        }
        surface.drm_output.reset_buffers();
        if on {
            if let Ok(color) = &mut surface.color { color.invalidate(); }
            surface.hdr_requested = Some(surface.hdr_requested.unwrap_or(surface.hdr.is_some()));
        }
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
    if gv.power.is_off() { gv.schedule_power_frames(); return; }
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

// SDR client buffers can have a different format from the XR30 swapchain;
// Smithay still tests the complete plane assignment before the real flip.
const SDR_SCANOUT_FLAGS: FrameFlags = FrameFlags::DEFAULT.union(FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY);

/// A driver rejection latches for the output's entire session, across config
/// changes and VT switches. HDR never allows any plane to bypass PQ encoding.
#[derive(Debug, Default)]
struct Scanout {
    rejected: bool,
    attempted: bool,
}
impl Scanout {
    fn frame_flags(&mut self, configured: bool, hdr: bool) -> FrameFlags {
        self.attempted = configured && !hdr && !self.rejected;
        if self.attempted { SDR_SCANOUT_FLAGS } else { FrameFlags::empty() }
    }

    /// Returns true exactly once, when the driver first rejects a scanout frame.
    fn reject(&mut self) -> bool {
        if !self.attempted || self.rejected { return false; }
        self.rejected = true;
        self.attempted = false;
        true
    }
}

pub(crate) fn reset_scanout_buffers(gv: &mut Gavno) {
    if let Some(gpu) = udev(gv).and_then(|data| data.gpu.as_mut()) {
        for surface in gpu.surfaces.values_mut() {
            surface.drm_output.reset_buffers();
        }
    }
}

fn render_surface(gv: &mut Gavno, crtc: crtc::Handle) {
    if gv.power.is_off() { return; }
    let started = std::time::Instant::now();
    let session_active = udev(gv).is_some_and(|d| d.session.is_active());
    let pointer_pos = gv.pointer_location();
    let time = gv.start_time.elapsed();

    let Backend::Udev(data) = &mut gv.backend else { return };
    let Some(gpu) = data.gpu.as_mut() else { return };
    let Some(surface) = gpu.surfaces.get_mut(&crtc) else { return };
    let output = surface.output.clone();
    let mut stall = crate::perf::Stall::output_since("udev.render", &output, started);
    if !matches!(surface.redraw, RedrawState::Queued) {
        return;
    }
    if !session_active {
        // Resumed by `on_session_activate`.
        surface.redraw = RedrawState::Idle;
        return;
    }
    gv.power.rendered_frames += 1;
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
    if !surface.first_frame_queued {
        crate::perf::startup_output("udev.first_frame.render.begin", &output.name());
    }

    gv.cursor.dimmed = gv.media.brightness.dimmed();
    let mut elements = render::output_elements(
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
        &mut gv.launcher,
        &mut gv.classic,
        &mut gv.polkit,
    );
    if !surface.first_frame_queued {
        crate::perf::startup_output("udev.first_frame.elements.ready", &output.name());
    }
    render::dim_output(&mut elements, &mut gv.media.brightness, &output, time);
    stall.mark("elements");
    let mut result = draw_frame(surface, &mut gpu.renderer, gv.config.display.direct_scanout, &elements, &mut stall);
    if result.is_err() && surface.hdr.is_some() {
        warn!(output = output.name(), error = ?result.as_ref().err(), "HDR render failed; falling back to SDR");
        if change_hdr(surface, &mut gpu.renderer, &gpu.allocator, false).is_ok() {
            for element in &mut elements {
                if let GavnoRenderElement::Color(element) = element { element.fallback_sdr(); }
            }
            result = draw_frame(surface, &mut gpu.renderer, gv.config.display.direct_scanout, &elements, &mut stall);
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

    if !surface.first_frame_queued {
        crate::perf::startup_output("udev.first_frame.drawn", &output.name());
    }
    let mut retry_sdr = false;
    if !is_empty {
        let feedback = render::take_presentation_feedback(&gv.space, &output, &states);
        if !surface.first_frame_queued {
            crate::perf::startup_output("udev.first_commit.begin", &output.name());
        }
        let queued = surface.drm_output.queue_frame(Some(feedback));
        stall.mark("queue");
        match queued {
            Ok(()) => {
                surface.flip_failures = 0;
                if !surface.first_frame_queued {
                    surface.first_frame_queued = true;
                    crate::perf::startup_output("udev.first_commit.submitted", &output.name());
                    crate::perf::startup_output("udev.first_frame.queued", &output.name());
                }
                surface.redraw = RedrawState::WaitingForVBlank { queued: false };
                gv.protocols.locks.submitted(&output);
            },
            Err(err) => {
                let scanout_rejected = surface.scanout.reject();
                if scanout_rejected {
                    data.scanout_blocked.insert(output.name());
                    warn!(?err, output = output.name(), "direct scanout rejected; using composition for the rest of this session");
                    // Discard the plane assignment as well as the buffer damage.
                    if let Err(error) = surface.drm_output.with_compositor(|c| c.reset_state()) {
                        warn!(?error, output = output.name(), "DRM state reset failed");
                    }
                }
                surface.flip_failures = surface.flip_failures.saturating_add(1);
                let failures = surface.flip_failures;
                if !scanout_rejected && (failures == 1 || failures % 600 == 0) {
                    warn!(?err, output = output.name(), failures, "queue_frame failed");
                }
                // The rejected frame's damage counts as drawn; without this the
                // retry can render nothing and leave the old image on screen.
                surface.drm_output.reset_buffers();
                // Re-read the CRTC state from the kernel so the next frame is a
                // full commit instead of another flip on top of a rejected state.
                if failures % 3 == 0 {
                    if let Err(error) = surface.drm_output.with_compositor(|c| c.reset_state()) {
                        warn!(?error, output = output.name(), "DRM state reset failed");
                    }
                }
                if surface.hdr.is_some() {
                    retry_sdr = change_hdr(surface, &mut gpu.renderer, &gpu.allocator, false).is_ok();
                }
                // Retry after one refresh period. Going idle here let every
                // client commit (sent a frame callback below) trigger another
                // render and failed flip at once: a busy loop that starved input.
                surface.redraw = frame_timer(&gv.loop_handle, &output, crtc, true);
            }
        }
    } else {
        gv.protocols.locks.submitted(&output);
        gv.protocols.locks.presented(&output);
        // Nothing changed on screen. Pace frame callbacks with a timer of
        // one refresh period instead of spinning.
        surface.redraw = frame_timer(&gv.loop_handle, &output, crtc, false);
    }

    let feedback = surface.feedback.clone();
    render::post_render(gv, &output, &states, feedback.as_ref());
    if !is_empty {
        crate::handlers::screencopy::frame_drawn(gv, &output);
    }
    if retry_sdr || gv.is_animating() {
        queue_redraw(gv, crtc);
    }
    // A deliberately empty scene may never generate the first page flip.
    if is_empty { probe_after_first_frame(gv); }
}

/// Arms a one-refresh-period timer; `queued` redraws when it fires.
fn frame_timer(handle: &smithay::reexports::calloop::LoopHandle<'static, Gavno>, output: &smithay::output::Output, crtc: crtc::Handle,
    queued: bool) -> RedrawState {
    let refresh = output.current_mode().map(|m| m.refresh).unwrap_or(60_000).max(1);
    let period = Duration::from_micros(1_000_000_000 / refresh as u64);
    handle
        .insert_source(Timer::from_duration(period), move |_, _, gv| {
            on_timer(gv, crtc);
            TimeoutAction::Drop
        })
        .ok()
        .map_or(RedrawState::Idle, |token| RedrawState::WaitingForTimer { token, queued })
}

fn on_timer(gv: &mut Gavno, crtc: crtc::Handle) {
    let _stall = crate::perf::Stall::new("timer.udev.frame");
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
    let _stall = crate::perf::Stall::new("drm.vblank");
    if gv.power.is_off() { return; }
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
    let mut frame_completed = false;
    match surface.drm_output.frame_submitted() {
        Ok(Some(Some(mut feedback))) => {
            presented = true;
            frame_completed = true;
            feedback.presented(time, Refresh::fixed(period), seq as u64, flags);
        }
        Ok(_) => { presented = true; }
        Err(err) => warn!(?err, "frame_submitted failed"),
    }

    if frame_completed && surface.first_frame_queued && !surface.first_frame_presented {
        surface.first_frame_presented = true;
        crate::perf::startup_output("udev.first_frame.presented", &lock_output.name());
    }
    let queued = matches!(surface.redraw, RedrawState::WaitingForVBlank { queued: true });
    surface.redraw = RedrawState::Idle;
    if presented { gv.protocols.locks.presented(&lock_output); }
    if queued {
        queue_redraw(gv, crtc);
    }
    probe_after_first_frame(gv);
}

fn probe_after_first_frame(gv: &mut Gavno) {
    // Keep AUX probing out of the first modeset/frame path, even on drivers
    // whose connector probe takes modeset locks shared with atomic commits.
    let probe = udev(gv).and_then(|data| data.gpu.as_mut()).and_then(|gpu| {
        std::mem::take(&mut gpu.probe_after_present).then_some(gpu.node)
    });
    if let Some(node) = probe { device_changed(gv, node); }
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
        gv.power_on_monitors("session activation");
        gv.queue_redraw_all();
        return;
    }
    let Some(data) = udev(gv) else { return };
    if let Some(gpu) = data.gpu.as_mut() {
        if let Err(err) = gpu.manager.activate(false) {
            error!(?err, "failed to reactivate the DRM device");
        }
        for (crtc, surface) in &mut gpu.surfaces {
            if let Err(error) = crate::hdr::startup::clear_inherited_planes(gpu.manager.device(), *crtc) {
                warn!(%error, "could not clear inherited planes after session activation");
            }
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
    gv.power_on_monitors("session activation");
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
        &mut gv.launcher,
        &mut gv.classic,
        &mut gv.polkit,
    );
    let bytes = render::capture_elements(&mut gpu.renderer, &elements, (mode.size.w, mode.size.h), scale)?;
    Some((mode.size.w as u32, mode.size.h as u32, bytes))
}

pub(crate) fn set_sdr_white(gv: &mut Gavno, nits: f32) {
    if let Some(gpu) = udev(gv).and_then(|d| d.gpu.as_mut()) {
        for surface in gpu.surfaces.values_mut() {
            surface.white_nits = nits;
            if let Some(hdr) = &mut surface.hdr { hdr.adjust_white(nits - hdr.white_nits); }
        }
    }
}

#[cfg(test)]
mod scanout_tests {
    use super::*;

    #[test]
    fn rejection_latches_across_frames_hdr_and_configuration_changes() {
        let mut state = Scanout::default();
        assert_eq!(state.frame_flags(true, false), SDR_SCANOUT_FLAGS);
        assert!(state.reject());
        assert!(!state.reject());
        for (configured, hdr) in [(true, false), (false, false), (true, true), (true, false)] {
            assert!(state.frame_flags(configured, hdr).is_empty());
            assert!(!state.reject());
        }
    }

    #[test]
    fn hdr_and_disabled_scanout_never_latch_a_scanout_rejection() {
        let mut state = Scanout::default();
        for (configured, hdr) in [(true, true), (false, false), (false, true)] {
            assert!(state.frame_flags(configured, hdr).is_empty());
            assert!(!state.reject());
        }
        assert_eq!(state.frame_flags(true, false), SDR_SCANOUT_FLAGS);
        // Successful frames do not latch a rejection; HDR leaves SDR enabled.
        assert!(state.frame_flags(true, true).is_empty());
        assert_eq!(state.frame_flags(true, false), SDR_SCANOUT_FLAGS);
        assert!(state.reject());
    }

    #[test]
    fn cached_scanner_overrides_force_probe_requests() {
        use std::{cell::Cell, os::fd::{AsFd, BorrowedFd, OwnedFd}};
        use smithay::reexports::drm::{self, control::Device};
        struct Fake { fd: OwnedFd, force: Cell<Option<bool>> }
        impl AsFd for Fake { fn as_fd(&self) -> BorrowedFd<'_> { self.fd.as_fd() } }
        impl drm::Device for Fake {}
        impl Device for Fake {
            fn get_connector(&self, _: connector::Handle, force: bool) -> std::io::Result<connector::Info> {
                self.force.set(Some(force));
                Err(std::io::Error::other("fake connector"))
            }
        }
        let fake = Fake { fd: std::fs::File::open("/dev/null").unwrap().into(), force: Cell::new(None) };
        assert!(CachedConnectors(&fake).get_connector(std::num::NonZeroU32::new(1).unwrap().into(), true).is_err());
        assert_eq!(fake.force.get(), Some(false));
    }
}
