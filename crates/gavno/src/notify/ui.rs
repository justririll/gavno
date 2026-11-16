use super::Notifications;
use crate::{
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        label_style, FULL_COLOR, LINE,
    },
    ui::text::Weight,
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    utils::{Logical, Size},
};
use std::time::Duration;

const HEIGHT: i32 = 164;
pub(super) fn close_rect(r: Rect) -> Rect {
    rect(r.loc.x + r.size.w - 32, r.loc.y + 6, 26, 26)
}

impl Notifications {
    pub(super) fn layout(&mut self, size: Size<i32, Logical>) {
        let width = 360.min((size.w - 16).max(1));
        let mut y = 40;
        for c in &mut self.cards {
            if !c.visible {
                c.rect = None;
                continue;
            }
            let x = size.w - width - 8
                + ((1.0 - c.spring.value.clamp(0.0, 1.0)) * (width + 8) as f64) as i32;
            let panel = rect(x, y, width, HEIGHT);
            c.rect = Some(panel);
            c.buttons.clear();
            let actions: Vec<_> = c
                .notice
                .actions
                .iter()
                .filter(|(k, _)| k != "default")
                .take(3)
                .collect();
            let w = (width - 24) / actions.len().max(1) as i32;
            for (i, (key, _)) in actions.into_iter().enumerate() {
                c.buttons
                    .push((rect(x + 12 + i as i32 * w, y + 128, w - 4, 26), key.clone()));
            }
            y += HEIGHT + 8;
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
    ) -> Vec<GavnoRenderElement> {
        if !self.active || self.blocked || self.output.as_ref() != Some(output) {
            return Vec::new();
        }
        for c in &mut self.cards {
            if c.visible {
                c.spring.advance(now);
            }
        }
        self.layout(size);
        let canvas = &mut self.canvas;
        canvas.begin(renderer, scale);
        for c in &mut self.cards {
            let Some(panel) = c.rect else { continue };
            canvas.alpha = if c.closing {
                c.spring.value.clamp(0.0, 1.0) as f32
            } else {
                1.0
            };
            canvas.shadow(panel, 14.0);
            canvas.rect(panel, 14.0, [0.065, 0.075, 0.10, 0.98], 1.0, LINE);
            let x = panel.loc.x;
            let y = panel.loc.y;
            let icon = if !c.notice.icon.is_empty() {
                c.notice.icon.clone()
            } else {
                self.entries
                    .resolve(if c.notice.desktop.is_empty() {
                        &c.notice.app
                    } else {
                        &c.notice.desktop
                    })
                    .map(|e| e.icon)
                    .unwrap_or_default()
            };
            if !canvas.colored_icon(
                renderer,
                &mut self.icons,
                &[&icon],
                theme,
                rect(x + 12, y + 14, 32, 32),
                Some(FULL_COLOR),
            ) {
                canvas.rect(
                    rect(x + 12, y + 14, 32, 32),
                    9.0,
                    [0.25, 0.29, 0.36, 1.0],
                    0.0,
                    LINE,
                );
                canvas.label(
                    renderer,
                    &c.notice
                        .app
                        .chars()
                        .next()
                        .unwrap_or('?')
                        .to_uppercase()
                        .to_string(),
                    x + 22,
                    y + 20,
                    20,
                    16.0,
                );
            }
            let mut app = label_style(11.0);
            app.color[3] = 0.55;
            canvas.styled_label(
                renderer,
                &c.notice.app,
                x + 56,
                y + 12,
                panel.size.w - 96,
                app,
            );
            let mut summary = label_style(13.5);
            summary.weight = Weight::SemiBold;
            canvas.styled_label(
                renderer,
                &c.notice.summary,
                x + 56,
                y + 30,
                panel.size.w - 72,
                summary,
            );
            canvas.label(renderer, "×", x + panel.size.w - 25, y + 8, 20, 18.0);
            let style = label_style(12.0);
            let lines = wrap(&mut canvas.text, &c.notice.body, style, panel.size.w - 24);
            for (i, line) in lines.iter().enumerate() {
                canvas.styled_label(
                    renderer,
                    line,
                    x + 12,
                    y + 62 + i as i32 * 19,
                    panel.size.w - 24,
                    style,
                );
            }
            for (r, key) in &c.buttons {
                canvas.rect(*r, 7.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);
                if let Some((_, label)) = c.notice.actions.iter().find(|(k, _)| k == key) {
                    canvas.label(
                        renderer,
                        label,
                        r.loc.x + 6,
                        r.loc.y + 5,
                        r.size.w - 12,
                        11.0,
                    );
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
