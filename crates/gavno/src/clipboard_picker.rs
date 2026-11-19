//! The clipboard picker (Mod+V), drawn in the launcher's style: a search box
//! over a list of recent copies with text previews and image thumbnails.
//! Enter makes the compositor own the chosen entry and pastes it into the
//! focused window; Delete forgets an entry.
use crate::{
    clipboard::{Clipboard, Content, History},
    render::GavnoRenderElement,
    topbar::{geometry::{rect, Rect}, label_style, Canvas, LINE},
    ui::{
        spring::{Spring, SpringParams},
        text::Weight,
    },
    Gavno,
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    input::keyboard::{keysyms, Keysym, ModifiersState},
    output::Output,
    utils::{Logical, Point, Size},
};
use std::time::Duration;

/// Rows visible at once.
const ROWS: usize = 8;
const ROW_H: i32 = 52;
const SEARCH_H: i32 = 56;
const FOOT_H: i32 = 30;
const PAD: i32 = 8;
/// Thumbnail box inside a row, logical pixels.
const THUMB_W: u32 = 56;
const THUMB_H: u32 = 32;

pub struct Picker {
    pub open: bool,
    query: String,
    selected: usize,
    /// Entry ids matching the query, newest first.
    results: Vec<u64>,
    fade: Spring,
    output: Option<Output>,
    canvas: Canvas,
    /// Panel and row rectangles from the last frame, output-local.
    panel: Option<Rect>,
    rows: Vec<(Rect, usize)>,
}

impl Default for Picker {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            results: Vec::new(),
            fade: Spring::new(0.0, SpringParams::SNAPPY),
            output: None,
            canvas: Canvas::default(),
            panel: None,
            rows: Vec::new(),
        }
    }
}

impl Picker {
    pub fn is_animating(&self) -> bool {
        self.fade.is_active()
    }
    /// Whether anything is on screen (open, or still fading out).
    pub fn visible(&self) -> bool {
        self.open || self.fade.value > 0.004 || self.fade.is_active()
    }

    /// Recomputes the matches; every word of the query must occur in the entry.
    pub fn refilter(&mut self, history: &History) {
        let selected = self.results.get(self.selected).copied();
        let query = self.query.trim().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.results = history
            .entries()
            .iter()
            .filter(|e| words.iter().all(|w| e.haystack.contains(w)))
            .map(|e| e.id)
            .collect();
        self.selected = selected.and_then(|id| self.results.iter().position(|r| *r == id)).unwrap_or(0);
    }

    pub fn selected_id(&self) -> Option<u64> {
        self.results.get(self.selected).copied()
    }

    fn move_selection(&mut self, down: bool) {
        if self.results.is_empty() {
            return;
        }
        let n = self.results.len();
        self.selected = if down { (self.selected + 1) % n } else { (self.selected + n - 1) % n };
    }

    /// First visible result index, keeping the selection in view.
    fn scroll(&self) -> usize {
        self.selected.saturating_sub(ROWS - 1)
    }

    fn panel_rect(&self, size: Size<i32, Logical>) -> Rect {
        let w = 640.min(size.w - 32).max(200);
        let rows = self.results.len().clamp(1, ROWS) as i32;
        let h = SEARCH_H + PAD + rows * ROW_H + PAD + FOOT_H;
        rect((size.w - w) / 2, (size.h as f64 * 0.16) as i32, w, h)
    }

    fn show(&mut self, history: &History, output: Option<Output>) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.refilter(history);
        self.output = output;
        self.fade.set(1.0);
    }

    fn hide(&mut self) {
        self.open = false;
        self.fade.set(0.0);
    }

    /// Closes without a fade, e.g. before a capture.
    pub fn dismiss(&mut self) {
        self.open = false;
        self.fade.jump(0.0);
    }

    #[cfg(test)]
    pub(crate) fn results(&self) -> &[u64] {
        &self.results
    }
}

impl Clipboard {
    pub fn picker_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        accent: [f32; 4],
    ) -> Vec<GavnoRenderElement> {
        let Clipboard { history, picker: p, .. } = self;
        p.fade.advance(now);
        if !p.visible() || p.output.as_ref().is_some_and(|o| o != output) {
            return Vec::new();
        }
        let panel = p.panel_rect(size);
        p.panel = Some(panel);
        p.rows.clear();
        let fade = p.fade.value.clamp(0.0, 1.0);
        let first = p.scroll();
        let c = &mut p.canvas;
        c.begin(renderer, scale);
        c.alpha = fade as f32;
        c.zoom = 0.96 + 0.04 * fade;
        c.anchor = Point::from(((panel.loc.x + panel.size.w / 2) as f64, panel.loc.y as f64));
        c.shadow(panel, 18.0);
        c.rect(panel, 18.0, [0.065, 0.075, 0.10, 0.97], 1.0, LINE);

        // Search row.
        let (x, y) = (panel.loc.x, panel.loc.y);
        let mut style = label_style(16.0);
        let text_x = x + 24;
        let text_w = panel.size.w - 48;
        if p.query.is_empty() {
            style.color = [0.96, 0.95, 0.98, 0.45];
            c.styled_label(renderer, "Search clipboard history", text_x, y + 17, text_w, style);
        } else {
            c.styled_label(renderer, &p.query, text_x, y + 17, text_w, style);
        }
        if p.open {
            let typed = if p.query.is_empty() { 0.0 } else { c.text.measure(&p.query, style) };
            let caret_x = text_x + (typed.ceil() as i32).min(text_w);
            c.rect(rect(caret_x + 1, y + 17, 2, 22), 1.0, accent, 0.0, LINE);
        }
        c.rect(rect(x + 12, y + SEARCH_H, panel.size.w - 24, 1), 0.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);

        let mut dim = label_style(13.5);
        dim.color = [0.96, 0.95, 0.98, 0.5];
        if p.results.is_empty() {
            let text = if history.len() == 0 { "Nothing copied yet" } else { "No matching entries" };
            c.styled_label(renderer, text, x + 20, y + SEARCH_H + PAD + 14, panel.size.w - 40, dim);
        }
        for (row, &id) in p.results.iter().enumerate().skip(first).take(ROWS) {
            let Some(entry) = history.get(id) else { continue };
            let ry = y + SEARCH_H + PAD + (row - first) as i32 * ROW_H;
            let r = rect(x + PAD, ry, panel.size.w - 2 * PAD, ROW_H);
            if row == p.selected {
                let [a, b, d, _] = accent;
                c.rect(r, 12.0, [a, b, d, 0.22], 0.0, LINE);
            }
            let mut text_left = r.loc.x + 16;
            if let Content::Image(image) = &entry.content {
                let frame = rect(r.loc.x + 10, ry + 8, THUMB_W as i32 + 4, ROW_H - 16);
                c.rect(frame, 6.0, [1.0, 1.0, 1.0, 0.06], 1.0, LINE);
                if let Some(thumb) = &image.thumb {
                    let (w, h) = crate::clipboard::fit(thumb.width, thumb.height, THUMB_W, THUMB_H);
                    let at = rect(frame.loc.x + 2 + (THUMB_W as i32 - w as i32) / 2, frame.loc.y + (frame.size.h - h as i32) / 2, w as i32, h as i32);
                    c.image(renderer, &thumb.buffer, (thumb.width, thumb.height), at);
                }
                text_left = frame.loc.x + frame.size.w + 14;
            }
            let mut name = label_style(14.0);
            name.weight = Weight::SemiBold;
            let width = r.loc.x + r.size.w - text_left - 16;
            if entry.detail.is_empty() {
                c.styled_label(renderer, &entry.preview, text_left, ry + 16, width, name);
            } else {
                c.styled_label(renderer, &entry.preview, text_left, ry + 8, width, name);
                let mut sub = label_style(11.5);
                sub.color = [0.96, 0.95, 0.98, 0.55];
                c.styled_label(renderer, &entry.detail, text_left, ry + 28, width, sub);
            }
            p.rows.push((r, row));
        }

        // Footer.
        let rows = p.results.len().clamp(1, ROWS) as i32;
        let fy = y + SEARCH_H + PAD + rows * ROW_H + PAD;
        c.rect(rect(x + 12, fy, panel.size.w - 24, 1), 0.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);
        let mut foot = label_style(11.5);
        foot.color = [0.96, 0.95, 0.98, 0.5];
        c.styled_label(renderer, "Enter paste     Delete forget     Esc close", x + 20, fy + 8, panel.size.w - 40, foot);
        c.finish()
    }
}

impl Gavno {
    pub fn clipboard_picker_open(&self) -> bool {
        self.overlay.clipboard.picker.open
    }

    /// Opens the picker on the active output, or closes it when open.
    pub fn toggle_clipboard_picker(&mut self) {
        if self.overlay.clipboard.picker.open {
            self.close_clipboard_picker();
            return;
        }
        if !self.config.clipboard.history {
            return;
        }
        self.close_launcher();
        let output = self.active_output();
        let Clipboard { history, picker, .. } = &mut self.overlay.clipboard;
        picker.show(history, output);
        self.queue_redraw_all();
    }

    pub fn close_clipboard_picker(&mut self) {
        if self.overlay.clipboard.picker.open {
            self.overlay.clipboard.picker.hide();
            self.queue_redraw_all();
        }
    }

    /// Enter or a click: own the entry, then paste after the picker is gone.
    fn clipboard_choose(&mut self) {
        let Some(id) = self.overlay.clipboard.picker.selected_id() else {
            return self.close_clipboard_picker();
        };
        let paste = self.focused_window().is_some();
        self.overlay.clipboard.picker.hide();
        self.queue_redraw_all();
        // Selection and key injection touch the seat: never from the key filter.
        self.loop_handle.insert_idle(move |gv| gv.clipboard_apply(id, paste));
    }

    fn clipboard_apply(&mut self, id: u64, paste: bool) {
        let content = match self.overlay.clipboard.history.get(id).map(|e| &e.content) {
            Some(Content::Text(text)) => Ok(text.clone()),
            Some(Content::Image(image)) => Err(image.png.clone()),
            None => return,
        };
        match content {
            Ok(text) => self.own_text_selection(&text),
            Err(png) => self.own_png_selection(png),
        }
        self.overlay.clipboard.history.promote(id);
        if paste && !self.protocols.locks.locked && self.focused_window().is_some() {
            self.inject_paste();
        }
    }

    /// Keys while the picker is open. Every key is consumed.
    pub fn clipboard_key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        let mods = *mods;
        let Clipboard { history, picker: p, .. } = &mut self.overlay.clipboard;
        match sym.raw() {
            keysyms::KEY_Escape => p.hide(),
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => return self.clipboard_choose(),
            keysyms::KEY_Down | keysyms::KEY_Tab => p.move_selection(true),
            keysyms::KEY_Up | keysyms::KEY_ISO_Left_Tab => p.move_selection(false),
            keysyms::KEY_n if mods.ctrl => p.move_selection(true),
            keysyms::KEY_p if mods.ctrl => p.move_selection(false),
            keysyms::KEY_v | keysyms::KEY_V if mods.logo => p.hide(),
            keysyms::KEY_Delete | keysyms::KEY_KP_Delete => {
                if let Some(id) = p.selected_id() {
                    history.remove(id);
                    let keep = p.selected;
                    p.refilter(history);
                    p.selected = keep.min(p.results.len().saturating_sub(1));
                }
            }
            keysyms::KEY_BackSpace | keysyms::KEY_u if mods.ctrl => {
                p.query.clear();
                p.refilter(history);
            }
            keysyms::KEY_BackSpace => {
                p.query.pop();
                p.refilter(history);
            }
            _ => match ch {
                Some(c) if !c.is_control() && !mods.ctrl && !mods.logo && !mods.alt => {
                    p.query.push(c);
                    p.selected = 0;
                    p.refilter(history);
                    p.selected = 0;
                }
                _ => {}
            },
        }
        self.queue_redraw_all();
    }

    /// Wheel over the picker moves the selection. Returns true when consumed.
    pub fn clipboard_axis(&mut self, vertical: f64) -> bool {
        if !self.overlay.clipboard.picker.open {
            return false;
        }
        if vertical != 0.0 {
            self.overlay.clipboard.picker.move_selection(vertical > 0.0);
            self.queue_redraw_all();
        }
        true
    }

    /// Pointer buttons while the picker is open. Returns true when consumed.
    pub fn clipboard_button(&mut self, pos: Point<f64, Logical>, pressed: bool) -> bool {
        if !self.overlay.clipboard.picker.open {
            return false;
        }
        let Some(local) = self.clipboard_local(pos) else {
            if pressed {
                self.close_clipboard_picker();
            }
            return true;
        };
        if !pressed {
            return true;
        }
        let p = &mut self.overlay.clipboard.picker;
        let hit = p.rows.iter().find(|(r, _)| r.to_f64().contains(local)).map(|(_, i)| *i);
        if let Some(row) = hit {
            p.selected = row;
            self.clipboard_choose();
        } else if !p.panel.is_some_and(|panel| panel.to_f64().contains(local)) {
            self.close_clipboard_picker();
        }
        true
    }

    /// Hover selects the row under the pointer.
    pub fn clipboard_motion(&mut self, pos: Point<f64, Logical>) {
        if !self.overlay.clipboard.picker.open {
            return;
        }
        let Some(local) = self.clipboard_local(pos) else { return };
        let p = &mut self.overlay.clipboard.picker;
        let hit = p.rows.iter().find(|(r, _)| r.to_f64().contains(local)).map(|(_, i)| *i);
        if let Some(row) = hit.filter(|r| *r != p.selected) {
            p.selected = row;
            self.queue_redraw_all();
        }
    }

    fn clipboard_local(&self, pos: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
        let output = self.overlay.clipboard.picker.output.as_ref()?;
        let geo = self.space.output_geometry(output)?;
        geo.to_f64().contains(pos).then(|| pos - geo.loc.to_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(texts: &[&str]) -> History {
        let mut h = History::with_limit(50);
        for t in texts.iter().rev() {
            h.push_text((*t).into());
        }
        h
    }

    fn picker(h: &History) -> Picker {
        let mut p = Picker::default();
        p.show(h, None);
        p
    }

    #[test]
    fn filtering_requires_every_word_and_ignores_case() {
        let h = history(&["Hello World", "hello there", "Goodbye World"]);
        let mut p = picker(&h);
        assert_eq!(p.results().len(), 3);
        p.query = "WORLD".into();
        p.refilter(&h);
        assert_eq!(p.results().len(), 2);
        p.query = "hello world".into();
        p.refilter(&h);
        assert_eq!(p.results().len(), 1);
        p.query = "zzz".into();
        p.refilter(&h);
        assert!(p.results().is_empty() && p.selected_id().is_none());
        // Newest first: the first pushed text ends up last.
        p.query.clear();
        p.refilter(&h);
        assert_eq!(h.get(p.results()[0]).unwrap().preview, "Hello World");
    }

    #[test]
    fn selection_wraps_scrolls_and_survives_refilter() {
        let texts: Vec<String> = (0..12).map(|i| format!("entry {i:02}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let h = history(&refs);
        let mut p = picker(&h);
        p.move_selection(false);
        assert_eq!(p.selected, 11);
        assert_eq!(p.scroll(), 11 - (ROWS - 1));
        p.move_selection(true);
        assert_eq!(p.selected, 0);
        for _ in 0..3 { p.move_selection(true); }
        let id = p.selected_id().unwrap();
        // The same entry stays selected when the list shrinks around it.
        p.query = "entry 0".into();
        p.refilter(&h);
        assert_eq!(p.selected_id(), Some(id));
    }

    #[test]
    fn panel_grows_with_results_and_stays_on_screen() {
        let h = history(&["a", "b"]);
        let p = picker(&h);
        let panel = p.panel_rect((1646, 926).into());
        assert_eq!(panel.size.h, SEARCH_H + PAD + 2 * ROW_H + PAD + FOOT_H);
        let narrow = p.panel_rect((400, 300).into());
        assert!(narrow.loc.x >= 0 && narrow.loc.x + narrow.size.w <= 400);
    }

    #[test]
    fn delete_forgets_the_selected_entry_and_keeps_position() {
        let mut h = history(&["one", "two", "three"]);
        let mut p = picker(&h);
        p.move_selection(true);
        let id = p.selected_id().unwrap();
        assert_eq!(h.get(id).unwrap().preview, "two");
        h.remove(id);
        let keep = p.selected;
        p.refilter(&h);
        p.selected = keep.min(p.results.len().saturating_sub(1));
        assert_eq!(h.get(p.selected_id().unwrap()).unwrap().preview, "three");
    }

    #[test]
    fn keys_choose_filter_and_close_through_the_compositor() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.overlay.clipboard.history.push_text("alpha".into());
        h.gv.overlay.clipboard.history.push_text("beta".into());
        h.gv.toggle_clipboard_picker();
        assert!(h.gv.clipboard_picker_open());
        let none = ModifiersState::default();
        h.gv.clipboard_key(Keysym::from(keysyms::KEY_a), Some('a'), &none);
        h.gv.clipboard_key(Keysym::from(keysyms::KEY_l), Some('l'), &none);
        assert_eq!(h.gv.overlay.clipboard.picker.results().len(), 1);
        h.gv.clipboard_key(Keysym::from(keysyms::KEY_BackSpace), None, &none);
        h.gv.clipboard_key(Keysym::from(keysyms::KEY_BackSpace), None, &none);
        assert_eq!(h.gv.overlay.clipboard.picker.results().len(), 2);
        h.gv.clipboard_key(Keysym::from(keysyms::KEY_Delete), None, &none);
        assert_eq!(h.gv.overlay.clipboard.history.len(), 1);
        h.gv.clipboard_key(Keysym::from(keysyms::KEY_Escape), None, &none);
        assert!(!h.gv.clipboard_picker_open());
        // History off: the picker refuses to open.
        h.gv.config.clipboard.history = false;
        h.gv.toggle_clipboard_picker();
        assert!(!h.gv.clipboard_picker_open());
    }
}
