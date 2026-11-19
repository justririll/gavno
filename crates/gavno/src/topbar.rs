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
use geometry::{rect, Item, Layout, PopoverLayout, Rect, StatusItem};
use smithay::utils::Point;

pub fn next_minute(now: Duration) -> Duration {
    Duration::from_secs(60) - Duration::from_nanos((now.as_nanos() % 60_000_000_000) as u64)
}
pub(crate) fn wall_time() -> Duration {
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
    /// Minutes until empty (discharging) or full (charging), when the
    /// firmware reports a usable rate.
    pub minutes: Option<u32>,
}
impl Battery {
    /// "64%  ·  1 h 35 min left" for the quick settings footer.
    pub fn summary(&self) -> String {
        let time = self.minutes.map(|m| {
            let span = if m >= 60 { format!("{} h {:02} min", m / 60, m % 60) } else { format!("{m} min") };
            format!(" · {span} {}", if self.charging { "until full" } else { "left" })
        });
        format!("{}%{}", self.percent, time.unwrap_or_default())
    }
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
        let number = |key: &str| read(&path, key).and_then(|v| v.parse::<u64>().ok());
        // Energy (µWh) over power (µW), or charge (µAh) over current (µA).
        let (now, full, rate) = if number("energy_now").is_some() {
            (number("energy_now"), number("energy_full"), number("power_now"))
        } else {
            (number("charge_now"), number("charge_full"), number("current_now"))
        };
        let charging = status == "Charging";
        let minutes = rate.filter(|r| *r > 0).and_then(|rate| {
            let amount = if charging { full?.saturating_sub(now?) } else { now? };
            let minutes = amount.saturating_mul(60) / rate;
            (status == "Charging" || status == "Discharging")
                .then_some(minutes.min(99 * 60) as u32)
                .filter(|m| *m > 0)
        });
        Some(Battery {
            percent,
            charging: matches!(status.as_str(), "Charging" | "Full"),
            minutes,
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
    pub offset: Point<f64, Logical>,
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
            offset: (0.0, 0.0).into(),
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
        self.offset = (0.0, 0.0).into();
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
            (self.offset.x + self.anchor.x + (r.loc.x as f64 - self.anchor.x) * self.zoom).round() as i32,
            (self.offset.y + self.anchor.y + (r.loc.y as f64 - self.anchor.y) * self.zoom).round() as i32,
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
    /// Draws `buffer` (`pixels` large) scaled into `r`.
    pub fn image(&mut self, renderer: &mut GlesRenderer, buffer: &smithay::backend::renderer::element::memory::MemoryRenderBuffer, pixels: (u32, u32), r: Rect) {
        let r = self.mapped(r);
        if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            ((r.loc.x as f64 * self.scale).round(), (r.loc.y as f64 * self.scale).round()),
            buffer,
            Some(self.alpha),
            Some(Rectangle::from_size((pixels.0 as f64, pixels.1 as f64).into())),
            Some(r.size),
            Kind::Unspecified,
        ) {
            self.elements.push(GavnoRenderElement::Memory(e));
        }
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
        // Binary search the longest prefix that still fits with the ellipsis.
        let ends: Vec<usize> = value.char_indices().map(|(i, c)| i + c.len_utf8()).collect();
        let prefix = |k: usize| if k == 0 { 0 } else { ends[k - 1] };
        let (mut lo, mut hi) = (0, ends.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if text.measure(&format!("{}…", &value[..prefix(mid)]), style) <= width as f32 {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        value.truncate(prefix(lo));
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
    CodeMode,
    Media(crate::media::Command),
    MusicPanel,
    Workspace(usize),
    Layout,
    PowerMenu,
    Calendar,
    Month(i32),
    DoNotDisturb,
    ClearNotifications,
    Notification(u32),
    Power(PowerAction),
    Consume,
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
    pub controls: crate::panel_controls::State,
    /// Finger pointer camera indicator: `Some(hand_present)` while it runs.
    pub camera: Option<bool>,
    music_panel: bool,
    pub shell_progress: f64,
    pub shell_height: i32,
    pub notifications: crate::notify::Shared,
    pub now_playing: crate::media::Shared,
    pub notification_history: Vec<crate::notify::History>,
    pub notification_dnd: bool,
    pub notification_unread: bool,
    pub notifications_active: bool,
    views: Vec<BarView>,
    pub enabled: bool,
    /// Code mode chip and key hint strip state, refreshed on mode changes.
    pub code_mode: bool,
    pub hint_context: Option<crate::hints::HintContext>,
    /// The hints of `hint_context` whose binds are currently enabled.
    pub hint_items: Vec<&'static crate::hints::BindHint>,
    pub mod_label: &'static str,
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
    /// Mirrors `display.night_light` for the quick settings tile.
    night_light: bool,
    popover_output: Option<Output>,
    opening: Spring,
    month: (i32, u32),
    pub scroll: f64,
    status: status::Snapshot,
}
impl Default for TopBar {
    fn default() -> Self {
        let today = theme::LocalTime::now();
        Self {
            controls: Default::default(),
            camera: None,
            shell_progress: 0.0,
            shell_height: 0,
            notifications: Default::default(),
            now_playing: Default::default(),
            notification_history: Vec::new(),
            notification_dnd: false,
            notification_unread: false,
            notifications_active: false,
            views: Vec::new(),
            enabled: true,
            code_mode: false,
            hint_context: None,
            hint_items: Vec::new(),
            mod_label: "Super",
            title: String::new(),
            layout: "EN".into(),
            app_icon: String::new(),
            entries: Default::default(),
            multiple_layouts: true,
            clock: clock_label(),
            accent: [0.95, 0.64, 0.42, 1.0],
            icons: Icons::nonblocking(crate::icons::app_worker::Consumer::Chrome),
            icon_theme: crate::config::DockConfig::default().icon_theme,
            menu: None,
            night_light: false,
            music_panel: true,
            popover_output: None,
            opening: Spring::new(0.0, SpringParams::SNAPPY),
            month: (today.year, today.month),
            scroll: 0.0,
            status: Default::default(),
        }
    }
}
impl TopBar {
    pub fn status(&self) -> &status::Snapshot { &self.status }
    pub(crate) fn set_night_light(&mut self, on: bool) { self.night_light = on; }
    /// A bar view whose hit list is empty but whose layout is real.
    #[cfg(test)]
    pub(crate) fn test_view(&mut self, output: &Output, layout: Layout) {
        self.views.push(BarView {
            output: output.clone(), canvas: Canvas::default(), dots: vec![], hits: vec![], visible: true,
            hover: None, layout: Some(layout), popover: None,
        });
    }
    /// What the quick settings panel shows and toggles outside its own workers.
    pub(crate) fn quick_info(&self, classic: bool, media: bool) -> crate::panel_controls::Info {
        crate::panel_controls::Info {
            battery: self.status.battery.clone(),
            profile: self.status.profile.clone(),
            brightness: self.status.brightness.as_ref().map(|b| b.level()),
            dnd: self.notification_dnd,
            night_light: self.night_light,
            code_mode: self.code_mode,
            code_available: !classic,
            player: media && self.now_playing.borrow().snapshot.player.is_some(),
        }
    }
    /// The quick settings panel, painted above windows and below the polkit dialog.
    pub fn panel_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: smithay::utils::Size<i32, Logical>,
        scale: f64,
        accent: [f32; 4],
        theme: &str,
        classic: bool,
        media_on: bool,
    ) -> Vec<GavnoRenderElement> {
        if self.controls.open.as_ref() != Some(output) {
            return Vec::new();
        }
        let info = self.quick_info(classic, media_on);
        let media = if info.player {
            self.now_playing.borrow().snapshot.clone()
        } else {
            Default::default()
        };
        self.controls.elements(renderer, output, size, scale, accent, theme, classic, &info, &media)
    }
    pub(crate) fn set_panel_volume(&mut self, volume: Option<status::Volume>) { self.status.volume = volume; }
    pub(crate) fn clear_media_hits(&mut self) {
        for view in &mut self.views {
            view.hits.retain(|(_, action)| !matches!(action, BarAction::Media(_) | BarAction::MusicPanel));
        }
    }
    pub fn accent(&self) -> [f32; 4] {
        self.accent
    }
    pub(crate) fn update_clock(&mut self) -> bool {
        let next = clock_label();
        let changed = self.clock != next;
        self.clock = next;
        changed
    }
    pub(crate) fn set_accent(&mut self, accent: [f32; 4]) { self.accent = accent; }
    pub fn active(&self) -> bool {
        self.opening.is_active()
            || self.controls.animating()
            || self
                .views
                .iter()
                .any(|v| v.visible && v.dots.iter().any(Spring::is_active))
    }
    pub fn close(&mut self) {
        self.controls.close();
        self.menu = None;
        self.opening.set(0.0);
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
        features: &crate::module::Features,
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
        let code_mode = self.code_mode;
        let hint_context = self.hint_context;
        let hint_items = self.hint_items.clone();
        let mod_label = self.mod_label;
        let accent = self.accent;
        let v = &mut self.views[index];
        v.visible = visible && self.enabled;
        if !v.visible {
            v.hits.clear();
            v.popover = None;
            return Vec::new();
        }
        if self.shell_progress >= 0.999 && self.menu.is_none() && !self.opening.is_active() {
            if v.visible { v.canvas=Canvas::default(); }
            v.visible=false; v.hits.clear(); v.popover=None;
            for dot in &mut v.dots { dot.jump(dot.target); }
            return Vec::new();
        }
        let c = &mut v.canvas;
        c.begin(renderer, output.current_scale().fractional_scale());
        c.offset.y = -self.shell_progress * PANEL as f64;
        c.alpha = (1.0 - self.shell_progress) as f32;
        v.hits.clear();
        let count = owner.map(|o| o.list.items.len()).unwrap_or(1);
        let (l, media) = media_layout(&self.now_playing, width, count as i32 * 12 + 31, self.multiple_layouts, self.music_panel, features, self.status.battery.is_some(), self.camera.is_some());
        if self.shell_progress < 0.999 {
        c.rect(rect(0, 0, width, PANEL), 0.0, GLASS, 0.0, LINE);
        c.rect(
            rect(0, PANEL - 1, width, 1),
            0.0,
            [self.accent[0], self.accent[1], self.accent[2], 0.22],
            0.0,
            LINE,
        );
        let active = owner.map(|o| o.list.active).unwrap_or(0);
        v.dots
            .resize_with(count, || Spring::new(7.0, SpringParams::SNAPPY));
        v.dots.truncate(count);
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
        // The hint strip lives in the title area and never reaches the clock,
        // the workspace dots or any other fixed capsule.
        let strip = draw_hints(
            c,
            renderer,
            &l,
            code_mode,
            hint_context,
            &hint_items,
            mod_label,
            accent,
            v.hover.as_ref(),
            &mut v.hits,
        );
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
            (l.title.size.w - strip - if strip > 0 { 8 } else { 0 }).max(0),
            12.5,
        );
        if v.hover == Some(BarAction::Calendar) {
            capsule(c, l.clock, true);
        }
        let mut clock_style = label_style(12.5);
        clock_style.weight = Weight::SemiBold;
        let clock_width = crate::ui::style::tabular_width(c, &self.clock, clock_style);
        crate::ui::style::tabular_label(
            c,
            renderer,
            &self.clock,
            l.clock.loc.x + (l.clock.size.w - clock_width) / 2,
            7,
            clock_style,
        );
        if self.notification_unread && self.notifications_active {
            c.rect(rect(l.clock.loc.x + l.clock.size.w - 12, 13, 5, 5), 2.5, self.accent, 0.0, LINE);
        }
        v.hits.push((l.clock.to_f64(), BarAction::Calendar));
        // One capsule for the whole status cluster: a single hover highlight
        // and a single click target. The slots are laid out in `Layout`.
        capsule(
            c,
            l.status,
            matches!(v.hover, Some(BarAction::PowerMenu | BarAction::Layout)),
        );
        push_hit(c, &mut v.hits, l.status, BarAction::PowerMenu);
        for (item, area) in &l.slots {
            match item {
                StatusItem::Layout => {
                    let style = label_style(11.5);
                    let width = c.text.measure(&self.layout, style).ceil() as i32;
                    c.styled_label(renderer, &self.layout, area.loc.x + (area.size.w - width) / 2, 7, area.size.w, style);
                    push_hit(c, &mut v.hits, rect(area.loc.x - 4, 4, area.size.w + 8, 24), BarAction::Layout);
                }
                StatusItem::Camera => {
                    // Dim while no hand is in view, accent colour once it tracks.
                    let color = match self.camera {
                        Some(true) => accent_rgb(self.accent),
                        _ => [150, 150, 160],
                    };
                    c.colored_icon(renderer, &mut self.icons, &["camera-web-symbolic"], &self.icon_theme, *area, Some(color));
                }
                StatusItem::Wifi => {
                    c.icon(
                        renderer,
                        &mut self.icons,
                        &[self.controls.wifi.icon(), "network-offline-symbolic"],
                        &self.icon_theme,
                        *area,
                    );
                }
                StatusItem::Volume => {
                    c.icon(renderer, &mut self.icons, &[status::volume_icon(self.status.volume)], &self.icon_theme, *area);
                }
                StatusItem::Bluetooth => {
                    c.icon(renderer, &mut self.icons, &[self.controls.bluetooth.icon()], &self.icon_theme, *area);
                }
                StatusItem::Battery => draw_battery(c, renderer, self.status.battery.as_ref(), *area),
            }
        }
        features.read(crate::module::Feature::Media, (), || {
            if let (Some(area), Some(player)) = (l.media, media.player.as_ref()) {
                let action = BarAction::Media(crate::media::Command::PlayPause);
                capsule(c, area, v.hover == Some(action.clone()));
                crate::media::ui::capsule_content(c,renderer,&mut self.icons,&self.icon_theme,&media,player,area,v.hover==Some(action.clone()));
                if self.music_panel {
                    push_hit(c, &mut v.hits, rect(area.loc.x, area.loc.y, 28, area.size.h), action);
                    push_hit(c, &mut v.hits, rect(area.loc.x + 28, area.loc.y, area.size.w - 28, area.size.h), BarAction::MusicPanel);
                } else { push_hit(c, &mut v.hits, area, action); }
            } else if let Some(area) = l.media.filter(|_| self.music_panel && media.music_available) {
                let area = rect(area.loc.x, area.loc.y, 28, area.size.h);
                capsule(c, area, v.hover == Some(BarAction::MusicPanel));
                c.colored_icon(renderer, &mut self.icons, &["audio-x-generic-symbolic"], &self.icon_theme, rect(area.loc.x + 6, 8, 16, 16), Some(crate::media::ui::ICON_COLOR));
                push_hit(c, &mut v.hits, area, BarAction::MusicPanel);
            }
        });
        if self.shell_progress >= 0.999 { v.hits.clear(); }
        v.layout = Some(l);
        } else {
            v.layout=None;
            for dot in &mut v.dots { dot.jump(dot.target); }
        }
        v.popover = None;
        // Keep drawing the closing panel until its spring settles.
        if (self.popover_output.as_ref() == Some(output)) && self.opening.value > 0.001 {
            let mut p = if self.notifications_active { PopoverLayout::calendar_notifications(width, self.notification_history.len()) }
                else { PopoverLayout::calendar(width) };
            if self.shell_progress > 0.0 {
                p.anchor_bottom(width, self.shell_height, crate::classic::TASKBAR_HEIGHT, self.shell_progress);
            }
            c.offset = (0.0, 0.0).into();
            c.alpha = self.opening.value.clamp(0.0, 1.0) as f32;
            c.zoom = 0.96 + 0.04 * self.opening.value;
            c.anchor = (
                p.panel.loc.x as f64 + p.panel.size.w as f64 / 2.0,
                p.panel.loc.y as f64,
            )
                .into();
            crate::ui::style::card(c, p.panel, crate::ui::style::radius::PANEL);
            push_hit(c, &mut v.hits, p.panel, BarAction::Consume);
            draw_calendar(c, renderer, &mut self.icons, &self.icon_theme, &p, self.month, self.accent, v.hover.as_ref());
            calendar::draw_notifications(c, renderer, &mut self.icons, &self.icon_theme, &p, &self.notification_history, v.hover.as_ref());
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
fn accent_rgb(accent: [f32; 4]) -> [u8; 3] {
    [0, 1, 2].map(|i| (accent[i].clamp(0.0, 1.0) * 255.0).round() as u8)
}
fn media_layout(
    media: &crate::media::Shared, width: i32, workspace_width: i32, layouts: bool, music_panel: bool,
    features: &crate::module::Features, battery: bool, camera: bool,
) -> (Layout, crate::media::Snapshot) {
    let snapshot = features.read(crate::module::Feature::Media, Default::default(), || media.borrow().snapshot.clone());
    let slot = if battery { geometry::BATTERY_SLOT } else { geometry::CLUSTER_ICON };
    let mut layout = Layout::with_battery(width, workspace_width, layouts, slot, camera).with_media(snapshot.player.is_some());
    if snapshot.player.is_none() && music_panel && snapshot.music_available {
        let x = layout.clock.loc.x + layout.clock.size.w + 12;
        if layout.status.loc.x - x >= 40 { layout.media = Some(rect(x, 4, 28, 24)); }
    }
    (layout, snapshot)
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

/// Draws the code-mode chip and its context hint strip inside the title area.
/// Returns the width used; the title is ellipsized in the remaining space.
fn draw_hints(
    c: &mut Canvas,
    renderer: &mut GlesRenderer,
    layout: &Layout,
    code_mode: bool,
    context: Option<crate::hints::HintContext>,
    hint_items: &[&'static crate::hints::BindHint],
    mod_label: &str,
    accent: [f32; 4],
    hover: Option<&BarAction>,
    hits: &mut Vec<(Rectangle<f64, Logical>, BarAction)>,
) -> i32 {
    const CHIP: i32 = 44;
    const CHIP_GAP: i32 = 8;
    const ITEM_GAP: i32 = 14;
    if !code_mode {
        return 0;
    }
    let title = layout.title;
    let end = title.loc.x + title.size.w;
    if title.size.w < CHIP {
        return 0;
    }
    let mut label = label_style(10.5);
    label.color = [0.96, 0.95, 0.98, 0.62];
    label.weight = Weight::Regular;
    let mut key = label_style(10.5);
    key.color = [0.96, 0.95, 0.98, 0.95];
    key.weight = Weight::SemiBold;
    // `measure` follows the last rendered scale; prime it once per bar frame.
    let _ = c.text.render(" ", label_style(1.0), c.scale);
    let mut items: Vec<(&crate::hints::BindHint, String, i32, i32)> = Vec::new();
    if let Some(context) = context {
        for hint in hint_items.iter().copied() {
            let keys = match context {
                crate::hints::HintContext::Code => format!("{mod_label} {}", hint.keys),
                _ => hint.keys.to_owned(),
            };
            let key_width = c.text.measure(&keys, key).ceil() as i32 + 10;
            let label_width = c.text.measure(hint.label, label).ceil() as i32;
            items.push((hint, keys, key_width, label_width));
        }
    }
    let budget = (title.size.w - CHIP - CHIP_GAP).max(0);
    let refs: Vec<&crate::hints::BindHint> = items.iter().map(|(hint, ..)| *hint).collect();
    let chosen = crate::hints::fit(&refs, budget, ITEM_GAP, |hint| {
        items
            .iter()
            .find(|(candidate, ..)| std::ptr::eq(*candidate, hint))
            .map(|(_, _, key, label)| key + 6 + label)
            .unwrap_or(0)
    });
    let chosen_width: i32 = chosen
        .iter()
        .map(|hint| {
            items
                .iter()
                .find(|(candidate, ..)| std::ptr::eq(*candidate, *hint))
                .map(|(_, _, key, label)| key + 6 + label)
                .unwrap_or(0)
        })
        .sum::<i32>()
        + ITEM_GAP * chosen.len().saturating_sub(1) as i32;
    let mut total = CHIP + if chosen.is_empty() { 0 } else { CHIP_GAP + chosen_width };
    if total > title.size.w {
        total = CHIP;
    }
    let start = (end - total).max(title.loc.x);
    let chip = rect(start, 4, CHIP, 24);
    capsule(c, chip, hover == Some(&BarAction::CodeMode));
    let mut code = label_style(10.5);
    code.weight = Weight::SemiBold;
    code.color = [accent[0], accent[1], accent[2], 1.0];
    let code_width = c.text.measure("CODE", code).ceil() as i32;
    c.styled_label(renderer, "CODE", chip.loc.x + (CHIP - code_width) / 2, 9, code_width + 2, code);
    hits.push((c.mapped(chip).to_f64(), BarAction::CodeMode));
    if chosen.is_empty() || total == CHIP {
        return total;
    }
    let mut x = start + CHIP + CHIP_GAP;
    for hint in chosen {
        let Some((_, keys, key_width, label_width)) =
            items.iter().find(|(candidate, ..)| std::ptr::eq(*candidate, hint))
        else {
            continue;
        };
        let code_context = hint.context == crate::hints::HintContext::Code;
        let label_x = if code_context { x } else { x + *key_width + 6 };
        let key_x = if code_context { x + *label_width + 6 } else { x };
        if *label_width > 0 {
            c.styled_label(renderer, hint.label, label_x, 9, *label_width + 2, label);
        }
        c.rect(rect(key_x, 6, *key_width, 20), 5.0, [1.0, 1.0, 1.0, 0.10], 0.0, LINE);
        c.styled_label(renderer, keys, key_x + 5, 9, *key_width - 10, key);
        x += *label_width + 6 + *key_width + ITEM_GAP;
    }
    total
}
fn item_action(item: Item) -> BarAction {
    match item {
        Item::ClearNotifications => BarAction::ClearNotifications,
        Item::Notification(i) => BarAction::Notification(i as u32),
        Item::Previous => BarAction::Month(-1),
        Item::Next => BarAction::Month(1),
    }
}
pub(crate) const POWER: [(PowerAction, &str, &str); 5] = [
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
/// Battery in the status cluster: an outlined cell with a fill level and the
/// percentage, a bolt while charging. Without a battery (a desktop or a
/// nested session) only a plug is drawn, never text that could be cut off.
fn draw_battery(c: &mut Canvas, r: &mut GlesRenderer, battery: Option<&Battery>, area: Rect) {
    let Some(b) = battery else {
        plug_glyph(c, area, [0.96, 0.95, 0.98, 0.9]);
        return;
    };
    let low = b.percent <= 15 && !b.charging;
    let mut style = label_style(12.0);
    if low {
        style.color = [1.0, 0.40, 0.43, 1.0];
    }
    battery_glyph(c, rect(area.loc.x, area.loc.y + 3, 22, 10), b.percent, b.charging, style.color);
    c.styled_label(r, &format!("{}%", b.percent), area.loc.x + 28, area.loc.y - 1, area.size.w - 28, style);
}
/// A battery cell of any size at least 14x8: outline, nub, fill, optional bolt.
pub(crate) fn battery_glyph(c: &mut Canvas, area: Rect, percent: u8, charging: bool, color: [f32; 4]) {
    let body = rect(area.loc.x, area.loc.y, area.size.w - 2, area.size.h);
    c.rect(body, 2.5, [0.0; 4], 1.0, [color[0], color[1], color[2], color[3] * 0.85]);
    c.rect(rect(area.loc.x + area.size.w - 2, area.loc.y + area.size.h / 2 - 2, 2, 4), 1.0, [color[0], color[1], color[2], color[3] * 0.85], 0.0, LINE);
    let inner = rect(body.loc.x + 2, body.loc.y + 2, body.size.w - 4, body.size.h - 4);
    let fill = (inner.size.w * percent.min(100) as i32 / 100).max(if percent > 0 { 2 } else { 0 });
    if fill > 0 {
        c.rect(rect(inner.loc.x, inner.loc.y, fill, inner.size.h), 1.0, color, 0.0, LINE);
    }
    if charging {
        // Seven one pixel rows of a lightning bolt, centred in the cell.
        const BOLT: [(i32, i32); 7] = [(3, 3), (2, 3), (1, 3), (0, 5), (2, 3), (1, 3), (0, 3)];
        let x = body.loc.x + (body.size.w - 5) / 2;
        let y = body.loc.y + (body.size.h - 7) / 2;
        // A dark halo keeps the bolt readable over both the fill and the empty part.
        for (row, (dx, w)) in BOLT.iter().enumerate() {
            c.rect(rect(x + dx - 1, y + row as i32, w + 2, 1), 0.0, [0.04, 0.05, 0.08, 0.95], 0.0, LINE);
        }
        for (row, (dx, w)) in BOLT.iter().enumerate() {
            c.rect(rect(x + dx, y + row as i32, *w, 1), 0.0, [1.0; 4], 0.0, LINE);
        }
    }
}
/// Mains plug for machines without a battery, drawn in the 16 px icon cell.
fn plug_glyph(c: &mut Canvas, area: Rect, color: [f32; 4]) {
    let x = area.loc.x + (area.size.w - 16) / 2;
    let y = area.loc.y;
    c.rect(rect(x + 5, y + 1, 2, 5), 1.0, color, 0.0, LINE);
    c.rect(rect(x + 9, y + 1, 2, 5), 1.0, color, 0.0, LINE);
    c.rect(rect(x + 3, y + 5, 10, 6), 2.5, color, 0.0, LINE);
    c.rect(rect(x + 7, y + 10, 2, 5), 1.0, color, 0.0, LINE);
}
#[allow(clippy::too_many_arguments)]
fn draw_calendar(
    c: &mut Canvas,
    r: &mut GlesRenderer,
    icons: &mut Icons,
    theme_name: &str,
    p: &PopoverLayout,
    month: (i32, u32),
    accent: [f32; 4],
    hover: Option<&BarAction>,
) {
    use crate::ui::style::{self as look, radius, size, text, TEXT};
    let x = p.panel.loc.x;
    let y = p.panel.loc.y;
    let today = theme::LocalTime::now();
    // Today header: the date in words and the day number on an accent tile,
    // the same "on" look as a quick settings tile.
    let on = look::on_accent(accent);
    c.styled_label(r, "Today", x + 20, y + 14, 160, text(size::CAPTION, Weight::SemiBold, look::dim(accent, 0.95)));
    let date = format!("{}, {} {}", today.weekday_name(), today.day, today.month_name());
    c.styled_label(r, &date, x + 20, y + 30, 230, text(size::TITLE, Weight::SemiBold, TEXT));
    let tile = rect(x + 320 - 20 - 40, y + 12, 40, 40);
    c.rect(tile, radius::TILE, look::wash(accent, 0.94), 0.0, LINE);
    let day = today.day.to_string();
    let st = text(size::BODY, Weight::SemiBold, on);
    let w = look::tabular_width(c, &day, st);
    look::tabular_label(c, r, &day, tile.loc.x + (40 - w) / 2, tile.loc.y + 11, st);
    let y = y + geometry::TODAY_H;
    let header = format!("{} {}", calendar::MONTHS[month.1 as usize - 1], month.0);
    let hs = text(size::HEADING, Weight::SemiBold, TEXT);
    let w = c.text.measure(&header, hs).ceil() as i32;
    c.styled_label(r, &header, x + (320 - w) / 2, y + 20, w, hs);
    for (item, a) in &p.items {
        if !matches!(item, Item::Previous | Item::Next) { continue; }
        look::row_bg(c, *a, hover == Some(&item_action(*item)));
        let name = if *item == Item::Previous { "go-previous-symbolic" } else { "go-next-symbolic" };
        c.icon(r, icons, &[name], theme_name, rect(a.loc.x + 8, a.loc.y + 8, 16, 16));
    }
    // Monday first, to match `month_grid`.
    for (i, day) in ["M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
        let st = text(size::CAPTION, Weight::SemiBold, look::dim(TEXT, if i >= 5 { 0.38 } else { 0.55 }));
        let w = c.text.measure(day, st).ceil() as i32;
        c.styled_label(r, day, x + 36 + i as i32 * 40 - w / 2, y + 64, 24, st);
    }
    for (i, date) in calendar::month_grid(month.0, month.1).iter().enumerate() {
        let a = rect(x + 20 + (i % 7) as i32 * 40, y + 88 + (i / 7) as i32 * 34, 32, 30);
        let is_today = (date.year, date.month, date.day) == (today.year, today.month, today.day);
        let mut color = if is_today { on } else { TEXT };
        if date.month != month.1 && !is_today {
            color = look::dim(color, 0.3);
        }
        if is_today {
            c.rect(a, radius::ROW, look::wash(accent, 0.94), 0.0, LINE);
        }
        let st = text(size::LABEL, if is_today { Weight::SemiBold } else { Weight::Medium }, color);
        let value = date.day.to_string();
        let w = look::tabular_width(c, &value, st);
        look::tabular_label(c, r, &value, a.loc.x + (32 - w) / 2, a.loc.y + 6, st);
    }
}

impl Gavno {
    /// Re-evaluate shell visibility and share one minute source with the theme.
    pub fn refresh_shell(&mut self) {
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        self.background.retain_outputs(&outputs);
        self.refresh_theme_clock();
        let progress = self.shell_progress();
        self.topbar.shell_progress = progress;
        self.dock.shell_progress = progress;
        self.window_decor.classic = self.features.on(crate::module::Feature::Classic) && self.classic.settled == crate::classic::ShellMode::Classic;
        self.window_decor.pointer = self.pointer_location();
        self.guard(crate::module::Feature::Dock, (), |gv| {
            gv.dock.refresh(&gv.config.dock, &gv.workspaces, &gv.space, gv.start_time.elapsed());
            gv.dock.drop_stale_menu(&outputs);
        });
        if self.topbar.menu.as_ref().is_some_and(|o| !outputs.contains(o)) {
            self.topbar.menu = None;
        }
        self.guard(crate::module::Feature::Classic, (), |gv| {
            let pins = gv.config.classic.pinned.as_ref().unwrap_or(&gv.config.dock.pinned);
            if gv.classic.mode == crate::classic::ShellMode::Classic || gv.shell_progress() > 0.001 {
                gv.classic.taskbar.focused = gv.focused_window();
                gv.classic.taskbar.refresh(&mut gv.dock, pins, &outputs);
            } else {
                if let Some(t)=gv.classic.taskbar.hover_timer.take() { gv.loop_handle.remove(t); }
                gv.classic.taskbar.release(); gv.classic.start.release();
            }
        });
        self.refresh_bar_status(!self.power.is_off() && outputs.iter().any(|o| self.shell_visible(o)) && self.features.on(crate::module::Feature::Bar));
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
        self.topbar.music_panel = self.config.media.music_panel;
        self.topbar.night_light = self.config.display.night_light;
        self.topbar.icon_theme = self.config.dock.icon_theme.clone();
        self.sync_bar_hints();
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
        if changed && visible {
            self.queue_redraw_all();
        }
    }
    pub fn bar_action(&mut self, output: &Output, action: BarAction) {
        self.guard(crate::module::Feature::Bar, Default::default(), |gv| gv.bar_action_feature(output, action))
    }

    fn bar_action_feature(&mut self, output: &Output, action: BarAction) {
        match action {
            BarAction::Tiling => self.toggle_workspace_tiling(output),
            BarAction::CodeMode => self.set_code_mode(!self.code_mode),
            BarAction::Media(command) => self.media_command(command),
            BarAction::MusicPanel => self.toggle_music(output),
            BarAction::Workspace(i) => {
                self.switch_workspace(output, i);
            }
            BarAction::Layout => {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.with_xkb_state(self, |mut ctx| ctx.cycle_next_layout());
                }
            }
            // The status cluster and the Classic tray open quick settings.
            BarAction::PowerMenu => self.toggle_panel_controls(crate::panel_controls::Page::Main, output),
            BarAction::Calendar => {
                self.topbar.controls.close();
                self.guard(crate::module::Feature::Dock, (), |gv| gv.dock.close_menu());
                if self.topbar.menu.as_ref() == Some(output) {
                    self.topbar.close();
                } else {
                    self.topbar.menu = Some(output.clone());
                    self.topbar.popover_output = Some(output.clone());
                    self.guard(crate::module::Feature::Notifications, (), |gv| {
                        gv.notifications.borrow_mut().unread = false;
                        gv.refresh_notifications();
                    });
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
            for p in [PopoverLayout::calendar(1280), PopoverLayout::calendar_notifications(1280, 2)] {
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
                charging: true,
                minutes: None
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
                charging: false,
                minutes: None
            })
        );
        std::fs::write(b.join("present"), "0").unwrap();
        assert!(read_battery(&root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl Gavno {
    pub(crate) fn reset_bar(&mut self) {
        if let Some((_, _, token)) = self.media.repeat.take() { self.loop_handle.remove(token); }
        self.refresh_bar_status(false);
        if let Some(token) = self.topbar.controls.event_token.take() { self.loop_handle.remove(token); }
        self.topbar.controls.close();
        self.topbar = Default::default();
        self.media.panel_volume = None;
        self.topbar.notifications = self.notifications.clone();
        self.topbar.now_playing = self.now_playing.clone();
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

#[cfg(test)]
mod music_panel_tests {
    use super::*;
    #[test]
    fn installed_daemon_icon_and_media_title_open_panel_with_legacy_opt_out() {
        use smithay::backend::egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay};
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        let mut renderer = unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap();
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new("music-bar".into(), smithay::output::PhysicalProperties { size: (0,0).into(), subpixel: smithay::output::Subpixel::Unknown, make: "test".into(), model: "test".into() });
        output.change_current_state(Some(smithay::output::Mode { size: (1280,800).into(), refresh: 60000 }), None, None, None);
        h.gv.space.map_output(&output, (0,0));
        h.gv.now_playing.borrow_mut().music.bus_address = Some("unix:path=/nonexistent/music-test".into());
        h.gv.media_update(crate::media::Snapshot { music_available: true, ..Default::default() });
        let (layout, _) = media_layout(&h.gv.now_playing, 1280, 43, false, true, &h.gv.features, true, false);
        let area = layout.media.unwrap(); assert_eq!(area.size.w, 28);
        let _ = h.gv.topbar.elements(&mut renderer, &output, 1280, None, Duration::ZERO, true, &h.gv.features);
        let action = h.gv.topbar.action(&output, (area.loc.x as f64 + 6.0, 15.0).into()).unwrap();
        assert_eq!(action, BarAction::MusicPanel);
        h.gv.bar_action(&output, action.clone()); assert!(h.gv.music_open());
        h.gv.bar_action(&output, action); assert!(!h.gv.music_open());
        h.gv.media_update(crate::media::Snapshot { player: Some(crate::media::Player { metadata: crate::media::Metadata { title: "Song".into(), ..Default::default() }, ..Default::default() }), ..Default::default() });
        let _ = h.gv.topbar.elements(&mut renderer, &output, 1280, None, Duration::ZERO, true, &h.gv.features);
        assert_eq!(h.gv.topbar.action(&output, (area.loc.x as f64 + 40.0, 15.0).into()), Some(BarAction::MusicPanel));
        assert_eq!(h.gv.topbar.action(&output, (area.loc.x as f64 + 10.0, 15.0).into()), Some(BarAction::Media(crate::media::Command::PlayPause)));
        h.gv.config.media.music_panel = false; h.gv.refresh_shell();
        let _ = h.gv.topbar.elements(&mut renderer, &output, 1280, None, Duration::ZERO, true, &h.gv.features);
        assert_eq!(h.gv.topbar.action(&output, (area.loc.x as f64 + 40.0, 15.0).into()), Some(BarAction::Media(crate::media::Command::PlayPause)));
        h.gv.media_update(Default::default());
        assert!(media_layout(&h.gv.now_playing, 1280, 43, false, false, &h.gv.features, true, false).0.media.is_none());
    }
}

#[cfg(test)]
mod power_clock_tests {
    use super::*;
    #[test]
    fn panel_clock_stops_while_powered_off_and_restarts_on_wake() {
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new("test".into(), smithay::output::PhysicalProperties {
            size: (0, 0).into(), subpixel: smithay::output::Subpixel::Unknown, make: "test".into(), model: "test".into(),
        });
        output.change_current_state(Some(smithay::output::Mode { size: (1280, 800).into(), refresh: 60000 }), None, None, None);
        h.gv.space.map_output(&output, (0, 0));
        h.gv.refresh_shell();
        assert!(h.gv.minute_timer.is_some());
        h.gv.power_off_monitors("test");
        h.gv.refresh_shell();
        assert!(h.gv.minute_timer.is_none());
        h.gv.power_on_monitors("test");
        h.gv.refresh_shell();
        assert!(h.gv.minute_timer.is_some());
    }
}

#[cfg(test)]
mod music_capsule_artifacts {
    use super::*;
    use crate::media::ui::artifacts;
    #[test]
    fn bar_capsule_artifacts() {
        let mut renderer=artifacts::renderer();
        let path=artifacts::cover_path("bar-art");
        let mut bar=TopBar::default();
        bar.now_playing.borrow_mut().snapshot=crate::media::Snapshot {
            player:Some(artifacts::player()),music_player:Some(artifacts::player()),music_available:true,
            music_image:Some(artifacts::decoded_cover(&path)),..Default::default()
        };
        let output=Output::new("capsule-test".into(),smithay::output::PhysicalProperties {size:(0,0).into(),subpixel:smithay::output::Subpixel::Unknown,make:"test".into(),model:"test".into()});
        let features=crate::module::Features::new(&crate::config::Config::parse_checked(true,"",|_|None).unwrap());
        for scale in [1.0,1.35] {
            output.change_current_state(Some(smithay::output::Mode {size:((1280.0*scale) as i32,(800.0*scale) as i32).into(),refresh:60000}),None,Some(smithay::output::Scale::Fractional(scale)),None);
            for hover in [false,true] {
                if let Some(view)=bar.views.first_mut(){view.hover=hover.then_some(BarAction::Media(crate::media::Command::PlayPause));}
                let elements=bar.elements(&mut renderer,&output,1280,None,Duration::from_secs(100),true,&features);
                artifacts::capture(&mut renderer,&elements,(1280,40),scale,if hover{"bar-capsule-hover"}else{"bar-capsule"});
            }
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod capsule_render_tests {
    use super::*;
    use crate::media::{ui::artifacts,Command};
    use smithay::backend::renderer::element::Element;
    #[test]
    fn cover_toggle_scrim_and_static_capsule_commits() {
        let mut renderer=artifacts::renderer();
        let path=artifacts::cover_path("bar-check");
        let art=artifacts::decoded_cover(&path);
        let output=Output::new("bar-check".into(),smithay::output::PhysicalProperties {size:(0,0).into(),subpixel:smithay::output::Subpixel::Unknown,make:"test".into(),model:"test".into()});
        output.change_current_state(Some(smithay::output::Mode {size:(1280,800).into(),refresh:60000}),None,None,None);
        let features=crate::module::Features::new(&crate::config::Config::parse_checked(true,"",|_|None).unwrap());
        let mut bar=TopBar::default();
        let draw=|bar:&mut TopBar,renderer:&mut GlesRenderer,seconds|bar.elements(renderer,&output,1280,None,Duration::from_secs(seconds),true,&features);
        for (spotify,loaded) in [(true,false),(false,true),(true,true)] {
            let mut player=artifacts::player();if !spotify {player.name="org.mpris.MediaPlayer2.other".into();}
            bar.now_playing.borrow_mut().snapshot=crate::media::Snapshot {player:Some(player),music_image:loaded.then(||art.clone()),..Default::default()};
            let elements=draw(&mut bar,&mut renderer,100);
            let area=bar.views[0].layout.as_ref().unwrap().media.unwrap();
            let cover_rect=rect(area.loc.x+5,area.loc.y+3,18,18);
            let has_cover=elements.iter().any(|e| {let g=e.geometry(1.0.into());g.loc.x==cover_rect.loc.x && g.loc.y==cover_rect.loc.y && g.size.w==18 && g.size.h==18 && e.src().size.w==160.0});
            assert_eq!(has_cover,spotify && loaded);
            assert!(!bar.views[0].canvas.drawn.contains(&cover_rect));
            bar.views[0].hover=Some(BarAction::MusicPanel);draw(&mut bar,&mut renderer,101);
            assert!(!bar.views[0].canvas.drawn.contains(&cover_rect));
            bar.views[0].hover=Some(BarAction::Media(Command::PlayPause));draw(&mut bar,&mut renderer,102);
            assert_eq!(bar.views[0].canvas.drawn.contains(&cover_rect),spotify && loaded);
            bar.views[0].hover=None;
        }
        // Settle workspace springs. Position-only redraws retain every capsule
        // element's identity and commit; no clock or progress timer is added.
        draw(&mut bar,&mut renderer,200);draw(&mut bar,&mut renderer,201);
        let before=draw(&mut bar,&mut renderer,202);
        let after=draw(&mut bar,&mut renderer,203);
        let area=bar.views[0].layout.as_ref().unwrap().media.unwrap();
        let commits=|elements:Vec<GavnoRenderElement>|elements.into_iter().filter_map(|e| {let g=e.geometry(1.0.into());(g.loc.x>=area.loc.x && g.loc.x+g.size.w<=area.loc.x+area.size.w).then(||(e.id().clone(),e.current_commit()))}).collect::<Vec<_>>();
        assert_eq!(commits(before),commits(after));
        std::fs::remove_file(path).unwrap();
    }
}
