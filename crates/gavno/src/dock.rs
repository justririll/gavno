//! Built-in dock, compositor chrome input routing, and reversible minimize motion.
use crate::{
    config::DockConfig,
    desktop_entry::{DesktopEntries, DesktopEntry},
    icons::Icons,
    overview::SceneWindow,
    render::GavnoRenderElement,
    topbar::{Canvas, GLASS, LINE, PANEL},
    ui::spring::{Spring, SpringParams},
    window_mode::{mode_state, with_mode_state, WindowMode},
    Gavno,
};
use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBufferRenderElement, Kind},
        gles::GlesRenderer,
    },
    desktop::{Space, Window},
    output::Output,
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{IsAlive, Logical, Point, Rectangle},
};
use std::time::Duration;

pub const DOCK_BASE: f64 = 46.0;
pub const DOCK_MAG: f64 = 1.72;
pub const DOCK_RANGE: f64 = 150.0;
pub const DOCK_H: i32 = 64;
pub const DOCK_M: i32 = 8;

pub fn magnification(distance: f64) -> f64 {
    if distance.abs() < DOCK_RANGE {
        1.0 + (DOCK_MAG - 1.0) * (distance.abs() / DOCK_RANGE * std::f64::consts::FRAC_PI_2).cos()
    } else {
        1.0
    }
}
/// Apply built-in reservations to an already layer-shell-adjusted rectangle.
pub fn reserve(
    mut area: Rectangle<i32, Logical>,
    bar: bool,
    dock: bool,
    autohide: bool,
) -> Rectangle<i32, Logical> {
    let top = if bar {
        PANEL.min(area.size.h.max(0))
    } else {
        0
    };
    let bottom = if dock && !autohide {
        DOCK_H + DOCK_M
    } else {
        0
    };
    area.loc.y += top;
    area.size.h = (area.size.h - top - bottom).max(0);
    area
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKey {
    App(String),
    Separator,
}
pub fn item_order(pinned: &[String], running: &[String]) -> Vec<ItemKey> {
    let mut apps = Vec::new();
    for id in pinned {
        if !apps.contains(id) {
            apps.push(id.clone());
        }
    }
    let mut out: Vec<_> = apps.iter().cloned().map(ItemKey::App).collect();
    let mut extras = Vec::new();
    for id in running {
        if !apps.contains(id) && !extras.contains(id) {
            extras.push(id.clone());
        }
    }
    if !extras.is_empty() && !out.is_empty() {
        out.push(ItemKey::Separator);
    }
    out.extend(extras.into_iter().map(ItemKey::App));
    out
}
#[derive(Clone)]
struct App {
    id: String,
    name: String,
    icon: String,
    entry: Option<DesktopEntry>,
    windows: Vec<Window>,
}
struct IconMotion {
    id: String,
    scale: Spring,
}
struct DockView {
    output: Output,
    canvas: Canvas,
    icons: Vec<IconMotion>,
    slide: Spring,
    hits: Vec<(String, Rectangle<f64, Logical>)>,
    panel: Rectangle<f64, Logical>,
    visible: bool,
    menu_rect: Option<Rectangle<f64, Logical>>,
}
struct Launch {
    id: String,
    start: Duration,
    baseline: usize,
    timer: Option<RegistrationToken>,
}
struct MinimizeMotion {
    window: Window,
    output: Output,
    location: Point<i32, Logical>,
    from: crate::anim::Rect,
    to: crate::anim::Rect,
    progress: Spring,
}
#[derive(Clone)]
struct DockMenu {
    id: String,
    output: Output,
}

pub struct Dock {
    initialized: bool,
    config: DockConfig,
    pinned: Vec<String>,
    entries: DesktopEntries,
    icons: Icons,
    apps: Vec<App>,
    order: Vec<ItemKey>,
    views: Vec<DockView>,
    menu: Option<DockMenu>,
    launches: Vec<Launch>,
    minimized: Vec<MinimizeMotion>,
    now: Duration,
    buttons: Vec<u32>,
    pub changed: bool,
}
impl Default for Dock {
    fn default() -> Self {
        Self {
            initialized: false,
            config: Default::default(),
            pinned: Vec::new(),
            entries: Default::default(),
            icons: Default::default(),
            apps: Vec::new(),
            order: Vec::new(),
            views: Vec::new(),
            menu: None,
            launches: Vec::new(),
            minimized: Vec::new(),
            now: Duration::ZERO,
            buttons: Vec::new(),
            changed: false,
        }
    }
}
impl Dock {
    pub fn close_menu(&mut self) {
        self.menu = None;
    }
    pub fn refresh(
        &mut self,
        config: &DockConfig,
        workspaces: &crate::workspace::Workspaces,
        space: &Space<Window>,
        now: Duration,
    ) {
        self.now = now;
        self.config = config.clone();
        if !self.initialized {
            self.pinned = config
                .pinned
                .iter()
                .take(128)
                .map(|id| self.canonical(id))
                .collect();
            self.initialized = true;
        }
        let mut running = Vec::new();
        let mut windows = Vec::new();
        for w in workspaces.all().map(|v| &v.window).chain(space.elements()) {
            if w.alive() && !windows.contains(w) {
                windows.push(w.clone());
            }
        }
        let mut grouped: Vec<(String, Vec<Window>)> = Vec::new();
        for window in windows {
            let id = self.canonical(&crate::overlay::window_labels(&window).1);
            if let Some((_, all)) = grouped.iter_mut().find(|(app, _)| app == &id) {
                all.push(window);
            } else {
                running.push(id.clone());
                grouped.push((id, vec![window]));
            }
        }
        // Keep existing running positions stable as workspace MRU changes.
        running.sort_by_key(|id| {
            self.order
                .iter()
                .position(|k| k == &ItemKey::App(id.clone()))
                .unwrap_or(usize::MAX)
        });
        let order = item_order(&self.pinned, &running);
        let apps = order
            .iter()
            .filter_map(|key| {
                let ItemKey::App(id) = key else {
                    return None;
                };
                let entry = self.entries.resolve(id);
                let windows = grouped
                    .iter()
                    .find(|(key, _)| key == id)
                    .map(|(_, w)| w.clone())
                    .unwrap_or_default();
                Some(App {
                    id: id.clone(),
                    name: entry
                        .as_ref()
                        .map(|e| e.name.clone())
                        .unwrap_or_else(|| id.clone()),
                    icon: entry.as_ref().map(|e| e.icon.clone()).unwrap_or_default(),
                    entry,
                    windows,
                })
            })
            .collect::<Vec<_>>();
        if self.order != order
            || self
                .apps
                .iter()
                .map(|a| (&a.id, &a.windows))
                .ne(apps.iter().map(|a| (&a.id, &a.windows)))
        {
            self.changed = true;
        }
        self.order = order;
        self.apps = apps;
        self.views
            .retain(|v| space.outputs().any(|o| o == &v.output));
        self.minimized
            .retain(|m| m.window.alive() && space.outputs().any(|o| o == &m.output));
        // Remove sources as soon as a launched instance maps its first window.
        // Tokens are handed back to Gavno below, since Dock does not own the loop.
    }
    fn canonical(&mut self, id: &str) -> String {
        self.entries
            .resolve(id)
            .map(|e| e.id)
            .unwrap_or_else(|| id.trim_end_matches(".desktop").to_owned())
    }
    pub fn active(&self) -> bool {
        self.views.iter().any(|v| {
            v.visible
                && (v.slide.is_active()
                    || v.icons.iter().any(|i| i.scale.is_active())
                    || self
                        .launches
                        .iter()
                        .any(|l| self.now.saturating_sub(l.start) < Duration::from_millis(950)))
        }) || self.minimized.iter().any(|m| m.progress.is_active())
    }
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: smithay::utils::Size<i32, Logical>,
        pointer: Point<f64, Logical>,
        now: Duration,
        visible: bool,
    ) -> Vec<GavnoRenderElement> {
        self.now = now;
        let index = if let Some(i) = self.views.iter().position(|v| &v.output == output) {
            i
        } else {
            self.views.push(DockView {
                output: output.clone(),
                canvas: Default::default(),
                icons: Vec::new(),
                slide: Spring::new(
                    if self.config.autohide { 1.0 } else { 0.0 },
                    SpringParams::SMOOTH,
                ),
                hits: Vec::new(),
                panel: Rectangle::default(),
                visible,
                menu_rect: None,
            });
            self.views.len() - 1
        };
        let v = &mut self.views[index];
        v.visible = visible && self.config.enabled;
        if !v.visible {
            v.hits.clear();
            v.menu_rect = None;
            return Vec::new();
        }
        let count = self.apps.len();
        if count == 0 {
            v.hits.clear();
            return Vec::new();
        }
        let has_separator = self.order.contains(&ItemKey::Separator);
        let base_width = count as f64 * DOCK_BASE
            + (count.saturating_sub(1)) as f64 * 6.0
            + 18.0
            + if has_separator { 13.0 } else { 0.0 };
        // Scale the entire dock to fit small outputs instead of making items unreachable.
        let fit = ((size.w as f64 - 16.0) / base_width.max(1.0)).clamp(0.05, 1.0);
        let base_left = (size.w as f64 - base_width * fit) / 2.0;
        let over = v.panel.contains(pointer) || v.hits.iter().any(|(_, r)| r.contains(pointer));
        let menu_open = self.menu.as_ref().is_some_and(|m| &m.output == output);
        let reveal = !self.config.autohide || pointer.y >= size.h as f64 - 1.5 || over || menu_open;
        v.slide.advance(now);
        v.slide.set(if reveal { 0.0 } else { 1.0 });
        let bottom = size.h as f64 - DOCK_M as f64 + (DOCK_H + 16) as f64 * v.slide.value;
        let mut base_x = base_left + 9.0 * fit;
        let mut scales = Vec::new();
        for key in &self.order {
            match key {
                ItemKey::Separator => {
                    base_x += 13.0 * fit;
                }
                ItemKey::App(id) => {
                    let i = if let Some(i) = v.icons.iter().position(|m| &m.id == id) {
                        i
                    } else {
                        v.icons.push(IconMotion {
                            id: id.clone(),
                            scale: Spring::new(1.0, SpringParams::SNAPPY),
                        });
                        v.icons.len() - 1
                    };
                    let s = &mut v.icons[i].scale;
                    s.advance(now);
                    let near = pointer.y > bottom - DOCK_BASE * DOCK_MAG - 20.0
                        && pointer.y < bottom + 10.0;
                    s.set(if self.config.magnify && near && reveal {
                        magnification((pointer.x - (base_x + DOCK_BASE * fit / 2.0)) / fit)
                    } else {
                        1.0
                    });
                    scales.push((id.clone(), s.value));
                    base_x += (DOCK_BASE + 6.0) * fit;
                }
            }
        }
        v.icons.retain(|m| self.apps.iter().any(|a| a.id == m.id));
        let magnified_width = scales.iter().map(|(_, s)| DOCK_BASE * s * fit).sum::<f64>()
            + count.saturating_sub(1) as f64 * 6.0 * fit
            + 18.0 * fit
            + if has_separator { 13.0 * fit } else { 0.0 };
        let left = (size.w as f64 - magnified_width) / 2.0;
        let panel = Rectangle::new(
            (left, bottom - DOCK_H as f64).into(),
            (magnified_width, DOCK_H as f64).into(),
        );
        v.panel = panel;
        let c = &mut v.canvas;
        c.begin(renderer, output.current_scale().fractional_scale());
        c.shadow(panel.to_i32_round(), 20.0);
        c.rect(panel.to_i32_round(), 20.0, GLASS, 1.0, LINE);
        v.hits.clear();
        v.menu_rect = None;
        let mut x = left + 9.0 * fit;
        for key in &self.order {
            let ItemKey::App(id) = key else {
                c.rect(
                    Rectangle::new(
                        (
                            (x + 3.0 * fit).round() as i32,
                            (bottom - DOCK_H as f64 + 12.0).round() as i32,
                        )
                            .into(),
                        (1, DOCK_H - 21).into(),
                    ),
                    0.0,
                    LINE,
                    0.0,
                    LINE,
                );
                x += 13.0 * fit;
                continue;
            };
            let Some(app) = self.apps.iter().find(|a| &a.id == id) else {
                continue;
            };
            let factor = scales
                .iter()
                .find(|(key, _)| key == id)
                .map(|(_, s)| *s)
                .unwrap_or(1.0);
            let sz = DOCK_BASE * factor * fit;
            let bounce = self
                .launches
                .iter()
                .find(|l| &l.id == id)
                .map(|l| bounce_offset(now.saturating_sub(l.start)))
                .unwrap_or(0.0);
            let rect = Rectangle::new((x, bottom - 9.0 - sz - bounce).into(), (sz, sz).into());
            v.hits.push((id.clone(), rect));
            let px = (DOCK_BASE * DOCK_MAG * fit * c.scale).ceil() as u32;
            if let Some(buffer) = self.icons.get(&app.icon, px, &self.config.icon_theme) {
                if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    ((x * c.scale).round(), (rect.loc.y * c.scale).round()),
                    &buffer,
                    None,
                    None,
                    Some((sz, sz).into()),
                    Kind::Unspecified,
                ) {
                    c.elements.push(GavnoRenderElement::Memory(e));
                }
            } else {
                c.rect(
                    rect.to_i32_round(),
                    (sz * 0.26) as f32,
                    [0.22, 0.30, 0.43, 1.0],
                    0.0,
                    LINE,
                );
                let letter = app
                    .name
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string();
                c.label(
                    renderer,
                    &letter,
                    (x + sz * 0.32) as i32,
                    (rect.loc.y + sz * 0.21) as i32,
                    sz as i32,
                    (sz * 0.46) as f32,
                );
            }
            if !app.windows.is_empty() {
                c.rect(
                    Rectangle::new(
                        (
                            (x + sz / 2.0 - 2.0).round() as i32,
                            (bottom - 6.0).round() as i32,
                        )
                            .into(),
                        (4, 4).into(),
                    ),
                    2.0,
                    [0.96, 0.95, 0.98, 0.85],
                    0.0,
                    LINE,
                );
            }
            if rect.contains(pointer) && !menu_open {
                let w = ((app.name.chars().count().min(30) * 7 + 20) as i32).min(size.w - 8);
                let tx = ((x + sz / 2.0).round() as i32 - w / 2).clamp(4, (size.w - w - 4).max(4));
                let ty = rect.loc.y.round() as i32 - 36;
                c.rect(
                    Rectangle::new((tx, ty).into(), (w, 26).into()),
                    8.0,
                    [0.06, 0.07, 0.10, 0.98],
                    1.0,
                    LINE,
                );
                c.label(renderer, &app.name, tx + 10, ty + 5, w - 20, 12.5);
            }
            x += sz + 6.0 * fit;
        }
        if let Some(menu) = &self.menu {
            if &menu.output == output {
                if let Some((_, icon)) = v.hits.iter().find(|(id, _)| id == &menu.id) {
                    let mx = (icon.loc.x + icon.size.w / 2.0 - 90.0)
                        .clamp(4.0, (size.w - 184).max(4) as f64);
                    let my = (bottom - DOCK_H as f64 - 104.0).max(PANEL as f64 + 4.0);
                    let rect = Rectangle::new((mx, my).into(), (180.0, 96.0).into());
                    v.menu_rect = Some(rect);
                    c.shadow(rect.to_i32_round(), 12.0);
                    c.rect(
                        rect.to_i32_round(),
                        12.0,
                        [0.065, 0.075, 0.10, 0.98],
                        1.0,
                        LINE,
                    );
                    for (i, label) in [
                        "New window",
                        "Quit",
                        if self.pinned.contains(&menu.id) {
                            "Unpin"
                        } else {
                            "Pin"
                        },
                    ]
                    .iter()
                    .enumerate()
                    {
                        c.label(
                            renderer,
                            label,
                            mx as i32 + 13,
                            my as i32 + 10 + i as i32 * 28,
                            160,
                            13.0,
                        );
                    }
                }
            }
        }
        c.finish()
    }
    pub fn minimize_scene(
        &mut self,
        space: &Space<Window>,
        output: &Output,
        now: Duration,
        scene: &mut Option<Vec<SceneWindow>>,
    ) {
        self.now = now;
        for m in &mut self.minimized {
            m.progress.advance(now);
        }
        self.minimized
            .retain(|m| m.progress.is_active() && m.window.alive());
        if !self.minimized.iter().any(|m| &m.output == output) {
            return;
        }
        let Some(geo) = space.output_geometry(output) else {
            return;
        };
        let scene = scene.get_or_insert_with(|| {
            space
                .elements()
                .filter(|w| !mode_state(w).minimized)
                .filter_map(|w| {
                    let loc = space.element_location(w)?;
                    Some(SceneWindow {
                        window: w.clone(),
                        location: loc,
                        rect: Rectangle::new(
                            loc.to_f64() - geo.loc.to_f64(),
                            w.geometry().size.to_f64(),
                        ),
                    })
                })
                .collect()
        });
        for m in self.minimized.iter().filter(|m| &m.output == output) {
            let mut rect =
                crate::anim::interpolate_rect(m.from, m.to, m.progress.value.clamp(0.0, 1.0));
            rect.loc -= geo.loc.to_f64();
            if let Some(v) = scene.iter_mut().find(|v| v.window == m.window) {
                v.rect = rect;
            } else {
                scene.push(SceneWindow {
                    window: m.window.clone(),
                    location: m.location,
                    rect,
                });
            }
        }
    }
    fn target(&self, output: &Output, id: &str, geo: Rectangle<i32, Logical>) -> crate::anim::Rect {
        let rect = self
            .views
            .iter()
            .find(|v| &v.output == output)
            .and_then(|v| v.hits.iter().find(|(key, _)| key == id).map(|(_, r)| *r));
        let centre = rect
            .map(|r| r.loc + Point::from((r.size.w / 2.0, r.size.h / 2.0)))
            .unwrap_or_else(|| {
                Point::from((
                    geo.size.w as f64 / 2.0,
                    geo.size.h as f64 - DOCK_M as f64 - DOCK_BASE / 2.0,
                ))
            });
        // Keep a hidden auto-hide target at the visible bottom edge.
        let y = centre
            .y
            .min(geo.size.h as f64 - DOCK_M as f64 - DOCK_BASE / 2.0);
        Rectangle::new(
            geo.loc.to_f64() + Point::from((centre.x - DOCK_BASE / 2.0, y - DOCK_BASE / 2.0)),
            (DOCK_BASE, DOCK_BASE).into(),
        )
    }
}
fn bounce_offset(elapsed: Duration) -> f64 {
    let t = elapsed.as_secs_f64() / 0.95;
    if t < 0.36 {
        22.0 * (std::f64::consts::PI * t / 0.36).sin().max(0.0)
    } else if t < 0.72 {
        10.0 * (std::f64::consts::PI * (t - 0.36) / 0.36).sin().max(0.0)
    } else {
        0.0
    }
}

impl Gavno {
    pub fn shell_visible(&self, output: &Output) -> bool {
        self.greeter.is_none()
            && !self.protocols.locks.locked
            && !self.space.elements().any(|w| {
                mode_state(w).mode == WindowMode::Fullscreen
                    && self.output_for_window(w).as_ref() == Some(output)
            })
    }
    pub fn shell_contains(&self, pos: Point<f64, Logical>) -> bool {
        self.space.outputs().any(|o| {
            if !self.shell_visible(o) {
                return false;
            }
            let Some(geo) = self.space.output_geometry(o) else {
                return false;
            };
            if !geo.to_f64().contains(pos) {
                return false;
            }
            let p = pos - geo.loc.to_f64();
            (self.config.bar.enabled && (p.y < PANEL as f64 || self.topbar.action(o, p).is_some()))
                || (self.config.dock.enabled
                    && self
                        .dock
                        .views
                        .iter()
                        .find(|v| &v.output == o)
                        .is_some_and(|v| {
                            v.panel.contains(p)
                                || v.hits.iter().any(|(_, r)| r.contains(p))
                                || v.menu_rect.is_some_and(|r| r.contains(p))
                        }))
                || self.topbar.menu.is_some()
                || self.dock.menu.is_some()
        })
    }
    pub fn shell_button(&mut self, button: u32, pressed: bool) -> bool {
        if !pressed {
            if let Some(i) = self.dock.buttons.iter().position(|b| *b == button) {
                self.dock.buttons.remove(i);
                return true;
            }
            return false;
        }
        let pointer = self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() || !self.shell_contains(pointer.current_location()) {
            return false;
        }
        self.dock.buttons.push(button);
        let Some(output) = self.active_output() else {
            return true;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return true;
        };
        let p = pointer.current_location() - geo.loc.to_f64();
        if button == 0x110 {
            if let Some(action) = self.topbar.action(&output, p) {
                self.bar_action(&output, action);
                return true;
            }
        }
        let menu_hit = self
            .dock
            .views
            .iter()
            .find(|v| v.output == output)
            .and_then(|v| v.menu_rect)
            .filter(|r| r.contains(p));
        if let (Some(rect), Some(menu)) = (menu_hit, self.dock.menu.clone()) {
            if button == 0x110 {
                match ((p.y - rect.loc.y - 5.0) / 28.0).floor() as i32 {
                    0 => self.launch_dock_app(&menu.id),
                    1 => {
                        if let Some(app) = self.dock.apps.iter().find(|a| a.id == menu.id) {
                            for w in &app.windows {
                                if let Some(t) = w.toplevel() {
                                    t.send_close();
                                }
                            }
                        }
                    }
                    2 => {
                        if let Some(i) = self.dock.pinned.iter().position(|id| id == &menu.id) {
                            self.dock.pinned.remove(i);
                        } else {
                            self.dock.pinned.push(menu.id.clone());
                        }
                        tracing::info!(app=%menu.id,"dock pin changed in memory; config writing is not implemented");
                    }
                    _ => {}
                }
            }
            self.dock.close_menu();
            self.queue_redraw_all();
            return true;
        }
        let hit = self
            .dock
            .views
            .iter()
            .find(|v| v.output == output)
            .and_then(|v| {
                v.hits
                    .iter()
                    .find(|(_, r)| r.contains(p))
                    .map(|(id, _)| id.clone())
            });
        let had_menu = self.dock.menu.is_some() || self.topbar.menu.is_some();
        self.dock.close_menu();
        self.topbar.menu = None;
        if !had_menu {
            if let Some(id) = hit {
                match button {
                    0x111 => self.dock.menu = Some(DockMenu { id, output }),
                    0x112 => self.launch_dock_app(&id),
                    0x110 => {
                        let windows = self
                            .dock
                            .apps
                            .iter()
                            .find(|a| a.id == id)
                            .map(|a| a.windows.clone())
                            .unwrap_or_default();
                        if windows.is_empty() {
                            self.launch_dock_app(&id);
                        } else if let Some(window) = self
                            .mru
                            .iter()
                            .find(|w| windows.contains(w))
                            .cloned()
                            .or_else(|| windows.last().cloned())
                        {
                            if self.focused_window().as_ref() == Some(&window) {
                                self.minimize_window(&window);
                            } else {
                                self.focus_window(Some(&window));
                                self.close_overview();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.queue_redraw_all();
        true
    }
    pub fn shell_axis(&mut self, horizontal: f64, vertical: f64) -> bool {
        let pos = self.pointer_location();
        let Some(output) = self.active_output() else {
            return false;
        };
        if !self.shell_visible(&output) {
            return false;
        }
        let Some(geo) = self.space.output_geometry(&output) else {
            return false;
        };
        if self.config.bar.enabled && pos.y - (geo.loc.y as f64) < PANEL as f64 {
            let delta = if horizontal.abs() > vertical.abs() {
                horizontal
            } else {
                vertical
            };
            if delta == 0.0 {
                self.topbar.scroll = 0.0;
                return true;
            }
            if delta.signum() != self.topbar.scroll.signum() {
                self.topbar.scroll = 0.0;
            }
            self.topbar.scroll += delta;
            if self.topbar.scroll.abs() >= 30.0 {
                let o = self.workspaces.ensure(&output);
                let next = if self.topbar.scroll < 0.0 {
                    o.list.active.saturating_sub(1)
                } else {
                    (o.list.active + 1).min(o.list.items.len() - 1)
                };
                self.topbar.scroll = 0.0;
                self.switch_workspace(&output, next);
            }
            return true;
        }
        self.shell_contains(pos)
    }
    pub fn finish_dock_launches(&mut self) {
        let mut i = 0;
        while i < self.dock.launches.len() {
            let launch = &self.dock.launches[i];
            if self
                .dock
                .apps
                .iter()
                .find(|a| a.id == launch.id)
                .is_some_and(|a| a.windows.len() > launch.baseline)
            {
                let launch = self.dock.launches.remove(i);
                if let Some(t) = launch.timer {
                    self.loop_handle.remove(t);
                }
                self.queue_redraw_all();
            } else {
                i += 1;
            }
        }
        if self.dock.changed {
            self.dock.changed = false;
            self.queue_redraw_all();
        }
    }
    fn launch_dock_app(&mut self, id: &str) {
        let Some(app) = self.dock.apps.iter().find(|a| a.id == id) else {
            return;
        };
        let Some(command) = app
            .entry
            .as_ref()
            .and_then(|e| e.command(&self.config.terminal))
        else {
            tracing::warn!(app=%id,"desktop entry has no launch command");
            return;
        };
        let baseline = app.windows.len();
        crate::state::spawn_shell(&command);
        if let Some(index) = self.dock.launches.iter().position(|l| l.id == id) {
            if let Some(t) = self.dock.launches.remove(index).timer {
                self.loop_handle.remove(t);
            }
        }
        let key = id.to_owned();
        let expired = key.clone();
        let timer = self
            .loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_secs(3)),
                move |_, _, gv| {
                    gv.dock.launches.retain(|l| l.id != expired);
                    gv.queue_redraw_all();
                    TimeoutAction::Drop
                },
            )
            .map_err(|error| tracing::warn!(%error,"cannot schedule launch timeout"))
            .ok();
        self.dock.launches.push(Launch {
            id: key,
            start: self.start_time.elapsed(),
            baseline,
            timer,
        });
        self.queue_redraw_all();
    }
    pub fn minimize_window(&mut self, window: &Window) {
        if mode_state(window).minimized || self.greeter.is_some() || self.protocols.locks.locked {
            return;
        }
        let Some(location) = self.space.element_location(window) else {
            return;
        };
        let Some(output) = self.output_for_window(window) else {
            return;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        self.workspaces.snapshot(&self.space);
        self.save_workspace_location(window, location);
        let id = self
            .dock
            .canonical(&crate::overlay::window_labels(window).1);
        let from = Rectangle::new(location, window.geometry().size).to_f64();
        let to = self.dock.target(&output, &id, geo);
        let now = self.start_time.elapsed();
        let current = self
            .dock
            .minimized
            .iter()
            .find(|m| &m.window == window)
            .map(|m| crate::anim::interpolate_rect(m.from, m.to, m.progress.value.clamp(0.0, 1.0)))
            .unwrap_or(from);
        let mut progress = Spring::new(0.0, SpringParams::SNAPPY);
        progress.advance(now);
        progress.set(1.0);
        self.dock.minimized.retain(|m| &m.window != window);
        self.dock.minimized.push(MinimizeMotion {
            window: window.clone(),
            output,
            location,
            from: current,
            to,
            progress,
        });
        with_mode_state(window, |s| s.minimized = true);
        self.window_animations.cancel(window);
        self.space.unmap_elem(window);
        self.focus_topmost();
        self.clear_desktop_pointer();
        self.queue_redraw_all();
    }
    /// Focus calls this after selecting the owning workspace, including Alt+Tab/overview.
    pub fn restore_minimized(&mut self, window: &Window) {
        if !mode_state(window).minimized {
            return;
        }
        let Some(location) = self.workspaces.location(window) else {
            return;
        };
        let Some(output) = self.output_for_window(window) else {
            return;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        let id = self
            .dock
            .canonical(&crate::overlay::window_labels(window).1);
        let now = self.start_time.elapsed();
        let from = self
            .dock
            .minimized
            .iter()
            .find(|m| &m.window == window)
            .map(|m| crate::anim::interpolate_rect(m.from, m.to, m.progress.value.clamp(0.0, 1.0)))
            .unwrap_or_else(|| self.dock.target(&output, &id, geo));
        let to = Rectangle::new(location, window.geometry().size).to_f64();
        let mut progress = Spring::new(0.0, SpringParams::SNAPPY);
        progress.advance(now);
        progress.set(1.0);
        self.dock.minimized.retain(|m| &m.window != window);
        self.dock.minimized.push(MinimizeMotion {
            window: window.clone(),
            output,
            location,
            from,
            to,
            progress,
        });
        with_mode_state(window, |s| s.minimized = false);
        self.space.map_element(window.clone(), location, false);
        self.window_animations.cancel(window);
        self.queue_redraw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn minimize_restore_keeps_geometry_mode_and_workspace() {
        let mut h = crate::state::tests::Harness::new();
        let output = Output::new(
            "minimize-test".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: (1600, 900).into(),
                refresh: 120_000,
            }),
            None,
            None,
            Some((0, 0).into()),
        );
        h.gv.space.map_output(&output, (0, 0));
        let w = h.window();
        h.gv.map_new_window(w.clone());
        h.gv.space.map_element(w.clone(), (123, 234), false);
        h.gv.minimize_window(&w);
        assert!(mode_state(&w).minimized);
        assert!(h.gv.space.element_location(&w).is_none());
        assert_eq!(h.gv.workspaces.location(&w), Some((123, 234).into()));
        assert_eq!(mode_state(&w).mode, WindowMode::Floating);
        assert!(h.gv.focused_window().is_none());
        // Workspace round trips must not implicitly restore a minimized client.
        h.gv.switch_workspace(&output, 1);
        h.gv.switch_workspace(&output, 0);
        assert!(h.gv.space.element_location(&w).is_none());
        h.gv.prune_navigation();
        assert!(h.gv.mru.contains(&w));
        h.gv.focus_window(Some(&w));
        assert!(!mode_state(&w).minimized);
        assert_eq!(h.gv.space.element_location(&w), Some((123, 234).into()));
        assert_eq!(h.gv.focused_window(), Some(w.clone()));
        let mut scene = None;
        for i in 1..400 {
            h.gv.dock.minimize_scene(
                &h.gv.space,
                &output,
                Duration::from_millis(i * 8),
                &mut scene,
            );
        }
        assert!(!h.gv.dock.active());
        h.gv.minimize_window(&w);
        h.gv.minimize_window(&w);
        assert_eq!(h.gv.dock.minimized.len(), 1);
    }
    #[test]
    fn work_area_reservations_preserve_origin_and_clamp() {
        let a = Rectangle::new((100, 200).into(), (1280, 800).into());
        assert_eq!(
            reserve(a, true, true, false),
            Rectangle::new((100, 230).into(), (1280, 698).into())
        );
        assert_eq!(reserve(a, true, true, true).size.h, 770);
        assert_eq!(reserve(a, false, false, false), a);
        assert_eq!(
            reserve(Rectangle::from_size((20, 10).into()), true, true, false)
                .size
                .h,
            0
        );
    }
    #[test]
    fn cosine_curve_and_order() {
        assert_eq!(magnification(0.0), DOCK_MAG);
        assert_eq!(magnification(DOCK_RANGE), 1.0);
        assert_eq!(magnification(999.0), 1.0);
        assert!(
            (magnification(75.0) - (1.0 + 0.72 * std::f64::consts::FRAC_1_SQRT_2)).abs() < 1e-12
        );
        assert_eq!(magnification(-75.0), magnification(75.0));
        let p = vec!["a".into(), "b".into(), "a".into()];
        let r = vec!["b".into(), "c".into(), "c".into(), "d".into()];
        assert_eq!(
            item_order(&p, &r),
            vec![
                ItemKey::App("a".into()),
                ItemKey::App("b".into()),
                ItemKey::Separator,
                ItemKey::App("c".into()),
                ItemKey::App("d".into())
            ]
        );
        assert_eq!(
            item_order(&[], &["c".into()]),
            vec![ItemKey::App("c".into())]
        );
        assert_eq!(item_order(&p, &[]).len(), 2);
    }
}
