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

/// The same layout rectangles drive rendering and pointer actions.
pub(super) fn draw_notifications(
    c: &mut super::Canvas,
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    icons: &mut crate::icons::Icons,
    theme: &str,
    panel: &super::PopoverLayout,
    history: &[crate::notify::History],
    hover: Option<&super::BarAction>,
) {
    use super::{geometry::{rect, Item, CALENDAR_H}, BarAction, FULL_COLOR, LINE};
    use crate::ui::{style::{self as look, radius, size, text, TEXT}, text::Weight};
    // The section exists when the panel is taller than the bare calendar.
    if panel.panel.size.h <= CALENDAR_H { return; }
    let x = panel.panel.loc.x;
    let y = panel.panel.loc.y + CALENDAR_H;
    look::heading(c, renderer, "Notifications", x + 20, y + 8, 200);
    if history.is_empty() || !panel.items.iter().any(|(i, _)| *i == Item::ClearNotifications) {
        let mid = x + 160;
        c.icon(renderer, icons, &["preferences-system-notifications-symbolic", "notification-symbolic"], theme, rect(mid - 12, y + 52, 24, 24));
        for (value, dy, st) in [
            ("No notifications", 88, text(size::LABEL, Weight::SemiBold, look::dim(TEXT, 0.8))),
            ("You are all caught up", 108, text(size::CAPTION, Weight::Medium, look::dim(TEXT, 0.5))),
        ] {
            let w = c.text.measure(value, st).ceil() as i32;
            c.styled_label(renderer, value, mid - w / 2, y + dy, w + 2, st);
        }
        return;
    }
    for (item, r) in &panel.items {
        match *item {
            Item::ClearNotifications => {
                let hot = hover == Some(&BarAction::ClearNotifications);
                c.rect(*r, radius::FIELD, if hot { look::SURFACE_HOVER } else { look::SURFACE }, 0.0, LINE);
                let st = text(size::CAPTION, Weight::SemiBold, TEXT);
                let w = c.text.measure("Clear", st).ceil() as i32;
                c.styled_label(renderer, "Clear", r.loc.x + (r.size.w - w) / 2, r.loc.y + 7, w + 2, st);
            }
            Item::Notification(i) => {
                let Some(h) = history.get(i) else { continue };
                look::row_bg(c, *r, hover == Some(&BarAction::Notification(h.notice.id)));
                let icon = rect(r.loc.x + 12, r.loc.y + 12, 24, 24);
                let names: Vec<String> = [h.notice.icon.clone(), h.notice.desktop.clone(), h.notice.app.to_lowercase()]
                    .into_iter().filter(|n| !n.is_empty()).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                if !c.colored_icon(renderer, icons, &names, theme, icon, Some(FULL_COLOR)) {
                    c.rect(icon, radius::CHIP + 2.0, look::SURFACE_RAISED, 0.0, LINE);
                    let initial = h.notice.app.chars().next().unwrap_or('?').to_uppercase().to_string();
                    let st = text(size::SMALL, Weight::SemiBold, TEXT);
                    let w = c.text.measure(&initial, st).ceil() as i32;
                    c.styled_label(renderer, &initial, icon.loc.x + (24 - w) / 2, icon.loc.y + 4, w + 2, st);
                }
                let time = text(size::CAPTION, Weight::Medium, look::dim(TEXT, 0.5));
                let tw = look::tabular_width(c, &h.time, time);
                look::tabular_label(c, renderer, &h.time, r.loc.x + r.size.w - 12 - tw, r.loc.y + 8, time);
                let width = r.size.w - 48 - 12 - tw - 8;
                c.styled_label(renderer, &h.notice.summary, r.loc.x + 44, r.loc.y + 6, width, text(size::LABEL, Weight::SemiBold, TEXT));
                let sub = if h.notice.body.is_empty() { h.notice.app.clone() } else { format!("{} · {}", h.notice.app, h.notice.body) };
                c.styled_label(renderer, &sub, r.loc.x + 44, r.loc.y + 25, r.size.w - 56, text(size::CAPTION, Weight::Medium, look::dim(TEXT, 0.55)));
            }
            _ => {}
        }
    }
}
