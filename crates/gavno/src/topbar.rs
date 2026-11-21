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
    collections::HashMap,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Sentinel for `Canvas::colored_icon`: draw the icon untinted.
pub(crate) const FULL_COLOR: [u8; 3] = [1, 2, 3];
pub const PANEL: i32 = 32;
/// The bar is a floating island inside its `PANEL` band.
const ISLAND_INSET_X: i32 = 4;
const ISLAND_INSET_Y: i32 = 2;
const ISLAND_RADIUS: f32 = 14.0;
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
use geometry::{rect, Item, Layout, PillSpec, PopoverLayout, Rect, StatusItem};
use crate::ui::style::{dim, focus_line, radius, size, text, wash, SURFACE_HOVER, SURFACE_RAISED, TEXT};
use smithay::utils::Point;

/// Hover time before a bar tooltip appears.
pub(crate) const TOOLTIP_DELAY: Duration = Duration::from_millis(450);

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
    /// Power flowing out of (or into, while charging) the battery, in
    /// milliwatts, when the firmware reports it.
    pub milliwatts: Option<u32>,
}
impl Battery {
    /// "4.6 W" for the quick settings footer.
    pub fn power(&self) -> Option<String> {
        self.milliwatts.filter(|mw| *mw > 0).map(|mw| format!("{:.1} W", mw as f64 / 1000.0))
    }
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
        // Some firmware (Qualcomm battmgr) reports power and current signed,
        // negative while discharging; only the magnitude matters here.
        let number = |key: &str| read(&path, key).and_then(|v| v.parse::<i64>().ok()).map(i64::unsigned_abs);
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
        // power_now is µW; otherwise current_now (µA) times voltage_now (µV).
        let milliwatts = match number("power_now") {
            Some(uw) => Some(uw / 1000),
            None => number("current_now").zip(number("voltage_now")).map(|(ua, uv)| (ua as u128 * uv as u128 / 1_000_000_000) as u64),
        }
        .filter(|_| status == "Charging" || status == "Discharging")
        .map(|mw| mw.min(u32::MAX as u64) as u32);
        Some(Battery {
            percent,
            charging: matches!(status.as_str(), "Charging" | "Full"),
            minutes,
            milliwatts,
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
    /// A liquid glass surface (see `ui::glass`) with an optional hairline;
    /// `fallback` fills it while the output has no glass yet.
    pub fn glass(
        &mut self,
        rect: Rectangle<i32, Logical>,
        radius: f32,
        tone: crate::ui::glass::Tone,
        fallback: [f32; 4],
        border: f32,
        line: [f32; 4],
    ) {
        let mapped = self.mapped(rect);
        match crate::ui::glass::element(mapped, radius * self.zoom as f32, tone, self.alpha, self.scale) {
            Some(element) => {
                self.elements.push(element);
                if border > 0.0 {
                    self.rect(rect, radius, [0.0; 4], border, line);
                }
            }
            None => self.rect(rect, radius, fallback, border, line),
        }
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
    /// Soft coloured halo around `rect`: a shadow without offset.
    pub fn glow(&mut self, rect: Rectangle<i32, Logical>, radius: f32, blur: i32, color: [f32; 4]) {
        let rect = self.mapped(rect);
        if self.used == self.boxes.len() {
            self.boxes.push(ShaderBox::default());
        }
        if let Some(shaders) = &self.shaders {
            self.elements.push(GavnoRenderElement::Shader(shader::shadow(
                &mut self.boxes[self.used],
                shaders,
                rect,
                radius * self.zoom as f32,
                blur,
                0,
                color,
                self.alpha,
                self.scale,
            )));
        }
        self.used += 1;
    }
    /// Logical width of `value` drawn by `tabular_label`.
    pub fn tabular_width(&mut self, value: &str, style: TextStyleSpec) -> i32 {
        self.text.tabular_cells(value, style).1.ceil() as i32
    }
    /// Draws digits in fixed-width cells; returns the logical width used.
    pub fn tabular_label(
        &mut self,
        renderer: &mut GlesRenderer,
        value: &str,
        x: i32,
        y: i32,
        style: TextStyleSpec,
    ) -> i32 {
        let (cells, width) = self.text.tabular_cells(value, style);
        for (ch, dx) in cells {
            if !ch.is_whitespace() {
                self.styled_label(renderer, ch.encode_utf8(&mut [0; 4]), x + dx.round() as i32, y, 512, style);
            }
        }
        width.ceil() as i32
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
/// One workspace as the bar shows it: its windows' icons, count and name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorkspaceInfo {
    /// Icon names of the front-most windows, at most three.
    icons: Vec<String>,
    windows: usize,
    name: Option<String>,
    active: bool,
}
struct BarView {
    output: Output,
    canvas: Canvas,
    /// What each workspace of this output holds, refreshed every frame.
    workspaces: Vec<WorkspaceInfo>,
    hits: Vec<(Rectangle<f64, Logical>, BarAction)>,
    visible: bool,
    hover: Option<BarAction>,
    layout: Option<Layout>,
    popover: Option<PopoverLayout>,
}
pub struct TopBar {
    pub controls: crate::panel_controls::State,
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
    /// TTY mode is on; shown by the quick settings chip.
    pub tty_mode: bool,
    pub hint_context: Option<crate::hints::HintContext>,
    /// The hints of `hint_context` whose binds are currently enabled.
    pub hint_items: Vec<&'static crate::hints::BindHint>,
    pub mod_label: &'static str,
    /// The focused window's title, without the workspace name.
    pub title: String,
    /// The active workspace's name, drawn dimmer before the title.
    pub workspace_name: Option<String>,
    pub layout: String,
    /// False until the first refresh, so startup does not announce a layout.
    layout_seen: bool,
    app_icon: String,
    /// App id to icon name, filled from the desktop entries on first use.
    app_icons: HashMap<String, String>,
    /// The tooltip being shown, and the one-shot timer that arms it.
    tooltip: Option<(Output, BarAction)>,
    pub(crate) tooltip_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    pub(crate) entries: crate::desktop_entry::DesktopEntries,
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
            tty_mode: false,
            hint_context: None,
            hint_items: Vec::new(),
            mod_label: "Super",
            title: String::new(),
            workspace_name: None,
            layout: "EN".into(),
            layout_seen: false,
            app_icon: String::new(),
            app_icons: HashMap::new(),
            tooltip: None,
            tooltip_timer: None,
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
            output: output.clone(), canvas: Canvas::default(), workspaces: vec![], hits: vec![], visible: true,
            hover: None, layout: Some(layout), popover: None,
        });
    }
    /// What the quick settings panel shows and toggles outside its own workers.
    pub(crate) fn quick_info(&self, classic: bool, media: bool) -> crate::panel_controls::Info {
        crate::panel_controls::Info {
            battery: self.status.battery.clone(),
            power_history: self.status.power_history.clone(),
            profile: self.status.profile.clone(),
            brightness: self.status.brightness.as_ref().map(|b| b.level()),
            dnd: self.notification_dnd,
            night_light: self.night_light,
            code_mode: self.code_mode,
            code_available: !classic,
            tty: self.tty_mode,
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
    /// Area of the bar popover open (or still closing) on `output`: quick
    /// settings, calendar or the music panel. Toasts keep clear of it.
    pub fn popover_rect(&self, output: &Output, size: smithay::utils::Size<i32, Logical>) -> Option<Rect> {
        if self.controls.open.as_ref() == Some(output) {
            if let Some(panel) = self.controls.panel_rect() {
                return Some(panel);
            }
        }
        if let Some(panel) = self
            .views
            .iter()
            .find(|v| &v.output == output)
            .and_then(|v| v.popover.as_ref())
            .map(|p| p.panel)
        {
            return Some(panel);
        }
        let media = self.now_playing.borrow();
        let session = media.music.session.as_ref().filter(|s| &s.output == output)?;
        Some(crate::media::music::panel_rect(size, session.anchor))
    }
    /// Whether `action` shows a tooltip after the hover delay.
    pub(crate) fn has_tooltip(action: &BarAction) -> bool {
        matches!(action, BarAction::Workspace(_) | BarAction::Tiling | BarAction::Layout | BarAction::CodeMode)
    }
    pub(crate) fn update_clock(&mut self) -> bool {
        let next = clock_label();
        let changed = self.clock != next;
        self.clock = next;
        changed
    }
    pub(crate) fn set_accent(&mut self, accent: [f32; 4]) { self.accent = accent; }
    pub fn active(&self) -> bool {
        self.opening.is_active() || self.controls.animating()
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
                workspaces: Vec::new(),
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
        let workspaces = workspace_infos(owner, &mut self.entries, &mut self.app_icons);
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
            return Vec::new();
        }
        let c = &mut v.canvas;
        c.begin(renderer, output.current_scale().fractional_scale());
        c.offset.y = -self.shell_progress * PANEL as f64;
        c.alpha = (1.0 - self.shell_progress) as f32;
        v.hits.clear();
        let specs: Vec<PillSpec> = workspaces.iter().map(|w| PillSpec { windows: w.windows, active: w.active }).collect();
        let battery = self.status.battery.is_some();
        // The strip fits into what the title and clock leave it.
        let cap = media_layout(&self.now_playing, width, i32::MAX / 4, self.multiple_layouts, self.music_panel, features, battery).0.workspaces.size.w;
        let strip = geometry::workspace_strip(8, cap, &specs);
        let (l, media) = media_layout(&self.now_playing, width, strip.rect.size.w, self.multiple_layouts, self.music_panel, features, battery);
        if self.shell_progress < 0.999 {
        // A floating liquid glass island, inset from the screen edges.
        c.glass(rect(ISLAND_INSET_X, ISLAND_INSET_Y, width - 2 * ISLAND_INSET_X, PANEL - 2 * ISLAND_INSET_Y), ISLAND_RADIUS, crate::ui::glass::tone::CLEAR, GLASS, 1.0, [1.0, 1.0, 1.0, 0.10]);
        capsule(c, l.workspaces, false);
        for pill in &strip.pills {
            let info = &workspaces[pill.index];
            let hover = v.hover == Some(BarAction::Workspace(pill.index));
            if pill.dot {
                if hover {
                    c.rect(pill.rect, radius::FIELD, SURFACE_HOVER, 0.0, LINE);
                }
                let (w, color) = if info.active {
                    (22, accent)
                } else if info.windows > 0 {
                    (8, dim(TEXT, 0.75))
                } else {
                    (8, dim(TEXT, 0.38))
                };
                c.rect(rect(pill.rect.loc.x + (pill.rect.size.w - w) / 2, 12, w, 8), 4.0, color, 0.0, LINE);
            } else {
                if info.active {
                    c.rect(pill.rect, radius::FIELD, wash(accent, 0.34), 1.0, focus_line(accent));
                } else {
                    c.rect(pill.rect, radius::FIELD, if hover { SURFACE_HOVER } else { SURFACE_RAISED }, 0.0, LINE);
                }
                let mut x = pill.rect.loc.x + geometry::PILL_PAD;
                let y = pill.rect.loc.y + (geometry::PILL_H - geometry::PILL_ICON) / 2;
                for name in info.icons.iter().take(pill.icons) {
                    let area = rect(x, y, geometry::PILL_ICON, geometry::PILL_ICON);
                    if !c.colored_icon(renderer, &mut self.icons, &[name, "application-x-executable"], &self.icon_theme, area, Some(FULL_COLOR)) {
                        c.rect(area, 4.0, dim(TEXT, 0.35), 0.0, LINE);
                    }
                    x += geometry::PILL_ICON + geometry::PILL_ICON_GAP;
                }
                if pill.extra > 0 {
                    let style = text(size::CAPTION, Weight::SemiBold, dim(TEXT, 0.85));
                    c.styled_label(renderer, &format!("+{}", pill.extra), x, pill.rect.loc.y + 3, geometry::extra_width(pill.extra), style);
                }
            }
            v.hits.push((pill.hit.to_f64(), BarAction::Workspace(pill.index)));
        }
        v.workspaces = workspaces;
        capsule(c, l.toggle, v.hover == Some(BarAction::Tiling));
        let tiling = owner.is_some_and(|o| o.list.items.get(o.list.active).is_some_and(|w| w.tiling));
        draw_tiling_glyph(c, l.toggle, tiling, accent);
        v.hits.push((l.toggle.to_f64(), BarAction::Tiling));
        // The hint strip lives in the title area and never reaches the clock,
        // the workspace dots or any other fixed capsule.
        let strip_w = draw_hints(
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
        // The focused app keeps its own colours. An icon the theme lacks gets
        // an initial on a tile instead of an empty gap.
        if !self.app_icon.is_empty()
            && !c.colored_icon(renderer, &mut self.icons, &[&self.app_icon], &self.icon_theme, l.app, Some(FULL_COLOR))
        {
            c.rect(l.app, 5.0, SURFACE_RAISED, 0.0, LINE);
            let initial = self.app_icon.chars().next().unwrap_or('?').to_uppercase().to_string();
            let st = text(size::CAPTION, Weight::SemiBold, TEXT);
            let w = c.text.measure(&initial, st).ceil() as i32;
            c.styled_label(renderer, &initial, l.app.loc.x + (l.app.size.w - w) / 2, l.app.loc.y + 1, w + 2, st);
        }
        draw_title(
            c,
            renderer,
            self.workspace_name.as_deref(),
            &self.title,
            l.title.loc.x,
            l.title.loc.y,
            (l.title.size.w - strip_w - if strip_w > 0 { 8 } else { 0 }).max(0),
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
        // A tooltip waits for the hover timer and gives way to every popover.
        if let Some((_, action)) = self.tooltip.as_ref().filter(|(o, a)| o == output && v.hover.as_ref() == Some(a)) {
            let popover = self.menu.is_some() || self.controls.open.is_some() || self.now_playing.borrow().music.session.is_some();
            let anchor = v.hits.iter().find(|(_, a)| a == action).map(|(r, _)| *r);
            if let (false, Some(anchor), Some(value)) = (popover, anchor, tooltip_text(action, &v.workspaces, tiling)) {
                draw_tooltip(c, renderer, anchor, &value, width);
            }
        }
        if self.shell_progress >= 0.999 { v.hits.clear(); }
        v.layout = Some(l);
        } else {
            v.layout=None;
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

/// The app id of a window, without cloning its title.
fn app_id_of(window: &smithay::desktop::Window) -> String {
    use smithay::wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData};
    let Some(toplevel) = window.toplevel() else { return String::new() };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().unwrap_or_else(|e| e.into_inner()).app_id.clone())
            .unwrap_or_default()
    })
}
/// Icon name of an app id: the desktop entry's icon, else the id itself.
fn icon_for(entries: &mut crate::desktop_entry::DesktopEntries, cache: &mut HashMap<String, String>, app: &str) -> String {
    if app.is_empty() {
        return String::new();
    }
    if let Some(icon) = cache.get(app) {
        return icon.clone();
    }
    let icon = entries.resolve(app).map(|e| e.icon).filter(|i| !i.is_empty()).unwrap_or_else(|| app.to_owned());
    cache.insert(app.to_owned(), icon.clone());
    icon
}
fn workspace_infos(
    owner: Option<&crate::workspace::OutputWorkspaces>,
    entries: &mut crate::desktop_entry::DesktopEntries,
    cache: &mut HashMap<String, String>,
) -> Vec<WorkspaceInfo> {
    let Some(owner) = owner else {
        return vec![WorkspaceInfo { active: true, ..Default::default() }];
    };
    owner
        .list
        .items
        .iter()
        .enumerate()
        .map(|(i, ws)| WorkspaceInfo {
            // Windows are stored back to front; the front ones lead.
            icons: ws.windows.iter().rev().take(3).map(|w| icon_for(entries, cache, &app_id_of(&w.window))).collect(),
            windows: ws.windows.len(),
            name: ws.name.clone(),
            active: i == owner.list.active,
        })
        .collect()
}
/// The tiling toggle: a tiled layout (one tall tile, two small) when tiling is
/// on, two overlapping windows when windows float.
fn draw_tiling_glyph(c: &mut Canvas, toggle: Rect, tiling: bool, accent: [f32; 4]) {
    let x = toggle.loc.x + (toggle.size.w - 14) / 2;
    let y = toggle.loc.y + (toggle.size.h - 12) / 2;
    if tiling {
        c.rect(rect(x, y, 6, 12), 2.0, accent, 0.0, LINE);
        c.rect(rect(x + 8, y, 6, 5), 2.0, accent, 0.0, LINE);
        c.rect(rect(x + 8, y + 7, 6, 5), 2.0, accent, 0.0, LINE);
    } else {
        let line = dim(TEXT, 0.75);
        c.rect(rect(x, y, 10, 8), 2.0, [0.0; 4], 1.0, line);
        c.rect(rect(x + 4, y + 4, 10, 8), 2.0, [0.06, 0.07, 0.10, 1.0], 1.0, line);
    }
}
/// Tooltip text of a bar target, if it has one.
fn tooltip_text(action: &BarAction, workspaces: &[WorkspaceInfo], tiling: bool) -> Option<String> {
    match action {
        BarAction::Workspace(i) => {
            let ws = workspaces.get(*i)?;
            let name = ws.name.clone().unwrap_or_else(|| format!("Workspace {}", i + 1));
            let count = match ws.windows {
                0 => "empty".to_owned(),
                1 => "1 window".to_owned(),
                n => format!("{n} windows"),
            };
            Some(format!("{name} · {count}"))
        }
        BarAction::Tiling => Some(if tiling {
            "Tiling is on · click to float windows".into()
        } else {
            "Windows float · click to tile them".into()
        }),
        BarAction::Layout => Some("Switch keyboard layout".into()),
        BarAction::CodeMode => Some("Code mode · click to leave".into()),
        _ => None,
    }
}
/// A small card under `anchor` (output-local, mapped), kept inside `width`.
fn draw_tooltip(c: &mut Canvas, renderer: &mut GlesRenderer, anchor: Rectangle<f64, Logical>, value: &str, width: i32) {
    let style = text(size::SMALL, Weight::Medium, TEXT);
    let _ = c.text.render(" ", style, c.scale);
    let w = c.text.measure(value, style).ceil() as i32;
    let card = (w + 2 * crate::ui::style::space::SM).min((width - 16).max(1));
    let center = (anchor.loc.x + anchor.size.w / 2.0).round() as i32;
    let x = (center - card / 2).clamp(8, (width - card - 8).max(8));
    let area = rect(x, PANEL + 4, card, 24);
    crate::ui::style::card(c, area, radius::FIELD);
    c.styled_label(renderer, value, area.loc.x + crate::ui::style::space::SM, area.loc.y + 4, card - 2 * crate::ui::style::space::SM, style);
}
/// The workspace name (dim), a separator and the window title, sharing `width`.
fn draw_title(c: &mut Canvas, renderer: &mut GlesRenderer, name: Option<&str>, title: &str, x: i32, y: i32, width: i32) {
    if width <= 0 {
        return;
    }
    let main = label_style(size::LABEL);
    let dimmed = text(size::LABEL, Weight::Medium, dim(TEXT, 0.5));
    let _ = c.text.render(" ", main, c.scale);
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        c.styled_label(renderer, title, x, y, width, main);
        return;
    };
    if title.is_empty() {
        c.styled_label(renderer, name, x, y, width, dimmed);
        return;
    }
    // The name never takes more than 40% of the room.
    let sep = " · ";
    let sep_w = c.text.measure(sep, dimmed).ceil() as i32;
    let allowed = (width * 2 / 5).max(0);
    let name = ellipsize(&mut c.text, name, dimmed, (allowed - sep_w).max(0));
    if name.is_empty() {
        c.styled_label(renderer, title, x, y, width, main);
        return;
    }
    let prefix = format!("{name}{sep}");
    let prefix_w = c.text.measure(&prefix, dimmed).ceil() as i32;
    c.styled_label(renderer, &prefix, x, y, prefix_w + 2, dimmed);
    c.styled_label(renderer, title, x + prefix_w + 2, y, (width - prefix_w - 2).max(0), main);
}
fn media_layout(
    media: &crate::media::Shared, width: i32, workspace_width: i32, layouts: bool, music_panel: bool,
    features: &crate::module::Features, battery: bool,
) -> (Layout, crate::media::Snapshot) {
    let snapshot = features.read(crate::module::Feature::Media, Default::default(), || media.borrow().snapshot.clone());
    let slot = if battery { geometry::BATTERY_SLOT } else { geometry::CLUSTER_ICON };
    let mut layout = Layout::with_battery(width, workspace_width, layouts, slot).with_media(snapshot.player.is_some());
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
    // The title keeps its minimum width; hints only use what is left.
    let budget = geometry::hint_budget(title.size.w, CHIP, CHIP_GAP);
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
            gv.window_decor.app_icons.clear();
            gv.window_decor.app_icons.extend(gv.dock.window_icons());
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
        // The status line of TTY mode reads the same data, only while it shows.
        let status_wanted = if self.tty.on {
            self.tty.status_wanted()
        } else {
            self.features.on(crate::module::Feature::Bar)
        };
        self.refresh_bar_status(!self.power.is_off() && outputs.iter().any(|o| self.shell_visible(o)) && status_wanted);
        self.guard(crate::module::Feature::Bar, (), |gv| gv.refresh_bar(&outputs));
    }
    fn refresh_bar(&mut self, outputs: &[Output]) {
        self.refresh_workspace_names();
        let (title, app) = self
            .focused_window()
            .map(|w| crate::overlay::window_labels(&w))
            .unwrap_or_default();
        // The active workspace's name is drawn dimmer before the title.
        let workspace_name = self.active_workspace_name();
        self.topbar.app_icon = icon_for(&mut self.topbar.entries, &mut self.topbar.app_icons, &app);
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
        let changed = self.topbar.title != title || self.topbar.layout != layout || self.topbar.workspace_name != workspace_name;
        let layout_switched = self.topbar.layout_seen && self.topbar.layout != layout;
        self.topbar.layout_seen = true;
        self.topbar.title = title;
        self.topbar.workspace_name = workspace_name;
        self.topbar.layout = layout.clone();
        if layout_switched && !self.protocols.locks.locked && self.greeter.is_none() {
            self.show_layout_osd(&layout);
        }
        self.topbar.enabled = self.config.bar.enabled;
        crate::topbar::status::VOLUME_SOUND.store(self.config.bar.volume_sound, std::sync::atomic::Ordering::Relaxed);
        self.topbar.music_panel = self.config.media.music_panel;
        self.topbar.night_light = self.config.display.night_light;
        if self.topbar.icon_theme != self.config.dock.icon_theme {
            self.topbar.app_icons.clear();
        }
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
        self.topbar.tooltip = None;
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
                minutes: None, milliwatts: None,
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
                minutes: None, milliwatts: None,
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
        if let Some(token) = self.topbar.tooltip_timer.take() { self.loop_handle.remove(token); }
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
        let (layout, _) = media_layout(&h.gv.now_playing, 1280, 43, false, true, &h.gv.features, true);
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
        assert!(media_layout(&h.gv.now_playing, 1280, 43, false, false, &h.gv.features, true).0.media.is_none());
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
            let has_cover=elements.iter().any(|e| {let g=e.geometry(1.0.into());g.loc.x==cover_rect.loc.x && g.loc.y==cover_rect.loc.y && g.size.w==18 && g.size.h==18 && e.src().size.w==f64::from(crate::media::music::worker::CoverKind::Hero.size())});
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

#[cfg(test)]
mod bar_redesign_tests {
    use super::*;
    use crate::media::ui::artifacts;
    use crate::workspace::Workspace;

    fn output(name: &str, logical: (i32, i32), scale: f64) -> Output {
        let output = Output::new(name.into(), smithay::output::PhysicalProperties { size: (0, 0).into(), subpixel: smithay::output::Subpixel::Unknown, make: "test".into(), model: "test".into() });
        output.change_current_state(
            Some(smithay::output::Mode { size: ((logical.0 as f64 * scale) as i32, (logical.1 as f64 * scale) as i32).into(), refresh: 60000 }),
            None,
            Some(smithay::output::Scale::Fractional(scale)),
            None,
        );
        output
    }
    fn workspaces(h: &mut crate::state::tests::Harness, output: &Output, count: usize, active: usize, named: bool) {
        let owner = h.gv.workspaces.ensure(output);
        while owner.list.items.len() < count {
            let id = owner.list.items.len() as u64 + 100;
            owner.list.items.push(Workspace { id, windows: Vec::new(), tiling: false, tree: Default::default(), name: None, user_named: false, name_sig: None });
        }
        if named {
            for (i, ws) in owner.list.items.iter_mut().enumerate() {
                ws.name = Some(format!("Space {i}"));
            }
        }
        owner.list.active = active;
    }

    #[test]
    fn workspace_pills_are_hit_testable_and_tooltips_use_the_names() {
        let mut renderer = artifacts::renderer();
        for (logical, scale, count) in [((1280, 800), 1.0, 6), ((2134, 1200), 1.35, 6), ((1280, 800), 1.0, 40), ((2134, 1200), 1.35, 40)] {
            let mut h = crate::state::tests::Harness::new();
            let out = output("bar-redesign", logical, scale);
            h.gv.space.map_output(&out, (0, 0));
            workspaces(&mut h, &out, count, count / 2, true);
            let owner = h.gv.workspaces.outputs.iter().find(|o| o.output == out);
            let _ = h.gv.topbar.elements(&mut renderer, &out, logical.0, owner, Duration::from_secs(1), true, &h.gv.features);
            let view = &h.gv.topbar.views[0];
            let l = view.layout.as_ref().unwrap();
            let pills: Vec<_> = view.hits.iter().filter_map(|(r, a)| matches!(a, BarAction::Workspace(_)).then_some((*r, a.clone()))).collect();
            assert!(!pills.is_empty());
            // The strip stays inside its slot and clear of the toggle.
            for (r, _) in &pills {
                assert!(l.workspaces.to_f64().contains_rect(*r) || r.loc.x >= l.workspaces.loc.x as f64);
                assert!(r.loc.x + r.size.w <= (l.toggle.loc.x) as f64, "{logical:?}/{count}");
            }
            // Every pill answers with its own index at its centre.
            for (r, a) in &pills {
                let centre = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0).into();
                assert_eq!(h.gv.topbar.action(&out, centre).as_ref(), Some(a));
            }
            assert!(pills.iter().any(|(_, a)| *a == BarAction::Workspace(count / 2)), "active workspace stays reachable");
            // Tooltip text is the workspace name plus its window count.
            let infos = h.gv.topbar.views[0].workspaces.clone();
            assert_eq!(tooltip_text(&BarAction::Workspace(1), &infos, false).as_deref(), Some("Space 1 · empty"));
            assert_eq!(tooltip_text(&BarAction::Workspace(count + 5), &infos, false), None);
            // Layout geometry never overlaps: workspaces, toggle, title, clock.
            assert!(l.workspaces.loc.x + l.workspaces.size.w <= l.toggle.loc.x);
            assert!(l.title.loc.x + l.title.size.w <= l.clock.loc.x);
        }
    }
    #[test]
    fn tooltip_texts_cover_the_documented_targets_only() {
        let infos = vec![WorkspaceInfo { windows: 3, ..Default::default() }, WorkspaceInfo { windows: 1, name: Some("Code".into()), ..Default::default() }];
        assert_eq!(tooltip_text(&BarAction::Workspace(0), &infos, false).as_deref(), Some("Workspace 1 · 3 windows"));
        assert_eq!(tooltip_text(&BarAction::Workspace(1), &infos, false).as_deref(), Some("Code · 1 window"));
        assert!(tooltip_text(&BarAction::Tiling, &infos, true).unwrap().starts_with("Tiling is on"));
        assert!(tooltip_text(&BarAction::Tiling, &infos, false).unwrap().starts_with("Windows float"));
        assert!(tooltip_text(&BarAction::Calendar, &infos, false).is_none());
        assert!(TopBar::has_tooltip(&BarAction::Tiling) && !TopBar::has_tooltip(&BarAction::PowerMenu));
    }
    #[test]
    fn code_mode_hints_never_reach_the_title_minimum_or_the_clock() {
        let renderer_and_text = &mut crate::ui::text::TextRenderer::new();
        let style = label_style(10.5);
        let _ = renderer_and_text.render(" ", style, 1.0);
        let all: Vec<&crate::hints::BindHint> = crate::hints::items(crate::hints::HintContext::Code).iter().collect();
        assert!(!all.is_empty());
        let sizes: Vec<(*const crate::hints::BindHint, i32)> = all
            .iter()
            .map(|h| (*h as *const _, renderer_and_text.measure(h.keys, style).ceil() as i32 + 10 + 6 + renderer_and_text.measure(h.label, style).ceil() as i32))
            .collect();
        let width = |h: &crate::hints::BindHint| sizes.iter().find(|(p, _)| std::ptr::eq(*p, h)).map_or(0, |(_, w)| *w);
        for window in [800, 1024, 1280, 1646, 2134] {
            for layouts in [false, true] {
                let l = Layout::with_battery(window, 120, layouts, geometry::BATTERY_SLOT);
                let budget = geometry::hint_budget(l.title.size.w, 44, 8);
                let kept = crate::hints::fit(&all, budget, 14, width);
                let used: i32 = kept.iter().map(|h| width(h)).sum::<i32>() + 14 * kept.len().saturating_sub(1) as i32;
                assert!(used <= budget, "{window}: hints {used} > budget {budget}");
                let total = 44 + if kept.is_empty() { 0 } else { 8 + used };
                // Title text keeps room whenever the area has any to spare.
                let title_left = l.title.size.w - total - 8;
                assert!(title_left >= geometry::MIN_TITLE_TEXT - 8 || kept.is_empty(), "{window}: title squeezed to {title_left}");
                assert!(l.title.loc.x + l.title.size.w <= l.clock.loc.x);
            }
        }
        // A narrow title area drops every hint and keeps only the chip.
        assert_eq!(geometry::hint_budget(150, 44, 8), 0);
        assert!(crate::hints::fit(&all, 0, 14, width).is_empty());
    }
    #[test]
    fn bar_draws_without_panicking_in_code_mode_at_both_scales() {
        let mut renderer = artifacts::renderer();
        for (logical, scale) in [((1280, 800), 1.0), ((2134, 1200), 1.35)] {
            let mut h = crate::state::tests::Harness::new();
            let out = output("bar-code", logical, scale);
            h.gv.space.map_output(&out, (0, 0));
            workspaces(&mut h, &out, 5, 1, true);
            h.gv.topbar.code_mode = true;
            h.gv.topbar.hint_context = Some(crate::hints::HintContext::Code);
            h.gv.topbar.hint_items = crate::hints::items(crate::hints::HintContext::Code).iter().collect();
            h.gv.topbar.title = "A very long focused window title that has to be shortened gracefully".into();
            h.gv.topbar.workspace_name = Some("Writing and research".into());
            h.gv.topbar.app_icon = "no-such-icon-anywhere".into();
            let owner = h.gv.workspaces.outputs.iter().find(|o| o.output == out);
            let elements = h.gv.topbar.elements(&mut renderer, &out, logical.0, owner, Duration::from_secs(1), true, &h.gv.features);
            assert!(!elements.is_empty());
            artifacts::capture(&mut renderer, &elements, (logical.0, 40), scale, &format!("bar2-code-{}", logical.0));
        }
    }
}
