//! One-Euro filter (Casiez et al.): strong smoothing while the finger is
//! slow, little lag while it moves fast.

use std::f32::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct OneEuro {
    pub min_cutoff: f32,
    pub beta: f32,
    d_cutoff: f32,
    state: Option<State>,
}

#[derive(Clone, Copy, Debug)]
struct State {
    t: f64,
    x: f32,
    dx: f32,
}

fn alpha(cutoff: f32, dt: f32) -> f32 {
    let tau = 1.0 / (2.0 * PI * cutoff);
    1.0 / (1.0 + tau / dt)
}

impl OneEuro {
    pub fn new(min_cutoff: f32, beta: f32) -> Self {
        Self { min_cutoff, beta, d_cutoff: 1.0, state: None }
    }

    /// Forget the history; the next sample passes through unchanged.
    pub fn reset(&mut self) {
        self.state = None;
    }

    /// Restart the history at `x` with zero velocity.
    pub fn reset_to(&mut self, x: f32, t: f64) {
        self.state = Some(State { t, x, dx: 0.0 });
    }

    pub fn filter(&mut self, x: f32, t: f64) -> f32 {
        let Some(prev) = self.state else {
            self.state = Some(State { t, x, dx: 0.0 });
            return x;
        };
        let dt = ((t - prev.t) as f32).max(1e-3);
        let dx = (x - prev.x) / dt;
        let dx_hat = prev.dx + alpha(self.d_cutoff, dt) * (dx - prev.dx);
        let cutoff = self.min_cutoff + self.beta * dx_hat.abs();
        let x_hat = prev.x + alpha(cutoff, dt) * (x - prev.x);
        self.state = Some(State { t, x: x_hat, dx: dx_hat });
        x_hat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_passes_through() {
        let mut f = OneEuro::new(1.0, 0.0);
        assert_eq!(f.filter(42.0, 0.0), 42.0);
    }

    #[test]
    fn jitter_is_damped_when_slow() {
        let mut f = OneEuro::new(1.0, 0.0);
        let mut worst = 0.0f32;
        for i in 0..90 {
            let noise = if i % 2 == 0 { 3.0 } else { -3.0 };
            let y = f.filter(100.0 + noise, f64::from(i) / 30.0);
            if i > 30 {
                worst = worst.max((y - 100.0).abs());
            }
        }
        assert!(worst < 1.5, "jitter {worst}");
    }

    #[test]
    fn fast_motion_follows_with_beta() {
        let mut slow = OneEuro::new(1.0, 0.0);
        let mut fast = OneEuro::new(1.0, 0.05);
        let (mut a, mut b) = (0.0, 0.0);
        for i in 0..30 {
            let x = i as f32 * 40.0;
            let t = f64::from(i) / 30.0;
            a = slow.filter(x, t);
            b = fast.filter(x, t);
        }
        let target = 29.0 * 40.0;
        assert!(target - b < target - a, "beta must reduce lag: {a} vs {b}");
        assert!(target - b < 120.0);
    }

    #[test]
    fn reset_to_restarts_at_value() {
        let mut f = OneEuro::new(1.0, 0.0);
        f.filter(0.0, 0.0);
        f.reset_to(500.0, 1.0);
        assert!((f.filter(500.0, 1.033) - 500.0).abs() < 1e-3);
    }
}
