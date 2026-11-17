//! Evening white-point shift. The warm gain is folded into the sRGB to BT.2020
//! matrix of the single HDR PQ pass, so it costs no extra GPU work, and it
//! changes only in visible steps scheduled on the theme's shared one-shot clock.
use std::{cell::Cell, time::Duration};

use crate::{
    state::Gavno,
    topbar::wall_time,
    ui::theme::{sun_position, LocalTime},
};

pub const NEUTRAL_K: u32 = 6500;
pub const MIN_K: u32 = 1900;
pub const DEFAULT_K: u32 = 3400;
/// Smallest visible matrix step.
const STEP_K: u32 = 50;
/// Full strength at or below this sun elevation, none at or above `DAY_ELEVATION`.
const NIGHT_ELEVATION: f64 = -6.0;
const DAY_ELEVATION: f64 = 3.0;
const LOOKAHEAD_MINUTES: u32 = 24 * 60;

thread_local! {
    /// Temperature every HDR composition on the compositor thread should apply.
    static APPLIED_K: Cell<u32> = const { Cell::new(NEUTRAL_K) };
}
pub fn applied() -> u32 {
    APPLIED_K.with(Cell::get)
}
fn set_applied(kelvin: u32) {
    APPLIED_K.with(|k| k.set(kelvin));
}
#[cfg(test)]
pub fn set_applied_for_test(kelvin: u32) {
    set_applied(kelvin);
}

/// 0 in daylight, 1 once the sun is well below the horizon, smooth between.
pub fn strength(elevation: f64) -> f64 {
    let t = ((DAY_ELEVATION - elevation) / (DAY_ELEVATION - NIGHT_ELEVATION)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Effective temperature, on a 50 K grid so tiny sun movements change nothing.
pub fn kelvin_for(strength: f64, target: u32) -> u32 {
    let target = target.clamp(MIN_K, NEUTRAL_K);
    if strength >= 1.0 {
        return target;
    }
    if strength <= 0.0 {
        return NEUTRAL_K;
    }
    let raw = f64::from(NEUTRAL_K) + (f64::from(target) - f64::from(NEUTRAL_K)) * strength;
    let steps = ((f64::from(NEUTRAL_K) - raw) / f64::from(STEP_K)).round() as u32;
    (NEUTRAL_K - steps * STEP_K).max(target)
}

pub fn kelvin_at(time: &LocalTime, lat: f64, lon: f64, target: u32, preview: bool) -> u32 {
    if preview {
        return target.clamp(MIN_K, NEUTRAL_K);
    }
    kelvin_for(strength(sun_position(time, lat, lon).0), target)
}

/// Time until the temperature next differs from `current`. Coarse scan over a
/// day, then minute refinement: flat nights and middays sleep for a full day.
pub fn next_change(time: &LocalTime, lat: f64, lon: f64, target: u32, current: u32) -> Duration {
    let at = |m: u32| kelvin_at(&time.after_minutes(m), lat, lon, target, false);
    let mut previous = 0;
    for m in (5..=LOOKAHEAD_MINUTES).step_by(5) {
        if at(m) != current {
            let minutes = (previous + 1..=m).find(|m| at(*m) != current).unwrap_or(m);
            return Duration::from_secs(u64::from(minutes) * 60);
        }
        previous = m;
    }
    Duration::from_secs(u64::from(LOOKAHEAD_MINUTES) * 60)
}

/// Linear BT.2020 white of a blackbody (Kim et al. Planckian locus fit),
/// relative to the 6500 K one and scaled so the largest channel is 1.
pub fn gain(kelvin: u32) -> [f64; 3] {
    if kelvin >= NEUTRAL_K {
        return [1.0; 3];
    }
    let white = blackbody_2020(f64::from(kelvin.max(MIN_K)));
    let reference = blackbody_2020(f64::from(NEUTRAL_K));
    let relative: [f64; 3] = std::array::from_fn(|i| white[i] / reference[i]);
    let peak = relative.iter().copied().fold(f64::MIN, f64::max);
    relative.map(|v| v / peak)
}

fn blackbody_2020(t: f64) -> [f64; 3] {
    let x = if t < 4000.0 {
        -0.2661239e9 / t.powi(3) - 0.2343589e6 / t.powi(2) + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t.powi(3) + 2.1070379e6 / t.powi(2) + 0.2226347e3 / t + 0.240390
    };
    let y = if t < 2222.0 {
        -1.1063814 * x.powi(3) - 1.34811020 * x.powi(2) + 2.18555832 * x - 0.20219683
    } else if t < 4000.0 {
        -0.9549476 * x.powi(3) - 1.37418593 * x.powi(2) + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x.powi(3) - 5.87338670 * x.powi(2) + 3.75112997 * x - 0.37001483
    };
    let (cx, cy, cz) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        1.7166512 * cx - 0.3556708 * cy - 0.2533663 * cz,
        -0.6666844 * cx + 1.6164812 * cy + 0.0157685 * cz,
        0.0176399 * cx - 0.0427706 * cy + 0.9421031 * cz,
    ]
}

/// The sRGB to BT.2020 matrix with the white-point gain applied to its output rows.
pub fn matrix(kelvin: u32) -> [[f64; 3]; 3] {
    let g = gain(kelvin);
    std::array::from_fn(|r| std::array::from_fn(|c| crate::color::BT709_TO_BT2020[r][c] * g[r]))
}

struct Plan {
    key: (bool, u32, bool, bool),
    due: Option<Duration>,
    at: Duration,
    wall: Duration,
}

#[derive(Default)]
pub struct NightLight {
    pub preview: bool,
    plan: Option<Plan>,
    logged_sdr: bool,
}

impl Plan {
    fn current(&self, key: (bool, u32, bool, bool), now: Duration, wall: Duration) -> bool {
        self.key == key
            && self.due.is_none_or(|due| now < due)
            && wall.saturating_sub(self.wall).abs_diff(now.saturating_sub(self.at)) <= Duration::from_secs(5)
    }
}

impl Gavno {
    /// Re-evaluate the white point and return the next monotonic deadline, if any.
    /// Cheap when nothing is due: one comparison per call.
    pub(crate) fn sync_night_light(&mut self, now: Duration) -> Option<Duration> {
        // Nothing is shown while outputs are off; resume re-evaluates.
        if self.power.is_off() {
            return None;
        }
        let display = &self.config.display;
        let (enabled, target) = (display.night_light, display.night_light_temperature);
        let preview = self.night_light.preview;
        // Without an HDR output nothing can change on screen: keep the clock idle.
        let hdr = crate::backend::udev::any_hdr(self);
        let key = (enabled, target, preview, hdr);
        let wall = wall_time();
        if let Some(plan) = self.night_light.plan.as_ref().filter(|p| p.current(key, now, wall)) {
            return plan.due;
        }
        let time = LocalTime::now();
        let (lat, lon) = self.config.theme.location(time.utc_offset);
        let kelvin = if enabled { kelvin_at(&time, lat, lon, target, preview) } else { NEUTRAL_K };
        let due = (enabled && !preview && hdr).then(|| now + next_change(&time, lat, lon, target, kelvin));
        self.night_light.plan = Some(Plan { key, due, at: now, wall });
        if enabled && !hdr && !self.night_light.logged_sdr {
            self.night_light.logged_sdr = true;
            tracing::info!("night light needs HDR output; SDR outputs are not adjusted");
        }
        if kelvin != applied() {
            set_applied(kelvin);
            tracing::debug!(kelvin, "night light white point changed");
            if hdr {
                self.queue_redraw_all();
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(hour: u32, minute: u32) -> LocalTime {
        LocalTime {
            year: 2026, month: 11, day: 17, weekday: 2, hour, minute, second: 0,
            yday: 321, utc_offset: 10800,
        }
    }
    const MOSCOW: (f64, f64) = (55.75, 37.62);

    #[test]
    fn neutral_matrix_is_the_existing_one() {
        assert_eq!(matrix(NEUTRAL_K), crate::color::BT709_TO_BT2020);
        assert_eq!(matrix(9000), crate::color::BT709_TO_BT2020);
    }

    #[test]
    fn warmer_temperatures_cut_blue_then_green_and_keep_red_at_one() {
        let mut previous = gain(NEUTRAL_K);
        for k in (MIN_K..NEUTRAL_K).rev().step_by(50) {
            let g = gain(k);
            assert_eq!(g.iter().copied().fold(0.0, f64::max), 1.0, "{k}");
            assert!(g.iter().all(|v| *v > 0.0), "{k}");
            assert!(g[1] <= previous[1] + 1e-12 && g[2] <= previous[2] + 1e-12, "{k}");
            previous = g;
        }
        let warm = gain(3400);
        assert_eq!(warm[0], 1.0);
        assert!(warm[2] < warm[1] && warm[1] < warm[0], "{warm:?}");
        let warmer = gain(2000);
        assert!(warmer[2] < warm[2] && warmer[1] < warm[1]);
    }

    #[test]
    fn strength_at_fixed_elevations() {
        assert_eq!(strength(-6.0), 1.0);
        assert_eq!(strength(-30.0), 1.0);
        assert_eq!(strength(3.0), 0.0);
        assert_eq!(strength(40.0), 0.0);
        assert!((strength(-1.5) - 0.5).abs() < 1e-12);
        assert!(strength(-3.0) > strength(0.0));
        assert_eq!(kelvin_for(0.0, 3400), 6500);
        assert_eq!(kelvin_for(1.0, 3400), 3400);
        assert_eq!(kelvin_for(0.5, 3400), 4950);
        assert_eq!(kelvin_for(0.999, 3410), 3410);
        assert_eq!(kelvin_for(1.0, 100), MIN_K);
        assert_eq!(kelvin_for(1.0, 9000), NEUTRAL_K);
    }

    #[test]
    fn no_change_is_scheduled_at_noon_or_midnight_within_hours() {
        let (lat, lon) = MOSCOW;
        let noon = time(12, 0);
        let k = kelvin_at(&noon, lat, lon, 3400, false);
        assert_eq!(k, NEUTRAL_K);
        // The next change is the evening: many hours away, not a periodic wake.
        assert!(next_change(&noon, lat, lon, 3400, k) > Duration::from_secs(3 * 3600));
        let midnight = time(0, 0);
        let k = kelvin_at(&midnight, lat, lon, 3400, false);
        assert_eq!(k, 3400);
        assert!(next_change(&midnight, lat, lon, 3400, k) > Duration::from_secs(3 * 3600));
    }

    #[test]
    fn evening_transition_takes_few_steps() {
        let (lat, lon) = MOSCOW;
        let mut now = time(12, 0);
        let mut minutes = 0;
        let mut kelvin = kelvin_at(&now, lat, lon, 3400, false);
        let mut changes = Vec::new();
        while minutes < 24 * 60 {
            let wait = next_change(&now, lat, lon, 3400, kelvin).as_secs() as u32 / 60;
            minutes += wait;
            now = time(12, 0).after_minutes(minutes);
            let next = kelvin_at(&now, lat, lon, 3400, false);
            assert_ne!(next, kelvin);
            changes.push(next);
            kelvin = next;
        }
        // One evening ramp down and one morning ramp up in a day.
        let evening = changes.iter().take_while(|k| **k != 3400).count() + 1;
        eprintln!("night light matrix changes: evening {evening}, per day {}", changes.len());
        assert!(evening <= 63, "{evening}");
        assert!(changes.len() <= 2 * 63, "{}", changes.len());
        assert!(changes.windows(2).all(|w| w[0].abs_diff(w[1]) >= STEP_K || w[0] == 3400 || w[1] == 3400 || w[0] == NEUTRAL_K || w[1] == NEUTRAL_K));
    }

    #[test]
    fn preview_forces_full_strength() {
        assert_eq!(kelvin_at(&time(12, 0), 55.75, 37.62, 3400, true), 3400);
    }

    /// The matrix really reaches the PQ shader: neutral white stays balanced, and a
    /// warm white keeps red while cutting green and blue more.
    #[test]
    fn pq_pass_applies_the_white_point() {
        use smithay::{
            backend::{
                allocator::Fourcc,
                egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
                renderer::{
                    damage::OutputDamageTracker,
                    element::{
                        solid::{SolidColorBuffer, SolidColorRenderElement},
                        Kind,
                    },
                    gles::{GlesRenderer, GlesTexture},
                    Bind, ExportMem, Offscreen,
                },
            },
            output::{Mode, Output, PhysicalProperties, Subpixel},
            utils::{Rectangle, Transform},
        };
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let mut r = unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap();
        let properties = PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        };
        let o = Output::new("night-pixels".into(), properties);
        let mode = Mode { size: (16, 8).into(), refresh: 60000 };
        o.change_current_state(Some(mode), None, None, None);
        let white = SolidColorBuffer::new((16, 8), [1.0; 4]);
        let mut pixel = |kelvin| {
            set_applied(kelvin);
            let mut hdr = crate::hdr::compose::Composition::new(&mut r, &o, 203.0).unwrap();
            let element = crate::render::GavnoRenderElement::Solid(
                SolidColorRenderElement::from_buffer(&white, (0, 0), 1.0, 1.0, Kind::Unspecified),
            );
            let (pq, _) = hdr.render(&mut r, &o, &[element], crate::render::CLEAR_COLOR).unwrap();
            let mut target: GlesTexture = r.create_buffer(Fourcc::Abgr8888, (16, 8).into()).unwrap();
            let mut tracker = OutputDamageTracker::new((16, 8), 1.0, Transform::Normal);
            let mapping = {
                let mut fb = r.bind(&mut target).unwrap();
                tracker.render_output(&mut r, &mut fb, 0, &[pq], [0.0, 0.0, 0.0, 1.0]).unwrap();
                r.copy_framebuffer(&fb, Rectangle::from_size((16, 8).into()), Fourcc::Abgr8888)
                    .unwrap()
            };
            let bytes = r.map_texture(&mapping).unwrap();
            [bytes[0], bytes[1], bytes[2]].map(i32::from)
        };
        let neutral = pixel(NEUTRAL_K);
        let warm = pixel(DEFAULT_K);
        set_applied(NEUTRAL_K);
        // SDR white at 203 nits is PQ 0.58, about 148 of 255.
        assert!(neutral.iter().all(|v| (v - 148).abs() <= 2), "{neutral:?}");
        assert!((warm[0] - neutral[0]).abs() <= 1, "{warm:?}");
        assert!(warm[1] < neutral[1] && warm[2] < warm[1], "{warm:?}");
    }
}
