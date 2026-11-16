//! Cached conversion around existing elements, including decoration shaders.
//! Only explicitly tagged surfaces enter this path. FP16 retains scRGB's
//! negative and above-white values until the final BT.2020/PQ output pass.
use std::{cell::RefCell, collections::HashMap, sync::Arc};
use smithay::{
    backend::{allocator::Fourcc, renderer::{
        element::{texture::{TextureBuffer, TextureRenderElement}, Element, Id, Kind, RenderElement},
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType, UniformValue},
        utils::{CommitCounter, DamageSet, OpaqueRegions}, Bind, ContextId, Frame, Offscreen, Renderer,
    }},
    output::Output,
    utils::{Buffer, Physical, Rectangle, Scale, Transform},
};
use crate::{color, handlers::color_management::Description, render::GavnoRenderElement};

#[derive(PartialEq)]
struct Key { source: CommitCounter, geometry: Rectangle<i32,Physical>, description: u32, hdr: bool, white: f32, scale: f64, alpha: f32, src: Rectangle<f64,Buffer>, transform: Transform }
struct Entry { key: Key, texture: GlesTexture, serial: usize }
struct Cache { context: ContextId<GlesTexture>, program: GlesTexProgram, entries: HashMap<(Id,usize),Entry> }
#[derive(Default)]
struct OutputCache(RefCell<Option<Cache>>);

pub fn reset(output: &Output) {
    if let Some(cache)=output.user_data().get::<OutputCache>() { cache.0.replace(None); }
}

pub fn convert(renderer: &mut GlesRenderer, output: &Output, elements: &[GavnoRenderElement], descriptions: &HashMap<Id,Arc<Description>>, hdr: bool, white: f32) -> Result<Vec<Option<ManagedElement>>, GlesError> {
    let cell=output.user_data().get_or_insert(OutputCache::default);
    let mut cache=cell.0.borrow_mut();
    if cache.as_ref().is_some_and(|c| c.context != renderer.context_id()) { *cache=None; }
    if descriptions.is_empty() { if let Some(c)=cache.as_mut() { c.entries.clear(); } return Ok(Vec::new()); }
    if cache.is_none() {
        let uniforms=[UniformName::new("tf_id",UniformType::_1f),UniformName::new("primary_matrix",UniformType::Matrix3x3),
            UniformName::new("luminance",UniformType::_2f),UniformName::new("white_nits",UniformType::_1f),UniformName::new("hdr_output",UniformType::_1f)];
        *cache=Some(Cache { context: renderer.context_id(), program: renderer.compile_custom_texture_shader(include_str!("surface.frag"),&uniforms)?, entries: HashMap::new() });
    }
    let Some(cache)=cache.as_mut() else { return Ok(Vec::new()); };
    let scale=Scale::from(output.current_scale().fractional_scale());
    let mut alive=Vec::new();
    let mut result=Vec::with_capacity(elements.len());
    let mut occurrences=HashMap::<Id,usize>::new();
    for element in elements {
        let id=element.id().clone();
        let occurrence=occurrences.entry(id.clone()).or_default();
        let cache_id=(id.clone(),*occurrence); *occurrence += 1;
        let Some(description)=descriptions.get(&id) else { result.push(None); continue; };
        // Tagged standard sRGB keeps the ordinary SDR path as well.
        if !hdr && description.tf==color::Transfer::Srgb && description.primaries==color::SRGB {
            result.push(None); continue;
        }
        let geometry=element.geometry(scale);
        if geometry.is_empty() { result.push(None); continue; }
        let key=Key { source: element.current_commit(), geometry, description: description.identity, hdr, white, scale: scale.x, alpha:element.alpha(), src:element.src(), transform:element.transform() };
        if !cache.entries.get(&cache_id).is_some_and(|e| e.key==key) {
            let old=cache.entries.remove(&cache_id);
            let serial=old.as_ref().map_or(1usize << (usize::BITS-1),|e| e.serial.wrapping_add(1));
            let mut texture=match old.filter(|e|e.key.geometry.size==geometry.size) {
                Some(e)=>e.texture, None=><GlesRenderer as Offscreen<GlesTexture>>::create_buffer(renderer,Fourcc::Abgr16161616f,(geometry.size.w,geometry.size.h).into())?,
            };
            {
                let mut fb=renderer.bind(&mut texture)?;
                let mut frame=renderer.render(&mut fb,geometry.size,Transform::Normal)?;
                let rect=Rectangle::from_size(geometry.size);
                frame.clear([0.0;4].into(),&[rect])?;
                element.draw(&mut frame,element.src(),rect,&[rect],&[])?;
                let _ = frame.finish()?;
            }
            cache.entries.insert(cache_id.clone(),Entry { key,texture,serial });
        }
        let Some(entry)=cache.entries.get(&cache_id) else { result.push(None); continue; };
        let buffer=TextureBuffer::from_texture(renderer,entry.texture.clone(),1,Transform::Normal,None);
        let inner=TextureRenderElement::from_texture_buffer((0.0,0.0),&buffer,Some(1.0),None,None,Kind::Unspecified);
        let matrix=color::conversion(description.primaries,color::SRGB).unwrap_or([[1.0,0.0,0.0],[0.0,1.0,0.0],[0.0,0.0,1.0]]);
        let values=std::array::from_fn::<_,9,_>(|i| matrix[i%3][i/3] as f32);
        let lum=description.tf.scale(white,description.luminances);
        let uniforms=vec![Uniform::new("tf_id",description.tf as u32 as f32),
            Uniform::new("primary_matrix",UniformValue::Matrix3x3 { matrices: vec![values],transpose:false }),
            Uniform::new("luminance",lum),Uniform::new("white_nits",white),Uniform::new("hdr_output",if hdr {1.0f32}else{0.0})];
        result.push(Some(ManagedElement { inner, id:id.clone(), geometry, commit:entry.serial.into(),
            program:cache.program.clone(), uniforms, opaque: element.opaque_regions(scale), kind:element.kind(), alpha:element.alpha() }));
        alive.push(cache_id);
    }
    cache.entries.retain(|id,_| alive.contains(id));
    Ok(result)
}

#[derive(Debug)]
pub struct ManagedElement {
    inner: TextureRenderElement<GlesTexture>, id: Id, geometry: Rectangle<i32,Physical>, commit: CommitCounter,
    program: GlesTexProgram, uniforms: Vec<Uniform<'static>>, opaque: OpaqueRegions<i32,Physical>, kind: Kind, alpha: f32,
}
impl Element for ManagedElement {
    fn id(&self)->&Id { &self.id }
    fn current_commit(&self)->CommitCounter { self.commit }
    fn src(&self)->Rectangle<f64,Buffer> { self.inner.src() }
    fn geometry(&self,_:Scale<f64>)->Rectangle<i32,Physical> { self.geometry }
    fn kind(&self)->Kind { self.kind }
    fn alpha(&self)->f32 { self.alpha }
    fn damage_since(&self,_:Scale<f64>,commit:Option<CommitCounter>)->DamageSet<i32,Physical> {
        if commit==Some(self.commit) { DamageSet::default() } else { DamageSet::from_slice(&[Rectangle::from_size(self.geometry.size)]) }
    }
    fn opaque_regions(&self,_:Scale<f64>)->OpaqueRegions<i32,Physical> { OpaqueRegions::from_slice(&self.opaque) }
}
impl RenderElement<GlesRenderer> for ManagedElement {
    fn draw(&self,frame:&mut GlesFrame<'_,'_>,src:Rectangle<f64,Buffer>,dst:Rectangle<i32,Physical>,damage:&[Rectangle<i32,Physical>],opaque:&[Rectangle<i32,Physical>])->Result<(),GlesError> {
        frame.override_default_tex_program(self.program.clone(),self.uniforms.clone());
        let result=<TextureRenderElement<GlesTexture> as RenderElement<GlesRenderer>>::draw(&self.inner,frame,src,dst,damage,opaque);
        frame.clear_tex_program_override(); result
    }
}

impl ManagedElement {
    /// Reuse the encoded staging texture if KMS or rendering falls back to SDR
    /// in the same frame, before the next element-list construction.
    pub fn fallback_sdr(&mut self) {
        if let Some(uniform)=self.uniforms.iter_mut().find(|u| u.name=="hdr_output") { uniform.value=0.0f32.into(); }
        self.commit.increment();
    }
}
