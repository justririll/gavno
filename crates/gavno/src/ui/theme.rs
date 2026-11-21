//! The living theme: colors that follow the sun and the season.
//!
//! The sun position comes from the local time and the coordinates of the
//! configured place (`theme.latitude`/`longitude`, then `GAVNO_LAT`/`GAVNO_LON`,
//! else a guess from the time zone).
//! Nothing touches the network.

use std::f64::consts::PI;

/// Local wall-clock time, from libc (respects /etc/localtime).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct LocalTime {
    pub year: i32,
    /// 1-12
    pub month: u32,
    pub day: u32,
    /// 0 = Sunday
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 1-366
    pub yday: u32,
    /// Seconds east of UTC.
    pub utc_offset: i64,
}

impl LocalTime {
    pub fn now() -> Self {
        unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            LocalTime {
                year: tm.tm_year + 1900,
                month: tm.tm_mon as u32 + 1,
                day: tm.tm_mday as u32,
                weekday: tm.tm_wday as u32,
                hour: tm.tm_hour as u32,
                minute: tm.tm_min as u32,
                second: tm.tm_sec as u32,
                yday: tm.tm_yday as u32 + 1,
                utc_offset: tm.tm_gmtoff,
            }
        }
    }

    /// Predict solar samples without I/O. Only solar fields are consumed by
    /// `palette`; actual clock samples revalidate offset/date after DST/resume.
    pub fn after_minutes(mut self, minutes: u32) -> Self {
        let total = self.hour * 60 + self.minute + minutes;
        self.hour = total / 60 % 24;
        self.minute = total % 60;
        self.yday += total / 1440;
        loop {
            let days = if self.year % 4 == 0 && (self.year % 100 != 0 || self.year % 400 == 0) { 366 } else { 365 };
            if self.yday <= days { break; }
            self.yday -= days;
            self.year += 1;
        }
        self
    }

    pub fn weekday_name(&self) -> &'static str {
        ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"][self.weekday as usize % 7]
    }

    pub fn month_name(&self) -> &'static str {
        [
            "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
            "November", "December",
        ][(self.month as usize + 11) % 12]
    }
}

/// Coordinates of a few time zones' main cities; the fallback derives the
/// longitude from the UTC offset.
fn coords_for_zone(zone: &str) -> Option<(f64, f64)> {
    Some(match zone {
        "Europe/Moscow" => (55.75, 37.62),
        "Europe/Minsk" => (53.90, 27.56),
        "Europe/Kiev" | "Europe/Kyiv" => (50.45, 30.52),
        "Europe/Berlin" => (52.52, 13.40),
        "Europe/London" => (51.51, -0.13),
        "Europe/Paris" => (48.86, 2.35),
        "Asia/Yekaterinburg" => (56.84, 60.61),
        "Asia/Novosibirsk" => (55.03, 82.92),
        "Asia/Almaty" => (43.24, 76.89),
        "America/New_York" => (40.71, -74.01),
        "America/Los_Angeles" => (34.05, -118.24),
        _ => return None,
    })
}

pub fn location(utc_offset: i64) -> (f64, f64) {
    let env = |n: &str, limit: f64| std::env::var(n).ok().and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && (-limit..=limit).contains(v));
    let zone = std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().into_owned();
            s.split("zoneinfo/").nth(1).map(str::to_owned)
        })
        .unwrap_or_default();
    let guess = coords_for_zone(&zone).unwrap_or((50.0, (utc_offset as f64 / 3600.0 * 15.0).clamp(-180.0, 180.0)));
    (env("GAVNO_LAT", 90.0).unwrap_or(guess.0), env("GAVNO_LON", 180.0).unwrap_or(guess.1))
}

/// Sun elevation and azimuth in degrees (NOAA approximation, ~0.5° accurate).
pub fn sun_position(t: &LocalTime, lat: f64, lon: f64) -> (f64, f64) {
    let rad = PI / 180.0;
    let hours_utc = t.hour as f64 + t.minute as f64 / 60.0 + t.second as f64 / 3600.0 - t.utc_offset as f64 / 3600.0;
    let gamma = 2.0 * PI / 365.0 * (t.yday as f64 - 1.0 + (hours_utc - 12.0) / 24.0);
    let eqtime = 229.18
        * (0.000075 + 0.001868 * gamma.cos()
            - 0.032077 * gamma.sin()
            - 0.014615 * (2.0 * gamma).cos()
            - 0.040849 * (2.0 * gamma).sin());
    let decl = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin() - 0.006758 * (2.0 * gamma).cos()
        + 0.000907 * (2.0 * gamma).sin()
        - 0.002697 * (3.0 * gamma).cos()
        + 0.00148 * (3.0 * gamma).sin();
    let true_solar_min = hours_utc * 60.0 + eqtime + 4.0 * lon;
    let ha = (true_solar_min / 4.0 - 180.0) * rad;
    let lat_r = lat * rad;
    let cos_zen = lat_r.sin() * decl.sin() + lat_r.cos() * decl.cos() * ha.cos();
    let zen = cos_zen.clamp(-1.0, 1.0).acos();
    let elevation = 90.0 - zen / rad;
    let az = (ha.sin().atan2(ha.cos() * lat_r.sin() - decl.tan() * lat_r.cos()) / rad + 180.0).rem_euclid(360.0);
    (elevation, az)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub sky_top: [f32; 3],
    pub sky_bottom: [f32; 3],
    pub blob_a: [f32; 3],
    pub blob_b: [f32; 3],
    pub glow: [f32; 3],
    /// Sun glow position in the frame (0..1) and strength.
    pub sun: [f32; 3],
    /// Accent for focus rings, avatar, etc.
    pub accent: [f32; 3],
}

fn hex(c: u32) -> [f32; 3] {
    [((c >> 16) & 0xff) as f32 / 255.0, ((c >> 8) & 0xff) as f32 / 255.0, (c & 0xff) as f32 / 255.0]
}

pub(crate) fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

struct Key {
    elev: f64,
    top: u32,
    bottom: u32,
    a: u32,
    b: u32,
    glow: u32,
    glow_k: f32,
}

/// Keyframes by sun elevation: deep night -> blue hour -> golden hour -> day.
const KEYS: &[Key] = &[
    Key { elev: -18.0, top: 0x070b1a, bottom: 0x141a33, a: 0x231c4a, b: 0x0e2a3d, glow: 0x5d6ea8, glow_k: 0.10 },
    Key { elev: -8.0, top: 0x101b3f, bottom: 0x3a2f5c, a: 0x5b2f6b, b: 0x1d3d6b, glow: 0x9a7bd0, glow_k: 0.18 },
    Key { elev: -2.0, top: 0x1f3570, bottom: 0xc26b5a, a: 0x8a3f6e, b: 0x2c5a8f, glow: 0xff9a6a, glow_k: 0.45 },
    Key { elev: 6.0, top: 0x3c6fb0, bottom: 0xf2a65e, a: 0xe0785c, b: 0x6aa7d8, glow: 0xffd08a, glow_k: 0.55 },
    // Daytime stays a deep, calm blue: bright pastel skies glared behind windows.
    Key { elev: 20.0, top: 0x1e4f9c, bottom: 0x4f86c2, a: 0x3d78c8, b: 0x6fa3d6, glow: 0xffe6c4, glow_k: 0.24 },
    Key { elev: 60.0, top: 0x1a4690, bottom: 0x4677b2, a: 0x346bb8, b: 0x6797c9, glow: 0xfff1dc, glow_k: 0.20 },
];

/// Season tint: warmer in summer, cooler and paler in winter.
fn season_tint(yday: u32, lat: f64) -> ([f32; 3], f32) {
    let north = lat >= 0.0;
    // 0 = mid-winter, 1 = mid-summer.
    let phase = ((2.0 * PI * (yday as f64 - 172.0) / 365.25).cos() * 0.5 + 0.5) as f32;
    let summer = if north { phase } else { 1.0 - phase };
    let tint = mix3(hex(0x9fb8ff), hex(0xffd59a), summer);
    (tint, 0.12)
}

pub fn palette(t: &LocalTime, lat: f64, lon: f64) -> Palette {
    let (elev, az) = sun_position(t, lat, lon);
    palette_at(t, lat, elev, az)
}

fn palette_at(t: &LocalTime, lat: f64, elev: f64, az: f64) -> Palette {
    let i = KEYS.iter().position(|k| k.elev > elev).unwrap_or(KEYS.len());
    let (k0, k1, f) = if i == 0 {
        (&KEYS[0], &KEYS[0], 0.0)
    } else if i == KEYS.len() {
        (&KEYS[i - 1], &KEYS[i - 1], 0.0)
    } else {
        let (a, b) = (&KEYS[i - 1], &KEYS[i]);
        (a, b, ((elev - a.elev) / (b.elev - a.elev)) as f32)
    };
    let f = f * f * (3.0 - 2.0 * f);
    let (tint, amount) = season_tint(t.yday, lat);
    let tinted = |c: [f32; 3]| mix3(c, [c[0] * tint[0], c[1] * tint[1], c[2] * tint[2]], amount);

    // Project the sun's horizontal component rather than azimuth alone: this
    // stays continuous at midnight and through zenith near the equator. Deep
    // night rests at a fixed moon position; dawn brings it back smoothly.
    let daylight = ((elev + 18.0) / 10.0).clamp(0.0, 1.0) as f32;
    let daylight = daylight * daylight * (3.0 - 2.0 * daylight);
    let horizontal = (az * PI / 180.0).sin() * (elev * PI / 180.0).cos();
    let x = 0.5 - 0.45 * horizontal as f32 * daylight;
    let y = (0.85 - (elev.max(-10.0) / 70.0) * 0.7).clamp(0.1, 0.95) as f32;
    let strength = k0.glow_k + (k1.glow_k - k0.glow_k) * f;

    Palette {
        sky_top: tinted(mix3(hex(k0.top), hex(k1.top), f)),
        sky_bottom: tinted(mix3(hex(k0.bottom), hex(k1.bottom), f)),
        blob_a: tinted(mix3(hex(k0.a), hex(k1.a), f)),
        blob_b: tinted(mix3(hex(k0.b), hex(k1.b), f)),
        glow: mix3(hex(k0.glow), hex(k1.glow), f),
        sun: [x, y, strength],
        accent: mix3(hex(0x7aa2ff), hex(0xffb36b), ((elev + 4.0) / 14.0).clamp(0.0, 1.0) as f32 * 0.6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn time(yday: u32, minute: u32, utc_offset: i64) -> LocalTime {
        LocalTime { year: 2026, month: 6, day: 21, weekday: 0, hour: minute / 60, minute: minute % 60, second: 0, yday, utc_offset }
    }

    #[test]
    fn oklab_reference_colors_and_calendar_rollover() {
        let red = oklab([1.0,0.0,0.0]);
        for (actual, expected) in red.into_iter().zip([0.62795536,0.22486306,0.12584630]) { assert!((actual-expected).abs() < 1e-6); }
        let white = oklab([1.0;3]);
        assert!((white[0]-1.0).abs() < 1e-6);
        assert!(white[1].abs() < 1e-6 && white[2].abs() < 1e-6);
        let mut last = time(365, 1439, 10800);
        let next = last.after_minutes(1);
        assert_eq!((next.year,next.yday,next.hour,next.minute), (2027,1,0,0));
        last.year = 2024; last.yday = 366;
        assert_eq!(last.after_minutes(1).year, 2025);
    }

    #[test]
    fn keyframes_are_continuous() {
        let t = time(172, 720, 0);
        for k in KEYS {
            let before = palette_at(&t, 50.0, k.elev - 0.00001, 180.0);
            let after = palette_at(&t, 50.0, k.elev + 0.00001, 180.0);
            assert!(before.delta(after) < 0.00001, "keyframe {}", k.elev);
        }
    }

    #[test]
    fn day_sweeps_have_bounded_minute_changes_in_both_hemispheres() {
        for (lat, lon, offset) in [(55.75, 37.62, 10800), (-33.87, 151.21, 39600), (0.0, 0.0, 0), (70.0, 15.0, 3600)] {
            for day in [1, 80, 172, 266, 355] {
                let mut previous = palette(&time(day, 0, offset), lat, lon);
                for minute in 1..=1440 {
                    let next = palette(&time(day, minute, offset), lat, lon);
                    assert!(previous.delta(next) < 0.04, "lat={lat}, day={day}, minute={minute}, delta={}", previous.delta(next));
                    previous = next;
                }
            }
        }
    }

    #[test]
    fn southern_seasons_are_reversed() {
        let north_summer = season_tint(172, 40.0).0;
        let south_summer = season_tint(355, -40.0).0;
        let south_winter = season_tint(172, -40.0).0;
        assert!((north_summer[0] - south_summer[0]).abs() < 0.001);
        assert!(south_summer[0] > south_winter[0]);
        assert!(south_summer[2] < south_winter[2]);
    }

    #[test]
    fn moscow_noon_in_june_is_high() {
        let t = LocalTime {
            year: 2026,
            month: 6,
            day: 21,
            weekday: 0,
            hour: 12,
            minute: 30,
            second: 0,
            yday: 172,
            utc_offset: 3 * 3600,
        };
        let (elev, _) = sun_position(&t, 55.75, 37.62);
        assert!((elev - 57.6).abs() < 2.0, "elev {elev}");
    }

    #[test]
    fn moscow_november_evening_is_dark() {
        let t = LocalTime {
            year: 2026,
            month: 11,
            day: 15,
            weekday: 0,
            hour: 18,
            minute: 0,
            second: 0,
            yday: 319,
            utc_offset: 3 * 3600,
        };
        let (elev, _) = sun_position(&t, 55.75, 37.62);
        assert!(elev < -5.0, "elev {elev}");
    }
}

impl Palette {
    pub fn mix(self, next: Self, t: f32) -> Self {
        Self { sky_top: mix3(self.sky_top, next.sky_top, t), sky_bottom: mix3(self.sky_bottom, next.sky_bottom, t),
            blob_a: mix3(self.blob_a, next.blob_a, t), blob_b: mix3(self.blob_b, next.blob_b, t),
            glow: mix3(self.glow, next.glow, t), sun: mix3(self.sun, next.sun, t), accent: mix3(self.accent, next.accent, t) }
    }
    /// Conservative normalized RGB/glow displacement bound, accumulated against
    /// the last accepted palette rather than the preceding minute's sample.
    pub fn delta(self, next: Self) -> f32 {
        let colors = [self.sky_top, self.sky_bottom, self.blob_a, self.blob_b, self.glow, self.accent];
        let other = [next.sky_top, next.sky_bottom, next.blob_a, next.blob_b, next.glow, next.accent];
        let color = colors.into_iter().zip(other).flat_map(|(a, b)| a.into_iter().zip(b).map(|(a, b)| (a-b).abs())).fold(0.0, f32::max);
        let glow = (self.sun[0] - next.sun[0]).abs().max((self.sun[1] - next.sun[1]).abs())
            * self.sun[2].max(next.sun[2]) * 1.8;
        color.max(glow).max((self.sun[2] - next.sun[2]).abs())
    }
}

/// D65 sRGB -> OKLab, using the public-domain reference matrices:
/// https://bottosson.github.io/posts/oklab/#converting-from-linear-srgb-to-oklab
pub(crate) fn oklab(rgb: [f32; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(|c| {
        let c = f64::from(c);
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    });
    let l = (0.4122214708*r + 0.5363325363*g + 0.0514459929*b).cbrt();
    let m = (0.2119034982*r + 0.6806995451*g + 0.1073969566*b).cbrt();
    let s = (0.0883024619*r + 0.2817188376*g + 0.6299787005*b).cbrt();
    [0.2104542553*l + 0.7936177850*m - 0.0040720468*s,
     1.9779984951*l - 2.4285922050*m + 0.4505937099*s,
     0.0259040371*l + 0.7827717662*m - 0.8086757660*s]
}
fn delta_e(a: [f32; 3], b: [f32; 3]) -> f64 {
    oklab(a).into_iter().zip(oklab(b)).map(|(a,b)| (a-b).powi(2)).sum::<f64>().sqrt()
}
impl Palette {
    pub fn solid(color: [f32; 4], accent: [f32; 4]) -> Self {
        let c = [color[0], color[1], color[2]];
        Self { sky_top: c, sky_bottom: c, blob_a: c, blob_b: c, glow: [0.0; 3], sun: [0.5, 0.95, 0.0], accent: [accent[0], accent[1], accent[2]] }
    }
    /// Maximum perceptual distance of sky keyframe colors and accent. Sample
    /// the shared shader's fields as well so moving glow cannot evade the budget.
    pub fn perceptual_delta(self, other: Self) -> f64 {
        let a = [self.sky_top, self.sky_bottom, self.blob_a, self.blob_b, self.glow, self.accent];
        let b = [other.sky_top, other.sky_bottom, other.blob_a, other.blob_b, other.glow, other.accent];
        let mut delta = a.into_iter().zip(b).map(|(a,b)| delta_e(a,b)).fold(0.0, f64::max);
        for x in [0.15, 0.5, 0.85] {
            for y in [0.15, 0.5, 0.85] {
                delta = delta.max(delta_e(self.sample(x,y), other.sample(x,y)));
            }
        }
        delta
    }
    fn sample(self, x: f32, y: f32) -> [f32; 3] {
        let aspect = 16.0 / 9.0;
        let field = |cx: f32, cy: f32, k: f32| (-k*((x-cx).powi(2)*aspect*aspect + (y-cy).powi(2))).exp();
        let color = mix3(self.sky_top, self.sky_bottom, y*y*(3.0-2.0*y));
        let color = mix3(color, self.blob_a, 0.55*field(0.18,0.95,3.2));
        let color = mix3(color, self.blob_b, 0.45*field(0.92,0.15,4.0));
        std::array::from_fn(|i| color[i] + self.glow[i]*self.sun[2]*field(self.sun[0],self.sun[1],9.0))
    }
}
