//! Gregorian calendar arithmetic, independent of the system clock and renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}
pub fn leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 => {
            if leap_year(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
pub fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let m = year * 12 + month as i32 - 1 + delta;
    (m.div_euclid(12), m.rem_euclid(12) as u32 + 1)
}
/// Monday = 0. Count days relative to 1 January 0001 (a Monday).
pub fn first_weekday(year: i32, month: u32) -> u32 {
    let y = year as i64 - 1;
    let before: u32 = (1..month).map(|m| days_in_month(year, m)).sum();
    (365 * y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400) + before as i64)
        .rem_euclid(7) as u32
}
pub fn month_grid(year: i32, month: u32) -> [Date; 42] {
    let offset = first_weekday(year, month) as i32;
    let (py, pm) = shift_month(year, month, -1);
    let (ny, nm) = shift_month(year, month, 1);
    std::array::from_fn(|i| {
        let day = i as i32 + 1 - offset;
        if day < 1 {
            Date {
                year: py,
                month: pm,
                day: (days_in_month(py, pm) as i32 + day) as u32,
            }
        } else if day > days_in_month(year, month) as i32 {
            Date {
                year: ny,
                month: nm,
                day: day as u32 - days_in_month(year, month),
            }
        } else {
            Date {
                year,
                month,
                day: day as u32,
            }
        }
    })
}
pub const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leap_years() {
        for (y, v) in [
            (1900, false),
            (2000, true),
            (2024, true),
            (2026, false),
            (2100, false),
        ] {
            assert_eq!(leap_year(y), v);
        }
    }
    #[test]
    fn february_grids() {
        let a = month_grid(2024, 2);
        assert_eq!(
            a[0],
            Date {
                year: 2024,
                month: 1,
                day: 29
            }
        );
        assert_eq!(a[3].day, 1);
        assert_eq!(a[31].day, 29);
        assert_eq!(
            a[32],
            Date {
                year: 2024,
                month: 3,
                day: 1
            }
        );
        let b = month_grid(2026, 2);
        assert_eq!(
            b[0],
            Date {
                year: 2026,
                month: 1,
                day: 26
            }
        );
        assert_eq!(b[6].day, 1);
        assert_eq!(b[33].day, 28);
        assert_eq!(
            b[34],
            Date {
                year: 2026,
                month: 3,
                day: 1
            }
        );
    }
    #[test]
    fn sunday_and_year_rollover() {
        assert_eq!(first_weekday(2026, 2), 6);
        assert_eq!(shift_month(2025, 12, 1), (2026, 1));
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
        assert_eq!(first_weekday(2024, 1), 0);
    }
}
