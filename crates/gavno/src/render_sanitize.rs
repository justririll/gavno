//! Reject empty physical rectangles before Smithay builds DRM plane damage.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
        Renderer,
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Transform},
};

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
