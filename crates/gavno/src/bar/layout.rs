//! Shared drawing and input geometry. All dimensions are logical pixels.
use smithay::utils::{Logical, Rectangle};
pub type Rect = Rectangle<i32, Logical>;
pub fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rectangle::new((x, y).into(), (w.max(0), h.max(0)).into())
}
/// One slot of the status cluster at the right end of the bar. Adding an
/// indicator (for example a camera dot) means adding a variant, one entry in
/// `Layout::status_slots` and one arm where the slot is drawn.
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
        let workspaces = rect(8, 4, workspace_width.min((clock.loc.x - 104).max(24)), 24);
        let toggle = rect(workspaces.loc.x + workspaces.size.w + 8, 4, 32, 24);
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
        let y = layout.panel.loc.y + 316;
        layout.items.push((Item::ClearNotifications, rect(x + 244, y, 60, 28)));
        for i in 0..count.min(4) {
            layout.items.push((Item::Notification(i), rect(x + 12, y + 36 + i as i32 * 52, 296, 48)));
        }
        layout.panel.size.h += 44 + count.min(4) as i32 * 52;
        layout
    }
    pub fn calendar(width: i32) -> Self {
        let panel = rect((width - 320) / 2, 40, 320, 316);
        Self {
            panel,
            items: vec![
                (Item::Previous, rect(panel.loc.x + 16, 52, 32, 32)),
                (Item::Next, rect(panel.loc.x + 272, 52, 32, 32)),
            ],
        }
    }
}
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
}
