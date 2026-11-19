//! Glass music browser. Playback ticks reuse every static render element.
use super::{worker::CoverKind, Covers, Hit, Hits, Item, Session};
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        Canvas, GLASS, LINE,
    },
    ui::text::{TextStyleSpec, Weight},
};
use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBufferRenderElement, Kind},
        gles::GlesRenderer,
    },
    output::Output,
    utils::{Logical, Point, Rectangle, Size},
};

#[derive(Default)]
pub struct View {
    canvas: Canvas,
    icons: Icons,
    pub covers: Covers,
    pub hits: Hits,
    pub visible_rows: usize,
    cached: Vec<crate::classic::cache::CachedElement>,
    geometry: Option<(Size<i32, Logical>, f64, [f32; 4])>,
    tick: Canvas,
    tick_cached: Vec<crate::classic::cache::CachedElement>,
    tick_key: Option<(i32, u32, u32, bool)>,
    #[cfg(test)]
    pub static_builds: usize,
    #[cfg(test)]
    pub tick_builds: usize,
    #[cfg(test)]
    pub labels: Vec<(String, Rect, TextStyleSpec)>,
    #[cfg(test)]
    pub remaining_right: i32,
}
impl View {
    pub fn hit(&self, point: Point<f64, Logical>) -> Option<(Rect, Hit)> {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| r.to_f64().contains(point))
            .copied()
    }
}
pub const PANEL_HEIGHT: i32 = 760;
/// Every rectangle of the now-playing block, shared by the static painter,
/// the progress painter and the hit targets.
pub(super) struct Layout {
    pub cover: Rect,
    pub title: Rect,
    pub artist: Rect,
    pub device: Rect,
    pub track: Rect,
    pub seek_hit: Rect,
    pub times_y: i32,
    pub buttons: Vec<(Hit, Rect)>,
    pub volume: Rect,
    pub search: Rect,
    pub tabs: Rect,
    pub status_y: i32,
    pub list_top: i32,
}
impl Layout {
    pub fn new(p: Rect, options: bool) -> Self {
        let w = p.size.w;
        // The cover shrinks on short outputs; everything below follows it.
        let side = (88 + (p.size.h - 300) * 96 / 340).clamp(88, 184).min(w - 48);
        let cover = rect(p.loc.x + (w - side) / 2, p.loc.y + 18, side, side);
        let title = rect(p.loc.x + 24, cover.loc.y + side + 14, w - 48, 24);
        let artist = rect(title.loc.x, title.loc.y + 25, title.size.w, 20);
        let device = rect(title.loc.x, artist.loc.y + 22, title.size.w, 16);
        let track = rect(p.loc.x + 24, device.loc.y + device.size.h + 16, w - 48, 4);
        let seek_hit = rect(track.loc.x, track.loc.y - 8, track.size.w, 20);
        let times_y = track.loc.y + 10;
        let row = times_y + 16 + 10;
        let cx = p.loc.x + w / 2;
        let mid = row + 26;
        let spot = |dx: i32, d: i32| rect(cx + dx - d / 2, mid - d / 2, d, d);
        let mut buttons = Vec::new();
        if options {
            buttons.push((Hit::Shuffle, spot(-132, 36)));
            buttons.push((Hit::Control("previous"), spot(-72, 40)));
            buttons.push((Hit::Control("toggle"), spot(0, 52)));
            buttons.push((Hit::Control("next"), spot(72, 40)));
            buttons.push((Hit::Repeat, spot(132, 36)));
        } else {
            buttons.push((Hit::Control("previous"), spot(-66, 40)));
            buttons.push((Hit::Control("toggle"), spot(0, 52)));
            buttons.push((Hit::Control("next"), spot(66, 40)));
        }
        let volume = rect(p.loc.x + 52, row + 52 + 12, w - 76, 24);
        let search = rect(p.loc.x + 12, volume.loc.y + 24 + 14, w - 24, 36);
        let tabs = rect(p.loc.x + 12, search.loc.y + 36 + 8, w - 24, 32);
        Self {
            cover,
            title,
            artist,
            device,
            track,
            seek_hit,
            times_y,
            buttons,
            volume,
            search,
            tabs,
            status_y: tabs.loc.y + 32 + 6,
            list_top: tabs.loc.y + 32 + 8,
        }
    }
}
pub fn panel_rect(size: Size<i32, Logical>, anchor: Option<Rect>) -> Rect {
    if let Some(anchor) = anchor {
        let available_h = (size.h - crate::classic::TASKBAR_HEIGHT).max(1);
        let width = 420.min(size.w.max(1));
        let height = PANEL_HEIGHT.min(available_h);
        let left = anchor.loc.x - width - 12;
        let x = if left >= 0 {
            left
        } else {
            anchor.loc.x + anchor.size.w + 12
        };
        return rect(
            x.clamp(0, (size.w - width).max(0)),
            anchor.loc.y.clamp(0, (available_h - height).max(0)),
            width,
            height,
        );
    }
    let width = 420.min((size.w - 16).max(1));
    let height = PANEL_HEIGHT.min((size.h - 48).max(1));
    let x = (size.w / 2 + 64).min(size.w - width - 8).max(0);
    rect(x, 40.min((size.h - height).max(0)), width, height)
}
fn white(alpha: f32) -> [f32; 4] {
    [1.0, 1.0, 1.0, alpha]
}
fn style(size: f32, weight: Weight, color: [f32; 4]) -> TextStyleSpec {
    TextStyleSpec {
        size,
        weight,
        color,
        tracking: 0.0,
    }
}
fn label(view: &mut View, renderer: &mut GlesRenderer, value: &str, area: Rect, st: TextStyleSpec) {
    #[cfg(test)]
    view.labels.push((value.to_owned(), area, st));
    view.canvas
        .styled_label(renderer, value, area.loc.x, area.loc.y, area.size.w, st);
}
fn centered(
    view: &mut View,
    renderer: &mut GlesRenderer,
    value: &str,
    area: Rect,
    st: TextStyleSpec,
) {
    let w = (view.canvas.text.measure(value, st).ceil() as i32).min(area.size.w);
    label(
        view,
        renderer,
        value,
        rect(
            area.loc.x + (area.size.w - w) / 2,
            area.loc.y,
            w,
            area.size.h,
        ),
        st,
    );
}
fn icon(view: &mut View, renderer: &mut GlesRenderer, name: &str, area: Rect, color: [f32; 4]) {
    let alpha = view.canvas.alpha;
    view.canvas.alpha *= color[3];
    view.canvas.colored_icon(
        renderer,
        &mut view.icons,
        &[name],
        "Adwaita",
        area,
        Some([color[0], color[1], color[2]].map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)),
    );
    view.canvas.alpha = alpha;
}
fn image(
    view: &mut View,
    renderer: &mut GlesRenderer,
    item: Option<&Item>,
    area: Rect,
    kind: CoverKind,
) {
    let cover = item
        .and_then(|i| i.cover_path.as_ref())
        .and_then(|p| view.covers.variant(p, kind));
    if let Some(cover) = cover {
        let c = &mut view.canvas;
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (
                (area.loc.x as f64 * c.scale).round(),
                (area.loc.y as f64 * c.scale).round(),
            ),
            &cover.buffer,
            Some(1.0),
            Some(Rectangle::from_size(
                (kind.size() as f64, kind.size() as f64).into(),
            )),
            Some(area.size),
            Kind::Unspecified,
        ) {
            c.elements.push(GavnoRenderElement::Memory(element));
        }
    } else {
        let radius = match kind {
            CoverKind::Hero => 10.0,
            CoverKind::Artist => area.size.w as f32 / 2.0,
            CoverKind::List => 7.0,
        };
        view.canvas.rect(area, radius, white(0.07), 0.0, LINE);
        let size = if kind == CoverKind::Hero { 40 } else { 18 };
        icon(
            view,
            renderer,
            "audio-x-generic-symbolic",
            rect(
                area.loc.x + (area.size.w - size) / 2,
                area.loc.y + (area.size.h - size) / 2,
                size,
                size,
            ),
            white(0.35),
        );
    }
}
pub(super) fn time(ms: u32) -> String {
    format!("{}:{:02}", ms / 60000, ms / 1000 % 60)
}
pub(super) fn page_title(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}
impl Session {
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
        accent: [f32; 4],
    ) -> Vec<GavnoRenderElement> {
        if &self.output != output {
            return Vec::new();
        }
        if self.dirty || self.view.geometry != Some((size, scale, accent)) {
            self.build_static(renderer, size, scale, accent);
        }
        self.build_progress(renderer, size, scale, accent);
        self.request_covers();
        self.view
            .tick_cached
            .iter()
            .chain(&self.view.cached)
            .cloned()
            .map(GavnoRenderElement::Desktop)
            .collect()
    }
    fn build_static(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Logical>,
        scale: f64,
        accent: [f32; 4],
    ) {
        let view = &mut self.view;
        #[cfg(test)]
        {
            view.static_builds += 1;
            view.labels.clear();
        }
        self.dirty = false;
        view.tick_key = None;
        view.geometry = Some((size, scale, accent));
        view.canvas.begin(renderer, scale);
        view.hits.clear();
        view.visible_rows = 0;
        let p = panel_rect(size, self.anchor);
        let (x, y, w) = (p.loc.x, p.loc.y, p.size.w);
        let right = x + w;
        let accent_text = [
            accent[0] * 0.65 + 0.35,
            accent[1] * 0.65 + 0.35,
            accent[2] * 0.65 + 0.35,
            1.0,
        ];
        view.canvas.shadow(p, 16.0);
        view.canvas.rect(p, 16.0, GLASS, 1.0, LINE);
        view.hits.push((p, Hit::Panel));
        if w >= 240 && p.size.h >= 300 {
            let lay = Layout::new(p, self.state.shuffle.is_some() && self.state.repeat.is_some());
            let item = self.state.item.as_ref();
            let tint = item
                .and_then(|i| i.cover_path.as_ref())
                .and_then(|p| view.covers.variant(p, CoverKind::Hero))
                .map_or([0.3, 0.3, 0.34], |c| c.tint);
            if item.is_some() {
                // A halo in the cover's own colour. Drawn once per static build.
                view.canvas.glow(
                    lay.cover,
                    18.0,
                    46,
                    [tint[0], tint[1], tint[2], 0.62],
                );
            }
            image(view, renderer, item, lay.cover, CoverKind::Hero);
            let title = item.map_or("Nothing playing", |i| i.title.as_str());
            let artist = item.map_or("Pick something below", |i| i.subtitle.as_str());
            centered(
                view,
                renderer,
                title,
                lay.title,
                style(18.0, Weight::SemiBold, white(if item.is_some() { 0.97 } else { 0.9 })),
            );
            centered(
                view,
                renderer,
                artist,
                lay.artist,
                style(13.0, Weight::Medium, white(if item.is_some() { 0.70 } else { 0.55 })),
            );
            if item.is_some() {
                let line = if self.state.message.is_empty() {
                    "Spotify"
                } else {
                    self.state.message.as_str()
                };
                let st = style(11.0, Weight::Regular, white(0.50));
                let text_w = (view.canvas.text.measure(line, st).ceil() as i32)
                    .min(lay.device.size.w - 28);
                let left = lay.device.loc.x + (lay.device.size.w - text_w - 20) / 2;
                icon(
                    view,
                    renderer,
                    "audio-speakers-symbolic",
                    rect(left, lay.device.loc.y + 1, 14, 14),
                    white(0.55),
                );
                label(
                    view,
                    renderer,
                    line,
                    rect(left + 20, lay.device.loc.y, text_w, lay.device.size.h),
                    st,
                );
            }
            view.canvas
                .rect(lay.track, 2.0, white(0.16), 0.0, LINE);
            view.hits.push((lay.seek_hit, Hit::Seek));
            view.canvas.alpha = if item.is_some() { 1.0 } else { 0.45 };
            for (hit, area) in &lay.buttons {
                let hover = self.hovered == Some(*hit);
                match hit {
                    Hit::Control(command) => crate::media::ui::transport(
                        &mut view.canvas,
                        renderer,
                        &mut view.icons,
                        "Adwaita",
                        match *command {
                            "previous" => crate::media::Command::Previous,
                            "next" => crate::media::Command::Next,
                            _ => crate::media::Command::PlayPause,
                        },
                        self.state.playing,
                        true,
                        *area,
                        hover,
                    ),
                    Hit::Shuffle | Hit::Repeat => {
                        let (name, active) = if *hit == Hit::Shuffle {
                            ("media-playlist-shuffle-symbolic", self.state.shuffle == Some(true))
                        } else {
                            let mode = self.state.repeat.unwrap_or_default();
                            (
                                if mode == super::RepeatMode::Track {
                                    "media-playlist-repeat-song-symbolic"
                                } else {
                                    "media-playlist-repeat-symbolic"
                                },
                                mode != super::RepeatMode::None,
                            )
                        };
                        if hover {
                            view.canvas
                                .rect(*area, area.size.w as f32 / 2.0, white(0.10), 0.0, LINE);
                        }
                        icon(
                            view,
                            renderer,
                            name,
                            rect(area.loc.x + (area.size.w - 18) / 2, area.loc.y + (area.size.h - 18) / 2, 18, 18),
                            if active { accent_text } else { white(0.60) },
                        );
                        if active {
                            view.canvas.rect(
                                rect(area.loc.x + area.size.w / 2 - 2, area.loc.y + area.size.h - 5, 4, 4),
                                2.0,
                                accent,
                                0.0,
                                LINE,
                            );
                        }
                    }
                    _ => (),
                }
                view.hits.push((*area, *hit));
            }
            view.canvas.alpha = 1.0;
            let volume = self.state.volume.clamp(0.0, 1.0);
            let name = if volume == 0.0 {
                "audio-volume-muted-symbolic"
            } else if volume < 0.34 {
                "audio-volume-low-symbolic"
            } else if volume < 0.67 {
                "audio-volume-medium-symbolic"
            } else {
                "audio-volume-high-symbolic"
            };
            let row_y = lay.volume.loc.y;
            icon(
                view,
                renderer,
                name,
                rect(x + 24, row_y + 4, 16, 16),
                white(0.72),
            );
            let slider = lay.volume;
            view.canvas.rect(
                rect(slider.loc.x, row_y + 10, slider.size.w, 4),
                2.0,
                white(0.16),
                0.0,
                LINE,
            );
            let fill = (slider.size.w as f64 * volume).round() as i32;
            view.canvas.rect(
                rect(slider.loc.x, row_y + 10, fill, 4),
                2.0,
                accent,
                0.0,
                LINE,
            );
            view.canvas.rect(
                rect(slider.loc.x + fill - 6, row_y + 6, 12, 12),
                6.0,
                white(1.0),
                0.0,
                LINE,
            );
            view.hits.push((slider, Hit::Volume));
            let field = lay.search;
            view.canvas.rect(
                field,
                18.0,
                white(0.06),
                1.0,
                if self.query.is_empty() {
                    LINE
                } else {
                    [accent[0], accent[1], accent[2], 0.6]
                },
            );
            icon(
                view,
                renderer,
                "system-search-symbolic",
                rect(field.loc.x + 14, field.loc.y + 10, 16, 16),
                white(0.6),
            );
            let st = style(
                13.0,
                Weight::Regular,
                white(if self.query.is_empty() { 0.45 } else { 0.95 }),
            );
            let text = if self.query.is_empty() {
                "Search Spotify"
            } else {
                &self.query
            };
            let text_width = field.size.w - 52;
            label(
                view,
                renderer,
                text,
                rect(field.loc.x + 38, field.loc.y + 9, text_width, 20),
                st,
            );
            if !self.query.is_empty() {
                let caret = view.canvas.text.measure(text, st).ceil() as i32;
                view.canvas.rect(
                    rect(
                        field.loc.x + 38 + caret.min(text_width),
                        field.loc.y + 10,
                        2,
                        16,
                    ),
                    0.0,
                    accent,
                    0.0,
                    LINE,
                );
            }
            let tabs = lay.tabs;
            view.canvas.rect(tabs, 10.0, white(0.05), 0.0, LINE);
            for (i, name) in ["Home", "Library", "Playlists"].into_iter().enumerate() {
                let left = i as i32 * (tabs.size.w - 6) / 3;
                let end = (i as i32 + 1) * (tabs.size.w - 6) / 3;
                let area = rect(tabs.loc.x + 3 + left, tabs.loc.y + 3, end - left, 26);
                if i == self.tab || self.hovered == Some(Hit::Tab(i)) {
                    view.canvas.rect(
                        area,
                        8.0,
                        if i == self.tab {
                            [accent[0], accent[1], accent[2], 0.28]
                        } else {
                            white(0.05)
                        },
                        0.0,
                        LINE,
                    );
                }
                centered(
                    view,
                    renderer,
                    name,
                    rect(area.loc.x, area.loc.y + 5, area.size.w, 20),
                    style(
                        12.0,
                        Weight::Medium,
                        white(if i == self.tab { 0.97 } else { 0.62 }),
                    ),
                );
                view.hits.push((area, Hit::Tab(i)));
            }
            let status = if !self.error.is_empty() {
                &self.error
            } else if self.state.item.is_none() || !self.state.logged_in {
                &self.state.message
            } else {
                ""
            };
            if !status.is_empty() {
                label(
                    view,
                    renderer,
                    status,
                    rect(x + 16, lay.status_y, w - 32, 18),
                    style(
                        11.0,
                        Weight::Regular,
                        if self.error.is_empty() {
                            white(0.55)
                        } else {
                            [1.0, 0.56, 0.56, 0.95]
                        },
                    ),
                );
            }
            let list_top = lay.list_top + if status.is_empty() { 0 } else { 22 };
            let bottom = y + p.size.h - 12;
            if !self.state.logged_in {
                let available = bottom - list_top;
                // Keep the full card on tall outputs and a reachable button
                // when there is too little room for its illustration and copy.
                let cy = list_top + (available - 164).max(0) / 2;
                if available >= 164 {
                    icon(
                        view,
                        renderer,
                        "audio-x-generic-symbolic",
                        rect(x + w / 2 - 20, cy, 40, 40),
                        white(0.35),
                    );
                    centered(
                        view,
                        renderer,
                        "Connect Spotify",
                        rect(x + 12, cy + 52, w - 24, 24),
                        style(15.0, Weight::SemiBold, white(0.95)),
                    );
                    centered(
                        view,
                        renderer,
                        "Sign in once to play your music here.",
                        rect(x + 12, cy + 80, w - 24, 20),
                        style(11.5, Weight::Regular, white(0.58)),
                    );
                }
                let button_y = if available >= 164 {
                    cy + 116
                } else {
                    (list_top + (available - 36) / 2).min(bottom - 36)
                };
                let button = rect(x + (w - 140) / 2, button_y, 140, 36);
                view.canvas.rect(
                    button,
                    18.0,
                    white(if self.state.login_pending { 0.12 } else { 0.96 }),
                    0.0,
                    LINE,
                );
                centered(
                    view,
                    renderer,
                    if self.state.login_pending {
                        "Waiting for login…"
                    } else {
                        "Log in"
                    },
                    rect(button.loc.x + 4, button.loc.y + 9, 132, 20),
                    style(
                        13.0,
                        Weight::SemiBold,
                        if self.state.login_pending {
                            white(0.75)
                        } else {
                            [0.08, 0.09, 0.12, 1.0]
                        },
                    ),
                );
                view.hits.push((button, Hit::Login));
            } else {
                let title_x = if self.back.is_empty() { x + 16 } else { x + 48 };
                if !self.back.is_empty() {
                    let area = rect(x + 12, list_top, 28, 28);
                    if self.hovered == Some(Hit::Back) {
                        view.canvas.rect(area, 14.0, white(0.10), 0.0, LINE);
                    }
                    icon(
                        view,
                        renderer,
                        "go-previous-symbolic",
                        rect(area.loc.x + 6, area.loc.y + 6, 16, 16),
                        white(1.0),
                    );
                    view.hits.push((area, Hit::Back));
                }
                let show_header = !self.back.is_empty()
                    || self.searching
                    || !self.query.is_empty()
                    || self.loading;
                if show_header {
                    label(
                        view,
                        renderer,
                        &if self.loading {
                            "Loading…".into()
                        } else {
                            page_title(&self.page.title)
                        },
                        rect(title_x, list_top + 5, right - 16 - title_x, 22),
                        style(
                            14.0,
                            Weight::SemiBold,
                            white(if self.loading { 0.55 } else { 0.95 }),
                        ),
                    );
                }
                let mut row_y = list_top + if show_header { 36 } else { 4 };
                view.visible_rows = ((bottom - row_y) / 52).max(0) as usize;
                for index in self.scroll
                    ..(self.scroll + view.visible_rows)
                        .min(self.page.items.len() + usize::from(self.page.next.is_some()))
                {
                    let area = rect(x + 8, row_y, w - 16, 48);
                    if index == self.page.items.len() {
                        if self.hovered == Some(Hit::More) {
                            view.canvas.rect(area, 10.0, white(0.06), 0.0, LINE);
                        }
                        centered(
                            view,
                            renderer,
                            "Show more",
                            rect(area.loc.x, row_y + 16, area.size.w, 20),
                            style(12.0, Weight::Medium, accent_text),
                        );
                        view.hits.push((area, Hit::More));
                        break;
                    }
                    let item = &self.page.items[index];
                    if item.kind == "section" {
                        let mut st = style(11.0, Weight::SemiBold, white(0.55));
                        st.tracking = 0.8;
                        label(
                            view,
                            renderer,
                            &item.title.to_uppercase(),
                            rect(x + 16, row_y + 22, w - 32, 20),
                            st,
                        );
                    } else {
                        if index == self.selected || self.hovered == Some(Hit::Row(index)) {
                            view.canvas.rect(area, 10.0, white(0.07), 0.0, LINE);
                        }
                        image(
                            view,
                            renderer,
                            Some(item),
                            rect(x + 14, row_y + 3, 42, 42),
                            if item.kind == "artist" {
                                CoverKind::Artist
                            } else {
                                CoverKind::List
                            },
                        );
                        let playing = self
                            .state
                            .item
                            .as_ref()
                            .is_some_and(|current| current.uri == item.uri);
                        let duration = item.kind == "track" && item.duration_ms > 0;
                        let text_w = w - 86 - if playing || duration { 56 } else { 0 };
                        label(
                            view,
                            renderer,
                            &item.title,
                            rect(x + 66, row_y + 7, text_w, 20),
                            style(
                                12.5,
                                Weight::Medium,
                                if playing { accent_text } else { white(0.95) },
                            ),
                        );
                        label(
                            view,
                            renderer,
                            &item.subtitle,
                            rect(x + 66, row_y + 27, text_w, 18),
                            style(11.0, Weight::Regular, white(0.58)),
                        );
                        if playing {
                            icon(
                                view,
                                renderer,
                                "audio-volume-high-symbolic",
                                rect(right - 36, row_y + 16, 16, 16),
                                accent,
                            );
                        } else if duration {
                            let text = time(item.duration_ms);
                            let st = style(11.0, Weight::Regular, white(0.45));
                            let width = view.canvas.text.measure(&text, st).ceil() as i32;
                            label(
                                view,
                                renderer,
                                &text,
                                rect(right - 20 - width, row_y + 17, width, 18),
                                st,
                            );
                        }
                        view.hits.push((area, Hit::Row(index)));
                    }
                    row_y += 52;
                }
            }
        } else {
            label(
                view,
                renderer,
                "Music",
                rect(x + 16, y + 16, w - 32, 20),
                style(14.0, Weight::SemiBold, white(0.95)),
            );
        }
        view.cached = view
            .canvas
            .finish()
            .into_iter()
            .filter_map(crate::classic::cache::CachedElement::from_ui)
            .collect();
    }
    fn build_progress(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Logical>,
        scale: f64,
        accent: [f32; 4],
    ) {
        let view = &mut self.view;
        let p = panel_rect(size, self.anchor);
        if p.size.w < 240 || p.size.h < 300 {
            view.tick_cached.clear();
            return;
        }
        let lay = Layout::new(p, self.state.shuffle.is_some() && self.state.repeat.is_some());
        let duration = self.state.item.as_ref().map_or(0, |i| i.duration_ms);
        let position = self.state.position().min(duration);
        let width = lay.track.size.w;
        let fill = if duration == 0 {
            0
        } else {
            (width as f64 * position as f64 / duration as f64).round() as i32
        };
        let key = (
            fill,
            position / 1000,
            duration.saturating_sub(position) / 1000,
            self.state.item.is_some(),
        );
        if view.tick_key == Some(key) {
            return;
        }
        #[cfg(test)]
        {
            view.tick_builds += 1;
        }
        view.tick.begin(renderer, scale);
        let (x, y) = (lay.track.loc.x, lay.track.loc.y);
        if self.state.item.is_some() {
            view.tick.rect(rect(x, y, fill, 4), 2.0, accent, 0.0, LINE);
            view.tick.rect(
                rect(x + fill - 6, y - 4, 12, 12),
                6.0,
                white(1.0),
                0.0,
                LINE,
            );
            let st = style(11.0, Weight::Medium, white(0.62));
            view.tick
                .tabular_label(renderer, &time(position), x, lay.times_y, st);
            let remaining = format!("-{}", time(duration.saturating_sub(position)));
            let text_width = view.tick.tabular_width(&remaining, st);
            let label_x = x + width - text_width;
            view.tick
                .tabular_label(renderer, &remaining, label_x, lay.times_y, st);
            #[cfg(test)]
            {
                view.remaining_right = label_x + text_width;
            }
        }
        view.tick_cached = view
            .tick
            .finish()
            .into_iter()
            .filter_map(crate::classic::cache::CachedElement::from_ui)
            .collect();
        view.tick_key = Some(key);
    }
}
