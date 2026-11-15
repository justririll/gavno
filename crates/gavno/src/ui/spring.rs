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
}

const STEP: f64 = 1.0 / 240.0;
const EPS_VALUE: f64 = 0.0005;
const EPS_VELOCITY: f64 = 0.005;

impl Spring {
    pub fn new(value: f64, params: SpringParams) -> Self {
        Self {
            value,
            target: value,
            velocity: 0.0,
            params,
            last: None,
        }
    }

    /// Animate towards `target` from wherever the spring is now.
    pub fn set(&mut self, target: f64) {
        self.target = target;
    }

    /// Jump to `value` without animation.
    pub fn jump(&mut self, value: f64) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }

    /// Add an impulse (used for shakes).
    pub fn kick(&mut self, velocity: f64) {
        self.velocity += velocity;
    }

    pub fn is_active(&self) -> bool {
        (self.value - self.target).abs() > EPS_VALUE || self.velocity.abs() > EPS_VELOCITY
    }

    /// Advance to monotonic time `now` with fixed 240 Hz substeps, so the
    /// motion is identical at 60 and 120 Hz.
    pub fn advance(&mut self, now: Duration) -> f64 {
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
