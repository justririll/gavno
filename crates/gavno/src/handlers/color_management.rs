//! Version 1 parametric color management. All requests are CPU-only and bounded.
use std::sync::{atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering}, Arc, Mutex, MutexGuard};
use smithay::{
    backend::renderer::element::Id,
    output::{Output, WeakOutput},
    reexports::wayland_server::{
        backend::GlobalId, protocol::{wl_output::WlOutput, wl_surface::WlSurface},
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak, WEnum,
    },
    wayland::compositor::{with_states, Cacheable},
};
use wayland_protocols::wp::color_management::v1::server::{
    wp_color_manager_v1::{self as manager, WpColorManagerV1},
    wp_color_management_output_v1::{self as output, WpColorManagementOutputV1},
    wp_color_management_surface_v1::{self as surface, WpColorManagementSurfaceV1},
    wp_color_management_surface_feedback_v1::{self as feedback, WpColorManagementSurfaceFeedbackV1},
    wp_image_description_creator_params_v1::{self as params, WpImageDescriptionCreatorParamsV1},
    wp_image_description_v1::{self as image, WpImageDescriptionV1},
    wp_image_description_info_v1::{self as info, WpImageDescriptionInfoV1},
};
use crate::{color::{self, Transfer}, module::Feature, Gavno};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }
fn raw<T: Into<u32>>(v: WEnum<T>) -> u32 { match v { WEnum::Value(v) => v.into(), WEnum::Unknown(v) => v } }
static TAGGED: AtomicUsize = AtomicUsize::new(0);
pub fn any_tagged() -> bool { TAGGED.load(Ordering::Relaxed) != 0 }
static IDENTITY: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, Debug)]
pub struct Description {
    pub identity: u32,
    pub tf: Transfer,
    pub primaries: [i32; 8],
    named: Option<manager::Primaries>,
    /// Minimum, maximum and reference white, in cd/m².
    pub luminances: [f64; 3],
    target: [i32; 8],
    target_luminance: [f64; 2],
    max_cll: Option<u32>,
    max_fall: Option<u32>,
}
impl Description {
    fn new(tf: Transfer, primaries: [i32;8], named: Option<manager::Primaries>, luminances: [f64;3]) -> Self {
        Self { identity: IDENTITY.fetch_add(1, Ordering::Relaxed).max(1), tf, primaries, named,
            luminances, target: primaries, target_luminance: [luminances[0],luminances[1]], max_cll: None, max_fall: None }
    }
    fn srgb() -> Self { Self::new(Transfer::Srgb, color::SRGB, Some(manager::Primaries::Srgb), [0.2,80.0,80.0]) }
}

#[derive(Default, Debug)]
pub struct Params {
    tf: Option<Transfer>,
    primaries: Option<([i32;8], Option<manager::Primaries>)>,
    luminances: Option<[f64;3]>,
    target: Option<[i32;8]>,
    target_luminance: Option<[f64;2]>,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
}
fn set<T>(slot: &mut Option<T>, value: T) -> Result<(), params::Error> {
    if slot.is_some() { return Err(params::Error::AlreadySet); }
    *slot = Some(value); Ok(())
}
impl Params {
    fn tf(&mut self, value: u32) -> Result<(), params::Error> {
        if self.tf.is_some() { return Err(params::Error::AlreadySet); }
        self.tf = Some(Transfer::from_named(value).ok_or(params::Error::InvalidTf)?); Ok(())
    }
    fn primaries(&mut self, value: u32) -> Result<(), params::Error> {
        if self.primaries.is_some() { return Err(params::Error::AlreadySet); }
        let (p,n) = match value {
            1 => (color::SRGB, manager::Primaries::Srgb),
            6 => (color::BT2020, manager::Primaries::Bt2020),
            9 => (color::DISPLAY_P3, manager::Primaries::DisplayP3),
            _ => return Err(params::Error::InvalidPrimariesNamed),
        };
        self.primaries = Some((p,Some(n))); Ok(())
    }
    fn luminances(&mut self, min: u32, max: u32, reference: u32) -> Result<(), params::Error> {
        if self.luminances.is_some() { return Err(params::Error::AlreadySet); }
        let min = f64::from(min)/10000.0;
        if f64::from(max) <= min || f64::from(reference) <= min { return Err(params::Error::InvalidLuminance); }
        self.luminances = Some([min,f64::from(max),f64::from(reference)]); Ok(())
    }
    fn mastering_luminance(&mut self, min: u32, max: u32) -> Result<(), params::Error> {
        if self.target_luminance.is_some() { return Err(params::Error::AlreadySet); }
        let min = f64::from(min)/10000.0;
        if f64::from(max) <= min { return Err(params::Error::InvalidLuminance); }
        self.target_luminance = Some([min,f64::from(max)]); Ok(())
    }
    fn create(&self) -> Result<Description, params::Error> {
        let tf = self.tf.ok_or(params::Error::IncompleteSet)?;
        let (p,n) = self.primaries.ok_or(params::Error::IncompleteSet)?;
        let mut lum = self.luminances.unwrap_or(match tf { Transfer::Pq => [0.005,10000.0,203.0], _ => [0.2,80.0,80.0] });
        if tf == Transfer::Pq { lum[1] = lum[0] + 10000.0; }
        let target_luminance = self.target_luminance.unwrap_or([lum[0],lum[1]]);
        if self.max_cll.zip(self.max_fall).is_some_and(|(c,f)| f > c)
            || [self.max_cll,self.max_fall].into_iter().flatten().any(|v| f64::from(v) <= target_luminance[0] || f64::from(v) > target_luminance[1]) {
            return Err(params::Error::InvalidLuminance);
        }
        let mut result = Description::new(tf,p,n,lum);
        result.target = self.target.unwrap_or(p); result.target_luminance = target_luminance;
        result.max_cll = self.max_cll; result.max_fall = self.max_fall;
        Ok(result)
    }
}

/// Smithay caches this together with buffer state, including synchronized
/// subsurface transactions. None inside Some means an explicit unset.
#[derive(Default)]
pub struct SurfaceColor { pub description: Option<Option<Arc<Description>>>, active: bool, enabled: Option<Arc<AtomicBool>> }
impl Cacheable for SurfaceColor {
    fn commit(&mut self, _: &DisplayHandle) -> Self { Self { description: self.description.take(), active: false, enabled: self.enabled.clone() } }
    fn merge_into(mut self, into: &mut Self, _: &DisplayHandle) {
        if self.description.is_some() {
            if into.active { TAGGED.fetch_sub(1, Ordering::Relaxed); }
            into.description = if self.enabled.as_ref().is_some_and(|enabled| !enabled.load(Ordering::Relaxed)) { Some(None) } else { self.description.take() };
            into.enabled = self.enabled.take();
            into.active = into.description.as_ref().is_some_and(Option::is_some);
            if into.active { TAGGED.fetch_add(1, Ordering::Relaxed); }
        }
    }
}
impl Drop for SurfaceColor {
    fn drop(&mut self) { if self.active { TAGGED.fetch_sub(1,Ordering::Relaxed); } }
}
#[derive(Default)]
struct SurfaceData {
    controller: Option<Weak<WpColorManagementSurfaceV1>>,
    feedback: Vec<Weak<WpColorManagementSurfaceFeedbackV1>>,
}
pub struct SurfaceRef { surface: Weak<WlSurface>, enabled: Arc<AtomicBool> }
pub struct FeedbackRef { surface: Weak<WlSurface>, enabled: Arc<AtomicBool>, preferred: Mutex<Arc<Description>>, output: Mutex<Option<WeakOutput>> }
pub struct OutputRef { output: Option<WeakOutput>, wl: Weak<WlOutput> }
pub struct ImageRef { pub description: Option<Arc<Description>>, information: bool }
pub struct GlobalData(Arc<AtomicBool>);

pub struct ColorManagement {
    global: Option<GlobalId>,
    enabled: Arc<AtomicBool>,
    surfaces: Vec<Weak<WlSurface>>,
}
impl ColorManagement {
    pub fn new(dh: &DisplayHandle, enabled: bool) -> Self {
        let enabled = Arc::new(AtomicBool::new(enabled));
        let global = enabled.load(Ordering::Relaxed).then(|| dh.create_global::<Gavno,WpColorManagerV1,_>(1,GlobalData(enabled.clone())));
        Self { global, enabled, surfaces: Vec::new() }
    }
    pub fn reset(&mut self, dh: &DisplayHandle) {
        self.enabled.store(false,Ordering::Relaxed);
        if let Some(global) = self.global.take() { dh.remove_global::<Gavno>(global); }
        for surface in self.surfaces.drain(..).filter_map(|s| s.upgrade().ok()) {
            with_states(&surface, |states| {
                let mut cached = states.cached_state.get::<SurfaceColor>();
                *cached.pending() = SurfaceColor::default(); *cached.current() = SurfaceColor::default();
                if let Some(data) = states.data_map.get::<Mutex<SurfaceData>>() { *lock(data) = SurfaceData::default(); }
            });
        }
    }
    fn track(&mut self, surface: &WlSurface) {
        self.surfaces.retain(|s| s.is_alive());
        if !self.surfaces.iter().any(|s| s == &surface.downgrade()) { self.surfaces.push(surface.downgrade()); }
        with_states(surface, |s| { s.data_map.insert_if_missing(|| Mutex::new(SurfaceData::default())); });
    }
}

struct OutputColor {
    description: Arc<Description>,
    outputs: Vec<Weak<WpColorManagementOutputV1>>,
    feedback: Vec<Weak<WpColorManagementSurfaceFeedbackV1>>,
    hdr: bool,
    removed: bool,
}
impl Default for OutputColor {
    fn default() -> Self { Self { description: Arc::new(Description::srgb()), outputs: Vec::new(), feedback: Vec::new(), hdr: false, removed: false } }
}
fn output_color(output: &Output) -> &Mutex<OutputColor> {
    output.user_data().get_or_insert_threadsafe(|| Mutex::new(OutputColor::default()))
}
pub fn reset_output(output: &Output) {
    if let Some(state)=output.user_data().get::<Mutex<OutputColor>>() {
        let mut state=lock(state); state.outputs.clear(); state.feedback.clear();
    }
    crate::hdr::surface::reset(output);
}
pub fn output_removed(output: &Output) {
    lock(output_color(output)).removed = true;
    crate::hdr::surface::reset(output);
}
pub fn output_description(output: &Output) -> Arc<Description> { lock(output_color(output)).description.clone() }
pub fn is_hdr(output: &Output) -> bool { lock(output_color(output)).hdr }

/// Called only after a successful KMS color transition, including fallback.
pub fn output_changed(output: &Output, hdr: bool, caps: crate::hdr::edid::Capabilities, white: f32) {
    let mut state = lock(output_color(output));
    let target_luminance = [caps.min_nits.unwrap_or(0.0),caps.max_nits.unwrap_or(f64::from(white))];
    if state.hdr == hdr && (!hdr || (state.description.target_luminance == target_luminance && state.description.luminances[2] == f64::from(white))) { return; }
    let mut desc = if hdr {
        Description::new(Transfer::Pq,color::BT2020,Some(manager::Primaries::Bt2020),[0.005,10000.005,f64::from(white)])
    } else { Description::srgb() };
    if hdr { desc.target_luminance = target_luminance; }
    state.description = Arc::new(desc); state.hdr = hdr;
    let mut done_outputs=Vec::new();
    state.outputs.retain(|weak| {
        let Ok(resource) = weak.upgrade() else { return false; };
        resource.image_description_changed();
        if let Some(wl) = resource.data::<OutputRef>().and_then(|d| d.wl.upgrade().ok()) {
            if !done_outputs.contains(&wl) { done_outputs.push(wl); }
        }
        true
    });
    for wl in done_outputs { if wl.version() >= 2 { wl.done(); } }
    let desc = state.description.clone();
    state.feedback.retain(|weak| {
        let Ok(resource) = weak.upgrade() else { return false; };
        if let Some(data) = resource.data::<FeedbackRef>() {
            if data.enabled.load(Ordering::Relaxed) && lock(&data.output).as_ref().and_then(WeakOutput::upgrade).as_ref() == Some(output) { *lock(&data.preferred) = desc.clone(); resource.preferred_changed(desc.identity); }
        }
        true
    });
}

pub fn surface_description(surface: &WlSurface) -> Option<Arc<Description>> {
    with_states(surface, |s| s.cached_state.get::<SurfaceColor>().current().description.clone().flatten())
}
/// Refresh feedback on output migration without a timer or a synthetic frame.
pub fn surface_output(surface: &WlSurface, output: &Output) {
    with_states(surface, |s| {
        if let Some(data) = s.data_map.get::<Mutex<SurfaceData>>() {
            lock(data).feedback.retain(|w| {
                let Ok(resource) = w.upgrade() else { return false; };
                if let Some(data) = resource.data::<FeedbackRef>() { prefer(&resource,data,output); }
                true
            });
        }
    });
}
fn prefer(resource: &WpColorManagementSurfaceFeedbackV1, data: &FeedbackRef, output: &Output) {
    let mut state = lock(output_color(output));
    *lock(&data.output) = Some(output.downgrade());
    let mut current = lock(&data.preferred);
    if current.identity != state.description.identity {
        *current = state.description.clone(); resource.preferred_changed(current.identity);
    }
    state.feedback.retain(|w| w.is_alive());
    if !state.feedback.contains(&resource.downgrade()) { state.feedback.push(resource.downgrade()); }
}
fn preferred_output(gv: &Gavno, surface: &WlSurface) -> Option<Output> {
    let mut root = surface.clone();
    while let Some(parent) = smithay::wayland::compositor::get_parent(&root) { root = parent; }
    gv.window_for_surface(&root).and_then(|w| gv.space.outputs_for_element(&w).first().cloned())
        .or_else(|| gv.space.outputs().find(|o| smithay::desktop::layer_map_for_output(o).layers().any(|l| l.wl_surface() == &root)).cloned())
        .or_else(|| gv.active_output())
}
fn image(data_init: &mut DataInit<'_,Gavno>, id: New<WpImageDescriptionV1>, description: Option<Arc<Description>>, information: bool) {
    let identity = description.as_ref().map(|d| d.identity);
    let image = data_init.init(id,ImageRef { description, information });
    if let Some(identity) = identity { image.ready(identity); }
    else { image.failed(image::Cause::NoOutput,"output is no longer available".into()); }
}

fn unavailable(data_init: &mut DataInit<'_,Gavno>, id: New<WpImageDescriptionV1>) {
    let image=data_init.init(id,ImageRef { description: None, information: false });
    image.failed(image::Cause::OperatingSystem,"color management has been disabled".into());
}

impl GlobalDispatch<WpColorManagerV1,GlobalData> for Gavno {
    fn bind(state: &mut Self, _: &DisplayHandle, _: &Client, resource: New<WpColorManagerV1>, _: &GlobalData, data_init: &mut DataInit<'_,Self>) {
        let manager = data_init.init(resource,());
        state.guard(Feature::ColorManagement,(), |_| {
            for feature in [manager::Feature::Parametric,manager::Feature::SetLuminances,manager::Feature::SetPrimaries,manager::Feature::SetMasteringDisplayPrimaries,manager::Feature::ExtendedTargetVolume] { manager.supported_feature(feature); }
            manager.supported_intent(manager::RenderIntent::Perceptual);
            for p in [manager::Primaries::Srgb,manager::Primaries::Bt2020,manager::Primaries::DisplayP3] { manager.supported_primaries_named(p); }
            for tf in [manager::TransferFunction::Srgb,manager::TransferFunction::ExtLinear,manager::TransferFunction::St2084Pq,manager::TransferFunction::Gamma22] { manager.supported_tf_named(tf); }
            manager.done();
        });
    }
    fn can_view(_: Client, data: &GlobalData) -> bool { data.0.load(Ordering::Relaxed) }
}
impl Dispatch<WpColorManagerV1,()> for Gavno {
    fn request(state: &mut Self, _: &Client, resource: &WpColorManagerV1, request: manager::Request, _: &(), _: &DisplayHandle, data_init: &mut DataInit<'_,Self>) {
        if !state.features.on(Feature::ColorManagement) { resource.post_error(manager::Error::UnsupportedFeature,"color management has been disabled"); return; }
        state.guard(Feature::ColorManagement,(), |state| match request {
            manager::Request::Destroy => (),
            manager::Request::GetOutput { id, output } => {
                let out = Output::from_resource(&output);
                let r = data_init.init(id,OutputRef { output: out.as_ref().map(Output::downgrade), wl: output.downgrade() });
                if let Some(out) = out { let mut c=lock(output_color(&out)); c.outputs.retain(|w|w.is_alive()); c.outputs.push(r.downgrade()); }
            }
            manager::Request::GetSurface { id, surface } => {
                state.protocols.color_management.track(&surface);
                let exists = with_states(&surface, |s| s.data_map.get::<Mutex<SurfaceData>>().is_some_and(|d| lock(d).controller.as_ref().is_some_and(|w| w.is_alive())));
                if exists { resource.post_error(manager::Error::SurfaceExists,"surface already has a color controller"); return; }
                let r = data_init.init(id,SurfaceRef { surface: surface.downgrade(), enabled: state.protocols.color_management.enabled.clone() });
                with_states(&surface, |s| { if let Some(d)=s.data_map.get::<Mutex<SurfaceData>>() { lock(d).controller=Some(r.downgrade()); } });
            }
            manager::Request::GetSurfaceFeedback { id, surface } => {
                state.protocols.color_management.track(&surface);
                let out = preferred_output(state,&surface);
                let desc = out.as_ref().map(output_description).unwrap_or_else(||Arc::new(Description::srgb()));
                let r=data_init.init(id, FeedbackRef { surface:surface.downgrade(), enabled:state.protocols.color_management.enabled.clone(), preferred:Mutex::new(desc.clone()), output:Mutex::new(None) });
                r.preferred_changed(desc.identity);
                if let (Some(out),Some(data)) = (out,r.data::<FeedbackRef>()) { prefer(&r,data,&out); }
                if let Some(data) = r.data::<FeedbackRef>() { r.preferred_changed(lock(&data.preferred).identity); }
                with_states(&surface, |s| { if let Some(d)=s.data_map.get::<Mutex<SurfaceData>>() { lock(d).feedback.push(r.downgrade()); } });
            }
            manager::Request::CreateParametricCreator { obj } => { data_init.init(obj,Mutex::new(Params::default())); }
            manager::Request::CreateIccCreator { .. } => { resource.post_error(manager::Error::UnsupportedFeature,"ICC profiles are not supported"); }
            manager::Request::CreateWindowsScrgb { .. } => { resource.post_error(manager::Error::UnsupportedFeature,"Windows scRGB creator is not supported; use parameters"); }
            _ => resource.post_error(manager::Error::UnsupportedFeature,"unsupported version-1 request"),
        });
    }
}
impl Dispatch<WpColorManagementOutputV1,OutputRef> for Gavno {
    fn request(state: &mut Self, _: &Client, _: &WpColorManagementOutputV1, request: output::Request, data: &OutputRef, _: &DisplayHandle, data_init: &mut DataInit<'_,Self>) {
        if !state.features.on(Feature::ColorManagement) {
            if let output::Request::GetImageDescription { image_description } = request { unavailable(data_init,image_description); }
            return;
        }
        state.guard(Feature::ColorManagement,(), |_| {
            if let output::Request::GetImageDescription { image_description } = request {
                image(data_init,image_description,data.output.as_ref().and_then(WeakOutput::upgrade).and_then(|o| { let state=lock(output_color(&o)); (!state.removed).then(|| state.description.clone()) }),true);
            }
        });
    }
}
impl Dispatch<WpColorManagementSurfaceV1,SurfaceRef> for Gavno {
    fn request(state: &mut Self, _: &Client, resource: &WpColorManagementSurfaceV1, request: surface::Request, data: &SurfaceRef, _: &DisplayHandle, _: &mut DataInit<'_,Self>) {
        state.guard(Feature::ColorManagement,(), |_| {
            let Ok(surface) = data.surface.upgrade() else {
                if !matches!(request,surface::Request::Destroy) { resource.post_error(surface::Error::Inert,"surface has been destroyed"); } return;
            };
            if !data.enabled.load(Ordering::Relaxed) { return; }
            let description = match request {
                surface::Request::SetImageDescription { image_description, render_intent } => {
                    if raw(render_intent) != 0 { resource.post_error(surface::Error::RenderIntent,"only perceptual intent is supported"); return; }
                    let Some(desc) = image_description.data::<ImageRef>().and_then(|d| d.description.clone()) else { resource.post_error(surface::Error::ImageDescription,"image description is not ready"); return; };
                    Some(desc)
                }
                surface::Request::UnsetImageDescription => None,
                surface::Request::Destroy => {
                    with_states(&surface, |s| { if let Some(d)=s.data_map.get::<Mutex<SurfaceData>>() { lock(d).controller=None; } });
                    None
                },
                _ => return,
            };
            with_states(&surface, |s| {
                let mut cached = s.cached_state.get::<SurfaceColor>();
                cached.pending().description=Some(description);
                cached.pending().enabled=Some(data.enabled.clone());
            });
        });
    }
    fn destroyed(_: &mut Self, _: smithay::reexports::wayland_server::backend::ClientId, resource: &WpColorManagementSurfaceV1, data: &SurfaceRef) {
        if let Ok(surface) = data.surface.upgrade() { with_states(&surface, |s| {
            if let Some(d)=s.data_map.get::<Mutex<SurfaceData>>() {
                let mut d=lock(d); if d.controller.as_ref()==Some(&resource.downgrade()) { d.controller=None; }
            }
        }); }
    }
}
impl Dispatch<WpColorManagementSurfaceFeedbackV1,FeedbackRef> for Gavno {
    fn request(state: &mut Self, _: &Client, resource: &WpColorManagementSurfaceFeedbackV1, request: feedback::Request, data: &FeedbackRef, _: &DisplayHandle, data_init: &mut DataInit<'_,Self>) {
        if !state.features.on(Feature::ColorManagement) {
            if let feedback::Request::GetPreferred { image_description } | feedback::Request::GetPreferredParametric { image_description } = request { unavailable(data_init,image_description); }
            return;
        }
        state.guard(Feature::ColorManagement,(), |state| {
            let id = match request { feedback::Request::GetPreferred { image_description } | feedback::Request::GetPreferredParametric { image_description } => image_description, _ => return };
            let Ok(surface) = data.surface.upgrade() else { resource.post_error(feedback::Error::Inert,"surface has been destroyed"); return; };
            if let Some(out)=preferred_output(state,&surface) { prefer(resource,data,&out); }
            image(data_init,id,Some(lock(&data.preferred).clone()),true);
        });
    }
}
impl Dispatch<WpImageDescriptionCreatorParamsV1,Mutex<Params>> for Gavno {
    fn request(state: &mut Self, _: &Client, resource: &WpImageDescriptionCreatorParamsV1, request: params::Request, data: &Mutex<Params>, _: &DisplayHandle, data_init: &mut DataInit<'_,Self>) {
        if !state.features.on(Feature::ColorManagement) { resource.post_error(params::Error::UnsupportedFeature,"color management has been disabled"); return; }
        state.guard(Feature::ColorManagement,(), |_| {
            let mut p=lock(data);
            let result = match request {
                params::Request::Create { image_description } => {
                    match p.create() {
                        Err(e) => { resource.post_error(e,"invalid parameter set"); }
                        Ok(desc) => {
                            // Arbitrary coordinates are legal protocol input. Degenerate
                            // color spaces fail gracefully, never with a protocol error.
                            if color::conversion(desc.primaries,color::SRGB).is_none() || color::conversion(desc.target,color::SRGB).is_none() {
                                let r=data_init.init(image_description,ImageRef { description:None, information:false });
                                r.failed(image::Cause::Unsupported,"degenerate primaries or white point".into());
                            } else { image(data_init,image_description,Some(Arc::new(desc)),false); }
                        }
                    } return;
                }
                params::Request::SetTfNamed { tf } => p.tf(raw(tf)),
                params::Request::SetTfPower { .. } => Err(params::Error::UnsupportedFeature),
                params::Request::SetPrimariesNamed { primaries } => p.primaries(raw(primaries)),
                params::Request::SetPrimaries { r_x,r_y,g_x,g_y,b_x,b_y,w_x,w_y } => set(&mut p.primaries,([r_x,r_y,g_x,g_y,b_x,b_y,w_x,w_y],None)),
                params::Request::SetLuminances { min_lum,max_lum,reference_lum } => p.luminances(min_lum,max_lum,reference_lum),
                params::Request::SetMasteringDisplayPrimaries { r_x,r_y,g_x,g_y,b_x,b_y,w_x,w_y } => set(&mut p.target,[r_x,r_y,g_x,g_y,b_x,b_y,w_x,w_y]),
                params::Request::SetMasteringLuminance { min_lum,max_lum } => p.mastering_luminance(min_lum,max_lum),
                params::Request::SetMaxCll { max_cll } => set(&mut p.max_cll,max_cll),
                params::Request::SetMaxFall { max_fall } => set(&mut p.max_fall,max_fall),
                _ => Err(params::Error::UnsupportedFeature),
            };
            if let Err(error)=result { resource.post_error(error,"invalid or repeated color parameter"); }
        });
    }
}
impl Dispatch<WpImageDescriptionV1,ImageRef> for Gavno {
    fn request(state: &mut Self, _: &Client, resource: &WpImageDescriptionV1, request: image::Request, data: &ImageRef, _: &DisplayHandle, data_init: &mut DataInit<'_,Self>) {
        if !state.features.on(Feature::ColorManagement) { resource.post_error(image::Error::NoInformation,"color management has been disabled"); return; }
        state.guard(Feature::ColorManagement,(), |state| {
            if let image::Request::GetInformation { information } = request {
                let Some(d)=&data.description else { resource.post_error(image::Error::NotReady,"image description is not ready"); return; };
                if !data.information { resource.post_error(image::Error::NoInformation,"client-created descriptions do not allow information queries"); return; }
                let r=data_init.init(information,());
                let [rx,ry,gx,gy,bx,by,wx,wy]=d.primaries; r.primaries(rx,ry,gx,gy,bx,by,wx,wy);
                if let Some(named)=d.named { r.primaries_named(named); }
                r.tf_named(manager::TransferFunction::try_from(d.tf as u32).unwrap_or(manager::TransferFunction::Srgb));
                r.luminances((d.luminances[0]*10000.0).round() as u32,d.luminances[1].round() as u32,d.luminances[2].round() as u32);
                let [rx,ry,gx,gy,bx,by,wx,wy]=d.target; r.target_primaries(rx,ry,gx,gy,bx,by,wx,wy);
                r.target_luminance((d.target_luminance[0]*10000.0).round() as u32,d.target_luminance[1].round() as u32);
                if let Some(v)=d.max_cll { r.target_max_cll(v); } if let Some(v)=d.max_fall { r.target_max_fall(v); }
                // A destructor event must wait until DataInit has attached
                // user data to the newly created info resource.
                state.loop_handle.insert_idle(move |_| { if r.is_alive() { r.done(); } });
            }
        });
    }
}
impl Dispatch<WpImageDescriptionInfoV1,()> for Gavno {
    fn request(_: &mut Self, _: &Client, _: &WpImageDescriptionInfoV1, _: info::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_,Self>) {}
}

/// Used by the renderer to match decorated, relocated and plain surface IDs.
pub fn surface_id(surface: &WlSurface) -> Id { Id::from_wayland_resource(surface) }


#[cfg(test)]
mod tests {
    use super::*;
    fn complete() -> Params { Params { tf:Some(Transfer::Srgb),primaries:Some((color::SRGB,Some(manager::Primaries::Srgb))), ..Default::default() } }
    #[test]
    fn creator_validation_table() {
        use params::Error::*;
        assert_eq!(Params::default().create().unwrap_err(),IncompleteSet);
        assert_eq!(Params { tf:Some(Transfer::Pq),..Default::default() }.create().unwrap_err(),IncompleteSet);
        for tf in [Transfer::Srgb,Transfer::Linear,Transfer::Pq,Transfer::Gamma22] {
            let p=Params { tf:Some(tf),..complete() }; assert_eq!(p.create().unwrap().tf,tf);
        }
        let mut p=complete();
        assert_eq!(p.tf(9),Err(AlreadySet));
        assert_eq!(p.primaries(1),Err(AlreadySet));
        for invalid in [0,1,3,14,u32::MAX] { assert_eq!(Params::default().tf(invalid),Err(InvalidTf)); }
        for invalid in [0,2,8,10,u32::MAX] { assert_eq!(Params::default().primaries(invalid),Err(InvalidPrimariesNamed)); }
        for named in [1,6,9] { assert!(Params::default().primaries(named).is_ok()); }
        for lum in [[0,0,80],[10000,1,80],[10000,80,1],[u32::MAX,1,1]] { assert_eq!(Params::default().luminances(lum[0],lum[1],lum[2]),Err(InvalidLuminance)); }
        assert!(p.luminances(0,80,203).is_ok()); // extended reference white is legal
        assert_eq!(p.luminances(0,80,80),Err(AlreadySet));
        assert_eq!(Params::default().mastering_luminance(10000,1),Err(InvalidLuminance));
        assert!(p.mastering_luminance(50,1000).is_ok());
        assert_eq!(p.mastering_luminance(0,1000),Err(AlreadySet));
        for (cll,fall) in [(Some(1001),None),(None,Some(1001)),(Some(0),None),(None,Some(0)),(Some(100),Some(101))] {
            let p=Params { max_cll:cll,max_fall:fall,target_luminance:Some([0.005,1000.0]),..complete() };
            assert_eq!(p.create().unwrap_err(),InvalidLuminance);
        }
        let mut p=Params { tf:Some(Transfer::Linear),primaries:Some((color::SRGB,None)),
            target:Some(color::BT2020),target_luminance:Some([0.0,4000.0]),max_cll:Some(4000),max_fall:Some(500),..Default::default() };
        assert!(p.create().is_ok());
        assert_eq!(set(&mut p.target,color::DISPLAY_P3),Err(AlreadySet));
        assert_eq!(set(&mut p.max_cll,100),Err(AlreadySet));
        assert_eq!(set(&mut p.max_fall,100),Err(AlreadySet));
        let mut pq=Params { tf:Some(Transfer::Pq),..complete() }; pq.luminances(50,1000,203).unwrap();
        assert_eq!(pq.create().unwrap().luminances,[0.005,10000.005,203.0]);
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use std::{os::unix::net::UnixStream,time::Duration};
    use wayland_client::{Connection,Dispatch,QueueHandle,protocol::{wl_registry,wl_compositor,wl_surface,wl_output,wl_subcompositor,wl_subsurface}};
    use wayland_protocols::wp::color_management::v1::client::{wp_color_manager_v1 as cm,wp_color_management_surface_v1 as cs,wp_color_management_surface_feedback_v1 as cf,wp_color_management_output_v1 as co,wp_image_description_creator_params_v1 as cp,wp_image_description_v1 as ci,wp_image_description_info_v1 as ii};
    #[derive(Default)]
    struct Peer { manager:Option<cm::WpColorManagerV1>,compositor:Option<wl_compositor::WlCompositor>,output:Option<wl_output::WlOutput>,ready:Vec<u32>,preferred:Vec<u32>,changed:usize,information:usize,features:Vec<u32>,subcompositor:Option<wl_subcompositor::WlSubcompositor> }
    impl Dispatch<wl_registry::WlRegistry,()> for Peer {
        fn event(s:&mut Self,r:&wl_registry::WlRegistry,e:wl_registry::Event,_:&(),_:&Connection,q:&QueueHandle<Self>) {
            if let wl_registry::Event::Global { name,interface,version }=e { match interface.as_str() {
                "wp_color_manager_v1"=> { assert_eq!(version,1); s.manager=Some(r.bind(name,1,q,())); },
                "wl_subcompositor"=>s.subcompositor=Some(r.bind(name,1,q,())),
                "wl_compositor"=>s.compositor=Some(r.bind(name,version.min(6),q,())),
                "wl_output"=>s.output=Some(r.bind(name,version.min(4),q,())), _=>()
            } }
        }
    }
    impl Dispatch<cm::WpColorManagerV1,()> for Peer {
        fn event(s:&mut Self,_:&cm::WpColorManagerV1,e:cm::Event,_:&(),_:&Connection,_:&QueueHandle<Self>) { if let cm::Event::SupportedFeature { feature }=e { s.features.push(raw(feature)); } }
    }
    impl Dispatch<ci::WpImageDescriptionV1,()> for Peer {
        fn event(s:&mut Self,_:&ci::WpImageDescriptionV1,e:ci::Event,_:&(),_:&Connection,_:&QueueHandle<Self>) { match e { ci::Event::Ready { identity }=>s.ready.push(identity),ci::Event::Failed { cause,msg }=>panic!("description failed: {cause:?} {msg}"),_=>() } }
    }
    impl Dispatch<cf::WpColorManagementSurfaceFeedbackV1,()> for Peer {
        fn event(s:&mut Self,_:&cf::WpColorManagementSurfaceFeedbackV1,e:cf::Event,_:&(),_:&Connection,_:&QueueHandle<Self>) { if let cf::Event::PreferredChanged { identity }=e { s.preferred.push(identity); } }
    }
    impl Dispatch<co::WpColorManagementOutputV1,()> for Peer {
        fn event(s:&mut Self,_:&co::WpColorManagementOutputV1,e:co::Event,_:&(),_:&Connection,_:&QueueHandle<Self>) { if let co::Event::ImageDescriptionChanged=e { s.changed+=1; } }
    }
    impl Dispatch<ii::WpImageDescriptionInfoV1,()> for Peer {
        fn event(s:&mut Self,_:&ii::WpImageDescriptionInfoV1,e:ii::Event,_:&(),_:&Connection,_:&QueueHandle<Self>) { if let ii::Event::Done=e { s.information+=1; } }
    }
    wayland_client::delegate_noop!(Peer:ignore wl_subcompositor::WlSubcompositor);
    wayland_client::delegate_noop!(Peer:ignore wl_subsurface::WlSubsurface);
    wayland_client::delegate_noop!(Peer:ignore wl_compositor::WlCompositor);
    wayland_client::delegate_noop!(Peer:ignore wl_surface::WlSurface);
    wayland_client::delegate_noop!(Peer:ignore wl_output::WlOutput);
    wayland_client::delegate_noop!(Peer:ignore cs::WpColorManagementSurfaceV1);
    wayland_client::delegate_noop!(Peer:ignore cp::WpImageDescriptionCreatorParamsV1);

    struct Headless { gv:Gavno,event_loop:smithay::reexports::calloop::EventLoop<'static,Gavno> }
    struct Wire { h:Headless,connection:Connection,queue:wayland_client::EventQueue<Peer>,peer:Peer,output:Output,server_client:Client }
    impl Wire {
        fn new(enabled:bool)->Self {
            let mut event_loop=smithay::reexports::calloop::EventLoop::try_new().unwrap();
            let display=smithay::reexports::wayland_server::Display::new().unwrap();
            let gv=Gavno::new(&mut event_loop,display,crate::config::Config::build(true,None,|_|None).0);
            let mut h=Headless { gv,event_loop };
            if !enabled { h.gv.protocols.color_management.reset(&h.gv.display_handle); }
            let output=Output::new("color-test".into(),smithay::output::PhysicalProperties { size:(300,200).into(),subpixel:smithay::output::Subpixel::Unknown,make:"test".into(),model:"color".into() });
            output.create_global::<Gavno>(&h.gv.display_handle);
            output.change_current_state(Some(smithay::output::Mode { size:(800,600).into(),refresh:60000 }),None,None,Some((0,0).into()));
            h.gv.space.map_output(&output,(0,0));
            let (server,client)=UnixStream::pair().unwrap(); client.set_nonblocking(true).unwrap();
            let server_client=h.gv.display_handle.insert_client(server,Arc::new(crate::state::ClientState::default())).unwrap();
            let connection=Connection::from_socket(client).unwrap(); let queue=connection.new_event_queue();
            connection.display().get_registry(&queue.handle(),());
            let mut wire=Self { h,connection,queue,peer:Peer::default(),output,server_client }; wire.exchange(); wire.exchange(); wire
        }
        fn exchange(&mut self) {
            self.connection.flush().unwrap();
            self.h.event_loop.dispatch(Some(Duration::ZERO),&mut self.h.gv).unwrap();
            self.h.gv.display_handle.flush_clients().unwrap();
            self.queue.dispatch_pending(&mut self.peer).unwrap();
            if let Some(read)=self.queue.prepare_read() { let _=read.read(); }
            self.queue.dispatch_pending(&mut self.peer).unwrap();
        }

    }
    impl Wire {
        fn assert_error(&mut self, interface: &str, code: u32) {
            let _=self.connection.flush();
            self.h.event_loop.dispatch(Some(Duration::ZERO),&mut self.h.gv).unwrap();
            self.h.gv.display_handle.flush_clients().unwrap();
            let _=self.queue.dispatch_pending(&mut self.peer);
            if let Some(read)=self.queue.prepare_read() { let _=read.read(); }
            let _=self.queue.dispatch_pending(&mut self.peer);
            let error=self.connection.protocol_error().expect("expected a protocol error");
            // A destructor request has already removed its client proxy, so
            // libwayland may no longer provide the creator interface name.
            assert!(error.object_interface == interface || (interface == "wp_image_description_creator_params_v1" && error.object_interface.is_empty())); assert_eq!(error.code,code);
            assert!(self.h.gv.features.on(Feature::ColorManagement));
        }
    }
    #[test]
    fn real_client_creator_errors() {
        use wayland_client::Proxy;
        for case in 0..8 {
            let mut w=Wire::new(true); let q=w.queue.handle();
            let p=w.peer.manager.as_ref().unwrap().create_parametric_creator(&q,());
            let code=match case {
                0=> { p.create(&q,()); params::Error::IncompleteSet },
                1=> { p.set_tf_named(cm::TransferFunction::Srgb); p.set_tf_named(cm::TransferFunction::Gamma22); params::Error::AlreadySet },
                2=> { p.set_tf_power(22000); params::Error::UnsupportedFeature },
                3=> { p.send_request(cp::Request::SetTfNamed { tf:WEnum::Unknown(0) }).unwrap(); params::Error::InvalidTf },
                4=> { p.send_request(cp::Request::SetPrimariesNamed { primaries:WEnum::Unknown(0) }).unwrap(); params::Error::InvalidPrimariesNamed },
                5=> { p.set_luminances(10000,1,80); params::Error::InvalidLuminance },
                6=> { p.set_tf_named(cm::TransferFunction::Srgb); p.set_primaries_named(cm::Primaries::Srgb); p.set_max_cll(81); p.create(&q,()); params::Error::InvalidLuminance },
                _=> { p.set_max_cll(10); p.set_max_cll(10); params::Error::AlreadySet },
            };
            w.assert_error("wp_image_description_creator_params_v1",code as u32);
        }
    }
    #[test]
    fn real_client_rejects_duplicate_controller_and_no_information() {
        let mut w=Wire::new(true); let q=w.queue.handle(); let m=w.peer.manager.as_ref().unwrap().clone();
        let s=w.peer.compositor.as_ref().unwrap().create_surface(&q,());
        m.get_surface(&s,&q,()); m.get_surface(&s,&q,());
        w.assert_error("wp_color_manager_v1",manager::Error::SurfaceExists as u32);
        let mut w=Wire::new(true); let q=w.queue.handle(); let m=w.peer.manager.as_ref().unwrap();
        let p=m.create_parametric_creator(&q,()); p.set_tf_named(cm::TransferFunction::Srgb); p.set_primaries_named(cm::Primaries::Srgb);
        let image=p.create(&q,()); w.exchange(); image.get_information(&q,());
        w.assert_error("wp_image_description_v1",image::Error::NoInformation as u32);
    }
    #[test]
    fn synchronized_subsurface_waits_for_parent_commit() {
        use wayland_client::Proxy;
        let mut w=Wire::new(true); let q=w.queue.handle();
        let compositor=w.peer.compositor.as_ref().unwrap();
        let parent=compositor.create_surface(&q,()); let child=compositor.create_surface(&q,());
        let sub=w.peer.subcompositor.as_ref().unwrap().get_subsurface(&child,&parent,&q,());
        let m=w.peer.manager.as_ref().unwrap().clone(); let controller=m.get_surface(&child,&q,());
        let p=m.create_parametric_creator(&q,()); p.set_tf_named(cm::TransferFunction::ExtLinear); p.set_primaries_named(cm::Primaries::Srgb);
        let image=p.create(&q,()); w.exchange();
        controller.set_image_description(&image,cm::RenderIntent::Perceptual); child.commit(); w.exchange();
        let server=w.server_client.object_from_protocol_id::<WlSurface>(&w.h.gv.display_handle,child.id().protocol_id()).unwrap();
        assert!(surface_description(&server).is_none()); parent.commit(); w.exchange();
        assert_eq!(surface_description(&server).unwrap().tf,Transfer::Linear);
        controller.destroy(); child.commit(); w.exchange(); assert!(surface_description(&server).is_some());
        parent.commit(); w.exchange(); assert!(surface_description(&server).is_none());
        let controller=m.get_surface(&child,&q,());
        controller.set_image_description(&image,cm::RenderIntent::Perceptual); child.commit(); w.exchange();
        w.h.gv.protocols.color_management.reset(&w.h.gv.display_handle);
        parent.commit(); w.exchange(); assert!(surface_description(&server).is_none());
        sub.destroy();
    }
    #[test]
    fn global_switch() { assert!(Wire::new(false).peer.manager.is_none()); }
    #[test]
    fn real_client_commits_descriptions_and_reads_feedback() {
        let mut w=Wire::new(true); let q=w.queue.handle(); let m=w.peer.manager.as_ref().unwrap().clone();
        assert_eq!(w.peer.features,[1,4,2,5,6]);
        let surface=w.peer.compositor.as_ref().unwrap().create_surface(&q,());
        let controller=m.get_surface(&surface,&q,());
        let feedback=m.get_surface_feedback(&surface,&q,());
        let output=m.get_output(w.peer.output.as_ref().unwrap(),&q,());
        let original=output.get_image_description(&q,()); original.get_information(&q,());
        let preferred=feedback.get_preferred_parametric(&q,()); preferred.get_information(&q,());
        let creator=m.create_parametric_creator(&q,());
        creator.set_tf_named(cm::TransferFunction::St2084Pq); creator.set_primaries_named(cm::Primaries::Bt2020);
        let content=creator.create(&q,()); w.exchange(); w.exchange();
        assert_eq!(w.peer.ready.len(),3); assert_eq!(w.peer.information,2);
        controller.set_image_description(&content,cm::RenderIntent::Perceptual); content.destroy(); w.exchange();
        // Find the server surface by the client's protocol ID.
        use wayland_client::Proxy;
        let server_surface=w.server_client.object_from_protocol_id::<WlSurface>(&w.h.gv.display_handle,surface.id().protocol_id()).unwrap();
        assert!(surface_description(&server_surface).is_none());
        surface.commit(); w.exchange();
        assert_eq!(surface_description(&server_surface).unwrap().tf,Transfer::Pq);
        output_changed(&w.output,true,crate::hdr::edid::Capabilities { max_nits:Some(600.0),min_nits:Some(0.01),..Default::default() },203.0);
        w.exchange(); assert_eq!(w.peer.changed,1); assert!(!w.peer.preferred.is_empty());
        assert_eq!(output_description(&w.output).tf,Transfer::Pq);
        let updated=output.get_image_description(&q,()); updated.get_information(&q,()); w.exchange(); w.exchange();
        assert_eq!(w.peer.information,3);
        controller.unset_image_description(); w.exchange();
        assert!(surface_description(&server_surface).is_some());
        surface.commit(); w.exchange(); assert!(surface_description(&server_surface).is_none());
        output_changed(&w.output,false,Default::default(),203.0); w.exchange(); assert_eq!(w.peer.changed,2);
    }
}
