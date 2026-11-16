//! Built-in panel, shared cached drawing primitives, and visible-only one-shot timers.
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    ui::{
        shader::{self, ShaderBox, Shaders},
        spring::{Spring, SpringParams},
        text::{TextRenderer, TextStyleSpec, Weight},
        theme,
    },
    Gavno,
};
use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBufferRenderElement, Kind},
        gles::GlesRenderer,
    },
    output::Output,
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{Logical, Rectangle},
};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Sentinel for `Canvas::colored_icon`: draw the icon untinted.
pub(crate) const FULL_COLOR: [u8; 3] = [1, 2, 3];
pub const PANEL: i32 = 32;
pub const GLASS: [f32; 4] = [0.055, 0.065, 0.09, 0.90];
pub const LINE: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
#[path = "bar/calendar.rs"]
pub mod calendar;
#[path = "bar/control.rs"]
pub mod control;
#[path = "bar/layout.rs"]
pub mod geometry;
#[path = "bar/osd.rs"]
pub mod osd;
#[path = "bar/status.rs"]
pub mod status;
use geometry::{rect, Item, Layout, PopoverLayout, Rect};
use smithay::utils::Point;

pub fn next_minute(now: Duration) -> Duration {
    Duration::from_secs(60) - Duration::from_nanos((now.as_nanos() % 60_000_000_000) as u64)
}
fn wall_time() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}
fn clock_label() -> String {
    let t = theme::LocalTime::now();
    format!(
        "{} {:02} {} {:02}:{:02}",
        &t.weekday_name()[..3],
        t.day,
        &t.month_name()[..3],
        t.hour,
        t.minute
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}
pub fn read_battery(root: &Path) -> Option<Battery> {
    let mut batteries: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    batteries.sort();
    let read = |path: &Path, key: &str| {
        std::fs::read_to_string(path.join(key))
            .ok()
            .map(|s| s.trim().to_owned())
    };
    batteries.into_iter().find_map(|path| {
        if read(&path, "type")?.trim() != "Battery"
            || read(&path, "present").as_deref() == Some("0")
        {
            return None;
        }
        let percent = read(&path, "capacity")
            .and_then(|s| s.parse::<u32>().ok())
            .or_else(|| {
                for (now, full) in [("energy_now", "energy_full"), ("charge_now", "charge_full")] {
                    if let (Some(n), Some(f)) = (
                        read(&path, now).and_then(|v| v.parse::<u64>().ok()),
                        read(&path, full)
                            .and_then(|v| v.parse::<u64>().ok())
                            .filter(|v| *v > 0),
                    ) {
                        return Some((n.saturating_mul(100) / f).min(100) as u32);
                    }
                }
                None
            })?
            .min(100) as u8;
        let status = read(&path, "status").unwrap_or_default();
        Some(Battery {
            percent,
            charging: matches!(status.as_str(), "Charging" | "Full"),
        })
    })
}

/// Pool indices are stable between frames. Unchanged shapes keep their damage commits.
pub struct Canvas {
    shaders: Option<Shaders>,
    failed: bool,
    boxes: Vec<ShaderBox>,
    used: usize,
    pub text: TextRenderer,
    pub elements: Vec<GavnoRenderElement>,
    pub scale: f64,
    pub alpha: f32,
    pub zoom: f64,
    pub anchor: Point<f64, Logical>,
    #[cfg(test)]
    drawn: Vec<Rect>,
}
impl Default for Canvas {
    fn default() -> Self {
        Self {
            shaders: None,
            failed: false,
            boxes: Vec::new(),
            used: 0,
            text: TextRenderer::new(),
            elements: Vec::new(),
            scale: 1.0,
            alpha: 1.0,
            zoom: 1.0,
            anchor: (0.0, 0.0).into(),
            #[cfg(test)]
            drawn: Vec::new(),
        }
    }
}
impl Canvas {
    pub fn begin(&mut self, renderer: &mut GlesRenderer, scale: f64) {
        self.scale = scale;
        self.alpha = 1.0;
        self.zoom = 1.0;
        self.used = 0;
        #[cfg(test)]
        self.drawn.clear();
        self.elements.clear();
        if self.shaders.is_none() && !self.failed {
            match Shaders::compile(renderer) {
                Ok(s) => self.shaders = Some(s),
                Err(error) => {
                    self.failed = true;
                    tracing::warn!(%error, "desktop chrome shaders unavailable");
                }
            }
        }
    }
    pub fn rect(
        &mut self,
        rect: Rectangle<i32, Logical>,
        radius: f32,
        color: [f32; 4],
        border: f32,
        line: [f32; 4],
    ) {
        let rect = self.mapped(rect);
        #[cfg(test)]
        self.drawn.push(rect);
        if self.used == self.boxes.len() {
            self.boxes.push(ShaderBox::default());
        }
        if let Some(shaders) = &self.shaders {
            self.elements.push(GavnoRenderElement::Shader(shader::rrect(
                &mut self.boxes[self.used],
                shaders,
                rect,
                radius * self.scale as f32 * self.zoom as f32,
                color,
                border * self.scale as f32,
                line,
                self.alpha,
                self.scale,
            )));
        }
        self.used += 1;
    }
    pub fn shadow(&mut self, rect: Rectangle<i32, Logical>, radius: f32) {
        let rect = self.mapped(rect);
        if self.used == self.boxes.len() {
            self.boxes.push(ShaderBox::default());
        }
        if let Some(shaders) = &self.shaders {
            self.elements
                .push(GavnoRenderElement::Shader(shader::shadow(
                    &mut self.boxes[self.used],
                    shaders,
                    rect,
                    radius * self.zoom as f32,
                    20,
                    12,
                    [0.0, 0.0, 0.0, 0.38],
                    self.alpha,
                    self.scale,
                )));
        }
        self.used += 1;
    }
    pub fn label(
        &mut self,
        renderer: &mut GlesRenderer,
        value: &str,
        x: i32,
        y: i32,
        width: i32,
        size: f32,
    ) {
        if width <= 0 {
            return;
        }
        self.styled_label(renderer, value, x, y, width, label_style(size));
    }
    pub fn styled_label(
        &mut self,
        renderer: &mut GlesRenderer,
        value: &str,
        x: i32,
        y: i32,
        width: i32,
        style: TextStyleSpec,
    ) {
        let value = ellipsize(&mut self.text, value, style, width);
        if let Some(img) = self.text.render(&value, style, self.scale) {
            let destination = self.mapped(rect(x, y, img.logical_w, img.logical_h));
            if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                (
                    (destination.loc.x as f64 * self.scale).round(),
                    (destination.loc.y as f64 * self.scale).round(),
                ),
                &img.buffer,
                Some(self.alpha),
                Some(Rectangle::from_size(
                    (img.width as f64, img.height as f64).into(),
                )),
                Some(destination.size),
                Kind::Unspecified,
            ) {
                self.elements.push(GavnoRenderElement::Memory(e));
            }
        }
    }
    pub fn mapped(&self, r: Rect) -> Rect {
        rect(
            (self.anchor.x + (r.loc.x as f64 - self.anchor.x) * self.zoom).round() as i32,
            (self.anchor.y + (r.loc.y as f64 - self.anchor.y) * self.zoom).round() as i32,
            (r.size.w as f64 * self.zoom).round() as i32,
            (r.size.h as f64 * self.zoom).round() as i32,
        )
    }
    pub fn icon(
        &mut self,
        renderer: &mut GlesRenderer,
        icons: &mut Icons,
        names: &[&str],
        theme: &str,
        r: Rect,
    ) -> bool {
        self.colored_icon(renderer, icons, names, theme, r, None)
    }
    pub fn colored_icon(
        &mut self,
        renderer: &mut GlesRenderer,
        icons: &mut Icons,
        names: &[&str],
        theme: &str,
        r: Rect,
        color: Option<[u8; 3]>,
    ) -> bool {
        let px = (r.size.w as f64 * self.scale).ceil() as u32;
        let Some(buffer) = names.iter().find_map(|name| match color {
            Some(c) if c == FULL_COLOR => icons.get(name, px, theme),
            Some(color) => icons.get_tinted_color(name, px, theme, color),
            None => icons.get_tinted(name, px, theme),
        }) else {
            return false;
        };
        let r = self.mapped(r);
        if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (
                (r.loc.x as f64 * self.scale).round(),
                (r.loc.y as f64 * self.scale).round(),
            ),
            &buffer,
            Some(self.alpha),
            Some(Rectangle::from_size((px as f64, px as f64).into())),
            Some(r.size),
            Kind::Unspecified,
        ) {
            self.elements.push(GavnoRenderElement::Memory(e));
        }
        true
    }
    pub fn finish(&mut self) -> Vec<GavnoRenderElement> {
        std::mem::take(&mut self.elements)
            .into_iter()
            .rev()
            .collect()
    }
}

pub fn label_style(size: f32) -> TextStyleSpec {
    TextStyleSpec {
        size,
        weight: Weight::Medium,
        color: [0.96, 0.95, 0.98, 1.0],
        tracking: 0.0,
    }
}
pub fn ellipsize(text: &mut TextRenderer, value: &str, style: TextStyleSpec, width: i32) -> String {
    if width <= 0 {
        return String::new();
    }
    let mut value: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect();
    if text.measure(&value, style) > width as f32 {
        while !value.is_empty() && text.measure(&format!("{value}…"), style) > width as f32 {
            value.pop();
        }
        value.push('…');
        if text.measure(&value, style) > width as f32 {
            value.clear();
        }
    }
    value
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Lock,
    Logout,
    Suspend,
    Restart,
    Shutdown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarAction {
    Tiling,
    Workspace(usize),
    Layout,
    PowerMenu,
    Calendar,
    Month(i32),
    DoNotDisturb,
    ClearNotifications,
    Notification(u32),
    Power(PowerAction),
    Mute,
    Volume,
    Brightness,
    Network,
    Consume,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Popover {
    Quick,
    Calendar,
}
struct BarView {
    output: Output,
    canvas: Canvas,
    dots: Vec<Spring>,
    hits: Vec<(Rectangle<f64, Logical>, BarAction)>,
    visible: bool,
    hover: Option<BarAction>,
    layout: Option<Layout>,
    popover: Option<PopoverLayout>,
}
pub struct TopBar {
    pub notifications: crate::notify::Shared,
    pub notification_history: Vec<crate::notify::History>,
    pub notification_dnd: bool,
    pub notification_unread: bool,
    pub notifications_active: bool,
    views: Vec<BarView>,
    pub enabled: bool,
    pub title: String,
    pub layout: String,
    app_icon: String,
    entries: crate::desktop_entry::DesktopEntries,
    multiple_layouts: bool,
    clock: String,
    accent: [f32; 4],
    icons: Icons,
    icon_theme: String,
    pub menu: Option<Output>,
    popover: Popover,
    popover_output: Option<Output>,
    opening: Spring,
    month: (i32, u32),
    clock_timer: Option<RegistrationToken>,
    pub scroll: f64,
    status: status::Snapshot,
    drag: Option<(Output, BarAction)>,
}
impl Default for TopBar {
    fn default() -> Self {
        let today = theme::LocalTime::now();
        Self {
            notifications: Default::default(),
            notification_history: Vec::new(),
            notification_dnd: false,
            notification_unread: false,
            notifications_active: false,
            views: Vec::new(),
            enabled: true,
            title: String::new(),
            layout: "EN".into(),
            app_icon: String::new(),
            entries: Default::default(),
            multiple_layouts: true,
            clock: clock_label(),
            accent: [0.95, 0.64, 0.42, 1.0],
            icons: Default::default(),
            icon_theme: crate::config::DockConfig::default().icon_theme,
            menu: None,
            popover: Popover::Quick,
            popover_output: None,
            opening: Spring::new(0.0, SpringParams::SNAPPY),
            month: (today.year, today.month),
            clock_timer: None,
            scroll: 0.0,
            status: Default::default(),
            drag: None,
        }
    }
}
impl TopBar {
    pub fn accent(&self) -> [f32; 4] {
        self.accent
    }
    fn update_clock(&mut self) {
        self.clock = clock_label();
        let t = theme::LocalTime::now();
        let (lat, lon) = theme::location(t.utc_offset);
        let a = theme::palette(&t, lat, lon).accent;
        self.accent = [a[0], a[1], a[2], 1.0];
    }
    pub fn active(&self) -> bool {
        self.opening.is_active()
            || self
                .views
                .iter()
                .any(|v| v.visible && v.dots.iter().any(Spring::is_active))
    }
    pub fn close(&mut self) {
        self.menu = None;
        self.opening.set(0.0);
        self.drag = None;
    }
    pub fn action(&self, output: &Output, p: Point<f64, Logical>) -> Option<BarAction> {
        self.views
            .iter()
            .find(|v| &v.output == output && v.visible)?
            .hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains(p))
            .map(|(_, a)| a.clone())
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        width: i32,
        owner: Option<&crate::workspace::OutputWorkspaces>,
        now: Duration,
        visible: bool,
    ) -> Vec<GavnoRenderElement> {
        let index = if let Some(i) = self.views.iter().position(|v| &v.output == output) {
            i
        } else {
            self.views.push(BarView {
                output: output.clone(),
                canvas: Default::default(),
                dots: Vec::new(),
                hits: Vec::new(),
                visible,
                hover: None,
                layout: None,
                popover: None,
            });
            self.views.len() - 1
        };
        self.opening
            .set(if self.menu.is_some() { 1.0 } else { 0.0 });
        self.opening.advance(now);
        let v = &mut self.views[index];
        v.visible = visible && self.enabled;
        if !v.visible {
            v.hits.clear();
            v.popover = None;
            return Vec::new();
        }
        let c = &mut v.canvas;
        c.begin(renderer, output.current_scale().fractional_scale());
        v.hits.clear();
        c.rect(rect(0, 0, width, PANEL), 0.0, GLASS, 0.0, LINE);
        c.rect(
            rect(0, PANEL - 1, width, 1),
            0.0,
            [self.accent[0], self.accent[1], self.accent[2], 0.22],
            0.0,
            LINE,
        );
        let count = owner.map(|o| o.list.items.len()).unwrap_or(1);
        let active = owner.map(|o| o.list.active).unwrap_or(0);
        v.dots
            .resize_with(count, || Spring::new(7.0, SpringParams::SNAPPY));
        v.dots.truncate(count);
        let l = Layout::new(width, count as i32 * 12 + 31, self.multiple_layouts);
        capsule(
            c,
            l.workspaces,
            v.hover
                .as_ref()
                .is_some_and(|a| matches!(a, BarAction::Workspace(_))),
        );
        let mut x = l.workspaces.loc.x + 8;
        for (i, dot) in v.dots.iter_mut().enumerate() {
            dot.set(if i == active { 22.0 } else { 7.0 });
            dot.advance(now);
            let w = dot.value.round() as i32;
            if x + w > l.workspaces.loc.x + l.workspaces.size.w - 4 {
                break;
            }
            c.rect(
                rect(x, 12, w, 8),
                4.0,
                if i == active {
                    self.accent
                } else {
                    [0.96, 0.95, 0.98, 0.5]
                },
                0.0,
                LINE,
            );
            v.hits
                .push((rect(x - 2, 4, w + 5, 24).to_f64(), BarAction::Workspace(i)));
            x += w + 5;
        }
        capsule(c, l.toggle, v.hover == Some(BarAction::Tiling));
        let tiling = owner.is_some_and(|o| o.list.items[active].tiling);
        let names = if tiling {
            &["view-grid-symbolic", "view-app-grid-symbolic"][..]
        } else {
            &["view-restore-symbolic", "window-symbolic"][..]
        };
        if !c.icon(
            renderer,
            &mut self.icons,
            names,
            &self.icon_theme,
            rect(l.toggle.loc.x + 8, 8, 16, 16),
        ) {
            let x = l.toggle.loc.x + 8;
            for i in 0..if tiling { 4 } else { 2 } {
                c.rect(
                    rect(
                        x + (i % 2) * 8,
                        8 + (i / 2) * 8,
                        if tiling { 6 } else { 10 },
                        if tiling { 6 } else { 14 },
                    ),
                    2.0,
                    [0.85, 0.85, 0.9, 0.8],
                    1.0,
                    LINE,
                );
            }
        }
        v.hits.push((l.toggle.to_f64(), BarAction::Tiling));
        // The focused app keeps its own colours; tinting turned it into a
        // white square that looked like a dead button. No generic fallback.
        c.colored_icon(
            renderer,
            &mut self.icons,
            &[&self.app_icon],
            &self.icon_theme,
            l.app,
            Some(FULL_COLOR),
        );
        c.label(
            renderer,
            &self.title,
            l.title.loc.x,
            l.title.loc.y,
            l.title.size.w,
            12.5,
        );
        if v.hover == Some(BarAction::Calendar) {
            capsule(c, l.clock, true);
        }
        let mut clock_style = label_style(12.5);
        clock_style.weight = Weight::SemiBold;
        let clock_width = c.text.measure(&self.clock, clock_style).ceil() as i32;
        c.styled_label(
            renderer,
            &self.clock,
            l.clock.loc.x + (l.clock.size.w - clock_width) / 2,
            7,
            l.clock.size.w,
            clock_style,
        );
        if self.notification_unread && self.notifications_active {
            c.rect(rect(l.clock.loc.x + l.clock.size.w - 12, 13, 5, 5), 2.5, self.accent, 0.0, LINE);
        }
        v.hits.push((l.clock.to_f64(), BarAction::Calendar));
        capsule(
            c,
            l.status,
            matches!(v.hover, Some(BarAction::PowerMenu | BarAction::Layout)),
        );
        push_hit(c, &mut v.hits, l.status, BarAction::PowerMenu);
        if let Some(r) = l.layout {
            c.label(renderer, &self.layout, r.loc.x, 7, r.size.w, 11.5);
            v.hits.push((r.to_f64(), BarAction::Layout));
        }
        c.icon(
            renderer,
            &mut self.icons,
            &[self.status.network.icon(), "network-offline-symbolic"],
            &self.icon_theme,
            l.network,
        );
        c.icon(
            renderer,
            &mut self.icons,
            &[status::volume_icon(self.status.volume)],
            &self.icon_theme,
            l.volume,
        );
        draw_battery(
            c,
            renderer,
            &mut self.icons,
            &self.icon_theme,
            self.status.battery.as_ref(),
            l.battery,
            false,
        );
        v.layout = Some(l);
        v.popover = None;
        // Keep drawing the closing panel until its spring settles.
        if (self.popover_output.as_ref() == Some(output)) && self.opening.value > 0.001 {
            let p = if self.popover == Popover::Quick {
                PopoverLayout::quick_notifications(width, self.status.brightness.is_some(), self.notifications_active)
            } else {
                if self.notifications_active { PopoverLayout::calendar_notifications(width, self.notification_history.len()) }
                else { PopoverLayout::calendar(width) }
            };
            c.alpha = self.opening.value.clamp(0.0, 1.0) as f32;
            c.zoom = 0.94 + 0.06 * self.opening.value;
            c.anchor = (
                p.panel.loc.x as f64 + p.panel.size.w as f64 / 2.0,
                p.panel.loc.y as f64,
            )
                .into();
            c.shadow(p.panel, 20.0);
            c.rect(p.panel, 20.0, [0.065, 0.075, 0.10, 0.98], 1.0, LINE);
            push_hit(c, &mut v.hits, p.panel, BarAction::Consume);
            if self.popover == Popover::Quick {
                draw_quick(
                    c,
                    renderer,
                    &mut self.icons,
                    &self.icon_theme,
                    &self.status,
                    self.accent,
                    &p,
                    v.hover.as_ref(),
                    self.notification_dnd,
                );
            } else {
                draw_calendar(c, renderer, &p, self.month, self.accent, v.hover.as_ref());
                calendar::draw_notifications(c, renderer, &p, &self.notification_history, v.hover.as_ref());
            }
            for (item, r) in &p.items {
                let action = if let Item::Notification(i) = *item {
                    let Some(h) = self.notification_history.get(i) else { continue };
                    BarAction::Notification(h.notice.id)
                } else { item_action(*item) };
                push_hit(c, &mut v.hits, *r, action);
            }
            v.popover = Some(p);
        }
        c.finish()
    }
}
fn push_hit(
    c: &Canvas,
    hits: &mut Vec<(Rectangle<f64, Logical>, BarAction)>,
    r: Rect,
    action: BarAction,
) {
    hits.push((c.mapped(r).to_f64(), action));
}
fn capsule(c: &mut Canvas, r: Rect, hover: bool) {
    c.rect(
        r,
        9.0,
        [1.0, 1.0, 1.0, if hover { 0.14 } else { 0.065 }],
        0.0,
        LINE,
    );
}
fn item_action(item: Item) -> BarAction {
    match item {
        Item::VolumeIcon => BarAction::Mute,
        Item::Volume => BarAction::Volume,
        Item::Brightness => BarAction::Brightness,
        Item::Network => BarAction::Network,
        Item::Battery => BarAction::Consume,
        Item::DoNotDisturb => BarAction::DoNotDisturb,
        Item::ClearNotifications => BarAction::ClearNotifications,
        Item::Notification(i) => BarAction::Notification(i as u32),
        Item::Previous => BarAction::Month(-1),
        Item::Next => BarAction::Month(1),
        Item::Power(i) => BarAction::Power(POWER[i].0),
    }
}
const POWER: [(PowerAction, &str, &str); 5] = [
    (PowerAction::Lock, "system-lock-screen-symbolic", "Lock"),
    (
        PowerAction::Suspend,
        "media-playback-pause-symbolic",
        "Suspend",
    ),
    (PowerAction::Logout, "system-log-out-symbolic", "Log out"),
    (PowerAction::Restart, "system-reboot-symbolic", "Restart"),
    (
        PowerAction::Shutdown,
        "system-shutdown-symbolic",
        "Power off",
    ),
];
fn draw_battery(
    c: &mut Canvas,
    r: &mut GlesRenderer,
    icons: &mut Icons,
    theme: &str,
    battery: Option<&Battery>,
    area: Rect,
    detail: bool,
) {
    let Some(b) = battery else {
        c.label(r, "AC power", area.loc.x, area.loc.y, area.size.w, 12.0);
        return;
    };
    let names = status::battery_icons(b);
    let low = b.percent <= 15 && !b.charging;
    if low {
        c.rect(
            rect(area.loc.x - 3, area.loc.y - 3, 22, 22),
            6.0,
            [0.95, 0.25, 0.3, 0.35],
            0.0,
            LINE,
        );
    }
    c.colored_icon(
        r,
        icons,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
        theme,
        rect(area.loc.x, area.loc.y, 16, 16),
        low.then_some([255, 102, 110]),
    );
    let mut style = label_style(12.0);
    if low {
        style.color = [1.0, 0.40, 0.43, 1.0];
    }
    let text = if detail {
        format!(
            "{}% · {}",
            b.percent,
            if b.charging { "Charging" } else { "On battery" }
        )
    } else {
        format!("{}%", b.percent)
    };
    c.styled_label(
        r,
        &text,
        area.loc.x + 24,
        area.loc.y - 1,
        area.size.w - 24,
        style,
    );
}
fn draw_quick(
    c: &mut Canvas,
    r: &mut GlesRenderer,
    icons: &mut Icons,
    theme: &str,
    s: &status::Snapshot,
    accent: [f32; 4],
    p: &PopoverLayout,
    hover: Option<&BarAction>,
    dnd: bool,
) {
    for (item, area) in &p.items {
        let a = *area;
        match *item {
            Item::DoNotDisturb => {
                capsule(c, a, hover == Some(&BarAction::DoNotDisturb));
                c.label(r, "Do not disturb", a.loc.x + 12, a.loc.y + 12, 210, 12.5);
                c.label(r, if dnd { "On" } else { "Off" }, a.loc.x + 244, a.loc.y + 12, 36, 12.5);
            }
            Item::ClearNotifications | Item::Notification(_) => {}
            Item::VolumeIcon => {
                if hover == Some(&BarAction::Mute) {
                    capsule(c, a, true);
                }
                c.icon(
                    r,
                    icons,
                    &[status::volume_icon(s.volume)],
                    theme,
                    rect(a.loc.x + 8, a.loc.y + 8, 16, 16),
                );
            }
            Item::Volume | Item::Brightness => {
                let volume = *item == Item::Volume;
                let level = if volume {
                    s.volume.map(|v| v.level).unwrap_or(0.0)
                } else {
                    s.brightness.as_ref().map(|b| b.level()).unwrap_or(0.0)
                };
                if !volume {
                    c.icon(
                        r,
                        icons,
                        &["display-brightness-symbolic"],
                        theme,
                        rect(a.loc.x - 40, a.loc.y + 8, 16, 16),
                    );
                }
                c.rect(
                    rect(a.loc.x + 7, a.loc.y + 12, a.size.w - 14, 8),
                    4.0,
                    [1.0, 1.0, 1.0, 0.13],
                    0.0,
                    LINE,
                );
                let mut color = accent;
                if volume && s.volume.is_some_and(|v| v.muted) {
                    color[3] = 0.35;
                }
                c.rect(
                    rect(
                        a.loc.x + 7,
                        a.loc.y + 12,
                        ((a.size.w - 14) as f64 * level).round() as i32,
                        8,
                    ),
                    4.0,
                    color,
                    0.0,
                    LINE,
                );
                c.rect(
                    rect(
                        a.loc.x + ((a.size.w - 14) as f64 * level).round() as i32,
                        a.loc.y + 9,
                        14,
                        14,
                    ),
                    7.0,
                    [0.97, 0.97, 1.0, 1.0],
                    0.0,
                    LINE,
                );
            }
            Item::Network => {
                capsule(c, a, hover == Some(&BarAction::Network));
                c.icon(
                    r,
                    icons,
                    &[s.network.icon(), "network-offline-symbolic"],
                    theme,
                    rect(a.loc.x + 12, a.loc.y + 12, 16, 16),
                );
                c.label(
                    r,
                    s.network.label(),
                    a.loc.x + 40,
                    a.loc.y + 11,
                    a.size.w - 56,
                    13.0,
                );
            }
            Item::Battery => draw_battery(
                c,
                r,
                icons,
                theme,
                s.battery.as_ref(),
                rect(a.loc.x + 12, a.loc.y + 12, a.size.w - 24, 16),
                true,
            ),
            Item::Power(i) => {
                c.rect(
                    a,
                    20.0,
                    [
                        1.0,
                        1.0,
                        1.0,
                        if hover == Some(&BarAction::Power(POWER[i].0)) {
                            0.16
                        } else {
                            0.065
                        },
                    ],
                    0.0,
                    LINE,
                );
                if !c.icon(
                    r,
                    icons,
                    &[POWER[i].1],
                    theme,
                    rect(a.loc.x + 12, a.loc.y + 12, 16, 16),
                ) {
                    c.label(r, &POWER[i].2[..1], a.loc.x + 15, a.loc.y + 10, 20, 14.0);
                }
                if hover == Some(&BarAction::Power(POWER[i].0)) {
                    let w = c.text.measure(POWER[i].2, label_style(11.0)).ceil() as i32;
                    c.label(r, POWER[i].2, a.loc.x + (40 - w) / 2, a.loc.y + 48, w, 11.0);
                }
            }
            _ => {}
        }
    }
}
fn draw_calendar(
    c: &mut Canvas,
    r: &mut GlesRenderer,
    p: &PopoverLayout,
    month: (i32, u32),
    accent: [f32; 4],
    hover: Option<&BarAction>,
) {
    let x = p.panel.loc.x;
    let y = p.panel.loc.y;
    let header = format!("{} {}", calendar::MONTHS[month.1 as usize - 1], month.0);
    let w = c.text.measure(&header, label_style(14.0)).ceil() as i32;
    c.label(r, &header, x + (320 - w) / 2, y + 20, w, 14.0);
    for (item, a) in &p.items {
        if !matches!(item, Item::Previous | Item::Next) { continue; }
        if hover == Some(&item_action(*item)) {
            capsule(c, *a, true);
        }
        c.label(
            r,
            if *item == Item::Previous {
                "‹"
            } else {
                "›"
            },
            a.loc.x + 10,
            a.loc.y + 5,
            20,
            18.0,
        );
    }
    for (i, day) in ["M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
        let mut st = label_style(11.0);
        st.color[3] = 0.5;
        let w = c.text.measure(day, st).ceil() as i32;
        c.styled_label(r, day, x + 36 + i as i32 * 40 - w / 2, y + 64, 24, st);
    }
    let today = theme::LocalTime::now();
    for (i, date) in calendar::month_grid(month.0, month.1).iter().enumerate() {
        let a = rect(
            x + 20 + (i % 7) as i32 * 40,
            y + 88 + (i / 7) as i32 * 34,
            32,
            30,
        );
        if (date.year, date.month, date.day) == (today.year, today.month, today.day) {
            c.rect(a, 10.0, accent, 0.0, LINE);
        }
        let mut st = label_style(12.5);
        if date.month != month.1 {
            st.color[3] = 0.3;
        }
        if (date.year, date.month, date.day) == (today.year, today.month, today.day) {
            st.color = [0.08, 0.10, 0.14, 1.0];
        }
        let value = date.day.to_string();
        let w = c.text.measure(&value, st).ceil() as i32;
        c.styled_label(r, &value, a.loc.x + (32 - w) / 2, a.loc.y + 6, 32, st);
    }
}

impl Gavno {
    /// Re-evaluate visibility after protocol/input events; no source wakes a hidden panel.
    pub fn refresh_shell(&mut self) {
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        self.guard(crate::module::Feature::Dock, (), |gv| {
            gv.dock.refresh(&gv.config.dock, &gv.workspaces, &gv.space, gv.start_time.elapsed());
            gv.dock.drop_stale_menu(&outputs);
        });
        if self.topbar.menu.as_ref().is_some_and(|o| !outputs.contains(o)) {
            self.topbar.menu = None;
        }
        self.refresh_bar_status(outputs.iter().any(|o| self.shell_visible(o)) && self.features.on(crate::module::Feature::Bar));
        self.guard(crate::module::Feature::Bar, (), |gv| gv.refresh_bar(&outputs));
    }
    fn refresh_bar(&mut self, outputs: &[Output]) {
        let (title, app) = self
            .focused_window()
            .map(|w| crate::overlay::window_labels(&w))
            .unwrap_or_default();
        self.topbar.app_icon = self
            .topbar
            .entries
            .resolve(&app)
            .map(|e| e.icon)
            .unwrap_or(app);
        self.topbar.multiple_layouts = self
            .config
            .xkb_layout
            .split(',')
            .filter(|s| !s.is_empty())
            .count()
            > 1;
        let layouts = self.config.xkb_layout.clone();
        let layout = self
            .seat
            .get_keyboard()
            .map(|k| {
                k.with_xkb_state(self, |ctx| {
                    let xkb = ctx.xkb().lock().unwrap_or_else(|e| e.into_inner());
                    let index = xkb.active_layout().0 as usize;
                    short_layout(layouts.split(',').nth(index).unwrap_or("us"))
                })
            })
            .unwrap_or_else(|| "EN".into());
        let changed = self.topbar.title != title || self.topbar.layout != layout;
        self.topbar.title = title;
        self.topbar.layout = layout;
        self.topbar.enabled = self.config.bar.enabled;
        self.topbar.icon_theme = self.config.dock.icon_theme.clone();
        self.topbar.views.retain(|v| outputs.contains(&v.output));
        let visible = outputs.iter().any(|o| self.shell_visible(o));
        if !self.config.bar.enabled
            || self
                .topbar
                .menu
                .as_ref()
                .is_some_and(|o| !self.shell_visible(o))
        {
            self.topbar.close();
            self.topbar.opening.jump(0.0);
        }
        if !visible {
            self.topbar.close();
            self.topbar.opening.jump(0.0);
            self.guard(crate::module::Feature::Dock, (), |gv| gv.dock.close_menu());
        }
        if visible && self.config.bar.enabled {
            if self.topbar.clock_timer.is_none() {
                self.topbar.update_clock();
                self.schedule_bar_clock();
            }
        } else {
            if let Some(token) = self.topbar.clock_timer.take() {
                self.loop_handle.remove(token);
            }
        }
        if changed && visible {
            self.queue_redraw_all();
        }
    }
    fn schedule_bar_clock(&mut self) {
        self.guard(crate::module::Feature::Bar, Default::default(), |gv| gv.schedule_bar_clock_feature())
    }

    fn schedule_bar_clock_feature(&mut self) {
        match self.loop_handle.insert_source(
            Timer::from_duration(next_minute(wall_time())),
            |_, _, gv| {
                gv.guard(crate::module::Feature::Bar, (), |gv| {
                    gv.topbar.clock_timer = None;
                    gv.topbar.update_clock();
                    gv.queue_redraw_all();
                    if gv.space.outputs().any(|o| gv.shell_visible(o)) {
                        gv.schedule_bar_clock();
                    }

                });
                TimeoutAction::Drop
            },
        ) {
            Ok(t) => self.topbar.clock_timer = Some(t),
            Err(error) => tracing::warn!(%error,"cannot schedule panel clock"),
        }
    }
    pub fn bar_action(&mut self, output: &Output, action: BarAction) {
        self.guard(crate::module::Feature::Bar, Default::default(), |gv| gv.bar_action_feature(output, action))
    }

    fn bar_action_feature(&mut self, output: &Output, action: BarAction) {
        match action {
            BarAction::Tiling => self.toggle_workspace_tiling(output),
            BarAction::Workspace(i) => {
                self.switch_workspace(output, i);
            }
            BarAction::Layout => {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.with_xkb_state(self, |mut ctx| ctx.cycle_next_layout());
                }
            }
            BarAction::Calendar | BarAction::PowerMenu => {
                self.guard(crate::module::Feature::Dock, (), |gv| gv.dock.close_menu());
                let kind = if action == BarAction::Calendar {
                    Popover::Calendar
                } else {
                    Popover::Quick
                };
                if self.topbar.menu.as_ref() == Some(output) && self.topbar.popover == kind {
                    self.topbar.close();
                } else {
                    self.topbar.menu = Some(output.clone());
                    self.topbar.popover_output = Some(output.clone());
                    self.topbar.popover = kind;
                    if kind == Popover::Calendar {
                        self.guard(crate::module::Feature::Notifications, (), |gv| {
                            gv.notifications.borrow_mut().unread = false;
                            gv.refresh_notifications();
                        });
                    }
                    self.topbar.opening.set(1.0);
                    let today = theme::LocalTime::now();
                    self.topbar.month = (today.year, today.month);
                }
            }
            BarAction::DoNotDisturb => {
                self.guard(crate::module::Feature::Notifications, (), |gv| {
                    let dnd = gv.notifications.borrow().dnd;
                    gv.notifications.borrow_mut().dnd = !dnd;
                    gv.refresh_notifications();
                });
            }
            BarAction::ClearNotifications => {
                self.guard(crate::module::Feature::Notifications, (), |gv| {
                    gv.notifications.borrow_mut().clear_history();
                    gv.refresh_notifications();
                });
            }
            BarAction::Notification(id) => self.notification_history_action(id),
            BarAction::Month(delta) => {
                self.topbar.month =
                    calendar::shift_month(self.topbar.month.0, self.topbar.month.1, delta)
            }
            BarAction::Mute => self.bar_media(status::Media::Mute),
            BarAction::Volume | BarAction::Brightness => {
                self.topbar.drag = Some((output.clone(), action.clone()));
                self.bar_drag(self.pointer_location());
            }
            BarAction::Network => {
                std::thread::spawn(|| {
                    if let Ok(mut child) =
                        std::process::Command::new("nm-connection-editor").spawn()
                    {
                        let _ = child.wait();
                    }
                });
            }
            BarAction::Consume => {}
            BarAction::Power(action) => {
                self.topbar.close();
                match action {
                    PowerAction::Logout => self.quit(),
                    PowerAction::Lock => {
                        self.lock_session();
                    }
                    PowerAction::Suspend => power_command("systemctl", "suspend"),
                    PowerAction::Restart => power_command("systemctl", "reboot"),
                    PowerAction::Shutdown => power_command("systemctl", "poweroff"),
                }
            }
        }
        self.queue_redraw_all();
    }
}
fn power_command(program: &str, arg: &str) {
    match std::process::Command::new(program).arg(arg).spawn() {
        Ok(mut child) => {
            let program = program.to_owned();
            let arg = arg.to_owned();
            std::thread::spawn(move || match child.wait() {
                Ok(status) if status.success() => {}
                result => tracing::warn!(%program,%arg,?result,"session power command failed"),
            });
        }
        Err(error) => tracing::warn!(%program,%arg,%error,"session power command failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hits_match_drawn_status_and_transformed_popover_rectangles() {
        let mut c = Canvas::default();
        c.scale = 1.75;
        let status = Layout::new(1646, 55, true).status;
        let mut hits = Vec::new();
        capsule(&mut c, status, true);
        push_hit(&c, &mut hits, status, BarAction::PowerMenu);
        assert_eq!(hits[0].0, c.drawn[0].to_f64());
        for opening in [0.1, 0.5, 1.0] {
            for p in [
                PopoverLayout::quick(1646, true),
                PopoverLayout::quick(800, false),
                PopoverLayout::calendar(1280),
            ] {
                c.zoom = 0.94 + 0.06 * opening;
                c.anchor = (p.panel.loc.x as f64 + 160.0, p.panel.loc.y as f64).into();
                c.drawn.clear();
                hits.clear();
                for (item, r) in &p.items {
                    // The production painter and hit registration use the same
                    // logical item rect and the same animated Canvas transform.
                    capsule(&mut c, *r, false);
                    push_hit(&c, &mut hits, *r, item_action(*item));
                }
                for ((hit, _), drawn) in hits.iter().zip(&c.drawn) {
                    assert_eq!(*hit, drawn.to_f64());
                    assert!(hit.contains((drawn.loc.x as f64 + 0.1, drawn.loc.y as f64 + 0.1)));
                    assert!(!hit.contains((drawn.loc.x as f64 - 0.1, drawn.loc.y as f64 - 0.1)));
                }
            }
        }
    }
    #[test]
    fn minute_boundaries_preserve_subseconds() {
        assert_eq!(next_minute(Duration::ZERO), Duration::from_secs(60));
        assert_eq!(
            next_minute(Duration::from_millis(59_750)),
            Duration::from_millis(250)
        );
        assert_eq!(
            next_minute(Duration::from_secs(120)),
            Duration::from_secs(60)
        );
    }
    #[test]
    fn battery_fixture_and_invalid_values() {
        let root = std::env::temp_dir().join(format!("gavno-battery-{}", std::process::id()));
        let b = root.join("BAT0");
        std::fs::create_dir_all(&b).unwrap();
        for (key, value) in [
            ("type", "Battery\n"),
            ("capacity", "87\n"),
            ("status", "Charging\n"),
        ] {
            std::fs::write(b.join(key), value).unwrap();
        }
        assert_eq!(
            read_battery(&root),
            Some(Battery {
                percent: 87,
                charging: true
            })
        );
        std::fs::write(b.join("capacity"), "broken").unwrap();
        assert!(read_battery(&root).is_none());
        std::fs::write(b.join("energy_now"), "45").unwrap();
        std::fs::write(b.join("energy_full"), "60").unwrap();
        std::fs::write(b.join("status"), "Discharging").unwrap();
        assert_eq!(
            read_battery(&root),
            Some(Battery {
                percent: 75,
                charging: false
            })
        );
        std::fs::write(b.join("present"), "0").unwrap();
        assert!(read_battery(&root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl Gavno {
    pub(crate) fn reset_bar(&mut self) {
        if let Some(token) = self.topbar.clock_timer.take() { self.loop_handle.remove(token); }
        if let Some((_, _, token)) = self.media.repeat.take() { self.loop_handle.remove(token); }
        self.refresh_bar_status(false);
        self.topbar = Default::default();
        self.topbar.notifications = self.notifications.clone();
        self.loop_handle.insert_idle(|gv| gv.refit_fixed_windows());
    }
}

fn short_layout(layout: &str) -> String {
    match layout.trim() {
        "us" | "gb" => "EN".into(),
        "ru" => "RU".into(),
        other => other.chars().take(2).collect::<String>().to_uppercase(),
    }
}
