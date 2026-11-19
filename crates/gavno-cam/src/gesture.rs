//! Landmarks to pointer messages: absolute mapping, filtering and gestures.
//! Pure logic with no camera or model, driven by synthetic landmarks in tests.

use crate::{filter::OneEuro, protocol::Message};

/// 21 MediaPipe hand landmarks in camera-frame coordinates: x and y in 0..1,
/// x to the right and y down, not mirrored.
pub type Landmarks = [[f32; 2]; 21];

pub const WRIST: usize = 0;
pub const THUMB_TIP: usize = 4;
pub const INDEX_MCP: usize = 5;
pub const INDEX_TIP: usize = 8;
pub const MIDDLE_MCP: usize = 9;
pub const MIDDLE_TIP: usize = 12;
pub const RING_MCP: usize = 13;
pub const RING_TIP: usize = 16;
pub const PINKY_MCP: usize = 17;
pub const PINKY_TIP: usize = 20;

#[derive(Clone, Debug)]
pub struct Config {
    /// Output size in pixels; only the dead zone and the filter units use it.
    pub out_w: f32,
    pub out_h: f32,
    /// Camera frame width / height, so distances are isotropic.
    pub aspect: f32,
    /// Fraction of the mirrored frame, centred, that maps to the whole output.
    pub active_box: f32,
    pub min_cutoff: f32,
    pub beta: f32,
    pub dead_zone_px: f32,
    /// Pinch distance (fraction of the hand size) that presses / releases.
    pub pinch_enter: f32,
    pub pinch_exit: f32,
    /// The cursor stays put this long after a pinch starts.
    pub freeze: f64,
    /// A hand missing for this long is reported lost and releases buttons.
    pub lost_grace: f64,
    /// Scroll pixels per hand-size of vertical finger travel.
    pub scroll_px_per_hand: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            out_w: 1920.0,
            out_h: 1080.0,
            aspect: 16.0 / 9.0,
            active_box: 0.7,
            min_cutoff: 1.0,
            beta: 0.01,
            dead_zone_px: 2.0,
            pinch_enter: 0.30,
            pinch_exit: 0.45,
            freeze: 0.12,
            lost_grace: 0.25,
            scroll_px_per_hand: 700.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Point,
    Pinch { right: bool },
    Scroll,
    Fist,
}

struct Pinch {
    right: bool,
    since: f64,
    /// Index MCP (mirrored, normalised) and cursor (px) once the freeze ends.
    anchor: Option<([f32; 2], [f32; 2])>,
    open_frames: u32,
}

/// Seconds over which the resume offset (see `resume`) fades out.
const GLIDE: f32 = 0.25;
/// Longest look-back used to undo the drift while the pinch closes.
const REWIND: f64 = 0.10;
const SCROLL_HOLD: f64 = 0.10;
const FIST_ENTER: f32 = 1.05;
const FIST_STAY: f32 = 1.30;
const EXTENDED: f32 = 1.45;
const SCROLL_STAY: f32 = 1.30;
const CURLED: f32 = 1.35;

pub struct Engine {
    cfg: Config,
    fx: OneEuro,
    fy: OneEuro,
    present: bool,
    last_seen: f64,
    last_t: f64,
    mode: Mode,
    pinch: Option<Pinch>,
    scroll_since: Option<f64>,
    last_mid_y: f32,
    scroll_acc: f32,
    /// Filtered cursor in px, and the last position sent to the compositor.
    cursor: [f32; 2],
    sent: Option<[f32; 2]>,
    offset: [f32; 2],
    history: Vec<(f64, [f32; 2])>,
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        let (fx, fy) = (OneEuro::new(cfg.min_cutoff, cfg.beta), OneEuro::new(cfg.min_cutoff, cfg.beta));
        Self {
            cfg, fx, fy, present: false, last_seen: 0.0, last_t: 0.0, mode: Mode::Point, pinch: None,
            scroll_since: None, last_mid_y: 0.0, scroll_acc: 0.0, cursor: [0.0; 2], sent: None,
            offset: [0.0; 2], history: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode { self.mode }
    pub fn present(&self) -> bool { self.present }
    /// The filtered cursor in output pixels.
    pub fn cursor(&self) -> [f32; 2] { self.cursor }

    fn iso(&self, p: [f32; 2]) -> [f32; 2] { [p[0] * self.cfg.aspect, p[1]] }
    fn dist(&self, lm: &Landmarks, a: usize, b: usize) -> f32 {
        let (a, b) = (self.iso(lm[a]), self.iso(lm[b]));
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
    }

    /// Mirrored frame position to output pixels through the active box.
    fn to_px(&self, p: [f32; 2]) -> [f32; 2] {
        let b = self.cfg.active_box.clamp(0.1, 1.0);
        let u = ((1.0 - p[0]) - (0.5 - b / 2.0)) / b;
        let v = (p[1] - (0.5 - b / 2.0)) / b;
        [u.clamp(0.0, 1.0) * self.cfg.out_w, v.clamp(0.0, 1.0) * self.cfg.out_h]
    }

    /// Feed one camera frame result. `hand` is `None` when nothing is tracked.
    pub fn update(&mut self, t: f64, hand: Option<&Landmarks>, out: &mut Vec<Message>) {
        let dt = (t - self.last_t).clamp(0.0, 0.5) as f32;
        self.last_t = t;
        let Some(lm) = hand else {
            if self.present && t - self.last_seen >= self.cfg.lost_grace { self.lose(out); }
            return;
        };
        let scale = self.dist(lm, WRIST, MIDDLE_MCP);
        if scale < 1e-4 { return; }
        self.last_seen = t;
        if !self.present {
            self.present = true;
            self.fx.reset();
            self.fy.reset();
            self.offset = [0.0; 2];
            self.history.clear();
            self.mode = Mode::Point;
            self.pinch = None;
            self.scroll_since = None;
            out.push(Message::Hand { present: true });
        }
        let ext = |tip: usize, mcp: usize, e: &Self| e.dist(lm, tip, WRIST) / e.dist(lm, mcp, WRIST).max(1e-4);
        let e = [ext(INDEX_TIP, INDEX_MCP, self), ext(MIDDLE_TIP, MIDDLE_MCP, self),
            ext(RING_TIP, RING_MCP, self), ext(PINKY_TIP, PINKY_MCP, self)];
        let tip_px = self.to_px(lm[INDEX_TIP]);
        self.offset = [self.offset[0] * (-dt / GLIDE).exp(), self.offset[1] * (-dt / GLIDE).exp()];

        // A running pinch only ends by opening; nothing else can interrupt it.
        if let Some(right) = self.pinch.as_ref().map(|p| p.right) {
            let d = self.dist(lm, THUMB_TIP, if right { MIDDLE_TIP } else { INDEX_TIP }) / scale;
            let open_frames = {
                let pinch = self.pinch.as_mut().unwrap();
                if d > self.cfg.pinch_exit { pinch.open_frames += 1; } else { pinch.open_frames = 0; }
                pinch.open_frames
            };
            if open_frames >= 2 {
                self.pinch = None;
                self.mode = Mode::Point;
                out.push(Message::Button { right, pressed: false });
                self.resume(tip_px);
            } else {
                self.pinch_motion(t, lm, out);
                return;
            }
        }

        let fist_limit = if self.mode == Mode::Fist { FIST_STAY } else { FIST_ENTER };
        if e.iter().all(|&r| r < fist_limit) {
            self.mode = Mode::Fist;
            self.scroll_since = None;
            return;
        }
        if self.mode == Mode::Fist {
            self.mode = Mode::Point;
            self.resume(tip_px);
        }

        let di = self.dist(lm, THUMB_TIP, INDEX_TIP) / scale;
        let dm = self.dist(lm, THUMB_TIP, MIDDLE_TIP) / scale;
        if di.min(dm) < self.cfg.pinch_enter {
            self.start_pinch(t, dm < di, out);
            return;
        }

        // Index and middle up together, the others folded: scroll.
        let together = self.dist(lm, INDEX_TIP, MIDDLE_TIP) / scale < 0.8;
        let stay = self.mode == Mode::Scroll;
        let limit = if stay { SCROLL_STAY } else { EXTENDED };
        let pose = e[0] > limit && e[1] > limit && e[2] < CURLED && e[3] < CURLED && together;
        if !pose {
            self.scroll_since = None;
            if stay {
                self.mode = Mode::Point;
                self.resume(tip_px);
            }
        } else {
            let since = *self.scroll_since.get_or_insert(t);
            let mid_y = (lm[INDEX_TIP][1] + lm[MIDDLE_TIP][1]) / 2.0;
            if self.mode == Mode::Scroll {
                let travel = (mid_y - self.last_mid_y) / scale;
                if travel.abs() > 0.01 {
                    self.scroll_acc -= travel * self.cfg.scroll_px_per_hand;
                    let whole = self.scroll_acc.trunc();
                    if whole != 0.0 {
                        self.scroll_acc -= whole;
                        out.push(Message::Scroll { dy: whole });
                    }
                }
            } else if t - since >= SCROLL_HOLD {
                self.mode = Mode::Scroll;
                self.scroll_acc = 0.0;
            }
            self.last_mid_y = mid_y;
            return; // the cursor is frozen while the pose is forming or active
        }

        let target = [tip_px[0] + self.offset[0], tip_px[1] + self.offset[1]];
        self.emit(t, target, true, out);
    }

    fn lose(&mut self, out: &mut Vec<Message>) {
        self.release(out);
        self.present = false;
        self.mode = Mode::Point;
        self.scroll_since = None;
        out.push(Message::Hand { present: false });
    }

    /// Releases a held button, for example when the child exits.
    pub fn release(&mut self, out: &mut Vec<Message>) {
        if let Some(pinch) = self.pinch.take() {
            out.push(Message::Button { right: pinch.right, pressed: false });
        }
        if matches!(self.mode, Mode::Pinch { .. }) { self.mode = Mode::Point; }
    }

    /// Keep the cursor where it is and glide to the fingertip, instead of
    /// jumping when absolute pointing resumes.
    fn resume(&mut self, tip_px: [f32; 2]) {
        self.offset = [self.cursor[0] - tip_px[0], self.cursor[1] - tip_px[1]];
        self.fx.reset_to(self.cursor[0], self.last_t);
        self.fy.reset_to(self.cursor[1], self.last_t);
    }

    fn start_pinch(&mut self, t: f64, right: bool, out: &mut Vec<Message>) {
        // The fingers drift while they close; go back to where the finger
        // pointed a moment ago, then press.
        if let Some(&(_, p)) = self.history.iter().find(|(h, _)| *h >= t - REWIND) {
            self.cursor = p;
            self.send_motion(p, out);
        }
        self.history.clear();
        self.scroll_since = None;
        self.mode = Mode::Pinch { right };
        self.pinch = Some(Pinch { right, since: t, anchor: None, open_frames: 0 });
        out.push(Message::Button { right, pressed: true });
    }

    /// While pinching the index MCP, not the fingertip, moves the cursor.
    fn pinch_motion(&mut self, t: f64, lm: &Landmarks, out: &mut Vec<Message>) {
        let freeze = self.cfg.freeze;
        let mcp = [lm[INDEX_MCP][0], lm[INDEX_MCP][1]];
        let b = self.cfg.active_box.clamp(0.1, 1.0);
        let cursor = self.cursor;
        let pinch = self.pinch.as_mut().unwrap();
        if t - pinch.since < freeze { return; }
        let Some((anchor_mcp, anchor_pos)) = pinch.anchor else {
            pinch.anchor = Some((mcp, cursor));
            self.fx.reset_to(cursor[0], t);
            self.fy.reset_to(cursor[1], t);
            return;
        };
        let target = [
            (anchor_pos[0] - (mcp[0] - anchor_mcp[0]) / b * self.cfg.out_w).clamp(0.0, self.cfg.out_w),
            (anchor_pos[1] + (mcp[1] - anchor_mcp[1]) / b * self.cfg.out_h).clamp(0.0, self.cfg.out_h),
        ];
        self.emit(t, target, false, out);
    }

    fn emit(&mut self, t: f64, target: [f32; 2], record: bool, out: &mut Vec<Message>) {
        let p = [self.fx.filter(target[0], t), self.fy.filter(target[1], t)];
        self.cursor = p;
        if record {
            self.history.push((t, p));
            self.history.retain(|(h, _)| *h >= t - 0.3);
        }
        let moved = self.sent.is_none_or(|s| ((p[0] - s[0]).powi(2) + (p[1] - s[1]).powi(2)).sqrt() >= self.cfg.dead_zone_px);
        if moved { self.send_motion(p, out); }
    }

    fn send_motion(&mut self, p: [f32; 2], out: &mut Vec<Message>) {
        self.sent = Some(p);
        out.push(Message::Motion {
            x: (p[0] / self.cfg.out_w).clamp(0.0, 1.0),
            y: (p[1] / self.cfg.out_h).clamp(0.0, 1.0),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct Pose {
        /// Wrist position in the frame.
        wrist: [f32; 2],
        /// Tip-to-wrist over MCP-to-wrist for index, middle, ring and pinky.
        ext: [f32; 4],
        /// Thumb tip to index tip, and to middle tip, in hand sizes.
        thumb_index: f32,
        thumb_middle: f32,
    }

    const SIZE: f32 = 0.2; // wrist to middle MCP, frame heights
    const ASPECT: f32 = 16.0 / 9.0;

    fn pointing(x: f32, y: f32) -> Pose {
        Pose { wrist: [x, y], ext: [1.9, 1.0, 1.0, 1.0], thumb_index: 1.0, thumb_middle: 1.0 }
    }
    fn open(x: f32, y: f32) -> Pose { Pose { ext: [1.9; 4], ..pointing(x, y) } }
    fn fist(x: f32, y: f32) -> Pose { Pose { ext: [0.8; 4], ..pointing(x, y) } }
    fn pinch(x: f32, y: f32) -> Pose { Pose { thumb_index: 0.15, ..open(x, y) } }
    fn right_pinch(x: f32, y: f32) -> Pose { Pose { thumb_middle: 0.15, ..open(x, y) } }
    fn two_fingers(x: f32, y: f32) -> Pose { Pose { ext: [1.9, 1.9, 1.0, 1.0], ..pointing(x, y) } }

    /// An upright hand: MCPs 0.2 above the wrist side by side, tips straight up.
    fn landmarks(p: Pose) -> Landmarks {
        let mut lm = [[0.0; 2]; 21];
        let to_frame = |dx: f32, dy: f32| [p.wrist[0] + dx / ASPECT, p.wrist[1] + dy];
        lm[WRIST] = p.wrist;
        let mcps = [(INDEX_MCP, INDEX_TIP, -0.07), (MIDDLE_MCP, MIDDLE_TIP, 0.0), (RING_MCP, RING_TIP, 0.07), (PINKY_MCP, PINKY_TIP, 0.14)];
        for (i, (mcp, tip, lateral)) in mcps.into_iter().enumerate() {
            let height = if mcp == MIDDLE_MCP { SIZE } else { SIZE * 0.97 };
            lm[mcp] = to_frame(lateral, -height);
            // Tip distance from the wrist is ext * MCP distance, straight up.
            let reach = (height * height + lateral * lateral).sqrt() * p.ext[i];
            lm[tip] = to_frame(lateral, -(reach * reach - lateral * lateral).sqrt());
        }
        // The thumb tip sits next to the index or the middle tip.
        let (index, middle) = (lm[INDEX_TIP], lm[MIDDLE_TIP]);
        let near = |tip: [f32; 2], d: f32| [tip[0], tip[1] + d * SIZE];
        lm[THUMB_TIP] = if p.thumb_index < p.thumb_middle { near(index, p.thumb_index) } else { near(middle, p.thumb_middle) };
        lm
    }

    fn config() -> Config {
        Config { out_w: 1000.0, out_h: 1000.0, aspect: ASPECT, ..Config::default() }
    }

    fn run(engine: &mut Engine, t: &mut f64, pose: Pose, frames: usize) -> Vec<Message> {
        let mut out = Vec::new();
        for _ in 0..frames {
            engine.update(*t, Some(&landmarks(pose)), &mut out);
            *t += 1.0 / 30.0;
        }
        out
    }

    fn motions(out: &[Message]) -> Vec<(f32, f32)> {
        out.iter().filter_map(|m| if let Message::Motion { x, y } = m { Some((*x, *y)) } else { None }).collect()
    }

    #[test]
    fn synthetic_hand_has_the_requested_proportions() {
        let e = Engine::new(config());
        let lm = landmarks(pinch(0.5, 0.8));
        assert!((e.dist(&lm, WRIST, MIDDLE_MCP) - SIZE).abs() < 1e-4);
        assert!((e.dist(&lm, THUMB_TIP, INDEX_TIP) / SIZE - 0.15).abs() < 1e-3);
        let lm = landmarks(pointing(0.5, 0.8));
        let ratio = e.dist(&lm, INDEX_TIP, WRIST) / e.dist(&lm, INDEX_MCP, WRIST);
        assert!((ratio - 1.9).abs() < 1e-3);
    }

    #[test]
    fn pointing_is_absolute_and_mirrored() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        // Index tip is 0.2 above the wrist; frame centre in x is mirror centre.
        let out = run(&mut e, &mut t, pointing(0.5, 0.7), 30);
        assert_eq!(out[0], Message::Hand { present: true });
        let &(x, y) = motions(&out).last().unwrap();
        // The index tip sits 0.07 hand-heights left of the wrist, 1.9 reaches up.
        let tip = landmarks(pointing(0.5, 0.7))[INDEX_TIP];
        assert!((x - ((1.0 - tip[0]) - 0.15) / 0.7).abs() < 0.01, "x {x}");
        assert!((y - (tip[1] - 0.15) / 0.7).abs() < 0.01, "y {y}");
        // Moving the hand to the camera's left moves the cursor right (mirror).
        let out = run(&mut e, &mut t, pointing(0.3, 0.7), 60);
        assert!(motions(&out).last().unwrap().0 > 0.7);
    }

    #[test]
    fn active_box_edges_reach_the_output_edges() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        let out = run(&mut e, &mut t, pointing(0.99, 0.99), 60);
        let &(x, _) = motions(&out).last().unwrap();
        assert!(x < 0.001, "{x}");
        let out = run(&mut e, &mut t, pointing(0.01, 0.5), 90);
        assert!(motions(&out).last().unwrap().0 > 0.999);
    }

    #[test]
    fn dead_zone_suppresses_tiny_moves() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 30);
        // 0.5 px of hand movement on a 1000 px output stays silent.
        let out = run(&mut e, &mut t, pointing(0.5 + 0.0004, 0.7), 30);
        assert!(motions(&out).is_empty());
    }

    #[test]
    fn pinch_presses_once_and_releases_with_hysteresis() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        let out = run(&mut e, &mut t, pinch(0.5, 0.7), 10);
        let presses = out.iter().filter(|m| **m == Message::Button { right: false, pressed: true }).count();
        assert_eq!(presses, 1);
        assert_eq!(e.mode(), Mode::Pinch { right: false });
        // Between enter (0.30) and exit (0.45): still held.
        let between = Pose { thumb_index: 0.38, ..open(0.5, 0.7) };
        let out = run(&mut e, &mut t, between, 10);
        assert!(!out.iter().any(|m| matches!(m, Message::Button { .. })));
        let out = run(&mut e, &mut t, pointing(0.5, 0.7), 5);
        assert_eq!(out.iter().filter(|m| **m == Message::Button { right: false, pressed: false }).count(), 1);
        assert_eq!(e.mode(), Mode::Point);
    }

    #[test]
    fn pinching_does_not_move_the_cursor() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        let before = e.cursor();
        // The fingertip swings while the pinch closes (open hand has a longer
        // reach than pointing), but the MCP stays put.
        let out = run(&mut e, &mut t, pinch(0.5, 0.7), 30);
        for (x, y) in motions(&out) {
            assert!((x * 1000.0 - before[0]).abs() < 3.0 && (y * 1000.0 - before[1]).abs() < 3.0, "({x},{y}) vs {before:?}");
        }
    }

    #[test]
    fn dragging_follows_the_index_mcp_not_the_tip() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        run(&mut e, &mut t, pinch(0.5, 0.7), 10); // press, freeze, anchor
        let start = e.cursor();
        // Move the hand 7% of the frame to the camera's left, down 7%.
        for i in 1..=30 {
            let f = i as f32 / 30.0;
            run(&mut e, &mut t, pinch(0.5 - 0.07 * f, 0.7 + 0.07 * f), 1);
        }
        run(&mut e, &mut t, pinch(0.43, 0.77), 30);
        let last = e.cursor();
        // 7% of the frame is 10% of the 70% box: 100 px right and down.
        assert!((last[0] - start[0] - 100.0).abs() < 8.0, "{start:?} {last:?}");
        assert!((last[1] - start[1] - 100.0).abs() < 8.0, "{start:?} {last:?}");
    }

    #[test]
    fn freeze_holds_the_cursor_after_press() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        let out = run(&mut e, &mut t, pinch(0.5, 0.7), 1);
        assert!(out.contains(&Message::Button { right: false, pressed: true }));
        // 3 frames = 100 ms < 120 ms freeze, even if the hand moves a lot.
        let out = run(&mut e, &mut t, pinch(0.4, 0.7), 3);
        assert!(motions(&out).is_empty());
    }

    #[test]
    fn the_pinch_rewinds_the_drift_before_pressing() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        let steady = e.cursor();
        // Fingertip drifts away (hand moves) for two frames before the pinch is seen.
        run(&mut e, &mut t, pointing(0.5, 0.7 + 0.04), 2);
        let out = run(&mut e, &mut t, pinch(0.5, 0.7 + 0.04), 1);
        let press = out.iter().position(|m| *m == Message::Button { right: false, pressed: true }).unwrap();
        let before = motions(&out[..press]);
        let &(x, y) = before.last().expect("a corrective motion precedes the press");
        assert!((x * 1000.0 - steady[0]).abs() < 12.0 && (y * 1000.0 - steady[1]).abs() < 12.0);
    }

    #[test]
    fn middle_pinch_is_right_click() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        let out = run(&mut e, &mut t, right_pinch(0.5, 0.7), 6);
        assert!(out.contains(&Message::Button { right: true, pressed: true }));
        assert!(!out.contains(&Message::Button { right: false, pressed: true }));
        let out = run(&mut e, &mut t, pointing(0.5, 0.7), 4);
        assert!(out.contains(&Message::Button { right: true, pressed: false }));
    }

    #[test]
    fn fist_pauses_the_cursor_without_clicking() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        let out = run(&mut e, &mut t, fist(0.5, 0.7), 5);
        assert_eq!(e.mode(), Mode::Fist);
        assert!(!out.iter().any(|m| matches!(m, Message::Button { .. })));
        // Moving a fisted hand does not move the cursor.
        let out = run(&mut e, &mut t, fist(0.3, 0.6), 30);
        assert!(motions(&out).is_empty());
        // Opening the fist glides to the new place instead of jumping.
        let out = run(&mut e, &mut t, pointing(0.3, 0.6), 1);
        assert!(motions(&out).iter().all(|m| (m.0 - 0.5).abs() < 0.1), "{out:?}");
        let out = run(&mut e, &mut t, pointing(0.3, 0.6), 60);
        assert!(motions(&out).last().unwrap().0 > 0.7);
    }

    #[test]
    fn two_fingers_scroll_by_vertical_travel_and_freeze_the_cursor() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.8), 20);
        let before = e.cursor();
        let mut out = run(&mut e, &mut t, two_fingers(0.5, 0.8), 6);
        assert_eq!(e.mode(), Mode::Scroll);
        // Fingers move up by half a hand: content scrolls down (positive).
        for i in 1..=15 {
            out.extend(run(&mut e, &mut t, two_fingers(0.5, 0.8 - SIZE * 0.5 * i as f32 / 15.0), 1));
        }
        let total: f32 = out.iter().filter_map(|m| if let Message::Scroll { dy } = m { Some(*dy) } else { None }).sum();
        assert!((total - 350.0).abs() < 40.0, "scrolled {total}");
        assert_eq!(e.cursor(), before);
        let down: f32 = {
            let mut o = Vec::new();
            for i in 1..=15 {
                o.extend(run(&mut e, &mut t, two_fingers(0.5, 0.8 - SIZE * 0.5 + SIZE * 0.5 * i as f32 / 15.0), 1));
            }
            o.iter().filter_map(|m| if let Message::Scroll { dy } = m { Some(*dy) } else { None }).sum()
        };
        assert!(down < -300.0, "{down}");
    }

    #[test]
    fn a_pointing_hand_never_scrolls() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        let mut out = Vec::new();
        for i in 0..60 {
            out.extend(run(&mut e, &mut t, pointing(0.5, 0.8 - 0.002 * i as f32), 1));
        }
        assert!(!out.iter().any(|m| matches!(m, Message::Scroll { .. })));
    }

    #[test]
    fn losing_the_hand_releases_buttons_after_the_grace_period() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        run(&mut e, &mut t, pinch(0.5, 0.7), 5);
        let mut out = Vec::new();
        e.update(t, None, &mut out);
        assert!(out.is_empty(), "a single missed frame must not release");
        t += 0.3;
        e.update(t, None, &mut out);
        assert_eq!(out, [Message::Button { right: false, pressed: false }, Message::Hand { present: false }]);
        assert!(!e.present());
        // Returning snaps to the new position without a glide.
        let out = run(&mut e, &mut t, pointing(0.3, 0.7), 1);
        assert_eq!(out[0], Message::Hand { present: true });
        assert!(motions(&out)[0].0 > 0.7);
    }

    #[test]
    fn release_is_idempotent() {
        let mut e = Engine::new(config());
        let mut t = 0.0;
        run(&mut e, &mut t, pointing(0.5, 0.7), 20);
        run(&mut e, &mut t, pinch(0.5, 0.7), 5);
        let mut out = Vec::new();
        e.release(&mut out);
        e.release(&mut out);
        assert_eq!(out, [Message::Button { right: false, pressed: false }]);
    }
}
