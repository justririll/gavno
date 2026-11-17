use super::{Action, Kind, State};
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        Canvas, GLASS, LINE,
    },
    ui::text::{TextStyleSpec, Weight},
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    utils::{Logical, Size},
};
pub(super) struct View {
    canvas: Canvas,
    icons: Icons,
    pub hits: Vec<(Rect, Action, bool)>,
    pub panel: Rect,
    pub close: Rect,
}
impl Default for View {
    fn default() -> Self {
        Self {
            canvas: Canvas::default(),
            icons: Icons::nonblocking(crate::icons::app_worker::Consumer::Panel),
            hits: Vec::new(),
            panel: Rect::default(),
            close: Rect::default(),
        }
    }
}
pub(super) fn geometry(size: Size<i32, Logical>, classic: bool, rows: usize) -> (Rect, usize) {
    let w = 420.min((size.w - 16).max(1));
    let h = (rows.max(1).min(10) as i32 * 46 + 132).min((size.h - 88).max(1));
    let y = if classic {
        (size.h - 40 - h - 8).max(0)
    } else {
        40.min((size.h - h).max(0))
    };
    (
        rect((size.w - w - 8).max(0), y, w, h),
        ((h - 132).max(46) / 46) as usize,
    )
}
fn style(size: f32, weight: Weight, color: [f32; 4]) -> TextStyleSpec {
    TextStyleSpec {
        size,
        weight,
        color,
        tracking: 0.0,
    }
}
impl State {
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        accent: [f32; 4],
        theme: &str,
        classic: bool,
    ) -> Vec<GavnoRenderElement> {
        let Some((kind, owner)) = self.open.as_ref() else {
            return vec![];
        };
        if owner != output {
            return vec![];
        }
        let kind = *kind;
        let rows = self.rows(kind);
        let (panel, capacity) = geometry(size, classic, rows.len());
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        self.offset = self.offset.min(rows.len().saturating_sub(capacity));
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + capacity {
            self.offset = self.selected + 1 - capacity;
        }
        let view = &mut self.view;
        view.panel = panel;
        view.hits.clear();
        let c = &mut view.canvas;
        c.begin(renderer, scale);
        c.shadow(panel, 20.0);
        c.rect(panel, 20.0, GLASS, 1.0, LINE);
        let x = panel.loc.x + 16;
        let y = panel.loc.y;
        c.styled_label(
            renderer,
            match kind {
                Kind::Wifi => "Wi-Fi",
                Kind::Bluetooth => "Bluetooth",
                Kind::Audio => "Volume mixer",
            },
            x,
            y + 16,
            panel.size.w - 32,
            style(20.0, Weight::SemiBold, [0.96, 0.95, 0.98, 1.0]),
        );
        let close = rect(panel.loc.x + panel.size.w - 42, y + 12, 28, 28);
        view.close = close;
        c.label(renderer, "×", close.loc.x + 7, close.loc.y, 24, 22.0);
        // Outside the card and the close button both dismiss without activating a row.
        let row_width = panel.size.w - 32;
        for (i, row) in rows.iter().enumerate().skip(self.offset).take(capacity) {
            let area = rect(x, y + 52 + (i - self.offset) as i32 * 46, row_width, 42);
            c.rect(
                area,
                9.0,
                if i == self.selected {
                    [accent[0], accent[1], accent[2], 0.16]
                } else {
                    [1.0, 1.0, 1.0, 0.035]
                },
                0.0,
                LINE,
            );
            c.colored_icon(
                renderer,
                &mut view.icons,
                &[&row.icon, "application-x-executable-symbolic"],
                theme,
                rect(area.loc.x + 10, area.loc.y + 11, 20, 20),
                matches!(row.action, Action::Slider(super::audio::Target::Stream(_)))
                    .then_some(crate::topbar::FULL_COLOR),
            );
            let text_x = area.loc.x + 40;
            if let Some(level) = row.level {
                c.styled_label(
                    renderer,
                    &row.label,
                    text_x,
                    area.loc.y + 3,
                    row_width - 104,
                    style(11.5, Weight::Medium, [0.96, 0.95, 0.98, 0.85]),
                );
                let track = rect(text_x, area.loc.y + 25, (row_width - 104).max(10), 6);
                c.rect(track, 3.0, [1.0, 1.0, 1.0, 0.14], 0.0, LINE);
                c.rect(
                    rect(
                        track.loc.x,
                        track.loc.y,
                        track.size.w * level as i32 / 100,
                        6,
                    ),
                    3.0,
                    accent,
                    0.0,
                    LINE,
                );
                let mute = rect(area.loc.x + row_width - 54, area.loc.y, 54, 42);
                c.label(
                    renderer,
                    &format!("{level}%"),
                    mute.loc.x + 2,
                    mute.loc.y + 3,
                    50,
                    11.5,
                );
                c.label(
                    renderer,
                    if row.muted { "Muted" } else { "Mute" },
                    mute.loc.x + 2,
                    mute.loc.y + 22,
                    50,
                    10.0,
                );
                view.hits.push((mute, row.action.clone(), false));
                view.hits.push((
                    rect(track.loc.x, area.loc.y + 19, track.size.w, 23),
                    row.action.clone(),
                    true,
                ));
            } else {
                c.styled_label(
                    renderer,
                    &row.label,
                    text_x,
                    area.loc.y + 3,
                    row_width - 92,
                    style(13.0, Weight::Medium, [0.96, 0.95, 0.98, 1.0]),
                );
                c.styled_label(
                    renderer,
                    &row.detail,
                    text_x,
                    area.loc.y + 23,
                    row_width - 92,
                    style(10.5, Weight::Medium, [0.96, 0.95, 0.98, 0.55]),
                );
                if matches!(row.action, Action::WifiPower | Action::BluetoothPower) {
                    let on = if row.action == Action::WifiPower {
                        self.wifi.enabled && self.wifi.hardware
                    } else {
                        self.bluetooth.powered
                    };
                    let switch = rect(area.loc.x + area.size.w - 48, area.loc.y + 11, 36, 20);
                    c.rect(
                        switch,
                        10.0,
                        if on { accent } else { [1.0, 1.0, 1.0, 0.18] },
                        0.0,
                        LINE,
                    );
                    c.rect(
                        rect(
                            switch.loc.x + if on { 18 } else { 2 },
                            switch.loc.y + 2,
                            16,
                            16,
                        ),
                        8.0,
                        [1.0; 4],
                        0.0,
                        LINE,
                    );
                } else if let Action::Network(path) = &row.action {
                    if self
                        .wifi
                        .networks
                        .iter()
                        .any(|n| &n.path == path && n.secured)
                    {
                        c.icon(
                            renderer,
                            &mut view.icons,
                            &["changes-prevent-symbolic"],
                            theme,
                            rect(area.loc.x + area.size.w - 26, area.loc.y + 13, 16, 16),
                        );
                    }
                }
                view.hits.push((area, row.action.clone(), false));
            }
        }
        if rows.is_empty() {
            c.label(
                renderer,
                "No audio devices or streams",
                x,
                y + 64,
                row_width,
                13.0,
            );
        }
        let footer_y = y + panel.size.h - 72;
        if let Some((_, secret)) = &self.password {
            c.rect(
                rect(x, footer_y - 5, row_width, 32),
                8.0,
                [accent[0], accent[1], accent[2], 0.12],
                1.0,
                accent,
            );
            let masked = if secret.0.is_empty() {
                "Password".into()
            } else {
                "•".repeat(secret.0.chars().count().min(48))
            };
            c.label(renderer, &masked, x + 10, footer_y, row_width - 20, 13.0);
            c.label(
                renderer,
                "Enter connects · Escape cancels",
                x,
                footer_y + 34,
                row_width,
                11.0,
            );
        } else {
            if let Some(error) = &self.errors[super::index(kind)] {
                c.styled_label(
                    renderer,
                    error,
                    x,
                    footer_y,
                    row_width,
                    style(11.0, Weight::Medium, [1.0, 0.6, 0.5, 1.0]),
                );
            } else if rows.len() > capacity {
                c.label(
                    renderer,
                    &format!(
                        "{}–{} of {} · Scroll for more",
                        self.offset + 1,
                        (self.offset + capacity).min(rows.len()),
                        rows.len()
                    ),
                    x,
                    footer_y,
                    row_width,
                    11.0,
                );
            }
            c.label(
                renderer,
                "↑↓ / jk select · ←→ / hl adjust · Enter · Esc",
                x,
                y + panel.size.h - 30,
                row_width,
                10.5,
            );
        }
        c.finish()
    }
}
