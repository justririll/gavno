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
            status,
            layout: layouts.then(|| rect(status.loc.x + 8, 4, 28, 24)),
            network: rect(x, 8, 16, 16),
            volume: rect(x + 32, 8, 16, 16),
            battery: rect(x + 64, 8, 56, 16),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    VolumeIcon,
    Volume,
    Brightness,
    Network,
    Battery,
    Power(usize),
    Previous,
    Next,
}
#[derive(Debug)]
pub struct PopoverLayout {
    pub panel: Rect,
    pub items: Vec<(Item, Rect)>,
}
impl PopoverLayout {
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
    fn popover_items_are_inside_panel_and_disjoint() {
        for l in [
            PopoverLayout::quick(800, false),
            PopoverLayout::quick(1646, true),
            PopoverLayout::calendar(1280),
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
