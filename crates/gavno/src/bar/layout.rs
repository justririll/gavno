//! Shared drawing and input geometry. All dimensions are logical pixels.
use smithay::utils::{Logical, Rectangle};
pub type Rect = Rectangle<i32, Logical>;
pub fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rectangle::new((x, y).into(), (w.max(0), h.max(0)).into())
}
#[derive(Debug)]
pub struct Layout {
    pub workspaces: Rect,
    pub toggle: Rect,
    pub app: Rect,
    pub title: Rect,
    pub clock: Rect,
    pub media: Option<Rect>,
    pub status: Rect,
    pub layout: Option<Rect>,
    pub network: Rect,
    pub volume: Rect,
    pub battery: Rect,
}
impl Layout {
    pub fn new(width: i32, workspace_width: i32, layouts: bool) -> Self {
        let clock = rect((width - 184) / 2, 4, 184, 24);
        let status = rect(
            width - if layouts { 176 } else { 144 },
            4,
            if layouts { 168 } else { 136 },
            24,
        );
        let workspaces = rect(8, 4, workspace_width.min((clock.loc.x - 104).max(24)), 24);
        let toggle = rect(workspaces.loc.x + workspaces.size.w + 8, 4, 32, 24);
        let app = rect(toggle.loc.x + 40, 8, 16, 16);
        let title = rect(
            app.loc.x + 24,
            7,
            (clock.loc.x - 8 - app.loc.x - 24).max(0),
            18,
        );
        let x = status.loc.x + if layouts { 40 } else { 8 };
        Self {
            workspaces,
            toggle,
            app,
            title,
            clock,
            media: None,
            status,
            layout: layouts.then(|| rect(status.loc.x + 8, 4, 28, 24)),
            network: rect(x, 8, 16, 16),
            volume: rect(x + 32, 8, 16, 16),
            battery: rect(x + 64, 8, 56, 16),
        }
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
    MediaInfo,
    MediaPrevious,
    MediaPlayPause,
    MediaNext,
    VolumeIcon,
    Volume,
    Brightness,
    Network,
    Battery,
    Power(usize),
    Previous,
    Next,
    DoNotDisturb,
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
    pub fn quick(width: i32, brightness: bool) -> Self {
        let x = (width - 328).max(8);
        let y = 40;
        let mut items = vec![
            (Item::VolumeIcon, rect(x + 16, y + 20, 32, 32)),
            (Item::Volume, rect(x + 64, y + 20, 232, 32)),
        ];
        let offset = if brightness {
            items.push((Item::Brightness, rect(x + 64, y + 68, 232, 32)));
            48
        } else {
            0
        };
        items.push((Item::Network, rect(x + 16, y + 68 + offset, 288, 40)));
        items.push((Item::Battery, rect(x + 16, y + 116 + offset, 288, 40)));
        for i in 0..5 {
            items.push((
                Item::Power(i),
                rect(x + 20 + i as i32 * 56, y + 172 + offset, 40, 40),
            ));
        }
        Self {
            panel: rect(x, y, 320, 244 + offset),
            items,
        }
    }
    pub fn quick_notifications(width: i32, brightness: bool, enabled: bool) -> Self {
        let mut layout = Self::quick(width, brightness);
        if enabled {
            let y = layout.panel.loc.y + 164 + if brightness { 48 } else { 0 };
            for (item, r) in &mut layout.items {
                if matches!(item, Item::Power(_)) { r.loc.y += 48; }
            }
            layout.items.push((Item::DoNotDisturb, rect(layout.panel.loc.x + 16, y, 288, 40)));
            layout.panel.size.h += 48;
        }
        layout
    }
    pub fn with_media(mut self, enabled: bool) -> Self {
        if enabled {
            for (_, area) in &mut self.items { area.loc.y += 116; }
            let x = self.panel.loc.x + 16;
            let y = self.panel.loc.y + 16;
            self.items.push((Item::MediaInfo, rect(x, y, 288, 52)));
            for (i, item) in [Item::MediaPrevious, Item::MediaPlayPause, Item::MediaNext].into_iter().enumerate() {
                self.items.push((item, rect(x + 76 + i as i32 * 48, y + 60, 40, 32)));
            }
            self.panel.size.h += 116;
        }
        self
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
            PopoverLayout::quick(800, false),
            PopoverLayout::quick(1646, true),
            PopoverLayout::quick_notifications(1646, true, true).with_media(true),
            PopoverLayout::quick_notifications(1280, false, true).with_media(true),
            PopoverLayout::calendar(1280),
            PopoverLayout::calendar_notifications(1280, 4),
            PopoverLayout::quick_notifications(1646, true, true),
        ] {
            for (i, (_, r)) in l.items.iter().enumerate() {
                assert!(l.panel.contains_rect(*r));
                for (_, s) in &l.items[i + 1..] {
                    assert!(!r.overlaps(*s));
                }
            }
        }
    }
}
