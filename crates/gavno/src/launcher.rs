//! Built-in application launcher: Super+D (or a Super tap when the overview is
//! off) opens a centred search panel over the desktop. Typing filters the
//! installed desktop entries, Enter or a click launches, Esc closes.
use crate::{
    desktop_entry::{self, DesktopEntry},
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{geometry::{rect, Rect}, label_style, Canvas, FULL_COLOR, LINE},
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
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{Logical, Point, Size},
};
use std::time::{Duration, Instant};

/// Rows visible at once.
const ROWS: usize = 8;
const ROW_H: i32 = 48;
const SEARCH_H: i32 = 56;
const PAD: i32 = 8;

struct App {
    entry: DesktopEntry,
    name: String,
    words: String,
}

pub struct Launcher {
    pub open: bool,
    pub run_style: bool,
    loading: bool,
    launch_after_load: bool,
    launch_action: Option<Instant>,
    query: String,
    selected: usize,
    apps: Option<Vec<App>>,
    results: Vec<usize>,
    fade: Spring,
    output: Option<Output>,
    canvas: Canvas,
    icons: Icons,
    /// Panel and row rectangles from the last frame, output-local.
    panel: Option<Rect>,
    rows: Vec<(Rect, usize)>,
    /// Held key being repeated: keycode and its repeat timer.
    repeat: Option<(u32, RegistrationToken)>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            open: false,
            run_style: false,
            loading: false,
            launch_after_load: false,
            launch_action: None,
            query: String::new(),
            selected: 0,
            apps: None,
            results: Vec::new(),
            fade: Spring::new(0.0, SpringParams::SNAPPY),
            output: None,
            canvas: Canvas::default(),
            icons: Icons::default(),
            panel: None,
            rows: Vec::new(),
            repeat: None,
        }
    }
}

/// Rank of `app` for a lower-case query: lower is better, `None` hides it.
pub(crate) fn score(name: &str, words: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    if name.starts_with(query) {
        Some(0)
    } else if name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(query)) {
        Some(1)
    } else if name.contains(query) {
        Some(2)
    } else if words.contains(query) {
        Some(3)
    } else {
        None
    }
}

impl Launcher {
    pub fn is_animating(&self) -> bool {
        self.fade.is_active()
    }
    /// Whether anything is on screen (open, or still fading out).
    pub fn visible(&self) -> bool {
        self.open || self.fade.value > 0.004 || self.fade.is_active()
    }

    fn install_entries(&mut self, entries: Vec<DesktopEntry>) {
        self.loading = false;
        let mut seen = std::collections::HashSet::new();
        let mut apps: Vec<App> = entries.into_iter().take(4096)
            .filter(|e| !e.hidden_from_menus && !e.name.is_empty())
            // Several data dirs may ship the same app; keep the first by name.
            .filter(|e| seen.insert(e.name.to_lowercase()))
            .map(|entry| App {
                name: entry.name.to_lowercase(),
                words: format!(
                    "{} {} {} {} {}",
                    entry.generic_name, entry.keywords, entry.comment, entry.id, entry.exec
                )
                .to_lowercase(),
                entry,
            })
            .collect();
        apps.sort_by(|a, b| a.name.cmp(&b.name));
        self.apps = Some(apps);
    }

    fn refilter(&mut self) {
        let query = self.query.trim().to_lowercase();
        let apps = self.apps.as_deref().unwrap_or_default();
        let mut ranked: Vec<(u8, usize)> = apps
            .iter()
            .enumerate()
            .filter_map(|(i, a)| score(&a.name, &a.words, &query).map(|s| (s, i)))
            .collect();
        // Apps are already sorted by name, so a stable sort keeps that order
        // within each rank.
        ranked.sort_by_key(|(s, _)| *s);
        self.results = ranked.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }

    fn show(&mut self, output: Option<Output>) {
        self.open = true;
        self.run_style = false;
        self.launch_after_load = false;
        self.launch_action = None;
        self.query.clear();
        self.refilter();
        self.output = output;
        self.fade.set(1.0);
    }

    fn hide(&mut self) {
        self.open = false;
        self.launch_after_load = false;
        self.launch_action = None;
        self.fade.set(0.0);
    }

    fn selected_entry(&self) -> Option<&DesktopEntry> {
        let i = *self.results.get(self.selected)?;
        Some(&self.apps.as_ref()?.get(i)?.entry)
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

    /// Panel rectangle for an output of `size` logical pixels.
    fn panel_rect(&self, size: Size<i32, Logical>) -> Rect {
        let w = 640.min(size.w - 32).max(200);
        let rows = if self.run_style { 0 } else { self.results.len().clamp(1, ROWS) as i32 };
        let h = SEARCH_H + PAD + rows * ROW_H + PAD;
        rect((size.w - w) / 2, (size.h as f64 * 0.16) as i32, w, h)
    }

    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        accent: [f32; 4],
        icon_theme: &str,
    ) -> Vec<GavnoRenderElement> {
        self.fade.advance(now);
        if !self.visible() || self.output.as_ref().is_some_and(|o| o != output) {
            return Vec::new();
        }
        let panel = self.panel_rect(size);
        self.panel = Some(panel);
        self.rows.clear();
        let fade = self.fade.value.clamp(0.0, 1.0);
        let first = self.scroll();
        let c = &mut self.canvas;
        c.begin(renderer, scale);
        c.alpha = fade as f32;
        c.zoom = 0.96 + 0.04 * fade;
        c.anchor = Point::from((
            (panel.loc.x + panel.size.w / 2) as f64,
            panel.loc.y as f64,
        ));
        c.shadow(panel, 18.0);
        c.rect(panel, 18.0, [0.065, 0.075, 0.10, 0.97], 1.0, LINE);

        // Search row.
        let x = panel.loc.x;
        let y = panel.loc.y;
        c.icon(renderer, &mut self.icons, &["system-search-symbolic", "edit-find-symbolic"], icon_theme, rect(x + 20, y + 18, 20, 20));
        let mut style = label_style(16.0);
        let text_x = x + 52;
        let text_w = panel.size.w - 52 - 20;
        if self.query.is_empty() {
            style.color = [0.96, 0.95, 0.98, 0.45];
            c.styled_label(renderer, if self.run_style { "Run a command" } else { "Search applications" }, text_x, y + 17, text_w, style);
        } else {
            c.styled_label(renderer, &self.query, text_x, y + 17, text_w, style);
        }
        if self.open {
            let typed = if self.query.is_empty() { 0.0 } else { c.text.measure(&self.query, style) };
            let caret_x = text_x + (typed.ceil() as i32).min(text_w);
            c.rect(rect(caret_x + 1, y + 17, 2, 22), 1.0, accent, 0.0, LINE);
        }
        c.rect(rect(x + 12, y + SEARCH_H, panel.size.w - 24, 1), 0.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);

        // Results.
        let apps = self.apps.as_deref().unwrap_or_default();
        if self.results.is_empty() && !self.run_style {
            let mut dim = label_style(13.5);
            dim.color = [0.96, 0.95, 0.98, 0.5];
            c.styled_label(renderer, if self.loading { "Loading applications…" } else { "No applications found" }, x + 20, y + SEARCH_H + PAD + 14, panel.size.w - 40, dim);
        }
        for (row, &i) in self.results.iter().enumerate().skip(first).take(if self.run_style { 0 } else { ROWS }) {
            let Some(app) = apps.get(i) else { continue };
            let ry = y + SEARCH_H + PAD + (row - first) as i32 * ROW_H;
            let r = rect(x + PAD, ry, panel.size.w - 2 * PAD, ROW_H);
            if row == self.selected {
                let [a, b, d, _] = accent;
                c.rect(r, 12.0, [a, b, d, 0.22], 0.0, LINE);
            }
            let icon = rect(r.loc.x + 12, ry + 10, 28, 28);
            if !c.colored_icon(renderer, &mut self.icons, &[&app.entry.icon], icon_theme, icon, Some(FULL_COLOR)) {
                c.icon(renderer, &mut self.icons, &["application-x-executable-symbolic"], icon_theme, rect(r.loc.x + 16, ry + 14, 20, 20));
            }
            let mut name = label_style(14.0);
            name.weight = Weight::SemiBold;
            let detail = if app.entry.generic_name.is_empty() { &app.entry.comment } else { &app.entry.generic_name };
            if detail.is_empty() {
                c.styled_label(renderer, &app.entry.name, r.loc.x + 52, ry + 14, r.size.w - 64, name);
            } else {
                c.styled_label(renderer, &app.entry.name, r.loc.x + 52, ry + 6, r.size.w - 64, name);
                let mut sub = label_style(11.5);
                sub.color = [0.96, 0.95, 0.98, 0.55];
                c.styled_label(renderer, detail, r.loc.x + 52, ry + 26, r.size.w - 64, sub);
            }
            self.rows.push((r, row));
        }
        c.finish()
    }
}

impl Gavno {
    pub fn toggle_run_launcher(&mut self) {
        self.guard(crate::module::Feature::Launcher, (), |gv| {
            if gv.launcher.open && gv.launcher.run_style { gv.launcher.hide(); }
            else {
                gv.launcher.open = true;
                gv.launcher.run_style = true;
                gv.launcher.output = gv.active_output();
                gv.launcher.query.clear(); gv.launcher.results.clear(); gv.launcher.selected=0;
                gv.launcher.fade.set(1.0);
            }
            gv.queue_redraw_all();
        });
    }
    /// Opens the launcher on the active output, or closes it when open.
    pub fn toggle_launcher(&mut self) {
        self.guard(crate::module::Feature::Launcher, (), |gv| {
            if gv.launcher.open {
                gv.launcher.hide();
            } else {
                let output = gv.active_output();
                gv.launcher.show(output);
                if !gv.launcher.loading && gv.launcher.apps.as_ref().is_none_or(|apps| apps.is_empty()) {
                    gv.launcher.loading = true;
                    let (tx,rx)=smithay::reexports::calloop::channel::channel();
                    let result=gv.loop_handle.insert_source(rx,|event,_,gv|{
                        let _stall = crate::perf::Stall::new("launcher.apps_reply");
                        if let smithay::reexports::calloop::channel::Event::Msg(entries)=event {
                            gv.guard(crate::module::Feature::Launcher,(),|gv|{
                                gv.launcher.install_entries(entries);
                                if !gv.launcher.run_style { gv.launcher.refilter(); }
                                if gv.launcher.open && !gv.launcher.run_style && gv.launcher.launch_after_load { gv.launch_selected(); }
                                else if gv.launcher.open { gv.queue_redraw_all(); }
                            });
                        }
                    });
                    if result.is_ok() { std::thread::spawn(move||{let _=tx.send(desktop_entry::load_all());}); }
                    else {gv.launcher.loading=false;}
                }
            }
            gv.queue_redraw_all();
        });
    }

    pub fn close_launcher(&mut self) {
        if self.launcher.open {
            self.launcher.hide();
            self.queue_redraw_all();
        }
    }

    fn launch_selected(&mut self) {
        let action = *self.launcher.launch_action.get_or_insert_with(Instant::now);
        if !self.launcher.run_style && self.launcher.loading && self.launcher.results.is_empty() {
            self.launcher.launch_after_load=true;
            return;
        }
        let app_id = self.launcher.selected_entry().map(|e| e.id.clone());
        let command = if self.launcher.run_style {
            Some(self.launcher.query.trim().to_owned()).filter(|s| !s.is_empty())
        } else { self.launcher.selected_entry().and_then(|e| e.command(&self.config.terminal)) };
        self.launcher.hide();
        if let Some(command) = command {
            self.launch_shell_at(action, &command, app_id.as_deref());
        }
        self.queue_redraw_all();
    }

    /// Keys while the launcher is open. Every key is consumed.
    pub fn launcher_key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        let mods = *mods;
        self.guard(crate::module::Feature::Launcher, (), |gv| {
            let l = &mut gv.launcher;
            if !matches!(sym.raw(), keysyms::KEY_Return | keysyms::KEY_KP_Enter) { l.launch_after_load=false; l.launch_action=None; }
            match sym.raw() {
                keysyms::KEY_Escape => l.hide(),
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => return gv.launch_selected(),
                keysyms::KEY_Down | keysyms::KEY_Tab => l.move_selection(true),
                keysyms::KEY_Up | keysyms::KEY_ISO_Left_Tab => l.move_selection(false),
                keysyms::KEY_n if mods.ctrl => l.move_selection(true),
                keysyms::KEY_p if mods.ctrl => l.move_selection(false),
                keysyms::KEY_d | keysyms::KEY_D if mods.logo => l.hide(),
                keysyms::KEY_BackSpace if mods.ctrl => {
                    l.query.clear();
                    l.refilter();
                }
                keysyms::KEY_u if mods.ctrl => {
                    l.query.clear();
                    l.refilter();
                }
                keysyms::KEY_BackSpace => {
                    l.query.pop();
                    l.refilter();
                }
                _ => match ch {
                    Some(c) if !c.is_control() && !mods.ctrl && !mods.logo && !mods.alt => {
                        l.query.push(c);
                        l.refilter();
                    }
                    _ => {}
                },
            }
            gv.queue_redraw_all();
        });
    }

    /// A key press while the launcher is open. Navigation, Backspace and
    /// typed characters repeat while held, at the keyboard repeat rate:
    /// compositor-consumed keys never reach a client that would repeat them.
    pub fn launcher_key_press(&mut self, code: u32, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        self.cancel_launcher_repeat();
        self.launcher_key(sym, ch, mods);
        let repeats = !matches!(
            sym.raw(),
            keysyms::KEY_Escape | keysyms::KEY_Return | keysyms::KEY_KP_Enter
        ) && !mods.logo;
        if !self.launcher.open || !repeats || self.config.repeat_rate <= 0 {
            return;
        }
        let mods = *mods;
        let delay = Duration::from_millis(self.config.repeat_delay.max(0) as u64);
        let timer = self.loop_handle.insert_source(Timer::from_duration(delay), move |_, _, gv| {
            let _stall = crate::perf::Stall::new("timer.launcher_repeat");
            let held = gv.launcher.repeat.as_ref().is_some_and(|(key, _)| *key == code);
            if !held || !gv.launcher.open || gv.config.repeat_rate <= 0 {
                gv.launcher.repeat = None;
                return TimeoutAction::Drop;
            }
            gv.launcher_key(sym, ch, &mods);
            TimeoutAction::ToDuration(Duration::from_secs_f64(1.0 / gv.config.repeat_rate as f64))
        });
        match timer {
            Ok(token) => self.launcher.repeat = Some((code, token)),
            Err(error) => tracing::warn!(%error, "cannot schedule launcher key repeat"),
        }
    }

    /// Stops the repeat when its key is released.
    pub fn launcher_key_release(&mut self, code: u32) {
        if self.launcher.repeat.as_ref().is_some_and(|(key, _)| *key == code) {
            self.cancel_launcher_repeat();
        }
    }

    fn cancel_launcher_repeat(&mut self) {
        if let Some((_, token)) = self.launcher.repeat.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Pointer buttons while the launcher is open: a row launches, a press
    /// outside the panel closes it. Returns true when the button was consumed.
    pub fn launcher_button(&mut self, pos: Point<f64, Logical>, pressed: bool) -> bool {
        if !self.launcher.open {
            return false;
        }
        self.guard(crate::module::Feature::Launcher, false, |gv| {
            let Some(local) = gv.launcher_local(pos) else {
                if pressed { gv.close_launcher(); }
                return true;
            };
            if !pressed {
                return true;
            }
            let hit = gv.launcher.rows.iter().find(|(r, _)| r.to_f64().contains(local)).map(|(_, i)| *i);
            if let Some(row) = hit {
                gv.launcher.selected = row;
                gv.launch_selected();
            } else if !gv.launcher.panel.is_some_and(|p| p.to_f64().contains(local)) {
                gv.close_launcher();
            }
            true
        })
    }

    /// Hover selects the row under the pointer.
    pub fn launcher_motion(&mut self, pos: Point<f64, Logical>) {
        if !self.launcher.open {
            return;
        }
        self.guard(crate::module::Feature::Launcher, (), |gv| {
            let Some(local) = gv.launcher_local(pos) else { return };
            let hit = gv.launcher.rows.iter().find(|(r, _)| r.to_f64().contains(local)).map(|(_, i)| *i);
            if let Some(row) = hit.filter(|r| *r != gv.launcher.selected) {
                gv.launcher.selected = row;
                gv.queue_redraw_all();
            }
        });
    }

    /// `pos` relative to the launcher's output, if the pointer is on it.
    fn launcher_local(&self, pos: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
        let output = self.launcher.output.as_ref()?;
        let geo = self.space.output_geometry(output)?;
        geo.to_f64().contains(pos).then(|| pos - geo.loc.to_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking_prefers_name_prefix_then_word_then_substring_then_keywords() {
        assert_eq!(score("firefox", "web browser", "fire"), Some(0));
        assert_eq!(score("gnome terminal", "", "term"), Some(1));
        assert_eq!(score("libreoffice writer", "", "office"), Some(2));
        assert_eq!(score("foot", "terminal emulator", "termi"), Some(3));
        assert_eq!(score("foot", "terminal", "zzz"), None);
        assert_eq!(score("anything", "", ""), Some(0));
    }

    fn launcher_with(names: &[(&str, &str)]) -> Launcher {
        let mut l = Launcher::default();
        l.apps = Some(
            names
                .iter()
                .map(|(name, generic)| {
                    let text = format!("[Desktop Entry]\nType=Application\nName={name}\nGenericName={generic}\nExec={name}\n");
                    let entry = DesktopEntry::parse(name, &text, None).unwrap();
                    App { name: name.to_lowercase(), words: generic.to_lowercase(), entry }
                })
                .collect(),
        );
        l
    }

    #[test]
    fn typing_filters_and_selection_wraps() {
        let mut l = launcher_with(&[("Files", "File Manager"), ("Firefox", "Web Browser"), ("foot", "Terminal")]);
        l.query = "fi".into();
        l.refilter();
        let names: Vec<_> = l.results.iter().map(|&i| l.apps.as_ref().unwrap()[i].entry.name.clone()).collect();
        assert_eq!(names, ["Files", "Firefox"]);
        l.move_selection(false);
        assert_eq!(l.selected, 1);
        l.move_selection(true);
        assert_eq!(l.selected, 0);
        l.query = "browser".into();
        l.refilter();
        assert_eq!(l.selected_entry().unwrap().name, "Firefox");
    }

    #[test]
    fn hidden_entries_are_skipped_by_the_parser_flag() {
        let e = DesktopEntry::parse("x", "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n", None).unwrap();
        assert!(e.hidden_from_menus);
        let gnome = DesktopEntry::parse("y", "[Desktop Entry]\nType=Application\nName=Y\nExec=y\nOnlyShowIn=GNOME;\n", None).unwrap();
        assert!(gnome.hidden_from_menus);
        let ours = DesktopEntry::parse("z", "[Desktop Entry]\nType=Application\nName=Z\nExec=z\nOnlyShowIn=GNOME;GAVNO;\n", None).unwrap();
        assert!(!ours.hidden_from_menus);
    }

    #[test]
    fn panel_grows_with_results_and_stays_on_screen() {
        let mut l = launcher_with(&[("A", ""), ("B", "")]);
        l.refilter();
        let small = l.panel_rect((1646, 926).into());
        assert_eq!(small.size.h, SEARCH_H + PAD + 2 * ROW_H + PAD);
        assert_eq!(small.loc.x, (1646 - 640) / 2);
        let narrow = l.panel_rect((400, 300).into());
        assert!(narrow.loc.x >= 0 && narrow.loc.x + narrow.size.w <= 400);
    }

    #[test]
    fn scroll_keeps_selection_visible() {
        let names: Vec<(String, String)> = (0..20).map(|i| (format!("App{i:02}"), String::new())).collect();
        let refs: Vec<(&str, &str)> = names.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let mut l = launcher_with(&refs);
        l.refilter();
        for _ in 0..10 { l.move_selection(true); }
        assert_eq!(l.selected, 10);
        assert_eq!(l.scroll(), 10 - (ROWS - 1));
    }
}

#[cfg(test)]
mod asynchronous_tests {
    use super::*;
    #[test]
    fn loading_preserves_query_and_escape_cancels_a_queued_launch() {
        let mut h=crate::state::tests::Harness::new();h.gv.launcher.open=true;h.gv.launcher.loading=true;h.gv.launcher.query="term".into();h.gv.launch_selected();assert!(h.gv.launcher.open&&h.gv.launcher.launch_after_load);
        h.gv.launcher.install_entries(vec![DesktopEntry::parse("foot","[Desktop Entry]\nName=Terminal\nExec=foot\n",None).unwrap()]);h.gv.launcher.refilter();assert_eq!(h.gv.launcher.selected_entry().unwrap().id,"foot");assert_eq!(h.gv.launcher.query,"term");h.gv.close_launcher();assert!(!h.gv.launcher.launch_after_load);
        h.gv.launcher=Launcher::default();h.gv.toggle_run_launcher();assert!(h.gv.launcher.apps.is_none());assert!(!h.gv.launcher.loading);
    }
}
