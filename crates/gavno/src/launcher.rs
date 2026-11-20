//! Built-in application launcher: Super+D (or a Super tap when the overview is
//! off) opens a centred search panel over the desktop. Typing filters the
//! installed desktop entries, open windows and file names, and recognises
//! calculations and unit conversions; Enter or a click activates the selected
//! row, Esc closes.
//!
//! Layout: with an empty query the most used apps sit on top as large tiles
//! (the "Recent" section) above the full app list. With a query the sections
//! are Calculator, Apps, Windows, Files and AI, each with a label when more
//! than one is shown. Tab jumps to the next section.
mod calc;
mod files;
mod frecency;
mod tty;

use crate::{
    desktop_entry::{self, DesktopEntry},
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{geometry::{rect, Rect}, Canvas, FULL_COLOR, LINE},
    ui::{
        style::{self, radius, size, space, TEXT},
        text::Weight,
        spring::{Spring, SpringParams},
    },
    Gavno,
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    desktop::Window,
    input::keyboard::{keysyms, Keysym, ModifiersState},
    output::Output,
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{IsAlive, Logical, Point, Rectangle, Size},
};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

/// Rows visible at once; also the height budget of the list.
const ROWS: usize = 8;
const ROW_H: i32 = 48;
const HEADER_H: i32 = 24;
const TILES_H: i32 = 88;
const SEARCH_H: i32 = 56;
const PAD: i32 = 8;
const ICON: i32 = 32;
/// Large tiles on top of the empty launcher.
const TILE_COUNT: usize = 6;
const MAX_WINDOW_ROWS: usize = 5;
const MAX_FILE_ROWS: usize = 5;
const MAX_WINDOWS: usize = 64;

struct App {
    entry: DesktopEntry,
    name: String,
    words: String,
}

/// An open window, captured when the launcher opens.
struct WindowRow {
    window: Window,
    title: String,
    app_id: String,
    lower: String,
    workspace: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Recent,
    Calculator,
    Apps,
    Windows,
    Files,
    Ai,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Recent => "RECENT",
            Section::Calculator => "CALCULATOR",
            Section::Apps => "APPS",
            Section::Windows => "WINDOWS",
            Section::Files => "FILES",
            Section::Ai => "AI",
        }
    }
}

/// One selectable row; indexes point into the launcher's own vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    /// A recent app tile (app index).
    Tile(usize),
    App(usize),
    /// Index into `windows`.
    Window(usize),
    /// Index into `files`.
    File(usize),
    Calc,
    Ask,
}

impl Item {
    fn section(self) -> Section {
        match self {
            Item::Tile(_) => Section::Recent,
            Item::App(_) => Section::Apps,
            Item::Window(_) => Section::Windows,
            Item::File(_) => Section::Files,
            Item::Calc => Section::Calculator,
            Item::Ask => Section::Ai,
        }
    }
}

/// One drawn line of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Line {
    Header(Section),
    Row(usize),
    /// A row of tiles: item range.
    Tiles(std::ops::Range<usize>),
}

impl Line {
    fn height(&self) -> i32 {
        match self {
            Line::Header(_) => HEADER_H,
            Line::Row(_) => ROW_H,
            Line::Tiles(_) => TILES_H,
        }
    }
}

/// What the row of an item shows.
enum Glyph {
    /// An application icon by name, with a generic fallback.
    App(String),
    Symbolic(&'static [&'static str]),
}

struct RowView {
    glyph: Glyph,
    title: String,
    detail: String,
    error: bool,
}

pub struct Launcher {
    pub open: bool,
    pub run_style: bool,
    /// TTY mode draws the launcher as a console command line.
    pub(crate) tty: bool,
    loading: bool,
    launch_after_load: bool,
    launch_action: Option<Instant>,
    query: String,
    selected: usize,
    apps: Option<Vec<App>>,
    /// Application indices matching the query, best first.
    results: Vec<usize>,
    /// Every selectable row, in display order.
    items: Vec<Item>,
    /// `items` grouped under section headers.
    lines: Vec<Line>,
    /// Recent apps shown as tiles for the empty query.
    tiles: Vec<usize>,
    frecency: frecency::Frecency,
    frecency_path: Option<PathBuf>,
    windows: Vec<WindowRow>,
    /// Indices into `windows` matching the query.
    window_hits: Vec<usize>,
    files: Vec<files::Hit>,
    files_seq: u64,
    files_tx: Option<mpsc::Sender<files::Request>>,
    calc: Option<calc::Answer>,
    fade: Spring,
    output: Option<Output>,
    canvas: Canvas,
    icons: Icons,
    /// Panel and row rectangles from the last frame, output-local.
    panel: Option<Rect>,
    rows: Vec<(Rect, usize)>,
    /// Held key being repeated: keycode and its repeat timer.
    repeat: Option<(u32, RegistrationToken)>,
    /// `ai.enabled`, copied when the launcher opens.
    pub(crate) ai_enabled: bool,
    pub(crate) ai_phase: crate::ai::Phase,
    /// The query the AI row was asked about; editing it drops the answer.
    ai_query: String,
    /// Best application rank for the current query.
    best_score: Option<u8>,
    /// One prewarm per opening of the launcher.
    pub(crate) prewarm_sent: bool,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            open: false,
            run_style: false,
            tty: false,
            loading: false,
            launch_after_load: false,
            launch_action: None,
            query: String::new(),
            selected: 0,
            apps: None,
            results: Vec::new(),
            items: Vec::new(),
            lines: Vec::new(),
            tiles: Vec::new(),
            frecency: Default::default(),
            frecency_path: None,
            windows: Vec::new(),
            window_hits: Vec::new(),
            files: Vec::new(),
            files_seq: 0,
            files_tx: None,
            calc: None,
            fade: Spring::new(0.0, SpringParams::SNAPPY),
            output: None,
            canvas: Canvas::default(),
            icons: Icons::nonblocking(crate::icons::app_worker::Consumer::Launcher),
            panel: None,
            rows: Vec::new(),
            repeat: None,
            ai_enabled: false,
            ai_phase: Default::default(),
            ai_query: String::new(),
            best_score: None,
            prewarm_sent: false,
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

/// Whether `lower` (window title and app id) contains every word of `query`.
fn window_matches(lower: &str, query: &str) -> bool {
    !query.is_empty() && query.split_whitespace().all(|w| lower.contains(w))
}

/// `/home/me/docs` as `~/docs`.
fn tilde(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => format!("~{}", &text[home.len()..]),
        _ => text.into_owned(),
    }
}

#[cfg(test)]
impl Launcher {
    pub(crate) fn test_apps(&mut self, entries: Vec<DesktopEntry>) {
        self.install_entries(entries);
    }
    pub(crate) fn test_query(&mut self, query: &str) {
        self.query = query.into();
        self.refilter();
    }
    pub(crate) fn test_results(&self) -> usize {
        self.results.len()
    }
    fn ask_selected_for_test(&self) -> bool {
        self.items.get(self.selected) == Some(&Item::Ask)
    }
}

impl Launcher {
    #[cfg(test)]
    pub(crate) fn repeating_key(&self) -> Option<u32> {
        self.repeat.as_ref().map(|(key, _)| *key)
    }

    pub(crate) fn icon_ready(&mut self) -> Option<Output> {
        self.visible().then(|| self.output.clone()).flatten()
    }
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
        let now = frecency::now_secs();
        let apps = self.apps.as_deref().unwrap_or_default();
        let mut ranked: Vec<(u8, f64, usize)> = apps
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                score(&a.name, &a.words, &query).map(|s| (s, self.frecency.score(&a.entry.id, now), i))
            })
            .collect();
        // Apps are already sorted by name, so a stable sort keeps that order
        // among equals; a better rank wins, then the more used app.
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));
        self.best_score = ranked.first().map(|(s, _, _)| *s);
        self.results = ranked.into_iter().map(|(_, _, i)| i).collect();
        self.tiles = if query.is_empty() && !self.run_style {
            self.frecency
                .top(TILE_COUNT, now)
                .into_iter()
                .filter_map(|id| apps.iter().position(|a| a.entry.id == id))
                .collect()
        } else {
            Vec::new()
        };
        self.calc = if self.run_style { None } else { calc::evaluate(&self.query) };
        self.window_hits = if self.run_style {
            Vec::new()
        } else {
            self.windows
                .iter()
                .enumerate()
                .filter(|(_, w)| window_matches(&w.lower, &query))
                .map(|(i, _)| i)
                .take(MAX_WINDOW_ROWS)
                .collect()
        };
        // Results for a shorter query may still match the longer one.
        let words: Vec<&str> = query.split_whitespace().collect();
        self.files.retain(|f| !words.is_empty() && words.iter().all(|w| f.name.to_lowercase().contains(w)));
        if query != self.ai_query {
            self.ai_phase = crate::ai::Phase::Idle;
        }
        self.rebuild();
        // A command nothing matches strongly starts on the Ask AI row.
        let start_on_ask =
            self.ask_visible() && self.best_score.is_none_or(|s| s >= 2) && self.calc.is_none() && self.window_hits.is_empty();
        self.selected = if start_on_ask { self.ask_index().unwrap_or(0) } else { 0 };
    }

    /// Rebuilds `items` and `lines` from the matches.
    fn rebuild(&mut self) {
        let mut items = Vec::new();
        items.extend(self.tiles.iter().map(|&i| Item::Tile(i)));
        if self.calc.is_some() && !self.run_style {
            items.push(Item::Calc);
        }
        if !self.run_style {
            items.extend(self.results.iter().map(|&i| Item::App(i)));
            items.extend(self.window_hits.iter().map(|&i| Item::Window(i)));
            items.extend((0..self.files.len().min(MAX_FILE_ROWS)).map(Item::File));
        }
        if self.ask_visible() {
            items.push(Item::Ask);
        }
        // Calculator results lead: they are the answer to what was typed.
        if let Some(at) = items.iter().position(|i| *i == Item::Calc) {
            let calc = items.remove(at);
            let first_list = items.iter().position(|i| !matches!(i, Item::Tile(_))).unwrap_or(items.len());
            items.insert(first_list, calc);
        }
        let mut sections: Vec<Section> = Vec::new();
        for item in &items {
            if sections.last() != Some(&item.section()) {
                sections.push(item.section());
            }
        }
        let headers = sections.len() > 1 || sections.first().is_some_and(|s| *s != Section::Apps);
        let mut lines = Vec::new();
        let mut i = 0;
        while i < items.len() {
            let section = items[i].section();
            let end = items[i..].iter().position(|it| it.section() != section).map_or(items.len(), |n| i + n);
            if headers {
                lines.push(Line::Header(section));
            }
            if section == Section::Recent {
                lines.push(Line::Tiles(i..end));
            } else {
                lines.extend((i..end).map(Line::Row));
            }
            i = end;
        }
        self.items = items;
        self.lines = lines;
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
    }

    fn ask_index(&self) -> Option<usize> {
        self.items.iter().position(|i| *i == Item::Ask)
    }

    /// Whether the "Ask AI" row is the last row.
    pub(crate) fn ask_visible(&self) -> bool {
        self.ai_enabled
            && !self.run_style
            && self.calc.is_none()
            && crate::ai::is_candidate(&self.query, self.best_score)
    }

    pub(crate) fn ask_selected(&self) -> bool {
        self.open && self.items.get(self.selected) == Some(&Item::Ask)
    }

    /// Records the query the AI row is asked about and returns it.
    pub(crate) fn begin_ask(&mut self) -> String {
        self.ai_query = self.query.trim().to_lowercase();
        self.query.trim().to_owned()
    }

    fn total_rows(&self) -> usize {
        self.items.len()
    }

    /// Called after the AI phase changed from outside, so the selection stays valid.
    pub(crate) fn refresh_ai_row(&mut self) {
        let was_ask = self.ask_selected();
        self.rebuild();
        if was_ask {
            self.selected = self.ask_index().unwrap_or(self.selected);
        }
        self.selected = self.selected.min(self.total_rows().saturating_sub(1));
    }

    /// Installed apps for the model: pinned ones first, then by name.
    pub(crate) fn app_choices(&self, pinned: &[String], max: usize) -> Vec<(String, String)> {
        let apps = self.apps.as_deref().unwrap_or_default();
        let is_pinned = |a: &App| pinned.iter().any(|p| p.eq_ignore_ascii_case(&a.entry.id));
        apps.iter()
            .filter(|a| is_pinned(a))
            .chain(apps.iter().filter(|a| !is_pinned(a)))
            .take(max)
            .map(|a| (a.entry.id.clone(), a.entry.name.clone()))
            .collect()
    }

    pub(crate) fn launch_command(&self, id: &str, terminal: &str) -> Option<String> {
        self.apps.as_deref()?.iter().find(|a| a.entry.id == id)?.entry.command(terminal)
    }

    /// Title and detail of the AI row, and whether it reports an error.
    pub(crate) fn ask_texts(&self) -> (String, String, bool) {
        use crate::ai::Phase;
        match &self.ai_phase {
            Phase::Idle => ("Ask AI".into(), format!("\"{}\", press Enter", self.query.trim()), false),
            Phase::Asking(_) => ("Thinking…".into(), "The first answer can take a few seconds".into(), false),
            Phase::Preview(i) => (i.summary.join(" · "), "Enter to run, Esc to cancel".into(), false),
            Phase::Error(e) => (e.clone(), "Enter to try again".into(), true),
        }
    }

    fn show(&mut self, output: Option<Output>) {
        self.open = true;
        self.run_style = false;
        self.launch_after_load = false;
        self.launch_action = None;
        self.query.clear();
        self.ai_query.clear();
        self.ai_phase = Default::default();
        self.prewarm_sent = false;
        self.files.clear();
        self.files_seq += 1;
        self.refilter();
        self.output = output;
        self.fade.set(1.0);
    }

    fn hide(&mut self) {
        self.open = false;
        self.ai_phase = Default::default();
        self.launch_after_load = false;
        self.launch_action = None;
        // Window handles must not outlive the window.
        self.windows.clear();
        self.window_hits.clear();
        self.files.clear();
        self.files_seq += 1;
        self.fade.set(0.0);
    }

    /// Closes without a fade, e.g. before a capture.
    pub(crate) fn dismiss(&mut self) {
        self.hide();
        self.fade.jump(0.0);
    }

    fn selected_entry(&self) -> Option<&DesktopEntry> {
        match self.items.get(self.selected)? {
            Item::App(i) | Item::Tile(i) => Some(&self.apps.as_ref()?.get(*i)?.entry),
            _ => None,
        }
    }

    fn move_selection(&mut self, down: bool) {
        let n = self.total_rows();
        if n == 0 {
            return;
        }
        let tiles = self.items.iter().take_while(|i| matches!(i, Item::Tile(_))).count();
        self.selected = if down {
            if self.selected < tiles && tiles < n {
                // Down from a tile enters the list below the tiles.
                tiles
            } else {
                (self.selected + 1) % n
            }
        } else if tiles > 0 && self.selected == tiles && tiles < n {
            // Up from the first list row returns to the tiles.
            0
        } else {
            (self.selected + n - 1) % n
        };
    }

    /// Left and Right move along a row of tiles.
    fn move_in_tiles(&mut self, forward: bool) -> bool {
        let tiles = self.items.iter().take_while(|i| matches!(i, Item::Tile(_))).count();
        if self.selected >= tiles || tiles == 0 {
            return false;
        }
        self.selected = if forward { (self.selected + 1) % tiles } else { (self.selected + tiles - 1) % tiles };
        true
    }

    /// Tab: the first row of the next (or, backwards, previous) section.
    fn cycle_section(&mut self, forward: bool) {
        let Some(current) = self.items.get(self.selected).map(|i| i.section()) else { return };
        let mut starts: Vec<usize> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            if i == 0 || self.items[i - 1].section() != item.section() {
                starts.push(i);
            }
        }
        if starts.len() < 2 {
            return;
        }
        let at = starts.iter().rposition(|&s| s <= self.selected).unwrap_or(0);
        let _ = current;
        let next = if forward { (at + 1) % starts.len() } else { (at + starts.len() - 1) % starts.len() };
        self.selected = starts[next];
    }

    /// Index of the line holding the selected item.
    fn selected_line(&self) -> usize {
        self.lines
            .iter()
            .position(|l| match l {
                Line::Row(i) => *i == self.selected,
                Line::Tiles(range) => range.contains(&self.selected),
                Line::Header(_) => false,
            })
            .unwrap_or(0)
    }

    /// Height of the list area: what fits, at most `ROWS` rows.
    fn list_budget() -> i32 {
        ROWS as i32 * ROW_H
    }

    /// First visible line, keeping the selection in view.
    fn scroll(&self) -> usize {
        let sel = self.selected_line();
        let mut first = sel;
        let mut used = self.lines.get(sel).map_or(0, Line::height);
        while first > 0 {
            let h = self.lines[first - 1].height();
            if used + h > Self::list_budget() {
                break;
            }
            used += h;
            first -= 1;
        }
        first
    }

    /// Panel rectangle for an output of `size` logical pixels.
    fn panel_rect(&self, size: Size<i32, Logical>) -> Rect {
        let w = 640.min(size.w - 32).max(200);
        let list = if self.run_style {
            0
        } else {
            self.lines.iter().map(Line::height).sum::<i32>().clamp(ROW_H, Self::list_budget())
        };
        let h = SEARCH_H + PAD + list + PAD;
        rect((size.w - w) / 2, (size.h as f64 * 0.16) as i32, w, h)
    }

    /// What the row of `item` shows.
    fn row_view(&self, item: Item) -> Option<RowView> {
        let apps = self.apps.as_deref().unwrap_or_default();
        Some(match item {
            Item::App(i) | Item::Tile(i) => {
                let app = apps.get(i)?;
                let detail = if app.entry.generic_name.is_empty() { &app.entry.comment } else { &app.entry.generic_name };
                RowView { glyph: Glyph::App(app.entry.icon.clone()), title: app.entry.name.clone(), detail: detail.clone(), error: false }
            }
            Item::Window(i) => {
                let w = self.windows.get(i)?;
                let icon = apps
                    .iter()
                    .find(|a| !w.app_id.is_empty() && a.entry.id.trim_end_matches(".desktop").eq_ignore_ascii_case(&w.app_id))
                    .map_or_else(|| w.app_id.clone(), |a| a.entry.icon.clone());
                RowView {
                    glyph: Glyph::App(icon),
                    title: format!("Switch to {}", w.title),
                    detail: format!("{} · Workspace {}", if w.app_id.is_empty() { "Window" } else { &w.app_id }, w.workspace + 1),
                    error: false,
                }
            }
            Item::File(i) => {
                let f = self.files.get(i)?;
                let parent = f.path.parent().map(tilde).unwrap_or_default();
                RowView {
                    glyph: Glyph::Symbolic(if f.is_dir {
                        &["folder-symbolic", "inode-directory-symbolic"]
                    } else {
                        &["text-x-generic-symbolic", "document-symbolic"]
                    }),
                    title: f.name.clone(),
                    detail: format!("{parent}  ·  Enter to open, Ctrl+Enter for the folder"),
                    error: false,
                }
            }
            Item::Calc => {
                let c = self.calc.as_ref()?;
                RowView {
                    glyph: Glyph::Symbolic(&["accessories-calculator-symbolic", "x-office-spreadsheet-symbolic"]),
                    title: format!("= {}", c.text),
                    detail: format!("{}  ·  Enter to copy", c.detail),
                    error: false,
                }
            }
            Item::Ask => {
                let (title, detail, error) = self.ask_texts();
                RowView { glyph: Glyph::Symbolic(&["starred-symbolic", "emblem-favorite-symbolic"]), title, detail, error }
            }
        })
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
        if self.tty {
            return self.elements_tty(renderer, output, size, scale, now, accent);
        }
        self.fade.advance(now);
        if !self.visible() || self.output.as_ref().is_some_and(|o| o != output) {
            return Vec::new();
        }
        let panel = self.panel_rect(size);
        self.panel = Some(panel);
        self.rows.clear();
        let fade = self.fade.value.clamp(0.0, 1.0);
        let first = self.scroll();
        let budget = Self::list_budget();
        let loading = self.loading;
        // Visible lines with their top edge, and the rows they draw, before the
        // canvas is borrowed.
        let mut visible: Vec<(Line, i32)> = Vec::new();
        let mut views: Vec<(usize, RowView)> = Vec::new();
        if !self.run_style {
            let mut used = 0;
            for line in self.lines.iter().skip(first) {
                if used + line.height() > budget {
                    break;
                }
                match line {
                    Line::Row(i) => views.extend(self.items.get(*i).and_then(|it| self.row_view(*it)).map(|v| (*i, v))),
                    Line::Tiles(range) => views.extend(range.clone().filter_map(|i| self.row_view(*self.items.get(i)?).map(|v| (i, v)))),
                    Line::Header(_) => {}
                }
                visible.push((line.clone(), used));
                used += line.height();
            }
        }
        let c = &mut self.canvas;
        c.begin(renderer, scale);
        c.alpha = fade as f32;
        c.zoom = 0.96 + 0.04 * fade;
        c.anchor = Point::from((
            (panel.loc.x + panel.size.w / 2) as f64,
            panel.loc.y as f64,
        ));
        style::card(c, panel, radius::PANEL);

        // Search row.
        let x = panel.loc.x;
        let y = panel.loc.y;
        c.icon(renderer, &mut self.icons, &["system-search-symbolic", "edit-find-symbolic"], icon_theme, rect(x + 20, y + 18, 20, 20));
        let mut field = style::text(16.0, Weight::Medium, TEXT);
        let text_x = x + 52;
        let text_w = panel.size.w - 52 - 20;
        if self.query.is_empty() {
            field.color = style::dim(TEXT, 0.45);
            c.styled_label(renderer, if self.run_style { "Run a command" } else { "Search apps, windows and files" }, text_x, y + 17, text_w, field);
        } else {
            c.styled_label(renderer, &self.query, text_x, y + 17, text_w, field);
        }
        if self.open {
            let typed = if self.query.is_empty() { 0.0 } else { c.text.measure(&self.query, field) };
            let caret_x = text_x + (typed.ceil() as i32).min(text_w);
            c.rect(rect(caret_x + 1, y + 17, 2, 22), 1.0, accent, 0.0, LINE);
        }
        c.rect(rect(x + 12, y + SEARCH_H, panel.size.w - 24, 1), 0.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);

        // Results.
        let top = y + SEARCH_H + PAD;
        if self.items.is_empty() && !self.run_style {
            c.styled_label(
                renderer,
                if loading { "Loading applications…" } else { "No results" },
                x + 20,
                top + 14,
                panel.size.w - 40,
                style::text(size::HEADING, Weight::Medium, style::dim(TEXT, 0.5)),
            );
        }
        let inner = panel.size.w - 2 * PAD;
        for (line, offset) in &visible {
            let ly = top + offset;
            match line {
                Line::Header(section) => {
                    style::heading(c, renderer, section.label(), x + PAD + space::SM + space::XS, ly + 8, inner - 24);
                }
                Line::Tiles(range) => {
                    let tw = inner / TILE_COUNT as i32;
                    for (n, item_ix) in range.clone().enumerate() {
                        let Some((_, view)) = views.iter().find(|(i, _)| *i == item_ix) else { continue };
                        let r = rect(x + PAD + n as i32 * tw, ly, tw, TILES_H - 4);
                        if item_ix == self.selected {
                            let [a, b, d, _] = accent;
                            c.rect(r, radius::TILE, style::wash([a, b, d, 1.0], 0.22), 1.0, style::dim(accent, 0.5));
                        }
                        let icon = rect(r.loc.x + (r.size.w - 44) / 2, ly + 10, 44, 44);
                        if let Glyph::App(name) = &view.glyph {
                            if !c.colored_icon(renderer, &mut self.icons, &[name], icon_theme, icon, Some(FULL_COLOR)) {
                                c.icon(renderer, &mut self.icons, &["application-x-executable-symbolic"], icon_theme, rect(icon.loc.x + 10, icon.loc.y + 10, 24, 24));
                            }
                        }
                        let label = style::text(size::SMALL, Weight::Medium, TEXT);
                        let w = (c.text.measure(&view.title, label).ceil() as i32).min(r.size.w - 8);
                        c.styled_label(renderer, &view.title, r.loc.x + (r.size.w - w) / 2, ly + 60, r.size.w - 8, label);
                        self.rows.push((r, item_ix));
                    }
                }
                Line::Row(item_ix) => {
                    let Some((_, view)) = views.iter().find(|(i, _)| i == item_ix) else { continue };
                    let r = rect(x + PAD, ly, inner, ROW_H);
                    if *item_ix == self.selected {
                        let [a, b, d, _] = accent;
                        c.rect(r, radius::ROW, style::wash([a, b, d, 1.0], 0.22), 0.0, LINE);
                    }
                    let icon = rect(r.loc.x + 12, ly + (ROW_H - ICON) / 2, ICON, ICON);
                    match &view.glyph {
                        Glyph::App(name) => {
                            if !c.colored_icon(renderer, &mut self.icons, &[name], icon_theme, icon, Some(FULL_COLOR)) {
                                c.icon(renderer, &mut self.icons, &["application-x-executable-symbolic"], icon_theme, rect(icon.loc.x + 6, icon.loc.y + 6, 20, 20));
                            }
                        }
                        Glyph::Symbolic(names) => {
                            c.icon(renderer, &mut self.icons, names, icon_theme, rect(icon.loc.x + 6, icon.loc.y + 6, 20, 20));
                        }
                    }
                    let mut name = style::text(size::HEADING, Weight::SemiBold, TEXT);
                    if view.error {
                        name.color = style::DANGER;
                    }
                    let tx = r.loc.x + 12 + ICON + 12;
                    let tw = r.size.w - (tx - r.loc.x) - 12;
                    if view.detail.is_empty() {
                        c.styled_label(renderer, &view.title, tx, ly + 14, tw, name);
                    } else {
                        c.styled_label(renderer, &view.title, tx, ly + 6, tw, name);
                        c.styled_label(renderer, &view.detail, tx, ly + 26, tw, style::text(size::SMALL, Weight::Medium, style::dim(TEXT, 0.55)));
                    }
                    self.rows.push((r, *item_ix));
                }
            }
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
                gv.launcher.windows.clear(); gv.launcher.refilter();
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
                gv.launcher.ai_enabled = gv.config.ai.enabled;
                gv.launcher.windows = gv.launcher_windows();
                gv.launcher.show(output);
                if !gv.launcher.loading && gv.launcher.apps.as_ref().is_none_or(|apps| apps.is_empty()) {
                    gv.launcher.loading = true;
                    let (tx,rx)=smithay::reexports::calloop::channel::channel();
                    let result=gv.loop_handle.insert_source(rx,|event,_,gv|{
                        let _stall = crate::perf::Stall::new("launcher.apps_reply");
                        if let smithay::reexports::calloop::channel::Event::Msg((entries, state))=event {
                            gv.guard(crate::module::Feature::Launcher,(),|gv|{
                                gv.launcher.install_entries(entries);
                                if let Some((path, table)) = state {
                                    gv.launcher.frecency = table;
                                    gv.launcher.frecency_path = Some(path);
                                }
                                if !gv.launcher.run_style { gv.launcher.refilter(); }
                                if gv.launcher.open && !gv.launcher.run_style && gv.launcher.launch_after_load { gv.launch_selected(); }
                                else if gv.launcher.open { gv.queue_redraw_all(); }
                            });
                        }
                    });
                    if result.is_ok() { std::thread::spawn(move||{
                        let entries = desktop_entry::load_all();
                        let state = frecency::state_path().map(|path| { let table = frecency::Frecency::load(&path); (path, table) });
                        let _ = tx.send((entries, state));
                    }); }
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

    /// Windows of every output, most recently used first, for "Switch to" rows.
    fn launcher_windows(&self) -> Vec<WindowRow> {
        let mut out = Vec::new();
        for output in &self.workspaces.outputs {
            for (workspace, ws) in output.list.items.iter().enumerate() {
                for w in ws.windows.iter().rev().filter(|w| w.window.alive()) {
                    let (title, app_id) = crate::overlay::window_labels(&w.window);
                    let lower = format!("{} {}", title.to_lowercase(), app_id.to_lowercase());
                    out.push(WindowRow { window: w.window.clone(), title, app_id, lower, workspace });
                }
            }
        }
        out.truncate(MAX_WINDOWS);
        out
    }

    /// Asks the worker for file names matching the query (two characters or more).
    fn launcher_files_request(&mut self) {
        // Tests must not walk the home directory.
        if cfg!(test) {
            return;
        }
        let query = self.launcher.query.trim().to_lowercase();
        self.launcher.files_seq += 1;
        if !self.launcher.open || self.launcher.run_style || query.chars().count() < 2 {
            self.launcher.files.clear();
            return;
        }
        if self.launcher.files_tx.is_none() {
            let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
            let (tx, rx) = smithay::reexports::calloop::channel::channel::<files::Reply>();
            let registered = self.loop_handle.insert_source(rx, |event, _, gv| {
                let _stall = crate::perf::Stall::new("launcher.files_reply");
                if let smithay::reexports::calloop::channel::Event::Msg(reply) = event {
                    gv.guard(crate::module::Feature::Launcher, (), |gv| gv.launcher_files_reply(reply));
                }
            });
            if registered.is_err() {
                return;
            }
            self.launcher.files_tx = files::spawn(home, tx);
        }
        let seq = self.launcher.files_seq;
        if let Some(tx) = &self.launcher.files_tx {
            if tx.send(files::Request { seq, query }).is_err() {
                self.launcher.files_tx = None;
            }
        }
    }

    fn launcher_files_reply(&mut self, reply: files::Reply) {
        let l = &mut self.launcher;
        if !l.open || reply.seq != l.files_seq {
            return;
        }
        let ask_selected = l.ask_selected();
        l.files = reply.hits.into_iter().take(MAX_FILE_ROWS).collect();
        let selected = l.selected;
        l.rebuild();
        // Late file rows must not move the selection off what the user picked.
        l.selected = if ask_selected { l.ask_index().unwrap_or(selected) } else { selected.min(l.items.len().saturating_sub(1)) };
        self.queue_redraw_all();
    }

    fn launch_selected(&mut self) {
        self.launch_selected_with(false);
    }

    /// Activates the selected row; `folder` (Ctrl+Enter) opens a file's folder.
    fn launch_selected_with(&mut self, folder: bool) {
        let action = *self.launcher.launch_action.get_or_insert_with(Instant::now);
        if !self.launcher.run_style {
            match self.launcher.items.get(self.launcher.selected).copied() {
                Some(Item::Ask) => return self.ai_enter(),
                Some(Item::Calc) => {
                    let text = self.launcher.calc.as_ref().map(|c| c.text.clone());
                    self.launcher.hide();
                    if let Some(text) = text {
                        self.own_text_selection(&text);
                    }
                    self.queue_redraw_all();
                    return;
                }
                Some(Item::Window(i)) => {
                    let window = self.launcher.windows.get(i).map(|w| w.window.clone());
                    self.launcher.hide();
                    if let Some(window) = window.filter(|w| w.alive()) {
                        self.focus_window(Some(&window));
                    }
                    self.queue_redraw_all();
                    return;
                }
                Some(Item::File(i)) => {
                    let target = self.launcher.files.get(i).map(|f| {
                        if folder { f.path.parent().unwrap_or(&f.path).to_path_buf() } else { f.path.clone() }
                    });
                    self.launcher.hide();
                    if let Some(target) = target {
                        let command = format!("xdg-open {}", desktop_entry::shell_quote(&target.to_string_lossy()));
                        self.launch_shell_at(action, &command, None);
                    }
                    self.queue_redraw_all();
                    return;
                }
                _ => {}
            }
        }
        if !self.launcher.run_style && self.launcher.loading && self.launcher.results.is_empty() {
            self.launcher.launch_after_load=true;
            return;
        }
        let app_id = self.launcher.selected_entry().map(|e| e.id.clone());
        // The selected row from the last frame is where the window grows from.
        let source = self.active_output().and_then(|output| {
            let geo = self.space.output_geometry(&output)?;
            let (row, _) = self.launcher.rows.iter().find(|(_, i)| *i == self.launcher.selected)?;
            Some(Rectangle::new(row.loc.to_f64() + geo.loc.to_f64(), row.size.to_f64()))
        });
        let command = if self.launcher.run_style {
            Some(self.launcher.query.trim().to_owned()).filter(|s| !s.is_empty())
        } else { self.launcher.selected_entry().and_then(|e| e.command(&self.config.terminal)) };
        self.launcher.hide();
        if let Some(command) = command {
            if let (false, Some(id)) = (self.launcher.run_style, app_id.as_deref()) {
                self.launcher.frecency.record(id, frecency::now_secs());
                if let Some(path) = self.launcher.frecency_path.clone() {
                    self.launcher.frecency.save_in_background(path);
                }
            }
            self.launch_shell_from(action, &command, app_id.as_deref(), source);
        }
        self.queue_redraw_all();
    }

    /// Keys while the launcher is open. Every key is consumed.
    pub fn launcher_key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        let mods = *mods;
        self.guard(crate::module::Feature::Launcher, (), |gv| {
            gv.launcher.ai_enabled = gv.config.ai.enabled;
            let l = &mut gv.launcher;
            let before = l.query.clone();
            if !matches!(sym.raw(), keysyms::KEY_Return | keysyms::KEY_KP_Enter) { l.launch_after_load=false; l.launch_action=None; }
            match sym.raw() {
                keysyms::KEY_Escape => l.hide(),
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => return gv.launch_selected_with(mods.ctrl),
                keysyms::KEY_Down => l.move_selection(true),
                keysyms::KEY_Up => l.move_selection(false),
                keysyms::KEY_Tab => l.cycle_section(true),
                keysyms::KEY_ISO_Left_Tab => l.cycle_section(false),
                keysyms::KEY_Right => {
                    l.move_in_tiles(true);
                }
                keysyms::KEY_Left => {
                    l.move_in_tiles(false);
                }
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
            if gv.launcher.query != before {
                gv.launcher_files_request();
            }
            gv.ai_after_query_change();
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
    fn cold_launcher_draws_placeholders_without_icon_io_then_uses_delivery() {
        use crate::icons::app_worker::{wait_delivery, with_test_worker};
        use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let mut renderer =
            unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap();
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new(
            "launcher-icons".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        h.gv.space.map_output(&output, (0, 0));
        h.gv.launcher = launcher_with(&[("Fixture", "")]);
        h.gv.launcher.apps.as_mut().unwrap()[0].entry.icon = "fixture".into();
        h.gv.launcher.show(Some(output.clone()));
        h.gv.launcher.fade.jump(1.0);
        let caller = std::thread::current().id();
        let (release, gate) = std::sync::mpsc::channel();
        let mut blocked = false;
        with_test_worker(
            move |_, _| {
                assert_ne!(caller, std::thread::current().id());
                if !blocked {
                    gate.recv().unwrap();
                    blocked = true;
                }
                Some("/injected/fixture.svg".into())
            },
            crate::icons::app_worker::tests::pixels,
            |rx| {
                let before = crate::icons::caller_io();
                let cold = h.gv.launcher.elements(
                    &mut renderer,
                    &output,
                    (1000, 800).into(),
                    1.0,
                    Duration::ZERO,
                    [1.0; 4],
                    "test",
                );
                assert_eq!(before, crate::icons::caller_io());
                assert_eq!(h.gv.launcher.rows.len(), 1);
                let rows = h.gv.launcher.rows.clone();
                release.send(()).unwrap();
                // Search, application and fallback icon requests each deliver once.
                for _ in 0..4 {
                    wait_delivery(rx).apply(&mut h.gv);
                }
                let ready = h.gv.launcher.elements(
                    &mut renderer,
                    &output,
                    (1000, 800).into(),
                    1.0,
                    Duration::ZERO,
                    [1.0; 4],
                    "test",
                );
                assert_eq!(rows, h.gv.launcher.rows);
                assert!(
                    ready.len() > cold.len(),
                    "ready icons must enter the rendered frame"
                );
                assert_eq!(before, crate::icons::caller_io());
            },
        );
    }
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

    fn kinds(l: &Launcher) -> Vec<Item> {
        l.items.clone()
    }

    #[test]
    fn calculator_row_leads_and_hides_the_ai_row() {
        let mut l = launcher_with(&[("Calculator", ""), ("Files", "")]);
        l.ai_enabled = true;
        l.query = "2+2*3".into();
        l.refilter();
        assert_eq!(l.calc.as_ref().map(|c| c.text.as_str()), Some("8"));
        assert_eq!(kinds(&l).first(), Some(&Item::Calc));
        assert_eq!(l.selected, 0);
        assert!(!l.ask_visible(), "a calculation is not a model prompt");
        assert_eq!(l.lines.first(), Some(&Line::Header(Section::Calculator)));
        l.query = "5 km in miles".into();
        l.refilter();
        assert!(l.calc.is_some() && !l.ask_visible());
        l.query = "calc".into();
        l.refilter();
        assert!(l.calc.is_none());
        assert_eq!(kinds(&l), [Item::App(0)]);
    }

    #[test]
    fn single_apps_section_has_no_header_but_mixed_sections_do() {
        let mut l = launcher_with(&[("Alpha", ""), ("Beta", "")]);
        l.refilter();
        assert!(l.lines.iter().all(|line| matches!(line, Line::Row(_))));
        l.files = vec![files::Hit { path: "/home/u/alpha.txt".into(), name: "alpha.txt".into(), is_dir: false }];
        l.query = "alpha".into();
        l.refilter();
        // Files retained by the refilter because the name still matches.
        assert_eq!(kinds(&l), [Item::App(0), Item::File(0)]);
        assert_eq!(
            l.lines,
            [Line::Header(Section::Apps), Line::Row(0), Line::Header(Section::Files), Line::Row(1)]
        );
        // The panel height counts headers.
        let h = l.panel_rect((1646, 926).into()).size.h;
        assert_eq!(h, SEARCH_H + PAD + 2 * ROW_H + 2 * HEADER_H + PAD);
        l.query = "beta".into();
        l.refilter();
        assert!(l.files.is_empty(), "file rows that no longer match are dropped");
    }

    #[test]
    fn frecency_orders_equal_matches_and_fills_the_tiles() {
        let mut l = launcher_with(&[("Alpha", ""), ("Alpine", ""), ("Zed", "")]);
        l.frecency.record("Zed", frecency::now_secs());
        l.frecency.record("Zed", frecency::now_secs());
        l.frecency.record("Alpine", frecency::now_secs());
        l.refilter();
        assert_eq!(l.tiles, [2, 1], "most used first");
        assert_eq!(&kinds(&l)[..2], [Item::Tile(2), Item::Tile(1)]);
        assert_eq!(l.lines[0], Line::Header(Section::Recent));
        assert_eq!(l.lines[1], Line::Tiles(0..2));
        assert_eq!(l.lines[2], Line::Header(Section::Apps));
        // Typing hides the tiles and ranks by frecency inside one match rank.
        l.query = "al".into();
        l.refilter();
        assert!(l.tiles.is_empty());
        let names: Vec<_> = l.results.iter().map(|&i| l.apps.as_ref().unwrap()[i].entry.name.clone()).collect();
        assert_eq!(names, ["Alpine", "Alpha"]);
        // Rank still beats frecency: a prefix match stays above a keyword match.
        l.query = "zed".into();
        l.refilter();
        assert_eq!(l.results.len(), 1);
    }

    #[test]
    fn tiles_navigate_with_arrows_and_the_list_with_up_down() {
        let mut l = launcher_with(&[("A", ""), ("B", ""), ("C", ""), ("D", "")]);
        l.frecency.record("A", frecency::now_secs());
        l.frecency.record("B", frecency::now_secs());
        l.refilter();
        assert_eq!(l.tiles.len(), 2);
        assert!(l.move_in_tiles(true));
        assert_eq!(l.selected, 1);
        assert!(l.move_in_tiles(true));
        assert_eq!(l.selected, 0, "wraps inside the tile row");
        l.move_selection(true);
        assert_eq!(l.selected, 2, "Down leaves the tiles for the first list row");
        l.move_selection(false);
        assert_eq!(l.selected, 0, "Up returns to the tiles");
        l.selected = 3;
        assert!(!l.move_in_tiles(true), "arrows do nothing in the list");
    }

    #[test]
    fn tab_cycles_sections() {
        let mut l = launcher_with(&[("Alpha", ""), ("Alpine", "")]);
        l.files = vec![files::Hit { path: "/h/al.txt".into(), name: "al.txt".into(), is_dir: false }];
        l.query = "al".into();
        l.refilter();
        assert_eq!(kinds(&l), [Item::App(0), Item::App(1), Item::File(0)]);
        l.cycle_section(true);
        assert_eq!(l.selected, 2);
        l.cycle_section(true);
        assert_eq!(l.selected, 0, "wraps to the first section");
        l.cycle_section(false);
        assert_eq!(l.selected, 2);
        l.selected = 1;
        l.cycle_section(false);
        assert_eq!(l.selected, 2, "backwards from the first section wraps to the last");
    }

    #[test]
    fn windows_match_title_or_app_id_words() {
        assert!(window_matches("inbox - mozilla firefox firefox", "fire"));
        assert!(window_matches("inbox - mozilla firefox firefox", "inbox firefox"));
        assert!(!window_matches("inbox firefox", "chrome"));
        assert!(!window_matches("inbox firefox", ""));
    }

    #[test]
    fn late_file_rows_keep_the_ask_row_selected() {
        let mut l = launcher_with(&[("Zzz", "")]);
        l.ai_enabled = true;
        l.query = "play something calm".into();
        l.refilter();
        assert!(l.ask_selected_for_test());
        l.files = vec![files::Hit { path: "/h/play something calm.txt".into(), name: "play something calm.txt".into(), is_dir: false }];
        l.rebuild();
        l.selected = l.ask_index().unwrap();
        assert_eq!(l.items.last(), Some(&Item::Ask));
        assert_eq!(l.items.len(), 2);
    }

    #[test]
    fn selection_stays_visible_with_headers() {
        let names: Vec<(String, String)> = (0..12).map(|i| (format!("App{i:02}"), String::new())).collect();
        let refs: Vec<(&str, &str)> = names.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let mut l = launcher_with(&refs);
        l.files = vec![files::Hit { path: "/h/app.txt".into(), name: "app.txt".into(), is_dir: false }];
        l.query = "app".into();
        l.refilter();
        l.selected = l.items.len() - 1;
        let first = l.scroll();
        let used: i32 = l.lines[first..=l.selected_line()].iter().map(Line::height).sum();
        assert!(used <= Launcher::list_budget());
        assert!(first > 0);
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
