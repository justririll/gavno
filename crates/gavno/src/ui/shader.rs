//! GPU shapes drawn with pixel shaders: rounded rectangles (with borders),
//! soft shadows and the living-theme background.
//!
//! Elements are kept between frames and only rebuilt when their inputs
//! change, so a still UI produces no damage and no redraws.

use smithay::{
    backend::renderer::{
        element::Kind,
        gles::{element::PixelShaderElement, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType},
    },
    utils::{Logical, Rectangle},
};

const HEADER: &str = r#"
precision highp float;
uniform float alpha;
// Smithay passes the element size in logical px; `scale` converts to
// physical px so edges antialias over exactly one device pixel.
uniform vec2 size;
uniform float scale;
varying vec2 v_coords;

float sd_round_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
"#;

/// Rounded rectangle with an optional inner border. Colors are premultiplied.
const RRECT: &str = r#"
uniform vec4 color;
uniform vec4 border_color;
uniform float radius;
uniform float border;

void main() {
    vec2 sz = size * scale;
    vec2 p = v_coords * sz - sz * 0.5;
    float d = sd_round_box(p, sz * 0.5, min(radius, min(sz.x, sz.y) * 0.5));
    float cover = clamp(0.5 - d, 0.0, 1.0);
    vec4 c = color;
    if (border > 0.0) {
        float inner = clamp(0.5 - (d + border), 0.0, 1.0);
        c = mix(border_color, color, inner);
    }
    gl_FragColor = c * cover * alpha;
}
"#;

/// Soft shadow of a rounded rectangle centered in the element. The element
/// is larger than the rectangle by `blur` on each side.
const SHADOW: &str = r#"
uniform vec4 color;
uniform vec2 rect;
uniform float radius;
uniform float blur;

void main() {
    vec2 sz = size * scale;
    vec2 p = v_coords * sz - sz * 0.5;
    float d = sd_round_box(p, rect * 0.5, radius);
    float a = 1.0 - smoothstep(-blur * 0.5, blur, d);
    gl_FragColor = color * (a * a) * alpha;
}
"#;

/// Sky gradient with two soft color fields and a sun/moon glow. Dithered so
/// the long gradients do not band on 8-bit panels.
const BACKGROUND: &str = r#"
uniform vec3 sky_top;
uniform vec3 sky_bottom;
uniform vec3 blob_a;
uniform vec3 blob_b;
uniform vec3 glow_color;
uniform vec3 sun;

float field(vec2 p, vec2 c, float k) {
    vec2 d = p - c;
    return exp(-k * dot(d, d));
}

void main() {
    vec2 uv = v_coords;
    float aspect = size.x / size.y;
    vec2 p = vec2(uv.x * aspect, uv.y);
    vec3 col = mix(sky_top, sky_bottom, smoothstep(0.0, 1.0, uv.y));
    col = mix(col, blob_a, 0.55 * field(p, vec2(0.18 * aspect, 0.95), 3.2));
    col = mix(col, blob_b, 0.45 * field(p, vec2(0.92 * aspect, 0.15), 4.0));
    col += glow_color * sun.z * field(p, vec2(sun.x * aspect, sun.y), 9.0);
    // Interleaved gradient noise: small intermediate values, safe on GPUs
    // that evaluate parts of this in fp16 (sin() of large numbers is not).
    float n = fract(52.9829189 * fract(dot(gl_FragCoord.xy, vec2(0.06711056, 0.00583715))));
    col += (n - 0.5) / 255.0;
    gl_FragColor = vec4(col, 1.0) * alpha;
}
"#;

pub struct Shaders {
    pub rrect: GlesPixelProgram,
    pub shadow: GlesPixelProgram,
    pub background: GlesPixelProgram,
}

impl Shaders {
    pub fn compile(renderer: &mut GlesRenderer) -> Result<Self, smithay::backend::renderer::gles::GlesError> {
        let u = |name: &'static str, t: UniformType| UniformName::new(name, t);
        let rrect = renderer.compile_custom_pixel_shader(
            format!("{HEADER}{RRECT}"),
            &[
                u("scale", UniformType::_1f),
                u("color", UniformType::_4f),
                u("border_color", UniformType::_4f),
                u("radius", UniformType::_1f),
                u("border", UniformType::_1f),
            ],
        )?;
        let shadow = renderer.compile_custom_pixel_shader(
            format!("{HEADER}{SHADOW}"),
            &[
                u("scale", UniformType::_1f),
                u("color", UniformType::_4f),
                u("rect", UniformType::_2f),
                u("radius", UniformType::_1f),
                u("blur", UniformType::_1f),
            ],
        )?;
        let background = renderer.compile_custom_pixel_shader(
            format!("{HEADER}{BACKGROUND}"),
            &[
                u("scale", UniformType::_1f),
                u("sky_top", UniformType::_3f),
                u("sky_bottom", UniformType::_3f),
                u("blob_a", UniformType::_3f),
                u("blob_b", UniformType::_3f),
                u("glow_color", UniformType::_3f),
                u("sun", UniformType::_3f),
            ],
        )?;
        Ok(Self { rrect, shadow, background })
    }
}

/// Premultiplies a straight RGBA color.
pub fn premul(c: [f32; 4]) -> [f32; 4] {
    [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]]
}

/// A shader element that survives across frames while its inputs stay the
/// same. Any change rebuilds it, which damages exactly its area.
#[derive(Default)]
pub struct ShaderBox {
    elem: Option<PixelShaderElement>,
    key: Vec<f32>,
}

impl ShaderBox {
    pub fn element(
        &mut self,
        program: &GlesPixelProgram,
        area: Rectangle<i32, Logical>,
        alpha: f32,
        uniforms: Vec<(&'static str, Vec<f32>)>,
    ) -> PixelShaderElement {
        let mut key = vec![
            area.loc.x as f32,
            area.loc.y as f32,
            area.size.w as f32,
            area.size.h as f32,
            alpha,
        ];
        for (_, v) in &uniforms {
            key.extend_from_slice(v);
        }
        if self.elem.is_none() || self.key != key {
            let uniforms = uniforms
                .into_iter()
                .map(|(name, v)| match v.len() {
                    1 => Uniform::new(name, v[0]),
                    2 => Uniform::new(name, [v[0], v[1]]),
                    3 => Uniform::new(name, [v[0], v[1], v[2]]),
                    _ => Uniform::new(name, [v[0], v[1], v[2], v[3]]),
                })
                .collect();
            self.elem = Some(PixelShaderElement::new(
                program.clone(),
                area,
                None,
                alpha,
                uniforms,
                Kind::Unspecified,
            ));
            self.key = key;
        }
        self.elem.clone().unwrap()
    }
}

/// Rounded rectangle helper.
pub fn rrect(
    sb: &mut ShaderBox,
    shaders: &Shaders,
    area: Rectangle<i32, Logical>,
    radius_px: f32,
    color: [f32; 4],
    border_px: f32,
    border_color: [f32; 4],
    alpha: f32,
    scale: f64,
) -> PixelShaderElement {
    sb.element(
        &shaders.rrect,
        area,
        alpha,
        vec![
            ("scale", vec![scale as f32]),
            ("color", premul(color).to_vec()),
            ("border_color", premul(border_color).to_vec()),
            ("radius", vec![radius_px]),
            ("border", vec![border_px]),
        ],
    )
}

/// Shadow under `rect` (logical); `blur` in logical px, `scale` converts the
/// shader's physical-pixel parameters.
pub fn shadow(
    sb: &mut ShaderBox,
    shaders: &Shaders,
    rect: Rectangle<i32, Logical>,
    radius: f32,
    blur: i32,
    offset_y: i32,
    color: [f32; 4],
    alpha: f32,
    scale: f64,
) -> PixelShaderElement {
    let area = Rectangle::new(
        (rect.loc.x - blur, rect.loc.y - blur + offset_y).into(),
        (rect.size.w + 2 * blur, rect.size.h + 2 * blur).into(),
    );
    let s = scale as f32;
    sb.element(
        &shaders.shadow,
        area,
        alpha,
        vec![
            ("scale", vec![s]),
            ("color", premul(color).to_vec()),
            ("rect", vec![rect.size.w as f32 * s, rect.size.h as f32 * s]),
            ("radius", vec![radius * s]),
            ("blur", vec![blur as f32 * s]),
        ],
    )
}
