//! Geometry, hit targets and painting of the quick settings panel.
//!
//! `geometry` and `hits` are pure functions of the rows and the output size,
//! so the layout and the pointer targets are tested without a renderer, and
//! the painter draws exactly the rectangles the hit testing uses.
use super::{Action, Info, Page, Row, RowKind, State};
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        Canvas, FULL_COLOR, LINE,
    },
    ui::{
        style::{self as look, dim, ease_out, radius, text as style, SURFACE, TEXT},
        text::Weight,
    },
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    output::Output,
    utils::{Logical, Size},
};
use std::time::Instant;

pub(super) const WIDTH: i32 = 360;
const PAD: i32 = look::space::LG;
const TILE_H: i32 = 72;
const SLIDER_H: i32 = 28;
const BUTTON: i32 = 32;
const HEADER_H: i32 = 56;
const ROW_H: i32 = 44;
const SLIDER_ROW_H: i32 = 52;
const APP_H: i32 = 52;
const SECTION_H: i32 = 28;
const ERROR_H: i32 = 24;
const PASSWORD_H: i32 = 72;
const HINTS_H: i32 = 48;
const BOTTOM: i32 = 12;
/// Distance of a non-Classic panel from the top of the output.
const TOP: i32 = 36;
/// Classic keeps the panel above the taskbar.
const TASKBAR: i32 = 40;

pub(super) struct View {
    canvas: Canvas,
    icons: Icons,
    /// Pointer targets in output-local logical coordinates; later entries win.
    /// The flag marks a slider, which starts a drag rather than an action.
    pub hits: Vec<(Rect, Action, bool)>,
    pub panel: Rect,
}
impl Default for View {
    fn default() -> Self {
        Self {
            canvas: Canvas::default(),
            icons: Icons::nonblocking(crate::icons::app_worker::Consumer::Panel),
            hits: Vec::new(),
            panel: Rect::default(),
        }
    }
}
#[derive(Clone, Copy, Default)]
pub(super) struct Extras {
    pub error: bool,
    pub password: bool,
    pub hints: bool,
}
impl Extras {
    fn height(self) -> i32 {
        [
            (self.error, ERROR_H),
            (self.password, PASSWORD_H),
            (self.hints, HINTS_H),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, h)| h)
        .sum()
    }
}
/// The rectangles of one row. Which ones matter depends on the row kind.
#[derive(Clone, Copy, Default)]
pub(super) struct Cell {
    pub row: usize,
    pub area: Rect,
    /// Tile: the icon disc. Slider: the mute zone. App slider: the app icon.
    pub icon: Rect,
    pub track: Rect,
    /// Tile and slider: the zone that opens the details page.
    pub chevron: Rect,
    /// Network: the Disconnect button.
    pub aux: Rect,
}
#[derive(Default)]
pub(super) struct Geometry {
    pub panel: Rect,
    pub back: Option<Rect>,
    pub title: Rect,
    /// The row index and the clickable rectangle of the header switch.
    pub switch: Option<(usize, Rect)>,
    pub cells: Vec<Cell>,
    pub sections: Vec<(&'static str, Rect)>,
    /// Media info and the previous, play and next buttons.
    pub media: Option<[Rect; 4]>,
    /// Battery and power profile text on the main page.
    pub footer_text: Rect,
    pub error: Rect,
    pub password: Rect,
    pub hints: Rect,
    pub scrollbar: Option<(Rect, Rect)>,
}
enum Entry {
    Header(&'static str),
    Row(usize),
}
fn entries(rows: &[Row]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut section = "";
    for (i, row) in rows.iter().enumerate() {
        if row.kind == RowKind::Switch {
            continue;
        }
        if row.section != section {
            section = row.section;
            if !section.is_empty() {
                out.push(Entry::Header(section));
            }
        }
        out.push(Entry::Row(i));
    }
    out
}
fn entry_height(rows: &[Row], entry: &Entry) -> i32 {
    match entry {
        Entry::Header(_) => SECTION_H,
        Entry::Row(i) => match rows[*i].kind {
            RowKind::Slider => SLIDER_ROW_H,
            RowKind::AppSlider => APP_H,
            _ => ROW_H,
        },
    }
}
fn translate(r: Rect, dx: i32, dy: i32) -> Rect {
    rect(r.loc.x + dx, r.loc.y + dy, r.size.w, r.size.h)
}
/// Panel rectangle at the top right, or above the Classic taskbar.
fn place(size: Size<i32, Logical>, classic: bool, height: i32) -> Rect {
    let w = WIDTH.min((size.w - 16).max(1));
    let x = (size.w - w - 8).max(0);
    let y = if classic {
        (size.h - TASKBAR - height - 8).max(0)
    } else {
        TOP.min((size.h - height).max(0))
    };
    rect(x, y, w, height)
}
fn max_height(size: Size<i32, Logical>, classic: bool) -> i32 {
    if classic {
        size.h - TASKBAR - 16
    } else {
        size.h - TOP - 8
    }
    .max(120)
}
/// Space for the list of a detail page on this output.
pub(super) fn viewport(size: Size<i32, Logical>, classic: bool, extras: Extras) -> i32 {
    max_height(size, classic) - (HEADER_H + 4 + BOTTOM + extras.height())
}
/// The first visible entry that keeps row `selected` inside `viewport`.
pub(super) fn scroll_to(rows: &[Row], selected: usize, offset: usize, viewport: i32) -> usize {
    let list = entries(rows);
    let mut offset = offset.min(list.len().saturating_sub(1));
    let Some(at) = list
        .iter()
        .position(|e| matches!(e, Entry::Row(i) if *i == selected))
    else {
        return offset;
    };
    // A section header stays attached to its first row.
    let at_top = if at > 0 && matches!(list[at - 1], Entry::Header(_)) { at - 1 } else { at };
    offset = offset.min(at_top);
    while offset < at {
        let used: i32 = list[offset..=at].iter().map(|e| entry_height(rows, e)).sum();
        if used <= viewport {
            break;
        }
        offset += 1;
    }
    offset
}
pub(super) fn geometry(
    page: Page,
    rows: &[Row],
    size: Size<i32, Logical>,
    classic: bool,
    offset: usize,
    extras: Extras,
) -> Geometry {
    if page == Page::Main {
        return main_geometry(rows, size, classic, extras);
    }
    let width = WIDTH.min((size.w - 16).max(1));
    let inner = width - 2 * PAD;
    let list = entries(rows);
    let heights: Vec<i32> = list.iter().map(|e| entry_height(rows, e)).collect();
    let top = HEADER_H + 4;
    let chrome = top + BOTTOM + extras.height();
    let natural = chrome + heights.iter().sum::<i32>().max(ROW_H);
    let height = natural.min(max_height(size, classic)).max(chrome + ROW_H);
    let viewport = height - chrome;
    let panel = place(size, classic, height);
    let mut g = Geometry { panel, ..Default::default() };
    g.back = Some(rect(8, 12, 32, 32));
    g.title = rect(48, 16, width - 48 - 76, 24);
    if let Some(i) = rows.iter().position(|r| r.kind == RowKind::Switch) {
        g.switch = Some((i, rect(width - PAD - 44, 16, 44, 24)));
    }
    let first = offset.min(list.len().saturating_sub(1));
    let mut y = top;
    let mut shown = 0;
    for (entry, h) in list.iter().zip(&heights).skip(first) {
        if y + h > top + viewport {
            break;
        }
        match entry {
            Entry::Header(name) => g.sections.push((name, rect(PAD, y, inner, *h))),
            Entry::Row(i) => {
                let area = rect(PAD, y, inner, *h);
                let mut cell = Cell { row: *i, area, ..Default::default() };
                match rows[*i].kind {
                    RowKind::Slider => {
                        cell.track = rect(PAD, y + 20, inner, SLIDER_H);
                        cell.icon = rect(PAD, y + 20, 32, SLIDER_H);
                    }
                    RowKind::AppSlider => {
                        cell.icon = rect(PAD + 4, y + 12, 28, 28);
                        cell.track = rect(PAD + 44, y + 26, inner - 44 - 44, 14);
                    }
                    RowKind::Network if rows[*i].on => {
                        cell.aux = rect(PAD + inner - 88, y + 9, 80, 26);
                    }
                    _ => {}
                }
                g.cells.push(cell);
            }
        }
        y += h;
        shown += 1;
    }
    let mut bottom = height - BOTTOM;
    if extras.hints {
        bottom -= HINTS_H;
        g.hints = rect(PAD, bottom, inner, HINTS_H);
    }
    if extras.password {
        bottom -= PASSWORD_H;
        g.password = rect(PAD, bottom, inner, PASSWORD_H);
    }
    if extras.error {
        bottom -= ERROR_H;
        g.error = rect(PAD, bottom, inner, ERROR_H);
    }
    if first > 0 || first + shown < list.len() {
        let total = list.len().max(1) as i32;
        let thumb_h = (viewport * shown.max(1) as i32 / total).clamp(16, viewport);
        let travel = (list.len() - shown).max(1) as i32;
        let thumb_y = top + (viewport - thumb_h) * first as i32 / travel;
        g.scrollbar = Some((rect(width - 8, top, 3, viewport), rect(width - 8, thumb_y, 3, thumb_h)));
    }
    shift(&mut g, panel.loc.x, panel.loc.y);
    g
}
fn main_geometry(rows: &[Row], size: Size<i32, Logical>, classic: bool, extras: Extras) -> Geometry {
    let width = WIDTH.min((size.w - 16).max(1));
    let inner = width - 2 * PAD;
    let mut g = Geometry::default();
    let mut y = PAD;
    if let Some(media_row) = rows.iter().position(|r| r.kind == RowKind::Media) {
        let info = rect(PAD, y, inner, 52);
        let button = |i: i32| rect(PAD + (inner - 3 * 40 - 2 * 8) / 2 + i * 48, y + 60, 40, BUTTON);
        g.media = Some([info, button(0), button(1), button(2)]);
        g.cells.push(Cell { row: media_row, area: info, ..Default::default() });
        y += 60 + BUTTON + 12;
    }
    let tiles: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == RowKind::Tile)
        .map(|(i, _)| i)
        .collect();
    for (line, chunk) in tiles.chunks(3).enumerate() {
        let count = chunk.len() as i32;
        let w = (inner - 8 * (count - 1)) / count;
        for (k, row) in chunk.iter().enumerate() {
            let area = rect(PAD + k as i32 * (w + 8), y + line as i32 * (TILE_H + 8), w, TILE_H);
            g.cells.push(Cell {
                row: *row,
                area,
                icon: rect(area.loc.x + 8, area.loc.y + 8, 28, 28),
                chevron: if rows[*row].chevron.is_some() {
                    rect(area.loc.x + w - 36, area.loc.y + 4, 36, 36)
                } else {
                    Rect::default()
                },
                ..Default::default()
            });
        }
    }
    let lines = tiles.len().div_ceil(3) as i32;
    y += lines * TILE_H + (lines - 1).max(0) * 8 + 12;
    for (i, row) in rows.iter().enumerate().filter(|(_, r)| r.kind == RowKind::Slider) {
        let area = rect(PAD, y, inner, SLIDER_H);
        g.cells.push(Cell {
            row: i,
            area,
            icon: rect(PAD, y, 32, SLIDER_H),
            track: area,
            chevron: if row.chevron.is_some() {
                rect(PAD + inner - 32, y, 32, SLIDER_H)
            } else {
                Rect::default()
            },
            ..Default::default()
        });
        y += SLIDER_H + 12;
    }
    y += 4;
    let buttons: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == RowKind::Button)
        .map(|(i, _)| i)
        .collect();
    let n = buttons.len() as i32;
    for (k, row) in buttons.iter().enumerate() {
        let from_right = n - k as i32;
        let x = PAD + inner - from_right * BUTTON - (from_right - 1) * 8;
        let area = rect(x, y, BUTTON, BUTTON);
        g.cells.push(Cell { row: *row, area, icon: area, ..Default::default() });
    }
    g.footer_text = rect(PAD, y, (inner - n * (BUTTON + 8)).max(0), BUTTON);
    let mut height = y + BUTTON + PAD;
    if extras.hints {
        g.hints = rect(PAD, height - PAD + 4, inner, HINTS_H);
        height += HINTS_H;
    }
    g.panel = place(size, classic, height);
    let panel = g.panel;
    shift(&mut g, panel.loc.x, panel.loc.y);
    g
}
fn shift(g: &mut Geometry, dx: i32, dy: i32) {
    let s = |r: Rect| if r.size.w == 0 && r.size.h == 0 { r } else { translate(r, dx, dy) };
    g.back = g.back.map(s);
    g.title = s(g.title);
    g.switch = g.switch.map(|(i, r)| (i, s(r)));
    for cell in &mut g.cells {
        cell.area = s(cell.area);
        cell.icon = s(cell.icon);
        cell.track = s(cell.track);
        cell.chevron = s(cell.chevron);
        cell.aux = s(cell.aux);
    }
    for (_, r) in &mut g.sections {
        *r = s(*r);
    }
    g.media = g.media.map(|m| m.map(s));
    g.footer_text = s(g.footer_text);
    g.error = s(g.error);
    g.password = s(g.password);
    g.hints = s(g.hints);
    g.scrollbar = g.scrollbar.map(|(a, b)| (s(a), s(b)));
}
/// Pointer targets, from the bottom layer up.
pub(super) fn hits(page: Page, rows: &[Row], g: &Geometry) -> Vec<(Rect, Action, bool)> {
    let mut out = Vec::new();
    if let Some(back) = g.back {
        out.push((back, Action::Back, false));
    }
    if let Some((i, area)) = g.switch {
        let hit = rect(area.loc.x - 8, area.loc.y - 8, area.size.w + 16, area.size.h + 16);
        out.push((hit, rows[i].action.clone(), false));
    }
    if let Some([info, previous, play, next]) = g.media {
        out.push((info, Action::MediaInfo, false));
        for (area, command) in [
            (previous, crate::media::Command::Previous),
            (play, crate::media::Command::PlayPause),
            (next, crate::media::Command::Next),
        ] {
            out.push((area, Action::Media(command), false));
        }
    }
    for cell in &g.cells {
        let row = &rows[cell.row];
        match row.kind {
            RowKind::Media => {}
            RowKind::Slider | RowKind::AppSlider => {
                out.push((cell.track, row.action.clone(), true));
                if let Action::Slider(target) = &row.action {
                    out.push((cell.icon, Action::Mute(target.clone()), false));
                }
                if let Some(open) = row.chevron.filter(|_| page == Page::Main) {
                    out.push((cell.chevron, Action::Open(open), false));
                }
            }
            RowKind::Tile => {
                out.push((cell.area, row.action.clone(), false));
                if let Some(open) = row.chevron {
                    out.push((cell.chevron, Action::Open(open), false));
                }
            }
            _ => {
                out.push((cell.area, row.action.clone(), false));
                if cell.aux.size.w > 0 {
                    out.push((cell.aux, Action::Disconnect, false));
                }
            }
        }
    }
    out
}
pub(super) use look::on_accent;
/// What a page needs from the state to be painted, copied out so the
/// canvas and the icon cache can be borrowed mutably.
#[derive(Clone, Copy)]
struct Paint<'a> {
    accent: [f32; 4],
    theme: &'a str,
    selected: usize,
    focus: bool,
    password: Option<usize>,
    error: Option<&'a str>,
    footer: &'a str,
    profile: Option<&'a str>,
    media: &'a crate::media::Snapshot,
}
impl State {
    #[allow(clippy::too_many_arguments)]
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        accent: [f32; 4],
        theme: &str,
        classic: bool,
        info: &Info,
        media: &crate::media::Snapshot,
    ) -> Vec<GavnoRenderElement> {
        if self.open.as_ref() != Some(output) {
            return vec![];
        }
        let now = Instant::now();
        let progress = |t: Instant, time: std::time::Duration| {
            ease_out(now.saturating_duration_since(t).as_secs_f64() / time.as_secs_f64())
        };
        let open = self.anim.opened.map(|t| progress(t, super::OPEN_TIME));
        let slide = self
            .anim
            .slide
            .map(|(from, t)| (from, progress(t, super::SLIDE_TIME)))
            .filter(|(_, t)| *t < 1.0);
        let page = self.page;
        let rows = self.rows(page, info);
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        let extras = Extras {
            error: self.error_for(page).is_some(),
            password: page == Page::Wifi && self.password.is_some(),
            hints: self.keyboard || self.code_mode,
        };
        if page != Page::Main {
            self.offset = scroll_to(&rows, self.selected, self.offset, viewport(size, classic, extras));
        }
        let geo = geometry(page, &rows, size, classic, self.offset, extras);
        let from = slide.map(|(from, t)| {
            let rows = self.rows(from, info);
            let extras = Extras { hints: extras.hints, ..Default::default() };
            let geo = geometry(from, &rows, size, classic, 0, extras);
            (from, rows, geo, t)
        });
        // Pointer targets of a page that is still sliding in would be a moving target.
        self.view.hits = if from.is_none() { hits(page, &rows, &geo) } else { Vec::new() };
        self.view.panel = geo.panel;
        // Between two pages the card morphs from one size to the other.
        let panel = match &from {
            Some((_, _, from_geo, t)) => {
                let lerp = |a: i32, b: i32| (a as f64 + (b - a) as f64 * t).round() as i32;
                rect(
                    lerp(from_geo.panel.loc.x, geo.panel.loc.x),
                    lerp(from_geo.panel.loc.y, geo.panel.loc.y),
                    lerp(from_geo.panel.size.w, geo.panel.size.w),
                    lerp(from_geo.panel.size.h, geo.panel.size.h),
                )
            }
            None => geo.panel,
        };
        let footer = info
            .battery
            .as_ref()
            .map(|b| b.summary())
            .unwrap_or_else(|| "On AC power".into());
        let masked = self.password.as_ref().map(|(_, p)| "•".repeat(p.0.chars().count().min(48)));
        let error = self.error_for(page).map(str::to_owned);
        let from_error = from.as_ref().and_then(|(p, ..)| self.error_for(*p)).map(str::to_owned);
        let paint = Paint {
            accent,
            theme,
            selected: self.selected,
            focus: self.keyboard || self.hover,
            password: self.password.as_ref().map(|(_, p)| p.0.chars().count()),
            error: error.as_deref(),
            footer: &footer,
            profile: info.profile.as_deref(),
            media,
        };
        let c = &mut self.view.canvas;
        c.begin(renderer, scale);
        let opening = open.unwrap_or(1.0);
        c.zoom = look::OPEN_ZOOM + (1.0 - look::OPEN_ZOOM) * opening;
        // The panel grows from the status cluster at its top right corner.
        c.anchor = (
            (panel.loc.x + panel.size.w - 24) as f64,
            if classic { (panel.loc.y + panel.size.h) as f64 } else { panel.loc.y as f64 },
        )
            .into();
        c.alpha = opening as f32;
        look::card(c, panel, radius::PANEL);
        let base = c.alpha;
        match &from {
            Some((from_page, from_rows, from_geo, t)) => {
                // Opening a page moves the old one left and the new one in from the right.
                let direction = if page == Page::Main { -1.0 } else { 1.0 };
                c.alpha = base * (1.0 - *t) as f32;
                c.offset = (-direction * 28.0 * t, 0.0).into();
                let old = Paint { error: from_error.as_deref(), selected: usize::MAX, password: None, ..paint };
                draw_page(c, &mut self.view.icons, renderer, &old, *from_page, from_rows, from_geo, None);
                c.alpha = base * *t as f32;
                c.offset = (direction * 28.0 * (1.0 - t), 0.0).into();
                draw_page(c, &mut self.view.icons, renderer, &paint, page, &rows, &geo, masked.as_deref());
            }
            None => draw_page(c, &mut self.view.icons, renderer, &paint, page, &rows, &geo, masked.as_deref()),
        }
        let elements = c.finish();
        // A frame at full progress was just drawn; the animation can rest.
        if self.anim.opened.is_some_and(|t| now.saturating_duration_since(t) >= super::OPEN_TIME) {
            self.anim.opened = None;
        }
        if self.anim.slide.is_some_and(|(_, t)| now.saturating_duration_since(t) >= super::SLIDE_TIME) {
            self.anim.slide = None;
        }
        elements
    }
    fn error_for(&self, page: Page) -> Option<&str> {
        page.kind().and_then(|k| self.errors[super::index(k)].as_deref())
    }
}
#[allow(clippy::too_many_arguments)]
fn draw_page(
    c: &mut Canvas,
    icons: &mut Icons,
    r: &mut GlesRenderer,
    p: &Paint,
    page: Page,
    rows: &[Row],
    g: &Geometry,
    masked: Option<&str>,
) {
    if page == Page::Main {
        draw_main(c, icons, r, p, rows, g);
    } else {
        draw_detail(c, icons, r, p, page, rows, g, masked);
    }
    if g.hints.size.h > 0 {
        draw_hints(c, r, g.hints);
    }
}
fn ring(p: &Paint, row: usize) -> f32 {
    if p.focus && p.selected == row {
        1.5
    } else {
        0.0
    }
}
/// A 16 px symbolic icon centred in `area`.
fn glyph(c: &mut Canvas, icons: &mut Icons, r: &mut GlesRenderer, p: &Paint, names: &[&str], area: Rect, color: [f32; 4]) {
    let side = 16.min(area.size.w).min(area.size.h);
    let target = rect(
        area.loc.x + (area.size.w - side) / 2,
        area.loc.y + (area.size.h - side) / 2,
        side,
        side,
    );
    let names: Vec<&str> = names.iter().copied().filter(|n| !n.is_empty()).collect();
    let rgb = [0, 1, 2].map(|i| (color[i] * 255.0).round() as u8);
    if rgb == [255, 255, 255] || rgb == FULL_COLOR {
        c.icon(r, icons, &names, p.theme, target);
    } else {
        c.colored_icon(r, icons, &names, p.theme, target, Some(rgb));
    }
}
fn draw_main(c: &mut Canvas, icons: &mut Icons, r: &mut GlesRenderer, p: &Paint, rows: &[Row], g: &Geometry) {
    let on_color = on_accent(p.accent);
    for cell in &g.cells {
        let row = &rows[cell.row];
        let selected = ring(p, cell.row);
        match row.kind {
            RowKind::Media => {
                if let Some(player) = &p.media.player {
                    crate::media::ui::info(c, r, p.media, cell.area);
                    let [_, previous, play, next] = g.media.unwrap_or_default();
                    for (area, command) in [
                        (previous, crate::media::Command::Previous),
                        (play, crate::media::Command::PlayPause),
                        (next, crate::media::Command::Next),
                    ] {
                        crate::media::ui::button(c, r, icons, p.theme, player, command, area, false);
                    }
                }
            }
            RowKind::Tile => {
                let fg = if row.on { on_color } else { TEXT };
                c.rect(
                    cell.area,
                    radius::TILE,
                    if row.on { [p.accent[0], p.accent[1], p.accent[2], 0.94] } else { SURFACE },
                    selected,
                    if row.on { [1.0, 1.0, 1.0, 0.9] } else { dim(p.accent, 0.95) },
                );
                c.rect(
                    cell.icon,
                    radius::DISC,
                    if row.on { [0.0, 0.0, 0.0, 0.14] } else { look::SURFACE_RAISED },
                    0.0,
                    LINE,
                );
                glyph(c, icons, r, p, &[&row.icon], cell.icon, fg);
                if cell.chevron.size.w > 0 {
                    let area = rect(cell.chevron.loc.x + 8, cell.chevron.loc.y, 24, 36);
                    glyph(c, icons, r, p, &["go-next-symbolic"], area, dim(fg, 0.85));
                }
                let w = cell.area.size.w - 20;
                let (x, y) = (cell.area.loc.x + 10, cell.area.loc.y);
                c.styled_label(r, &row.label, x, y + 39, w, style(12.5, Weight::SemiBold, fg));
                c.styled_label(r, &row.detail, x, y + 55, w, style(10.5, Weight::Medium, dim(fg, 0.72)));
            }
            RowKind::Slider => draw_slider(c, icons, r, p, row, cell, selected, true),
            RowKind::Button => {
                c.rect(cell.area, radius::TILE, SURFACE, selected, look::focus_line(p.accent));
                glyph(c, icons, r, p, &[&row.icon], cell.area, TEXT);
            }
            _ => {}
        }
    }
    let t = g.footer_text;
    c.styled_label(r, p.footer, t.loc.x, t.loc.y + 1, t.size.w, style(12.0, Weight::SemiBold, TEXT));
    let profile = match p.profile {
        Some("power-saver") => Some("Power saver"),
        Some("balanced") => Some("Balanced"),
        Some("performance") => Some("Performance"),
        _ => None,
    };
    if let Some(profile) = profile {
        c.styled_label(r, profile, t.loc.x, t.loc.y + 18, t.size.w, style(10.5, Weight::Medium, dim(TEXT, 0.6)));
    }
}
#[allow(clippy::too_many_arguments)]
fn draw_slider(
    c: &mut Canvas,
    icons: &mut Icons,
    r: &mut GlesRenderer,
    p: &Paint,
    row: &Row,
    cell: &Cell,
    ring: f32,
    chevron: bool,
) {
    let track = cell.track;
    let level = row.level.unwrap_or(0).min(100) as i32;
    let fill_w = if level == 0 { 0 } else { (track.size.w * level / 100).max(track.size.h) };
    let fill = if row.muted { [1.0, 1.0, 1.0, 0.30] } else { p.accent };
    c.rect(track, radius::DISC, look::TRACK, ring, look::focus_line(p.accent));
    if fill_w > 0 {
        c.rect(rect(track.loc.x, track.loc.y, fill_w, track.size.h), radius::DISC, fill, 0.0, LINE);
    }
    // Icons that sit on the accent fill switch to the dark colour.
    let covered = |x: i32| !row.muted && fill_w >= x - track.loc.x;
    let dark = on_accent(p.accent);
    let icon = rect(track.loc.x + 6, track.loc.y + 6, 16, 16);
    let color = if covered(icon.loc.x + icon.size.w) { dark } else { TEXT };
    glyph(c, icons, r, p, &[&row.icon], icon, color);
    if chevron && cell.chevron.size.w > 0 {
        let color = if covered(cell.chevron.loc.x + 24) { dark } else { TEXT };
        glyph(c, icons, r, p, &["go-next-symbolic"], cell.chevron, color);
    }
}
#[allow(clippy::too_many_arguments)]
fn draw_detail(
    c: &mut Canvas,
    icons: &mut Icons,
    r: &mut GlesRenderer,
    p: &Paint,
    page: Page,
    rows: &[Row],
    g: &Geometry,
    masked: Option<&str>,
) {
    if let Some(back) = g.back {
        c.rect(back, radius::TILE, SURFACE, 0.0, LINE);
        glyph(c, icons, r, p, &["go-previous-symbolic"], back, TEXT);
    }
    c.styled_label(r, page.title(), g.title.loc.x, g.title.loc.y, g.title.size.w, style(18.0, Weight::SemiBold, TEXT));
    if let Some((i, area)) = g.switch {
        let on = rows[i].on;
        c.rect(area, radius::ROW, if on { p.accent } else { [1.0, 1.0, 1.0, 0.18] }, ring(p, i), [1.0, 1.0, 1.0, 0.9]);
        let knob = rect(area.loc.x + if on { area.size.w - 22 } else { 2 }, area.loc.y + 2, 20, 20);
        c.rect(knob, 10.0, if on { on_accent(p.accent) } else { [1.0; 4] }, 0.0, LINE);
    }
    for (name, area) in &g.sections {
        c.styled_label(r, name, area.loc.x + 4, area.loc.y + 10, area.size.w - 8, style(10.5, Weight::SemiBold, dim(TEXT, 0.55)));
    }
    for cell in &g.cells {
        let row = &rows[cell.row];
        let selected = ring(p, cell.row);
        let a = cell.area;
        match row.kind {
            RowKind::Slider => {
                c.styled_label(r, &row.label, a.loc.x + 4, a.loc.y, a.size.w - 8, style(11.5, Weight::Medium, dim(TEXT, 0.75)));
                draw_slider(c, icons, r, p, row, cell, selected, false);
            }
            RowKind::AppSlider => draw_app(c, icons, r, p, row, cell, selected),
            _ => draw_entry(c, icons, r, p, row, cell, selected),
        }
    }
    if g.cells.is_empty() {
        let text = match page {
            Page::Wifi => "No networks found",
            Page::Bluetooth => "No devices",
            Page::Audio => "No audio devices or streams",
            _ => "",
        };
        let y = g.panel.loc.y + HEADER_H + 16;
        c.styled_label(r, text, g.panel.loc.x + PAD + 4, y, g.panel.size.w - 2 * PAD, style(12.5, Weight::Medium, dim(TEXT, 0.6)));
    }
    if let Some((track, thumb)) = g.scrollbar {
        c.rect(track, 1.5, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);
        c.rect(thumb, 1.5, [1.0, 1.0, 1.0, 0.35], 0.0, LINE);
    }
    if let Some(error) = p.error.filter(|_| g.error.size.h > 0) {
        c.styled_label(r, error, g.error.loc.x, g.error.loc.y + 4, g.error.size.w, style(11.0, Weight::Medium, look::DANGER));
    }
    if let (Some(count), true) = (p.password, g.password.size.h > 0) {
        let field = rect(g.password.loc.x, g.password.loc.y + 4, g.password.size.w, 36);
        c.rect(field, radius::FIELD, look::wash(p.accent, 0.12), 1.0, p.accent);
        let (text, color) = match masked {
            Some(m) if count > 0 => (m, TEXT),
            _ => ("Password", dim(TEXT, 0.5)),
        };
        c.styled_label(r, text, field.loc.x + 12, field.loc.y + 9, field.size.w - 24, style(13.0, Weight::Medium, color));
        c.styled_label(r, "Enter connects · Esc cancels", field.loc.x + 2, field.loc.y + 44, field.size.w, style(10.5, Weight::Medium, dim(TEXT, 0.6)));
    }
}
fn draw_app(c: &mut Canvas, icons: &mut Icons, r: &mut GlesRenderer, p: &Paint, row: &Row, cell: &Cell, ring: f32) {
    let a = cell.area;
    if ring > 0.0 {
        c.rect(a, radius::FIELD, [1.0, 1.0, 1.0, 0.06], ring, look::focus_line(p.accent));
    }
    let alpha = c.alpha;
    if row.muted {
        c.alpha *= 0.45;
    }
    // The real application icon; the generic one stands in until it is loaded.
    if !c.colored_icon(r, icons, &[&row.icon], p.theme, cell.icon, Some(FULL_COLOR)) {
        glyph(c, icons, r, p, &["application-x-executable-symbolic"], cell.icon, TEXT);
    }
    c.alpha = alpha;
    c.styled_label(r, &row.label, a.loc.x + 44, a.loc.y + 4, a.size.w - 88, style(11.5, Weight::Medium, dim(TEXT, 0.85)));
    let level = row.level.unwrap_or(0).min(100) as i32;
    let t = cell.track;
    c.rect(t, 7.0, look::TRACK, 0.0, LINE);
    if level > 0 {
        let fill = if row.muted { [1.0, 1.0, 1.0, 0.3] } else { p.accent };
        c.rect(rect(t.loc.x, t.loc.y, (t.size.w * level / 100).max(t.size.h), t.size.h), 7.0, fill, 0.0, LINE);
    }
    let value = if row.muted { "Muted".to_string() } else { format!("{level}%") };
    let st = style(10.5, Weight::Medium, dim(TEXT, 0.65));
    let w = look::tabular_width(c, &value, st);
    look::tabular_label(c, r, &value, a.loc.x + a.size.w - w, t.loc.y - 1, st);
}
fn draw_entry(c: &mut Canvas, icons: &mut Icons, r: &mut GlesRenderer, p: &Paint, row: &Row, cell: &Cell, ring: f32) {
    // Rows are 4 px taller than their card, which leaves the gap between two.
    let a = rect(cell.area.loc.x, cell.area.loc.y + 2, cell.area.size.w, cell.area.size.h - 4);
    let highlight = row.on;
    c.rect(
        a,
        radius::ROW,
        if highlight { look::wash(p.accent, 0.20) } else { SURFACE },
        if ring > 0.0 { ring } else if highlight { 1.0 } else { 0.0 },
        dim(p.accent, if ring > 0.0 { 0.95 } else { 0.8 }),
    );
    let mid = |h: i32| a.loc.y + (a.size.h - h) / 2;
    let icon = rect(a.loc.x + 10, mid(20), 20, 20);
    if row.kind == RowKind::Network {
        signal_bars(c, icon, row.strength.unwrap_or(0), if row.on { p.accent } else { TEXT });
    } else {
        glyph(c, icons, r, p, &[&row.icon, "application-x-executable-symbolic"], icon, TEXT);
    }
    let mut right = a.loc.x + a.size.w - 10;
    if cell.aux.size.w > 0 {
        let label = style(10.5, Weight::SemiBold, TEXT);
        c.rect(cell.aux, 13.0, [1.0, 1.0, 1.0, 0.14], 0.0, LINE);
        let w = c.text.measure("Disconnect", label).ceil() as i32;
        c.styled_label(r, "Disconnect", cell.aux.loc.x + (cell.aux.size.w - w) / 2, cell.aux.loc.y + 6, cell.aux.size.w, label);
        right = cell.aux.loc.x - 8;
    }
    if row.on && matches!(row.kind, RowKind::Network | RowKind::Entry) {
        right -= 20;
        glyph(c, icons, r, p, &["object-select-symbolic"], rect(right, mid(20), 20, 20), p.accent);
        right -= 4;
    } else if row.secured {
        right -= 20;
        glyph(c, icons, r, p, &["changes-prevent-symbolic"], rect(right, mid(20), 20, 20), dim(TEXT, 0.7));
        right -= 4;
    }
    if let Some(percent) = row.battery {
        let label = style(10.5, Weight::Medium, dim(TEXT, 0.75));
        let text = format!("{percent}%");
        let w = look::tabular_width(c, &text, label) + 2;
        right -= w;
        look::tabular_label(c, r, &text, right, mid(14), label);
        right -= 22;
        crate::topbar::battery_glyph(c, rect(right, mid(10), 20, 10), percent, false, dim(TEXT, 0.75));
        right -= 6;
    }
    let text_x = a.loc.x + 40;
    let width = (right - text_x).max(0);
    if row.detail.is_empty() {
        c.styled_label(r, &row.label, text_x, mid(17), width, style(13.0, Weight::Medium, TEXT));
    } else {
        c.styled_label(r, &row.label, text_x, a.loc.y + 6, width, style(13.0, Weight::Medium, TEXT));
        c.styled_label(r, &row.detail, text_x, a.loc.y + 24, width, style(10.5, Weight::Medium, dim(TEXT, 0.55)));
    }
}
/// 1 to 4 bars, the lit ones in `color`.
fn signal_bars(c: &mut Canvas, area: Rect, strength: u8, color: [f32; 4]) {
    let lit = match strength {
        75.. => 4,
        50..=74 => 3,
        25..=49 => 2,
        _ => 1,
    };
    for i in 0..4 {
        let height = 5 + i * 3;
        c.rect(
            rect(area.loc.x + 1 + i * 5, area.loc.y + area.size.h - 2 - height, 3, height),
            1.0,
            if i < lit { color } else { dim(color, 0.28) },
            0.0,
            LINE,
        );
    }
}
/// The key hints come from the single bind list in `hints.rs`.
fn draw_hints(c: &mut Canvas, r: &mut GlesRenderer, area: Rect) {
    // `measure` follows the last rendered scale; prime it once per frame.
    let _ = c.text.render(" ", style(1.0, Weight::Regular, TEXT), c.scale);
    let label = style(10.5, Weight::Regular, dim(TEXT, 0.62));
    let key = style(10.5, Weight::SemiBold, dim(TEXT, 0.95));
    let (mut x, mut y) = (area.loc.x, area.loc.y + 2);
    for hint in crate::hints::items(crate::hints::HintContext::Popover) {
        let keys = c.text.measure(hint.keys, key).ceil() as i32 + 10;
        let name = c.text.measure(hint.label, label).ceil() as i32;
        let width = keys + 6 + name;
        if x + width > area.loc.x + area.size.w && x > area.loc.x {
            x = area.loc.x;
            y += 24;
        }
        c.rect(rect(x, y + 1, keys, 18), 5.0, [1.0, 1.0, 1.0, 0.10], 0.0, LINE);
        c.styled_label(r, hint.keys, x + 5, y + 4, keys - 10, key);
        c.styled_label(r, hint.label, x + keys + 6, y + 4, name + 2, label);
        x += width + 12;
    }
}
