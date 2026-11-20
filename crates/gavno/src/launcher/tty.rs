//! The launcher as a console command line: `$ query` at the bottom with the
//! matches listed above it as plain monospace lines. Only the look differs;
//! the filtering, frecency, calculator, files and AI row are the launcher's.

use super::*;

const LINES: usize = 10;
const FONT: f32 = 15.0;
const LINE_H: i32 = 20;
const MARGIN: i32 = 14;
/// Room kept for the status line under the command line.
const STRIP: i32 = 4;
const GREY: [f32; 4] = [0.78, 0.78, 0.81, 1.0];
const DIM: [f32; 4] = [0.46, 0.46, 0.51, 1.0];
const BAND: [f32; 4] = [0.035, 0.035, 0.042, 0.97];

fn mono(color: [f32; 4]) -> crate::ui::text::TextStyleSpec {
    crate::ui::text::TextStyleSpec { size: FONT, weight: Weight::Mono, color, tracking: 0.0 }
}

/// First item of the window of `LINES` rows that keeps `selected` in view.
pub(super) fn first_line(selected: usize) -> usize {
    (selected + 1).saturating_sub(LINES)
}

impl Launcher {
    pub(super) fn elements_tty(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        accent: [f32; 4],
    ) -> Vec<GavnoRenderElement> {
        self.fade.advance(now);
        if !self.visible() || self.output.as_ref().is_some_and(|o| o != output) {
            return Vec::new();
        }
        self.panel = None;
        self.rows.clear();
        let first = first_line(self.selected);
        let rows: Vec<(usize, Option<RowView>)> = self
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(LINES)
            .map(|(i, item)| (i, self.row_view(*item)))
            .collect();
        let shown = rows.len() as i32;
        let prompt_y = size.h - STRIP - LINE_H - 8;
        let top = prompt_y - shown * LINE_H;
        let c = &mut self.canvas;
        c.begin(renderer, scale);
        c.alpha = self.fade.value.clamp(0.0, 1.0) as f32;
        // Drawn first: it is the back of the stack.
        let band_top = (top - 8).max(0);
        c.rect(rect(0, band_top, size.w, size.h - band_top), 0.0, BAND, 0.0, LINE);
        c.rect(rect(0, band_top, size.w, 1), 0.0, [1.0, 1.0, 1.0, 0.07], 0.0, LINE);
        let prompt = "$ ";
        let prompt_w = c.text.measure(prompt, mono(GREY)).ceil() as i32;
        let text_x = MARGIN + prompt_w;
        let width = size.w - 2 * MARGIN;
        if self.query.is_empty() {
            c.styled_label(renderer, "type to search apps, windows and files", text_x + 8, prompt_y, width - prompt_w, mono(DIM));
        } else {
            c.styled_label(renderer, &self.query, text_x, prompt_y, width - prompt_w, mono(GREY));
        }
        if self.open {
            let typed = if self.query.is_empty() { 0.0 } else { c.text.measure(&self.query, mono(GREY)) };
            let caret = text_x + (typed.ceil() as i32).min(width - prompt_w);
            c.rect(rect(caret + 1, prompt_y + 1, 2, LINE_H - 3), 0.0, [accent[0], accent[1], accent[2], 1.0], 0.0, LINE);
        }
        c.styled_label(renderer, prompt, MARGIN, prompt_y, prompt_w + 4, mono(GREY));
        if self.items.is_empty() {
            let note = if self.loading { "loading applications..." } else { "no matches" };
            if !self.query.is_empty() || self.loading {
                c.styled_label(renderer, note, MARGIN + 2, prompt_y - LINE_H, width, mono(DIM));
            }
        }
        for (n, (index, view)) in rows.iter().enumerate() {
            let Some(view) = view else { continue };
            let y = top + n as i32 * LINE_H;
            let selected = *index == self.selected;
            let color = if view.error { style::DANGER } else if selected { [accent[0], accent[1], accent[2], 1.0] } else { GREY };
            let marker = if selected { "> " } else { "  " };
            let marker_w = c.text.measure(marker, mono(color)).ceil() as i32;
            c.styled_label(renderer, marker, MARGIN, y, marker_w + 4, mono(color));
            let title_w = (c.text.measure(&view.title, mono(color)).ceil() as i32).min(width - marker_w);
            c.styled_label(renderer, &view.title, MARGIN + marker_w, y, width - marker_w, mono(color));
            if !view.detail.is_empty() {
                let gap = c.text.measure("  ", mono(DIM)).ceil() as i32;
                let x = MARGIN + marker_w + title_w + gap;
                if x < size.w - MARGIN - 40 {
                    c.styled_label(renderer, &view.detail, x, y, size.w - MARGIN - x, mono(DIM));
                }
            }
        }
        c.finish()
    }
}
