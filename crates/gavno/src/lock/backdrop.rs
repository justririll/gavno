//! The blurred, dimmed desktop behind the lock screen.
//!
//! The renderer reads the desktop back once per output when the session
//! locks. Downscaling and blurring run on a worker thread, and the small
//! result is uploaded as an ordinary texture, so no frame ever blurs.
use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    reexports::calloop::channel,
    utils::Transform,
};
use std::collections::{HashMap, HashSet};

/// Source pixels per backdrop pixel, per axis.
const DOWNSCALE: usize = 8;
/// Box radius and pass count; three passes approximate a Gaussian.
const BLUR_RADIUS: usize = 5;
const BLUR_PASSES: usize = 3;
/// Brightness kept after dimming.
const DIM: f32 = 0.5;

/// A finished backdrop, premultiplied ARGB8888 (opaque).
pub struct Blurred {
    pub epoch: u64,
    pub output: String,
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

#[derive(Default)]
pub struct Backdrop {
    epoch: u64,
    wanted: bool,
    taken: HashSet<String>,
    buffers: HashMap<String, (MemoryRenderBuffer, (i32, i32))>,
    tx: Option<channel::Sender<Blurred>>,
    pub source: Option<smithay::reexports::calloop::RegistrationToken>,
}

impl Backdrop {
    /// A new lock wants one capture per output.
    pub fn request(&mut self, tx: channel::Sender<Blurred>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.wanted = true;
        self.taken.clear();
        self.buffers.clear();
        self.tx = Some(tx);
    }
    #[cfg(test)]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    /// Forget everything; late worker results are ignored.
    pub fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.wanted = false;
        self.taken.clear();
        self.buffers.clear();
        self.tx = None;
    }
    pub fn wants(&self, output: &str) -> bool {
        self.wanted && self.tx.is_some() && !self.taken.contains(output)
    }
    /// Hand the RGBA readback of `output` to a worker.
    pub fn start(&mut self, output: &str, rgba: Vec<u8>, width: usize, height: usize) {
        self.taken.insert(output.to_owned());
        let Some(tx) = self.tx.clone() else { return };
        let (epoch, name) = (self.epoch, output.to_owned());
        let spawned = std::thread::Builder::new()
            .name("lock-backdrop".into())
            .spawn(move || {
                if let Some((w, h, argb)) = blur(&rgba, width, height) {
                    let _ = tx.send(Blurred { epoch, output: name, width: w as i32, height: h as i32, argb });
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start the lock backdrop worker");
        }
    }
    /// Store a worker result; false when it belongs to an older lock.
    pub fn accept(&mut self, blurred: Blurred) -> bool {
        if blurred.epoch != self.epoch || !self.wanted {
            return false;
        }
        let buffer = MemoryRenderBuffer::from_slice(
            &blurred.argb,
            Fourcc::Argb8888,
            (blurred.width, blurred.height),
            1,
            Transform::Normal,
            None,
        );
        self.buffers.insert(blurred.output, (buffer, (blurred.width, blurred.height)));
        true
    }
    pub fn buffer(&self, output: &str) -> Option<(MemoryRenderBuffer, (i32, i32))> {
        self.buffers.get(output).cloned()
    }
}

/// Box-downscale an RGBA image, blur it and dim it; returns the small size
/// and premultiplied ARGB8888 bytes.
pub fn blur(rgba: &[u8], width: usize, height: usize) -> Option<(usize, usize, Vec<u8>)> {
    if width == 0 || height == 0 || rgba.len() < width * height * 4 {
        return None;
    }
    let (w, h) = (width.div_ceil(DOWNSCALE), height.div_ceil(DOWNSCALE));
    let mut image = vec![[0.0f32; 3]; w * h];
    for y in 0..h {
        for x in 0..w {
            let (x0, y0) = (x * DOWNSCALE, y * DOWNSCALE);
            let (x1, y1) = ((x0 + DOWNSCALE).min(width), (y0 + DOWNSCALE).min(height));
            let mut sum = [0u32; 3];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &rgba[(sy * width + sx) * 4..];
                    sum[0] += u32::from(p[0]);
                    sum[1] += u32::from(p[1]);
                    sum[2] += u32::from(p[2]);
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as f32;
            image[y * w + x] = sum.map(|c| c as f32 / n);
        }
    }
    let mut scratch = image.clone();
    for _ in 0..BLUR_PASSES {
        box_pass(&image, &mut scratch, w, h, true);
        box_pass(&scratch, &mut image, w, h, false);
    }
    let mut argb = Vec::with_capacity(w * h * 4);
    for [r, g, b] in image {
        // Pull the colours slightly towards grey so busy windows do not
        // fight the clock, then dim.
        let grey = (r + g + b) / 3.0;
        let mix = |c: f32| ((c * 0.7 + grey * 0.3) * DIM).round().clamp(0.0, 255.0) as u8;
        argb.extend([mix(b), mix(g), mix(r), 255]);
    }
    Some((w, h, argb))
}

/// One running-sum box blur along rows (`horizontal`) or columns, clamping at the edges.
fn box_pass(src: &[[f32; 3]], dst: &mut [[f32; 3]], w: usize, h: usize, horizontal: bool) {
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let at = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let window = (2 * BLUR_RADIUS + 1) as f32;
    for line in 0..lines {
        let mut sum = [0.0f32; 3];
        for k in -(BLUR_RADIUS as isize)..=BLUR_RADIUS as isize {
            let p = src[at(line, k.clamp(0, len as isize - 1) as usize)];
            for c in 0..3 {
                sum[c] += p[c];
            }
        }
        for i in 0..len {
            dst[at(line, i)] = sum.map(|c| c / window);
            let add = (i + BLUR_RADIUS + 1).min(len - 1);
            let sub = i.saturating_sub(BLUR_RADIUS);
            let (a, s) = (src[at(line, add)], src[at(line, sub)]);
            for c in 0..3 {
                sum[c] += a[c] - s[c];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_image(w: usize, h: usize) -> Vec<u8> {
        let mut rgba = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 255 } else { 0 };
                rgba.extend([v, v, v, 255]);
            }
        }
        rgba
    }

    #[test]
    fn blur_downscales_dims_and_softens_edges() {
        let (w, h, argb) = blur(&split_image(640, 320), 640, 320).unwrap();
        assert_eq!((w, h), (80, 40));
        assert_eq!(argb.len(), 80 * 40 * 4);
        let row = &argb[20 * 80 * 4..21 * 80 * 4];
        let value = |x: usize| row[x * 4 + 2];
        assert!(row.chunks(4).all(|p| p[3] == 255), "the backdrop is opaque");
        assert!(value(0) <= 128 && value(0) >= 120, "dimmed white: {}", value(0));
        assert_eq!(value(79), 0);
        let softened = (0..80).filter(|x| value(*x) > 4 && value(*x) < 120).count();
        assert!(softened >= 8, "the edge must be a ramp, not a step: {softened}");
        assert!((0..79).all(|x| value(x + 1) <= value(x)), "monotonic falloff");
    }
    #[test]
    fn blur_rejects_empty_or_short_input() {
        assert!(blur(&[], 0, 0).is_none());
        assert!(blur(&[0; 8], 4, 4).is_none());
        assert!(blur(&[0; 4], 1, 1).is_some());
    }
    #[test]
    fn stale_results_are_dropped_and_clear_forgets_buffers() {
        let (tx, _rx) = channel::channel();
        let mut backdrop = Backdrop::default();
        backdrop.request(tx.clone());
        assert!(backdrop.wants("eDP-1"));
        let result = |epoch| Blurred { epoch, output: "eDP-1".into(), width: 1, height: 1, argb: vec![0, 0, 0, 255] };
        assert!(!backdrop.accept(result(backdrop.epoch.wrapping_sub(1))));
        assert!(backdrop.accept(result(backdrop.epoch)));
        assert!(backdrop.buffer("eDP-1").is_some());
        backdrop.clear();
        assert!(backdrop.buffer("eDP-1").is_none());
        assert!(!backdrop.wants("eDP-1"));
        assert!(!backdrop.accept(result(backdrop.epoch)));
    }
}
