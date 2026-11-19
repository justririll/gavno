//! Shared drawing and input geometry. All dimensions are logical pixels.
use smithay::utils::{Logical, Rectangle};
pub type Rect = Rectangle<i32, Logical>;
pub fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rectangle::new((x, y).into(), (w.max(0), h.max(0)).into())
}
/// One slot of the status cluster at the right end of the bar. Adding an
/// indicator means adding a variant, one entry in
/// `Layout::with_battery` and one arm where the slot is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusItem {
    Layout,
    Wifi,
    Volume,
    Bluetooth,
    Battery,
}
/// Padding inside the cluster capsule and gap between its slots (8 px grid).
pub const CLUSTER_PAD: i32 = 8;
pub const CLUSTER_GAP: i32 = 8;
pub const CLUSTER_ICON: i32 = 16;
/// Width of the battery slot: fill icon, gap and a three digit percentage.
pub const BATTERY_SLOT: i32 = 60;
/// Width of the layout label slot ("EN", "RU").
pub const LAYOUT_SLOT: i32 = 24;
/// Width of the tiling toggle capsule.
pub const TOGGLE_W: i32 = 32;
/// Narrowest title text the bar keeps; code mode hints give way before it.
pub const MIN_TITLE_TEXT: i32 = 120;

/// What one workspace looks like in the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PillSpec {
    pub windows: usize,
    pub active: bool,
}
/// One placed workspace. `rect` is the drawn pill; `hit` is the pointer target
/// (full bar height, gaps shared with the neighbours).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pill {
    pub index: usize,
    pub rect: Rect,
    pub hit: Rect,
    /// App icons drawn (0 for dots).
    pub icons: usize,
    /// Windows beyond the drawn icons, shown as "+N".
    pub extra: usize,
    /// Drawn as a dot or accent bar instead of a pill.
    pub dot: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceStrip {
    /// The capsule behind all pills.
    pub rect: Rect,
    pub pills: Vec<Pill>,
}
pub const PILL_H: i32 = 20;
pub const PILL_ICON: i32 = 14;
pub const PILL_ICON_GAP: i32 = 3;
pub const PILL_PAD: i32 = 6;
pub const PILL_GAP: i32 = 4;
const STRIP_PAD: i32 = 4;
const DOT_SLOT: i32 = 14;
const ACTIVE_DOT_SLOT: i32 = 26;
/// Width of the "+N" label for `extra` hidden windows.
pub fn extra_width(extra: usize) -> i32 {
    8 + 7 * extra.to_string().len() as i32
}
fn pill_size(spec: PillSpec, max_icons: usize) -> (i32, usize, usize, bool) {
    if spec.windows == 0 || max_icons == 0 {
        // An occupied workspace in the most compact level is still a dot.
        return (if spec.active { ACTIVE_DOT_SLOT } else { DOT_SLOT }, 0, 0, true);
    }
    let icons = spec.windows.min(max_icons);
    let extra = spec.windows - icons;
    let mut w = 2 * PILL_PAD + icons as i32 * PILL_ICON + (icons as i32 - 1) * PILL_ICON_GAP;
    if extra > 0 {
        w += PILL_ICON_GAP + extra_width(extra);
    }
    (w, icons, extra, false)
}
fn strip_width(specs: &[PillSpec], max_icons: usize) -> i32 {
    2 * STRIP_PAD
        + specs.iter().map(|s| pill_size(*s, max_icons).0).sum::<i32>()
        + PILL_GAP * specs.len().saturating_sub(1) as i32
}
/// Places the workspace pills starting at `x`, using at most `max_width`.
/// Too many workspaces first lose icons (3, 2, 1, none), then the strip scrolls
/// to a window around the active workspace; it never exceeds `max_width`.
pub fn workspace_strip(x: i32, max_width: i32, specs: &[PillSpec]) -> WorkspaceStrip {
    let mut range = 0..specs.len();
    let mut level = 3;
    while level > 0 && strip_width(specs, level) > max_width {
        level -= 1;
    }
    if strip_width(specs, level) > max_width {
        let active = specs.iter().position(|s| s.active).unwrap_or(0);
        let (mut lo, mut hi) = (active, active + 1);
        loop {
            let mut grown = false;
            if lo > 0 && strip_width(&specs[lo - 1..hi], 0) <= max_width {
                lo -= 1;
                grown = true;
            }
            if hi < specs.len() && strip_width(&specs[lo..hi + 1], 0) <= max_width {
                hi += 1;
                grown = true;
            }
            if !grown {
                break;
            }
        }
        range = lo..hi;
    }
    let slice = &specs[range.clone()];
    let width = strip_width(slice, level).min(max_width.max(0));
    let mut at = x + STRIP_PAD;
    let mut pills = Vec::with_capacity(slice.len());
    for (offset, spec) in slice.iter().enumerate() {
        let (w, icons, extra, dot) = pill_size(*spec, level);
        let r = rect(at, 4 + (24 - PILL_H) / 2, w, PILL_H);
        let left = if offset == 0 { x } else { at - PILL_GAP / 2 };
        let right = if offset + 1 == slice.len() { x + width } else { at + w + PILL_GAP / 2 };
        pills.push(Pill {
            index: range.start + offset,
            rect: r,
            hit: rect(left, 4, right - left, 24),
            icons,
            extra,
            dot,
        });
        at += w + PILL_GAP;
    }
    WorkspaceStrip { rect: rect(x, 4, width, 24), pills }
}

/// Width the code mode hints may use next to the chip. The title keeps
/// `MIN_TITLE_TEXT` first; with less room than that no hint is drawn and only
/// the chip stays.
pub fn hint_budget(title_width: i32, chip: i32, chip_gap: i32) -> i32 {
    (title_width - chip - chip_gap - MIN_TITLE_TEXT).max(0)
}

/// Right-aligns `slots` into one capsule ending 8 px before `width`.
pub fn status_cluster(width: i32, slots: &[(StatusItem, i32)]) -> (Rect, Vec<(StatusItem, Rect)>) {
    let inner: i32 = slots.iter().map(|(_, w)| *w).sum::<i32>()
        + CLUSTER_GAP * slots.len().saturating_sub(1) as i32;
    let total = inner + 2 * CLUSTER_PAD;
    let x = width - 8 - total;
    let mut at = x + CLUSTER_PAD;
    let items = slots
        .iter()
        .map(|(item, w)| {
            let r = rect(at, 8, *w, CLUSTER_ICON);
            at += w + CLUSTER_GAP;
            (*item, r)
        })
        .collect();
    (rect(x, 4, total, 24), items)
}
#[derive(Debug)]
pub struct Layout {
    pub workspaces: Rect,
    pub toggle: Rect,
    pub app: Rect,
    pub title: Rect,
    pub clock: Rect,
    pub media: Option<Rect>,
    /// The whole status capsule: one hover highlight, one click target.
    pub status: Rect,
    pub slots: Vec<(StatusItem, Rect)>,
}
impl Layout {
    #[cfg(test)]
    pub fn new(width: i32, workspace_width: i32, layouts: bool) -> Self {
        Self::with_battery(width, workspace_width, layouts, BATTERY_SLOT)
    }
    /// `battery` is the width of the battery slot: `BATTERY_SLOT` for a
    /// battery, `CLUSTER_ICON` for the plug shown on machines without one.
    pub fn with_battery(width: i32, workspace_width: i32, layouts: bool, battery: i32) -> Self {
        let mut slots = Vec::new();
        if layouts {
            slots.push((StatusItem::Layout, LAYOUT_SLOT));
        }
        slots.extend([
            (StatusItem::Wifi, CLUSTER_ICON),
            (StatusItem::Volume, CLUSTER_ICON),
            (StatusItem::Bluetooth, CLUSTER_ICON),
            (StatusItem::Battery, battery),
        ]);
        let (status, slots) = status_cluster(width, &slots);
        let clock = rect(((width - 184) / 2).min(status.loc.x - 192).max(0), 4, 184, 24);
        // Room for the toggle, the app icon and a readable title before the clock.
        let cap = (clock.loc.x - 8 - 8 - TOGGLE_W - 8 - 24 - 8 - MIN_TITLE_TEXT).max(24);
        let workspaces = rect(8, 4, workspace_width.min(cap), 24);
        let toggle = rect(workspaces.loc.x + workspaces.size.w + 8, 4, TOGGLE_W, 24);
        let app = rect(toggle.loc.x + 40, 8, 16, 16);
        let title = rect(
            app.loc.x + 24,
            7,
            (clock.loc.x - 8 - app.loc.x - 24).max(0),
            18,
        );
        Self {
            workspaces,
            toggle,
            app,
            title,
            clock,
            media: None,
            status,
            slots,
        }
    }
    #[cfg(test)]
    pub fn slot(&self, item: StatusItem) -> Option<Rect> {
        self.slots.iter().find(|(i, _)| *i == item).map(|(_, r)| *r)
    }
    pub fn with_media(mut self, enabled: bool) -> Self {
        if enabled {
            let x = self.clock.loc.x + self.clock.size.w + 12;
            let width = (self.status.loc.x - x - 12).clamp(0, 260);
            if width >= 80 { self.media = Some(rect(x, 4, width, 24)); }
        }
        self
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Previous,
    Next,
    ClearNotifications,
    Notification(usize),
}
#[derive(Debug)]
pub struct PopoverLayout {
    pub panel: Rect,
    pub items: Vec<(Item, Rect)>,
}
impl PopoverLayout {
    pub fn anchor_bottom(&mut self, width: i32, height: i32, taskbar: i32, progress: f64) {
        let x = (width - self.panel.size.w - 8).max(0);
        let y = (height - taskbar - self.panel.size.h - 8).max(0);
        let dx = ((x - self.panel.loc.x) as f64 * progress).round() as i32;
        let dy = ((y - self.panel.loc.y) as f64 * progress).round() as i32;
        self.panel.loc.x += dx;
        self.panel.loc.y += dy;
        for (_, r) in &mut self.items { r.loc.x += dx; r.loc.y += dy; }
    }
    pub fn calendar_notifications(width: i32, count: usize) -> Self {
        let mut layout = Self::calendar(width);
        let x = layout.panel.loc.x;
        let y = layout.panel.loc.y + CALENDAR_H;
        let shown = count.min(4);
        if shown == 0 {
            layout.panel.size.h += EMPTY_H;
            return layout;
        }
        layout.items.push((Item::ClearNotifications, rect(x + 244, y, 60, 28)));
        for i in 0..shown {
            layout.items.push((Item::Notification(i), rect(x + 16, y + 36 + i as i32 * 56, 288, 48)));
        }
        layout.panel.size.h += 44 + shown as i32 * 56;
        layout
    }
    pub fn calendar(width: i32) -> Self {
        let panel = rect((width - 320) / 2, 40, 320, CALENDAR_H);
        let nav = panel.loc.y + TODAY_H + 12;
        Self {
            panel,
            items: vec![
                (Item::Previous, rect(panel.loc.x + 16, nav, 32, 32)),
                (Item::Next, rect(panel.loc.x + 272, nav, 32, 32)),
            ],
        }
    }
}
/// Height of the "Today" header above the month grid.
pub const TODAY_H: i32 = 64;
/// The calendar card: Today header plus the month grid.
pub const CALENDAR_H: i32 = TODAY_H + 316;
/// Extra height of the empty notifications state.
pub const EMPTY_H: i32 = 136;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_title_stays_between_capsules_and_clock() {
        let mut text = crate::ui::text::TextRenderer::new();
        for width in [800, 1280, 1646, 2560] {
            let l = Layout::new(width, 88, true);
            let boxes = [l.workspaces, l.toggle, l.app, l.title, l.clock, l.status];
            for pair in boxes.windows(2) {
                assert!(
                    pair[0].loc.x + pair[0].size.w <= pair[1].loc.x,
                    "{width}: {pair:?}"
                );
            }
            let title = "An extraordinarily long focused application title ".repeat(20);
            let style = crate::topbar::label_style(12.5);
            let fitted = crate::topbar::ellipsize(&mut text, &title, style, l.title.size.w);
            assert!(fitted.ends_with('…'));
            assert!(text.measure(&fitted, style) <= l.title.size.w as f32);
        }
    }
    #[test]
    fn media_capsule_fits_between_clock_and_status() {
        for width in [1280, 1646] {
            for layouts in [false, true] {
                let l = Layout::new(width, 88, layouts).with_media(true);
                let media = l.media.unwrap();
                assert!(media.size.w <= 260);
                for other in [l.workspaces, l.toggle, l.app, l.title, l.clock, l.status] {
                    assert!(!media.overlaps(other), "{width}: {other:?}");
                }
                assert!(rect(0, 0, width, 32).contains_rect(media));
            }
        }
    }
    #[test]
    fn popover_items_are_inside_panel_and_disjoint() {
        for l in [
            PopoverLayout::calendar(1280),
            PopoverLayout::calendar_notifications(1280, 4),
        ] {
            for (i, (_, r)) in l.items.iter().enumerate() {
                assert!(l.panel.contains_rect(*r));
                for (_, s) in &l.items[i + 1..] {
                    assert!(!r.overlaps(*s));
                }
            }
        }
    }
    #[test]
    fn status_cluster_is_one_aligned_capsule_on_the_8px_grid() {
        for width in [800, 1280, 1646, 2560] {
            for layouts in [false, true] {
                for battery in [CLUSTER_ICON, BATTERY_SLOT] {
                    let l = Layout::with_battery(width, 88, layouts, battery);
                    assert_eq!(l.status.loc.x + l.status.size.w, width - 8);
                    assert_eq!(l.slot(StatusItem::Layout).is_some(), layouts);
                    let mut previous_end = l.status.loc.x + CLUSTER_PAD;
                    for (i, (_, r)) in l.slots.iter().enumerate() {
                        assert!(l.status.contains_rect(*r));
                        assert_eq!(r.loc.x, previous_end + if i == 0 { 0 } else { CLUSTER_GAP });
                        previous_end = r.loc.x + r.size.w;
                    }
                    assert_eq!(previous_end + CLUSTER_PAD, l.status.loc.x + l.status.size.w);
                    assert_eq!(l.slot(StatusItem::Wifi).unwrap().size, (16, 16).into());
                }
            }
        }
    }

    fn specs(windows: &[usize], active: usize) -> Vec<PillSpec> {
        windows.iter().enumerate().map(|(i, w)| PillSpec { windows: *w, active: i == active }).collect()
    }
    /// Layout of the left side the way the bar builds it.
    fn left(width: i32, windows: &[usize], active: usize) -> (Layout, WorkspaceStrip) {
        let list = specs(windows, active);
        let cap = Layout::with_battery(width, i32::MAX / 4, true, BATTERY_SLOT).workspaces.size.w;
        let strip = workspace_strip(8, cap, &list);
        (Layout::with_battery(width, strip.rect.size.w, true, BATTERY_SLOT), strip)
    }
    #[test]
    fn workspace_strip_fits_every_size_and_never_crowds_the_title() {
        // 1280x800 and 2134x1200 (2880x1620 at scale 1.35), up to 40 workspaces.
        for width in [800, 1280, 2134] {
            for count in [1, 2, 5, 9, 20, 40] {
                for active in [0, count / 2, count - 1] {
                    let windows: Vec<usize> = (0..count).map(|i| [0, 1, 2, 3, 5][i % 5]).collect();
                    let (l, strip) = left(width, &windows, active);
                    assert!(strip.rect.size.w <= l.workspaces.size.w.max(24), "{width}/{count}: strip wider than its slot");
                    assert!(l.workspaces.loc.x + l.workspaces.size.w <= l.toggle.loc.x);
                    assert!(l.title.size.w >= MIN_TITLE_TEXT.min(l.clock.loc.x - 8 - l.title.loc.x), "{width}/{count}: title squeezed");
                    assert!(l.title.loc.x + l.title.size.w <= l.clock.loc.x);
                    assert!(strip.pills.iter().any(|p| p.index == active), "{width}/{count}: active workspace must stay visible");
                    for p in &strip.pills {
                        assert!(strip.rect.contains_rect(p.rect), "{width}/{count}: {p:?} outside {:?}", strip.rect);
                        assert!(p.rect.loc.x + p.rect.size.w <= l.workspaces.loc.x + l.workspaces.size.w);
                    }
                }
            }
        }
    }
    #[test]
    fn few_workspaces_keep_three_icons_and_overflow_counts() {
        let strip = workspace_strip(8, 400, &specs(&[0, 2, 5], 1));
        assert_eq!(strip.pills.len(), 3);
        assert!(strip.pills[0].dot);
        assert_eq!((strip.pills[1].icons, strip.pills[1].extra), (2, 0));
        assert_eq!((strip.pills[2].icons, strip.pills[2].extra), (3, 2));
        assert!(!strip.pills[2].dot);
    }
    #[test]
    fn many_workspaces_lose_icons_before_they_scroll() {
        let list = specs(&[3; 6], 0);
        let roomy = workspace_strip(8, 1000, &list);
        assert!(roomy.pills.iter().all(|p| p.icons == 3));
        // Too narrow for three icons each, wide enough for none.
        let squeezed = workspace_strip(8, roomy.rect.size.w - 40, &list);
        assert_eq!(squeezed.pills.len(), 6);
        assert!(squeezed.pills.iter().all(|p| p.icons < 3));
        let tiny = workspace_strip(8, 60, &list);
        assert!(tiny.pills.len() < 6 && tiny.rect.size.w <= 60);
        assert!(tiny.pills.iter().all(|p| p.dot));
    }
    #[test]
    fn scrolled_strip_follows_the_active_workspace() {
        let list = specs(&[1; 30], 25);
        let strip = workspace_strip(8, 120, &list);
        assert!(strip.pills.len() < 30);
        assert!(strip.pills.iter().any(|p| p.index == 25));
        let indices: Vec<_> = strip.pills.iter().map(|p| p.index).collect();
        assert!(indices.windows(2).all(|w| w[1] == w[0] + 1), "contiguous");
    }
    #[test]
    fn pill_hits_tile_the_capsule_and_pick_the_right_workspace() {
        let strip = workspace_strip(8, 500, &specs(&[0, 1, 4, 0, 2], 2));
        let mut end = strip.rect.loc.x;
        for p in &strip.pills {
            assert_eq!(p.hit.loc.x, end, "hit rects are contiguous");
            assert_eq!((p.hit.loc.y, p.hit.size.h), (4, 24));
            assert!(p.hit.contains_rect(p.rect));
            // The centre of a pill hits that pill and no other.
            let centre = (p.rect.loc.x + p.rect.size.w / 2, 16);
            let hits: Vec<_> = strip.pills.iter().filter(|q| q.hit.to_f64().contains(smithay::utils::Point::<f64, Logical>::from((centre.0 as f64, centre.1 as f64)))).collect();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].index, p.index);
            end = p.hit.loc.x + p.hit.size.w;
        }
        assert_eq!(end, strip.rect.loc.x + strip.rect.size.w);
    }
    #[test]
    fn hint_budget_keeps_the_title_readable() {
        // Wide title area: hints get what the title does not need.
        assert_eq!(hint_budget(500, 44, 8), 500 - 44 - 8 - MIN_TITLE_TEXT);
        // No room for the title plus hints: only the chip remains.
        assert_eq!(hint_budget(170, 44, 8), 0);
        assert_eq!(hint_budget(0, 44, 8), 0);
    }
}
