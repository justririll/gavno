//! Cached snap preview and text-only MRU switcher. Springs run only on frames.
use crate::{
    render::GavnoRenderElement,
    snap::Zone,
    ui::{
        shader::{self, ShaderBox, Shaders},
        spring::{Spring, SpringParams},
        text::{TextRenderer, TextStyleSpec, Weight},
    },
    Gavno,
};
use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBufferRenderElement, Kind},
        gles::GlesRenderer,
    },
    desktop::Window,
    output::Output,
    utils::{IsAlive, Logical, Physical, Point, Rectangle},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};
use std::time::Duration;

const ACCENT: [f32; 4] = [1.0, 0.73, 0.55, 1.0];

struct SpringRect([Spring; 4]);
impl SpringRect {
    fn new(r: Rectangle<i32, Logical>) -> Self {
        Self(
            [r.loc.x, r.loc.y, r.size.w, r.size.h]
                .map(|v| Spring::new(v as f64, SpringParams::SNAPPY)),
        )
    }
    fn set(&mut self, r: Rectangle<i32, Logical>) {
        for (s, v) in self
            .0
            .iter_mut()
            .zip([r.loc.x, r.loc.y, r.size.w, r.size.h])
        {
            s.set(v as f64);
        }
    }
    fn advance(&mut self, now: Duration) -> Rectangle<i32, Logical> {
        let [x, y, w, h] = self.0.each_mut().map(|s| s.advance(now).round() as i32);
        Rectangle::new((x, y).into(), (w.max(1), h.max(1)).into())
    }
    fn active(&self) -> bool {
        self.0.iter().any(Spring::is_active)
    }
}

struct Preview {
    output: Output,
    zone: Option<Zone>,
    rect: SpringRect,
    opacity: Spring,
}

pub struct Switcher {
    pub open: bool,
    windows: Vec<Window>,
    selected: usize,
    output: Output,
    opacity: Spring,
    row: Spring,
}

#[derive(Default)]
struct OutputCache {
    preview: ShaderBox,
    panel: ShaderBox,
    selection: ShaderBox,
}

pub struct Overlay {
    preview: Option<Preview>,
    pub switcher: Option<Switcher>,
    shaders: Option<Shaders>,
    shader_failed: bool,
    caches: Vec<(Output, OutputCache)>,
    text: TextRenderer,
}
impl Default for Overlay {
    fn default() -> Self {
        Self {
            preview: None,
            switcher: None,
            shaders: None,
            shader_failed: false,
            caches: Vec::new(),
            text: TextRenderer::new(),
        }
    }
}
impl Overlay {
    pub fn is_animating(&self) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|p| p.opacity.is_active() || p.rect.active())
            || self
                .switcher
                .as_ref()
                .is_some_and(|s| s.opacity.is_active() || s.row.is_active())
    }
    pub fn switcher_open(&self) -> bool {
        self.switcher.as_ref().is_some_and(|s| s.open)
    }
    pub fn preview(
        &mut self,
        output: Output,
        zone: Zone,
        from: Rectangle<i32, Logical>,
        target: Rectangle<i32, Logical>,
        now: Duration,
    ) {
        let fresh = self
            .preview
            .as_ref()
            .is_none_or(|p| p.opacity.value <= 0.001 && p.zone.is_none());
        if fresh {
            self.preview = Some(Preview {
                output: output.clone(),
                zone: None,
                rect: SpringRect::new(from),
                opacity: Spring::new(0.0, SpringParams::SNAPPY),
            });
        }
        if let Some(p) = &mut self.preview {
            // Prime before retargeting: a long idle must not become animation time.
            p.rect.advance(now);
            p.opacity.advance(now);
            p.output = output;
            p.zone = Some(zone);
            p.rect.set(target);
            p.opacity.set(1.0);
        }
    }
    pub fn hide_preview(&mut self, now: Duration) {
        if let Some(p) = &mut self.preview {
            p.opacity.advance(now);
            p.opacity.set(0.0);
            p.zone = None;
        }
    }
    pub fn prune(&mut self, mapped: &[Window], now: Duration) {
        if let Some(s) = &mut self.switcher {
            let selected = s.windows.get(s.selected).cloned();
            s.windows.retain(|w| w.alive() && mapped.contains(w));
            s.selected = selected
                .and_then(|w| s.windows.iter().position(|n| *n == w))
                .unwrap_or(s.selected.min(s.windows.len().saturating_sub(1)));
            if s.windows.is_empty() {
                s.open = false;
                s.opacity.advance(now);
                s.opacity.set(0.0);
            }
        }
    }
}

impl Gavno {
    pub fn switcher_cycle(&mut self, backwards: bool) {
        let now = self.start_time.elapsed();
        self.prune_navigation();
        if !self.overlay.switcher_open() {
            if self.mru.is_empty() {
                return;
            }
            let Some(output) = self.active_output() else {
                return;
            };
            let mut opacity = Spring::new(0.0, SpringParams::SNAPPY);
            opacity.advance(now);
            opacity.set(1.0);
            let mut row = Spring::new(0.0, SpringParams::SNAPPY);
            row.advance(now);
            self.overlay.switcher = Some(Switcher {
                open: true,
                windows: self.mru.clone(),
                selected: 0,
                output,
                opacity,
                row,
            });
        }
        if let Some(s) = &mut self.overlay.switcher {
            let n = s.windows.len();
            if n > 0 {
                s.selected = if backwards {
                    (s.selected + n - 1) % n
                } else {
                    (s.selected + 1) % n
                };
            }
        }
        self.queue_redraw_all();
    }
    pub fn switcher_finish(&mut self, commit: bool) {
        let selected = self.overlay.switcher.as_mut().and_then(|s| {
            s.open = false;
            s.opacity.advance(self.start_time.elapsed());
            s.opacity.set(0.0);
            commit.then(|| s.windows.get(s.selected).cloned()).flatten()
        });
        if let Some(w) = selected.filter(|w| w.alive() && (self.space.element_location(w).is_some() || self.workspaces.membership(w).is_some()))
        {
            self.focus_window(Some(&w));
        }
        self.queue_redraw_all();
    }
    pub fn prune_navigation(&mut self) {
        let mut mapped: Vec<_> = self.space.elements().cloned().collect();
        for v in self.workspaces.all() { if v.window.alive() && !mapped.contains(&v.window) { mapped.push(v.window.clone()); } }
        for w in &mapped { if !self.mru.contains(w) { self.mru.push(w.clone()); } }
        self.mru.retain(|w| w.alive() && mapped.contains(w));
        self.overlay.prune(&mapped, self.start_time.elapsed());
    }
    pub fn update_snap_preview(&mut self, window: &Window, pos: Point<f64, Logical>) {
        let target = self.space.outputs().find_map(|o| {
            let geo = self.space.output_geometry(o)?;
            let zone = crate::snap::zone_at(pos, geo)?;
            Some((
                o.clone(),
                zone,
                zone.rect(self.work_area(o), self.config.gap),
            ))
        });
        let now = self.start_time.elapsed();
        if let Some((output, zone, rect)) = target {
            let from = self.window_rect(window).unwrap_or(rect);
            self.overlay.preview(output, zone, from, rect, now);
        } else {
            self.overlay.hide_preview(now);
        }
        self.queue_redraw_all();
    }
    pub fn apply_drag_snap(&mut self, window: &Window, pos: Point<f64, Logical>) {
        let target = self.space.outputs().find_map(|o| {
            crate::snap::zone_at(pos, self.space.output_geometry(o)?).map(|z| (o.clone(), z))
        });
        self.overlay.hide_preview(self.start_time.elapsed());
        if let Some((o, z)) = target {
            self.set_window_mode(window, z.mode(), Some(o));
        }
        self.queue_redraw_all();
    }
}

/// Output-local elements, front to back. The split state arguments allow both
/// backends to borrow their renderer without borrowing all of Gavno again.
pub fn overlay_elements(
    gv: &mut Overlay,
    renderer: &mut GlesRenderer,
    output: &Output,
    geometry: Rectangle<i32, Logical>,
    scale: f64,
    now: Duration,
) -> Vec<GavnoRenderElement> {
    let mut out = Vec::new();
    let preview = gv.preview.as_mut().map(|p| {
        let r = p.rect.advance(now);
        let a = p.opacity.advance(now).clamp(0.0, 1.0) as f32;
        (p.output.clone(), r, a)
    });
    // Advance even on another output, including while an output is unplugged.
    if let Some(s) = &mut gv.switcher {
        s.opacity.advance(now);
        s.row.advance(now);
    }
    let visible_preview = preview
        .as_ref()
        .is_some_and(|(o, _, a)| o == output && *a > 0.001);
    let visible_switcher = gv
        .switcher
        .as_ref()
        .is_some_and(|s| s.output == *output && s.opacity.value > 0.001);
    if !visible_preview && !visible_switcher {
        return out;
    }
    if gv.shaders.is_none() && !gv.shader_failed {
        match Shaders::compile(renderer) {
            Ok(s) => gv.shaders = Some(s),
            Err(err) => {
                tracing::warn!(?err, "overlay shaders failed to compile");
                gv.shader_failed = true;
            }
        }
    }
    let Some(shaders) = gv.shaders.as_ref() else {
        return out;
    };
    if !gv.caches.iter().any(|(o, _)| o == output) {
        gv.caches.push((output.clone(), OutputCache::default()));
    }
    let Some((_, cache)) = gv.caches.iter_mut().find(|(o, _)| o == output) else {
        return out;
    };
    let local = |r: Rectangle<i32, Logical>| Rectangle::new(r.loc - geometry.loc, r.size);
    if visible_switcher {
        if let Some(s) = &mut gv.switcher {
            let rows = ((geometry.size.h - 80) / 60).clamp(1, 8) as usize;
            let count = s.windows.len().min(rows);
            let first = s.selected.saturating_sub(rows - 1);
            let width = (geometry.size.w - 48).clamp(1, 560);
            let height = (count as i32 * 60 + 32).max(32);
            let x = (geometry.size.w - width) / 2;
            let y = (geometry.size.h - height) / 2;
            let alpha = s.opacity.value.clamp(0.0, 1.0) as f32;
            s.row.set((s.selected - first) as f64 * 60.0);
            for (i, w) in s.windows.iter().skip(first).take(count).enumerate() {
                let (title, app) = window_labels(w);
                push_text(
                    &mut gv.text,
                    renderer,
                    &mut out,
                    &title,
                    15.0,
                    Weight::Medium,
                    x + 28,
                    y + 24 + i as i32 * 60,
                    width - 56,
                    alpha,
                    scale,
                );
                push_text(
                    &mut gv.text,
                    renderer,
                    &mut out,
                    &app,
                    11.0,
                    Weight::Regular,
                    x + 28,
                    y + 46 + i as i32 * 60,
                    width - 56,
                    alpha * 0.6,
                    scale,
                );
            }
            let selected = Rectangle::new(
                (x + 12, y + 16 + s.row.value.round() as i32).into(),
                ((width - 24).max(1), 56).into(),
            );
            out.push(GavnoRenderElement::Shader(shader::rrect(
                &mut cache.selection,
                shaders,
                selected,
                10.0 * scale as f32,
                [ACCENT[0], ACCENT[1], ACCENT[2], 0.18],
                1.0 * scale as f32,
                [ACCENT[0], ACCENT[1], ACCENT[2], 0.42],
                alpha,
                scale,
            )));
            out.push(GavnoRenderElement::Shader(shader::rrect(
                &mut cache.panel,
                shaders,
                Rectangle::new((x, y).into(), (width, height).into()),
                18.0 * scale as f32,
                [0.065, 0.075, 0.105, 0.94],
                1.0 * scale as f32,
                [1.0, 1.0, 1.0, 0.12],
                alpha,
                scale,
            )));
        }
    }
    if visible_preview {
        if let Some((_, r, a)) = preview {
            out.push(GavnoRenderElement::Shader(shader::rrect(
                &mut cache.preview,
                shaders,
                local(r),
                14.0 * scale as f32,
                [ACCENT[0], ACCENT[1], ACCENT[2], 0.18],
                1.5 * scale as f32,
                [ACCENT[0], ACCENT[1], ACCENT[2], 0.65],
                a,
                scale,
            )));
        }
    }
    out
}

pub(crate) fn window_labels(w: &Window) -> (String, String) {
    let Some(t) = w.toplevel() else {
        return ("Window".into(), String::new());
    };
    with_states(t.wl_surface(), |states| {
        let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
            return ("Window".into(), String::new());
        };
        let data = data.lock().unwrap_or_else(|e| e.into_inner());
        (
            data.title.clone().unwrap_or_else(|| "Window".into()),
            data.app_id.clone().unwrap_or_default(),
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text(
    text: &mut TextRenderer,
    renderer: &mut GlesRenderer,
    out: &mut Vec<GavnoRenderElement>,
    value: &str,
    size: f32,
    weight: Weight,
    x: i32,
    y: i32,
    width: i32,
    alpha: f32,
    scale: f64,
) {
    let style = TextStyleSpec {
        size,
        weight,
        color: [0.96, 0.95, 0.98, 1.0],
        tracking: 0.0,
    };
    // Bound client-controlled labels before measurement and raster allocation.
    let chars: Vec<_> = value.chars().filter(|c| !c.is_control()).take(96).collect();
    let mut label: String = chars.iter().collect();
    if text.measure(&label, style) > width.max(1) as f32 {
        let mut lo = 0;
        let mut hi = chars.len();
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let candidate: String = chars[..mid].iter().collect();
            if text.measure(&candidate, style) <= width.max(1) as f32 {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        label = chars[..lo].iter().collect();
    }
    let Some(img) = text.render(&label, style, scale) else {
        return;
    };
    if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        Point::<f64, Physical>::from(((x as f64 * scale).round(), (y as f64 * scale).round())),
        &img.buffer,
        Some(alpha),
        Some(Rectangle::from_size(
            (img.width as f64, img.height as f64).into(),
        )),
        Some((img.logical_w, img.logical_h).into()),
        Kind::Unspecified,
    ) {
        out.push(GavnoRenderElement::Memory(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_and_switcher_springs_settle_without_a_timer() {
        let output = Output::new(
            "test".into(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        );
        let mut overlay = Overlay::default();
        let from = Rectangle::new((100, 100).into(), (600, 400).into());
        let target = Rectangle::new((12, 12).into(), (480, 776).into());
        overlay.preview(output.clone(), Zone::Left, from, target, Duration::ZERO);
        let mut opacity = Spring::new(0.0, SpringParams::SNAPPY);
        opacity.advance(Duration::ZERO);
        opacity.set(1.0);
        let mut row = Spring::new(0.0, SpringParams::SNAPPY);
        row.advance(Duration::ZERO);
        row.set(120.0);
        overlay.switcher = Some(Switcher {
            open: true,
            windows: Vec::new(),
            selected: 0,
            output,
            opacity,
            row,
        });
        assert!(overlay.is_animating());
        for frame in 0..360 {
            let now = Duration::from_millis(frame * 8);
            if let Some(p) = &mut overlay.preview {
                p.rect.advance(now);
                p.opacity.advance(now);
            }
            if let Some(s) = &mut overlay.switcher {
                s.opacity.advance(now);
                s.row.advance(now);
            }
        }
        assert!(!overlay.is_animating());
        overlay.hide_preview(Duration::from_secs(3));
        if let Some(s) = &mut overlay.switcher {
            s.opacity.advance(Duration::from_secs(3));
            s.opacity.set(0.0);
            s.open = false;
        }
        assert!(overlay.is_animating());
        for frame in 0..360 {
            let now = Duration::from_secs(3) + Duration::from_millis(frame * 8);
            if let Some(p) = &mut overlay.preview {
                p.rect.advance(now);
                p.opacity.advance(now);
            }
            if let Some(s) = &mut overlay.switcher {
                s.opacity.advance(now);
                s.row.advance(now);
            }
        }
        assert!(!overlay.is_animating());
    }
}
