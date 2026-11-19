//! Prepared desktop assets. Static frames only clone existing render handles.
use crate::{
    icons::Icons,
    render::GavnoRenderElement,
    topbar::{
        ellipsize,
        geometry::{rect, Rect},
        label_style, Canvas, LINE,
    },
    ui::text::{TextRenderer, TextStyleSpec, Weight},
};
use smithay::{
    backend::renderer::{
        element::{
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            Kind,
        },
        gles::{element::PixelShaderElement, GlesRenderer},
    },
    utils::{Logical, Point, Rectangle},
};
use std::collections::HashMap;

#[derive(Clone)]
struct Image {
    alpha: f32,
    buffer: MemoryRenderBuffer,
    physical: (i32, i32),
    logical: (i32, i32),
}
#[derive(Clone)]
struct Label {
    lines: Vec<Image>,
    used: u64,
}
#[derive(Clone)]
enum Primitive {
    Box(Rect, f32, [f32; 4], f32),
    Image(Rect, Image),
}
enum Drawing {
    Memory(MemoryRenderBufferRenderElement<GlesRenderer>),
    Shader(PixelShaderElement),
}
#[derive(Clone)]
pub struct CachedElement(std::sync::Arc<Drawing>);
impl CachedElement {
    pub fn from_ui(element: GavnoRenderElement) -> Option<Self> {
        match element {
            GavnoRenderElement::Memory(e) => Some(Self(std::sync::Arc::new(Drawing::Memory(e)))),
            GavnoRenderElement::Shader(e) => Some(Self(std::sync::Arc::new(Drawing::Shader(e)))),
            GavnoRenderElement::Desktop(e) => Some(e),
            _ => None,
        }
    }
    fn element(&self) -> &dyn smithay::backend::renderer::element::Element {
        match self.0.as_ref() {
            Drawing::Memory(e) => e,
            Drawing::Shader(e) => e,
        }
    }
}
impl smithay::backend::renderer::element::Element for CachedElement {
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        self.element().id()
    }
    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        self.element().current_commit()
    }
    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        self.element().src()
    }
    fn transform(&self) -> smithay::utils::Transform {
        self.element().transform()
    }
    fn geometry(
        &self,
        scale: smithay::utils::Scale<f64>,
    ) -> Rectangle<i32, smithay::utils::Physical> {
        self.element().geometry(scale)
    }
    fn damage_since(
        &self,
        scale: smithay::utils::Scale<f64>,
        commit: Option<smithay::backend::renderer::utils::CommitCounter>,
    ) -> smithay::backend::renderer::utils::DamageSet<i32, smithay::utils::Physical> {
        self.element().damage_since(scale, commit)
    }
    fn opaque_regions(
        &self,
        scale: smithay::utils::Scale<f64>,
    ) -> smithay::backend::renderer::utils::OpaqueRegions<i32, smithay::utils::Physical> {
        self.element().opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.element().alpha()
    }
    fn kind(&self) -> Kind {
        self.element().kind()
    }
}
impl smithay::backend::renderer::element::RenderElement<GlesRenderer> for CachedElement {
    fn draw(
        &self,
        frame: &mut smithay::backend::renderer::gles::GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, smithay::utils::Physical>,
        damage: &[Rectangle<i32, smithay::utils::Physical>],
        opaque: &[Rectangle<i32, smithay::utils::Physical>],
    ) -> Result<(), smithay::backend::renderer::gles::GlesError> {
        use smithay::backend::renderer::element::RenderElement;
        match self.0.as_ref() {
            Drawing::Memory(e) => e.draw(frame, src, dst, damage, opaque),
            Drawing::Shader(e) => {
                RenderElement::<GlesRenderer>::draw(e, frame, src, dst, damage, opaque)
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct LabelKey {
    value: String,
    size: u32,
    weight: Weight,
    color: [u32; 4],
    tracking: u32,
    width: i32,
    lines: usize,
}
#[derive(Default)]
pub struct Layer {
    canvas: Canvas,
    text: Option<TextRenderer>,
    icons: Icons,
    labels: HashMap<LabelKey, Label>,
    primitives: Vec<Primitive>,
    rendered: Vec<CachedElement>,
    scale: f64,
    key: Option<(u64, u64, u64)>,
    pub revision: u64,
    #[cfg(test)]
    pub preparations: usize,
}
impl Layer {
    pub fn begin(&mut self, scale: f64) {
        if self.scale != scale {
            self.labels.clear();
            self.icons = Icons::default();
            self.text = None;
            self.scale = scale;
        }
        self.primitives.clear();
        self.key = None;
        self.revision = self.revision.wrapping_add(1);
        #[cfg(test)]
        {
            self.preparations += 1;
        }
    }
    pub fn panel(&mut self, r: Rect, radius: f32, color: [f32; 4], border: f32) {
        self.primitives
            .push(Primitive::Box(r, radius, color, border));
    }
    pub fn tinted_icon(&mut self, name: &str, theme: &str, r: Rect, color: [u8; 3]) {
        self.tinted_icon_alpha(name, theme, r, color, 1.0);
    }
    pub fn tinted_icon_alpha(
        &mut self,
        name: &str,
        theme: &str,
        r: Rect,
        color: [u8; 3],
        alpha: f32,
    ) {
        let px = (r.size.w as f64 * self.scale).ceil() as u32;
        if let Some(buffer) = self.icons.get_tinted_color(name, px, theme, color) {
            self.primitives.push(Primitive::Image(
                r,
                Image {
                    alpha,
                    buffer,
                    physical: (px as i32, px as i32),
                    logical: (r.size.w, r.size.h),
                },
            ));
        }
    }
    pub fn icon(&mut self, name: &str, theme: &str, r: Rect) {
        let px = (r.size.w as f64 * self.scale).ceil() as u32;
        let buffer = self
            .icons
            .get(name, px, theme)
            .or_else(|| self.icons.get("application-x-executable", px, theme));
        if let Some(buffer) = buffer {
            self.primitives.push(Primitive::Image(
                r,
                Image {
                    alpha: 1.0,
                    buffer,
                    physical: (px as i32, px as i32),
                    logical: (r.size.w, r.size.h),
                },
            ));
        }
    }
    /// Reuse the shared, rounded hero-size music cover without decoding it here.
    pub fn decoded_image(&mut self, buffer: &MemoryRenderBuffer, r: Rect) {
        self.primitives.push(Primitive::Image(
            r,
            Image {
                alpha: 1.0,
                buffer: buffer.clone(),
                physical: {
                    let hero = crate::media::music::worker::CoverKind::Hero.size() as i32;
                    (hero, hero)
                },
                logical: (r.size.w, r.size.h),
            },
        ));
    }
    pub fn label(&mut self, value: &str, r: Rect, size: f32, lines: usize, center: bool) {
        self.styled_label(value, r, label_style(size), lines, center);
    }
    pub fn styled_label(
        &mut self,
        value: &str,
        r: Rect,
        style: TextStyleSpec,
        lines: usize,
        center: bool,
    ) {
        let key = LabelKey {
            value: value.to_owned(),
            size: style.size.to_bits(),
            weight: style.weight,
            color: style.color.map(f32::to_bits),
            tracking: style.tracking.to_bits(),
            width: r.size.w,
            lines,
        };
        if !self.labels.contains_key(&key) {
            if self.labels.len() >= 384 {
                self.labels.retain(|_, label| label.used == self.revision);
            }
            let text = self.text.get_or_insert_with(TextRenderer::new);
            let mut values = Vec::new();
            if lines > 1 && text.measure(value, style) > r.size.w as f32 {
                let mut split = 0;
                for (index, ch) in value.char_indices() {
                    if text.measure(&value[..index + ch.len_utf8()], style) <= r.size.w as f32 {
                        split = index + ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if let Some(space) = value[..split]
                    .rfind(char::is_whitespace)
                    .filter(|i| *i > split / 2)
                {
                    split = space;
                }
                if split > 0 {
                    values.push(value[..split].trim().to_owned());
                    values.push(ellipsize(text, value[split..].trim(), style, r.size.w));
                }
            }
            if values.is_empty() {
                values.push(ellipsize(text, value, style, r.size.w));
            }
            let lines = values
                .iter()
                .filter_map(|s| text.render(s, style, self.scale))
                .map(|i| Image {
                    alpha: 1.0,
                    buffer: i.buffer,
                    physical: (i.width, i.height),
                    logical: (i.logical_w, i.logical_h),
                })
                .collect();
            self.labels.insert(
                key.clone(),
                Label {
                    lines,
                    used: self.revision,
                },
            );
        }
        if let Some(label) = self.labels.get_mut(&key) {
            label.used = self.revision;
            for (i, image) in label.lines.iter().enumerate() {
                let x = if center {
                    r.loc.x + (r.size.w - image.logical.0) / 2
                } else {
                    r.loc.x
                };
                self.primitives.push(Primitive::Image(
                    rect(
                        x,
                        r.loc.y + i as i32 * (style.size.ceil() as i32 + 3),
                        image.logical.0,
                        image.logical.1,
                    ),
                    image.clone(),
                ));
            }
        }
    }
    pub fn append(
        &mut self,
        renderer: &mut GlesRenderer,
        out: &mut Vec<GavnoRenderElement>,
        progress: f64,
        anchor: Point<f64, Logical>,
    ) {
        let p = progress.clamp(0.0, 1.0);
        if p <= 0.001 {
            return;
        }
        if self.key.is_none() {
            self.labels.retain(|_, label| label.used == self.revision);
            if let Some(text) = &mut self.text {
                text.clear_images();
            }
        }
        let key = (self.revision, p.to_bits(), self.scale.to_bits());
        if self.key != Some(key) {
            self.canvas.begin(renderer, self.scale);
            self.canvas.alpha = p as f32;
            self.canvas.zoom = 0.96 + 0.04 * p;
            self.canvas.anchor = anchor;
            for primitive in &self.primitives {
                match primitive {
                    Primitive::Box(r, radius, color, border) => {
                        self.canvas.rect(*r, *radius, *color, *border, LINE)
                    }
                    Primitive::Image(r, image) => {
                        let r = self.canvas.mapped(*r);
                        if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                            renderer,
                            (r.loc.x as f64 * self.scale, r.loc.y as f64 * self.scale),
                            &image.buffer,
                            Some(p as f32 * image.alpha),
                            Some(Rectangle::from_size(
                                (image.physical.0 as f64, image.physical.1 as f64).into(),
                            )),
                            Some(r.size),
                            Kind::Unspecified,
                        ) {
                            self.canvas.elements.push(GavnoRenderElement::Memory(e));
                        }
                    }
                }
            }
            self.rendered = self
                .canvas
                .finish()
                .into_iter()
                .filter_map(|e| match e {
                    GavnoRenderElement::Memory(e) => {
                        Some(CachedElement(std::sync::Arc::new(Drawing::Memory(e))))
                    }
                    GavnoRenderElement::Shader(e) => {
                        Some(CachedElement(std::sync::Arc::new(Drawing::Shader(e))))
                    }
                    _ => None,
                })
                .collect();
            self.key = Some(key);
        }
        out.extend(
            self.rendered
                .iter()
                .cloned()
                .map(GavnoRenderElement::Desktop),
        );
    }
    #[cfg(test)]
    pub(crate) fn panels(&self) -> Vec<(Rect, [f32; 4])> {
        self.primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Box(r, _, c, _) => Some((*r, *c)),
                _ => None,
            })
            .collect()
    }
    #[cfg(test)]
    pub(crate) fn image_rects(&self) -> Vec<Rect> {
        self.primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Image(r, _) => Some(*r),
                _ => None,
            })
            .collect()
    }
    #[cfg(test)]
    pub fn image_bytes(&self) -> usize {
        self.labels
            .values()
            .flat_map(|l| &l.lines)
            .map(|i| (i.physical.0 * i.physical.1 * 4) as usize)
            .sum()
    }
}

#[cfg(test)]
pub(crate) mod performance {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };
    thread_local! { static COUNT:Cell<Option<usize>> = const { Cell::new(None) }; }
    struct TestAllocator;
    #[global_allocator]
    static ALLOCATOR: TestAllocator = TestAllocator;
    unsafe impl GlobalAlloc for TestAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = COUNT.try_with(|c| {
                if let Some(n) = c.get() {
                    c.set(Some(n + 1));
                }
            });
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let _ = COUNT.try_with(|c| {
                if let Some(n) = c.get() {
                    c.set(Some(n + 1));
                }
            });
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let _ = COUNT.try_with(|c| {
                if let Some(n) = c.get() {
                    c.set(Some(n + 1));
                }
            });
            unsafe { System.realloc(ptr, layout, size) }
        }
    }
    pub(crate) fn count(f: impl FnOnce()) -> usize {
        COUNT.with(|c| c.set(Some(0)));
        f();
        COUNT.with(|c| c.replace(None).unwrap_or(0))
    }
    pub(crate) fn rss_kib() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|line| line.starts_with("VmRSS:"))
                    .and_then(|line| line.split_whitespace().nth(1)?.parse().ok())
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod styled_label_tests {
    use super::*;
    #[test]
    fn label_cache_separates_styles_and_reuses_identical_styles() {
        let mut layer = Layer::default();
        layer.begin(1.0);
        let mut style = label_style(12.0);
        style.color = [1.0, 0.0, 0.0, 1.0];
        let area = rect(0, 0, 120, 20);
        layer.styled_label("Same text", area, style, 1, false);
        assert_eq!(layer.labels.len(), 1);
        style.color = [0.0, 1.0, 0.0, 1.0];
        layer.styled_label("Same text", area, style, 1, false);
        assert_eq!(layer.labels.len(), 2);
        layer.styled_label("Same text", area, style, 1, true);
        assert_eq!(layer.labels.len(), 2);
        style.weight = Weight::Regular;
        layer.styled_label("Same text", area, style, 1, false);
        assert_eq!(layer.labels.len(), 3);
        style.tracking = 0.8;
        layer.styled_label("Same text", area, style, 1, false);
        assert_eq!(layer.labels.len(), 4);
    }
}
