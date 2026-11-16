//! The password dialog, drawn with the shell canvas in the launcher's style.
use super::Dialog;
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        label_style, Canvas, LINE,
    },
    ui::text::{TextRenderer, TextStyleSpec, Weight},
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Point, Size},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Cancel,
    Submit,
    /// Previous (-1) or next (+1) user.
    User(i32),
    Panel,
}

#[derive(Default)]
pub struct View {
    canvas: Canvas,
    icons: Icons,
    hits: Vec<(Rect, Hit)>,
}

const WIDTH: i32 = 440;
const PAD: i32 = 24;
const LINE_H: i32 = 19;
const MAX_LINES: usize = 4;
const TEXT: [f32; 4] = [0.96, 0.95, 0.98, 1.0];
const DIM: [f32; 4] = [0.96, 0.95, 0.98, 0.58];
const ERROR: [f32; 4] = [1.0, 0.48, 0.45, 1.0];
const PANEL: [f32; 4] = [0.065, 0.075, 0.10, 0.97];

fn style(size: f32, color: [f32; 4], weight: Option<Weight>) -> TextStyleSpec {
    let mut s = label_style(size);
    s.color = color;
    if let Some(weight) = weight {
        s.weight = weight;
    }
    s
}

/// Greedy word wrap into at most `max` lines; an overflowing last line ends in "…".
pub fn wrap(text: &mut TextRenderer, value: &str, style: TextStyleSpec, width: i32, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for paragraph in value.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() { word.to_owned() } else { format!("{line} {word}") };
            if line.is_empty() || text.measure(&candidate, style) <= width as f32 {
                line = candidate;
            } else {
                lines.push(std::mem::replace(&mut line, word.to_owned()));
            }
        }
        lines.push(line);
    }
    if lines.len() > max {
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    lines.retain(|l| !l.is_empty());
    lines
}

impl View {
    pub fn hit(&self, p: Point<f64, Logical>) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.to_f64().contains(p)).map(|(_, h)| *h)
    }

    fn centered(&mut self, renderer: &mut GlesRenderer, value: &str, x: i32, y: i32, width: i32, s: TextStyleSpec) {
        let w = (self.canvas.text.measure(value, s).ceil() as i32).min(width);
        self.canvas.styled_label(renderer, value, x + (width - w) / 2, y, width, s);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        dialog: &Dialog,
        size: Size<i32, Logical>,
        scale: f64,
        accent: [f32; 4],
        theme: &str,
    ) -> Vec<GavnoRenderElement> {
        self.canvas.begin(renderer, scale);
        self.hits.clear();
        let width = WIDTH.min(size.w - 32).max(200);
        let inner = width - 2 * PAD;
        let body = style(13.0, TEXT, None);
        let lines = wrap(&mut self.canvas.text, &dialog.begin.message, body, inner, MAX_LINES);
        let several = dialog.begin.users.len() > 1;
        let error = dialog.error.is_some();
        let height = PAD + 44 + 12 + 26 + 8 + lines.len() as i32 * LINE_H + 14 + 26 + 10 + 40
            + if error { 26 } else { 12 } + 36 + PAD;
        let x = (size.w - width) / 2;
        let mut y = ((size.h - height) / 2).max(0);
        let panel = rect(x, y, width, height);

        let c = &mut self.canvas;
        c.rect(rect(0, 0, size.w, size.h), 0.0, [0.0, 0.0, 0.0, 0.45], 0.0, LINE);
        c.shadow(panel, 18.0);
        c.rect(panel, 18.0, PANEL, 1.0, LINE);
        self.hits.push((panel, Hit::Panel));
        y += PAD;
        let icon = dialog.begin.icon.as_str();
        c.icon(renderer, &mut self.icons, &[icon, "dialog-password-symbolic", "dialog-password"], theme, rect(x + (width - 44) / 2, y, 44, 44));
        y += 44 + 12;
        self.centered(renderer, "Authentication Required", x + PAD, y, inner, style(17.0, TEXT, Some(Weight::SemiBold)));
        y += 26 + 8;
        for line in &lines {
            self.centered(renderer, line, x + PAD, y, inner, body);
            y += LINE_H;
        }
        y += 14;

        let user = dialog.user();
        if several {
            let arrows = style(15.0, TEXT, None);
            let left = rect(x + PAD, y - 2, 30, 28);
            let right = rect(x + width - PAD - 30, y - 2, 30, 28);
            self.canvas.rect(left, 8.0, [1.0, 1.0, 1.0, 0.06], 0.0, LINE);
            self.canvas.rect(right, 8.0, [1.0, 1.0, 1.0, 0.06], 0.0, LINE);
            self.centered(renderer, "‹", left.loc.x, left.loc.y + 4, left.size.w, arrows);
            self.centered(renderer, "›", right.loc.x, right.loc.y + 4, right.size.w, arrows);
            self.centered(renderer, user, x + PAD + 36, y + 2, inner - 72, style(14.0, TEXT, Some(Weight::Medium)));
            self.hits.push((left, Hit::User(-1)));
            self.hits.push((right, Hit::User(1)));
        } else {
            self.centered(renderer, &format!("Authenticating as {user}"), x + PAD, y + 4, inner, style(12.0, DIM, None));
        }
        y += 26 + 10;

        let field = rect(x + PAD, y, inner, 40);
        let border = if dialog.busy() { LINE } else { [accent[0], accent[1], accent[2], 0.85] };
        self.canvas.rect(field, 10.0, [1.0, 1.0, 1.0, 0.06], 1.0, border);
        let field_style = style(14.0, TEXT, None);
        if dialog.busy() {
            self.canvas.styled_label(renderer, "Checking…", field.loc.x + 14, y + 11, inner - 28, style(13.0, DIM, None));
        } else if dialog.password.is_empty() {
            self.canvas.styled_label(renderer, "Password", field.loc.x + 14, y + 11, inner - 28, style(13.0, DIM, None));
            self.canvas.rect(rect(field.loc.x + 14, y + 10, 2, 20), 1.0, accent, 0.0, LINE);
        } else {
            let dots: String = "•".repeat(dialog.password.chars().count().min(40));
            let w = self.canvas.text.measure(&dots, field_style).ceil() as i32;
            self.canvas.styled_label(renderer, &dots, field.loc.x + 14, y + 10, inner - 28, field_style);
            self.canvas.rect(rect(field.loc.x + 15 + w.min(inner - 30), y + 10, 2, 20), 1.0, accent, 0.0, LINE);
        }
        y += 40;
        if let Some(error) = &dialog.error {
            self.canvas.styled_label(renderer, error, x + PAD, y + 7, inner, style(12.0, ERROR, None));
            y += 26;
        } else {
            y += 12;
        }

        let submit = rect(x + width - PAD - 140, y, 140, 36);
        let cancel = rect(submit.loc.x - 10 - 110, y, 110, 36);
        self.canvas.rect(cancel, 10.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);
        let ready = !dialog.busy() && !dialog.password.is_empty();
        let fill = [accent[0], accent[1], accent[2], if ready { 0.92 } else { 0.35 }];
        self.canvas.rect(submit, 10.0, fill, 0.0, LINE);
        let button = style(13.0, TEXT, Some(Weight::Medium));
        self.centered(renderer, "Cancel", cancel.loc.x, y + 9, cancel.size.w, button);
        self.centered(renderer, "Authenticate", submit.loc.x, y + 9, submit.size.w, button);
        self.hits.push((cancel, Hit::Cancel));
        self.hits.push((submit, Hit::Submit));
        self.canvas.finish()
    }
}
