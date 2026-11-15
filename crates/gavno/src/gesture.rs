//! Gesture arbitration and pure motion math. Resting fingers never request frames.
use crate::Gavno;
use smithay::{
    backend::input::{
        Event, GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent as _,
        GestureSwipeUpdateEvent as _, InputBackend,
    },
    input::pointer::*,
    output::Output,
    utils::{Logical, Point, SERIAL_COUNTER},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}
pub fn lock_axis(x: f64, y: f64) -> Option<Axis> {
    (x.hypot(y) >= 12.0).then_some(if x.abs() > y.abs() {
        Axis::Horizontal
    } else {
        Axis::Vertical
    })
}
pub fn progress(start: f64, delta: f64, distance: f64) -> f64 {
    start - delta / distance.max(1.0)
}
pub fn rubber_band(value: f64, max: f64) -> f64 {
    let rubber = |d: f64| 0.35 * d / (1.0 + d.abs());
    if value < 0.0 {
        rubber(value)
    } else if value > max {
        max + rubber(value - max)
    } else {
        value
    }
}
pub fn release_target(position: f64, velocity: f64, max: usize) -> usize {
    (position + velocity * 0.18).round().clamp(0.0, max as f64) as usize
}
#[derive(Default, Debug)]
pub struct SuperTap {
    down: Option<u32>,
    cancelled: bool,
}
impl SuperTap {
    /// Return true exactly once on an unaccompanied Super release.
    pub fn key(&mut self, code: u32, is_super: bool, pressed: bool, other_down: bool) -> bool {
        if pressed {
            if is_super && self.down.is_none() {
                self.down = Some(code);
                self.cancelled = other_down;
            } else {
                self.cancel();
            }
            false
        } else if self.down == Some(code) {
            self.down = None;
            let toggle = !self.cancelled;
            self.cancelled = false;
            toggle
        } else {
            false
        }
    }
    pub fn cancel(&mut self) {
        if self.down.is_some() {
            self.cancelled = true;
        }
    }
}
struct Swipe {
    output: Output,
    axis: Option<Axis>,
    delta: Point<f64, Logical>,
    workspace_start: f64,
    overview_start: f64,
    workspace_origin: usize,
    overview_origin: bool,
    velocity: Point<f64, Logical>,
    last: u32,
    last_value: Point<f64, Logical>,
}
pub struct Gestures {
    swipe: Option<Swipe>,
    swipe_client: bool,
    pinch_client: bool,
    hold_client: bool,
    hold_output: Option<Output>,
    pub super_tap: SuperTap,
    /// Logical input units per workspace. Read once, never on the frame path.
    distance: f64,
    direction: f64,
}
impl Default for Gestures {
    fn default() -> Self {
        Self {
            swipe: None,
            swipe_client: false,
            pinch_client: false,
            hold_client: false,
            hold_output: None,
            super_tap: Default::default(),
            distance: std::env::var("GAVNO_GESTURE_DISTANCE")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 24.0)
                .unwrap_or(300.0),
            direction: if std::env::var("GAVNO_NATURAL_SCROLL").as_deref() == Ok("0") {
                -1.0
            } else {
                1.0
            },
        }
    }
}
impl Gavno {
    fn gestures_enabled(&self) -> bool {
        self.greeter.is_none() && !self.protocols.locks.locked
    }
    fn client_gesture(&self, fingers: u32) -> bool {
        fingers == 2
            && !self
                .seat
                .get_keyboard()
                .is_some_and(|k| k.modifier_state().logo)
    }
    fn catch_desktop(&mut self, output: &Output) {
        let now = self.start_time.elapsed();
        let o = self.workspaces.ensure(output);
        o.position.advance(now);
        o.position.jump(o.position.value);
        o.held = true;
        if self.overview.output.as_ref() == Some(output) {
            self.overview.progress.advance(now);
            self.overview.progress.jump(self.overview.progress.value);
            self.overview.held = true;
        }
    }
    pub fn on_gesture_swipe_begin<I: InputBackend>(&mut self, event: I::GestureSwipeBeginEvent) {
        if !self.gestures_enabled() {
            return;
        }
        self.gestures.super_tap.cancel();
        self.gestures.swipe_client = self.client_gesture(event.fingers());
        if self.gestures.swipe_client {
            self.seat.get_pointer().unwrap().gesture_swipe_begin(
                self,
                &GestureSwipeBeginEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                    fingers: event.fingers(),
                },
            );
            return;
        }
        if event.fingers() != 3
            && !(event.fingers() == 2 && self.seat.get_keyboard().unwrap().modifier_state().logo)
        {
            return;
        }
        let Some(output) = self.active_output() else {
            return;
        };
        self.workspaces.snapshot(&self.space);
        let o = self.workspaces.ensure(&output);
        let workspace_origin = o.list.active;
        let overview_origin = self.overview.open();
        self.catch_desktop(&output);
        let workspace_start = self.workspaces.ensure(&output).position.value;
        let overview_start = if self.overview.output.as_ref() == Some(&output) {
            self.overview.progress.value
        } else {
            0.0
        };
        self.gestures.swipe = Some(Swipe {
            output,
            axis: None,
            delta: (0.0, 0.0).into(),
            workspace_start,
            overview_start,
            workspace_origin,
            overview_origin,
            velocity: (0.0, 0.0).into(),
            last: event.time_msec(),
            last_value: (0.0, 0.0).into(),
        });
    }
    pub fn on_gesture_swipe_update<I: InputBackend>(&mut self, event: I::GestureSwipeUpdateEvent) {
        if !self.gestures_enabled() {
            return;
        }
        if self.gestures.swipe_client {
            self.seat.get_pointer().unwrap().gesture_swipe_update(
                self,
                &GestureSwipeUpdateEvent {
                    time: event.time_msec(),
                    delta: event.delta(),
                },
            );
            return;
        }
        let Some(s) = &mut self.gestures.swipe else {
            return;
        };
        s.delta += event.delta().upscale(self.gestures.direction);
        if s.axis.is_none() {
            s.axis = lock_axis(s.delta.x, s.delta.y);
        }
        let dt = event.time_msec().wrapping_sub(s.last) as f64 / 1000.0;
        s.last = event.time_msec();
        if dt > 0.0 {
            s.velocity = event.delta().upscale(self.gestures.direction / dt);
        }
        let Some(axis) = s.axis else {
            return;
        };
        let o = self.workspaces.ensure(&s.output);
        let value = match axis {
            Axis::Horizontal => {
                let value = rubber_band(
                    progress(s.workspace_start, s.delta.x, self.gestures.distance),
                    (o.list.items.len() - 1) as f64,
                );
                o.position.jump(value);
                value
            }
            Axis::Vertical => {
                let value = progress(s.overview_start, s.delta.y, 340.0).clamp(0.0, 1.0);
                self.overview.output = Some(s.output.clone());
                self.overview.held = true;
                self.overview.progress.jump(value);
                value
            }
        };
        let old = match axis {
            Axis::Horizontal => s.last_value.x,
            Axis::Vertical => s.last_value.y,
        };
        if axis == Axis::Horizontal {
            s.last_value.x = value;
        } else {
            s.last_value.y = value;
        }
        if value != old {
            self.clear_desktop_pointer();
            self.queue_redraw_all();
        }
    }
    pub fn on_gesture_swipe_end<I: InputBackend>(&mut self, event: I::GestureSwipeEndEvent) {
        if self.gestures.swipe_client {
            self.gestures.swipe_client = false;
            self.seat.get_pointer().unwrap().gesture_swipe_end(
                self,
                &GestureSwipeEndEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                    cancelled: event.cancelled(),
                },
            );
            return;
        }
        let Some(s) = self.gestures.swipe.take() else {
            return;
        };
        let stale = event.time_msec().wrapping_sub(s.last) > 100;
        let velocity = if stale || event.cancelled() {
            Point::from((0.0, 0.0))
        } else {
            s.velocity
        };
        let o = self.workspaces.ensure(&s.output);
        o.position.advance(self.start_time.elapsed());
        self.overview.progress.advance(self.start_time.elapsed());
        o.held = false;
        self.overview.held = false;
        match s.axis {
            Some(Axis::Horizontal) => {
                let target = if event.cancelled() {
                    s.workspace_origin
                } else {
                    release_target(
                        o.position.value,
                        -velocity.x / self.gestures.distance,
                        o.list.items.len() - 1,
                    )
                };
                self.switch_workspace(&s.output, target);
                self.workspaces.ensure(&s.output).position.velocity =
                    -velocity.x / self.gestures.distance;
                self.overview
                    .progress
                    .set(if s.overview_origin { 1.0 } else { 0.0 });
            }
            Some(Axis::Vertical) => {
                let v = -velocity.y / 340.0;
                let target = if event.cancelled() {
                    usize::from(s.overview_origin)
                } else {
                    release_target(self.overview.progress.value, v, 1)
                };
                self.overview.progress.set(target as f64);
                self.overview.progress.velocity = v;
                o.position.set(o.list.active as f64);
            }
            None => {
                o.position.set(o.list.active as f64);
                self.overview
                    .progress
                    .set(if s.overview_origin { 1.0 } else { 0.0 });
            }
        }
        if self.is_animating() {
            self.queue_redraw_all();
        }
    }
    pub fn on_gesture_pinch_begin<I: InputBackend>(&mut self, event: I::GesturePinchBeginEvent) {
        if !self.gestures_enabled() {
            return;
        }
        self.gestures.super_tap.cancel();
        self.gestures.pinch_client = self.client_gesture(event.fingers());
        if self.gestures.pinch_client {
            self.seat.get_pointer().unwrap().gesture_pinch_begin(
                self,
                &GesturePinchBeginEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                    fingers: event.fingers(),
                },
            );
        }
    }
    pub fn on_gesture_pinch_update<I: InputBackend>(&mut self, event: I::GesturePinchUpdateEvent) {
        if self.gestures.pinch_client {
            self.seat.get_pointer().unwrap().gesture_pinch_update(
                self,
                &GesturePinchUpdateEvent {
                    time: event.time_msec(),
                    delta: event.delta(),
                    scale: event.scale(),
                    rotation: event.rotation(),
                },
            );
        }
    }
    pub fn on_gesture_pinch_end<I: InputBackend>(&mut self, event: I::GesturePinchEndEvent) {
        if self.gestures.pinch_client {
            self.gestures.pinch_client = false;
            self.seat.get_pointer().unwrap().gesture_pinch_end(
                self,
                &GesturePinchEndEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                    cancelled: event.cancelled(),
                },
            );
        }
    }
    pub fn on_gesture_hold_begin<I: InputBackend>(&mut self, event: I::GestureHoldBeginEvent) {
        if !self.gestures_enabled() {
            return;
        }
        self.gestures.super_tap.cancel();
        self.gestures.hold_client = self.client_gesture(event.fingers());
        if self.gestures.hold_client {
            self.seat.get_pointer().unwrap().gesture_hold_begin(
                self,
                &GestureHoldBeginEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                    fingers: event.fingers(),
                },
            );
        } else if event.fingers() == 3 {
            if let Some(output) = self.active_output() {
                let moving = self.workspaces.ensure(&output).position.is_active()
                    || (self.overview.output.as_ref() == Some(&output)
                        && self.overview.progress.is_active());
                if moving {
                    self.catch_desktop(&output);
                    self.gestures.hold_output = Some(output);
                }
            }
        }
    }
    pub fn on_gesture_hold_end<I: InputBackend>(&mut self, event: I::GestureHoldEndEvent) {
        if self.gestures.hold_client {
            self.gestures.hold_client = false;
            self.seat.get_pointer().unwrap().gesture_hold_end(
                self,
                &GestureHoldEndEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                    cancelled: event.cancelled(),
                },
            );
        } else if let Some(output) = self.gestures.hold_output.take() {
            let o = self.workspaces.ensure(&output);
            let target = release_target(o.position.value, 0.0, o.list.items.len() - 1);
            o.held = false;
            self.switch_workspace(&output, target);
            if self.overview.output.as_ref() == Some(&output) {
                self.overview.held = false;
                self.overview.progress.advance(self.start_time.elapsed());
                self.overview
                    .progress
                    .set(release_target(self.overview.progress.value, 0.0, 1) as f64);
            }
            self.queue_redraw_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn axis_locks_after_threshold_and_uses_dominant_direction() {
        assert_eq!(lock_axis(4.0, 5.0), None);
        assert_eq!(lock_axis(-13.0, 3.0), Some(Axis::Horizontal));
        assert_eq!(lock_axis(2.0, -12.0), Some(Axis::Vertical));
    }
    #[test]
    fn mapping_is_one_to_one() {
        assert_eq!(progress(1.0, -150.0, 300.0), 1.5);
        assert_eq!(progress(0.0, -170.0, 340.0), 0.5);
    }
    #[test]
    fn rubber_is_continuous_bounded_and_monotonic() {
        assert_eq!(rubber_band(1.0, 2.0), 1.0);
        assert_eq!(rubber_band(0.0, 2.0), 0.0);
        assert!(rubber_band(-100.0, 2.0) > -0.35);
        assert!(rubber_band(100.0, 2.0) < 2.35);
        for i in -100..400 {
            let x = i as f64 / 100.0;
            assert!(rubber_band(x, 2.0) < rubber_band(x + 0.01, 2.0));
        }
    }
    #[test]
    fn release_projects_velocity_and_clamps_ends() {
        assert_eq!(release_target(0.4, 0.0, 3), 0);
        assert_eq!(release_target(0.4, 2.0, 3), 1);
        assert_eq!(release_target(0.2, -20.0, 3), 0);
        assert_eq!(release_target(2.8, 20.0, 3), 3);
    }
    #[test]
    fn super_tap_requires_unaccompanied_press_and_release() {
        let mut t = SuperTap::default();
        assert!(!t.key(1, true, true, false));
        assert!(t.key(1, true, false, false));
        assert!(!t.key(1, true, false, false));
        t.key(1, true, true, false);
        t.key(2, false, true, false);
        assert!(!t.key(1, true, false, false));
        t.key(1, true, true, false);
        t.cancel();
        assert!(!t.key(1, true, false, false));
        t.key(1, true, true, true);
        assert!(!t.key(1, true, false, false));
        t.key(1, true, true, false);
        t.key(3, true, true, true);
        assert!(!t.key(1, true, false, false));
    }
}
