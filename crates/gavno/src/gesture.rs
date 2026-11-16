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
use std::{any::Any, collections::VecDeque};

// Unaccelerated libinput motion is normalized to 1000 dpi. Around 400 units
// per workspace is the GNOME-style comfortable touchpad swipe; keep the env
// override for device/user tuning. Overview keeps the mockup's shorter travel.
const WORKSPACE_SWIPE_DISTANCE: f64 = 400.0;
const OVERVIEW_SWIPE_DISTANCE: f64 = 340.0;

/// Least-squares fit of normalized positions over the last 100 ms.
/// Use event microseconds, independent of the compositor's frame clock.
#[derive(Default)]
pub(crate) struct MotionHistory(VecDeque<(u64, Point<f64, Logical>)>);

impl MotionHistory {
    const WINDOW: u64 = 100_000;

    pub(crate) fn push(&mut self, time: u64, value: Point<f64, Logical>) {
        if let Some(&(last, _)) = self.0.back() {
            if time < last {
                return;
            }
            if time == last {
                self.0.pop_back();
            }
        }
        self.0.push_back((time, value));
        while self.0.front().is_some_and(|(t, _)| time - t > Self::WINDOW) || self.0.len() > 128 {
            self.0.pop_front();
        }
    }

    pub(crate) fn velocity(&self, now: u64) -> Point<f64, Logical> {
        let samples: Vec<_> = self
            .0
            .iter()
            .filter(|(t, _)| *t <= now && now - t <= Self::WINDOW)
            .collect();
        let mut velocity = Point::from((0.0, 0.0));
        let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
            return velocity;
        };
        if last.0 - first.0 < 4_000 {
            return velocity;
        }
        let n = samples.len() as f64;
        let mean_t = samples
            .iter()
            .map(|(t, _)| (now - t) as f64 / 1e6)
            .sum::<f64>()
            / n;
        let mean_x = samples.iter().map(|(_, p)| p.x).sum::<f64>() / n;
        let mean_y = samples.iter().map(|(_, p)| p.y).sum::<f64>() / n;
        let mut variance = 0.0;
        for (t, p) in samples {
            let dt = mean_t - (now - t) as f64 / 1e6;
            variance += dt * dt;
            velocity.x += dt * (p.x - mean_x);
            velocity.y += dt * (p.y - mean_y);
        }
        velocity.upscale(1.0 / variance)
    }
}

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
    history: MotionHistory,
    last_value: Point<f64, Logical>,
}
pub struct Gestures {
    swipe: Option<Swipe>,
    swipe_client: bool,
    pinch_client: bool,
    hold_client: bool,
    hold_output: Option<Output>,
    pub super_tap: SuperTap,
    /// Unaccelerated input units per workspace. Read once, never on the frame path.
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
                .unwrap_or(WORKSPACE_SWIPE_DISTANCE),
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
        self.features.read(crate::module::Feature::Gestures, Default::default(), || self.gestures_enabled_feature())
    }

    fn gestures_enabled_feature(&self) -> bool {
        self.greeter.is_none() && !self.protocols.locks.locked
    }
    fn client_gesture(&self, fingers: u32) -> bool {
        self.features.read(crate::module::Feature::Gestures, Default::default(), || self.client_gesture_feature(fingers))
    }

    fn client_gesture_feature(&self, fingers: u32) -> bool {
        fingers == 2
            && !self
                .seat
                .get_keyboard()
                .is_some_and(|k| k.modifier_state().logo)
    }
    fn catch_desktop(&mut self, output: &Output) {
        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| gv.catch_desktop_feature(output))
    }

    fn catch_desktop_feature(&mut self, output: &Output) {
        let now = self.start_time.elapsed();
        let o = self.workspaces.ensure(output);
        o.position.advance(now);
        o.position.jump(o.position.value);
        o.held = true;
        if self.overview.output.as_ref() == Some(output) {
            self.features.call(crate::module::Feature::Overview, Default::default(), || self.overview.progress.advance(now));
            self.features.call(crate::module::Feature::Overview, Default::default(), || self.overview.progress.jump(self.overview.progress.value));
            self.features.call(crate::module::Feature::Overview, (), || { self.overview.held = true; });
        }
    }
    pub fn on_gesture_swipe_begin<I: InputBackend>(&mut self, event: I::GestureSwipeBeginEvent) {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_swipe_begin(self, &GestureSwipeBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if !gv.gestures_enabled() {
                return;
            }
            gv.gestures.super_tap.cancel();
            gv.gestures.swipe_client = gv.client_gesture(event.fingers()) || (event.fingers()==4 && (!gv.features.on(crate::module::Feature::WindowGestures) || gv.window_gesture_locked()));
            if gv.gestures.swipe_client {
                gv.seat.get_pointer().unwrap().gesture_swipe_begin(
                    gv,
                    &GestureSwipeBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        fingers: event.fingers(),
                    },
                );
                return;
            }
            if event.fingers()==4 {gv.begin_window_drag(event.time());return;}
            if event.fingers() != 3
                && !(event.fingers() == 2 && gv.seat.get_keyboard().unwrap().modifier_state().logo)
            {
                return;
            }
            let Some(output) = gv.active_output() else {
                return;
            };
            gv.workspaces.snapshot(&gv.space);
            let o = gv.workspaces.ensure(&output);
            let workspace_origin = o.list.active;
            let overview_origin = gv.features.read(crate::module::Feature::Overview, false, || gv.overview.open());
            gv.catch_desktop(&output);
            let workspace_start = gv.workspaces.ensure(&output).position.value;
            let overview_start = if gv.overview.output.as_ref() == Some(&output) {
                gv.overview.progress.value
            } else {
                0.0
            };
            let mut history = MotionHistory::default();
            history.push(event.time(), (workspace_start, overview_start).into());
            gv.gestures.swipe = Some(Swipe {
                output,
                axis: None,
                delta: (0.0, 0.0).into(),
                workspace_start,
                overview_start,
                workspace_origin,
                overview_origin,
                history,
                last_value: (workspace_start, overview_start).into(),
            });
        })
    }
    pub fn on_gesture_swipe_update<I: InputBackend>(&mut self, event: I::GestureSwipeUpdateEvent)
    where
        I::GestureSwipeUpdateEvent: 'static,
    {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_swipe_update(self, &GestureSwipeUpdateEvent { time: event.time_msec(), delta: event.delta() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if !gv.gestures_enabled() {
                return;
            }
            if gv.gestures.swipe_client {
                gv.seat.get_pointer().unwrap().gesture_swipe_update(
                    gv,
                    &GestureSwipeUpdateEvent {
                        time: event.time_msec(),
                        delta: event.delta(),
                    },
                );
                return;
            }
            use smithay::reexports::input::event::gesture::GestureEventCoordinates;
            let delta = (&event as &dyn Any)
                .downcast_ref::<smithay::reexports::input::event::gesture::GestureSwipeUpdateEvent>()
                .map(|e| Point::from((e.dx_unaccelerated(), e.dy_unaccelerated())))
                .unwrap_or_else(|| event.delta());
            if !delta.x.is_finite() || !delta.y.is_finite() {
                return;
            }
            if gv.update_window_drag(delta.upscale(gv.gestures.direction),event.time()) {return;}
            let Some(s) = &mut gv.gestures.swipe else {return;};
            s.delta += delta.upscale(gv.gestures.direction);
            if s.axis.is_none() {
                s.axis = lock_axis(s.delta.x, s.delta.y);
            }
            let o = gv.workspaces.ensure(&s.output);
            let position = Point::from((
                rubber_band(
                    progress(s.workspace_start, s.delta.x, gv.gestures.distance),
                    (o.list.items.len() - 1) as f64,
                ),
                progress(s.overview_start, s.delta.y, OVERVIEW_SWIPE_DISTANCE).clamp(0.0, 1.0),
            ));
            s.history.push(event.time(), position);
            let Some(axis) = s.axis else {
                return;
            };
            let now = gv.start_time.elapsed();
            let value = match axis {
                Axis::Horizontal => {
                    let value = position.x;
                    o.position.follow(value, now);
                    value
                }
                Axis::Vertical => {
                    if !gv.features.on(crate::module::Feature::Overview) { return; }
                    let value = position.y;
                    gv.features.call(crate::module::Feature::Overview, (), || { gv.overview.output = Some(s.output.clone()); });
                    gv.features.call(crate::module::Feature::Overview, (), || { gv.overview.held = true; });
                    gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview.progress.follow(value, now));
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
                gv.clear_desktop_pointer();
                gv.queue_redraw_all();
            }
        })
    }
    pub fn on_gesture_swipe_end<I: InputBackend>(&mut self, event: I::GestureSwipeEndEvent) {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_swipe_end(self, &GestureSwipeEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if gv.gestures.swipe_client {
                gv.gestures.swipe_client = false;
                gv.seat.get_pointer().unwrap().gesture_swipe_end(
                    gv,
                    &GestureSwipeEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        cancelled: event.cancelled(),
                    },
                );
                return;
            }
            if gv.end_window_drag(event.time(),event.cancelled()) {return;}
            let Some(mut s) = gv.gestures.swipe.take() else {
                return;
            };
            // Include the stationary interval between the final update and release.
            s.history.push(event.time(), s.last_value);
            let velocity = if event.cancelled() {
                Point::from((0.0, 0.0))
            } else {
                s.history.velocity(event.time())
            };
            let o = gv.workspaces.ensure(&s.output);
            let now = gv.start_time.elapsed();
            let workspace_target = o.position.target;
            let overview_target = gv.overview.progress.target;
            o.position.end_follow(0.0, now);
            gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview.progress.end_follow(0.0, now));
            o.held = false;
            gv.features.call(crate::module::Feature::Overview, (), || { gv.overview.held = false; });
            match s.axis {
                Some(Axis::Horizontal) => {
                    let target = if event.cancelled() {
                        s.workspace_origin
                    } else {
                        release_target(workspace_target, velocity.x, o.list.items.len() - 1)
                    };
                    gv.switch_workspace(&s.output, target);
                    gv.workspaces.ensure(&s.output).position.velocity = velocity.x;
                    gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview
                        .progress
                        .set(if s.overview_origin { 1.0 } else { 0.0 }));
                }
                Some(Axis::Vertical) => {
                    let v = velocity.y;
                    let target = if event.cancelled() {
                        usize::from(s.overview_origin)
                    } else {
                        release_target(overview_target, v, 1)
                    };
                    gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview.progress.set(target as f64));
                    gv.overview.progress.velocity = v;
                    o.position.set(o.list.active as f64);
                }
                None => {
                    o.position.set(o.list.active as f64);
                    gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview
                        .progress
                        .set(if s.overview_origin { 1.0 } else { 0.0 }));
                }
            }
            if gv.is_animating() {
                gv.queue_redraw_all();
            }
        })
    }
    pub fn on_gesture_pinch_begin<I: InputBackend>(&mut self, event: I::GesturePinchBeginEvent) {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_pinch_begin(self, &GesturePinchBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if !gv.gestures_enabled() {
                return;
            }
            gv.gestures.super_tap.cancel();
            gv.gestures.pinch_client = gv.client_gesture(event.fingers()) || (event.fingers()==4 && (!gv.features.on(crate::module::Feature::WindowGestures) || gv.window_gesture_locked()));
            if !gv.gestures.pinch_client && (event.fingers()==4 || event.fingers()==2) {
                gv.begin_window_pinch(event.fingers()==2);
            }
            if gv.gestures.pinch_client {
                gv.seat.get_pointer().unwrap().gesture_pinch_begin(
                    gv,
                    &GesturePinchBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        fingers: event.fingers(),
                    },
                );
            }
        })
    }
    pub fn on_gesture_pinch_update<I: InputBackend>(&mut self, event: I::GesturePinchUpdateEvent) {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_pinch_update(self, &GesturePinchUpdateEvent { time: event.time_msec(), delta: event.delta(), scale: event.scale(), rotation: event.rotation() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if !gv.gestures_enabled() {return;}
            if !gv.gestures.pinch_client {gv.update_window_pinch(event.scale());}
            if gv.gestures.pinch_client {
                gv.seat.get_pointer().unwrap().gesture_pinch_update(
                    gv,
                    &GesturePinchUpdateEvent {
                        time: event.time_msec(),
                        delta: event.delta(),
                        scale: event.scale(),
                        rotation: event.rotation(),
                    },
                );
            }
        })
    }
    pub fn on_gesture_pinch_end<I: InputBackend>(&mut self, event: I::GesturePinchEndEvent) {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_pinch_end(self, &GesturePinchEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if !gv.gestures.pinch_client {gv.end_window_pinch(event.cancelled());}
            if gv.gestures.pinch_client {
                gv.gestures.pinch_client = false;
                gv.seat.get_pointer().unwrap().gesture_pinch_end(
                    gv,
                    &GesturePinchEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        cancelled: event.cancelled(),
                    },
                );
            }
        })
    }
    pub fn on_gesture_hold_begin<I: InputBackend>(&mut self, event: I::GestureHoldBeginEvent) {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_hold_begin(self, &GestureHoldBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if !gv.gestures_enabled() {
                return;
            }
            gv.gestures.super_tap.cancel();
            gv.gestures.hold_client = gv.client_gesture(event.fingers());
            if gv.gestures.hold_client {
                gv.seat.get_pointer().unwrap().gesture_hold_begin(
                    gv,
                    &GestureHoldBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        fingers: event.fingers(),
                    },
                );
            } else if event.fingers() == 3 {
                if let Some(output) = gv.active_output() {
                    let moving = gv.workspaces.ensure(&output).position.is_active()
                        || (gv.overview.output.as_ref() == Some(&output)
                            && gv.overview.progress.is_active());
                    if moving {
                        gv.catch_desktop(&output);
                        gv.gestures.hold_output = Some(output);
                    }
                }
            }
        })
    }
    pub fn on_gesture_hold_end<I: InputBackend>(&mut self, event: I::GestureHoldEndEvent) {
        if !self.features.on(crate::module::Feature::Gestures) {
            if self.greeter.is_none() && !self.protocols.locks.locked {
                self.seat.get_pointer().unwrap().gesture_hold_end(self, &GestureHoldEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() });
            }
            return;
        }

        self.guard(crate::module::Feature::Gestures, Default::default(), |gv| {
            if gv.gestures.hold_client {
                gv.gestures.hold_client = false;
                gv.seat.get_pointer().unwrap().gesture_hold_end(
                    gv,
                    &GestureHoldEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                        cancelled: event.cancelled(),
                    },
                );
            } else if let Some(output) = gv.gestures.hold_output.take() {
                let o = gv.workspaces.ensure(&output);
                let target = release_target(o.position.value, 0.0, o.list.items.len() - 1);
                o.held = false;
                gv.switch_workspace(&output, target);
                if gv.overview.output.as_ref() == Some(&output) {
                    gv.features.call(crate::module::Feature::Overview, (), || { gv.overview.held = false; });
                    gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview.progress.advance(gv.start_time.elapsed()));
                    gv.features.call(crate::module::Feature::Overview, Default::default(), || gv.overview
                        .progress
                        .set(release_target(gv.overview.progress.value, 0.0, 1) as f64));
                }
                gv.queue_redraw_all();
            }
        })
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
        assert_eq!(progress(1.0, -200.0, WORKSPACE_SWIPE_DISTANCE), 1.5);
        assert_eq!(progress(0.0, -170.0, OVERVIEW_SWIPE_DISTANCE), 0.5);
    }
    #[test]
    fn velocity_fit_recovers_constant_speed_at_irregular_timestamps() {
        let mut history = MotionHistory::default();
        // A large base also checks that the fit does not lose clock precision.
        let base = 9_000_000_000_000;
        for t in [0, 7_000, 21_000, 29_000, 41_000, 56_000, 70_000, 84_000] {
            let seconds = t as f64 / 1e6;
            history.push(base + t, (0.2 + 2.5 * seconds, 0.8 - 1.5 * seconds).into());
        }
        let velocity = history.velocity(base + 84_000);
        assert!((velocity.x - 2.5).abs() < 1e-10);
        assert!((velocity.y + 1.5).abs() < 1e-10);
    }

    #[test]
    fn velocity_fit_rejects_single_event_noise() {
        let mut history = MotionHistory::default();
        for i in 0..=10 {
            let noise = if i % 2 == 0 { 0.007 } else { -0.007 };
            let position = 0.1 + 2.0 * i as f64 * 0.008 + noise;
            history.push(i * 8_000, (position, -position).into());
        }
        let velocity = history.velocity(80_000);
        // Alternating sample noise changes the final single-event speed by 1.75
        // workspaces/s, while the fit recovers the underlying 2 workspaces/s.
        assert!((velocity.x - 2.0).abs() < 0.05);
        assert!((velocity.y + 2.0).abs() < 0.05);
    }

    #[test]
    fn velocity_fit_ignores_stale_samples_and_stationary_release() {
        let mut history = MotionHistory::default();
        history.push(0, (-100.0, 100.0).into());
        history.push(110_000, (0.0, 0.0).into());
        history.push(130_000, (0.04, -0.02).into());
        let velocity = history.velocity(130_000);
        assert!((velocity.x - 2.0).abs() < 1e-10);
        assert!((velocity.y + 1.0).abs() < 1e-10);
        assert_eq!(history.velocity(231_000), (0.0, 0.0).into());
        history.push(240_000, (0.04, -0.02).into());
        assert_eq!(history.velocity(240_000), (0.0, 0.0).into());
        history.push(250_000, (0.04, -0.02).into());
        assert_eq!(history.velocity(250_000), (0.0, 0.0).into());
    }

    #[test]
    fn velocity_fit_handles_duplicate_and_out_of_order_timestamps() {
        let mut history = MotionHistory::default();
        history.push(10_000, (0.0, 0.0).into());
        history.push(10_000, (0.1, 0.0).into());
        history.push(9_000, (999.0, 0.0).into());
        assert_eq!(history.velocity(10_000), (0.0, 0.0).into());
        history.push(20_000, (0.12, 0.0).into());
        assert!((history.velocity(20_000).x - 2.0).abs() < 1e-10);
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

impl Gavno {
    pub(crate) fn reset_gestures(&mut self) {
        let streams = [self.gestures.swipe_client, self.gestures.pinch_client, self.gestures.hold_client];
        self.loop_handle.insert_idle(move |gv| {
            if let Some(pointer) = gv.seat.get_pointer() {
                let serial = SERIAL_COUNTER.next_serial();
                let time = gv.clock.now().as_millis() as u32;
                if streams[0] { pointer.gesture_swipe_end(gv, &GestureSwipeEndEvent { serial, time, cancelled: true }); }
                if streams[1] { pointer.gesture_pinch_end(gv, &GesturePinchEndEvent { serial, time, cancelled: true }); }
                if streams[2] { pointer.gesture_hold_end(gv, &GestureHoldEndEvent { serial, time, cancelled: true }); }
            }
        });
        for w in self.space.elements() {
            crate::grabs::resize_grab::clear_anchor(w);
            if let Some(t) = w.toplevel() {
                t.with_pending_state(|s| s.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing));
                t.send_pending_configure();
            }
        }
        self.gestures = Default::default();
        self.window_gestures = Default::default();
        self.features.call(crate::module::Feature::Overview, (), || { self.overview.drag = None; });
        self.features.call(crate::module::Feature::Overview, (), || { self.overview.flick = None; });
        self.workspaces.moving_tile = None;
        for owner in &mut self.workspaces.outputs {
            owner.held = false;
            owner.position.jump(owner.list.active as f64);
        }
        self.overlay.reset_switcher();
        self.cancel_feature_grab();
    }

    pub(crate) fn cancel_feature_grab(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        // Recovery may run inside a PointerInnerHandle callback. Defer seat
        // access to idle there, since Smithay holds the pointer mutex.
        self.loop_handle.insert_idle(|gv| {
            if let Some(pointer) = gv.seat.get_pointer() {
                pointer.unset_grab(gv, smithay::utils::SERIAL_COUNTER.next_serial(), gv.clock.now().as_millis() as u32);
                for button in std::mem::take(&mut gv.pointer_buttons) {
                    pointer.button(gv, &smithay::input::pointer::ButtonEvent {
                        button, state: smithay::backend::input::ButtonState::Released,
                        serial: smithay::utils::SERIAL_COUNTER.next_serial(), time: gv.clock.now().as_millis() as u32,
                    });
                }
                pointer.frame(gv);
            }
        });
        drop(pointer);
        self.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
    }
}

impl Gavno {
    pub(crate) fn super_tap_key(&mut self, code: u32, is_super: bool, pressed: bool, other_down: bool) -> bool {
        self.guard(crate::module::Feature::Gestures, false, |gv| gv.gestures.super_tap.key(code, is_super, pressed, other_down))
    }
}
