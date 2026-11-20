//! Everything TTY mode draws: the status line on top, pane dividers, the
//! static vignette, the console switch dip and the CRT power animation.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                texture::{TextureBuffer, TextureRenderElement},
                Kind,
            },
            gles::{GlesRenderer, GlesTexture},
            Bind, Offscreen,
        },
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Point, Rectangle, Size, Transform},
};

use super::{crt_shape, fade_cover, Tty, BAR_HEIGHT, CRT_TIME};
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{
        geometry::rect,
        status::{battery_icons, strength_icon, volume_icon, Network, Snapshot},
        Canvas, FULL_COLOR,
    },
    ui::{
        spring::{Spring, SpringParams},
        text::{TextStyleSpec, Weight},
    },
    window_mode::{mode_state, WindowMode},
    workspace::{OwnedWindow, Workspace, Workspaces},
};

type Elem = GavnoRenderElement;

/// Console background at the centre; the vignette darkens towards the corners.
const BACKGROUND: [u8; 3] = [10, 10, 12];
const BAR_BG: [f32; 4] = [0.066, 0.066, 0.082, 1.0];
const HAIRLINE: [f32; 4] = [1.0, 1.0, 1.0, 0.075];
const TEXT: [f32; 4] = [0.80, 0.80, 0.84, 1.0];
const DIM: [f32; 4] = [0.50, 0.50, 0.56, 1.0];
const FAINT: [f32; 4] = [0.33, 0.33, 0.39, 1.0];
const FONT: f32 = 13.0;
const TEXT_HEIGHT: i32 = 16;
const PAD: i32 = 10;
const ICON: i32 = 14;

/// Render-side state: canvases and caches that are built once.
#[derive(Default)]
pub(super) struct View {
    front: Canvas,
    back: Canvas,
    icons: Icons,
    vignette: Option<((i32, i32), MemoryRenderBuffer)>,
    /// Position of the active-tab pill per output, in tab units.
    tabs: Vec<(String, Spring)>,
}

impl View {
    pub(super) fn tabs_active(&self) -> bool {
        self.tabs.iter().any(|(_, spring)| spring.is_active())
    }
    /// The pill spring of `output`, retargeted to `active`; the first frame
    /// starts in place.
    fn tab_position(&mut self, output: &str, active: usize, now: Duration) -> f64 {
        let at = match self.tabs.iter().position(|(name, _)| name == output) {
            Some(at) => at,
            None => {
                self.tabs.push((output.to_owned(), Spring::new(active as f64, SpringParams::SNAPPY)));
                self.tabs.len() - 1
            }
        };
        let spring = &mut self.tabs[at].1;
        if spring.target != active as f64 {
            spring.set(active as f64);
        }
        spring.advance(now)
    }
}

/// What a frame needs from the compositor.
pub(crate) struct Frame<'a> {
    pub workspaces: &'a Workspaces,
    pub space: &'a Space<Window>,
    pub status: &'a Snapshot,
    pub accent: [f32; 4],
    pub mod_label: &'static str,
    pub icon_theme: &'a str,
}

/// The workspace shows no window the user could see.
pub(crate) fn console_empty(workspace: &Workspace<OwnedWindow>) -> bool {
    !workspace.windows.iter().any(|w| !mode_state(&w.window).minimized)
}

fn style(color: [f32; 4]) -> TextStyleSpec {
    TextStyleSpec { size: FONT, weight: Weight::Mono, color, tracking: 0.0 }
}

/// A pane: a tiled window of a console.
fn pane(window: &Window) -> bool {
    let state = mode_state(window);
    crate::classic::windows::normal_window(window)
        && !state.tile_floating
        && !state.minimized
        && state.mode == WindowMode::Floating
}

/// "org.kde.konsole" -> "konsole", "Foot" -> "foot".
pub(crate) fn short_app(app: &str) -> String {
    app.rsplit('.').next().unwrap_or(app).trim().to_lowercase()
}

/// The tab label of a workspace: its name when it has one, else the app of
/// its focused window; empty for an empty workspace.
pub(crate) fn tab_label(workspace: &Workspace<OwnedWindow>) -> String {
    if let Some(name) = workspace.name.as_ref().filter(|n| !n.is_empty()) {
        return name.clone();
    }
    workspace
        .windows
        .iter()
        .rev()
        .find(|w| !mode_state(&w.window).minimized)
        .map(|w| short_app(&crate::overlay::window_labels(&w.window).1))
        .unwrap_or_default()
}

/// Colour of the battery icon: calm normally, amber and red when low, green
/// while charging.
pub(crate) fn battery_color(percent: u8, charging: bool) -> [u8; 3] {
    match (charging, percent) {
        (true, _) => [120, 200, 140],
        (_, 0..=10) => [240, 100, 90],
        (_, 11..=20) => [235, 180, 90],
        _ => [150, 150, 160],
    }
}

/// Radial vignette as premultiplied ARGB bytes, opaque.
pub(crate) fn vignette_pixels(width: i32, height: i32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let dx = (x as f32 + 0.5) / width as f32 * 2.0 - 1.0;
            let dy = (y as f32 + 0.5) / height as f32 * 2.0 - 1.0;
            let r = (dx * dx * 0.6 + dy * dy * 0.4).sqrt();
            let t = ((r - 0.35) / 0.85).clamp(0.0, 1.0);
            let shade = 1.0 - 0.6 * t * t * (3.0 - 2.0 * t);
            // Dither by a hash of the position, so the few dark levels do not
            // show as rings when the buffer is scaled up.
            let hash = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
            let hash = (hash ^ (hash >> 15)).wrapping_mul(0x2C1B_3C6D);
            let noise = ((hash >> 8) & 0xFFFF) as f32 / 65536.0;
            let channel = |c: u8| (c as f32 * shade + noise).floor().clamp(0.0, 255.0) as u8;
            pixels.extend_from_slice(&[channel(BACKGROUND[2]), channel(BACKGROUND[1]), channel(BACKGROUND[0]), 255]);
        }
    }
    pixels
}


impl Tty {
    /// Elements above everything: the status line and the dip through black.
    pub(crate) fn front(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        frame: &Frame,
    ) -> Vec<Elem> {
        let cover = self.fade.map_or(0.0, |start| fade_cover(start, now));
        let Some(owner) = frame.workspaces.get(output) else { return Vec::new() };
        let position = self.view.tab_position(&output.name(), owner.list.active, now);
        let view = &mut self.view;
        let c = &mut view.front;
        c.begin(renderer, scale);
        let w = size.w;
        c.rect(rect(0, 0, w, BAR_HEIGHT), 0.0, BAR_BG, 0.0, [0.0; 4]);
        c.rect(rect(0, BAR_HEIGHT - 1, w, 1), 0.0, HAIRLINE, 0.0, [0.0; 4]);
        let text_y = (BAR_HEIGHT - 1 - TEXT_HEIGHT) / 2;
        let accent = [frame.accent[0], frame.accent[1], frame.accent[2], 1.0];

        // Tabs on the left.
        let mut tabs = Vec::new();
        let mut x = 6;
        for (index, workspace) in owner.list.items.iter().enumerate() {
            let label = tab_label(workspace);
            let number = (index + 1).to_string();
            let text = if label.is_empty() { number.clone() } else { format!("{number} {label}") };
            let width = c.text.measure(&text, style(TEXT)).ceil() as i32;
            tabs.push((x, width + 2 * 8, text, number.len(), label.is_empty()));
            x += width + 2 * 8 + 2;
        }
        let tabs_end = x;
        // Draw order is back to front: the pill first.
        if !tabs.is_empty() {
            let last = tabs.len() - 1;
            let p = position.clamp(0.0, last as f64);
            let (lo, hi) = (p.floor() as usize, (p.ceil() as usize).min(last));
            let t = p - lo as f64;
            let pill_x = tabs[lo].0 as f64 + (tabs[hi].0 - tabs[lo].0) as f64 * t;
            let pill_w = tabs[lo].1 as f64 + (tabs[hi].1 - tabs[lo].1) as f64 * t;
            c.rect(rect(pill_x.round() as i32, 4, pill_w.round() as i32, BAR_HEIGHT - 9), 5.0, accent, 0.0, [0.0; 4]);
        }
        let on_accent = crate::ui::style::on_accent(accent);
        for (index, (tab_x, width, text, digits, empty)) in tabs.iter().enumerate() {
            // The label colour follows how much of the pill covers the tab.
            let cover_amount = (1.0 - (position - index as f64).abs()).clamp(0.0, 1.0) as f32;
            let base = if *empty { FAINT } else { DIM };
            let mix = |a: f32, b: f32| a + (b - a) * cover_amount;
            let color = [mix(base[0], on_accent[0]), mix(base[1], on_accent[1]), mix(base[2], on_accent[2]), 1.0];
            c.styled_label(renderer, text, tab_x + 8, text_y, width + 4, style(color));
            let _ = digits;
        }

        // Status segments on the right, from the right edge.
        let mut right = w - PAD;
        let divider = |c: &mut Canvas, x: i32| c.rect(rect(x, 8, 1, BAR_HEIGHT - 17), 0.0, HAIRLINE, 0.0, [0.0; 4]);
        let time = crate::ui::theme::LocalTime::now();
        let clock = format!("{} {:02} {} {:02}:{:02}", &time.weekday_name()[..3], time.day, &time.month_name()[..3], time.hour, time.minute);
        let clock_w = c.text.measure(&clock, style(TEXT)).ceil() as i32;
        right -= clock_w;
        c.styled_label(renderer, &clock, right, text_y, clock_w + 4, style(TEXT));
        let segment_gap = 14;
        let mut segments: Vec<(Vec<String>, [u8; 3], String)> = Vec::new();
        let snapshot = frame.status;
        match &snapshot.network {
            Network::Wifi { strength, .. } => segments.push((vec![strength_icon(*strength).into()], [150, 150, 160], format!("{strength}%"))),
            Network::Wired => segments.push((vec!["network-wired-symbolic".into()], [150, 150, 160], "eth".into())),
            Network::Offline => segments.push((vec!["network-wireless-offline-symbolic".into()], [95, 95, 105], "off".into())),
        }
        if let Some(volume) = snapshot.volume {
            let text = if volume.muted { "mute".to_owned() } else { format!("{}%", (volume.level * 100.0).round() as u32) };
            segments.push((vec![volume_icon(Some(volume)).into()], if volume.muted { [95, 95, 105] } else { [150, 150, 160] }, text));
        }
        if let Some(battery) = &snapshot.battery {
            segments.push((battery_icons(battery).to_vec(), battery_color(battery.percent, battery.charging), format!("{}%", battery.percent)));
        }
        let mut limit = tabs_end;
        // Draw the segments right to left.
        divider(c, right - segment_gap / 2 - 1);
        right -= segment_gap;
        for (names, color, text) in segments.iter().rev() {
            let text_w = c.text.measure(text, style(TEXT)).ceil() as i32;
            right -= text_w;
            let text_color = if *color == [150, 150, 160] || *color == [95, 95, 105] { DIM } else { [color[0] as f32 / 255.0, color[1] as f32 / 255.0, color[2] as f32 / 255.0, 1.0] };
            c.styled_label(renderer, text, right, text_y, text_w + 4, style(text_color));
            right -= 4 + ICON;
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let icon = rect(right, (BAR_HEIGHT - 1 - ICON) / 2, ICON, ICON);
            c.colored_icon(renderer, &mut view.icons, &refs, frame.icon_theme, icon, Some(*color));
            let _ = FULL_COLOR;
            divider(c, right - segment_gap / 2 - 1);
            right -= segment_gap;
        }
        limit = limit.max(0);

        // The focused title in the middle.
        if let Some(workspace) = owner.list.items.get(owner.list.active) {
            let title = workspace
                .windows
                .iter()
                .rev()
                .find(|w| !mode_state(&w.window).minimized)
                .map(|w| crate::overlay::window_labels(&w.window).0)
                .unwrap_or_default();
            let room = (right - limit - 24).max(0);
            if !title.is_empty() && room > 40 {
                let title = crate::topbar::ellipsize(&mut c.text, &title, style(DIM), room);
                let title_w = (c.text.measure(&title, style(DIM)).ceil() as i32).min(room);
                let x = ((w - title_w) / 2).clamp(limit + 12, (right - 12 - title_w).max(limit + 12));
                c.styled_label(renderer, &title, x, text_y, title_w + 4, style(DIM));
            }
        }
        if cover > 0.003 {
            c.rect(rect(0, 0, size.w, size.h), 0.0, [0.0, 0.0, 0.0, cover], 0.0, [0.0; 4]);
        }
        c.finish()
    }

    /// Behind the windows: the dividers between panes, the hint of an empty
    /// workspace and the static vignette.
    pub(crate) fn back(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        frame: &Frame,
    ) -> Vec<Elem> {
        let mut elements = Vec::new();
        let origin = frame.space.output_geometry(output).unwrap_or_default().loc;
        let accent = [frame.accent[0], frame.accent[1], frame.accent[2], 1.0];
        let panes: Vec<Rectangle<i32, Logical>> = frame
            .space
            .elements()
            .filter(|w| pane(w))
            .filter_map(|w| {
                let loc = frame.space.element_location(w)?;
                Some(Rectangle::new(loc - origin, w.geometry().size))
            })
            .collect();
        let empty = frame.workspaces.get(output).is_some_and(|o| console_empty(&o.list.items[o.list.active]));
        let c = &mut self.view.back;
        c.begin(renderer, scale);
        if empty {
            let hint = format!("{}+Enter  terminal", frame.mod_label);
            let width = c.text.measure(&hint, style(FAINT)).ceil() as i32;
            c.styled_label(renderer, &hint, (size.w - width) / 2, (size.h + BAR_HEIGHT) / 2 - TEXT_HEIGHT / 2, width + 4, style(FAINT));
        }
        // A 1 px frame around every pane; the focused one (the topmost) in
        // the accent. Windows cover their insides.
        for (index, area) in panes.iter().enumerate() {
            let focused = index + 1 == panes.len();
            let outer = Rectangle::new(area.loc - Point::from((1, 1)), area.size + Size::from((2, 2)));
            let color = if focused { [accent[0], accent[1], accent[2], 0.9] } else { [1.0, 1.0, 1.0, 0.10] };
            c.rect(outer, 0.0, [0.0; 4], 1.0, color);
        }
        elements.extend(c.finish());
        elements.push(self.vignette(renderer, size, scale));
        elements
    }

    fn vignette(&mut self, renderer: &mut GlesRenderer, size: Size<i32, Logical>, _scale: f64) -> Elem {
        let width = 480;
        let height = ((width as f64 * size.h as f64 / size.w.max(1) as f64).round() as i32).max(8);
        if self.view.vignette.as_ref().is_none_or(|(key, _)| *key != (width, height)) {
            let buffer = MemoryRenderBuffer::from_slice(
                &vignette_pixels(width, height),
                Fourcc::Argb8888,
                (width, height),
                1,
                Transform::Normal,
                None,
            );
            self.view.vignette = Some(((width, height), buffer));
        }
        let (_, buffer) = self.view.vignette.as_ref().expect("vignette was just built");
        let element = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (0.0, 0.0),
            buffer,
            Some(1.0),
            Some(Rectangle::from_size((width as f64, height as f64).into())),
            Some(size),
            Kind::Unspecified,
        );
        match element {
            Ok(element) => GavnoRenderElement::Memory(element),
            // Cannot happen with a valid buffer; fall back to the flat colour.
            Err(_) => GavnoRenderElement::Solid(smithay::backend::renderer::element::solid::SolidColorRenderElement::new(
                smithay::backend::renderer::element::Id::new(),
                Rectangle::from_size(size.to_physical_precise_round(1)),
                smithay::backend::renderer::utils::CommitCounter::default(),
                [0.04, 0.04, 0.05, 1.0],
                Kind::Unspecified,
            )),
        }
    }

    /// Wraps the finished frame in the CRT animation: the picture collapses
    /// to a bright line (power off) or grows out of one (power on). Returns
    /// `elements` untouched when no animation runs.
    pub(crate) fn crt_wrap(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: Vec<Elem>,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
    ) -> Vec<Elem> {
        let Some((start, power_on)) = self.crt else { return elements };
        let t = (now.saturating_sub(start).as_secs_f64() / CRT_TIME.as_secs_f64()).clamp(0.0, 1.0);
        if power_on && t >= 1.0 {
            return elements;
        }
        let position = if power_on { t } else { 1.0 - t };
        let (fraction, line_alpha) = crt_shape(position);
        let physical = (
            (size.w as f64 * scale).round() as i32,
            (size.h as f64 * scale).round() as i32,
        );
        let mut result = Vec::new();
        let c = &mut self.view.front;
        c.begin(renderer, scale);
        // Drawn first, so it ends up behind everything.
        c.rect(rect(0, 0, size.w, size.h), 0.0, [0.0, 0.0, 0.0, 1.0], 0.0, [0.0; 4]);
        let middle = size.h / 2;
        if line_alpha > 0.0 {
            let line = rect(0, middle - 1, size.w, 2);
            c.glow(rect(size.w / 8, middle - 1, size.w * 3 / 4, 2), 1.0, 14, [0.85, 0.9, 1.0, 0.55 * line_alpha]);
            c.rect(line, 0.0, [0.92, 0.95, 1.0, line_alpha], 0.0, [0.0; 4]);
        }
        let backdrop = c.finish();
        let height = (size.h as f64 * fraction).round() as i32;
        if height >= 1 {
            if let Some(texture) = render_texture(renderer, &elements, physical, scale) {
                let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
                let top = ((size.h - height) as f64 / 2.0 * scale).round();
                result.push(GavnoRenderElement::Texture(TextureRenderElement::from_texture_buffer(
                    Point::from((0.0, top)),
                    &buffer,
                    Some(1.0),
                    Some(Rectangle::from_size((physical.0 as f64, physical.1 as f64).into())),
                    Some((size.w, height).into()),
                    Kind::Unspecified,
                )));
            }
        }
        // The line is in front of the picture, the black behind it.
        let mut backdrop = backdrop.into_iter();
        let (front, back): (Vec<_>, Vec<_>) = {
            let all: Vec<_> = backdrop.by_ref().collect();
            let count = all.len();
            let mut all = all.into_iter();
            let front: Vec<_> = all.by_ref().take(count.saturating_sub(1)).collect();
            (front, all.collect())
        };
        let mut out = front;
        out.extend(result);
        out.extend(back);
        out
    }
}

/// Draws `elements` into a texture of `size` physical pixels.
fn render_texture(renderer: &mut GlesRenderer, elements: &[Elem], size: (i32, i32), scale: f64) -> Option<GlesTexture> {
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, size.into()).ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
        tracker.render_output(renderer, &mut target, 0, elements, [0.0; 4]).ok()?;
    }
    Some(texture)
}
