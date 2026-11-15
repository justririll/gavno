//! Critically/under-damped springs, the one animation primitive of GAVNO.
//!
//! A spring only costs frames while it moves: `is_active` turns false once
//! value and velocity settle, and the output goes back to idle.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct SpringParams {
    /// Stiffness (1/s^2). Higher is faster.
    pub k: f64,
    /// Damping ratio. 1.0 = no overshoot, < 1.0 bounces.
    pub zeta: f64,
}

impl SpringParams {
    pub const SMOOTH: Self = Self { k: 170.0, zeta: 1.0 };
    pub const SNAPPY: Self = Self { k: 400.0, zeta: 0.9 };
    pub const BOUNCY: Self = Self { k: 520.0, zeta: 0.55 };
    pub const SHAKE: Self = Self { k: 900.0, zeta: 0.18 };
}

#[derive(Debug, Clone)]
pub struct Spring {
    pub value: f64,
    pub target: f64,
    pub velocity: f64,
    pub params: SpringParams,
    last: Option<Duration>,
    follow: Option<Follow>,
}

const STEP: f64 = 1.0 / 240.0;
const EPS_VALUE: f64 = 0.0005;
const EPS_VELOCITY: f64 = 0.005;

/// Exact critically damped response, separate from the last displayed value.
/// Input can update this state without changing the scene between frames.
#[derive(Debug, Clone)]
struct Follow {
    value: f64,
    velocity: f64,
    target: f64,
    last: Duration,
}

impl Follow {
    // Mean tracking delay = 2 / omega = 5 ms, below one 120 Hz frame.
    const OMEGA: f64 = 400.0;

    fn advance(&mut self, now: Duration) {
        let dt = now.saturating_sub(self.last).as_secs_f64();
        self.last = self.last.max(now);
        let offset = self.value - self.target;
        let b = self.velocity + Self::OMEGA * offset;
        let decay = (-Self::OMEGA * dt).exp();
        let value = self.target + (offset + b * dt) * decay;
        // Reversing fingers can leave velocity pointing past the new target.
        // Clamp that response so a held gesture never overshoots the fingers.
        self.velocity = (self.velocity - Self::OMEGA * b * dt) * decay;
        self.value = value.clamp(self.value.min(self.target), self.value.max(self.target));
        if self.value != value {
            self.velocity = 0.0;
        }
    }
}

impl Spring {
    pub fn new(value: f64, params: SpringParams) -> Self {
        Self {
            value,
            target: value,
            velocity: 0.0,
            params,
            last: None,
            follow: None,
        }
    }

    /// Animate towards `target` from wherever the spring is now.
    pub fn set(&mut self, target: f64) {
        self.follow = None;
        self.target = target;
    }

    /// Update the finger target; only `advance` publishes a displayed value.
    pub fn follow(&mut self, target: f64, now: Duration) {
        let follow = self.follow.get_or_insert(Follow {
            value: self.value,
            velocity: 0.0,
            target: self.value,
            last: now,
        });
        follow.advance(now);
        follow.target = target;
        self.target = target;
    }

    /// Hand off from the last displayed frame, without evaluating a new position
    /// at input-event time. The release spring starts its clock here.
    pub fn end_follow(&mut self, velocity: f64, now: Duration) {
        self.follow = None;
        self.target = self.value;
        self.velocity = velocity;
        self.last = Some(now);
    }

    /// Jump to `value` without animation.
    pub fn jump(&mut self, value: f64) {
        self.follow = None;
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }

    /// Add an impulse (used for shakes).
    pub fn kick(&mut self, velocity: f64) {
        self.velocity += velocity;
    }

    pub fn is_active(&self) -> bool {
        // A held finger target requests vblank-paced frames until convergence,
        // just like an ordinary spring. Held-but-stationary costs no frames.
        (self.value - self.target).abs() > EPS_VALUE || self.velocity.abs() > EPS_VELOCITY
    }

    /// Publish the position at frame time `now`. Finger following is analytic;
    /// release/ordinary springs use fixed 240 Hz substeps.
    pub fn advance(&mut self, now: Duration) -> f64 {
        if let Some(follow) = &mut self.follow {
            follow.advance(now);
            self.value = follow.value;
            self.velocity = follow.velocity;
            self.last = Some(now);
            if !self.is_active() {
                self.value = self.target;
                self.velocity = 0.0;
                if let Some(follow) = &mut self.follow {
                    follow.value = self.target;
                    follow.velocity = 0.0;
                }
            }
            return self.value;
        }
        let last = self.last.replace(now).unwrap_or(now);
        if !self.is_active() {
            self.value = self.target;
            self.velocity = 0.0;
            return self.value;
        }
        // Cap the catch-up after a long pause (e.g. a VT switch).
        let mut dt = (now.saturating_sub(last)).as_secs_f64().min(0.1);
        let SpringParams { k, zeta } = self.params;
        let c = 2.0 * zeta * k.sqrt();
        while dt > 0.0 {
            let h = dt.min(STEP);
            let accel = -k * (self.value - self.target) - c * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
            dt -= h;
        }
        if !self.is_active() {
            self.value = self.target;
            self.velocity = 0.0;
        }
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_converges_without_overshoot_and_stops_requesting_frames() {
        for (start, target) in [(0.0, 1.0), (1.0, 0.0), (-0.2, 0.0)] {
            let mut s = Spring::new(start, SpringParams::SMOOTH);
            s.follow(start, Duration::ZERO);
            assert!(!s.is_active(), "resting fingers must not request frames");
            s.follow(target, Duration::ZERO);
            assert_eq!(s.value, start, "input must not change the displayed frame");
            assert!(s.is_active());
            let mut previous = start;
            for frame in 1..=12 {
                let value = s.advance(Duration::from_micros(frame * 8_333));
                assert!(value >= previous.min(target) && value <= previous.max(target));
                previous = value;
            }
            assert_eq!(s.value, target);
            assert!(
                !s.is_active(),
                "holding still must return the renderer to idle"
            );
        }
    }

    #[test]
    fn follow_is_monotonic_at_frame_time_with_irregular_input() {
        let mut s = Spring::new(0.0, SpringParams::SMOOTH);
        let events = [7_000, 18_000, 32_000, 39_000, 52_000, 61_000, 75_000];
        let mut pending = events.into_iter().peekable();
        let mut previous = s.value;
        let mut frame_without_input_moved = false;
        for frame in 0..=20 {
            let time = frame * 8_333;
            let mut had_input = false;
            while let Some(&event) = pending.peek().filter(|&&event| event <= time) {
                s.follow(event as f64 / 100_000.0, Duration::from_micros(event));
                assert_eq!(s.value, previous);
                pending.next();
                had_input = true;
            }
            let value = s.advance(Duration::from_micros(time));
            assert!(value >= previous && value <= s.target);
            frame_without_input_moved |= !had_input && value > previous;
            previous = value;
        }
        assert!(
            frame_without_input_moved,
            "motion must continue between input events"
        );
        assert_eq!(s.value, 0.75);
        assert!(!s.is_active());
    }

    #[test]
    fn follow_has_less_than_one_120hz_frame_of_filter_delay() {
        let mut s = Spring::new(0.0, SpringParams::SMOOTH);
        // Constant speed with fine samples isolates the filter's added delay.
        for millisecond in 0..=100 {
            let now = Duration::from_millis(millisecond);
            s.follow(millisecond as f64 / 1000.0, now);
            s.advance(now);
        }
        let delay = 0.1 - s.value; // Speed is one workspace per second.
        assert!(delay > 0.0 && delay < 1.0 / 120.0, "delay={delay}");
    }

    #[test]
    fn follow_is_frame_rate_independent_and_bounded_on_reversal() {
        let mut s = Spring::new(0.0, SpringParams::SMOOTH);
        s.follow(1.0, Duration::ZERO);
        let mut split = s.clone();
        split.advance(Duration::from_millis(4));
        split.advance(Duration::from_millis(8));
        s.advance(Duration::from_millis(8));
        assert!((s.value - split.value).abs() < 1e-12);
        assert!((s.velocity - split.velocity).abs() < 1e-12);
        let before = s.value;
        s.follow(0.0, Duration::from_millis(8));
        assert_eq!(s.value, before);
        for frame in 1..=12 {
            let previous = s.value;
            s.advance(Duration::from_micros(8_000 + frame * 8_333));
            assert!(s.value >= 0.0 && s.value <= previous);
        }
        assert_eq!(s.value, 0.0);
        assert!(!s.is_active());
    }

    #[test]
    fn release_starts_at_last_displayed_position_with_fitted_velocity() {
        // The workspace and vertical overview use the same hand-off, including
        // reverse motion, cancellation (zero velocity), and a pending input.
        for (target, velocity, snap) in [(0.7, 2.0, 1.0), (0.3, -2.0, 0.0), (0.7, 0.0, 0.0)] {
            let mut s = Spring::new(0.5, SpringParams::SMOOTH);
            s.follow(target, Duration::ZERO);
            s.advance(Duration::from_micros(8_333));
            let displayed = s.value;
            s.follow(target + velocity * 0.004, Duration::from_millis(12));
            let release = Duration::from_millis(15);
            s.end_follow(velocity, release);
            s.set(snap);
            assert_eq!(s.value, displayed);
            assert_eq!(s.velocity, velocity);
            assert_eq!(s.advance(release), displayed);
            for frame in 1..=400 {
                s.advance(release + Duration::from_micros(frame * 8_333));
            }
            assert_eq!(s.value, snap);
            assert!(!s.is_active());
        }
    }

    #[test]
    fn settles_on_target() {
        let mut s = Spring::new(0.0, SpringParams::SMOOTH);
        s.set(1.0);
        let mut t = Duration::ZERO;
        s.advance(t);
        for _ in 0..240 {
            t += Duration::from_millis(8);
            s.advance(t);
        }
        assert!(!s.is_active());
        assert_eq!(s.value, 1.0);
    }

    #[test]
    fn bouncy_overshoots() {
        let mut s = Spring::new(0.0, SpringParams::BOUNCY);
        s.set(1.0);
        let mut t = Duration::ZERO;
        s.advance(t);
        let mut max: f64 = 0.0;
        for _ in 0..120 {
            t += Duration::from_millis(8);
            max = max.max(s.advance(t));
        }
        assert!(max > 1.05);
    }
}
