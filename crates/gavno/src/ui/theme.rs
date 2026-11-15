//! The living theme: colors that follow the sun and the season.
//!
//! The sun position comes from the local time and the coordinates of the
//! configured place (`GAVNO_LAT`/`GAVNO_LON`, else a guess from the time zone).
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
    let env = |n: &str| std::env::var(n).ok().and_then(|v| v.parse::<f64>().ok());
    if let (Some(lat), Some(lon)) = (env("GAVNO_LAT"), env("GAVNO_LON")) {
        return (lat, lon);
    }
    let zone = std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy().into_owned();
            s.split("zoneinfo/").nth(1).map(str::to_owned)
        })
        .unwrap_or_default();
    coords_for_zone(&zone).unwrap_or((50.0, utc_offset as f64 / 3600.0 * 15.0))
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
    let az = {
        let s = zen.sin().max(1e-6);
        let cos_az = ((lat_r.sin() * zen.cos()) - decl.sin()) / (lat_r.cos() * s);
        let a = cos_az.clamp(-1.0, 1.0).acos() / rad;
        if ha > 0.0 { (a + 180.0) % 360.0 } else { (540.0 - a) % 360.0 }
    };
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

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
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
    Key { elev: 20.0, top: 0x2f6fd6, bottom: 0x9cc9f5, a: 0x76b3ff, b: 0xb9e3ff, glow: 0xfff3d6, glow_k: 0.35 },
    Key { elev: 60.0, top: 0x2563c9, bottom: 0x8fc2f2, a: 0x5ea2f5, b: 0xc8ecff, glow: 0xffffff, glow_k: 0.30 },
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

    // The glow sits where the sun (or, at night, the anti-sun "moon") is:
    // azimuth 90..270 maps across the screen, elevation to height.
    let x = ((az - 90.0) / 180.0).clamp(0.05, 0.95) as f32;
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
