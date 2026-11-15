//! Overview uses a single reversible progress spring for every visual transform.
use crate::{
    anim::{interpolate_rect, Rect},
    render::GavnoRenderElement,
    ui::{
        shader::{self, ShaderBox, Shaders},
        spring::{Spring, SpringParams},
        text::TextRenderer,
    },
    workspace::Workspaces,
    Gavno,
};
use smithay::{
    backend::renderer::{
        element::{
            surface::WaylandSurfaceRenderElement,
            utils::{CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement},
            AsRenderElements,
        },
        gles::GlesRenderer,
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Point, Rectangle, Scale},
};
use std::time::Duration;

pub type ThumbnailElement = CropRenderElement<
    RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
>;
#[derive(Clone)]
pub struct SceneWindow {
    pub window: Window,
    pub location: Point<i32, Logical>,
    pub rect: Rect,
}

/// Centred cells, with the mockup's 44px horizontal / 64px vertical breathing room.
/// A uniform scale preserves each window's aspect ratio, including extreme sizes.
pub fn pack(sizes: &[(f64, f64)], area: Rect) -> Vec<Rect> {
    if sizes.is_empty() || area.size.w <= 0.0 || area.size.h <= 0.0 {
        return Vec::new();
    }
    let n = sizes.len();
    let cols = ((n as f64 * area.size.w / area.size.h / 1.45).sqrt().ceil() as usize).clamp(1, n);
    let rows = n.div_ceil(cols);
    let cw = area.size.w / cols as f64;
    let ch = area.size.h / rows as f64;
    let gap_x = 44.0_f64.min(cw * 0.25);
    let gap_y = 64.0_f64.min(ch * 0.35);
    sizes
        .iter()
        .enumerate()
        .map(|(i, &(w, h))| {
            let (w, h) = (w.max(1.0), h.max(1.0));
            let scale = 1.0_f64.min((cw - gap_x) / w).min((ch - gap_y) / h);
            let in_row = (n - (i / cols) * cols).min(cols);
            Rectangle::new(
                (
                    area.loc.x
                        + (cols - in_row) as f64 * cw * 0.5
                        + (i % cols) as f64 * cw
                        + (cw - w * scale) * 0.5,
                    area.loc.y + (i / cols) as f64 * ch + (ch - gap_y - h * scale).max(0.0) * 0.5,
                )
                    .into(),
                (w * scale, h * scale).into(),
            )
        })
        .collect()
}

pub struct Overview {
    pub output: Option<Output>,
    pub progress: Spring,
    pub held: bool,
    pub hits: Vec<SceneWindow>,
    pub thumbnails: Vec<Rect>,
    order: Vec<Window>,
    pub drag: Option<(Window, Point<f64, Logical>, Point<f64, Logical>)>,
    shaders: Option<Shaders>,
    shader_failed: bool,
    boxes: Vec<ShaderBox>,
    text: TextRenderer,
}
impl Default for Overview {
    fn default() -> Self {
        Self {
            output: None,
            progress: Spring::new(0.0, SpringParams::SMOOTH),
            held: false,
            hits: Vec::new(),
            thumbnails: Vec::new(),
            order: Vec::new(),
            drag: None,
            shaders: None,
            shader_failed: false,
            boxes: Vec::new(),
            text: TextRenderer::new(),
        }
    }
}
impl Overview {
    pub fn visible(&self) -> bool {
        self.progress.value > 0.0005 || self.progress.target > 0.0
    }
    pub fn open(&self) -> bool {
        self.held && self.visible() || self.progress.target > 0.5
    }
    pub fn set(&mut self, output: Output, open: bool, now: Duration) {
        self.progress.advance(now);
        if !self.visible() {
            self.order.clear();
        }
        self.output = Some(output);
        self.held = false;
        self.progress.set(if open { 1.0 } else { 0.0 });
    }
    /// Builds output-local rectangles. Layer surfaces never enter this transform.
    pub fn scene(
        &mut self,
        workspaces: &mut Workspaces,
        space: &Space<Window>,
        output: &Output,
        now: Duration,
    ) -> Option<Vec<SceneWindow>> {
        workspaces.advance(now);
        self.progress.advance(now);
        let owner = workspaces.get(output)?;
        let p = if self.output.as_ref() == Some(output) {
            self.progress.value.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if p == 0.0
            && !owner.held
            && !owner.position.is_active()
            && (owner.position.value - owner.list.active as f64).abs() < 0.0005
        {
            return None;
        }
        let geo = space.output_geometry(output)?;
        let (w, h) = (geo.size.w as f64, geo.size.h as f64);
        let zoom = 1.0 - 0.34 * p;
        let spacing = w * (1.0 - 0.28 * p);
        let mut scene = Vec::new();
        for (index, ws) in owner.list.items.iter().enumerate() {
            let mut windows: Vec<_> = ws.windows.iter().collect();
            if p > 0.0 {
                for v in &windows {
                    if !self.order.contains(&v.window) {
                        self.order.push(v.window.clone());
                    }
                }
                windows.sort_by_key(|v| self.order.iter().position(|w| w == &v.window));
            }
            let area = Rectangle::new(
                (60.0_f64.min(w * 0.1), 60.0_f64.min(h * 0.1)).into(),
                ((w - 120.0).max(w * 0.8), (h - 130.0).max(h * 0.7)).into(),
            );
            let slots = pack(
                &windows
                    .iter()
                    .map(|v| {
                        let s = v.window.geometry().size;
                        (s.w as f64, s.h as f64)
                    })
                    .collect::<Vec<_>>(),
                area,
            );
            for (v, slot) in windows.into_iter().zip(slots) {
                let loc = space.element_location(&v.window).unwrap_or(v.location);
                let base = Rectangle::new(
                    loc.to_f64() - geo.loc.to_f64(),
                    v.window.geometry().size.to_f64(),
                );
                let mut rect = interpolate_rect(base, slot, p);
                rect.loc = (
                    rect.loc.x * zoom
                        + w * (1.0 - zoom) * 0.5
                        + (index as f64 - owner.position.value) * spacing,
                    rect.loc.y * zoom + h * (1.0 - zoom) * 0.5 + 60.0 * p,
                )
                    .into();
                rect.size = rect.size * zoom;
                if let Some((drag, start, current)) = &self.drag {
                    if drag == &v.window && (current.x - start.x).hypot(current.y - start.y) > 6.0 {
                        rect.loc += *current - *start;
                    }
                }
                scene.push(SceneWindow {
                    window: v.window.clone(),
                    location: loc,
                    rect,
                });
            }
        }
        if p > 0.0 {
            self.hits = scene.clone();
        }
        Some(scene)
    }
    pub fn chrome(
        &mut self,
        renderer: &mut GlesRenderer,
        workspaces: &Workspaces,
        space: &Space<Window>,
        output: &Output,
    ) -> Vec<GavnoRenderElement> {
        if self.output.as_ref() != Some(output) || !self.visible() {
            return Vec::new();
        }
        let Some(owner) = workspaces.get(output) else {
            return Vec::new();
        };
        let Some(geo) = space.output_geometry(output) else {
            return Vec::new();
        };
        let scale = output.current_scale().fractional_scale();
        let alpha = self.progress.value.clamp(0.0, 1.0) as f32;
        let mut out = Vec::new();
        for v in &self.hits {
            let title = crate::overlay::window_labels(&v.window).0;
            let x = v.rect.loc.x as i32;
            let y = (v.rect.loc.y + v.rect.size.h + 10.0) as i32;
            crate::overlay::push_text(
                &mut self.text,
                renderer,
                &mut out,
                &title,
                13.0,
                crate::ui::text::Weight::Medium,
                x,
                y,
                v.rect.size.w as i32,
                alpha,
                scale,
            );
        }
        if self.shaders.is_none() && !self.shader_failed {
            match Shaders::compile(renderer) {
                Ok(s) => self.shaders = Some(s),
                Err(err) => {
                    tracing::warn!(?err, "overview shaders unavailable");
                    self.shader_failed = true;
                }
            }
        }
        // Keep the strip reachable on small outputs and with many workspaces.
        let count = owner.list.items.len();
        let width = 96.0_f64.min(((geo.size.w as f64 - 40.0) / count as f64 - 10.0).max(8.0));
        let height = width * 60.0 / 96.0;
        let start = (geo.size.w as f64 - (width + 10.0) * count as f64 + 10.0) * 0.5;
        self.thumbnails = (0..count)
            .map(|i| {
                Rectangle::new(
                    (start + i as f64 * (width + 10.0), 90.0 * alpha as f64).into(),
                    (width, height).into(),
                )
            })
            .collect();
        self.boxes.resize_with(count * 2, ShaderBox::default);
        for (i, (ws, rect)) in owner.list.items.iter().zip(&self.thumbnails).enumerate() {
            let factor = width / geo.size.w.max(1) as f64;
            let physical = rect.to_physical_precise_round(scale);
            if let Some(shaders) = &self.shaders {
                let selected = i == owner.list.active;
                out.push(GavnoRenderElement::Shader(shader::rrect(
                    &mut self.boxes[i * 2],
                    shaders,
                    rect.to_i32_round(),
                    8.0 * scale as f32,
                    [0.0; 4],
                    if selected { 2.0 } else { 1.0 } * scale as f32,
                    if selected {
                        [0.95, 0.64, 0.42, 1.0]
                    } else {
                        [1.0, 1.0, 1.0, 0.15]
                    },
                    alpha,
                    scale,
                )));
            }
            for v in ws.windows.iter().rev() {
                let loc = space.element_location(&v.window).unwrap_or(v.location);
                let origin =
                    (loc - geo.loc - v.window.geometry().loc).to_physical_precise_round(scale);
                for e in v.window.render_elements::<WaylandSurfaceRenderElement<_>>(
                    renderer,
                    origin,
                    Scale::from(scale),
                    alpha,
                ) {
                    let e = RescaleRenderElement::from_element(e, (0, 0).into(), factor);
                    let e =
                        RelocateRenderElement::from_element(e, physical.loc, Relocate::Relative);
                    if let Some(e) = CropRenderElement::from_element(e, scale, physical) {
                        out.push(GavnoRenderElement::Thumbnail(e));
                    }
                }
            }
            if let Some(shaders) = &self.shaders {
                let selected = i == owner.list.active;
                out.push(GavnoRenderElement::Shader(shader::rrect(
                    &mut self.boxes[i * 2 + 1],
                    shaders,
                    rect.to_i32_round(),
                    8.0 * scale as f32,
                    [0.07, 0.08, 0.11, 1.0],
                    if selected { 2.0 } else { 1.0 } * scale as f32,
                    if selected {
                        [0.95, 0.64, 0.42, 1.0]
                    } else {
                        [1.0, 1.0, 1.0, 0.15]
                    },
                    alpha,
                    scale,
                )));
            }
        }
        out
    }
}
impl Gavno {
    pub fn toggle_overview(&mut self) {
        if self.greeter.is_some() || self.protocols.locks.locked {
            return;
        }
        if let Some(output) = self.active_output() {
            self.workspaces.snapshot(&self.space);
            self.overview
                .set(output, !self.overview.open(), self.start_time.elapsed());
            self.clear_desktop_pointer();
            self.queue_redraw_all();
        }
    }
    pub fn close_overview(&mut self) {
        if let Some(output) = self.overview.output.clone() {
            self.overview.set(output, false, self.start_time.elapsed());
            self.overview.drag = None;
            self.queue_redraw_all();
        }
    }
    pub fn clear_desktop_pointer(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.motion(
            self,
            None,
            &smithay::input::pointer::MotionEvent {
                location: pointer.current_location(),
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: self.clock.now().as_millis() as u32,
            },
        );
        pointer.frame(self);
    }
    pub fn overview_button(&mut self, button: u32, pressed: bool) -> bool {
        if !self.overview.visible() {
            return false;
        }
        let Some(output) = self.overview.output.clone() else {
            return true;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return true;
        };
        if !geo.to_f64().contains(self.pointer_location()) {
            return false;
        }
        let pos = self.pointer_location() - geo.loc.to_f64();
        if button == 0x110 {
            if pressed {
                if let Some(i) = self
                    .overview
                    .thumbnails
                    .iter()
                    .position(|r| r.contains(pos))
                {
                    self.switch_workspace(&output, i);
                } else if let Some(v) = self
                    .overview
                    .hits
                    .iter()
                    .rev()
                    .find(|v| v.rect.contains(pos))
                {
                    self.overview.drag = Some((v.window.clone(), pos, pos));
                }
            } else if let Some((w, start, _)) = self.overview.drag.take() {
                if let Some(i) = self
                    .overview
                    .thumbnails
                    .iter()
                    .position(|r| r.contains(pos))
                {
                    self.move_to_workspace(&w, i);
                } else if (pos.x - start.x).hypot(pos.y - start.y) < 6.0 {
                    self.focus_window(Some(&w));
                    self.close_overview();
                }
                self.queue_redraw_all();
            }
        } else if button == 0x112 && pressed {
            if let Some(v) = self
                .overview
                .hits
                .iter()
                .rev()
                .find(|v| v.rect.contains(pos))
            {
                if let Some(t) = v.window.toplevel() {
                    t.send_close();
                }
            }
        }
        true
    }
    pub fn overview_motion(&mut self, pos: Point<f64, Logical>) {
        if let Some(output) = &self.overview.output {
            if let Some(geo) = self.space.output_geometry(output) {
                if let Some((_, _, current)) = &mut self.overview.drag {
                    *current = pos - geo.loc.to_f64();
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packing_fits_preserves_aspect_and_never_overlaps() {
        for n in 1..80 {
            for (w, h) in [(1600.0, 900.0), (180.0, 120.0), (400.0, 1600.0)] {
                let sizes: Vec<_> = (0..n)
                    .map(|i| {
                        if i % 2 == 0 {
                            (2880.0, 200.0)
                        } else {
                            (100.0, 1800.0)
                        }
                    })
                    .collect();
                let area = Rectangle::new((60.0, 90.0).into(), (w, h).into());
                let slots = pack(&sizes, area);
                for (i, r) in slots.iter().enumerate() {
                    assert!(area.contains_rect(*r), "n={n}, area={area:?}, slot={r:?}");
                    assert!((r.size.w / r.size.h - sizes[i].0 / sizes[i].1).abs() < 1e-8);
                    for other in &slots[i + 1..] {
                        assert!(!r.overlaps(*other));
                    }
                }
            }
        }
    }
}
