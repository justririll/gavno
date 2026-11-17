//! Persistent SDR composition and a single damage-tracked PQ encoding pass.
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                texture::{TextureRenderBuffer, TextureRenderElement},
                Element, Id, Kind, RenderElement, RenderElementStates,
            },
            gles::{
                GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
                UniformName, UniformType,
            },
            utils::{CommitCounter, DamageSet, OpaqueRegions},
            Bind, Offscreen,
        },
    },
    output::Output,
    utils::{Buffer, Physical, Rectangle, Scale, Size, Transform},
};

pub struct Composition {
    buffer: TextureRenderBuffer<GlesTexture>,
    tracker: OutputDamageTracker,
    program: GlesTexProgram,
    pub white_nits: f32,
    force_damage: bool,
    pending: bool,
    extended: bool,
}

impl Composition {
    pub fn new(
        renderer: &mut GlesRenderer,
        output: &Output,
        white_nits: f32,
    ) -> Result<Self, String> {
        let _startup = crate::perf::Startup::new("hdr.composition.begin", "hdr.composition.end");
        let mode = output.current_mode().ok_or("output has no mode")?;
        let size = (mode.size.w, mode.size.h).into();
        let mut texture = None;
        let mut extended = false;
        for format in [Fourcc::Abgr16161616f, Fourcc::Abgr2101010] {
            match renderer.create_buffer(format, size) {
                Ok(mut candidate) => {
                    let renderable = renderer.bind(&mut candidate).map(|_| ());
                    match renderable {
                        Ok(()) => {
                            texture = Some(candidate);
                            extended = format == Fourcc::Abgr16161616f;
                            break;
                        }
                        Err(error) => {
                            tracing::warn!(?format, %error, "HDR offscreen format is not renderable")
                        }
                    }
                }
                Err(error) => tracing::warn!(?format, %error, "HDR offscreen allocation failed"),
            }
        }
        let texture: GlesTexture =
            texture.ok_or("neither fp16 nor 10-bit offscreen rendering is supported")?;
        tracing::info!(?texture, "HDR SDR-composition texture ready");
        crate::perf::startup("hdr.offscreen.ready");
        let program = renderer
            .compile_custom_texture_shader(
                crate::color::output_shader(),
                &[UniformName::new("white_nits", UniformType::_1f),
                  UniformName::new("extended_input", UniformType::_1f)],
            )
            .map_err(|e| e.to_string())?;
        crate::perf::startup("hdr.shader.ready");
        Ok(Self {
            buffer: TextureRenderBuffer::from_texture(
                renderer,
                texture,
                1,
                Transform::Normal,
                Some(vec![Rectangle::from_size(size)]),
            ),
            tracker: OutputDamageTracker::new(
                mode.size,
                output.current_scale().fractional_scale(),
                Transform::Normal,
            ),
            program,
            white_nits,
            force_damage: true,
            pending: true,
            extended,
        })
    }

    /// KMS buffers or connector state were invalidated even if the scene stayed still.
    pub fn invalidate(&mut self) { self.force_damage = true; }
    /// Keep retries alive until KMS confirms that the encoded pixels reached a vblank.
    pub fn presented(&mut self) { self.pending = false; }
    pub fn needs_scanout(&self) -> bool { self.pending }

    pub fn adjust_white(&mut self, delta: f32) {
        let white = (self.white_nits + delta).clamp(80.0, 400.0);
        if self.white_nits != white {
            self.white_nits = white;
            self.force_damage = true;
        }
    }

    pub fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        elements: &[crate::render::GavnoRenderElement],
        clear: [f32; 4],
    ) -> Result<(PqElement, RenderElementStates), String> {
        let extended_input = elements.iter().any(|e| matches!(e, crate::render::GavnoRenderElement::Color(_)));
        if !self.extended && extended_input {
            return Err("HDR client color requires renderable FP16 composition".into());
        }
        let mode = output.current_mode().ok_or("output has no mode")?;
        let mut states = RenderElementStates::default();
        let force = self.force_damage;
        let mut changed = force || self.pending;
        self.buffer.render().draw(|texture| -> Result<_, String> {
            let mut fb = renderer.bind(texture).map_err(|e| e.to_string())?;
            let result = self
                .tracker
                .render_output(renderer, &mut fb, 1, elements, clear)
                .map_err(|e| e.to_string())?;
            changed |= result.damage.is_some();
            states = result.states;
            // Both passes use the same GLES context: GL command ordering supplies
            // the dependency without a CPU wait or a readback.
            let damage = if force {
                vec![Rectangle::from_size(mode.size)]
            } else {
                result.damage.cloned().unwrap_or_default()
            };
            Ok(damage
                .into_iter()
                .map(|r| {
                    Rectangle::<i32, Buffer>::new(
                        (r.loc.x, r.loc.y).into(),
                        (r.size.w, r.size.h).into(),
                    )
                })
                .collect())
        })?;
        self.force_damage = false;
        self.pending = changed;
        let inner = TextureRenderElement::from_texture_render_buffer(
            (0.0, 0.0),
            &self.buffer,
            Some(1.0),
            None,
            None,
            Kind::Unspecified,
        );
        Ok((
            PqElement {
                inner,
                program: self.program.clone(),
                white_nits: self.white_nits,
                size: mode.size,
                extended_input,
            },
            states,
        ))
    }
}

/// A stable element identity and damage history, unlike rebuilding a
/// TextureShaderElement (which assigns a fresh ID on every construction).
#[derive(Debug)]
pub struct PqElement {
    inner: TextureRenderElement<GlesTexture>,
    program: GlesTexProgram,
    white_nits: f32,
    size: Size<i32, Physical>,
    extended_input: bool,
}
impl Element for PqElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::from_size(self.size)
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(1.0.into(), commit)
    }
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(1.0.into())
    }
}
impl RenderElement<GlesRenderer> for PqElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(
            self.program.clone(),
            vec![Uniform::new("white_nits", self.white_nits),
                 Uniform::new("extended_input", if self.extended_input { 1.0f32 } else { 0.0 })],
        );
        let result = <TextureRenderElement<GlesTexture> as RenderElement<GlesRenderer>>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque,
        );
        frame.clear_tex_program_override();
        result
    }
}
