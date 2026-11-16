//! wlr-screencopy-unstable-v1 (version 3): output and region capture into shm
//! buffers, for grim, slurp-based tools and screen recorders.
//!
//! `copy` renders the requested output offscreen right away. `copy_with_damage`
//! waits until GAVNO draws a new frame for that output, so recorders that loop
//! on it follow real screen updates instead of spinning.
use crate::Gavno;
use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, Flags, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            protocol::{wl_buffer::WlBuffer, wl_shm},
            backend::ClientId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
            Resource,
        },
    },
    utils::{Physical, Rectangle},
    wayland::shm::{with_buffer_contents_mut, BufferData},
};
use std::sync::atomic::{AtomicBool, Ordering};

const VERSION: u32 = 3;

#[derive(Default)]
pub struct Screencopy {
    pending: Vec<Pending>,
    /// Outputs that presented new content since their last completed
    /// capture; a `copy_with_damage` for them is answered at once.
    dirty: Vec<Output>,
    /// Clients that already received a damage-tracked frame of an output.
    /// Their first `copy_with_damage` has nothing to compare against, so it
    /// is answered at once (as wlroots does); otherwise a recorder started on
    /// a still screen would wait for its first frame forever.
    tracked: Vec<(ClientId, Output)>,
}

impl Screencopy {
    pub(crate) fn fail_pending(&mut self) {
        for p in self.pending.drain(..) { p.frame.failed(); }
    }
    /// Fails captures waiting on an output that is going away.
    pub(crate) fn output_removed(&mut self, output: &Output) {
        self.dirty.retain(|o| o != output);
        self.tracked.retain(|(_, o)| o != output);
        self.pending.retain(|p| {
            let ours = p.frame.data::<FrameData>().and_then(|d| d.output.as_ref()) == Some(output);
            if ours && p.frame.is_alive() {
                p.frame.failed();
            }
            !ours
        });
    }
}

struct Pending {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if std::thread::panicking() && self.frame.is_alive() { self.frame.failed(); }
    }
}

pub struct FrameData {
    output: Option<Output>,
    /// Requested area in the output's physical pixels.
    region: Rectangle<i32, Physical>,
    cursor: bool,
    used: AtomicBool,
}

/// Advertises the global. Not called in greeter mode.
pub fn init(gv: &mut Gavno) {
    gv.display_handle
        .create_global::<Gavno, ZwlrScreencopyManagerV1, _>(VERSION, ());
}

/// Physical region of `output` covered by a logical rectangle relative to the
/// output, clipped to the output. `None` when nothing remains.
pub fn physical_region(
    logical: Rectangle<i32, smithay::utils::Logical>,
    scale: f64,
    output_size: (i32, i32),
) -> Option<Rectangle<i32, Physical>> {
    // Client-supplied values: add in f64 so huge coordinates cannot overflow,
    // and let the saturating float-to-int casts clamp them.
    let x0 = (logical.loc.x as f64 * scale).floor() as i32;
    let y0 = (logical.loc.y as f64 * scale).floor() as i32;
    let x1 = ((logical.loc.x as f64 + logical.size.w as f64) * scale).ceil() as i32;
    let y1 = ((logical.loc.y as f64 + logical.size.h as f64) * scale).ceil() as i32;
    let (x0, y0) = (x0.max(0), y0.max(0));
    let (x1, y1) = (x1.min(output_size.0), y1.min(output_size.1));
    (x1 > x0 && y1 > y0).then(|| Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()))
}

/// Copies `region` out of a tightly packed RGBA frame of width `frame_w` into a
/// little-endian XRGB8888 destination with `stride`.
pub fn blit_xrgb(
    src: &[u8],
    frame_w: usize,
    region: Rectangle<i32, Physical>,
    dst: &mut [u8],
    stride: usize,
) -> bool {
    let (rx, ry, rw, rh) = (
        region.loc.x as usize,
        region.loc.y as usize,
        region.size.w as usize,
        region.size.h as usize,
    );
    if stride < rw * 4 || dst.len() < stride * rh.saturating_sub(1) + rw * 4 {
        return false;
    }
    for row in 0..rh {
        let s = ((ry + row) * frame_w + rx) * 4;
        let Some(src_row) = src.get(s..s + rw * 4) else {
            return false;
        };
        let dst_row = &mut dst[row * stride..row * stride + rw * 4];
        for (d, p) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(4)) {
            // RGBA bytes -> B, G, R, X (XRGB8888 in memory order).
            d[0] = p[2];
            d[1] = p[1];
            d[2] = p[0];
            d[3] = 0xff;
        }
    }
    true
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Gavno {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Gavno {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let (frame, cursor, output, logical) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput { frame, overlay_cursor, output } => {
                (frame, overlay_cursor != 0, output, None)
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (
                frame,
                overlay_cursor != 0,
                output,
                Some(Rectangle::new((x, y).into(), (width, height).into())),
            ),
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => return,
        };
        let (output, geometry) = state.guard(crate::module::Feature::Screencopy, (None, None), |state| {
        let output = Output::from_resource(&output).filter(|o| state.space.outputs().any(|s| s == o));
        let geometry = output.as_ref().and_then(|o| {
            let mode = o.current_mode()?;
            let scale = o.current_scale().fractional_scale();
            let size = (mode.size.w, mode.size.h);
            let logical = logical.unwrap_or_else(|| {
                let geo = state.space.output_geometry(o).unwrap_or_default();
                Rectangle::from_size(geo.size)
            });
            physical_region(logical, scale, size)
        });
        (output, geometry)
        });
        let frame = data_init.init(
            frame,
            FrameData {
                output: output.filter(|_| geometry.is_some()),
                region: geometry.unwrap_or_default(),
                cursor,
                used: AtomicBool::new(false),
            },
        );
        let Some(region) = geometry else {
            frame.failed();
            return;
        };
        let (w, h) = (region.size.w as u32, region.size.h as u32);
        frame.buffer(wl_shm::Format::Xrgb8888, w, h, w * 4);
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for Gavno {
    fn request(
        state: &mut Self,
        client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => return,
        };
        if data.used.swap(true, Ordering::SeqCst) {
            frame.post_error(zwlr_screencopy_frame_v1::Error::AlreadyUsed, "frame already used");
            return;
        }
        if !state.features.on(crate::module::Feature::Screencopy) || data.output.is_none() {
            frame.failed();
            return;
        }
        let (w, h) = (data.region.size.w, data.region.size.h);
        let valid = with_buffer_contents_mut(&buffer, |_, _, info: BufferData| {
            info.format == wl_shm::Format::Xrgb8888
                && info.width == w
                && info.height == h
                && info.stride >= w * 4
        })
        .unwrap_or(false);
        if !valid {
            frame.post_error(zwlr_screencopy_frame_v1::Error::InvalidBuffer, "invalid buffer");
            return;
        }
        let pending = Pending { frame: frame.clone(), buffer };
        let output = frame.data::<FrameData>().and_then(|d| d.output.clone());
        let mut dirty = output.as_ref().is_some_and(|o| state.protocols.screencopy.dirty.contains(o));
        if let (true, Some(output)) = (with_damage, output.as_ref()) {
            let tracked = &mut state.protocols.screencopy.tracked;
            tracked.retain(|(id, _)| dh.backend_handle().get_client_data(id.clone()).is_ok());
            let key = (client.id(), output.clone());
            if !tracked.contains(&key) {
                tracked.push(key);
                dirty = true;
            }
        }
        if with_damage && !dirty {
            state.protocols.screencopy.pending.push(pending);
            state.queue_redraw_all();
        } else {
            state.guard(crate::module::Feature::Screencopy, (), |gv| complete(gv, pending));
        }
    }
}

/// Completes `copy_with_damage` requests for `output` after a new frame was drawn.
pub fn frame_drawn(gv: &mut Gavno, output: &Output) {
    gv.guard(crate::module::Feature::Screencopy, (), |gv| {
        if gv.protocols.screencopy.pending.is_empty() {
            if !gv.protocols.screencopy.dirty.contains(output) {
                gv.protocols.screencopy.dirty.push(output.clone());
            }
            return;
        }
        let mut ready = Vec::new();
        gv.protocols.screencopy.pending.retain(|p| {
            if !p.frame.is_alive() {
                return false;
            }
            let ours = p
                .frame
                .data::<FrameData>()
                .and_then(|d| d.output.as_ref())
                .is_some_and(|o| o == output);
            if ours {
                ready.push(Pending { frame: p.frame.clone(), buffer: p.buffer.clone() });
            }
            !ours
        });
        for p in ready {
            complete(gv, p);
        }
    });
}

fn complete(gv: &mut Gavno, p: Pending) {
    let Some(data) = p.frame.data::<FrameData>() else { return };
    let Some(output) = data.output.clone() else {
        p.frame.failed();
        return;
    };
    let region = data.region;
    gv.protocols.screencopy.dirty.retain(|o| o != &output);
    let Some((frame_w, _, pixels)) = crate::backend::capture_output(gv, &output, data.cursor) else {
        p.frame.failed();
        return;
    };
    let copied = with_buffer_contents_mut(&p.buffer, |ptr, len, info: BufferData| {
        let offset = info.offset.max(0) as usize;
        if offset > len {
            return false;
        }
        // SAFETY: smithay maps the client pool for the duration of this callback.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(offset), len - offset) };
        blit_xrgb(&pixels, frame_w as usize, region, dst, info.stride as usize)
    })
    .unwrap_or(false);
    if !copied {
        p.frame.failed();
        return;
    }
    p.frame.flags(Flags::empty());
    if p.frame.version() >= 2 {
        p.frame.damage(0, 0, region.size.w as u32, region.size.h as u32);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    p.frame.ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_scales_and_clips_to_the_output() {
        let r = physical_region(Rectangle::new((10, 20).into(), (100, 50).into()), 1.75, (2880, 1620)).unwrap();
        assert_eq!((r.loc.x, r.loc.y, r.size.w, r.size.h), (17, 35, 176, 88));
        let full = physical_region(Rectangle::from_size((1646, 926).into()), 1.75, (2880, 1620)).unwrap();
        assert_eq!((full.size.w, full.size.h), (2880, 1620));
        let clipped = physical_region(Rectangle::new((-10, -10).into(), (20, 20).into()), 1.0, (100, 100)).unwrap();
        assert_eq!((clipped.loc.x, clipped.size.w), (0, 10));
        assert!(physical_region(Rectangle::new((200, 0).into(), (10, 10).into()), 1.0, (100, 100)).is_none());
        let huge = physical_region(Rectangle::new((i32::MAX, i32::MAX).into(), (i32::MAX, i32::MAX).into()), 1.75, (100, 100));
        assert!(huge.is_none());
        let negative = physical_region(Rectangle::new((i32::MIN, 0).into(), (i32::MAX, 10).into()), 1.0, (100, 100));
        assert!(negative.is_none());
    }

    #[test]
    fn blit_swizzles_rgba_into_xrgb_with_stride() {
        // 2x2 RGBA frame.
        let src = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let region = Rectangle::new((1, 0).into(), (1, 2).into());
        let mut dst = [0u8; 16];
        assert!(blit_xrgb(&src, 2, region, &mut dst, 8));
        assert_eq!(&dst[0..4], &[7, 6, 5, 0xff]);
        assert_eq!(&dst[8..12], &[15, 14, 13, 0xff]);
        assert!(!blit_xrgb(&src, 2, region, &mut [0u8; 4], 8));
    }
}
