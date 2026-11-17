//! Reject empty physical rectangles before Smithay builds DRM plane damage.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
        Renderer,
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Transform},
};

/// Last applied damage commit, including synchronized subsurface commits.
#[derive(Default)]
struct ShmDamage {
    commit: Option<CommitCounter>,
}

impl ShmDamage {
    fn record(&mut self, kind: Option<smithay::backend::renderer::BufferType>,
              commit: CommitCounter, damage: impl IntoIterator<Item = Rectangle<i32, Buffer>>) -> u64 {
        if self.commit == Some(commit) { return 0; }
        self.commit = Some(commit);
        if !matches!(kind, Some(smithay::backend::renderer::BufferType::Shm)) { return 0; }
        damage.into_iter().fold(0u64, |bytes, rect| {
            let pixels = (rect.size.w.max(0) as u64).saturating_mul(rect.size.h.max(0) as u64);
            bytes.saturating_add(pixels.saturating_mul(4))
        })
    }
}

#[derive(Default)]
struct ShmFrame(std::cell::Cell<u64>);

/// Assign damage when committed, rather than when first seen by a renderer.
/// Hidden or newly moved surfaces cannot carry old damage into a later frame.
pub fn add_output_shm_bytes(output: &smithay::output::Output, bytes: u64) {
    let pending = output.user_data().get_or_insert(ShmFrame::default);
    pending.0.set(pending.0.get().saturating_add(bytes));
}

pub fn take_output_shm_bytes(output: &smithay::output::Output) -> u64 {
    output.user_data().get::<ShmFrame>().map_or(0, |pending| pending.0.replace(0))
}

/// Smithay applies synchronized subsurface commits when their parent commits.
/// Walk the same applied tree, after on_commit_buffer_handler, so pending child
/// damage is neither counted early nor lost. No polling, I/O or GPU access.
pub fn record_shm_commit(surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) -> u64 {
    use smithay::{backend::renderer::{buffer_type, utils::RendererSurfaceStateUserData},
        wayland::compositor::{is_sync_subsurface, with_surface_tree_upward, TraversalAction}};
    if is_sync_subsurface(surface) { return 0; }
    let mut bytes = 0u64;
    with_surface_tree_upward(surface, (), |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            let Some(renderer) = states.data_map.get::<RendererSurfaceStateUserData>() else { return; };
            let renderer = renderer.lock().unwrap();
            let counter = states.data_map.get_or_insert(|| std::cell::RefCell::new(ShmDamage::default()));
            let mut counter = counter.borrow_mut();
            let commit = renderer.current_commit();
            if counter.commit == Some(commit) { return; }
            let kind = renderer.buffer().and_then(|b| buffer_type(b));
            let damage = renderer.damage_since(counter.commit);
            bytes = bytes.saturating_add(counter.record(kind, commit, damage));
        }, |_, _, _| true);
    bytes
}

/// Smithay 0.7 preserves empty physical damage after scaling and clipping.
/// Passing it to FB_DAMAGE_CLIPS rejects the entire flip: the msm kernel logged
/// `invalid damage clip 324 226 2553 226`, freezing the pointer in a retry loop.
/// Filter after physical rounding, including compositor-owned and HDR elements,
/// while preserving identity and underlying storage for direct scanout.
#[derive(Debug)]
pub struct SanitizedElement<E>(E);

impl<E: Element> SanitizedElement<E> {
    pub fn new(element: E, scale: Scale<f64>) -> Option<Self> {
        has_area(&element.geometry(scale)).then_some(Self(element))
    }
}

fn has_area(rect: &Rectangle<i32, Physical>) -> bool {
    rect.size.w > 0 && rect.size.h > 0
}

impl<E: Element> Element for SanitizedElement<E> {
    fn id(&self) -> &Id {
        self.0.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.0.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.0.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.0.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.0.location(scale)
    }

    fn transform(&self) -> Transform {
        self.0.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.0
            .damage_since(scale, commit)
            .into_iter()
            .filter(has_area)
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.0
            .opaque_regions(scale)
            .into_iter()
            .filter(has_area)
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.0.alpha()
    }

    fn kind(&self) -> Kind {
        self.0.kind()
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for SanitizedElement<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        self.0.draw(frame, src, dst, damage, opaque_regions)
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.0.underlying_storage(renderer)
    }
}

#[cfg(test)]
mod tests;
