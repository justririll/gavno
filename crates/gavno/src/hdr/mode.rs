//! Pure mode matching, using millihertz just like wl_output.
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeRequest {
    pub width: u16,
    pub height: u16,
    pub refresh: Option<i32>,
}

impl FromStr for ModeRequest {
    type Err = &'static str;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (resolution, hz) = text
            .split_once('@')
            .map_or((text, None), |(r, h)| (r, Some(h)));
        let (w, h) = resolution
            .split_once('x')
            .ok_or("expected WIDTHxHEIGHT[@HZ]")?;
        let positive = |s: &str| -> Result<u16, Self::Err> {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err("invalid resolution");
            }
            s.parse::<u16>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("invalid resolution")
        };
        let refresh = hz
            .map(|s| {
                let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
                if whole.is_empty()
                    || !whole.bytes().all(|b| b.is_ascii_digit())
                    || fraction.len() > 3
                    || !fraction.bytes().all(|b| b.is_ascii_digit())
                    || s.ends_with('.')
                {
                    return Err("invalid refresh rate");
                }
                let whole = whole.parse::<i32>().map_err(|_| "invalid refresh rate")?;
                let fraction =
                    fraction.parse::<i32>().unwrap_or(0) * 10_i32.pow(3 - fraction.len() as u32);
                whole
                    .checked_mul(1000)
                    .and_then(|w| w.checked_add(fraction))
                    .filter(|v| *v > 0)
                    .ok_or("invalid refresh rate")
            })
            .transpose()?;
        Ok(Self {
            width: positive(w)?,
            height: positive(h)?,
            refresh,
        })
    }
}

#[derive(Clone, Copy)]
pub struct Candidate {
    pub width: u16,
    pub height: u16,
    pub refresh: i32,
    pub preferred: bool,
}

/// Integer Hz requests allow the usual fractional timing (119.88/119.998).
/// A request with fractional Hz uses a 1 mHz tolerance instead.
pub fn select(modes: &[Candidate], request: Option<ModeRequest>) -> Option<usize> {
    modes
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            request.is_none_or(|r| {
                m.width == r.width
                    && m.height == r.height
                    && r.refresh.is_none_or(|hz| {
                        (i64::from(m.refresh) - i64::from(hz)).abs()
                            <= if hz % 1000 == 0 { 500 } else { 1 }
                    })
            })
        })
        .max_by_key(|(_, m)| (request.is_none() && m.preferred, m.refresh))
        .map(|(i, _)| i)
}

/// Try the configured timing first, then preferred timings and progressively
/// lower pixel rates. Every advertised timing is tried at most once.
pub fn try_modes<T>(
    modes: &[Candidate],
    request: Option<ModeRequest>,
    mut initialize: impl FnMut(usize) -> Option<T>,
) -> Option<(usize, T)> {
    let mut order: Vec<_> = (0..modes.len()).collect();
    order.sort_by_key(|&i| {
        let m = modes[i];
        std::cmp::Reverse((m.preferred, u64::from(m.width) * u64::from(m.height) * m.refresh.max(0) as u64))
    });
    if let Some(first) = select(modes, request) {
        order.retain(|&i| i != first);
        order.insert(0, first);
    }
    order.into_iter().find_map(|i| initialize(i).map(|output| (i, output)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejected_configured_and_preferred_modes_fall_back() {
        let modes = [
            Candidate { width: 2880, height: 1620, refresh: 120000, preferred: false },
            Candidate { width: 2880, height: 1620, refresh: 60000, preferred: true },
            Candidate { width: 1920, height: 1080, refresh: 60000, preferred: false },
        ];
        let request = Some("2880x1620@120".parse().unwrap());
        for working in [1, 2, 3] {
            let mut attempts = Vec::new();
            let result = try_modes(&modes, request, |i| {
                attempts.push(i);
                (i == working).then_some("initialized")
            });
            assert_eq!(attempts, (0..=working.min(2)).collect::<Vec<_>>());
            assert_eq!(result, (working < 3).then_some((working, "initialized")));
        }
        assert_eq!(try_modes::<()>(&[], None, |_| panic!("no modes")), None);
        let mut attempts = Vec::new();
        assert!(try_modes(&modes, Some("800x600".parse().unwrap()), |i| {
            attempts.push(i);
            Some(())
        }).is_some());
        assert_eq!(attempts, vec![1]);
    }
    #[test]
    fn parsing() {
        assert_eq!(
            "2880x1620@120".parse(),
            Ok(ModeRequest {
                width: 2880,
                height: 1620,
                refresh: Some(120000)
            })
        );
        assert_eq!(
            "1920x1080@59.94".parse::<ModeRequest>().unwrap().refresh,
            Some(59940)
        );
        assert_eq!("2880x1620".parse::<ModeRequest>().unwrap().refresh, None);
        for s in [
            "",
            "0x1",
            "65536x1",
            "1X2",
            "1x2@",
            "1x2@0",
            "1x2@NaN",
            "1x2@-1",
            "1x2@1.2345",
            "1x2@1.",
            "1x2@1@2",
            "1x2@2147483647",
        ] {
            assert!(s.parse::<ModeRequest>().is_err(), "{s}");
        }
    }
    #[test]
    fn selection() {
        let modes = [
            Candidate {
                width: 2880,
                height: 1620,
                refresh: 60000,
                preferred: true,
            },
            Candidate {
                width: 2880,
                height: 1620,
                refresh: 119880,
                preferred: false,
            },
            Candidate {
                width: 2880,
                height: 1620,
                refresh: 120000,
                preferred: false,
            },
        ];
        assert_eq!(select(&modes, None), Some(0));
        for s in ["2880x1620", "2880x1620@120"] {
            assert_eq!(select(&modes, Some(s.parse().unwrap())), Some(2));
        }
        assert_eq!(
            select(&modes, Some("2880x1620@119.88".parse().unwrap())),
            Some(1)
        );
        assert_eq!(select(&modes, Some("1920x1080@120".parse().unwrap())), None);
        assert_eq!(select(&[], None), None);
        let preferred = [
            Candidate {
                preferred: true,
                ..modes[1]
            },
            modes[0],
        ];
        assert_eq!(select(&preferred, None), Some(0));
    }
}
