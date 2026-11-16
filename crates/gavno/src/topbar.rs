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

pub const PANEL: i32 = 30;
pub const GLASS: [f32; 4] = [0.055, 0.065, 0.09, 0.90];
pub const LINE: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
/// The UI font has no power glyph, so the bar draws a themed icon instead.
const POWER_ICONS: [&str; 3] = [
    "system-shutdown-symbolic",
    "system-shutdown",
    "system-log-out",
];
const POWER_ICON: i32 = 16;

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
        }
    }
}
impl Canvas {
    pub fn begin(&mut self, renderer: &mut GlesRenderer, scale: f64) {
        self.scale = scale;
        self.used = 0;
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
        if self.used == self.boxes.len() {
            self.boxes.push(ShaderBox::default());
        }
        if let Some(shaders) = &self.shaders {
            self.elements.push(GavnoRenderElement::Shader(shader::rrect(
                &mut self.boxes[self.used],
                shaders,
                rect,
                radius * self.scale as f32,
                color,
                border * self.scale as f32,
                line,
                1.0,
                self.scale,
            )));
        }
        self.used += 1;
    }
    pub fn shadow(&mut self, rect: Rectangle<i32, Logical>, radius: f32) {
        if self.used == self.boxes.len() {
            self.boxes.push(ShaderBox::default());
        }
        if let Some(shaders) = &self.shaders {
            self.elements
                .push(GavnoRenderElement::Shader(shader::shadow(
                    &mut self.boxes[self.used],
                    shaders,
                    rect,
                    radius,
                    20,
                    12,
                    [0.0, 0.0, 0.0, 0.38],
                    1.0,
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
        let style = TextStyleSpec {
            size,
            weight: Weight::Medium,
            color: [0.96, 0.95, 0.98, 1.0],
            tracking: 0.0,
        };
        let mut value: String = value
            .chars()
            .filter(|c| !c.is_control())
            .take(160)
            .collect();
        if self.text.measure(&value, style) > width as f32 {
            while !value.is_empty() && self.text.measure(&format!("{value}…"), style) > width as f32
            {
                value.pop();
            }
            value.push('…');
            if self.text.measure(&value, style) > width as f32 {
                return;
            }
        }
        if let Some(img) = self.text.render(&value, style, self.scale) {
            if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                (
                    (x as f64 * self.scale).round(),
                    (y as f64 * self.scale).round(),
                ),
                &img.buffer,
                None,
                Some(Rectangle::from_size(
                    (img.width as f64, img.height as f64).into(),
                )),
                Some((img.logical_w, img.logical_h).into()),
                Kind::Unspecified,
            ) {
                self.elements.push(GavnoRenderElement::Memory(e));
            }
        }
    }
    pub fn finish(&mut self) -> Vec<GavnoRenderElement> {
        std::mem::take(&mut self.elements)
            .into_iter()
            .rev()
            .collect()
    }
}

#[derive(Clone, Copy)]
pub enum PowerAction {
    Lock,
    Logout,
    Suspend,
    Restart,
    Shutdown,
}
#[derive(Clone)]
pub enum BarAction {
    Tiling,
    Workspace(usize),
    Layout,
    PowerMenu,
    Power(PowerAction),
}
struct BarView {
    output: Output,
    canvas: Canvas,
    dots: Vec<Spring>,
    hits: Vec<(Rectangle<f64, Logical>, BarAction)>,
    visible: bool,
}

pub struct TopBar {
    views: Vec<BarView>,
    pub enabled: bool,
    pub title: String,
    pub layout: String,
    clock: String,
    battery: Option<Battery>,
    accent: [f32; 4],
    icons: Icons,
    icon_theme: String,
    pub menu: Option<Output>,
    clock_timer: Option<RegistrationToken>,
    battery_timer: Option<RegistrationToken>,
    pub scroll: f64,
}
impl Default for TopBar {
    fn default() -> Self {
        Self {
            views: Vec::new(),
            enabled: true,
            title: String::new(),
            layout: "US".into(),
            clock: clock_label(),
            battery: None,
            accent: [0.95, 0.64, 0.42, 1.0],
            icons: Default::default(),
            icon_theme: crate::config::DockConfig::default().icon_theme,
            menu: None,
            clock_timer: None,
            battery_timer: None,
            scroll: 0.0,
        }
    }
}
impl TopBar {
    pub fn accent(&self) -> [f32; 4] { self.accent }
    fn update_clock(&mut self) {
        self.clock = clock_label();
        let t = theme::LocalTime::now();
        let (lat, lon) = theme::location(t.utc_offset);
        let a = theme::palette(&t, lat, lon).accent;
        self.accent = [a[0], a[1], a[2], 1.0];
    }
    pub fn active(&self) -> bool {
        self.views
            .iter()
            .any(|v| v.visible && v.dots.iter().any(Spring::is_active))
    }
    pub fn action(
        &self,
        output: &Output,
        pos: smithay::utils::Point<f64, Logical>,
    ) -> Option<BarAction> {
        self.views
            .iter()
            .find(|v| &v.output == output && v.visible)?
            .hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains(pos))
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
            });
            self.views.len() - 1
        };
        let v = &mut self.views[index];
        v.visible = visible && self.enabled;
        if !v.visible {
            v.hits.clear();
            return Vec::new();
        }
        let c = &mut v.canvas;
        c.begin(renderer, output.current_scale().fractional_scale());
        v.hits.clear();
        c.rect(
            Rectangle::new((0, 0).into(), (width, PANEL).into()),
            0.0,
            GLASS,
            0.0,
            LINE,
        );
        c.rect(
            Rectangle::new((0, PANEL - 1).into(), (width, 1).into()),
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
        let mut x = 10;
        for (i, dot) in v.dots.iter_mut().enumerate() {
            dot.advance(now);
            dot.set(if i == active { 22.0 } else { 7.0 });
            let w = dot.value.round() as i32;
            c.rect(
                Rectangle::new((x, 11).into(), (w, 7).into()),
                3.5,
                if i == active {
                    self.accent
                } else {
                    [0.96, 0.95, 0.98, 0.55]
                },
                0.0,
                LINE,
            );
            v.hits.push((
                Rectangle::new(
                    (x as f64 - 2.0, 0.0).into(),
                    ((w + 5) as f64, PANEL as f64).into(),
                ),
                BarAction::Workspace(i),
            ));
            x += w + 5;
        }
        let tiling = owner.is_some_and(|o| o.list.items[active].tiling);
        c.rect(Rectangle::new((x + 5, 4).into(), (66, 22).into()), 6.0, [1.0, 1.0, 1.0, 0.07], 0.0, LINE);
        c.label(renderer, if tiling { "Tiling" } else { "Floating" }, x + 11, 7, 55, 11.5);
        v.hits.push((Rectangle::new(((x + 5) as f64, 0.0).into(), (66.0, PANEL as f64).into()), BarAction::Tiling));
        x += 76;
        c.label(
            renderer,
            &self.title,
            x + 10,
            7,
            (width / 2 - 105 - x - 10).max(0),
            12.5,
        );
        c.label(renderer, &self.clock, (width - 190) / 2, 7, 190, 12.5);
        c.label(renderer, &self.layout, width - 177, 7, 40, 12.5);
        let battery = self
            .battery
            .as_ref()
            .map(|b| format!("{}{}%", if b.charging { "+ " } else { "" }, b.percent))
            .unwrap_or_else(|| "AC".into());
        c.label(renderer, &battery, width - 120, 7, 75, 12.5);
        let px = (POWER_ICON as f64 * c.scale).ceil() as u32;
        let power = POWER_ICONS
            .iter()
            .find_map(|name| self.icons.get_tinted(name, px, &self.icon_theme));
        let x = width - 29;
        let y = (PANEL - POWER_ICON) / 2;
        match power {
            Some(buffer) => {
                if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    ((x as f64 * c.scale).round(), (y as f64 * c.scale).round()),
                    &buffer,
                    None,
                    // An explicit src is required: a size alone crops the buffer.
                    Some(Rectangle::from_size((px as f64, px as f64).into())),
                    Some((POWER_ICON, POWER_ICON).into()),
                    Kind::Unspecified,
                ) {
                    c.elements.push(GavnoRenderElement::Memory(e));
                }
            }
            None => c.label(renderer, "Power", width - 38, 9, 36, 12.5),
        }
        v.hits.push((
            Rectangle::new(
                ((width - 185) as f64, 0.0).into(),
                (55.0, PANEL as f64).into(),
            ),
            BarAction::Layout,
        ));
        v.hits.push((
            Rectangle::new(
                ((width - 38) as f64, 0.0).into(),
                (38.0, PANEL as f64).into(),
            ),
            BarAction::PowerMenu,
        ));
        if self.menu.as_ref() == Some(output) {
            let rect = Rectangle::new(((width - 190).max(0), PANEL + 6).into(), (184, 150).into());
            c.shadow(rect, 12.0);
            c.rect(rect, 12.0, [0.065, 0.075, 0.10, 0.98], 1.0, LINE);
            for (i, (label, action)) in [
                ("Lock", PowerAction::Lock),
                ("Log out", PowerAction::Logout),
                ("Suspend", PowerAction::Suspend),
                ("Restart", PowerAction::Restart),
                ("Shut down", PowerAction::Shutdown),
            ]
            .into_iter()
            .enumerate()
            {
                let y = rect.loc.y + 5 + i as i32 * 28;
                c.label(renderer, label, rect.loc.x + 13, y + 5, 160, 13.0);
                v.hits.push((
                    Rectangle::new((rect.loc.x as f64, y as f64).into(), (184.0, 28.0).into()),
                    BarAction::Power(action),
                ));
            }
        }
        c.finish()
    }
}

impl Gavno {
    /// Re-evaluate visibility after protocol/input events; no source wakes a hidden panel.
    pub fn refresh_shell(&mut self) {
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        self.guard(crate::module::Feature::Dock, (), |gv| {
            gv.dock.refresh(
                &gv.config.dock,
                &gv.workspaces,
                &gv.space,
                gv.start_time.elapsed(),
            );
        });
        self.refresh_bar(&outputs);
    }
    fn refresh_bar(&mut self, outputs: &[Output]) {
        self.guard(crate::module::Feature::Bar, (), |gv| {
            let title = gv
                .focused_window()
                .map(|w| {
                    let (title, app) = crate::overlay::window_labels(&w);
                    format!("{app}  {title}")
                })
                .unwrap_or_default();
            let layouts = gv.config.xkb_layout.clone();
            let layout = gv
                .seat
                .get_keyboard()
                .map(|k| {
                    k.with_xkb_state(gv, |ctx| {
                        let xkb = ctx.xkb().lock().unwrap_or_else(|e| e.into_inner());
                        let index = xkb.active_layout().0 as usize;
                        layouts.split(',').nth(index).unwrap_or("us").to_uppercase()
                    })
                })
                .unwrap_or_else(|| "US".into());
            let changed = gv.topbar.title != title || gv.topbar.layout != layout;
            gv.topbar.title = title;
            gv.topbar.layout = layout;
            gv.topbar.enabled = gv.features.on(crate::module::Feature::Bar) && gv.config.bar.enabled;
            gv.topbar.icon_theme = gv.config.dock.icon_theme.clone();
            gv.topbar.views.retain(|v| outputs.contains(&v.output));
            let visible = outputs.iter().any(|o| gv.shell_visible(o));
            if !visible {
                gv.topbar.menu = None;
                gv.guard(crate::module::Feature::Dock, (), |gv| gv.dock.close_menu());
            }
            if visible && (gv.features.on(crate::module::Feature::Bar) && gv.config.bar.enabled) {
                if gv.topbar.clock_timer.is_none() {
                    gv.topbar.update_clock();
                    gv.schedule_bar_clock();
                }
                if gv.topbar.battery_timer.is_none() {
                    gv.topbar.battery = read_battery(Path::new("/sys/class/power_supply"));
                    gv.schedule_bar_battery();
                }
            } else {
                if let Some(token) = gv.topbar.clock_timer.take() {
                    gv.loop_handle.remove(token);
                }
                if let Some(token) = gv.topbar.battery_timer.take() {
                    gv.loop_handle.remove(token);
                }
            }
            if changed && visible {
                gv.queue_redraw_all();
            }
        });
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
    fn schedule_bar_battery(&mut self) {
        self.guard(crate::module::Feature::Bar, Default::default(), |gv| gv.schedule_bar_battery_feature())
    }

    fn schedule_bar_battery_feature(&mut self) {
        match self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_secs(30)),
            |_, _, gv| {
                gv.guard(crate::module::Feature::Bar, (), |gv| {
                    gv.topbar.battery_timer = None;
                    let battery = read_battery(Path::new("/sys/class/power_supply"));
                    if battery != gv.topbar.battery {
                        gv.topbar.battery = battery;
                        gv.queue_redraw_all();
                    }
                    if gv.space.outputs().any(|o| gv.shell_visible(o)) {
                        gv.schedule_bar_battery();
                    }

                });
                TimeoutAction::Drop
            },
        ) {
            Ok(t) => self.topbar.battery_timer = Some(t),
            Err(error) => tracing::warn!(%error,"cannot schedule battery refresh"),
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
            BarAction::PowerMenu => {
                self.guard(crate::module::Feature::Dock, (), |gv| gv.dock.close_menu());
                self.topbar.menu = if self.topbar.menu.as_ref() == Some(output) {
                    None
                } else {
                    Some(output.clone())
                };
            }
            BarAction::Power(action) => {
                self.topbar.menu = None;
                match action {
                    PowerAction::Logout => self.quit(),
                    PowerAction::Lock => { self.lock_session(); },
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
        for token in [self.topbar.clock_timer.take(), self.topbar.battery_timer.take()].into_iter().flatten() {
            self.loop_handle.remove(token);
        }
        self.topbar = Default::default();
    }
}
