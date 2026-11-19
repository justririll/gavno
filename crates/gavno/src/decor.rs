//! Cached window decoration and rounded GLES content, with no work scheduled at rest.
use crate::{
    anim::{Rect, WindowAnimations},
    render::GavnoRenderElement,
    ui::shader::{self, ShaderBox, Shaders},
    window_mode::{mode_state, WindowMode},
};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                surface::{
                    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
                    WaylandSurfaceTexture,
                },
                texture::{TextureBuffer, TextureRenderElement},
                utils::{Relocate, RelocateRenderElement},
                AsRenderElements, Element, Id, Kind, RenderElement, UnderlyingStorage,
            },
            gles::{
                GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, GlesTarget,
                Uniform, UniformName, UniformType,
            },
            utils::{CommitCounter, DamageSet, OpaqueRegions},
            Bind, Offscreen,
        },
    },
    desktop::{layer_map_for_output, PopupManager, Space, Window},
    output::Output,
    utils::{Buffer, Physical, Rectangle, Scale, Transform},
    wayland::shell::wlr_layer::Layer,
};
use std::{collections::HashMap, sync::Arc, time::Duration};

/// Logical pixels. Maximized and fullscreen windows always have square corners.
pub const CORNER_RADIUS: f32 = 12.0;
/// Accent of the focus ring and glow. Local until the shared style module lands.
const ACCENT: [f32; 3] = [242.0 / 255.0, 164.0 / 255.0, 106.0 / 255.0];
/// Focus ring thickness in logical px and glow reach in logical px.
const RING_WIDTH: f32 = 2.0;
const GLOW_BLUR: i32 = 14;
const GLOW_ALPHA: f32 = 0.5;
/// Cross-fade between the old and new focused window.
pub const FOCUS_FADE: Duration = Duration::from_millis(120);

/// Focus emphasis settings, copied from `[appearance]` every frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FocusStyle {
    pub dim: f32,
    pub border: bool,
    /// The living theme's accent, so the ring matches the bar and panels.
    pub accent: [f32; 3],
}
impl Default for FocusStyle {
    fn default() -> Self {
        Self { dim: 0.06, border: true, accent: ACCENT }
    }
}

/// What one window draws for focus: ring/glow strength and dimming, both 0..1
/// already scaled by the cross-fade. All zero draws nothing at all.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FocusDraw {
    ring: f32,
    dim: f32,
}
impl FocusDraw {
    /// `ssd`: Classic title bars show focus themselves. `any_focused`: with no
    /// focused window (a layer surface has the keyboard) nothing is dimmed.
    fn new(style: FocusStyle, mode: WindowMode, ssd: bool, level: f32, any_focused: bool) -> Self {
        if ssd || mode == WindowMode::Fullscreen {
            return Self::default();
        }
        Self {
            ring: if style.border { level } else { 0.0 },
            dim: if any_focused { style.dim.clamp(0.0, 1.0) * (1.0 - level) } else { 0.0 },
        }
    }
}

/// Keyboard-focus cross-fade state. A window that has no entry is settled:
/// focused means 1.0, anything else 0.0, so the settled state stores nothing
/// and schedules no frames.
#[derive(Debug)]
pub struct FocusFade<K> {
    focused: Option<K>,
    fades: Vec<Fade<K>>,
    now: Duration,
}
#[derive(Debug)]
struct Fade<K> {
    key: K,
    from: f32,
    to: f32,
    /// Set by the first frame that draws the fade, so it always starts at `from`.
    start: Option<Duration>,
}
impl<K> Default for FocusFade<K> {
    fn default() -> Self {
        Self { focused: None, fades: Vec::new(), now: Duration::ZERO }
    }
}
impl<K: PartialEq + Clone> FocusFade<K> {
    pub fn focused(&self) -> Option<&K> {
        self.focused.as_ref()
    }
    /// Returns whether the focus actually changed. Fades in flight continue
    /// from their current level, so quick switches never jump.
    pub fn set(&mut self, key: Option<K>) -> bool {
        if self.focused == key {
            return false;
        }
        let old = self.focused.clone();
        let starts: Vec<(K, f32, f32)> = [(old, 0.0), (key.clone(), 1.0)]
            .into_iter()
            .filter_map(|(k, to)| k.map(|k| (self.level(&k, self.now), to, k)))
            .map(|(from, to, k)| (k, from, to))
            .collect();
        for (k, from, to) in starts {
            self.fades.retain(|f| f.key != k);
            if from != to {
                self.fades.push(Fade { key: k, from, to, start: None });
            }
        }
        self.focused = key;
        true
    }
    pub fn level(&self, key: &K, now: Duration) -> f32 {
        match self.fades.iter().find(|f| &f.key == key) {
            Some(f) => match f.start {
                None => f.from,
                Some(start) => {
                    let t = (now.saturating_sub(start).as_secs_f32() / FOCUS_FADE.as_secs_f32()).clamp(0.0, 1.0);
                    f.from + (f.to - f.from) * (1.0 - (1.0 - t) * (1.0 - t))
                }
            },
            None => f32::from(self.focused.as_ref() == Some(key)),
        }
    }
    /// Frame time: starts new fades and settles finished ones.
    pub fn advance(&mut self, now: Duration) {
        self.now = self.now.max(now);
        for f in &mut self.fades {
            f.start.get_or_insert(now);
        }
        self.fades.retain(|f| f.start.is_some_and(|s| now < s + FOCUS_FADE));
    }
    /// A fade is waiting for its first frame (the focus change itself queues one).
    #[cfg(test)]
    pub fn pending(&self) -> bool {
        self.fades.iter().any(|f| f.start.is_none())
    }
    /// A started fade needs the next frame. A fade that has not drawn yet does
    /// not: the focus change already queued the frame that starts it.
    pub fn active(&self) -> bool {
        self.fades.iter().any(|f| f.start.is_some())
    }
}
pub fn corner_radius(mode: WindowMode) -> f32 {
    if mode == WindowMode::Floating {
        CORNER_RADIUS
    } else {
        0.0
    }
}

// tex_matrix is Smithay's own vertex uniform. Inverting its affine 2x2 part
// recovers destination-local physical pixels, even with transformed buffers,
// viewporter crops, fractional scaling, external textures and Y inversion.
const MASK: &str = r#"
precision highp float;
uniform mat3 tex_matrix;
uniform float alpha;
uniform float opacity;
uniform vec4 clip_rect;
uniform float radius;
varying vec2 v_coords;
float coverage() {
    if (radius <= 0.0) return 1.0;
    vec2 uv = v_coords - tex_matrix[2].xy;
    vec2 a = tex_matrix[0].xy, b = tex_matrix[1].xy;
    float det = a.x*b.y-b.x*a.y;
    vec2 p = vec2(b.y*uv.x-b.x*uv.y, a.x*uv.y-a.y*uv.x)/det;
    vec2 half_size = clip_rect.zw*0.5;
    float r = min(radius,min(half_size.x,half_size.y));
    vec2 q = abs(p-clip_rect.xy-half_size)-half_size+r;
    float d = length(max(q,0.0))+min(max(q.x,q.y),0.0)-r;
    return clamp(0.5-d,0.0,1.0);
}
"#;
const TEXTURE: &str = r#"
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
"#;
const SAMPLE: &str = r#"
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
void main() {
    vec4 color = texture2D(tex,v_coords);
#if defined(NO_ALPHA)
    color.a = 1.0;
#endif
#if defined(DEBUG_FLAGS)
    if (tint == 1.0) color = vec4(0.0,0.2,0.0,0.2)+color*0.8;
#endif
    gl_FragColor = color * (alpha * opacity * coverage());
}
"#;
#[derive(Clone)]
struct Programs {
    texture: GlesTexProgram,
    solid: GlesPixelProgram,
}
impl Programs {
    fn compile(renderer: &mut GlesRenderer) -> Result<Self, GlesError> {
        let _startup = crate::perf::Startup::new("decor.programs.begin", "decor.programs.end");
        let u = || {
            vec![
                UniformName::new("opacity", UniformType::_1f),
                UniformName::new("clip_rect", UniformType::_4f),
                UniformName::new("radius", UniformType::_1f),
            ]
        };
        let texture =
            renderer.compile_custom_texture_shader(format!("{TEXTURE}{MASK}{SAMPLE}"), &u())?;
        crate::perf::startup("decor.texture_shader.ready");
        let mut uniforms = u();
        uniforms.push(UniformName::new("solid_color", UniformType::_4f));
        let solid=renderer.compile_custom_pixel_shader(format!("{MASK}\nuniform vec4 solid_color;\nvoid main() {{ gl_FragColor=solid_color*(alpha*opacity*coverage()); }}"),&uniforms)?;
        crate::perf::startup("decor.solid_shader.ready");
        Ok(Self { texture, solid })
    }
}

#[derive(Clone, Debug)]
enum Content {
    Surface(Arc<WaylandSurfaceRenderElement<GlesRenderer>>),
    Texture(Arc<TextureRenderElement<GlesTexture>>),
}
impl Content {
    fn element(&self) -> &dyn Element {
        match self {
            Self::Surface(e) => e.as_ref(),
            Self::Texture(e) => e.as_ref(),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Appearance {
    geometry: Rectangle<i32, Physical>,
    clip: Rectangle<i32, Physical>,
    radius: f32,
    opacity: f32,
}
#[derive(Clone, Debug)]
struct Revision {
    commit: CommitCounter,
    source: CommitCounter,
    appearance: Appearance,
}
#[derive(Default)]
struct Tracker {
    history: Vec<Revision>,
    serial: usize,
}
impl Tracker {
    fn update(&mut self, source: CommitCounter, appearance: Appearance) -> Vec<Revision> {
        if !self
            .history
            .last()
            .is_some_and(|r| r.source == source && r.appearance == appearance)
        {
            self.serial = self.serial.wrapping_add(1);
            self.history.push(Revision {
                commit: self.serial.into(),
                source,
                appearance,
            });
            if self.history.len() > 8 {
                self.history.remove(0);
            }
        }
        self.history.clone()
    }
}

/// Preserves the surface ID for frame callbacks, but prevents scanout whenever
/// content is rounded, translucent or transformed. Coordinates are output-local.
#[derive(Clone)]
pub struct WindowElement {
    content: Content,
    programs: Programs,
    appearance: Appearance,
    history: Vec<Revision>,
    scale: Scale<f64>,
}
impl std::fmt::Debug for WindowElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowElement")
            .field("appearance", &self.appearance)
            .finish()
    }
}
impl Element for WindowElement {
    fn id(&self) -> &Id {
        self.content.element().id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.history.last().map(|r| r.commit).unwrap_or_default()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.content.element().src()
    }
    fn transform(&self) -> Transform {
        self.content.element().transform()
    }
    fn geometry(&self, _: Scale<f64>) -> Rectangle<i32, Physical> {
        self.appearance.geometry
    }
    fn alpha(&self) -> f32 {
        self.content.element().alpha() * self.appearance.opacity
    }
    fn kind(&self) -> Kind {
        self.content.element().kind()
    }
    fn damage_since(
        &self,
        _: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.current_commit()) {
            return DamageSet::default();
        }
        if let Some(old) = self
            .history
            .iter()
            .find(|r| Some(r.commit) == commit && r.appearance == self.appearance)
        {
            let geo = self.content.element().geometry(self.scale);
            let ratio = Scale::from((
                self.appearance.geometry.size.w as f64 / geo.size.w.max(1) as f64,
                self.appearance.geometry.size.h as f64 / geo.size.h.max(1) as f64,
            ));
            return self
                .content
                .element()
                .damage_since(self.scale, Some(old.source))
                .into_iter()
                .map(|d| d.to_f64().upscale(ratio).to_i32_up())
                .collect();
        }
        DamageSet::from_slice(&[Rectangle::from_size(self.appearance.geometry.size)])
    }
    fn opaque_regions(&self, _: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.alpha() < 1.0 {
            return OpaqueRegions::default();
        }
        let source = self.content.element().geometry(self.scale);
        let target = self.appearance.geometry;
        let mut regions = Vec::new();
        for rect in self.content.element().opaque_regions(self.scale) {
            let mut rect = rect;
            rect.loc += source.loc;
            let rect = transformed_rect(rect, source, target);
            if self.appearance.radius <= 0.0 {
                let mut rect = rect;
                rect.loc -= target.loc;
                regions.push(rect);
                continue;
            }
            // Keep the antialiased corner squares out of opaque coverage, while
            // allowing lower windows to be culled behind the opaque interior.
            let clip = self.appearance.clip;
            let inset =
                (self.appearance.radius.ceil() as i32 + 1).min(clip.size.w.min(clip.size.h) / 2);
            let strips = [
                Rectangle::new(
                    (clip.loc.x + inset, clip.loc.y).into(),
                    (clip.size.w - 2 * inset, clip.size.h).into(),
                ),
                Rectangle::new(
                    (clip.loc.x, clip.loc.y + inset).into(),
                    (clip.size.w, clip.size.h - 2 * inset).into(),
                ),
            ];
            for strip in strips {
                if let Some(mut intersection) = rect.intersection(strip) {
                    intersection.loc -= target.loc;
                    regions.push(intersection);
                }
            }
        }
        regions.into_iter().collect()
    }
}
impl WindowElement {
    fn plain(&self) -> bool {
        self.appearance.radius == 0.0
            && self.appearance.opacity == 1.0
            && self.appearance.geometry == self.content.element().geometry(self.scale)
    }
}
impl RenderElement<GlesRenderer> for WindowElement {
    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        if !self.plain() {
            return None;
        }
        match &self.content {
            Content::Surface(e) => e.underlying_storage(renderer),
            Content::Texture(e) => e.underlying_storage(renderer),
        }
    }
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        let a = &self.appearance;
        let sx = dst.size.w as f32 / a.geometry.size.w.max(1) as f32;
        let sy = dst.size.h as f32 / a.geometry.size.h.max(1) as f32;
        let mut uniforms = vec![
            Uniform::new("opacity", a.opacity),
            Uniform::new("radius", a.radius * sx.min(sy)),
            Uniform::new(
                "clip_rect",
                [
                    (a.clip.loc.x as f32 - a.geometry.loc.x as f32) * sx,
                    (a.clip.loc.y as f32 - a.geometry.loc.y as f32) * sy,
                    a.clip.size.w as f32 * sx,
                    a.clip.size.h as f32 * sy,
                ],
            ),
        ];
        if let Content::Surface(e) = &self.content {
            if let WaylandSurfaceTexture::SolidColor(color) = e.texture() {
                uniforms.push(Uniform::new("solid_color", color.components()));
                return frame.render_pixel_shader_to(
                    &self.programs.solid,
                    Rectangle::from_size((1.0, 1.0).into()),
                    dst,
                    (1, 1).into(),
                    Some(damage),
                    e.alpha(),
                    &uniforms,
                );
            }
        }
        if self.plain() {
            return match &self.content {
                Content::Surface(e) => e.draw(frame, src, dst, damage, opaque),
                Content::Texture(e) => {
                    RenderElement::<GlesRenderer>::draw(e.as_ref(), frame, src, dst, damage, opaque)
                }
            };
        }
        frame.override_default_tex_program(self.programs.texture.clone(), uniforms);
        let result = match &self.content {
            Content::Surface(e) => e.draw(frame, src, dst, damage, &[]),
            Content::Texture(e) => {
                RenderElement::<GlesRenderer>::draw(e.as_ref(), frame, src, dst, damage, &[])
            }
        };
        frame.clear_tex_program_override();
        result
    }
}

#[derive(Default)]
struct Cache {
    last: Vec<WindowElement>,
    rect: Option<Rect>,
    mode: WindowMode,
    focused: bool,
    visibility: f64,
    window_scale: f64,
    snapshot: Option<(TextureBuffer<GlesTexture>, Rectangle<i32, Physical>)>,
    trackers: HashMap<Id, Tracker>,
    border: ShaderBox,
    ring: ShaderBox,
    glow: ShaderBox,
    dim: ShaderBox,
    draw: FocusDraw,
    shadows: [ShaderBox; 2],
    titlebar: crate::topbar::Canvas,
    icons: crate::icons::Icons,
}
struct WindowCache {
    window: Window,
    output: Output,
    cache: Cache,
}
#[derive(Default)]
pub struct WindowDecor {
    programs: Option<Programs>,
    pub(crate) warm_on_output: bool,
    shaders: Option<Shaders>,
    failed: bool,
    caches: Vec<WindowCache>,
    dropdown_caches: Vec<(Output, Cache)>,
    stack: Vec<Window>,
    fade: FocusFade<Window>,
    pub style: FocusStyle,
    pub classic: bool,
    pub pointer: smithay::utils::Point<f64, smithay::utils::Logical>,
}

fn transformed_rect(
    rect: Rectangle<i32, Physical>,
    source: Rectangle<i32, Physical>,
    target: Rectangle<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let sx = target.size.w as f64 / source.size.w.max(1) as f64;
    let sy = target.size.h as f64 / source.size.h.max(1) as f64;
    Rectangle::new(
        (
            target.loc.x as f64 + (rect.loc.x as f64 - source.loc.x as f64) * sx,
            target.loc.y as f64 + (rect.loc.y as f64 - source.loc.y as f64) * sy,
        )
            .into(),
        (rect.size.w as f64 * sx, rect.size.h as f64 * sy).into(),
    )
    .to_i32_round()
}
fn visual_rect(rect: Rect, scale: f64) -> Rect {
    let size = rect.size * scale;
    Rectangle::new(
        (
            rect.loc.x + (rect.size.w - size.w) * 0.5,
            rect.loc.y + (rect.size.h - size.h) * 0.5,
        )
            .into(),
        size,
    )
}
fn wrap(
    cache: &mut Cache,
    programs: &Programs,
    content: Content,
    appearance: Appearance,
    scale: Scale<f64>,
) -> WindowElement {
    let history = cache
        .trackers
        .entry(content.element().id().clone())
        .or_default()
        .update(content.element().current_commit(), appearance.clone());
    WindowElement {
        content,
        programs: programs.clone(),
        appearance,
        history,
        scale,
    }
}

/// The expensive copy happens once, after destruction, using retained render
/// elements whose imported textures/buffers outlive the Wayland resources.
/// This captures the last composed frame rather than re-reading a dead surface.
fn snapshot(
    renderer: &mut GlesRenderer,
    cache: &Cache,
    scale: Scale<f64>,
) -> Result<Option<(TextureBuffer<GlesTexture>, Rectangle<i32, Physical>)>, GlesError> {
    let Some(bounds) = cache
        .last
        .iter()
        .map(|e| e.geometry(scale))
        .reduce(|a, b| a.merge(b))
    else {
        return Ok(None);
    };
    if bounds.size.is_empty() {
        return Ok(None);
    }
    let mut texture: GlesTexture =
        renderer.create_buffer(Fourcc::Abgr8888, (bounds.size.w, bounds.size.h).into())?;
    let elems: Vec<_> = cache
        .last
        .iter()
        .cloned()
        .map(|e| {
            RelocateRenderElement::from_element(
                e,
                (-bounds.loc.x, -bounds.loc.y),
                Relocate::Relative,
            )
        })
        .collect();
    {
        let mut fb = renderer.bind(&mut texture)?;
        let mut tracker = OutputDamageTracker::new(bounds.size, 1.0, Transform::Normal);
        tracker
            .render_output(renderer, &mut fb, 0, &elems, [0.0; 4])
            .map_err(|e| match e {
                smithay::backend::renderer::damage::Error::Rendering(e) => e,
                _ => GlesError::UnknownSize,
            })?;
    }
    Ok(Some((
        TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None),
        bounds,
    )))
}

/// GLSL compilation alone can leave driver JIT work until the first draw.
/// Exercise rounded content, border and shadow once on a tiny throwaway target
/// while initializing chrome, before an application's first buffer can arrive.
fn warm_window_elements(renderer: &mut GlesRenderer, programs: &Programs, shaders: &Shaders) -> Result<Vec<GavnoRenderElement>, GlesError> {
    let mut cache = Cache::default();
    let mut elements = Vec::new();
    // Client window buffers normally have non-power-of-two dimensions. Some
    // drivers specialize sampling code for that case even with the same format.
    for (x, format) in [(8, Fourcc::Argb8888), (40, Fourcc::Xrgb8888)] {
        let buffer = TextureBuffer::from_memory(renderer, &[255; 36], format,
            (3, 3), false, 1, Transform::Normal, None)?;
        let texture = Arc::new(TextureRenderElement::from_texture_buffer((0.0, 0.0), &buffer,
            None, None, None, Kind::Unspecified));
        let rect = Rectangle::new((x, 8).into(), (16, 16).into());
        let content = wrap(&mut cache, programs, Content::Texture(texture), Appearance {
            geometry: rect, clip: rect, radius: 4.0, opacity: 1.0,
        }, Scale::from(1.0));
        let (border, shadows) = decorations(&mut cache, shaders, rect.to_f64().to_logical(1.0),
            WindowMode::Floating, true, FocusDraw { ring: 1.0, dim: 0.1 }, ACCENT, 1.0, 1.0);
        elements.extend(border);
        elements.push(GavnoRenderElement::Window(content));
        elements.extend(shadows);
    }
    Ok(elements)
}

fn draw_warm_window_elements(renderer: &mut GlesRenderer, fb: &mut GlesTarget<'_>, size: smithay::utils::Size<i32, Physical>, elements: &[GavnoRenderElement]) -> Result<(), GlesError> {
    OutputDamageTracker::new(size, 1.0, Transform::Normal)
        .render_output(renderer, fb, 0, elements, [0.0; 4])
        .map_err(|error| match error {
            smithay::backend::renderer::damage::Error::Rendering(error) => error,
            _ => GlesError::UnknownSize,
        })?;
    Ok(())
}

fn warm_window_programs(renderer: &mut GlesRenderer, programs: &Programs, shaders: &Shaders) -> Result<(), GlesError> {
    let _startup = crate::perf::Startup::new("decor.warmup.begin", "decor.warmup.end");
    let elements = warm_window_elements(renderer, programs, shaders)?;
    let mut target: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, (64, 32).into())?;
    let mut fb = renderer.bind(&mut target)?;
    draw_warm_window_elements(renderer, &mut fb, (64, 32).into(), &elements)
}

fn decorations(
    cache: &mut Cache,
    shaders: &Shaders,
    rect: Rect,
    mode: WindowMode,
    focused: bool,
    focus: FocusDraw,
    accent: [f32; 3],
    opacity: f32,
    scale: f64,
) -> (Vec<GavnoRenderElement>, Vec<GavnoRenderElement>) {
    let rect = rect.to_i32_round();
    let mut front = Vec::new();
    let mut behind = Vec::new();
    let radius = corner_radius(mode);
    if focus.ring > 0.0 {
        front.push(GavnoRenderElement::Shader(shader::rrect(
            &mut cache.ring,
            shaders,
            rect,
            radius * scale as f32,
            [0.0; 4],
            RING_WIDTH * scale as f32,
            [accent[0], accent[1], accent[2], 1.0],
            opacity * focus.ring,
            scale,
        )));
        behind.push(GavnoRenderElement::Shader(shader::shadow(
            &mut cache.glow,
            shaders,
            rect,
            radius,
            GLOW_BLUR,
            0,
            [accent[0], accent[1], accent[2], GLOW_ALPHA],
            opacity * focus.ring,
            scale,
        )));
    }
    if focus.dim > 0.0 {
        front.push(GavnoRenderElement::Shader(shader::rrect(
            &mut cache.dim,
            shaders,
            rect,
            radius * scale as f32,
            [0.0, 0.0, 0.0, 1.0],
            0.0,
            [0.0; 4],
            opacity * focus.dim,
            scale,
        )));
    }
    if mode != WindowMode::Floating {
        return (front, behind);
    }
    front.push(GavnoRenderElement::Shader(shader::rrect(
        &mut cache.border,
        shaders,
        rect,
        CORNER_RADIUS * scale as f32,
        [0.0; 4],
        scale as f32,
        [1.0, 1.0, 1.0, 0.12],
        opacity,
        scale,
    )));
    // The mockup's two shadow layers, including negative spread.
    let specs = if focused {
        [(80, 34, 18, 0.50), (26, 10, 8, 0.36)]
    } else {
        [(54, 22, 18, 0.32), (16, 6, 6, 0.24)]
    };
    for (sb, (blur, offset, spread, alpha)) in cache.shadows.iter_mut().zip(specs) {
        let inset = spread.min((rect.size.w.min(rect.size.h) - 1).max(0) / 2);
        let smaller = Rectangle::new(
            (rect.loc.x + inset, rect.loc.y + inset).into(),
            (rect.size.w - 2 * inset, rect.size.h - 2 * inset).into(),
        );
        behind.push(GavnoRenderElement::Shader(shader::shadow(
            sb,
            shaders,
            smaller,
            CORNER_RADIUS,
            blur,
            offset,
            [0.0, 0.0, 0.0, alpha],
            opacity,
            scale,
        )));
    }
    (front, behind)
}

impl WindowDecor {
    /// Window-system framebuffer formats can select a different driver pipeline
    /// from an offscreen texture. The backend overwrites this entire warmup draw
    /// before submitting any pixels to the host.
    pub(crate) fn warm_framebuffer(&self, renderer: &mut GlesRenderer, fb: &mut GlesTarget<'_>, size: smithay::utils::Size<i32, Physical>) -> Result<bool, GlesError> {
        let (Some(programs), Some(shaders)) = (&self.programs, &self.shaders) else { return Ok(false) };
        let _startup = crate::perf::Startup::new("decor.output_warmup.begin", "decor.output_warmup.end");
        let elements = warm_window_elements(renderer, programs, shaders)?;
        draw_warm_window_elements(renderer, fb, size, &elements)?;
        Ok(true)
    }
    fn ensure_programs(&mut self, renderer: &mut GlesRenderer) {
        if self.programs.is_none() && !self.failed {
            match Programs::compile(renderer)
                .and_then(|p| Shaders::compile(renderer).map(|s| (p, s)))
            {
                Ok((p, s)) => {
                    if !self.warm_on_output {
                        if let Err(error) = warm_window_programs(renderer, &p, &s) {
                            tracing::debug!(?error, "window shader warmup unavailable");
                        }
                    }
                    self.programs = Some(p);
                    self.shaders = Some(s);
                }
                Err(err) => {
                    tracing::warn!(?err, "window decoration shaders unavailable");
                    self.failed = true;
                }
            }
        }
    }
    /// The dropdown terminal: `rect` (output-local logical px) is where its
    /// window geometry is drawn. Only the bottom corners are rounded, the top
    /// edge is flush with the work area, and one soft shadow falls below it.
    pub(crate) fn dropdown_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        window: &Window,
        rect: Rectangle<i32, smithay::utils::Logical>,
    ) -> Vec<GavnoRenderElement> {
        self.ensure_programs(renderer);
        let scale = Scale::from(output.current_scale().fractional_scale());
        let Some(toplevel) = window.toplevel() else { return Vec::new() };
        let origin = (rect.loc - window.geometry().loc).to_f64().to_physical(scale).to_i32_round();
        let (Some(programs), Some(shaders)) = (&self.programs, &self.shaders) else {
            return window
                .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(renderer, origin, scale, 1.0)
                .into_iter()
                .map(GavnoRenderElement::Surface)
                .collect();
        };
        let index = match self.dropdown_caches.iter().position(|(o, _)| o == output) {
            Some(i) => i,
            None => {
                self.dropdown_caches.push((output.clone(), Cache::default()));
                self.dropdown_caches.len() - 1
            }
        };
        let cache = &mut self.dropdown_caches[index].1;
        let target = rect.to_f64().to_physical(scale).to_i32_round::<i32>();
        // Rounding is centred on a rectangle that reaches one radius above the
        // top edge, so only the bottom corners show it.
        let radius = CORNER_RADIUS * scale.x as f32;
        let lift = radius.ceil() as i32 + 1;
        let clip = Rectangle::new((target.loc.x, target.loc.y - lift).into(), (target.size.w, target.size.h + lift).into());
        let mut content = Vec::new();
        let mut popups = Vec::new();
        for (popup, offset) in PopupManager::popups_for_surface(toplevel.wl_surface()) {
            let offset = (window.geometry().loc + offset - popup.geometry().loc).to_physical_precise_round(scale);
            for e in render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<_>>(
                renderer, popup.wl_surface(), origin + offset, scale, 1.0, Kind::Unspecified,
            ) {
                let geometry = e.geometry(scale);
                popups.push(wrap(cache, programs, Content::Surface(Arc::new(e)),
                    Appearance { geometry, clip: target, radius: 0.0, opacity: 1.0 }, scale));
            }
        }
        let root_id = Id::from(toplevel.wl_surface());
        for e in render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<_>>(
            renderer, toplevel.wl_surface(), origin, scale, 1.0, Kind::Unspecified,
        ) {
            let geometry = e.geometry(scale);
            let round = e.id() == &root_id || target.contains_rect(geometry);
            content.push(wrap(cache, programs, Content::Surface(Arc::new(e)),
                Appearance { geometry, clip, radius: if round { radius } else { 0.0 }, opacity: 1.0 }, scale));
        }
        cache.trackers.retain(|id, _| content.iter().chain(&popups).any(|e| e.id() == id));
        let mut result: Vec<GavnoRenderElement> = popups.into_iter().chain(content).map(GavnoRenderElement::Window).collect();
        // Shadow rectangle reaches above the work area so its top corners stay hidden.
        let above = 2 * CORNER_RADIUS as i32;
        let shadow_rect: Rectangle<i32, smithay::utils::Logical> = Rectangle::new((rect.loc.x, rect.loc.y - above).into(), (rect.size.w, rect.size.h + above).into());
        for (sb, (blur, offset, spread, alpha)) in cache.shadows.iter_mut().zip([(80, 34, 18, 0.50), (26, 10, 8, 0.36)]) {
            let inset = spread.min((shadow_rect.size.w.min(shadow_rect.size.h) - 1).max(0) / 2);
            let smaller: Rectangle<i32, smithay::utils::Logical> = Rectangle::new(
                (shadow_rect.loc.x + inset, shadow_rect.loc.y).into(),
                (shadow_rect.size.w - 2 * inset, shadow_rect.size.h - inset).into(),
            );
            result.push(GavnoRenderElement::Shader(shader::shadow(
                sb, shaders, smaller, CORNER_RADIUS, blur, offset, [0.0, 0.0, 0.0, alpha], 1.0, scale.x,
            )));
        }
        result
    }
    /// The dropdown is not drawn on `output`: drop its retained elements.
    pub(crate) fn forget_dropdown(&mut self, output: &Output) {
        if !self.dropdown_caches.is_empty() { self.dropdown_caches.retain(|(o, _)| o != output); }
    }
    pub fn focus(&mut self, window: Option<&Window>) {
        self.fade.set(window.cloned());
    }
    pub fn focused(&self) -> Option<&Window> {
        self.fade.focused()
    }
    #[cfg(test)]
    pub(crate) fn advance_focus(&mut self, now: Duration) {
        self.fade.advance(now);
    }
    /// True while the started focus cross-fade needs frames.
    pub fn fading(&self) -> bool {
        self.fade.active()
    }
    /// Front-to-back layer order matches Smithay's space renderer, including popups.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        space: &Space<Window>,
        output: &Output,
        animations: &mut WindowAnimations,
        time: Duration,
        scene: Option<&[crate::overview::SceneWindow]>,
        features: &crate::module::Features,
        mut stall: Option<&mut crate::perf::Stall<'_>>,
    ) -> Vec<GavnoRenderElement> {
        let accent = self.style.accent;

        self.caches.retain(|c| {
            (features.read(crate::module::Feature::Animations, None, || animations.motion(&c.window)).is_some() || space.element_location(&c.window).is_some())
                && space.outputs().any(|o| o == &c.output)
        });
        self.ensure_programs(renderer);
        self.fade.advance(time);
        let (Some(programs), Some(shaders)) = (&self.programs, &self.shaders) else {
            return crate::render::plain_elements(renderer, space, output, scene, stall);
        };
        let Some(output_geo) = space.output_geometry(output) else {
            return Vec::new();
        };
        let scale = Scale::from(output.current_scale().fractional_scale());
        let mut result = Vec::new();
        let layer_map = layer_map_for_output(output);
        let append_layers = |result: &mut Vec<_>, renderer: &mut GlesRenderer, upper: bool| {
            for layer in layer_map
                .layers()
                .rev()
                .filter(|l| matches!(l.layer(), Layer::Top | Layer::Overlay) == upper)
            {
                if let Some(geo) = layer_map.layer_geometry(layer) {
                    result.extend(
                        layer
                            .render_elements::<WaylandSurfaceRenderElement<_>>(
                                renderer,
                                geo.loc.to_physical_precise_round(scale),
                                scale,
                                1.0,
                            )
                            .into_iter()
                            .map(GavnoRenderElement::Surface),
                    );
                }
            }
        };
        if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.decor"); }
        append_layers(&mut result, renderer, true);
        if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.layers.front"); }
        // Closing windows retain their former stacking position relative to surviving
        // windows using the last space order captured below.
        let mut windows: Vec<Window> = scene.map(|s| s.iter().map(|v| v.window.clone()).collect())
            .unwrap_or_else(|| space.elements().cloned().collect());
        for a in animations.entries.iter().filter(|_| features.on(crate::module::Feature::Animations)) {
            if scene.is_none() && a.motion.closing && !windows.contains(&a.window) {
                let above = self
                    .stack
                    .iter()
                    .position(|w| w == &a.window)
                    .and_then(|i| {
                        self.stack[i + 1..]
                            .iter()
                            .find_map(|w| windows.iter().position(|v| v == w))
                    });
                windows.insert(above.unwrap_or(windows.len()), a.window.clone());
            }
        }
        self.stack = windows.clone();
        for window in windows.iter().rev() {
            let closing = features.read(crate::module::Feature::Animations, None, || animations.motion(window)).is_some_and(|m| m.closing);
            let index = if let Some(i) = self
                .caches
                .iter()
                .position(|c| &c.window == window && &c.output == output)
            {
                i
            } else {
                if closing {
                    continue;
                }
                self.caches.push(WindowCache {
                    window: window.clone(),
                    output: output.clone(),
                    cache: Cache::default(),
                });
                self.caches.len() - 1
            };
            let cache = &mut self.caches[index].cache;
            let motion = features.read(crate::module::Feature::Animations, None, || animations.motion(window));
            if closing {
                if cache.snapshot.is_none() {
                    match snapshot(renderer, cache, scale) {
                        Ok(s) => cache.snapshot = s,
                        Err(err) => tracing::warn!(?err, "window close snapshot failed"),
                    }
                    // Never retry an allocation every animation frame.
                    cache.last.clear();
                }
                let (Some((buffer, bounds)), Some(rect)) = (&cache.snapshot, cache.rect) else {
                    continue;
                };
                let source = rect.to_physical_precise_round(scale);
                let target = visual_rect(
                    rect,
                    motion.map(|m| m.scale()).unwrap_or(1.0) / cache.window_scale.max(0.0001),
                )
                .to_physical_precise_round(scale);
                let geo = transformed_rect(*bounds, source, target);
                let texture = Arc::new(TextureRenderElement::from_texture_buffer(
                    bounds.loc.to_f64(),
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                ));
                let opacity = motion
                    .map(|m| m.opacity() as f32)
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0);
                let texture_opacity =
                    (opacity / cache.visibility.max(0.0001) as f32).clamp(0.0, 1.0);
                let element = wrap(
                    cache,
                    programs,
                    Content::Texture(texture),
                    Appearance {
                        geometry: geo,
                        clip: target,
                        radius: 0.0,
                        opacity: texture_opacity,
                    },
                    scale,
                );
                let shown = target
                    .to_f64()
                    .to_logical(scale)
                    .to_i32_round::<i32>()
                    .to_f64();
                let (border, shadow) = decorations(
                    cache,
                    shaders,
                    shown,
                    cache.mode,
                    cache.focused,
                    cache.draw,
                    accent,
                    opacity,
                    scale.x,
                );
                result.extend(border);
                result.push(GavnoRenderElement::Window(element));
                result.extend(shadow);
                continue;
            }
            cache.snapshot = None;
            let visual = scene.and_then(|s| s.iter().find(|v| &v.window == window));
            let Some(loc) = visual.map(|v| v.location).or_else(|| space.element_location(window)) else {
                continue;
            };
            let rect = Rectangle::new(
                loc.to_f64() - output_geo.loc.to_f64(),
                window.geometry().size.to_f64(),
            );
            let animated = motion
                .map(|m| m.rect(Rectangle::new(loc, window.geometry().size).to_f64()))
                .unwrap_or(Rectangle::new(loc, window.geometry().size).to_f64());
            let animated = Rectangle::new(animated.loc - output_geo.loc.to_f64(), animated.size);
            let shown = visual.map(|v| v.rect).unwrap_or_else(|| visual_rect(animated, motion.map(|m| m.scale()).unwrap_or(1.0)));
            let bbox = window.bbox_with_popups().to_f64();
            let ratio = Scale::from((
                shown.size.w / rect.size.w.max(1.0),
                shown.size.h / rect.size.h.max(1.0),
            ));
            let bounds = Rectangle::new(
                shown.loc + (bbox.loc - window.geometry().loc.to_f64()).upscale(ratio),
                bbox.size.upscale(ratio),
            );
            // Include both shadow lobes. Off-output windows need no imports or
            // snapshots, but their springs still settle through advance() above.
            let visible = Rectangle::new(
                (-120.0, -120.0).into(),
                (
                    output_geo.size.w as f64 + 240.0,
                    output_geo.size.h as f64 + 240.0,
                )
                    .into(),
            );
            if !bounds.overlaps(visible) {
                cache.last.clear();
                cache.rect = None;
                continue;
            }
            let source = rect.to_physical_precise_round(scale);
            let target = shown.to_physical_precise_round(scale);
            let opacity = motion
                .map(|m| m.opacity() as f32)
                .unwrap_or(1.0)
                .clamp(0.0, 1.0);
            let mode = mode_state(window).mode;
            let focused = self.fade.focused() == Some(window);
            let ssd = self.classic && crate::classic::decor::ssd(window);
            let draw = FocusDraw::new(self.style, mode, ssd, self.fade.level(window, time), self.fade.focused().is_some());
            let radius = if ssd { 0.0 } else { corner_radius(mode) * scale.x as f32 };
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let origin = (loc.to_f64() - window.geometry().loc.to_f64() - output_geo.loc.to_f64())
                .to_physical(scale)
                .to_i32_round();
            let mut content = Vec::new();
            for (popup, offset) in PopupManager::popups_for_surface(toplevel.wl_surface()) {
                let offset = (window.geometry().loc + offset - popup.geometry().loc)
                    .to_physical_precise_round(scale);
                for e in render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<_>>(
                    renderer,
                    popup.wl_surface(),
                    origin + offset,
                    scale,
                    1.0,
                    Kind::Unspecified,
                ) {
                    let geo = transformed_rect(e.geometry(scale), source, target);
                    content.push(wrap(
                        cache,
                        programs,
                        Content::Surface(Arc::new(e)),
                        Appearance {
                            geometry: geo,
                            clip: target,
                            radius: 0.0,
                            opacity,
                        },
                        scale,
                    ));
                }
            }
            let popup_count = content.len();
            let root_id = Id::from(toplevel.wl_surface());
            for e in render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<_>>(
                renderer,
                toplevel.wl_surface(),
                origin,
                scale,
                1.0,
                Kind::Unspecified,
            ) {
                let original = e.geometry(scale);
                let geo = transformed_rect(original, source, target);
                // Popups and outlying subsurfaces are allowed outside the window.
                let round = e.id() == &root_id || source.contains_rect(original);
                content.push(wrap(
                    cache,
                    programs,
                    Content::Surface(Arc::new(e)),
                    Appearance {
                        geometry: geo,
                        clip: target,
                        radius: if round { radius } else { 0.0 },
                        opacity,
                    },
                    scale,
                ));
            }
            cache.rect = Some(shown);
            cache.mode = mode;
            cache.focused = focused;
            cache.draw = draw;
            cache.visibility = opacity as f64;
            cache.window_scale = motion.map(|m| m.scale()).unwrap_or(1.0);
            cache
                .trackers
                .retain(|id, _| content.iter().any(|e| e.id() == id));
            cache.last = content.clone();
            let (border, shadow) =
                decorations(cache, shaders, shown, mode, focused, draw, accent, opacity, scale.x);
            // Popups must remain above the parent's focus ring.
            result.extend(content.drain(..popup_count).map(GavnoRenderElement::Window));
            result.extend(border);
            if ssd && mode != WindowMode::Fullscreen {
                let r = crate::classic::decor::title_rect(shown.to_i32_round());
                let c = &mut cache.titlebar;
                c.begin(renderer, scale.x);
                c.alpha = opacity;
                c.rect(r, 0.0, [0.065, 0.075, 0.10, 1.0], 0.0, crate::topbar::LINE);
                let (title, app) = crate::overlay::window_labels(window);
                cache.icons.nonblocking_apps();
                // App IDs commonly double as themed icon names; use the same fallback as the dock.
                c.icon(renderer, &mut cache.icons, &[&app, "application-x-executable-symbolic"], "Adwaita", crate::topbar::geometry::rect(r.loc.x + 8, r.loc.y + 7, 18, 18));
                c.label(renderer, &title, r.loc.x + 34, r.loc.y + 9, r.size.w - 180, 12.0);
                let hover = crate::classic::decor::part(r, self.pointer - output_geo.loc.to_f64());
                for (i, (part, glyph)) in [(crate::classic::decor::Part::Minimize, "−"), (crate::classic::decor::Part::Maximize, if mode == WindowMode::Maximized { "❐" } else { "□" }), (crate::classic::decor::Part::Close, "×")].into_iter().enumerate() {
                    let button = crate::topbar::geometry::rect(r.loc.x + r.size.w - 138 + i as i32 * 46, r.loc.y, 46, 32);
                    if hover == Some(part) { c.rect(button, 0.0, if part == crate::classic::decor::Part::Close { [0.85, 0.12, 0.16, 1.0] } else { [1.0, 1.0, 1.0, 0.12] }, 0.0, crate::topbar::LINE); }
                    c.label(renderer, glyph, button.loc.x + 17, button.loc.y + 7, 24, 16.0);
                }
                result.extend(c.finish());
            }
            result.extend(content.into_iter().map(GavnoRenderElement::Window));
            result.extend(shadow);
        }

        if let Some(stall) = stall.as_deref_mut() { stall.mark("elements.space"); }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clearing_keyboard_focus_clears_decoration_and_window_reference() {
        let mut h = crate::state::tests::Harness::new();
        let window = h.window();
        h.gv.map_new_window(window.clone());
        assert_eq!(h.gv.window_decor.focused(), Some(&window));
        h.gv.focus_window(None);
        assert!(h.gv.seat.get_keyboard().unwrap().current_focus().is_none());
        assert!(h.gv.window_decor.focused().is_none());

        h.gv.focus_window(Some(&window));
        h.gv.space.unmap_elem(&window);
        h.gv.focus_topmost();
        assert!(h.gv.seat.get_keyboard().unwrap().current_focus().is_none());
        assert!(h.gv.window_decor.focused().is_none());
    }
    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }
    #[test]
    fn settled_focus_is_exact_and_costs_no_frames() {
        let mut f = FocusFade::<u32>::default();
        assert!(!f.active());
        assert_eq!(f.level(&1, ms(0)), 0.0);
        assert!(f.set(Some(1)));
        assert!(!f.set(Some(1)), "same focus is no change");
        assert!(f.pending() && !f.active(), "the focus change queues its own first frame");
        f.advance(ms(999));
        assert!(f.active());
        let mut f = FocusFade::<u32>::default();
        f.set(Some(1));
        assert_eq!(f.level(&1, ms(500)), 0.0, "a fade starts at its first frame, not at the focus event");
        f.advance(ms(1000));
        assert_eq!(f.level(&1, ms(1000)), 0.0);
        f.advance(ms(1060));
        let mid = f.level(&1, ms(1060));
        assert!(mid > 0.5 && mid < 1.0, "ease-out is past halfway at half time: {mid}");
        f.advance(ms(1120));
        assert!(!f.active());
        assert_eq!((f.level(&1, ms(1120)), f.level(&2, ms(1120))), (1.0, 0.0));
        assert_eq!(f.focused(), Some(&1));
    }
    #[test]
    fn crossfade_swaps_levels_and_switching_mid_fade_is_continuous() {
        let mut f = FocusFade::<u32>::default();
        f.set(Some(1));
        f.advance(ms(0));
        f.advance(ms(200));
        f.set(Some(2));
        f.advance(ms(1000));
        f.advance(ms(1030));
        let (a, b) = (f.level(&1, ms(1030)), f.level(&2, ms(1030)));
        assert!(a < 1.0 && a > 0.0 && b > 0.0 && b < 1.0);
        assert!((a + b - 1.0).abs() < 1e-5, "complementary during a clean cross-fade");
        // Switch to a third window half way: the fading-in window continues from its level.
        f.set(Some(3));
        assert_eq!(f.level(&2, ms(1030)), b);
        f.advance(ms(1040));
        f.advance(ms(2000));
        assert!(!f.active());
        assert_eq!((f.level(&1, ms(2000)), f.level(&2, ms(2000)), f.level(&3, ms(2000))), (0.0, 0.0, 1.0));
    }
    #[test]
    fn clearing_focus_fades_the_old_window_out() {
        let mut f = FocusFade::<u32>::default();
        f.set(Some(1));
        f.advance(ms(0));
        f.advance(ms(500));
        assert!(f.set(None));
        f.advance(ms(600));
        f.advance(ms(660));
        let level = f.level(&1, ms(660));
        assert!(level > 0.0 && level < 1.0);
        f.advance(ms(800));
        assert!(!f.active());
        assert_eq!(f.level(&1, ms(800)), 0.0);
    }
    #[test]
    fn focus_draw_exclusions() {
        let style = FocusStyle { dim: 0.1, border: true, accent: ACCENT };
        let on = FocusDraw::new(style, WindowMode::Floating, false, 1.0, true);
        assert_eq!(on, FocusDraw { ring: 1.0, dim: 0.0 });
        let off = FocusDraw::new(style, WindowMode::Maximized, false, 0.0, true);
        assert_eq!(off, FocusDraw { ring: 0.0, dim: 0.1 });
        let half = FocusDraw::new(style, WindowMode::Floating, false, 0.5, true);
        assert_eq!(half, FocusDraw { ring: 0.5, dim: 0.05 });
        let fullscreen = FocusDraw::new(style, WindowMode::Fullscreen, false, 1.0, true);
        assert_eq!(fullscreen, FocusDraw::default());
        assert_eq!(FocusDraw::new(style, WindowMode::Fullscreen, false, 0.0, true), FocusDraw::default());
        assert_eq!(FocusDraw::new(style, WindowMode::Floating, true, 0.0, true), FocusDraw::default(), "Classic title bars show focus");
        assert_eq!(FocusDraw::new(FocusStyle { dim: 0.0, border: true, accent: ACCENT }, WindowMode::Floating, false, 0.0, true), FocusDraw::default());
        assert_eq!(FocusDraw::new(FocusStyle { dim: 0.1, border: false, accent: ACCENT }, WindowMode::Floating, false, 1.0, true), FocusDraw::default());
        // A layer surface holds the keyboard: nobody is dimmed.
        assert_eq!(FocusDraw::new(style, WindowMode::Floating, false, 0.0, false), FocusDraw::default());
    }
    #[test]
    fn real_focus_changes_drive_the_fade_and_idle_cost() {
        let mut h = crate::state::tests::Harness::new();
        h.gv.config.modules.animations = false;
        h.gv.features = crate::module::Features::new(&h.gv.config);
        let a = h.window();
        h.gv.map_new_window(a.clone());
        let b = h.window();
        h.gv.map_new_window(b.clone());
        h.gv.window_decor.advance_focus(Duration::from_secs(10));
        h.gv.window_decor.advance_focus(Duration::from_secs(11));
        assert!(!h.gv.window_decor.fading());
        assert!(!h.gv.is_animating());
        h.gv.focus_window(Some(&a));
        assert!(!h.gv.is_animating(), "an unstarted fade relies on the redraw the focus change queued");
        h.gv.window_decor.advance_focus(Duration::from_secs(12));
        assert!(h.gv.window_decor.fading());
        assert!(h.gv.is_animating());
        h.gv.window_decor.advance_focus(Duration::from_secs(13));
        assert!(!h.gv.window_decor.fading());
        assert!(!h.gv.is_animating());
    }
    #[test]
    fn radius_follows_mode() {
        assert_eq!(corner_radius(WindowMode::Floating), 12.0);
        assert_eq!(corner_radius(WindowMode::Maximized), 0.0);
        assert_eq!(corner_radius(WindowMode::Fullscreen), 0.0);
    }
    #[test]
    fn still_elements_keep_their_commit() {
        let mut tracker = Tracker::default();
        let appearance = Appearance {
            geometry: Rectangle::new((0, 0).into(), (100, 100).into()),
            clip: Rectangle::new((0, 0).into(), (100, 100).into()),
            radius: 21.0,
            opacity: 1.0,
        };
        let first = tracker
            .update(0.into(), appearance.clone())
            .last()
            .unwrap()
            .commit;
        assert_eq!(
            tracker
                .update(0.into(), appearance.clone())
                .last()
                .unwrap()
                .commit,
            first
        );
        assert_ne!(
            tracker.update(1.into(), appearance).last().unwrap().commit,
            first
        );
    }
    #[test]
    fn scaling_preserves_window_center() {
        let rect = Rectangle::new((10.0, 20.0).into(), (100.0, 200.0).into());
        let small = visual_rect(rect, 0.94);
        assert_eq!(small.loc.x + small.size.w / 2.0, 60.0);
        assert_eq!(small.loc.y + small.size.h / 2.0, 120.0);
    }
}
