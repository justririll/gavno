//! Initial HDR modeset. Smithay 0.7 cannot attach connector color properties to
//! its commit, so install a black XR30 frame atomically, then let its output
//! manager adopt the same mode/format. Black is zero in both sRGB and PQ.
use super::kms::ConnectorColor;
use smithay::{
    backend::{
        allocator::{
            dmabuf::AsDmabuf,
            gbm::{GbmAllocator, GbmBuffer},
            Allocator, Buffer, Fourcc, Modifier,
        },
        drm::{DrmDevice, DrmDeviceFd},
        renderer::{gles::GlesRenderer, Bind, Frame, Renderer},
    },
    reexports::drm::control::{
        atomic::AtomicModeReq, connector, crtc, framebuffer, property, Device, FbCmd2Flags, Mode,
        ResourceHandle,
    },
    utils::{Rectangle, Transform},
};
use std::{
    collections::{HashMap, HashSet},
    io,
};

pub struct InitialFrame {
    fd: DrmDeviceFd,
    framebuffer: framebuffer::Handle,
    // Keep the BO alive until Smithay's synchronous initialization commit.
    _buffer: GbmBuffer,
}
impl Drop for InitialFrame {
    fn drop(&mut self) {
        let _ = self.fd.destroy_framebuffer(self.framebuffer);
    }
}

fn properties(
    fd: &DrmDeviceFd,
    handle: impl ResourceHandle,
) -> io::Result<HashMap<String, property::Handle>> {
    fd.get_properties(handle)?
        .into_iter()
        .map(|(handle, _)| {
            Ok((
                fd.get_property(handle)?
                    .name()
                    .to_string_lossy()
                    .into_owned(),
                handle,
            ))
        })
        .collect()
}

fn set(
    req: &mut AtomicModeReq,
    handle: impl ResourceHandle,
    props: &HashMap<String, property::Handle>,
    name: &str,
    value: u64,
) -> io::Result<()> {
    let prop = props
        .get(name)
        .ok_or_else(|| io::Error::other(format!("missing KMS property {name}")))?;
    req.add_raw_property(handle.into(), *prop, value);
    Ok(())
}

/// Clear every inherited plane on this CRTC except its selected primary.
/// Smithay clears its cursor/overlay set on the first full frame, but not any
/// additional primary planes left enabled by the previous master.
fn inherited_planes(
    fd: &DrmDeviceFd,
    req: &mut AtomicModeReq,
    crtc: crtc::Handle,
    keep: smithay::reexports::drm::control::plane::Handle,
) -> io::Result<bool> {
    let mut changed = false;
    for plane in fd.plane_handles()? {
        if plane == keep || fd.get_plane(plane)?.crtc() != Some(crtc) {
            continue;
        }
        let props = properties(fd, plane)?;
        set(req, plane, &props, "CRTC_ID", 0)?;
        set(req, plane, &props, "FB_ID", 0)?;
        changed = true;
    }
    Ok(changed)
}

pub fn clear_inherited_planes(drm: &DrmDevice, crtc: crtc::Handle) -> Result<(), String> {
    if !drm.is_atomic() {
        return Ok(());
    }
    let planes = drm.planes(&crtc).map_err(|e| e.to_string())?;
    let primary = planes
        .primary
        .iter()
        .find(|p| drm.claim_plane(p.handle, crtc).is_some())
        .ok_or("no primary plane")?;
    let fd = drm.device_fd();
    let mut req = AtomicModeReq::new();
    if inherited_planes(fd, &mut req, crtc, primary.handle).map_err(|e| e.to_string())? {
        fd.atomic_commit(
            smithay::reexports::drm::control::AtomicCommitFlags::empty(),
            req,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn initialize(
    drm: &DrmDevice,
    allocator: &mut GbmAllocator<DrmDeviceFd>,
    renderer: &mut GlesRenderer,
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    color: &mut ConnectorColor,
    name: &str,
) -> Result<InitialFrame, String> {
    let fd = drm.device_fd();
    let planes = drm.planes(&crtc).map_err(|e| e.to_string())?;
    let primary = planes
        .primary
        .iter()
        .find(|p| drm.claim_plane(p.handle, crtc).is_some())
        .ok_or("no primary plane")?;
    let formats = renderer.egl_context().dmabuf_render_formats();
    let modifiers: Vec<_> = primary
        .formats
        .iter()
        .filter(|f| f.code == Fourcc::Xrgb2101010 && formats.contains(f))
        .map(|f| f.modifier)
        .collect();
    crate::perf::startup_output("udev.output.hdr.black.begin", name);
    let buffer = allocator
        .create_buffer(
            u32::from(mode.size().0),
            u32::from(mode.size().1),
            Fourcc::Xrgb2101010,
            &modifiers,
        )
        .map_err(|e| e.to_string())?;
    let mut dmabuf = buffer.export().map_err(|e| e.to_string())?;
    let size = (i32::from(mode.size().0), i32::from(mode.size().1)).into();
    let mut target = renderer.bind(&mut dmabuf).map_err(|e| e.to_string())?;
    let mut frame = renderer
        .render(&mut target, size, Transform::Normal)
        .map_err(|e| e.to_string())?;
    frame
        .clear([0.0, 0.0, 0.0, 1.0].into(), &[Rectangle::from_size(size)])
        .map_err(|e| e.to_string())?;
    frame
        .finish()
        .map_err(|e| e.to_string())?
        .wait()
        .map_err(|e| e.to_string())?;
    drop(target);
    let flags = if buffer.format().modifier == Modifier::Invalid {
        FbCmd2Flags::empty()
    } else {
        FbCmd2Flags::MODIFIERS
    };
    let framebuffer = fd
        .add_planar_framebuffer(&buffer, flags)
        .map_err(|e| e.to_string())?;
    let initial = InitialFrame {
        fd: fd.clone(),
        framebuffer,
        _buffer: buffer,
    };
    crate::perf::startup_output("udev.output.hdr.black.ready", name);
    let mode_blob = fd.create_property_blob(&mode).map_err(|e| e.to_string())?;
    let result = (|| -> io::Result<()> {
        let mut req = AtomicModeReq::new();
        let props = properties(fd, crtc)?;
        set(&mut req, crtc, &props, "MODE_ID", mode_blob.into())?;
        set(&mut req, crtc, &props, "ACTIVE", 1)?;
        if props.contains_key("VRR_ENABLED") {
            set(&mut req, crtc, &props, "VRR_ENABLED", 0)?;
        }
        // Remove any cloned connectors inherited on this CRTC.
        for handle in fd.resource_handles()?.connectors() {
            let props = properties(fd, *handle)?;
            let current = fd
                .get_properties(*handle)?
                .into_iter()
                .find(|(p, _)| Some(p) == props.get("CRTC_ID"))
                .map(|(_, v)| v);
            if *handle == connector || current == Some(u64::from(u32::from(crtc))) {
                set(
                    &mut req,
                    *handle,
                    &props,
                    "CRTC_ID",
                    if *handle == connector {
                        u64::from(u32::from(crtc))
                    } else {
                        0
                    },
                )?;
            }
        }
        inherited_planes(fd, &mut req, crtc, primary.handle)?;
        let plane = primary.handle;
        let props = properties(fd, plane)?;
        for (key, value) in [
            ("CRTC_ID", u64::from(u32::from(crtc))),
            ("FB_ID", u64::from(u32::from(framebuffer))),
            ("SRC_X", 0),
            ("SRC_Y", 0),
            ("SRC_W", u64::from(mode.size().0) << 16),
            ("SRC_H", u64::from(mode.size().1) << 16),
            ("CRTC_X", 0),
            ("CRTC_Y", 0),
            ("CRTC_W", u64::from(mode.size().0)),
            ("CRTC_H", u64::from(mode.size().1)),
        ] {
            set(&mut req, plane, &props, key, value)?;
        }
        for (key, value) in [
            ("rotation", 1),
            ("alpha", 0xffff),
            ("FB_DAMAGE_CLIPS", 0),
            ("IN_FENCE_FD", u64::MAX),
        ] {
            if props.contains_key(key) {
                set(&mut req, plane, &props, key, value)?;
            }
        }
        color.commit_initial_hdr(req, name)
    })();
    let _ = fd.destroy_property_blob(mode_blob.into());
    result.map_err(|e| e.to_string())?;
    Ok(initial)
}

/// With disable_connectors=false Smithay deliberately leaves unused CRTCs
/// alone. Disable only the inherited outputs we did not acquire, without
/// retraining any live connector or touching a plane owned by another output.
pub fn clear_unused_outputs(drm: &DrmDevice, active: &HashSet<crtc::Handle>) -> Result<(), String> {
    if !drm.is_atomic() {
        return Ok(());
    }
    let fd = drm.device_fd();
    let result = (|| -> io::Result<()> {
        let mut req = AtomicModeReq::new();
        let mut changed = false;
        for plane in fd.plane_handles()? {
            if fd
                .get_plane(plane)?
                .crtc()
                .is_some_and(|crtc| !active.contains(&crtc))
            {
                let props = properties(fd, plane)?;
                set(&mut req, plane, &props, "CRTC_ID", 0)?;
                set(&mut req, plane, &props, "FB_ID", 0)?;
                changed = true;
            }
        }
        for connector in fd.resource_handles()?.connectors() {
            let props = properties(fd, *connector)?;
            let current = fd
                .get_properties(*connector)?
                .into_iter()
                .find(|(p, _)| Some(p) == props.get("CRTC_ID"))
                .map(|(_, v)| v)
                .unwrap_or(0);
            if current != 0
                && !active
                    .iter()
                    .any(|crtc| u64::from(u32::from(*crtc)) == current)
            {
                set(&mut req, *connector, &props, "CRTC_ID", 0)?;
                changed = true;
            }
        }
        for crtc in drm.crtcs().iter().filter(|crtc| !active.contains(crtc)) {
            if fd.get_crtc(*crtc)?.mode().is_none() {
                continue;
            }
            let props = properties(fd, *crtc)?;
            set(&mut req, *crtc, &props, "ACTIVE", 0)?;
            set(&mut req, *crtc, &props, "MODE_ID", 0)?;
            changed = true;
        }
        if changed {
            fd.atomic_commit(
                smithay::reexports::drm::control::AtomicCommitFlags::ALLOW_MODESET,
                req,
            )?;
        }
        Ok(())
    })();
    result.map_err(|e| e.to_string())
}
