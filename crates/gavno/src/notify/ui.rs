use super::Notifications;
use crate::{
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        FULL_COLOR, LINE,
    },
    ui::{
        style::{self as look, radius, size, text, TEXT},
        text::Weight,
    },
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    utils::{Logical, Size},
};
use std::time::Duration;

const BODY_Y: i32 = 58;
const LINE_H: i32 = 19;
const BUTTONS_H: i32 = 40;
pub(super) fn close_rect(r: Rect) -> Rect {
    rect(r.loc.x + r.size.w - 34, r.loc.y + 8, 26, 26)
}

/// Height one toast needs at least; used to decide whether a column fits.
const NEED_H: i32 = 96;
/// Where the toast column goes: the top right corner, or beside or below an
/// open popover. `None` holds the toasts until the popover closes.
pub(super) fn column(size: Size<i32, Logical>, width: i32, avoid: Option<Rect>) -> Option<(i32, i32)> {
    let right = size.w - width - 8;
    let top = 40;
    let Some(a) = avoid else { return Some((right, top)) };
    let overlaps = |x: i32, y: i32| rect(x, y, width, NEED_H).overlaps(a);
    if !overlaps(right, top) {
        return Some((right, top));
    }
    let aside = a.loc.x - width - 8;
    if aside >= 8 && !overlaps(aside, top) {
        return Some((aside, top));
    }
    let below = a.loc.y + a.size.h + 8;
    (below + NEED_H <= size.h - 8).then_some((right, below))
}

impl Notifications {
    pub(super) fn layout(&mut self, size: Size<i32, Logical>) {
        if self.tty {
            for c in &mut self.cards {
                c.rect = None;
                c.buttons.clear();
            }
            return;
        }
        let width = 360.min((size.w - 16).max(1));
        let column = column(size, width, self.avoid);
        let (column_x, top) = column.unwrap_or((size.w - width - 8, 40));
        let mut y = top;
        for c in &mut self.cards {
            if !c.visible || column.is_none() || y + NEED_H > size.h - 8 {
                c.rect = None;
                continue;
            }
            if c.lines.as_ref().is_none_or(|(w, _)| *w != width) {
                let lines = wrap(&mut self.canvas.text, &c.notice.body, text(size::BODY - 1.0, Weight::Medium, TEXT), width - 32);
                c.lines = Some((width, lines));
            }
            let lines = c.lines.as_ref().map_or(0, |(_, l)| l.len()) as i32;
            let actions: Vec<_> = c
                .notice
                .actions
                .iter()
                .filter(|(k, _)| k != "default")
                .take(3)
                .collect();
            let buttons_y = BODY_Y + lines * LINE_H + look::space::XS;
            let height = buttons_y + if actions.is_empty() { 6 } else { BUTTONS_H };
            let x = column_x
                + ((1.0 - c.spring.value.clamp(0.0, 1.0)) * (width + 8) as f64) as i32;
            let panel = rect(x, y, width, height);
            c.rect = Some(panel);
            c.buttons.clear();
            let pad = look::space::LG;
            let w = (width - 2 * pad) / actions.len().max(1) as i32;
            for (i, (key, _)) in actions.into_iter().enumerate() {
                c.buttons
                    .push((rect(x + pad + i as i32 * w, y + buttons_y, w - look::space::SM, 28), key.clone()));
            }
            y += height + 8;
        }
    }
    pub fn anchor_classic(&mut self, size: Size<i32, Logical>, progress: f64) {
        let total = self.cards.iter().filter_map(|c| c.rect).map(|r| r.size.h + 8).sum::<i32>();
        let bottom = (size.h - crate::classic::TASKBAR_HEIGHT - 8 - total).max(8);
        let dy = ((bottom - 40) as f64 * progress.clamp(0.0, 1.0)).round() as i32;
        for card in &mut self.cards {
            if let Some(r) = &mut card.rect { r.loc.y += dy; }
            for (r, _) in &mut card.buttons { r.loc.y += dy; }
        }
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        theme: &str,
        shell_progress: f64,
        accent: [f32; 4],
    ) -> Vec<GavnoRenderElement> {
        if !self.active || self.blocked || self.output.as_ref() != Some(output) {
            return Vec::new();
        }
        for c in &mut self.cards {
            if c.visible {
                c.spring.advance(now);
            }
        }
        if self.tty {
            return self.elements_tty(renderer, scale, size.w);
        }
        self.layout(size);
        self.anchor_classic(size, shell_progress);
        // Several cards from one app share a count badge on the first of them.
        let counts: Vec<Option<usize>> = {
            let mut seen: Vec<&str> = Vec::new();
            self.cards
                .iter()
                .map(|c| {
                    if !c.visible || c.closing || seen.contains(&c.notice.app.as_str()) {
                        return None;
                    }
                    seen.push(c.notice.app.as_str());
                    let n = self.cards.iter().filter(|o| !o.closing && o.notice.app == c.notice.app).count();
                    (n > 1).then_some(n)
                })
                .collect()
        };
        let canvas = &mut self.canvas;
        canvas.begin(renderer, scale);
        for (c, count) in self.cards.iter_mut().zip(counts) {
            let Some(panel) = c.rect else { continue };
            // Fade with the slide, in and out.
            canvas.alpha = (c.spring.value * 2.0).clamp(0.0, 1.0) as f32;
            look::card(canvas, panel, radius::TILE);
            let x = panel.loc.x;
            let y = panel.loc.y;
            let entry = self
                .entries
                .resolve(if c.notice.desktop.is_empty() {
                    &c.notice.app
                } else {
                    &c.notice.desktop
                })
                .map(|e| e.icon)
                .unwrap_or_default();
            let names: Vec<&str> = [c.notice.icon.as_str(), entry.as_str()]
                .into_iter()
                .filter(|n| !n.is_empty())
                .collect();
            let icon = rect(x + 16, y + 14, 32, 32);
            if !canvas.colored_icon(renderer, &mut self.icons, &names, theme, icon, Some(FULL_COLOR)) {
                canvas.rect(icon, radius::ROW, look::SURFACE_RAISED, 0.0, LINE);
                let initial = c.notice.app.chars().next().unwrap_or('?').to_uppercase().to_string();
                let st = text(size::HEADING, Weight::SemiBold, TEXT);
                let w = canvas.text.measure(&initial, st).ceil() as i32;
                canvas.styled_label(renderer, &initial, icon.loc.x + (32 - w) / 2, icon.loc.y + 7, w + 2, st);
            }
            let app = text(size::CAPTION, Weight::SemiBold, look::dim(TEXT, 0.55));
            let mut app_w = panel.size.w - 104;
            if let Some(n) = count {
                // "3" on an accent-washed pill after the app name.
                let name_w = (canvas.text.measure(&c.notice.app, app).ceil() as i32).min(app_w - 32);
                let label = n.to_string();
                let st = text(size::CAPTION, Weight::SemiBold, look::on_accent(accent));
                let w = look::tabular_width(canvas, &label, st);
                let pill = rect(x + 60 + name_w + 8, y + 11, w + 12, 16);
                canvas.rect(pill, 8.0, look::wash(accent, 0.94), 0.0, LINE);
                look::tabular_label(canvas, renderer, &label, pill.loc.x + 6, pill.loc.y + 1, st);
                app_w = name_w;
            }
            canvas.styled_label(renderer, &c.notice.app, x + 60, y + 12, app_w, app);
            canvas.styled_label(
                renderer,
                &c.notice.summary,
                x + 60,
                y + 30,
                panel.size.w - 92,
                text(size::BODY + 0.5, Weight::SemiBold, TEXT),
            );
            let close = close_rect(panel);
            canvas.icon(renderer, &mut self.icons, &["window-close-symbolic"], theme, rect(close.loc.x + 5, close.loc.y + 5, 16, 16));
            let style = text(size::BODY - 1.0, Weight::Medium, look::dim(TEXT, 0.85));
            let lines = c.lines.as_ref().map(|(_, l)| l.as_slice()).unwrap_or_default();
            for (i, line) in lines.iter().enumerate() {
                canvas.styled_label(
                    renderer,
                    line,
                    x + 16,
                    y + BODY_Y + i as i32 * LINE_H,
                    panel.size.w - 32,
                    style,
                );
            }
            for (n, (r, key)) in c.buttons.iter().enumerate() {
                // The first action is the primary one and takes the accent.
                let primary = n == 0;
                canvas.rect(
                    *r,
                    radius::FIELD,
                    if primary { look::wash(accent, 0.94) } else { look::SURFACE_RAISED },
                    0.0,
                    LINE,
                );
                if let Some((_, label)) = c.notice.actions.iter().find(|(k, _)| k == key) {
                    let st = text(size::SMALL, Weight::SemiBold, if primary { look::on_accent(accent) } else { TEXT });
                    let w = (canvas.text.measure(label, st).ceil() as i32).min(r.size.w - 12);
                    canvas.styled_label(renderer, label, r.loc.x + (r.size.w - w) / 2, r.loc.y + 5, r.size.w - 12, st);
                }
            }
        }
        canvas.finish()
    }
}
fn wrap(
    text: &mut crate::ui::text::TextRenderer,
    body: &str,
    style: crate::ui::text::TextStyleSpec,
    width: i32,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut words = body.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let next = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if text.measure(&next, style) > width as f32 && !line.is_empty() && lines.len() < 2 {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
        } else {
            line = next;
        }
        if lines.len() == 2 {
            for word in words.by_ref() {
                line.push(' ');
                line.push_str(word);
            }
            break;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// "[ 1234.567] Telegram: Alice: see you at 6", flattened to one line.
pub(crate) fn kernel_line(born: Duration, app: &str, summary: &str, body: &str) -> String {
    let mut text = String::new();
    for part in [app, summary, body].into_iter().filter(|p| !p.trim().is_empty()) {
        if !text.is_empty() {
            text.push_str(": ");
        }
        text.push_str(&part.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    format!("[{:>5}.{:03}] {text}", born.as_secs(), born.subsec_millis())
}

impl Notifications {
    /// Console look: up to three kernel message lines at the top right, under the status line.
    fn elements_tty(&mut self, renderer: &mut GlesRenderer, scale: f64, width: i32) -> Vec<GavnoRenderElement> {
        const FONT: f32 = 15.0;
        const LINE: i32 = 20;
        let style = crate::ui::text::TextStyleSpec {
            size: FONT,
            weight: Weight::Mono,
            color: [0.70, 0.70, 0.74, 1.0],
            tracking: 0.0,
        };
        let canvas = &mut self.canvas;
        canvas.begin(renderer, scale);
        let width = width.max(200);
        let mut row = 0;
        for card in self.cards.iter().filter(|c| c.visible && !c.closing || c.spring.is_active() && c.spring.value > 0.01) {
            if row >= 3 {
                break;
            }
            canvas.alpha = (card.spring.value * 2.0).clamp(0.0, 1.0) as f32;
            let line = kernel_line(card.born, &card.notice.app, &card.notice.summary, &card.notice.body);
            let line_w = (canvas.text.measure(&line, style).ceil() as i32).min(width - 28);
            let y = crate::tty_mode::BAR_HEIGHT + 10 + row * LINE;
            // A dark backing keeps the line readable over a window.
            canvas.rect(crate::topbar::geometry::rect(width - 14 - line_w - 6, y - 1, line_w + 12, LINE), 0.0, [0.03, 0.03, 0.04, 0.88], 0.0, crate::topbar::LINE);
            canvas.styled_label(renderer, &line, width - 14 - line_w, y, line_w + 4, style);
            row += 1;
        }
        canvas.finish()
    }
}
