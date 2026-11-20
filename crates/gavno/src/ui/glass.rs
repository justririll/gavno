//! Liquid glass: the material of every GAVNO surface (bar, dock, taskbar,
//! popovers, cards).
//!
//! A glass surface shows the wallpaper behind it softly blurred, bent like a
//! lens near its rounded edges (with a hint of chromatic dispersion), lifted in
//! saturation and lit by a thin specular rim from the top left.
//!
//! The wallpaper rarely changes, so it is captured once per change at a third
//! of the output resolution and blurred on a worker thread; frames only sample
//! that small texture. While the background settles nothing is captured, and a
//! still screen costs nothing.
//!
//! Drawing code does not see any of this: `Canvas::glass` asks the glass of
//! the output being drawn ([`with_frame`]) for an element and falls back to a
//! flat translucent fill when there is none yet (first frames, tests).

use std::{
    cell::RefCell,
    collections::HashMap,
    hash::{Hash, Hasher},
    time::Duration,
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{Element, Id, Kind, RenderElement},
            gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType},
            utils::CommitCounter,
            ContextId, ImportMem, Renderer,
        },
    },
    reexports::calloop::{channel, RegistrationToken},
    utils::{Buffer, Logical, Physical, Rectangle, Scale, Size, Transform},
};

use crate::render::GavnoRenderElement;

/// Capture resolution relative to the output.
const CAPTURE_DIVISOR: f64 = 3.0;
/// Box blur of the small capture: radius and passes (three approximate a Gaussian).
const BLUR_RADIUS: usize = 3;
const BLUR_PASSES: usize = 3;
/// The background must hold still this long before it is captured again.
const SETTLE: Duration = Duration::from_millis(400);

const SHADER: &str = r#"
#version 100
//_DEFINES
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Source rectangle in texture coordinates (origin, size).
uniform vec4 src;
// Element size and corner radius, in physical px.
uniform vec2 size;
uniform float radius;
// Lens strength in physical px, the colour wash and its amount.
uniform float bend;
uniform vec4 wash;
// Specular rim brightness.
uniform float rim;

float sd_round_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

vec3 sample_at(vec2 uv) {
    return texture2D(tex, clamp(uv, vec2(0.0), vec2(1.0))).rgb;
}

void main() {
    vec2 local = (v_coords - src.xy) / src.zw * size;
    vec2 half_size = size * 0.5;
    vec2 p = local - half_size;
    float r = min(radius, min(half_size.x, half_size.y));
    float d = sd_round_box(p, half_size, r);
    float cover = clamp(0.5 - d, 0.0, 1.0);
    if (cover <= 0.0) {
        gl_FragColor = vec4(0.0);
        return;
    }

    // Outward normal of the rounded rectangle.
    vec2 e = vec2(1.0, 0.0);
    vec2 n = vec2(
        sd_round_box(p + e.xy, half_size, r) - sd_round_box(p - e.xy, half_size, r),
        sd_round_box(p + e.yx, half_size, r) - sd_round_box(p - e.yx, half_size, r));
    n = n / max(length(n), 1e-4);

    // A lens rim: the band near the edge bends the picture inwards, strongest
    // right at the edge, nothing in the flat middle.
    float band = max(min(r * 1.4 + 6.0, min(half_size.x, half_size.y)), 1.0);
    float edge = clamp(1.0 + d / band, 0.0, 1.0);
    float lens = edge * edge * edge;
    vec2 px = src.zw / size;
    vec2 shift = -n * lens * bend * px;
    vec2 uv = v_coords + shift;
    vec3 c;
    c.r = sample_at(uv + shift * 0.10).r;
    c.g = sample_at(uv).g;
    c.b = sample_at(uv - shift * 0.10).b;
    // A little extra softness around the sample.
    vec3 soft = sample_at(uv + vec2(px.x, 0.0) * 3.0) + sample_at(uv - vec2(px.x, 0.0) * 3.0)
        + sample_at(uv + vec2(0.0, px.y) * 3.0) + sample_at(uv - vec2(0.0, px.y) * 3.0);
    c = mix(c, soft * 0.25, 0.5);

    // Vivid glass: more saturation, a touch of light, then the wash.
    float luma = dot(c, vec3(0.2126, 0.7152, 0.0722));
    c = mix(vec3(luma), c, 1.35);
    c = c * 1.03 + 0.015;
    c = mix(c, wash.rgb, wash.a);

    // Specular rim, lit from the top left, a fainter one opposite.
    float line = clamp(1.0 - abs(d + 0.9) / 1.1, 0.0, 1.0);
    float lit = dot(n, normalize(vec2(-0.55, -0.85)));
    c += line * rim * (0.35 + 0.65 * max(lit, 0.0));
    c += line * rim * 0.35 * max(-lit, 0.0);
    // Light gathers just inside the rim.
    c += lens * rim * 0.10;

    gl_FragColor = vec4(clamp(c, 0.0, 1.0), 1.0) * cover * alpha;
}
"#;

/// How a surface tints its glass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    /// Wash colour (straight, not premultiplied) and how much of it covers
    /// the background: low for clear glass, high where text must stay legible
    /// over anything.
    pub wash: [f32; 4],
    /// Lens strength in logical px.
    pub bend: f32,
    /// Rim highlight brightness.
    pub rim: f32,
}

/// The one glass of GAVNO, in three thicknesses.
pub mod tone {
    use super::Tone;
    /// Bar, dock, taskbar: thin and clear, the wallpaper shows through.
    pub const CLEAR: Tone = Tone { wash: [0.05, 0.06, 0.09, 0.30], bend: 10.0, rim: 0.28 };
    /// Cards on the desktop and toasts.
    pub const REGULAR: Tone = Tone { wash: [0.05, 0.06, 0.09, 0.45], bend: 12.0, rim: 0.26 };
    /// Popovers and menus, which may float over busy windows.
    pub const THICK: Tone = Tone { wash: [0.045, 0.05, 0.075, 0.68], bend: 14.0, rim: 0.24 };
}

/// What a frame of one output needs to draw glass.
#[derive(Clone)]
pub struct Frame {
    program: GlesTexProgram,
    texture: GlesTexture,
    /// Texture size in px.
    texture_size: (i32, i32),
    /// Bumped with every new texture, so cached elements redraw.
    generation: u64,
    /// Output size in logical px.
    output: Size<i32, Logical>,
}

/// Identity of a glass element between frames: same surface, same id, so a
/// still bar or dock produces no damage.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    area: (i32, i32, i32, i32),
    radius: u32,
    tone: [u32; 6],
    alpha: u32,
    scale: u64,
}

thread_local! {
    static FRAME: RefCell<Option<Frame>> = const { RefCell::new(None) };
    static IDS: RefCell<HashMap<Key, (Id, u64, CommitCounter)>> = RefCell::new(HashMap::new());
}

/// The texture generation of the current frame's glass, 0 when there is none;
/// caches of drawn glass key on it.
pub fn generation() -> u64 {
    FRAME.with(|f| f.borrow().as_ref().map_or(0, |frame| frame.generation.wrapping_add(1)))
}

/// Makes `frame` the glass of everything drawn until the guard drops.
pub fn enter(frame: Option<Frame>) -> FrameGuard {
    FRAME.with(|f| *f.borrow_mut() = frame);
    FrameGuard
}
pub struct FrameGuard;
impl Drop for FrameGuard {
    fn drop(&mut self) {
        FRAME.with(|f| *f.borrow_mut() = None);
        // Forget surfaces that went away, keeping the map small.
        IDS.with(|ids| {
            let mut ids = ids.borrow_mut();
            if ids.len() > 512 {
                ids.clear();
            }
        });
    }
}

/// A glass element for `area` (output-local logical px), or None when the
/// output has no glass yet.
pub fn element(
    area: Rectangle<i32, Logical>,
    radius: f32,
    tone: Tone,
    alpha: f32,
    scale: f64,
) -> Option<GavnoRenderElement> {
    let frame = FRAME.with(|f| f.borrow().clone())?;
    if area.size.w <= 0 || area.size.h <= 0 || frame.output.w <= 0 || frame.output.h <= 0 || alpha <= 0.0 {
        return None;
    }
    let (tw, th) = (frame.texture_size.0 as f64, frame.texture_size.1 as f64);
    let sx = tw / frame.output.w as f64;
    let sy = th / frame.output.h as f64;
    let src = Rectangle::<f64, Buffer>::new(
        (area.loc.x as f64 * sx, area.loc.y as f64 * sy).into(),
        (area.size.w as f64 * sx, area.size.h as f64 * sy).into(),
    );
    let physical = area.to_f64().to_physical(scale).to_i32_round::<i32>();
    let key = Key {
        area: (area.loc.x, area.loc.y, area.size.w, area.size.h),
        radius: radius.to_bits(),
        tone: [tone.wash[0].to_bits(), tone.wash[1].to_bits(), tone.wash[2].to_bits(), tone.wash[3].to_bits(), tone.bend.to_bits(), tone.rim.to_bits()],
        alpha: alpha.to_bits(),
        scale: scale.to_bits(),
    };
    let (id, commit) = IDS.with(|ids| {
        let mut ids = ids.borrow_mut();
        let entry = ids.entry(key).or_insert_with(|| (Id::new(), frame.generation, CommitCounter::default()));
        if entry.1 != frame.generation {
            entry.1 = frame.generation;
            entry.2.increment();
        }
        (entry.0.clone(), entry.2)
    });
    let s = scale as f32;
    Some(GavnoRenderElement::Glass(GlassElement {
        id,
        commit,
        texture: frame.texture,
        program: frame.program,
        src,
        geometry: physical,
        alpha,
        uniforms: vec![
            Uniform::new("src", [(src.loc.x / tw) as f32, (src.loc.y / th) as f32, (src.size.w / tw) as f32, (src.size.h / th) as f32]),
            Uniform::new("size", [physical.size.w as f32, physical.size.h as f32]),
            Uniform::new("radius", radius * s),
            Uniform::new("bend", tone.bend * s),
            Uniform::new("wash", tone.wash),
            Uniform::new("rim", tone.rim),
        ],
    }))
}

/// One glass surface: samples the blurred backdrop through the glass shader.
#[derive(Debug)]
pub struct GlassElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    program: GlesTexProgram,
    src: Rectangle<f64, Buffer>,
    geometry: Rectangle<i32, Physical>,
    alpha: f32,
    uniforms: Vec<Uniform<'static>>,
}
impl Element for GlassElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.src
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }
    fn alpha(&self) -> f32 {
        self.alpha
    }
    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}
impl RenderElement<GlesRenderer> for GlassElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha,
            Some(&self.program),
            &self.uniforms,
        )
    }
}

/// A finished blur from the worker.
pub struct Blurred {
    pub output: String,
    pub signature: u64,
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

#[derive(Default)]
struct Output {
    /// Signature of the background last seen, and since when it holds.
    seen: u64,
    seen_since: Duration,
    /// Signature of the capture in flight or done.
    captured: u64,
    texture: Option<(GlesTexture, (i32, i32))>,
    generation: u64,
    upload: Option<Blurred>,
    output: Size<i32, Logical>,
}

#[derive(Default)]
pub struct Glass {
    program: Option<(ContextId<GlesTexture>, GlesTexProgram)>,
    failed: bool,
    outputs: HashMap<String, Output>,
    tx: Option<channel::Sender<Blurred>>,
    pub source: Option<RegistrationToken>,
}

impl Glass {
    pub fn set_sender(&mut self, tx: channel::Sender<Blurred>) {
        self.tx = Some(tx);
    }
    /// Stores a worker result; it is uploaded by the next frame of its output.
    pub fn accept(&mut self, blurred: Blurred) -> bool {
        let Some(out) = self.outputs.get_mut(&blurred.output) else { return false };
        if blurred.signature != out.captured {
            return false;
        }
        out.upload = Some(blurred);
        true
    }
    pub fn retain_outputs(&mut self, names: &[String]) {
        self.outputs.retain(|name, _| names.contains(name));
    }

    fn program(&mut self, renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
        if self.failed {
            return None;
        }
        let context = renderer.context_id();
        if self.program.as_ref().is_some_and(|(c, _)| *c == context) {
            return self.program.as_ref().map(|(_, p)| p.clone());
        }
        match renderer.compile_custom_texture_shader(
            SHADER,
            &[
                UniformName::new("src", UniformType::_4f),
                UniformName::new("size", UniformType::_2f),
                UniformName::new("radius", UniformType::_1f),
                UniformName::new("bend", UniformType::_1f),
                UniformName::new("wash", UniformType::_4f),
                UniformName::new("rim", UniformType::_1f),
            ],
        ) {
            Ok(program) => {
                self.program = Some((context, program.clone()));
                Some(program)
            }
            Err(error) => {
                tracing::warn!(?error, "cannot compile the glass shader; surfaces stay flat");
                self.failed = true;
                None
            }
        }
    }

    /// The glass of `output` for this frame: uploads a finished blur, and
    /// starts a capture of `background` (the wallpaper elements, back to
    /// front order as rendered) when it changed and has held still.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        renderer: &mut GlesRenderer,
        name: &str,
        output: Size<i32, Logical>,
        scale: f64,
        now: Duration,
        background: &[GavnoRenderElement],
        background_moving: bool,
    ) -> Option<Frame> {
        let program = self.program(renderer)?;
        let tx = self.tx.clone();
        let out = self.outputs.entry(name.to_owned()).or_default();
        if out.output != output {
            out.output = output;
            out.texture = None;
            out.captured = 0;
        }
        if let Some(blurred) = out.upload.take() {
            match renderer.import_memory(
                &blurred.argb,
                Fourcc::Argb8888,
                (blurred.width, blurred.height).into(),
                false,
            ) {
                Ok(texture) => {
                    out.texture = Some((texture, (blurred.width, blurred.height)));
                    out.generation += 1;
                }
                Err(error) => tracing::warn!(?error, "cannot upload the glass backdrop"),
            }
        }

        let signature = signature(background, output, scale);
        if signature != out.seen {
            out.seen = signature;
            out.seen_since = now;
        }
        let settled = !background_moving && now.saturating_sub(out.seen_since) >= SETTLE;
        if (settled || out.texture.is_none()) && out.captured != signature {
            if let Some(tx) = tx {
                let physical = (
                    ((output.w as f64 * scale) / CAPTURE_DIVISOR).round().max(1.0) as i32,
                    ((output.h as f64 * scale) / CAPTURE_DIVISOR).round().max(1.0) as i32,
                );
                if let Some(rgba) = crate::render::capture_elements(renderer, background, physical, scale / CAPTURE_DIVISOR) {
                    out.captured = signature;
                    let name = name.to_owned();
                    let spawned = std::thread::Builder::new().name("glass-blur".into()).spawn(move || {
                        let (w, h) = (physical.0 as usize, physical.1 as usize);
                        if let Some(argb) = blur(&rgba, w, h) {
                            let _ = tx.send(Blurred { output: name, signature, width: w as i32, height: h as i32, argb });
                        }
                    });
                    if let Err(error) = spawned {
                        tracing::warn!(%error, "cannot start the glass worker");
                    }
                }
            }
        }
        let (texture, texture_size) = out.texture.clone()?;
        Some(Frame { program, texture, texture_size, generation: out.generation, output })
    }
}

/// Identity of the background picture: what is drawn and how big.
fn signature(background: &[GavnoRenderElement], output: Size<i32, Logical>, scale: f64) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (output.w, output.h, scale.to_bits()).hash(&mut hasher);
    for element in background {
        element.id().hash(&mut hasher);
        element.current_commit().distance(Some(CommitCounter::default())).hash(&mut hasher);
    }
    hasher.finish().max(1)
}

/// Blurs an RGBA image in place of its size; returns opaque ARGB8888.
pub fn blur(rgba: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || rgba.len() < width * height * 4 {
        return None;
    }
    let mut image: Vec<[f32; 3]> = rgba[..width * height * 4]
        .chunks_exact(4)
        .map(|p| [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])])
        .collect();
    let mut scratch = image.clone();
    for _ in 0..BLUR_PASSES {
        box_pass(&image, &mut scratch, width, height, true);
        box_pass(&scratch, &mut image, width, height, false);
    }
    let mut argb = Vec::with_capacity(width * height * 4);
    for [r, g, b] in image {
        let byte = |c: f32| c.round().clamp(0.0, 255.0) as u8;
        argb.extend([byte(b), byte(g), byte(r), 255]);
    }
    Some(argb)
}

/// One running-sum box blur along rows (`horizontal`) or columns, clamping at the edges.
fn box_pass(src: &[[f32; 3]], dst: &mut [[f32; 3]], w: usize, h: usize, horizontal: bool) {
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let at = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let window = (2 * BLUR_RADIUS + 1) as f32;
    for line in 0..lines {
        let mut sum = [0.0f32; 3];
        for k in -(BLUR_RADIUS as isize)..=BLUR_RADIUS as isize {
            let p = src[at(line, k.clamp(0, len as isize - 1) as usize)];
            for c in 0..3 {
                sum[c] += p[c];
            }
        }
        for i in 0..len {
            dst[at(line, i)] = sum.map(|c| c / window);
            let add = (i + BLUR_RADIUS + 1).min(len - 1);
            let sub = i.saturating_sub(BLUR_RADIUS);
            let (a, s) = (src[at(line, add)], src[at(line, sub)]);
            for c in 0..3 {
                sum[c] += a[c] - s[c];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_keeps_size_and_softens_an_edge() {
        let (w, h) = (64, 8);
        let mut rgba = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 255 } else { 0 };
                rgba.extend([v, v, v, 255]);
            }
        }
        let argb = blur(&rgba, w, h).unwrap();
        assert_eq!(argb.len(), w * h * 4);
        let value = |x: usize| argb[(4 * w + x) * 4 + 2];
        assert_eq!(value(0), 255);
        assert_eq!(value(w - 1), 0);
        assert!((1..w).all(|x| value(x) <= value(x - 1)), "monotonic");
        let ramp = (0..w).filter(|x| value(*x) > 4 && value(*x) < 250).count();
        assert!(ramp >= 6, "the edge is a ramp: {ramp}");
        assert!(argb.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn blur_rejects_short_input() {
        assert!(blur(&[], 0, 0).is_none());
        assert!(blur(&[0; 8], 4, 4).is_none());
    }

    #[test]
    fn no_frame_means_no_glass() {
        let _guard = enter(None);
        assert!(element(Rectangle::new((0, 0).into(), (10, 10).into()), 4.0, tone::CLEAR, 1.0, 1.0).is_none());
    }

    #[test]
    fn tones_get_thicker_for_popovers() {
        assert!(tone::CLEAR.wash[3] < tone::REGULAR.wash[3]);
        assert!(tone::REGULAR.wash[3] < tone::THICK.wash[3]);
    }
}
