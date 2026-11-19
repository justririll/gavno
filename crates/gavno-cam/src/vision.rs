//! Pre- and post-processing around the MediaPipe palm detector and hand
//! landmark networks. Pure arithmetic, testable without a model.

use std::f32::consts::PI;

pub const PALM_SIZE: usize = 192;
pub const HAND_SIZE: usize = 224;
pub const ANCHORS: usize = 2016;

/// SSD anchor centres of the 192x192 palm detector: a stride-8 grid with two
/// anchors per cell followed by a stride-16 grid with six, in row-major order.
pub fn palm_anchors() -> Vec<[f32; 2]> {
    let mut anchors = Vec::with_capacity(ANCHORS);
    for (cells, per_cell) in [(PALM_SIZE / 8, 2), (PALM_SIZE / 16, 6)] {
        for y in 0..cells {
            for x in 0..cells {
                for _ in 0..per_cell {
                    anchors.push([(x as f32 + 0.5) / cells as f32, (y as f32 + 0.5) / cells as f32]);
                }
            }
        }
    }
    anchors
}

/// How a frame was fitted into the square palm detector input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Letterbox {
    pub scale: f32,
    pub pad_x: f32,
    pub pad_y: f32,
}

impl Letterbox {
    pub fn new(width: usize, height: usize) -> Self {
        let scale = PALM_SIZE as f32 / width.max(height) as f32;
        let fitted = |side: usize| (side as f32 * scale).round().min(PALM_SIZE as f32);
        Self {
            scale,
            pad_x: ((PALM_SIZE as f32 - fitted(width)) / 2.0).floor(),
            pad_y: ((PALM_SIZE as f32 - fitted(height)) / 2.0).floor(),
        }
    }
}

/// Fits an RGB8 frame into a 192x192 f32 tensor (0..1, NHWC), box-filtering
/// the downscale and padding the rest with black.
pub fn letterbox(rgb: &[u8], width: usize, height: usize, out: &mut Vec<f32>) -> Letterbox {
    let fit = Letterbox::new(width, height);
    out.clear();
    out.resize(PALM_SIZE * PALM_SIZE * 3, 0.0);
    let (w, h) = ((width as f32 * fit.scale).round() as usize, (height as f32 * fit.scale).round() as usize);
    let (left, top) = (fit.pad_x as usize, fit.pad_y as usize);
    for y in 0..h.min(PALM_SIZE - top) {
        let (y0, y1) = (y * height / h, ((y + 1) * height / h).max(y * height / h + 1).min(height));
        for x in 0..w.min(PALM_SIZE - left) {
            let (x0, x1) = (x * width / w, ((x + 1) * width / w).max(x * width / w + 1).min(width));
            let mut sum = [0u32; 3];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = (sy * width + sx) * 3;
                    sum[0] += u32::from(rgb[p]);
                    sum[1] += u32::from(rgb[p + 1]);
                    sum[2] += u32::from(rgb[p + 2]);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32 * 255.0;
            let o = ((y + top) * PALM_SIZE + x + left) * 3;
            for c in 0..3 {
                out[o + c] = sum[c] as f32 / n;
            }
        }
    }
    fit
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palm {
    pub score: f32,
    /// Box centre and size in frame pixels.
    pub center: [f32; 2],
    pub size: [f32; 2],
    /// Wrist and middle-finger base, in frame pixels.
    pub wrist: [f32; 2],
    pub middle: [f32; 2],
}

fn sigmoid(x: f32) -> f32 { 1.0 / (1.0 + (-x).exp()) }

/// Picks the best palm out of the raw network outputs: `boxes` is 2016x18
/// (cx, cy, w, h and seven keypoints, in input pixels relative to the anchor),
/// `scores` is 2016 logits.
pub fn decode_palm(boxes: &[f32], scores: &[f32], anchors: &[[f32; 2]], fit: Letterbox, frame: (usize, usize), threshold: f32) -> Option<Palm> {
    if boxes.len() < ANCHORS * 18 || scores.len() < ANCHORS || anchors.len() < ANCHORS { return None; }
    let (best, logit) = scores[..ANCHORS].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    let score = sigmoid(*logit);
    if score < threshold { return None; }
    let b = &boxes[best * 18..best * 18 + 18];
    let a = anchors[best];
    let side = frame.0.max(frame.1) as f32;
    let (pad_x, pad_y) = (fit.pad_x / fit.scale, fit.pad_y / fit.scale);
    let input = PALM_SIZE as f32;
    let to_frame = |x: f32, y: f32| [(x / input + a[0]) * side - pad_x, (y / input + a[1]) * side - pad_y];
    let center = to_frame(b[0], b[1]);
    Some(Palm {
        score,
        center,
        size: [b[2] / input * side, b[3] / input * side],
        wrist: to_frame(b[4], b[5]),
        middle: to_frame(b[8], b[9]),
    })
}

/// A rotated square crop of the frame fed to the landmark network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Roi {
    pub center: [f32; 2],
    pub size: f32,
    /// Rotation that makes the hand point up, radians.
    pub angle: f32,
}

fn rotate(v: [f32; 2], angle: f32) -> [f32; 2] {
    let (s, c) = angle.sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c]
}

fn normalize_angle(a: f32) -> f32 { a - 2.0 * PI * ((a + PI) / (2.0 * PI)).floor() }

/// Angle that rotates the vector wrist -> finger base to point up.
fn up_angle(from: [f32; 2], to: [f32; 2]) -> f32 {
    normalize_angle(PI / 2.0 - (-(to[1] - from[1])).atan2(to[0] - from[0]))
}

/// MediaPipe's palm-to-hand rectangle: shifted toward the fingers and 2.6x.
pub fn roi_from_palm(palm: &Palm) -> Roi {
    let angle = up_angle(palm.wrist, palm.middle);
    let shift = rotate([0.0, -0.5 * palm.size[1]], angle);
    Roi {
        center: [palm.center[0] + shift[0], palm.center[1] + shift[1]],
        size: palm.size[0].max(palm.size[1]) * 2.6,
        angle,
    }
}

/// The rectangle for the next frame from this frame's landmarks (frame pixels):
/// bounding box in the hand's own orientation, shifted toward the fingers, 2x.
pub fn roi_from_landmarks(lm: &[[f32; 2]; 21]) -> Roi {
    let mid = [(lm[5][0] + lm[13][0]) / 2.0, (lm[5][1] + lm[13][1]) / 2.0];
    let target = [(mid[0] + lm[9][0]) / 2.0, (mid[1] + lm[9][1]) / 2.0];
    let angle = up_angle(lm[0], target);
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for p in lm {
        let r = rotate([p[0] - lm[0][0], p[1] - lm[0][1]], -angle);
        for i in 0..2 {
            lo[i] = lo[i].min(r[i]);
            hi[i] = hi[i].max(r[i]);
        }
    }
    let (w, h) = (hi[0] - lo[0], hi[1] - lo[1]);
    let local = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0 - 0.1 * h];
    let back = rotate(local, angle);
    Roi { center: [lm[0][0] + back[0], lm[0][1] + back[1]], size: w.max(h) * 2.0, angle }
}

/// Samples the ROI from an RGB8 frame into a 224x224 f32 tensor (0..1, NHWC)
/// with bilinear filtering; outside the frame is black.
pub fn warp_roi(rgb: &[u8], width: usize, height: usize, roi: &Roi, out: &mut Vec<f32>) {
    out.clear();
    out.resize(HAND_SIZE * HAND_SIZE * 3, 0.0);
    let n = HAND_SIZE as f32;
    let (s, c) = roi.angle.sin_cos();
    for v in 0..HAND_SIZE {
        let ly = ((v as f32 + 0.5) / n - 0.5) * roi.size;
        for u in 0..HAND_SIZE {
            let lx = ((u as f32 + 0.5) / n - 0.5) * roi.size;
            let sx = lx * c - ly * s + roi.center[0] - 0.5;
            let sy = lx * s + ly * c + roi.center[1] - 0.5;
            let (x0, y0) = (sx.floor(), sy.floor());
            let (fx, fy) = (sx - x0, sy - y0);
            let o = (v * HAND_SIZE + u) * 3;
            for (dx, dy, weight) in [(0, 0, (1.0 - fx) * (1.0 - fy)), (1, 0, fx * (1.0 - fy)), (0, 1, (1.0 - fx) * fy), (1, 1, fx * fy)] {
                let (x, y) = (x0 as i32 + dx, y0 as i32 + dy);
                if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 { continue; }
                let p = (y as usize * width + x as usize) * 3;
                for ch in 0..3 {
                    out[o + ch] += f32::from(rgb[p + ch]) * weight / 255.0;
                }
            }
        }
    }
}

/// Maps the network's 21 (x, y) landmarks from crop pixels to frame pixels.
pub fn landmarks_to_frame(raw: &[f32], roi: &Roi) -> [[f32; 2]; 21] {
    let mut out = [[0.0; 2]; 21];
    let n = HAND_SIZE as f32;
    for (i, p) in out.iter_mut().enumerate() {
        let local = [(raw[i * 3] / n - 0.5) * roi.size, (raw[i * 3 + 1] / n - 0.5) * roi.size];
        let r = rotate(local, roi.angle);
        *p = [r[0] + roi.center[0], r[1] + roi.center[1]];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_match_the_reference_table() {
        let a = palm_anchors();
        assert_eq!(a.len(), ANCHORS);
        let close = |p: [f32; 2], x: f32, y: f32| (p[0] - x).abs() < 1e-5 && (p[1] - y).abs() < 1e-5;
        assert!(close(a[0], 0.02083333, 0.02083333) && close(a[1], 0.02083333, 0.02083333));
        assert!(close(a[2], 0.0625, 0.02083333));
        assert!(close(a[1151], 0.9791667, 0.9791667));
        assert!(close(a[1152], 0.04166667, 0.04166667) && close(a[1157], 0.04166667, 0.04166667));
        assert!(close(a[1158], 0.125, 0.04166667));
        assert!(close(a[2015], 0.9583333, 0.9583333));
    }

    #[test]
    fn letterbox_pads_the_short_side() {
        let fit = Letterbox::new(640, 360);
        assert!((fit.scale - 0.3).abs() < 1e-6);
        assert_eq!((fit.pad_x, fit.pad_y), (0.0, 42.0));
        let rgb = vec![255u8; 640 * 360 * 3];
        let mut out = Vec::new();
        letterbox(&rgb, 640, 360, &mut out);
        assert_eq!(out.len(), PALM_SIZE * PALM_SIZE * 3);
        assert_eq!(out[0], 0.0); // top padding
        assert!((out[(96 * PALM_SIZE + 96) * 3] - 1.0).abs() < 1e-6);
        assert_eq!(out[(PALM_SIZE - 1) * PALM_SIZE * 3], 0.0); // bottom padding
    }

    #[test]
    fn decode_maps_back_to_frame_pixels() {
        let anchors = palm_anchors();
        let fit = Letterbox::new(640, 360);
        let mut boxes = vec![0.0; ANCHORS * 18];
        let mut scores = vec![-10.0; ANCHORS];
        // Anchor 1152 sits at (8, 8) of 192: frame pixel (26.67, 26.67 - 140).
        // Pick one at the middle of the input instead: the first stride-16 cell
        // whose centre is (0.5, 0.5) is cell (5,5) or (6,6).
        let index = 1152 + (5 * 12 + 5) * 6;
        let a = anchors[index];
        assert!((a[0] - 5.5 / 12.0).abs() < 1e-6);
        scores[index] = 4.0;
        boxes[index * 18 + 2] = 38.4; // width 0.2 of the input = 128 px of frame
        boxes[index * 18 + 3] = 38.4;
        let palm = decode_palm(&boxes, &scores, &anchors, fit, (640, 360), 0.5).unwrap();
        assert!((palm.center[0] - a[0] * 640.0).abs() < 1e-2);
        assert!((palm.center[1] - (a[1] * 640.0 - 140.0)).abs() < 1e-2);
        assert!((palm.size[0] - 128.0).abs() < 1e-2);
        assert!(decode_palm(&boxes, &vec![-10.0; ANCHORS], &anchors, fit, (640, 360), 0.5).is_none());
    }

    #[test]
    fn upright_hand_has_zero_rotation() {
        let palm = Palm { score: 1.0, center: [100.0, 100.0], size: [40.0, 40.0], wrist: [100.0, 120.0], middle: [100.0, 80.0] };
        let roi = roi_from_palm(&palm);
        assert!(roi.angle.abs() < 1e-6);
        assert!((roi.size - 104.0).abs() < 1e-3);
        // Shifted 0.5 box heights toward the fingers (up in the frame).
        assert!((roi.center[1] - 80.0).abs() < 1e-3 && (roi.center[0] - 100.0).abs() < 1e-3);
    }

    #[test]
    fn hand_pointing_right_rotates_a_quarter_turn() {
        let a = up_angle([0.0, 0.0], [1.0, 0.0]);
        assert!((a - PI / 2.0).abs() < 1e-6);
        // Local "up" (0, -1) must map to the frame's right.
        let r = rotate([0.0, -1.0], a);
        assert!((r[0] - 1.0).abs() < 1e-6 && r[1].abs() < 1e-6);
        assert!(up_angle([0.0, 0.0], [-1.0, 0.0]) + PI / 2.0 < 1e-6);
    }

    #[test]
    fn identity_roi_round_trips_through_warp_and_landmarks() {
        // A 224x224 frame with a unique value at every pixel column.
        let mut rgb = vec![0u8; 224 * 224 * 3];
        for y in 0..224 {
            for x in 0..224 {
                let p = (y * 224 + x) * 3;
                rgb[p] = x as u8;
                rgb[p + 1] = y as u8;
            }
        }
        let roi = Roi { center: [112.0, 112.0], size: 224.0, angle: 0.0 };
        let mut out = Vec::new();
        warp_roi(&rgb, 224, 224, &roi, &mut out);
        let at = |x: usize, y: usize| out[(y * 224 + x) * 3] * 255.0;
        assert!((at(50, 60) - 50.0).abs() < 0.01);
        assert!((out[(60 * 224 + 50) * 3 + 1] * 255.0 - 60.0).abs() < 0.01);
        let mut raw = vec![0.0; 63];
        raw[3] = 50.0;
        raw[4] = 60.0;
        let lm = landmarks_to_frame(&raw, &roi);
        assert!((lm[1][0] - 50.0).abs() < 1e-4 && (lm[1][1] - 60.0).abs() < 1e-4);
        // Rotated by a quarter turn, crop-local up points to frame right.
        let roi = Roi { center: [100.0, 100.0], size: 224.0, angle: PI / 2.0 };
        let mut raw = vec![0.0; 63];
        raw[0] = 112.0;
        raw[1] = 0.0; // top middle of the crop
        let lm = landmarks_to_frame(&raw, &roi);
        assert!((lm[0][0] - 212.0).abs() < 1e-3 && (lm[0][1] - 100.0).abs() < 1e-3);
    }

    #[test]
    fn landmark_roi_follows_the_hand() {
        let mut lm = [[0.0f32; 2]; 21];
        // An upright hand: wrist at the bottom, finger bases above, tips at the top.
        for (i, p) in lm.iter_mut().enumerate() {
            *p = [100.0 + (i % 5) as f32 * 4.0, 200.0 - (i as f32) * 4.0];
        }
        lm[0] = [110.0, 200.0];
        let roi = roi_from_landmarks(&lm);
        assert!(roi.angle.abs() < 0.5);
        assert!(roi.size > 100.0);
        assert!(roi.center[1] < 200.0);
    }
}
