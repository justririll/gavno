//! Taskbar geometry and app actions share the dock's app identity and window groups.
use super::{cache::CachedElement, Align, TASKBAR_HEIGHT};
use crate::{
    dock::App,
    icons::Icons,
    module::Feature,
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        Canvas, FULL_COLOR, GLASS, LINE,
    },
    ui::{
        style::{self as look, radius, size as tsize, text as tstyle, TEXT},
        text::Weight,
    },
    Gavno,
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    desktop::Window,
    output::Output,
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{IsAlive, Logical, Point, Size},
};
use std::{
    hash::{Hash, Hasher},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub struct Layout {
    pub bar: Rect,
    pub start: Rect,
    pub apps: Vec<Rect>,
    pub tray: Rect,
    pub clock: Rect,
    pub desktop: Rect,
}
impl Layout {
    pub fn new(size: Size<i32, Logical>, count: usize, align: Align) -> Self {
        let y = size.h - TASKBAR_HEIGHT;
        let desktop = rect(size.w - 8, y, 8, TASKBAR_HEIGHT);
        let clock = rect(desktop.loc.x - 104, y + 4, 100, 40);
        let tray = rect(clock.loc.x - 108, y + 4, 104, 40);
        let slots = ((tray.loc.x - 16) / 44).max(1) as usize;
        let count = count.min(slots.saturating_sub(1));
        let width = (count as i32 * 44 - 4).max(0);
        // Start stays at the left edge. Center alignment applies to app items.
        let x = match align {
            Align::Left => 52,
            Align::Center => ((size.w - width) / 2).min(tray.loc.x - width - 8).max(52),
        };
        Self {
            bar: rect(0, y, size.w, TASKBAR_HEIGHT),
            start: rect(8, y + 4, 40, 40),
            apps: (0..count)
                .map(|i| rect(x + i as i32 * 44, y + 4, 40, 40))
                .collect(),
            tray,
            clock,
            desktop,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    Start,
    App(String),
    Quick,
    Clock,
    Desktop,
    Focus(Window),
    New(String),
    Pin(String),
    Close(String),
    Consume,
}
struct View {
    output: Output,
    canvas: Canvas,
    layout: Layout,
    hits: Vec<(Rect, Hit)>,
    visible: bool,
    cached: Vec<CachedElement>,
    key: Option<(u64, u64, u64, (i32, i32), usize, usize, u8, u64, [u32; 4])>,
    status: crate::topbar::status::Snapshot,
}
#[derive(Default)]
pub struct Taskbar {
    pub(crate) apps: Vec<App>,
    pub focused: Option<Window>,
    pub pins: Option<Vec<String>>,
    views: Vec<View>,
    icons: Icons,
    menu: Option<(Output, String)>,
    hover: Option<(Output, Hit)>,
    pub hover_timer: Option<RegistrationToken>,
    tooltip: bool,
    offset: usize,
    revision: u64,
    model_key: Option<u64>,
    /// The distribution logo icon and whether it is symbolic; None draws the
    /// built-in four panes.
    pub(crate) logo: Option<(String, bool)>,
}
impl Taskbar {
    /// Drop per-output render caches and icons after leaving Classic.
    pub fn release_caches(&mut self) {
        self.views = Vec::new();
        self.icons = Default::default();
        self.model_key = None;
        self.revision = self.revision.wrapping_add(1);
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Launch,
    Focus(usize),
    Minimize,
    Cycle(usize),
}
pub fn click(count: usize, focused: Option<usize>) -> Click {
    match (count, focused) {
        (0, _) => Click::Launch,
        (1, Some(0)) => Click::Minimize,
        (_, Some(i)) => Click::Cycle((i + 1) % count),
        _ => Click::Focus(0),
    }
}
impl Taskbar {
    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn release(&mut self) {
        if self.model_key.is_none() && self.views.is_empty() {
            return;
        }
        self.apps = Vec::new();
        self.views = Vec::new();
        self.icons = Icons::default();
        self.menu = None;
        self.focused = None;
        self.hover = None;
        self.tooltip = false;
        self.model_key = None;
        self.invalidate();
    }
    pub fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            self.invalidate();
        }
    }
    pub fn refresh(&mut self, dock: &mut crate::dock::Dock, pins: &[String], outputs: &[Output]) {
        if self.pins.is_none() {
            let mut seen = std::collections::HashSet::new();
            self.pins = Some(
                pins.iter()
                    .take(128)
                    .map(|id| dock.canonical(id))
                    .filter(|id| seen.insert(id.clone()))
                    .collect(),
            );
        }
        let pins = self.pins.as_deref().unwrap_or_default();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        pins.hash(&mut hash);
        let model = hash.finish();
        self.views.retain(|v| outputs.contains(&v.output));
        if self.model_key == Some(model) && !dock.changed {
            return;
        }
        self.model_key = Some(model);
        self.revision = self.revision.wrapping_add(1);
        let mut apps = dock.classic_apps(pins);
        // Workspace focus changes MRU order. Keep each group's cycle stable
        // across those changes, including groups spanning several workspaces.
        for app in &mut apps {
            if let Some(previous) = self.apps.iter().find(|old| old.id == app.id) {
                app.windows.sort_by_key(|w| {
                    previous
                        .windows
                        .iter()
                        .position(|old| old == w)
                        .unwrap_or(usize::MAX)
                });
            }
        }
        self.apps = apps;
        self.views.retain(|v| outputs.contains(&v.output));
        if self
            .menu
            .as_ref()
            .is_some_and(|(o, id)| !outputs.contains(o) || !self.apps.iter().any(|a| &a.id == id))
        {
            self.menu = None;
        }
    }
    pub fn contains(
        &self,
        output: &Output,
        p: Point<f64, Logical>,
        size: Size<i32, Logical>,
    ) -> bool {
        if self.menu.as_ref().is_some_and(|(o, _)| o == output) {
            return true;
        }
        self.views
            .iter()
            .find(|v| &v.output == output)
            .map_or(p.y >= (size.h - TASKBAR_HEIGHT) as f64, |v| {
                v.visible && v.hits.iter().any(|(r, _)| r.to_f64().contains(p))
            })
    }
    fn hit(&self, output: &Output, p: Point<f64, Logical>) -> Option<Hit> {
        self.views
            .iter()
            .find(|v| &v.output == output && v.visible)?
            .hits
            .iter()
            .rev()
            .find(|(r, _)| r.to_f64().contains(p))
            .map(|(_, h)| h.clone())
    }
    pub fn target(&self, output: &Output, id: &str, geo: Rect) -> crate::anim::Rect {
        let r = self
            .views
            .iter()
            .find(|v| &v.output == output)
            .and_then(|v| {
                v.hits
                    .iter()
                    .find(|(_, h)| *h == Hit::App(id.to_owned()))
                    .map(|(r, _)| *r)
            })
            .unwrap_or(rect(geo.size.w / 2 - 20, geo.size.h - 44, 40, 40));
        let mut r = r.to_f64();
        r.loc += geo.loc.to_f64();
        r
    }
    pub fn append(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        p: f64,
        align: Align,
        focused: Option<&Window>,
        status: &crate::topbar::status::Snapshot,
        accent: [f32; 4],
        theme: &str,
        visible: bool,
        out: &mut Vec<GavnoRenderElement>,
    ) {
        if p <= 0.001 {
            return;
        }
        if !visible {
            if let Some(v) = self.views.iter_mut().find(|v| &v.output == output) {
                if v.visible {
                    v.visible = false;
                    v.hits.clear();
                    v.cached = Vec::new();
                    v.canvas = Canvas::default();
                    v.key = None;
                }
            }
            return;
        }
        let index = self
            .views
            .iter()
            .position(|v| &v.output == output)
            .unwrap_or_else(|| {
                self.views.push(View {
                    output: output.clone(),
                    canvas: Default::default(),
                    layout: Layout::new(size, 0, align),
                    hits: Vec::new(),
                    visible: false,
                    cached: Vec::new(),
                    key: None,
                    status: Default::default(),
                });
                self.views.len() - 1
            });
        let minute = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / 60;
        let focused_index = focused
            .and_then(|w| self.apps.iter().position(|a| a.windows.contains(w)))
            .unwrap_or(usize::MAX);
        let key = (
            self.revision,
            p.to_bits(),
            output.current_scale().fractional_scale().to_bits(),
            (size.w, size.h),
            focused_index,
            self.offset,
            align as u8,
            minute,
            accent.map(f32::to_bits),
        );
        let v = &mut self.views[index];
        if v.key == Some(key) && v.status == *status {
            out.extend(v.cached.iter().cloned().map(GavnoRenderElement::Desktop));
            return;
        }
        v.key = Some(key);
        if v.status != *status {
            v.status = status.clone();
        }
        v.hits.clear();
        v.visible = true;
        self.offset = self.offset.min(self.apps.len().saturating_sub(1));
        v.layout = Layout::new(size, self.apps.len().saturating_sub(self.offset), align);
        let l = &v.layout;
        let dy = ((1.0 - p) * TASKBAR_HEIGHT as f64).round() as i32;
        let c = &mut v.canvas;
        c.begin(renderer, output.current_scale().fractional_scale());
        c.text.begin_frame_cache();
        c.anchor = (0.0, 0.0).into();
        let shift = |mut r: Rect| {
            r.loc.y += dy;
            r
        };
        c.rect(shift(l.bar), 0.0, GLASS, 0.0, LINE);
        c.rect(shift(rect(0, l.bar.loc.y, size.w, 1)), 0.0, LINE, 0.0, LINE);
        v.hits.push((shift(l.bar), Hit::Consume));
        let mut button = |c: &mut Canvas, r: Rect, hit: Hit| {
            let r = shift(r);
            if self
                .hover
                .as_ref()
                .is_some_and(|(o, h)| o == output && h == &hit)
            {
                c.rect(r, 5.0, [1.0, 1.0, 1.0, 0.09], 0.0, LINE);
            }
            v.hits.push((r, hit));
            r
        };
        let s = button(c, l.start, Hit::Start);
        let logo_rect = rect(s.loc.x + 8, s.loc.y + 8, 24, 24);
        let drawn = match &self.logo {
            Some((name, true)) => c.icon(renderer, &mut self.icons, &[name], theme, logo_rect),
            Some((name, false)) => c.colored_icon(renderer, &mut self.icons, &[name], theme, logo_rect, Some(FULL_COLOR)),
            None => false,
        };
        // Four panes form the GAVNO Start glyph without a font dependency;
        // they stand in until the distribution logo is resolved and decoded.
        if !drawn {
            for (dx, dy) in [(0, 0), (11, 0), (0, 11), (11, 11)] {
                c.rect(
                    rect(s.loc.x + 10 + dx, s.loc.y + 10 + dy, 9, 9),
                    1.0,
                    accent,
                    0.0,
                    LINE,
                );
            }
        }
        self.icons.nonblocking_apps();
        for (app, r) in self.apps.iter().skip(self.offset).zip(&l.apps) {
            let r = button(c, *r, Hit::App(app.id.clone()));
            let active = focused.is_some_and(|w| app.windows.contains(w));
            if !c.colored_icon(
                renderer,
                &mut self.icons,
                &[&app.icon],
                theme,
                rect(r.loc.x + 7, r.loc.y + 6, 26, 26),
                Some(FULL_COLOR),
            ) {
                c.icon(
                    renderer,
                    &mut self.icons,
                    &["application-x-executable-symbolic"],
                    theme,
                    rect(r.loc.x + 8, r.loc.y + 7, 24, 24),
                );
            }
            if !app.windows.is_empty() {
                let width = if active { 18 } else { 7 };
                c.rect(
                    rect(r.loc.x + (40 - width) / 2, r.loc.y + 36, width, 3),
                    1.5,
                    if active {
                        accent
                    } else {
                        [0.7, 0.7, 0.75, 0.8]
                    },
                    0.0,
                    LINE,
                );
            }
        }
        let tray = button(c, l.tray, Hit::Quick);
        let battery = status
            .battery
            .as_ref()
            .map(crate::topbar::status::battery_icons);
        let slots = [
            status.network.icon(),
            crate::topbar::status::volume_icon(status.volume),
            battery.as_ref().map(|n| n[0].as_str()).unwrap_or("battery-missing-symbolic"),
        ];
        for (i, name) in slots.into_iter().enumerate() {
            let r = rect(tray.loc.x + 10 + i as i32 * 32, tray.loc.y + 12, 16, 16);
            c.icon(renderer, &mut self.icons, &[name], theme, r);
        }
        let clock = button(c, l.clock, Hit::Clock);
        let time = crate::ui::theme::LocalTime::now();
        let hhmm = format!("{:02}:{:02}", time.hour, time.minute);
        let st = tstyle(tsize::LABEL, Weight::SemiBold, TEXT);
        let w = look::tabular_width(c, &hhmm, st);
        look::tabular_label(c, renderer, &hhmm, clock.loc.x + (clock.size.w - w) / 2, clock.loc.y + 3, st);
        let date = format!("{:02}.{:02}.{}", time.day, time.month, time.year);
        let st = tstyle(tsize::CAPTION, Weight::Medium, look::dim(TEXT, 0.7));
        let w = look::tabular_width(c, &date, st);
        look::tabular_label(c, renderer, &date, clock.loc.x + (clock.size.w - w) / 2, clock.loc.y + 22, st);
        let r = button(c, l.desktop, Hit::Desktop);
        c.rect(rect(r.loc.x, r.loc.y + 8, 1, 32), 0.0, LINE, 0.0, LINE);
        if let Some((o, id)) = &self.menu {
            if o == output {
                if let Some(app) = self.apps.iter().find(|a| &a.id == id) {
                    let rows = app.windows.len().min(12) + 3;
                    let x = v
                        .hits
                        .iter()
                        .find(|(_, h)| *h == Hit::App(id.clone()))
                        .map_or(l.start.loc.x, |(r, _)| r.loc.x)
                        .min((size.w - 308).max(8));
                    let panel = rect(
                        x,
                        (size.h - 56 - rows as i32 * 32 - 16).max(8),
                        300,
                        rows as i32 * 32 + 16,
                    );
                    look::card(c, panel, radius::TILE);
                    v.hits.push((panel, Hit::Consume));
                    let mut actions: Vec<(String, Hit)> = app
                        .windows
                        .iter()
                        .take(12)
                        .map(|w| (crate::overlay::window_labels(w).0, Hit::Focus(w.clone())))
                        .collect();
                    actions.push(("New window".into(), Hit::New(id.clone())));
                    actions.push((
                        if self.pins.as_ref().is_some_and(|p| p.contains(id)) {
                            "Unpin"
                        } else {
                            "Pin"
                        }
                        .into(),
                        Hit::Pin(id.clone()),
                    ));
                    actions.push((
                        if app.windows.len() > 1 {
                            "Close all windows"
                        } else {
                            "Close window"
                        }
                        .into(),
                        Hit::Close(id.clone()),
                    ));
                    for (i, (title, hit)) in actions.into_iter().enumerate() {
                        let r = rect(x + 8, panel.loc.y + 8 + i as i32 * 32, 284, 32);
                        let hot = self.hover.as_ref().is_some_and(|(o, h)| o == output && *h == hit);
                        if hot {
                            look::row_bg(c, r, true);
                        }
                        let danger = matches!(hit, Hit::Close(_));
                        c.styled_label(
                            renderer,
                            &title,
                            r.loc.x + 12,
                            r.loc.y + 8,
                            260,
                            tstyle(tsize::LABEL, Weight::Medium, if danger { look::DANGER } else { TEXT }),
                        );
                        v.hits.push((r, hit));
                    }
                }
            }
        } else if self.tooltip {
            if let Some((o, Hit::App(id))) = &self.hover {
                if o == output {
                    if let Some(app) = self.apps.iter().find(|a| &a.id == id) {
                        let title = app
                            .windows
                            .first()
                            .map(|w| crate::overlay::window_labels(w).0)
                            .unwrap_or_else(|| app.name.clone());
                        let x = v
                            .hits
                            .iter()
                            .find(|(_, h)| *h == Hit::App(id.clone()))
                            .map_or(8, |(r, _)| r.loc.x)
                            .min((size.w - 288).max(8));
                        let r = rect(x, size.h - 90, 280, 30);
                        look::card(c, r, radius::ROW);
                        c.styled_label(renderer, &title, x + 12, r.loc.y + 7, 256, tstyle(tsize::LABEL, Weight::Medium, TEXT));
                    }
                }
            }
        }
        v.cached = c
            .finish()
            .into_iter()
            .filter_map(CachedElement::from_ui)
            .collect();
        c.text.end_frame_cache();
        out.extend(v.cached.iter().cloned().map(GavnoRenderElement::Desktop));
    }
}
impl Gavno {
    pub fn classic_activate(&mut self, index: usize, new: bool) {
        let action = std::time::Instant::now();
        self.guard(Feature::Classic, (), |gv| {
            let Some(app) = gv.classic.taskbar.apps.get(index).cloned() else {
                return;
            };
            let windows: Vec<_> = app.windows.iter().filter(|w| w.alive()).cloned().collect();
            let selected = gv
                .focused_window()
                .and_then(|w| windows.iter().position(|v| v == &w));
            match if new {
                Click::Launch
            } else {
                click(windows.len(), selected)
            } {
                Click::Launch => {
                    if let Some(cmd) = app.entry.and_then(|e| e.command(&gv.config.terminal)) {
                        gv.launch_shell_at(action, &cmd, Some(&app.id));
                    }
                }
                Click::Minimize => gv.minimize_window(&windows[0]),
                Click::Focus(i) | Click::Cycle(i) => {
                    gv.focus_window(windows.get(i));
                    gv.close_overview();
                }
            }
            gv.classic.start.close();
            gv.queue_redraw_all();
        });
    }
    pub fn classic_motion(&mut self, pos: Point<f64, Logical>) {
        self.guard(Feature::Classic, (), |gv| {
            let hover = gv.space.outputs().find_map(|o| {
                let geo = gv.space.output_geometry(o)?;
                if !geo.to_f64().contains(pos) || !gv.shell_visible(o) {
                    return None;
                }
                gv.classic
                    .taskbar
                    .hit(o, pos - geo.loc.to_f64())
                    .map(|h| (o.clone(), h))
            });
            if hover == gv.classic.taskbar.hover {
                return;
            }
            if let Some(t) = gv.classic.taskbar.hover_timer.take() {
                gv.loop_handle.remove(t);
            }
            gv.classic.taskbar.hover = hover.clone();
            gv.classic.taskbar.tooltip = false;
            gv.classic.taskbar.invalidate();
            if matches!(hover, Some((_, Hit::App(_)))) {
                let result = gv.loop_handle.insert_source(
                    Timer::from_duration(Duration::from_millis(500)),
                    move |_, _, gv| {
                        let _stall = crate::perf::Stall::new("timer.taskbar_hover");
                        gv.guard(Feature::Classic, (), |gv| {
                            gv.classic.taskbar.hover_timer = None;
                            if gv.classic.taskbar.hover == hover {
                                gv.classic.taskbar.tooltip = true;
                                gv.classic.taskbar.invalidate();
                                gv.queue_redraw_all();
                            }
                        });
                        TimeoutAction::Drop
                    },
                );
                if let Ok(t) = result {
                    gv.classic.taskbar.hover_timer = Some(t);
                }
            }
            gv.queue_redraw_all();
        });
    }
    pub fn classic_button(&mut self, button: u32, pressed: bool) -> bool {
        if !self.features.on(Feature::Classic)
            || self.shell_progress() < 0.001
            || self.seat.get_pointer().is_some_and(|p| p.is_grabbed())
        {
            return false;
        }
        self.guard(Feature::Classic, false, |gv| {
            let pos = gv.pointer_location();
            let Some(o) = gv.active_output() else {
                return false;
            };
            if !gv.shell_visible(&o) && !gv.classic.start.open {
                return false;
            }
            let Some(geo) = gv.space.output_geometry(&o) else {
                return false;
            };
            let p = pos - geo.loc.to_f64();
            if gv.classic_start_button(&o, p, button, pressed) {
                return true;
            }
            let hit = gv.classic.taskbar.hit(&o, p);
            let menu = gv.classic.taskbar.menu.is_some();
            if hit.is_none() && !menu {
                return false;
            }
            if !pressed {
                return true;
            }
            gv.classic.taskbar.close_menu();
            gv.classic.taskbar.invalidate();
            if let Some(hit) = hit {
                match hit {
                    Hit::Start => gv.toggle_start(),
                    Hit::Desktop => gv.show_classic_desktop(true),
                    Hit::Quick | Hit::Clock => {
                        gv.classic.start.close();
                        gv.bar_action(
                            &o,
                            if hit == Hit::Quick {
                                crate::topbar::BarAction::PowerMenu
                            } else {
                                crate::topbar::BarAction::Calendar
                            },
                        );
                    }
                    Hit::App(id) => {
                        gv.topbar.close();
                        if button == 0x111 {
                            gv.classic.taskbar.menu = Some((o, id));
                        } else if matches!(button, 0x110 | 0x112) {
                            if let Some(i) = gv.classic.taskbar.apps.iter().position(|a| a.id == id)
                            {
                                gv.classic_activate(i, button == 0x112);
                            }
                        }
                    }
                    Hit::Focus(w) => gv.focus_window(Some(&w)),
                    Hit::New(id) => {
                        if let Some(i) = gv.classic.taskbar.apps.iter().position(|a| a.id == id) {
                            gv.classic_activate(i, true);
                        }
                    }
                    Hit::Pin(id) => {
                        let pins = gv.classic.taskbar.pins.get_or_insert_with(Vec::new);
                        if let Some(i) = pins.iter().position(|p| p == &id) {
                            pins.remove(i);
                        } else {
                            pins.push(id);
                        }
                    }
                    Hit::Close(id) => {
                        if let Some(app) = gv.classic.taskbar.apps.iter().find(|a| a.id == id) {
                            for w in &app.windows {
                                if let Some(t) = w.toplevel() {
                                    t.send_close();
                                }
                            }
                        }
                    }
                    Hit::Consume => {}
                }
            }
            gv.queue_redraw_all();
            true
        })
    }
    pub fn classic_axis(&mut self, vertical: f64) -> bool {
        if !self.classic_mode() {
            return false;
        }
        self.guard(Feature::Classic, false, |gv| {
            if gv.classic.start.open {
                gv.classic.start.scroll(vertical);
                gv.queue_redraw_all();
                return true;
            }
            let Some(output) = gv.active_output() else {
                return false;
            };
            let Some(geo) = gv.space.output_geometry(&output) else {
                return false;
            };
            if gv
                .classic
                .taskbar
                .hit(&output, gv.pointer_location() - geo.loc.to_f64())
                .is_none()
            {
                return false;
            }
            if vertical != 0.0 {
                gv.classic.taskbar.offset = if vertical > 0.0 {
                    (gv.classic.taskbar.offset + 1)
                        .min(gv.classic.taskbar.apps.len().saturating_sub(1))
                } else {
                    gv.classic.taskbar.offset.saturating_sub(1)
                };
                gv.queue_redraw_all();
            }
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_keeps_start_first_and_tray_at_right() {
        for width in [1280, 1646] {
            for align in [Align::Center, Align::Left] {
                for count in [0, 4, 12, 128] {
                    let l = Layout::new((width, 926).into(), count, align);
                    assert_eq!(l.start.loc.x, 8);
                    if align == Align::Left {
                        assert!(l.apps.first().is_none_or(|r| r.loc.x == 52));
                    }
                    let mut boxes = vec![l.start];
                    boxes.extend(l.apps.iter().copied());
                    boxes.extend([l.tray, l.clock, l.desktop]);
                    for r in &boxes {
                        assert!(l.bar.contains_rect(*r), "{width} {align:?}: {r:?}");
                    }
                    for (i, r) in boxes.iter().enumerate() {
                        for other in &boxes[i + 1..] {
                            assert!(!r.overlaps(*other), "{width} {align:?}: {r:?}, {other:?}");
                        }
                    }
                    assert_eq!(l.desktop.loc.x + l.desktop.size.w, width);
                    assert_eq!(l.clock.loc.x + l.clock.size.w, l.desktop.loc.x - 4);
                    assert!(l.apps.first().is_none_or(|r| r.loc.x > l.start.loc.x));
                    assert!(l.apps.iter().all(|r| r.size.w == 40 && r.size.h == 40));
                }
            }
        }
    }
    #[test]
    fn item_click_behaviour_table() {
        for (count, focus, expected) in [
            (0, None, Click::Launch),
            (1, None, Click::Focus(0)),
            (1, Some(0), Click::Minimize),
            (3, None, Click::Focus(0)),
            (3, Some(0), Click::Cycle(1)),
            (3, Some(2), Click::Cycle(0)),
        ] {
            assert_eq!(click(count, focus), expected);
        }
    }
    #[test]
    fn activation_cycles_every_window_across_workspaces() {
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new(
            "cycle".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1280, 800).into(),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        h.gv.space.map_output(&output, (0, 0));
        let a = h.window();
        let b = h.window();
        let c = h.window();
        for w in [&a, &b, &c] {
            h.gv.map_new_window(w.clone());
        }
        h.gv.move_to_workspace(&c, 1);
        h.gv.switch_workspace(&output, 0);
        h.gv.focus_window(Some(&b));
        h.gv.classic.mode = super::super::ShellMode::Classic;
        h.gv.refresh_shell();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let index =
                h.gv.classic
                    .taskbar
                    .apps
                    .iter()
                    .position(|app| app.windows.contains(&a))
                    .unwrap();
            h.gv.classic_activate(index, false);
            seen.push(h.gv.focused_window().unwrap());
            h.gv.refresh_shell();
        }
        assert!(seen.contains(&a) && seen.contains(&b) && seen.contains(&c));
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn static_taskbar_frames_reuse_handles_without_allocating() {
        use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let mut renderer =
            unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap();
        let output = Output::new(
            "taskbar-cache".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        let mut bar = Taskbar::default();
        let status = crate::topbar::status::Snapshot::default();
        let mut elements = Vec::new();
        bar.append(
            &mut renderer,
            &output,
            (1646, 926).into(),
            1.0,
            Align::Left,
            None,
            &status,
            [0.9, 0.6, 0.4, 1.0],
            "Adwaita",
            true,
            &mut elements,
        );
        let count = super::super::cache::performance::count(|| {
            for _ in 0..180 {
                elements.clear();
                bar.append(
                    &mut renderer,
                    &output,
                    (1646, 926).into(),
                    1.0,
                    Align::Left,
                    None,
                    &status,
                    [0.9, 0.6, 0.4, 1.0],
                    "Adwaita",
                    true,
                    &mut elements,
                );
            }
        });
        assert_eq!(count, 0);
    }
}
