//! Start uses the launcher's ranking and desktop entries, loaded off the event loop.
use super::{cache::CachedElement, Align, ClassicConfig};
use crate::{
    desktop_entry::DesktopEntry,
    icons::Icons,
    module::Feature,
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        BarAction, Canvas, PowerAction, FULL_COLOR, LINE,
    },
    ui::{
        spring::{Spring, SpringParams},
        style::{self as look, radius, size as tsize, text as tstyle, TEXT},
        text::Weight,
    },
    Gavno,
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    input::keyboard::{keysyms, Keysym, ModifiersState},
    output::Output,
    reexports::calloop::channel,
    utils::{Logical, Point, Size},
};
use std::time::Duration;

#[derive(Clone)]
struct App {
    entry: DesktopEntry,
    name: String,
    words: String,
}
#[derive(Clone)]
enum Hit {
    App(usize),
    All,
    Power,
    Action(PowerAction),
}
pub struct Start {
    pub open: bool,
    output: Option<Output>,
    opening: Spring,
    apps: Vec<App>,
    loading: bool,
    /// `desktop_entry::apps_stamp` when `apps` was loaded.
    apps_stamp: u64,
    query: String,
    selected: usize,
    results: Vec<usize>,
    pinned: Vec<usize>,
    all: bool,
    scroll: usize,
    scroll_motion: f64,
    power: bool,
    panel: Option<Rect>,
    hits: Vec<(Rect, Hit)>,
    canvas: Canvas,
    icons: Icons,
    user: String,
    revision: u64,
    cached: Vec<CachedElement>,
    cache_key: Option<(u64, u64, i32, i32, u64, [u32; 4], u64)>,
}
impl Start {
    pub(crate) fn icon_ready(&mut self) -> Option<Output> {
        self.cache_key = None;
        (self.open || self.opening.is_active() || self.opening.value >= 0.001)
            .then(|| self.output.clone()).flatten()
    }
    /// Drop render caches after leaving Classic. The app list stays, so the
    /// next Start opens without a rescan.
    pub fn release_caches(&mut self) {
        if self.open {
            return;
        }
        self.canvas = Default::default();
        self.cached = Vec::new();
        self.cache_key = None;
        self.hits = Vec::new();
    }
}
impl Default for Start {
    fn default() -> Self {
        Self {
            open: false,
            output: None,
            opening: Spring::new(0.0, SpringParams::SNAPPY),
            apps: Vec::new(),
            loading: false,
            apps_stamp: 0,
            query: String::new(),
            selected: 0,
            results: Vec::new(),
            pinned: Vec::new(),
            all: false,
            scroll: 0,
            scroll_motion: 0.0,
            power: false,
            panel: None,
            hits: Vec::new(),
            canvas: Default::default(),
            icons: Icons::nonblocking(crate::icons::app_worker::Consumer::Start),
            revision: 0,
            cached: Vec::new(),
            cache_key: None,
            user: std::env::var("USER").unwrap_or_else(|_| "User".into()),
        }
    }
}
impl Start {
    pub fn release(&mut self) {
        if self.open || self.opening.is_active() || (self.apps.is_empty() && self.cached.is_empty())
        {
            return;
        }
        self.apps = Vec::new();
        self.results = Vec::new();
        self.pinned = Vec::new();
        self.cached = Vec::new();
        self.cache_key = None;
        self.canvas = Canvas::default();
        self.loading = false;
    }
    pub fn open_power(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.power = true;
    }
    pub fn active(&self) -> bool {
        self.opening.is_active()
    }
    pub fn close(&mut self) {
        if self.open || self.power {
            self.revision = self.revision.wrapping_add(1);
        }
        self.open = false;
        self.power = false;
        self.opening.set(0.0);
    }
    pub fn contains(&self, output: &Output, p: Point<f64, Logical>) -> bool {
        self.output.as_ref() == Some(output)
            && self.panel.is_some_and(|r| r.to_f64().contains(p))
            && (self.open || self.opening.value > 0.001)
    }
    fn filter(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        let q = self.query.trim().to_lowercase();
        if q.is_empty() && !self.all {
            self.results = self.pinned.clone();
        } else {
            let mut names = std::collections::HashSet::new();
            let mut ranked: Vec<_> = self
                .apps
                .iter()
                .enumerate()
                .filter(|(_, a)| names.insert(a.name.clone()))
                .filter_map(|(i, a)| crate::launcher::score(&a.name, &a.words, &q).map(|s| (s, i)))
                .collect();
            ranked.sort_by_key(|(s, _)| *s);
            self.results = ranked.into_iter().map(|(_, i)| i).collect();
        }
        self.selected = 0;
        self.scroll = 0;
    }
    fn load(
        &mut self,
        entries: Vec<DesktopEntry>,
        config: &ClassicConfig,
        terminal: &str,
        files: &str,
    ) {
        let mut seen = std::collections::HashSet::new();
        self.apps = entries
            .into_iter()
            .take(4096)
            .filter(|e| !e.hidden_from_menus && !e.name.is_empty() && seen.insert(e.id.clone()))
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
        for (id, name, command, icon) in [
            ("@terminal", "Terminal", terminal, "utilities-terminal"),
            ("@files", "Files", files, "system-file-manager"),
            (
                "@settings",
                "Settings",
                config.settings_command.as_str(),
                "preferences-system",
            ),
        ] {
            self.apps.push(App {
                name: name.to_lowercase(),
                words: format!("{id} {command}"),
                entry: DesktopEntry {
                    id: id.into(),
                    name: name.into(),
                    exec: command.into(),
                    icon: icon.into(),
                    startup_wm_class: String::new(),
                    terminal: false,
                    path: None,
                    generic_name: String::new(),
                    keywords: String::new(),
                    comment: String::new(),
                    hidden_from_menus: false,
                    mime_types: Vec::new(),
                    categories: Vec::new(),
                },
            });
        }
        self.apps.sort_by(|a, b| a.name.cmp(&b.name));
        self.pinned = config
            .start_pinned
            .iter()
            .take(128)
            .filter_map(|id| {
                self.apps.iter().position(|a| {
                    a.entry
                        .id
                        .eq_ignore_ascii_case(id.trim_end_matches(".desktop"))
                        || a.entry.name.eq_ignore_ascii_case(id)
                        || a.entry.startup_wm_class.eq_ignore_ascii_case(id)
                        || a.entry
                            .exec
                            .split_whitespace()
                            .next()
                            .and_then(|s| std::path::Path::new(s).file_name())
                            .is_some_and(|name| name == id.as_str())
                })
            })
            .collect();
        self.loading = false;
        self.filter();
    }
    fn move_selection(&mut self, delta: isize) {
        self.revision = self.revision.wrapping_add(1);
        if self.results.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + delta).rem_euclid(self.results.len() as isize) as usize;
        if self.list() {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + 10 {
                self.scroll = self.selected - 9;
            }
        } else {
            let row = self.selected / 6;
            if row < self.scroll {
                self.scroll = row;
            } else if row >= self.scroll + 4 {
                self.scroll = row - 3;
            }
        }
    }
    fn list(&self) -> bool {
        self.all || !self.query.is_empty()
    }
    pub fn scroll(&mut self, delta: f64) {
        self.revision = self.revision.wrapping_add(1);
        self.scroll_motion += delta;
        if self.scroll_motion.abs() < 24.0 {
            return;
        }
        let max = if self.list() {
            self.results.len().saturating_sub(10)
        } else {
            self.results.len().div_ceil(6).saturating_sub(4)
        };
        self.scroll = if self.scroll_motion > 0.0 {
            (self.scroll + 1).min(max)
        } else {
            self.scroll.saturating_sub(1)
        };
        self.scroll_motion = 0.0;
    }
    pub fn append(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        now: Duration,
        _align: Align,
        accent: [f32; 4],
        theme: &str,
        out: &mut Vec<GavnoRenderElement>,
    ) {
        self.opening.advance(now);
        if self.output.as_ref() != Some(output)
            || (!self.open && !self.opening.is_active() && self.opening.value < 0.001)
        {
            if !self.open && !self.opening.is_active() && !self.cached.is_empty() {
                self.cached = Vec::new();
                self.canvas = Canvas::default();
                self.cache_key = None;
            }
            return;
        }
        let key = (
            self.revision,
            self.opening.value.to_bits(),
            size.w,
            size.h,
            output.current_scale().fractional_scale().to_bits(),
            accent.map(f32::to_bits),
            crate::icons::app_worker::generation(),
        );
        if self.cache_key == Some(key) {
            out.extend(self.cached.iter().cloned().map(GavnoRenderElement::Desktop));
            return;
        }
        self.cache_key = Some(key);
        let width = 640.min((size.w - 16).max(1));
        let height = 700.min((size.h - 64).max(1));
        let x = 8;
        let y = (size.h - 56 - height).max(0);
        let panel = rect(x, y, width, height);
        self.panel = Some(panel);
        self.hits.clear();
        let list = self.list();
        let c = &mut self.canvas;
        c.begin(renderer, output.current_scale().fractional_scale());
        c.text.begin_frame_cache();
        let p = self.opening.value.clamp(0.0, 1.0);
        c.alpha = p as f32;
        c.zoom = 0.96 + 0.04 * p;
        c.anchor = (x as f64 + width as f64 / 2.0, (y + height) as f64).into();
        look::card(c, panel, radius::PANEL);
        let (xl, xxl) = (look::space::XL, look::space::XXL);
        let search = rect(x + xl, y + xl, width - 2 * xl, 40);
        c.rect(search, radius::FIELD, look::INSET, 1.0, LINE);
        c.icon(
            renderer,
            &mut self.icons,
            &["system-search-symbolic"],
            theme,
            rect(x + 38, y + 36, 18, 18),
        );
        let hint = self.query.is_empty();
        c.styled_label(
            renderer,
            if hint { "Search applications" } else { &self.query },
            x + 70,
            y + 35,
            width - 110,
            tstyle(tsize::HEADING, Weight::Medium, if hint { look::dim(TEXT, 0.5) } else { TEXT }),
        );
        c.rect(
            rect(search.loc.x + 12, search.loc.y + 37, search.size.w - 24, 2),
            1.0,
            accent,
            0.0,
            LINE,
        );
        let all = rect(x + width - 128, y + 84, 104, 32);
        look::row_bg(c, all, false);
        let all_label = if self.all { "Pinned" } else { "All apps" };
        let st = tstyle(tsize::SMALL, Weight::SemiBold, TEXT);
        let w = c.text.measure(all_label, st).ceil() as i32;
        c.styled_label(renderer, all_label, all.loc.x + 14, all.loc.y + 8, w + 2, st);
        c.icon(renderer, &mut self.icons, &["go-next-symbolic"], theme, rect(all.loc.x + 14 + w + 6, all.loc.y + 8, 16, 16));
        self.hits.push((all, Hit::All));
        c.styled_label(
            renderer,
            if !self.query.is_empty() {
                "Search results"
            } else if self.all {
                "All apps"
            } else {
                "Pinned"
            },
            x + xxl,
            y + 92,
            width - 180,
            tstyle(tsize::HEADING, Weight::SemiBold, TEXT),
        );
        if self.loading {
            c.styled_label(
                renderer,
                "Loading applications…",
                x + 32,
                y + 146,
                width - 64,
                tstyle(tsize::BODY, Weight::Medium, look::dim(TEXT, 0.6)),
            );
        } else if self.results.is_empty() {
            c.styled_label(
                renderer,
                "No applications found",
                x + 32,
                y + 146,
                width - 64,
                tstyle(tsize::BODY, Weight::Medium, look::dim(TEXT, 0.6)),
            );
        }
        let rows = ((height - 210) / if list { 44 } else { 96 }).max(1) as usize;
        let first = if list { self.scroll } else { self.scroll * 6 };
        let count = if list { rows } else { rows * 6 };
        for (position, &index) in self.results.iter().enumerate().skip(first).take(count) {
            let Some(app) = self.apps.get(index) else {
                continue;
            };
            let n = position - first;
            let r = if list {
                rect(x + 24, y + 134 + n as i32 * 44, width - 48, 44)
            } else {
                let cell = (width - 48) / 6;
                rect(
                    x + 24 + (n % 6) as i32 * cell,
                    y + 136 + (n / 6) as i32 * 96,
                    cell,
                    88,
                )
            };
            if position == self.selected {
                c.rect(r, radius::ROW, look::wash(accent, 0.16), 1.0, look::dim(accent, 0.6));
            }
            let icon = if list {
                rect(r.loc.x + 8, r.loc.y + 8, 28, 28)
            } else {
                rect(r.loc.x + (r.size.w - 32) / 2, r.loc.y + 10, 32, 32)
            };
            if !c.colored_icon(
                renderer,
                &mut self.icons,
                &[&app.entry.icon],
                theme,
                icon,
                Some(FULL_COLOR),
            ) {
                c.icon(
                    renderer,
                    &mut self.icons,
                    &["application-x-executable-symbolic"],
                    theme,
                    icon,
                );
            }
            if list {
                c.styled_label(
                    renderer,
                    &app.entry.name,
                    r.loc.x + 48,
                    r.loc.y + 13,
                    r.size.w - 56,
                    tstyle(tsize::BODY, Weight::Medium, TEXT),
                );
            } else {
                let st = tstyle(tsize::CAPTION + 0.5, Weight::Medium, TEXT);
                let w = c.text.measure(&app.entry.name, st).ceil().min((r.size.w - 10) as f32) as i32;
                c.styled_label(
                    renderer,
                    &app.entry.name,
                    r.loc.x + (r.size.w - w) / 2,
                    r.loc.y + 55,
                    r.size.w - 10,
                    st,
                );
            }
            self.hits.push((r, Hit::App(index)));
        }
        let sm = look::space::SM;
        let footer = rect(x + sm, y + height - 64 - sm, width - 2 * sm, 64);
        c.rect(footer, radius::TILE, look::SURFACE, 0.0, LINE);
        let avatar = rect(x + 24, footer.loc.y + 12, 40, 40);
        c.rect(avatar, radius::DISC, look::wash(accent, 0.94), 0.0, LINE);
        let on = look::on_accent(accent);
        let rgb = [0, 1, 2].map(|i| (on[i] * 255.0).round() as u8);
        c.colored_icon(renderer, &mut self.icons, &["avatar-default-symbolic"], theme, rect(avatar.loc.x + 8, avatar.loc.y + 8, 24, 24), Some(rgb));
        c.styled_label(
            renderer,
            &self.user,
            x + 80,
            footer.loc.y + 22,
            width - 176,
            tstyle(tsize::BODY, Weight::SemiBold, TEXT),
        );
        let power = rect(x + width - 64, footer.loc.y + 12, 40, 40);
        c.rect(power, radius::DISC, look::SURFACE_RAISED, 0.0, LINE);
        c.icon(
            renderer,
            &mut self.icons,
            &["system-shutdown-symbolic"],
            theme,
            rect(power.loc.x + 10, power.loc.y + 10, 20, 20),
        );
        self.hits.push((power, Hit::Power));
        if self.power {
            let r = rect(x + width - 232, footer.loc.y - 200, 208, 192);
            look::card(c, r, radius::TILE);
            for (i, (name, action)) in [
                ("Lock", PowerAction::Lock),
                ("Log out", PowerAction::Logout),
                ("Suspend", PowerAction::Suspend),
                ("Restart", PowerAction::Restart),
                ("Shut down", PowerAction::Shutdown),
            ]
            .into_iter()
            .enumerate()
            {
                let r = rect(r.loc.x + 8, r.loc.y + 8 + i as i32 * 35, 192, 32);
                if i == 4 {
                    c.rect(r, radius::ROW, look::wash(look::DANGER, 0.12), 0.0, LINE);
                }
                c.styled_label(
                    renderer,
                    name,
                    r.loc.x + 12,
                    r.loc.y + 8,
                    168,
                    tstyle(tsize::LABEL, Weight::Medium, if i == 4 { look::DANGER } else { TEXT }),
                );
                self.hits.push((r, Hit::Action(action)));
            }
        }
        // Match zoomed drawing exactly for pointer routing.
        for (r, _) in &mut self.hits {
            *r = c.mapped(*r);
        }
        self.panel = Some(c.mapped(panel));
        self.cached = c
            .finish()
            .into_iter()
            .filter_map(CachedElement::from_ui)
            .collect();
        c.text.end_frame_cache();
        out.extend(self.cached.iter().cloned().map(GavnoRenderElement::Desktop));
    }
}
impl Gavno {
    pub fn toggle_start(&mut self) {
        self.guard(Feature::Classic, (), |gv| {
            if gv.classic.start.open {
                gv.classic.start.close();
            } else {
                gv.close_launcher();
                gv.classic.desktop.cancel_interaction();
                gv.close_overview();
                gv.topbar.close();
                gv.classic.taskbar.close_menu();
                let o = gv.active_output();
                let s = &mut gv.classic.start;
                s.output = o;
                s.open = true;
                s.query.clear();
                s.all = false;
                s.power = false;
                s.filter();
                s.opening.set(1.0);
                let stamp = crate::desktop_entry::apps_stamp();
                let first = s.apps.is_empty();
                if (first || s.apps_stamp != stamp) && !s.loading {
                    if !first { crate::icons::forget_misses(); }
                    s.apps_stamp = stamp;
                    s.loading = true;
                    let theme = gv.config.dock.icon_theme.clone();
                    let (tx, rx) = channel::channel();
                    let result = gv.loop_handle.insert_source(rx, |event, _, gv| {
                        let _stall = crate::perf::Stall::new("classic.apps_reply");
                        if let channel::Event::Msg(entries) = event {
                            gv.guard(Feature::Classic, (), |gv| {
                                gv.classic.start.load(
                                    entries,
                                    &gv.config.classic,
                                    &gv.config.terminal,
                                    &gv.config.file_manager,
                                );
                                gv.queue_redraw_all();
                            });
                        }
                    });
                    if result.is_ok() {
                        std::thread::spawn(move || {
                            let _ = tx.send(crate::desktop_entry::load_all_with_icons(&theme));
                        });
                    } else {
                        gv.classic.start.loading = false;
                    }
                }
                gv.clear_desktop_pointer();
            }
            gv.queue_redraw_all();
        });
    }
    fn launch_start_entry(&mut self, index: usize) {
        let action = std::time::Instant::now();
        if let Some(entry) = self.classic.start.apps.get(index).map(|a| &a.entry) {
            // Configured commands are shell commands, whereas desktop Exec is parsed.
            let command = match entry.id.as_str() {
                "@terminal" => Some(self.config.terminal.clone()),
                "@files" => Some(self.config.file_manager.clone()),
                "@settings" => Some(self.config.classic.settings_command.clone()),
                _ => entry.command(&self.config.terminal),
            };
            let app_id = entry.id.clone();
            if let Some(cmd) = command {
                self.launch_shell_at(action, &cmd, Some(&app_id));
            }
        }
        self.classic.start.close();
        self.queue_redraw_all();
    }
    pub fn classic_start_key(&mut self, sym: Keysym, ch: Option<char>, mods: &ModifiersState) {
        self.guard(Feature::Classic, (), |gv| {
            let s = &mut gv.classic.start;
            match sym.raw() {
                keysyms::KEY_Escape => {
                    if s.power {
                        s.power = false;
                    } else {
                        s.close();
                    }
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                    if let Some(&i) = s.results.get(s.selected) {
                        gv.launch_start_entry(i);
                    }
                    return;
                }
                keysyms::KEY_Down => {
                    let n = if s.list() { 1 } else { 6 };
                    s.move_selection(n);
                }
                keysyms::KEY_Up => {
                    let n = if s.list() { -1 } else { -6 };
                    s.move_selection(n);
                }
                keysyms::KEY_Right | keysyms::KEY_Tab => s.move_selection(1),
                keysyms::KEY_Left | keysyms::KEY_ISO_Left_Tab => s.move_selection(-1),
                keysyms::KEY_BackSpace => {
                    if mods.ctrl {
                        s.query.clear();
                    } else {
                        s.query.pop();
                    }
                    s.filter();
                }
                _ => {
                    if let Some(c) =
                        ch.filter(|c| !c.is_control() && !mods.ctrl && !mods.logo && !mods.alt)
                    {
                        if s.query.len() < 512 {
                            s.query.push(c);
                            s.filter();
                        }
                    }
                }
            }
            gv.classic.start.revision = gv.classic.start.revision.wrapping_add(1);
            gv.queue_redraw_all();
        });
    }
    pub fn classic_start_button(
        &mut self,
        output: &Output,
        p: Point<f64, Logical>,
        button: u32,
        pressed: bool,
    ) -> bool {
        if !self.classic.start.open {
            return false;
        }
        if !pressed {
            return true;
        }
        let hit = if self.classic.start.output.as_ref() == Some(output) {
            self.classic
                .start
                .hits
                .iter()
                .rev()
                .find(|(r, _)| r.to_f64().contains(p))
                .map(|(_, h)| h.clone())
        } else {
            None
        };
        if !self.classic.start.contains(output, p) {
            self.classic.start.close();
            self.queue_redraw_all();
            return true;
        }
        if button == 0x110 {
            match hit {
                Some(Hit::App(i)) => self.launch_start_entry(i),
                Some(Hit::All) => {
                    let s = &mut self.classic.start;
                    s.all = !s.all;
                    s.filter();
                }
                Some(Hit::Power) => self.classic.start.power = !self.classic.start.power,
                Some(Hit::Action(a)) => {
                    self.classic.start.close();
                    self.bar_action(output, BarAction::Power(a));
                }
                None => {
                    if !self.classic.start.contains(output, p) {
                        self.classic.start.close();
                    }
                }
            }
        }
        self.classic.start.revision = self.classic.start.revision.wrapping_add(1);
        self.queue_redraw_all();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn start() -> Start {
        let entries = [
            ("firefox", "Firefox", "Web browser"),
            ("files", "Files", "File Manager"),
            ("foot", "foot", "Terminal"),
        ]
        .into_iter()
        .map(|(id, name, generic)| {
            DesktopEntry::parse(
                id,
                &format!("[Desktop Entry]\nName={name}\nGenericName={generic}\nExec={id}\n"),
                None,
            )
            .unwrap()
        })
        .collect();
        let mut s = Start::default();
        s.load(entries, &ClassicConfig::default(), "foot", "gavno-files");
        s
    }
    #[test]
    fn search_uses_launcher_ranking_and_keyboard_selection() {
        let mut s = start();
        s.query = "fi".into();
        s.filter();
        assert_eq!(
            s.results
                .iter()
                .map(|&i| s.apps[i].entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Files", "Firefox"]
        );
        s.move_selection(-1);
        assert_eq!(s.selected, 1);
        s.move_selection(1);
        assert_eq!(s.selected, 0);
        s.query = "browser".into();
        s.filter();
        assert_eq!(s.apps[s.results[0]].entry.id, "firefox");
        s.query = "does not exist".into();
        s.filter();
        s.move_selection(1);
        assert!(s.results.is_empty());
        assert_eq!(s.selected, 0);
        s.query.clear();
        s.all = true;
        s.filter();
        let names: Vec<_> = s.results.iter().map(|&i| &s.apps[i].name).collect();
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
        s.all = false;
        s.filter();
        assert_eq!(s.results.len(), 4);
        assert_eq!(s.apps[s.results[0]].entry.id, "@terminal");
    }
}

#[cfg(test)]
mod containment_tests {
    use super::*;
    #[test]
    fn start_panel_contains_only_its_output_and_panel() {
        let o = Output::new(
            "start".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        let mut s = Start::default();
        s.output = Some(o.clone());
        s.panel = Some(rect(8, 160, 640, 700));
        s.open = true;
        assert!(s.contains(&o, (100.0, 200.0).into()));
        assert!(!s.contains(&o, (700.0, 200.0).into()));
        s.close();
        assert!(!s.contains(&o, (100.0, 200.0).into()));
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn reopen_reuses_decoded_icons_after_render_cache_release() {
        use crate::icons::app_worker::{wait_delivery, with_test_worker};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let decodes = Arc::new(AtomicUsize::new(0));
        let count = decodes.clone();
        with_test_worker(
            |_, _| Some("/injected/start.svg".into()),
            move |path, size, tint, color| {
                count.fetch_add(1, Ordering::Relaxed);
                crate::icons::app_worker::tests::pixels(path, size, tint, color)
            },
            |rx| {
                let mut start = Start::default();
                start.open = true;
                assert!(start.icons.get("fixture", 32, "test").is_none());
                wait_delivery(rx);
                assert!(start.icons.get("fixture", 32, "test").is_some());
                start.close();
                start.opening.jump(0.0);
                start.release_caches();
                start.release();
                start.open = true;
                assert!(start.icons.get("fixture", 32, "test").is_some());
                assert_eq!(decodes.load(Ordering::Relaxed), 1);
            },
        );
    }
    #[test]
    fn start_stays_left_and_static_frames_do_not_allocate() {
        use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let mut renderer =
            unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap();
        let output = Output::new(
            "start-cache".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        let mut start = Start::default();
        start.open = true;
        start.output = Some(output.clone());
        start.opening.jump(1.0);
        let mut elements = Vec::new();
        start.append(
            &mut renderer,
            &output,
            (1646, 926).into(),
            Duration::ZERO,
            Align::Center,
            [0.9, 0.6, 0.4, 1.0],
            "Adwaita",
            &mut elements,
        );
        assert_eq!(start.panel.unwrap().loc.x, 8);
        let count = super::super::cache::performance::count(|| {
            for i in 1..180 {
                elements.clear();
                start.append(
                    &mut renderer,
                    &output,
                    (1646, 926).into(),
                    Duration::from_millis(i * 8),
                    Align::Center,
                    [0.9, 0.6, 0.4, 1.0],
                    "Adwaita",
                    &mut elements,
                );
            }
        });
        assert_eq!(count, 0);
    }
}
