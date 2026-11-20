//! Built-in screenshot tool. Print captures the active output at once;
//! Shift+Print and Mod+Shift+S freeze the desktop and let the user pick an
//! area or a window. The frozen frame is captured once on open, so nothing
//! renders (and no client is asked for frames) while the overlay is up. The
//! PNG is encoded and saved on a worker thread, offered on the clipboard by
//! the compositor itself and previewed in a short toast. Nothing is registered
//! or kept while the tool is unused.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use smithay::{
    backend::renderer::{element::memory::MemoryRenderBuffer, gles::GlesRenderer},
    input::{keyboard::{keysyms, Keysym, ModifiersState}, pointer::CursorImageStatus},
    output::Output,
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{Logical, Point, Rectangle, Size},
};

use crate::{
    clipboard::{downscale, fit},
    render::GavnoRenderElement,
    topbar::{geometry::rect, label_style, Canvas, LINE},
    ui::{
        spring::{Spring, SpringParams},
        text::Weight,
    },
    Gavno,
};

type IRect = Rectangle<i32, Logical>;

/// Pointer travel before a press becomes a drag.
const DRAG_THRESHOLD: f64 = 4.0;
const TOAST_W: i32 = 300;
const TOAST_SECS: u64 = 3;
const DIM: [f32; 4] = [0.0, 0.0, 0.0, 0.5];
const GLASS: [f32; 4] = [0.065, 0.075, 0.10, 0.94];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotMode {
    /// The active output, immediately.
    Full,
    /// The selection overlay.
    Area,
}

// ---- selection geometry (pure) -----------------------------------------------

/// Selection state for one output, in output-local logical pixels.
pub struct Selector {
    bounds: IRect,
    /// Window rectangles, front to back, clipped to `bounds`.
    windows: Vec<IRect>,
    pub sel: Option<IRect>,
    anchor: Option<Point<f64, Logical>>,
    pub dragging: bool,
    pub hover: Option<IRect>,
    pub pointer: Point<f64, Logical>,
}

/// The rectangle spanned by two points, grown to whole pixels and clamped.
pub fn rect_between(a: Point<f64, Logical>, b: Point<f64, Logical>, bounds: IRect) -> IRect {
    let x0 = (a.x.min(b.x).floor() as i32).clamp(bounds.loc.x, bounds.loc.x + bounds.size.w - 1);
    let y0 = (a.y.min(b.y).floor() as i32).clamp(bounds.loc.y, bounds.loc.y + bounds.size.h - 1);
    let x1 = (a.x.max(b.x).ceil() as i32).clamp(x0 + 1, bounds.loc.x + bounds.size.w);
    let y1 = (a.y.max(b.y).ceil() as i32).clamp(y0 + 1, bounds.loc.y + bounds.size.h);
    rect(x0, y0, x1 - x0, y1 - y0)
}

impl Selector {
    pub fn new(bounds: IRect, windows: Vec<IRect>) -> Self {
        let windows = windows
            .into_iter()
            .filter_map(|w| w.intersection(bounds))
            .filter(|w| w.size.w >= 8 && w.size.h >= 8)
            .collect();
        Self { bounds, windows, sel: None, anchor: None, dragging: false, hover: None, pointer: Point::from((0.0, 0.0)) }
    }

    /// The frontmost window under `p`.
    pub fn window_at(&self, p: Point<f64, Logical>) -> Option<IRect> {
        self.windows.iter().copied().find(|w| w.to_f64().contains(p))
    }

    pub fn motion(&mut self, p: Point<f64, Logical>) {
        self.pointer = p;
        match self.anchor {
            Some(a) => {
                if !self.dragging && (p.x - a.x).hypot(p.y - a.y) >= DRAG_THRESHOLD {
                    self.dragging = true;
                }
                if self.dragging {
                    self.sel = Some(rect_between(a, p, self.bounds));
                }
            }
            None => self.hover = self.window_at(p),
        }
    }

    pub fn press(&mut self, p: Point<f64, Logical>) {
        self.pointer = p;
        self.anchor = Some(p);
        self.dragging = false;
    }

    /// A drag keeps its rectangle for nudging and Enter. A click inside the
    /// selection, or on a window, returns the area to capture.
    pub fn release(&mut self, p: Point<f64, Logical>) -> Option<IRect> {
        self.pointer = p;
        self.anchor.take()?;
        if self.dragging {
            self.dragging = false;
            if self.sel.is_some_and(|s| s.size.w < 2 || s.size.h < 2) {
                self.sel = None;
            }
            return None;
        }
        if let Some(sel) = self.sel.filter(|s| s.to_f64().contains(p)) {
            return Some(sel);
        }
        if let Some(window) = self.window_at(p) {
            return Some(window);
        }
        self.sel = None;
        None
    }

    /// Space or F: the whole output, or back to nothing.
    pub fn toggle_full(&mut self) {
        self.sel = if self.sel == Some(self.bounds) { None } else { Some(self.bounds) };
    }

    /// Moves the selection by whole pixels, keeping it on the output.
    pub fn nudge(&mut self, dx: i32, dy: i32) -> bool {
        let (Some(sel), false) = (self.sel, self.dragging) else { return false };
        let x = (sel.loc.x + dx).clamp(self.bounds.loc.x, self.bounds.loc.x + self.bounds.size.w - sel.size.w);
        let y = (sel.loc.y + dy).clamp(self.bounds.loc.y, self.bounds.loc.y + self.bounds.size.h - sel.size.h);
        self.sel = Some(rect(x, y, sel.size.w, sel.size.h));
        true
    }

    /// Enter: the selection, else the highlighted window.
    pub fn confirm(&self) -> Option<IRect> {
        self.sel.or(self.hover)
    }
}

/// Physical crop `(x, y, w, h)` of a frame for a logical selection.
pub fn crop_rect(sel: IRect, logical: Size<i32, Logical>, pixels: (u32, u32)) -> (u32, u32, u32, u32) {
    let fx = pixels.0 as f64 / logical.w.max(1) as f64;
    let fy = pixels.1 as f64 / logical.h.max(1) as f64;
    let x0 = ((sel.loc.x as f64 * fx).round() as i64).clamp(0, pixels.0 as i64 - 1) as u32;
    let y0 = ((sel.loc.y as f64 * fy).round() as i64).clamp(0, pixels.1 as i64 - 1) as u32;
    let x1 = (((sel.loc.x + sel.size.w) as f64 * fx).round() as i64).clamp(x0 as i64 + 1, pixels.0 as i64) as u32;
    let y1 = (((sel.loc.y + sel.size.h) as f64 * fy).round() as i64).clamp(y0 as i64 + 1, pixels.1 as i64) as u32;
    (x0, y0, x1 - x0, y1 - y0)
}

/// Copies a crop out of tightly packed RGBA, forcing opaque alpha.
pub fn crop_rgba(rgba: &[u8], pixels: (u32, u32), crop: (u32, u32, u32, u32)) -> Vec<u8> {
    let (x, y, w, h) = crop;
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in y..y + h {
        let start = ((row * pixels.0 + x) * 4) as usize;
        out.extend_from_slice(&rgba[start..start + (w * 4) as usize]);
    }
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
    out
}

// ---- files -------------------------------------------------------------------

/// `$XDG_PICTURES_DIR/Screenshots`, falling back to `user-dirs.dirs` and then
/// `~/Pictures/Screenshots`.
pub fn screenshots_dir(env_dir: Option<&str>, home: &Path, user_dirs: Option<&str>) -> PathBuf {
    let from_file = user_dirs.and_then(|text| {
        text.lines().find_map(|line| {
            let value = line.trim().strip_prefix("XDG_PICTURES_DIR=")?.trim().strip_prefix('"')?.strip_suffix('"')?;
            let path = if value == "$HOME" {
                home.to_owned()
            } else if let Some(tail) = value.strip_prefix("$HOME/") {
                home.join(tail)
            } else {
                PathBuf::from(value)
            };
            path.is_absolute().then_some(path)
        })
    });
    env_dir
        .filter(|d| Path::new(d).is_absolute())
        .map(PathBuf::from)
        .or(from_file)
        .unwrap_or_else(|| home.join("Pictures"))
        .join("Screenshots")
}

fn encode_png(rgba: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut out = Vec::with_capacity(rgb.len() / 3);
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::FilterType::Sub);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&rgb).ok()?;
    }
    Some(out)
}

/// Writes `png` to a fresh `Screenshot_<date>.png` in `dir`.
fn write_unique(dir: &Path, stamp: &str, png: &[u8]) -> Option<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(dir).ok()?;
    for n in 0..100 {
        let name = if n == 0 { format!("Screenshot_{stamp}.png") } else { format!("Screenshot_{stamp}_{n}.png") };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => return file.write_all(png).is_ok().then_some(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

pub struct Saved {
    png: Vec<u8>,
    path: Option<PathBuf>,
    width: u32,
    height: u32,
    thumb: (u32, u32, Vec<u8>),
}

/// Encodes, saves and thumbnails one capture. Runs on a worker thread.
fn save_job(rgba: Vec<u8>, width: u32, height: u32) -> Saved {
    let png = encode_png(&rgba, width, height).unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let path = home.and_then(|home| {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let user_dirs = std::fs::read_to_string(config.join("user-dirs.dirs")).ok();
        let dir = screenshots_dir(std::env::var("XDG_PICTURES_DIR").ok().as_deref(), &home, user_dirs.as_deref());
        let t = crate::ui::theme::LocalTime::now();
        let stamp = format!("{}-{:02}-{:02}_{:02}-{:02}-{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
        if png.is_empty() { None } else { write_unique(&dir, &stamp, &png) }
    });
    let (tw, th) = fit(width, height, 112, 64);
    let thumb = (tw, th, downscale(&rgba, width, height, tw, th));
    Saved { png, path, width, height, thumb }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

// ---- state -------------------------------------------------------------------

struct Frame {
    output: Output,
    geo: IRect,
    pixels: (u32, u32),
    rgba: Vec<u8>,
    buffer: MemoryRenderBuffer,
}

struct Session {
    frames: Vec<Frame>,
    selectors: Vec<Selector>,
    active: usize,
    saved_cursor: CursorImageStatus,
}

struct Toast {
    output: Output,
    thumb: MemoryRenderBuffer,
    thumb_px: (u32, u32),
    size: (u32, u32),
    caption: String,
    detail: String,
    folder: Option<PathBuf>,
    slide: Spring,
    closing: bool,
    /// Output-local rectangle from the last frame.
    area: Option<IRect>,
    timer: Option<RegistrationToken>,
}

pub struct Screenshot {
    session: Option<Session>,
    toast: Option<Toast>,
    canvas: Canvas,
    tx: Option<smithay::reexports::calloop::channel::Sender<Saved>>,
}

impl Default for Screenshot {
    fn default() -> Self {
        Self { session: None, toast: None, canvas: Canvas::default(), tx: None }
    }
}

pub struct Drawn {
    pub elements: Vec<GavnoRenderElement>,
    /// The frozen frame replaces everything else on this output.
    pub frozen: bool,
}

impl Screenshot {
    pub fn is_animating(&self) -> bool {
        self.toast.as_ref().is_some_and(|t| t.slide.is_active())
    }
    pub fn active(&self) -> bool {
        self.session.is_some()
    }
    pub fn toast_visible(&self) -> bool {
        self.toast.as_ref().is_some_and(|t| !t.closing && t.slide.value > 0.3)
    }
    /// Selection of the active output, output-local logical pixels.
    pub fn selection(&self) -> Option<IRect> {
        let s = self.session.as_ref()?;
        s.selectors.get(s.active)?.sel
    }

    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        accent: [f32; 4],
    ) -> Drawn {
        if let Some(toast) = &mut self.toast {
            toast.slide.advance(now);
            if toast.closing && !toast.slide.is_active() {
                self.toast = None;
            }
        }
        if self.session.is_none() && self.toast.as_ref().is_none_or(|t| &t.output != output) {
            return Drawn { elements: Vec::new(), frozen: false };
        }
        let c = &mut self.canvas;
        c.begin(renderer, scale);
        let (w, h) = (size.w, size.h);
        let mut frozen = false;
        if let Some(session) = &self.session {
            frozen = true;
            draw_frozen(c, renderer, session, output, w, h, accent);
        }
        if let Some(toast) = self.toast.as_mut().filter(|t| &t.output == output) {
            draw_toast(c, renderer, toast, w, h);
        }
        Drawn { elements: c.finish(), frozen }
    }
}

fn pill(c: &mut Canvas, renderer: &mut GlesRenderer, text: &str, x: i32, y: i32, size: f32, bounds: (i32, i32)) -> IRect {
    let style = label_style(size);
    let tw = c.text.measure(text, style).ceil() as i32;
    let (pw, ph) = (tw + 26, (size * 1.0) as i32 + 16);
    let x = x.clamp(8, (bounds.0 - pw - 8).max(8));
    let r = rect(x, y, pw, ph);
    c.shadow(r, 10.0);
    c.glass(r, 10.0, crate::ui::glass::tone::THICK, GLASS, 1.0, LINE);
    c.styled_label(renderer, text, x + 13, y + 8, tw + 4, style);
    r
}

fn draw_frozen(c: &mut Canvas, renderer: &mut GlesRenderer, session: &Session, output: &Output, w: i32, h: i32, accent: [f32; 4]) {
    let Some(index) = session.frames.iter().position(|f| &f.output == output) else {
        c.rect(rect(0, 0, w, h), 0.0, [0.02, 0.02, 0.03, 1.0], 0.0, LINE);
        return;
    };
    let frame = &session.frames[index];
    c.image(renderer, &frame.buffer, frame.pixels, rect(0, 0, w, h));
    let selector = &session.selectors[index];
    let active = index == session.active;
    let [ar, ag, ab, _] = accent;
    let highlight = if active { selector.sel.or(selector.hover.filter(|_| selector.sel.is_none())) } else { None };
    match highlight {
        None => c.rect(rect(0, 0, w, h), 0.0, DIM, 0.0, LINE),
        Some(s) => {
            let (x, y, sw, sh) = (s.loc.x, s.loc.y, s.size.w, s.size.h);
            for r in [rect(0, 0, w, y), rect(0, y + sh, w, h - y - sh), rect(0, y, x, sh), rect(x + sw, y, w - x - sw, sh)] {
                if r.size.w > 0 && r.size.h > 0 {
                    c.rect(r, 0.0, DIM, 0.0, LINE);
                }
            }
            if selector.sel.is_none() {
                c.rect(s, 0.0, [ar, ag, ab, 0.14], 2.0, [ar, ag, ab, 1.0]);
            } else {
                c.rect(rect(x - 1, y - 1, sw + 2, sh + 2), 0.0, [0.0; 4], 1.5, [1.0, 1.0, 1.0, 0.9]);
            }
        }
    }
    if !active {
        return;
    }
    // Crosshair while there is no finished selection.
    if selector.sel.is_none() || selector.dragging {
        let (px, py) = (selector.pointer.x.round() as i32, selector.pointer.y.round() as i32);
        let line = [1.0, 1.0, 1.0, 0.45];
        c.rect(rect(px, 0, 1, h), 0.0, line, 0.0, LINE);
        c.rect(rect(0, py, w, 1), 0.0, line, 0.0, LINE);
    }
    // Size label next to the selection or highlighted window.
    if let Some(s) = highlight {
        let (_, _, cw, ch) = crop_rect(s, rect(0, 0, w, h).size, frame.pixels);
        let text = if selector.sel.is_none() {
            format!("Window  {cw} x {ch}")
        } else if s == rect(0, 0, w, h) {
            format!("Full screen  {cw} x {ch}")
        } else {
            format!("{cw} x {ch}")
        };
        let label_w = (c.text.measure(&text, label_style(13.0)).ceil() as i32) + 26;
        let below = s.loc.y + s.size.h + 10;
        let y = if below + 30 <= h { below } else if s.loc.y >= 40 { s.loc.y - 38 } else { s.loc.y + 10 };
        let x = if y >= s.loc.y + s.size.h { s.loc.x + s.size.w - label_w } else { s.loc.x };
        pill(c, renderer, &text, x, y, 13.0, (w, h));
    }
    if !selector.dragging {
        let hint = if selector.sel.is_some() {
            "Enter capture    Arrows nudge    Space full screen    Esc cancel"
        } else {
            "Drag to select    Click a window    Space full screen    Esc cancel"
        };
        let hw = c.text.measure(hint, label_style(13.0)).ceil() as i32 + 26;
        pill(c, renderer, hint, (w - hw) / 2, h - 60, 13.0, (w, h));
    }
}

fn draw_toast(c: &mut Canvas, renderer: &mut GlesRenderer, toast: &mut Toast, w: i32, h: i32) {
    let slide = toast.slide.value.clamp(0.0, 1.0);
    let (iw, ih) = fit(toast.size.0, toast.size.1, (TOAST_W - 24) as u32, 150);
    let card_h = 12 + ih as i32 + 10 + 20 + 18 + 12;
    let area = rect(w - 16 - TOAST_W, h - 24 - card_h, TOAST_W, card_h);
    toast.area = Some(area);
    c.alpha = slide as f32;
    c.offset = Point::from(((1.0 - slide) * (TOAST_W as f64 + 40.0), 0.0));
    c.shadow(area, 16.0);
    c.rect(area, 16.0, [0.065, 0.075, 0.10, 0.97], 1.0, LINE);
    let image = rect(area.loc.x + (TOAST_W - iw as i32) / 2, area.loc.y + 12, iw as i32, ih as i32);
    c.image(renderer, &toast.thumb, toast.thumb_px, image);
    c.rect(image, 4.0, [0.0; 4], 1.0, [1.0, 1.0, 1.0, 0.18]);
    let mut caption = label_style(14.0);
    caption.weight = Weight::SemiBold;
    let ty = image.loc.y + image.size.h + 10;
    c.styled_label(renderer, &toast.caption, area.loc.x + 14, ty, TOAST_W - 28, caption);
    let mut detail = label_style(11.5);
    detail.color = [0.96, 0.95, 0.98, 0.55];
    c.styled_label(renderer, &toast.detail, area.loc.x + 14, ty + 20, TOAST_W - 28, detail);
}

// ---- compositor glue ---------------------------------------------------------

impl Gavno {
    pub fn screenshot_active(&self) -> bool {
        self.overlay.screenshot.active()
    }

    pub(crate) fn run_screenshot(&mut self, mode: ScreenshotMode) {
        if self.greeter.is_some() || self.protocols.locks.locked || self.lock.internal() {
            return;
        }
        if self.screenshot_active() {
            return;
        }
        match mode {
            ScreenshotMode::Full => self.screenshot_full(),
            ScreenshotMode::Area => self.open_screenshot_selection(),
        }
    }

    /// Last resort when the GPU readback fails: the old grim/slurp path.
    fn screenshot_fallback(&mut self, area: bool) {
        tracing::warn!("screenshot readback failed, falling back to grim");
        let output = self.active_output().map(|o| o.name());
        let command = crate::input::screenshot_command(area, output.as_deref());
        self.launch_shell(&command, None);
    }

    fn dismiss_toast(&mut self) {
        if let Some(toast) = self.overlay.screenshot.toast.take() {
            if let Some(token) = toast.timer {
                self.loop_handle.remove(token);
            }
        }
    }

    fn screenshot_full(&mut self) {
        self.dismiss_toast();
        let Some(output) = self.active_output() else { return };
        match crate::backend::capture_output(self, &output, false) {
            Some((w, h, rgba)) => self.finish_capture(output, w, h, rgba),
            None => self.screenshot_fallback(false),
        }
    }

    fn open_screenshot_selection(&mut self) {
        self.dismiss_toast();
        // No fading panel may end up in the frozen frame.
        self.launcher.dismiss();
        self.overlay.clipboard.picker.dismiss();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mut frames = Vec::new();
        let mut selectors = Vec::new();
        for output in outputs {
            let Some(geo) = self.space.output_geometry(&output) else { continue };
            let Some((w, h, rgba)) = crate::backend::capture_output(self, &output, false) else {
                return self.screenshot_fallback(true);
            };
            let bounds = rect(0, 0, geo.size.w, geo.size.h);
            let windows = self
                .space
                .elements()
                .rev()
                .filter_map(|window| {
                    let mut r = self.space.element_geometry(window)?;
                    r.loc -= geo.loc;
                    Some(r)
                })
                .collect();
            let buffer = MemoryRenderBuffer::from_slice(
                &rgba,
                smithay::backend::allocator::Fourcc::Abgr8888,
                (w as i32, h as i32),
                1,
                smithay::utils::Transform::Normal,
                None,
            );
            selectors.push(Selector::new(bounds, windows));
            frames.push(Frame { output, geo, pixels: (w, h), rgba, buffer });
        }
        if frames.is_empty() {
            return self.screenshot_fallback(true);
        }
        let pointer = self.pointer_location();
        let active = frames.iter().position(|f| f.geo.to_f64().contains(pointer)).unwrap_or(0);
        selectors[active].motion(pointer - frames[active].geo.loc.to_f64());
        let saved_cursor = std::mem::replace(
            &mut self.cursor_status,
            CursorImageStatus::Named(smithay::input::pointer::CursorIcon::Crosshair),
        );
        self.overlay.screenshot.session = Some(Session { frames, selectors, active, saved_cursor });
        self.queue_redraw_all();
    }

    fn close_screenshot(&mut self) {
        if let Some(session) = self.overlay.screenshot.session.take() {
            self.cursor_status = session.saved_cursor;
            self.queue_redraw_all();
        }
    }

    /// Keys while the selection overlay is open. Every key is consumed.
    pub(crate) fn screenshot_key(&mut self, sym: Keysym, mods: &ModifiersState) {
        let step = if mods.shift { 10 } else { 1 };
        let Some(session) = self.overlay.screenshot.session.as_mut() else { return };
        let selector = &mut session.selectors[session.active];
        let mut confirm = None;
        match sym.raw() {
            keysyms::KEY_Escape => return self.close_screenshot(),
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => confirm = selector.confirm(),
            keysyms::KEY_space | keysyms::KEY_f | keysyms::KEY_F => selector.toggle_full(),
            keysyms::KEY_Left => { selector.nudge(-step, 0); }
            keysyms::KEY_Right => { selector.nudge(step, 0); }
            keysyms::KEY_Up => { selector.nudge(0, -step); }
            keysyms::KEY_Down => { selector.nudge(0, step); }
            _ => {}
        }
        match confirm {
            Some(area) => self.finish_selection(area),
            None => self.queue_redraw_all(),
        }
    }

    /// Pointer motion while the overlay is open. Returns true when consumed.
    pub(crate) fn screenshot_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some(session) = self.overlay.screenshot.session.as_mut() else { return false };
        // Another output takes over once nothing is being dragged.
        if session.selectors[session.active].anchor.is_none() {
            if let Some(i) = session.frames.iter().position(|f| f.geo.to_f64().contains(pos)) {
                if i != session.active {
                    let old = &mut session.selectors[session.active];
                    old.sel = None;
                    old.hover = None;
                    session.active = i;
                }
            }
        }
        let origin = session.frames[session.active].geo.loc.to_f64();
        session.selectors[session.active].motion(pos - origin);
        true
    }

    /// Pointer buttons: the overlay takes all of them, the toast takes a click.
    pub(crate) fn screenshot_button(&mut self, pos: Point<f64, Logical>, button: u32, pressed: bool) -> bool {
        if let Some(session) = self.overlay.screenshot.session.as_mut() {
            if pressed {
                if !self.suppressed_buttons.contains(&button) {
                    self.suppressed_buttons.push(button);
                }
            } else {
                self.suppressed_buttons.retain(|b| *b != button);
            }
            // Only the left button selects; the rest cancel on press.
            if button != 0x110 {
                if pressed && button == 0x111 {
                    self.close_screenshot();
                }
                return true;
            }
            let origin = session.frames[session.active].geo.loc.to_f64();
            let selector = &mut session.selectors[session.active];
            let local = pos - origin;
            if pressed {
                selector.press(local);
                self.queue_redraw_all();
                return true;
            }
            match selector.release(local) {
                Some(area) => self.finish_selection(area),
                None => self.queue_redraw_all(),
            }
            return true;
        }
        if !pressed || !self.overlay.screenshot.toast_visible() {
            return false;
        }
        let Some(toast) = self.overlay.screenshot.toast.as_ref() else { return false };
        let Some(geo) = self.space.output_geometry(&toast.output) else { return false };
        if !toast.area.is_some_and(|a| a.to_f64().contains(pos - geo.loc.to_f64())) {
            return false;
        }
        if !self.suppressed_buttons.contains(&button) {
            self.suppressed_buttons.push(button);
        }
        if let Some(folder) = toast.folder.clone() {
            self.launch_shell(&format!("xdg-open {}", shell_quote(&folder)), None);
        }
        self.dismiss_toast();
        self.queue_redraw_all();
        true
    }

    /// Crops the frozen frame and hands it to the save path.
    fn finish_selection(&mut self, area: IRect) {
        let Some(session) = self.overlay.screenshot.session.take() else { return };
        self.cursor_status = session.saved_cursor.clone();
        let Session { mut frames, active, .. } = session;
        let frame = frames.swap_remove(active);
        let crop = crop_rect(area, frame.geo.size, frame.pixels);
        let rgba = crop_rgba(&frame.rgba, frame.pixels, crop);
        drop(frames);
        self.finish_capture(frame.output, crop.2, crop.3, rgba);
        self.queue_redraw_all();
    }

    /// Shows the toast now, saves and copies when the worker is done.
    fn finish_capture(&mut self, output: Output, w: u32, h: u32, rgba: Vec<u8>) {
        let mut rgba = rgba;
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let (tw, th) = fit(w, h, 2 * (TOAST_W - 24) as u32, 300);
        let thumb_px = downscale(&rgba, w, h, tw, th);
        let thumb = MemoryRenderBuffer::from_slice(
            &thumb_px,
            smithay::backend::allocator::Fourcc::Abgr8888,
            (tw as i32, th as i32),
            1,
            smithay::utils::Transform::Normal,
            None,
        );
        self.dismiss_toast();
        let timer = self
            .loop_handle
            .insert_source(Timer::from_duration(Duration::from_secs(TOAST_SECS)), |_, _, gv| {
                if let Some(toast) = gv.overlay.screenshot.toast.as_mut() {
                    toast.closing = true;
                    toast.timer = None;
                    toast.slide.set(0.0);
                }
                gv.queue_redraw_all();
                TimeoutAction::Drop
            })
            .ok();
        let mut slide = Spring::new(0.0, SpringParams::SNAPPY);
        slide.set(1.0);
        self.overlay.screenshot.toast = Some(Toast {
            output,
            thumb,
            thumb_px: (tw, th),
            size: (w, h),
            caption: "Copied - click to open folder".into(),
            detail: format!("{w} x {h}  -  saving"),
            folder: None,
            slide,
            closing: false,
            area: None,
            timer,
        });
        if self.overlay.screenshot.tx.is_none() {
            self.overlay.screenshot.tx = crate::clipboard::worker_channel(&self.loop_handle, |saved: Saved, gv: &mut Gavno| gv.screenshot_saved(saved));
        }
        match self.overlay.screenshot.tx.clone() {
            Some(tx) => {
                let spawned = std::thread::Builder::new().name("gavno-shot-save".into()).spawn(move || {
                    let _ = tx.send(save_job(rgba, w, h));
                });
                if spawned.is_err() {
                    self.screenshot_saved(Saved { png: Vec::new(), path: None, width: w, height: h, thumb: (1, 1, vec![0; 4]) });
                }
            }
            None => self.screenshot_saved(Saved { png: Vec::new(), path: None, width: w, height: h, thumb: (1, 1, vec![0; 4]) }),
        }
        self.queue_redraw_all();
    }

    /// The worker finished: own the clipboard, remember it, update the toast.
    fn screenshot_saved(&mut self, saved: Saved) {
        let Saved { png, path, width, height, thumb } = saved;
        let copied = !png.is_empty();
        if copied {
            let bytes = Arc::new(png);
            self.own_png_selection(bytes.clone());
            self.clipboard_record_image((*bytes).clone(), width, height, Some(thumb));
        }
        if let Some(toast) = self.overlay.screenshot.toast.as_mut() {
            toast.folder = path.as_deref().and_then(Path::parent).map(Path::to_path_buf);
            toast.caption = match (copied, &path) {
                (true, Some(_)) => "Copied - click to open folder".into(),
                (true, None) => "Copied - could not save the file".into(),
                (false, _) => "Screenshot failed".into(),
            };
            toast.detail = match path.as_deref().and_then(Path::file_name) {
                Some(name) => name.to_string_lossy().into_owned(),
                None => format!("{width} x {height}"),
            };
        }
        self.queue_redraw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }

    fn selector() -> Selector {
        Selector::new(
            rect(0, 0, 1000, 600),
            vec![rect(100, 100, 400, 300), rect(0, 0, 1000, 600), rect(900, 500, 300, 300)],
        )
    }

    #[test]
    fn dragging_selects_a_normalised_clamped_rectangle() {
        let mut s = selector();
        s.press(p(300.0, 200.0));
        s.motion(p(301.0, 201.0));
        assert!(!s.dragging && s.sel.is_none(), "below the drag threshold");
        s.motion(p(100.5, 50.0));
        assert!(s.dragging);
        assert_eq!(s.sel, Some(rect(100, 50, 200, 150)));
        s.motion(p(2000.0, 2000.0));
        assert_eq!(s.sel, Some(rect(300, 200, 700, 400)));
        assert_eq!(s.release(p(2000.0, 2000.0)), None, "release keeps the selection for Enter");
        assert_eq!(s.confirm(), Some(rect(300, 200, 700, 400)));
        // A zero-sized drag cannot produce an empty selection.
        let mut s = selector();
        s.press(p(10.0, 10.0));
        s.motion(p(10.0, 30.0));
        assert_eq!(s.sel.map(|r| r.size.w), Some(1));
    }

    #[test]
    fn window_hit_testing_prefers_the_front_and_clips() {
        let mut s = selector();
        s.motion(p(150.0, 150.0));
        assert_eq!(s.hover, Some(rect(100, 100, 400, 300)));
        s.motion(p(600.0, 150.0));
        assert_eq!(s.hover, Some(rect(0, 0, 1000, 600)));
        // The third window is clipped to the output.
        assert_eq!(s.window_at(p(950.0, 550.0)), Some(rect(0, 0, 1000, 600)));
        let only = Selector::new(rect(0, 0, 1000, 600), vec![rect(900, 500, 300, 300), rect(0, 0, 4, 4)]);
        assert_eq!(only.window_at(p(950.0, 550.0)), Some(rect(900, 500, 100, 100)));
        assert_eq!(only.window_at(p(1.0, 1.0)), None, "tiny windows are not targets");
        assert_eq!(only.window_at(p(10.0, 10.0)), None);
    }

    #[test]
    fn clicks_capture_the_selection_or_the_window() {
        let mut s = selector();
        s.press(p(150.0, 150.0));
        assert_eq!(s.release(p(150.0, 150.0)), Some(rect(100, 100, 400, 300)), "click on a window");
        let mut s = Selector::new(rect(0, 0, 1000, 600), vec![rect(100, 100, 400, 300)]);
        s.press(p(10.0, 10.0));
        s.motion(p(60.0, 60.0));
        s.release(p(60.0, 60.0));
        let sel = s.sel.unwrap();
        s.press(p(20.0, 20.0));
        assert_eq!(s.release(p(20.0, 20.0)), Some(sel), "click inside the selection confirms it");
        s.press(p(800.0, 500.0));
        assert_eq!(s.release(p(800.0, 500.0)), None);
        assert_eq!(s.sel, None, "a click on nothing clears the selection");
    }

    #[test]
    fn space_toggles_full_and_arrows_nudge_inside_the_output() {
        let mut s = selector();
        assert!(!s.nudge(1, 0), "nothing to nudge yet");
        s.toggle_full();
        assert_eq!(s.sel, Some(rect(0, 0, 1000, 600)));
        assert!(s.nudge(10, 10));
        assert_eq!(s.sel, Some(rect(0, 0, 1000, 600)), "a full-size selection cannot move");
        s.toggle_full();
        assert_eq!(s.sel, None);
        s.press(p(100.0, 100.0));
        s.motion(p(300.0, 250.0));
        s.release(p(300.0, 250.0));
        assert!(s.nudge(-1, 0));
        assert_eq!(s.sel, Some(rect(99, 100, 200, 150)));
        assert!(s.nudge(0, 10));
        assert_eq!(s.sel, Some(rect(99, 110, 200, 150)));
        s.nudge(-500, -500);
        assert_eq!(s.sel, Some(rect(0, 0, 200, 150)));
        s.nudge(5000, 5000);
        assert_eq!(s.sel, Some(rect(800, 450, 200, 150)));
        assert_eq!(s.confirm(), s.sel);
        // Without a selection Enter takes the highlighted window.
        let mut s = selector();
        s.motion(p(150.0, 150.0));
        assert_eq!(s.confirm(), Some(rect(100, 100, 400, 300)));
    }

    #[test]
    fn crops_map_logical_to_physical_pixels() {
        let logical = rect(0, 0, 1600, 900).size;
        assert_eq!(crop_rect(rect(0, 0, 1600, 900), logical, (2160, 1215)), (0, 0, 2160, 1215));
        assert_eq!(crop_rect(rect(100, 100, 200, 100), logical, (3200, 1800)), (200, 200, 400, 200));
        let tiny = crop_rect(rect(1599, 899, 1, 1), logical, (1600, 900));
        assert_eq!(tiny, (1599, 899, 1, 1));
        let rgba: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
        let out = crop_rgba(&rgba, (4, 3), (1, 1, 2, 2));
        assert_eq!(out.len(), 16);
        assert_eq!(&out[..3], &[20, 21, 22]);
        assert!(out.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn screenshot_directory_follows_the_xdg_rules() {
        let home = Path::new("/home/u");
        assert_eq!(screenshots_dir(None, home, None), Path::new("/home/u/Pictures/Screenshots"));
        assert_eq!(screenshots_dir(Some("/data/pics"), home, None), Path::new("/data/pics/Screenshots"));
        assert_eq!(screenshots_dir(Some("relative"), home, None), Path::new("/home/u/Pictures/Screenshots"));
        let dirs = "XDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_PICTURES_DIR=\"$HOME/Bilder\"\n";
        assert_eq!(screenshots_dir(None, home, Some(dirs)), Path::new("/home/u/Bilder/Screenshots"));
        assert_eq!(screenshots_dir(Some("/x"), home, Some(dirs)), Path::new("/x/Screenshots"));
        assert_eq!(shell_quote(Path::new("/a b/it's")), "'/a b/it'\\''s'");
    }

    #[test]
    fn saving_writes_a_unique_png_and_a_thumbnail() {
        let dir = std::env::temp_dir().join(format!("gavno-shot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rgba: Vec<u8> = (0..64 * 32).flat_map(|_| [10u8, 200, 30, 255]).collect();
        let png = encode_png(&rgba, 64, 32).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let a = write_unique(&dir, "2026-01-01_00-00-00", &png).unwrap();
        let b = write_unique(&dir, "2026-01-01_00-00-00", &png).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), png);
        let decoder = png::Decoder::new(std::fs::File::open(&a).unwrap());
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buf).unwrap();
        assert_eq!((frame.width, frame.height), (64, 32));
        assert_eq!(&buf[..3], &[10, 200, 30]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
