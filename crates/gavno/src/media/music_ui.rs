//! Glass popover, with stable render elements reused until its contents change.
use super::{Covers, Hit, Hits, Item, Session};
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
        Canvas, GLASS, LINE,
    },
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
    geometry: Option<(Size<i32, Logical>, f64)>,
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
pub fn panel_rect(size: Size<i32, Logical>) -> Rect {
    let width = 420.min((size.w - 16).max(1));
    let height = 600.min((size.h - 48).max(1));
    let x = (size.w / 2 + 64).min(size.w - width - 8).max(0);
    rect(x, 40.min((size.h - height).max(0)), width, height)
}
fn image(view: &mut View, renderer: &mut GlesRenderer, item: Option<&Item>, area: Rect) {
    let c = &mut view.canvas;
    c.rect(area, 7.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);
    if let Some(buffer) = item
        .and_then(|i| i.cover_path.as_ref())
        .and_then(|p| view.covers.get(p))
    {
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (
                (area.loc.x as f64 * c.scale).round(),
                (area.loc.y as f64 * c.scale).round(),
            ),
            buffer,
            Some(1.0),
            Some(Rectangle::from_size((96.0, 96.0).into())),
            Some(area.size),
            Kind::Unspecified,
        ) {
            c.elements.push(GavnoRenderElement::Memory(element));
        }
    } else {
        c.colored_icon(
            renderer,
            &mut view.icons,
            &["audio-x-generic-symbolic"],
            "Adwaita",
            rect(
                area.loc.x + area.size.w / 4,
                area.loc.y + area.size.h / 4,
                area.size.w / 2,
                area.size.h / 2,
            ),
            Some(crate::media::ui::ICON_COLOR),
        );
    }
}
fn label(view: &mut View, renderer: &mut GlesRenderer, value: &str, area: Rect, size: f32) {
    view.canvas
        .label(renderer, value, area.loc.x, area.loc.y, area.size.w, size);
}
fn button(view: &mut View, renderer: &mut GlesRenderer, area: Rect, text: &str, hit: Hit) {
    view.canvas
        .rect(area, 8.0, [1.0, 1.0, 1.0, 0.07], 0.0, LINE);
    label(
        view,
        renderer,
        text,
        rect(area.loc.x + 10, area.loc.y + 8, area.size.w - 20, 20),
        12.0,
    );
    view.hits.push((area, hit));
}
impl Session {
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        size: Size<i32, Logical>,
        scale: f64,
    ) -> Vec<GavnoRenderElement> {
        if &self.output != output {
            return Vec::new();
        }
        let view = &mut self.view;
        if !self.dirty && view.geometry == Some((size, scale)) {
            return view
                .cached
                .iter()
                .cloned()
                .map(GavnoRenderElement::Desktop)
                .collect();
        }
        self.dirty = false;
        view.geometry = Some((size, scale));
        view.canvas.begin(renderer, scale);
        view.hits.clear();
        let panel = panel_rect(size);
        let x = panel.loc.x + 16;
        let y = panel.loc.y + 16;
        let w = (panel.size.w - 32).max(1);
        view.canvas.shadow(panel, 16.0);
        view.canvas.rect(panel, 16.0, GLASS, 1.0, LINE);
        view.hits.push((panel, Hit::Panel));
        // Tiny outputs retain a correctly clamped shell; omit overflowing contents.
        if panel.size.w < 240 || panel.size.h < 300 {
            label(view, renderer, "Music", rect(x, y, w, 20), 14.0);
            view.cached = view
                .canvas
                .finish()
                .into_iter()
                .filter_map(crate::classic::cache::CachedElement::from_ui)
                .collect();
            return view
                .cached
                .iter()
                .cloned()
                .map(GavnoRenderElement::Desktop)
                .collect();
        }
        image(view, renderer, self.state.item.as_ref(), rect(x, y, 52, 52));
        let title = self
            .state
            .item
            .as_ref()
            .map_or("Spotify", |i| i.title.as_str());
        let subtitle = self.state.item.as_ref().map_or("", |i| i.subtitle.as_str());
        label(view, renderer, title, rect(x + 64, y + 4, w - 64, 22), 14.0);
        label(
            view,
            renderer,
            subtitle,
            rect(x + 64, y + 30, w - 64, 18),
            11.5,
        );
        for (i, command) in ["previous", "toggle", "next"].into_iter().enumerate() {
            let r = rect(x + i as i32 * 48, y + 64, 40, 30);
            view.canvas.rect(r, 8.0, [1.0, 1.0, 1.0, 0.07], 0.0, LINE);
            let icon = match command {
                "previous" => "media-skip-backward-symbolic",
                "next" => "media-skip-forward-symbolic",
                _ if self.state.playing => "media-playback-pause-symbolic",
                _ => "media-playback-start-symbolic",
            };
            view.canvas.colored_icon(
                renderer,
                &mut view.icons,
                &[icon],
                "Adwaita",
                rect(r.loc.x + 12, r.loc.y + 7, 16, 16),
                Some(crate::media::ui::ICON_COLOR),
            );
            view.hits.push((r, Hit::Control(command)));
        }
        let duration = self.state.item.as_ref().map_or(0, |i| i.duration_ms);
        let position = self.state.position();
        let seek = rect(x, y + 104, w, 18);
        view.canvas.rect(
            rect(x, seek.loc.y + 6, w, 5),
            2.5,
            [1.0, 1.0, 1.0, 0.12],
            0.0,
            LINE,
        );
        let fraction = if duration == 0 {
            0.0
        } else {
            position as f64 / duration as f64
        };
        view.canvas.rect(
            rect(x, seek.loc.y + 6, (w as f64 * fraction) as i32, 5),
            2.5,
            [0.35, 0.82, 0.58, 1.0],
            0.0,
            LINE,
        );
        view.hits.push((seek, Hit::Seek));
        label(
            view,
            renderer,
            &format!(
                "{}:{:02} / {}:{:02}",
                position / 60000,
                position / 1000 % 60,
                duration / 60000,
                duration / 1000 % 60
            ),
            rect(x + 156, y + 72, w - 156, 20),
            11.0,
        );
        label(view, renderer, "Volume", rect(x, y + 133, 60, 20), 11.0);
        let volume = rect(x + 70, y + 128, w - 70, 22);
        view.canvas.rect(
            rect(volume.loc.x, volume.loc.y + 8, volume.size.w, 5),
            2.5,
            [1.0, 1.0, 1.0, 0.12],
            0.0,
            LINE,
        );
        view.canvas.rect(
            rect(
                volume.loc.x,
                volume.loc.y + 8,
                (volume.size.w as f64 * self.state.volume.clamp(0.0, 1.0)) as i32,
                5,
            ),
            2.5,
            [0.65, 0.72, 0.94, 1.0],
            0.0,
            LINE,
        );
        view.hits.push((volume, Hit::Volume));
        view.canvas.rect(
            rect(x, y + 162, w, 36),
            8.0,
            [1.0, 1.0, 1.0, 0.07],
            1.0,
            LINE,
        );
        label(
            view,
            renderer,
            if self.query.is_empty() {
                "Search Spotify…"
            } else {
                &self.query
            },
            rect(x + 12, y + 172, w - 24, 20),
            13.0,
        );
        for (i, tab) in ["Home", "Library", "Playlists"].into_iter().enumerate() {
            let r = rect(x + i as i32 * (w / 3), y + 208, w / 3 - 4, 32);
            if i == self.tab {
                view.canvas.rect(r, 8.0, [0.4, 0.6, 0.9, 0.18], 0.0, LINE);
            }
            label(
                view,
                renderer,
                tab,
                rect(r.loc.x + 8, r.loc.y + 8, r.size.w - 16, 20),
                12.0,
            );
            view.hits.push((r, Hit::Tab(i)));
        }
        let status = if !self.error.is_empty() {
            &self.error
        } else {
            &self.state.message
        };
        label(view, renderer, status, rect(x, y + 250, w, 18), 11.0);
        let mut row_y = y + 278;
        if row_y + 40 > panel.loc.y + panel.size.h - 12 {
            view.visible_rows = 0;
        } else if !self.state.logged_in {
            button(
                view,
                renderer,
                rect(x, row_y, w, 36),
                if self.state.login_pending {
                    "Waiting for Spotify login…"
                } else {
                    "Log in to Spotify"
                },
                Hit::Login,
            );
        } else {
            if !self.back.is_empty() || self.search_origin.is_some() {
                button(view, renderer, rect(x, row_y, 64, 30), "Back", Hit::Back);
            }
            label(
                view,
                renderer,
                if self.loading {
                    "Loading…"
                } else {
                    &self.page.title
                },
                rect(x + 76, row_y + 8, w - 76, 20),
                12.0,
            );
            row_y += 40;
            view.visible_rows = ((panel.loc.y + panel.size.h - 12 - row_y) / 52).max(0) as usize;
            for index in self.scroll
                ..(self.scroll + view.visible_rows)
                    .min(self.page.items.len() + usize::from(self.page.next.is_some()))
            {
                if index == self.page.items.len() {
                    button(
                        view,
                        renderer,
                        rect(x, row_y, w, 44),
                        "Load more",
                        Hit::More,
                    );
                    break;
                }
                let item = &self.page.items[index];
                let r = rect(x, row_y, w, 48);
                if item.kind == "section" {
                    label(
                        view,
                        renderer,
                        &item.title,
                        rect(x + 4, row_y + 14, w - 8, 20),
                        13.0,
                    );
                } else {
                    if index == self.selected {
                        view.canvas.rect(r, 8.0, [1.0, 1.0, 1.0, 0.07], 0.0, LINE);
                    }
                    image(view, renderer, Some(item), rect(x + 4, row_y + 4, 40, 40));
                    label(
                        view,
                        renderer,
                        &item.title,
                        rect(x + 56, row_y + 7, w - 64, 20),
                        12.5,
                    );
                    label(
                        view,
                        renderer,
                        &item.subtitle,
                        rect(x + 56, row_y + 28, w - 64, 18),
                        11.0,
                    );
                    view.hits.push((r, Hit::Row(index)));
                }
                row_y += 52;
            }
        }
        view.cached = view
            .canvas
            .finish()
            .into_iter()
            .filter_map(crate::classic::cache::CachedElement::from_ui)
            .collect();
        view.cached
            .iter()
            .cloned()
            .map(GavnoRenderElement::Desktop)
            .collect()
    }
}
