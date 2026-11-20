//! Everything TTY mode draws: the static vignette, the empty console, the
//! status line, the console switch dip and the CRT power animation.

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

use super::{crt_shape, fade_cover, Tty, CRT_TIME};
use crate::{
    overview::SceneWindow,
    render::GavnoRenderElement,
    topbar::{geometry::rect, status::{Network, Snapshot}, Canvas},
    ui::text::{TextStyleSpec, Weight},
    window_mode::{mode_state, WindowMode},
    workspace::{OwnedWindow, Workspace, Workspaces},
};

type Elem = GavnoRenderElement;

/// Console background at the centre; the vignette darkens towards the corners.
const BACKGROUND: [u8; 3] = [10, 10, 12];
const TEXT: [f32; 4] = [0.78, 0.78, 0.81, 1.0];
const DIM: [f32; 4] = [0.46, 0.46, 0.51, 1.0];
const STRIP: [f32; 4] = [0.085, 0.085, 0.10, 1.0];
const FONT: f32 = 15.0;
const LINE_HEIGHT: i32 = 20;
const MARGIN: i32 = 14;
const STRIP_HEIGHT: i32 = 24;

/// Render-side state: canvases and caches that are built once.
#[derive(Default)]
pub(super) struct View {
    front: Canvas,
    back: Canvas,
    vignette: Option<((i32, i32), MemoryRenderBuffer)>,
    host: Option<String>,
    user: Option<String>,
}

/// What a frame needs from the compositor.
pub(crate) struct Frame<'a> {
    pub workspaces: &'a Workspaces,
    pub status: &'a Snapshot,
    pub accent: [f32; 4],
    /// Notification lines above the console text.
    pub rows: usize,
}

/// The workspace shows no window the user could see.
pub(crate) fn console_empty(workspace: &Workspace<OwnedWindow>) -> bool {
    !workspace.windows.iter().any(|w| !mode_state(&w.window).minimized)
}

fn style(color: [f32; 4]) -> TextStyleSpec {
    TextStyleSpec { size: FONT, weight: Weight::Mono, color, tracking: 0.0 }
}

/// A tile that fills the console: it competes for the screen with the others.
fn monocle_candidate(window: &Window) -> bool {
    let state = mode_state(window);
    crate::classic::windows::normal_window(window)
        && !state.tile_floating
        && !state.minimized
        && state.mode == WindowMode::Floating
}

/// The windows to draw, back to front: every window that is not a console
/// filler, and only the topmost of the fillers.
pub(crate) fn scene(space: &Space<Window>, output: &Output) -> Vec<SceneWindow> {
    let origin = space.output_geometry(output).unwrap_or_default().loc;
    let elements: Vec<&Window> = space.elements().collect();
    let top = elements.iter().rposition(|w| monocle_candidate(w));
    elements
        .into_iter()
        .enumerate()
        .filter(|(i, w)| !monocle_candidate(w) || Some(*i) == top)
        .filter_map(|(_, window)| {
            let location = space.element_location(window)?;
            Some(SceneWindow {
                window: window.clone(),
                location,
                rect: Rectangle::new((location - origin).to_f64(), window.geometry().size.to_f64()),
            })
        })
        .collect()
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

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

impl View {
    fn identity(&mut self) -> (String, String) {
        let host = self
            .host
            .get_or_insert_with(|| read_trimmed("/etc/hostname").unwrap_or_else(|| "localhost".into()))
            .clone();
        let user = self
            .user
            .get_or_insert_with(|| std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "user".into()))
            .clone();
        (host, user)
    }
}

/// The lines of an empty console, top to bottom, and the column of the cursor.
pub(crate) fn console_lines(host: &str, user: &str, number: usize) -> [String; 4] {
    [
        format!("GAVNO {} {host} tty{number}", env!("CARGO_PKG_VERSION")),
        String::new(),
        format!("{host} login: {user}"),
        format!("[{user}@{host} ~]$ "),
    ]
}

/// "wifi 87%  bat 64%  14:32"
pub(crate) fn status_right(status: &Snapshot, hour: u32, minute: u32) -> String {
    let mut parts = Vec::new();
    match &status.network {
        Network::Wifi { strength, .. } => parts.push(format!("wifi {strength}%")),
        Network::Wired => parts.push("eth".to_owned()),
        Network::Offline => parts.push("net off".to_owned()),
    }
    if let Some(battery) = &status.battery {
        parts.push(format!("bat {}%{}", battery.percent, if battery.charging { "+" } else { "" }));
    }
    parts.push(format!("{hour:02}:{minute:02}"));
    parts.join("  ")
}

/// The app id of the window on top of a console, or nothing.
fn console_app(workspace: &Workspace<OwnedWindow>) -> String {
    workspace
        .windows
        .iter()
        .rev()
        .find(|w| !mode_state(&w.window).minimized)
        .map(|w| crate::overlay::window_labels(&w.window).1)
        .unwrap_or_default()
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
        let alpha = self.status_alpha(now);
        let cover = self.fade.map_or(0.0, |start| fade_cover(start, now));
        if alpha <= 0.0 && cover <= 0.003 {
            return Vec::new();
        }
        let c = &mut self.view.front;
        c.begin(renderer, scale);
        if alpha > 0.0 {
            if let Some(owner) = frame.workspaces.get(output) {
                let workspace = &owner.list.items[owner.list.active];
                let y = size.h - STRIP_HEIGHT;
                c.alpha = alpha;
                c.rect(rect(0, y, size.w, STRIP_HEIGHT), 0.0, STRIP, 0.0, [0.0; 4]);
                let text_y = y + (STRIP_HEIGHT - LINE_HEIGHT) / 2 + 1;
                let number = format!("tty{}", owner.list.active + 1);
                let accent = TextStyleSpec { color: [frame.accent[0], frame.accent[1], frame.accent[2], 1.0], ..style(TEXT) };
                let number_w = c.text.measure(&number, accent).ceil() as i32;
                c.styled_label(renderer, &number, MARGIN, text_y, number_w + 4, accent);
                let app = console_app(workspace);
                if !app.is_empty() {
                    let gap = c.text.measure("  ", style(DIM)).ceil() as i32;
                    c.styled_label(renderer, &app, MARGIN + number_w + gap, text_y, size.w / 2, style(DIM));
                }
                let time = crate::ui::theme::LocalTime::now();
                let right = status_right(frame.status, time.hour, time.minute);
                let right_w = c.text.measure(&right, style(DIM)).ceil() as i32;
                c.styled_label(renderer, &right, size.w - MARGIN - right_w, text_y, right_w + 4, style(DIM));
                c.alpha = 1.0;
            }
        }
        if cover > 0.003 {
            c.rect(rect(0, 0, size.w, size.h), 0.0, [0.0, 0.0, 0.0, cover], 0.0, [0.0; 4]);
        }
        c.finish()
    }

    /// The console itself, behind every window: the empty-console text and
    /// the static vignette.
    pub(crate) fn back(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        frame: &Frame,
    ) -> Vec<Elem> {
        let mut elements = Vec::new();
        if let Some(owner) = frame.workspaces.get(output) {
            if console_empty(&owner.list.items[owner.list.active]) {
                let (host, user) = self.view.identity();
                let lines = console_lines(&host, &user, owner.list.active + 1);
                let blink = self.blink_on;
                let top = MARGIN + frame.rows as i32 * LINE_HEIGHT;
                let c = &mut self.view.back;
                c.begin(renderer, scale);
                for (i, line) in lines.iter().enumerate() {
                    if line.is_empty() {
                        continue;
                    }
                    let color = if i == 0 { DIM } else { TEXT };
                    let width = c.text.measure(line, style(color)).ceil() as i32 + 4;
                    c.styled_label(renderer, line, MARGIN, top + i as i32 * LINE_HEIGHT, width, style(color));
                }
                if blink {
                    let x = MARGIN + c.text.measure(&lines[3], style(TEXT)).ceil() as i32;
                    let cell = c.text.measure("M", style(TEXT)).ceil() as i32;
                    let y = top + 3 * LINE_HEIGHT;
                    c.rect(rect(x, y + 2, cell, LINE_HEIGHT - 3), 0.0, [TEXT[0], TEXT[1], TEXT[2], 0.9], 0.0, [0.0; 4]);
                }
                elements.extend(c.finish());
            }
        }
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
