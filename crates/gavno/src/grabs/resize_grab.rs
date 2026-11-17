use crate::Gavno;
use smithay::{
    desktop::{Space, Window},
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
        GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel, wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::compositor,
};
use std::cell::RefCell;

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;

        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();

        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits_truncate(x as u32)
    }
}

pub struct ResizeSurfaceGrab {
    start_data: PointerGrabStartData<Gavno>,
    window: Window,

    edges: ResizeEdge,

    initial_rect: Rectangle<i32, Logical>,
    last_window_size: Size<i32, Logical>,
}

impl ResizeSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<Gavno>,
        window: Window,
        edges: ResizeEdge,
        initial_window_rect: Rectangle<i32, Logical>,
    ) -> Self {
        crate::window_mode::cancel_natural_center(&window);
        let initial_rect = initial_window_rect;

        if let Some(t)=window.toplevel() {
            ResizeSurfaceState::with(t.wl_surface(), |state| { *state = ResizeSurfaceState::Resizing { edges, initial_rect }; });
        }

        Self {
            start_data,
            window,
            edges,
            initial_rect,
            last_window_size: initial_rect.size,
        }
    }
}

impl PointerGrab<Gavno> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        let delta=event.location-self.start_data.location;
        let requested=crate::window_gesture::resized(self.initial_rect,self.edges,delta);
        data.apply_interactive_resize(&self.window,requested,crate::window_gesture::anchor_for(self.edges));
        self.last_window_size=data.interactive_rect(&self.window).map(|r|r.size).unwrap_or(self.initial_rect.size);
    }

    fn relative_motion(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if handle.current_pressed().is_empty() {
            // No more buttons are pressed, release the grab.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(&mut self, data: &mut Gavno, handle: &mut PointerInnerHandle<'_, Gavno>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<Gavno> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut Gavno) {
        let Some(xdg) = self.window.toplevel() else { return };
        xdg.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Resizing);
            state.size = Some(self.last_window_size);
        });
        xdg.send_pending_configure();
        // The final client commit still keeps the edge opposite the drag anchored.
        if directed_rect(&self.window).is_some() {
            finish_directed(&self.window);
        } else {
            ResizeSurfaceState::with(xdg.wl_surface(), |state| {
                *state = ResizeSurfaceState::WaitingForLastCommit {
                    edges: self.edges,
                    initial_rect: self.initial_rect,
                };
            });
        }
        data.queue_redraw_all();
    }
}

/// State of the resize operation.
///
/// It is stored inside of WlSurface,
/// and can be accessed using [`ResizeSurfaceState::with`]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum ResizeSurfaceState {
    #[default]
    Idle,
    Directed { rect: Rectangle<i32, Logical>, anchor: Point<f64, Logical>, axes: [bool; 2], active: bool },
    Resizing {
        edges: ResizeEdge,
        /// The initial window size and location.
        initial_rect: Rectangle<i32, Logical>,
    },
    /// Resize is done, we are now waiting for last commit, to do the final move
    WaitingForLastCommit {
        edges: ResizeEdge,
        /// The initial window size and location.
        initial_rect: Rectangle<i32, Logical>,
    },
}

impl ResizeSurfaceState {
    fn with<F, T>(surface: &WlSurface, cb: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<Self>::default);
            let state = states.data_map.get::<RefCell<Self>>().unwrap();

            cb(&mut state.borrow_mut())
        })
    }

    fn commit(&mut self) -> Option<(ResizeEdge, Rectangle<i32, Logical>)> {
        match *self {
            Self::Resizing { edges, initial_rect } => Some((edges, initial_rect)),
            Self::WaitingForLastCommit { edges, initial_rect } => {
                // The resize is done, let's go back to idle
                *self = Self::Idle;

                Some((edges, initial_rect))
            }
            Self::Idle | Self::Directed { .. } => None,
        }
    }
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface) -> Option<()> {
    let window = space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t|t.wl_surface()==surface))
        .cloned()?;

    let mut window_loc = space.element_location(&window)?;
    let geometry = window.geometry();

    let directed = ResizeSurfaceState::with(surface, |state| match *state {
        ResizeSurfaceState::Directed { rect, anchor, .. } => Some(Point::from((
            rect.loc.x + ((rect.size.w - geometry.size.w) as f64 * anchor.x).round() as i32,
            rect.loc.y + ((rect.size.h - geometry.size.h) as f64 * anchor.y).round() as i32,
        ))),
        _ => None,
    });
    if let Some(loc) = directed {
        space.map_element(window, loc, false);
        return Some(());
    }
    let new_loc: Point<Option<i32>, Logical> = ResizeSurfaceState::with(surface, |state| {
        state
            .commit()
            .and_then(|(edges, initial_rect)| {
                // If the window is being resized by top or left, its location must be adjusted
                // accordingly.
                edges.intersects(ResizeEdge::TOP_LEFT).then(|| {
                    let new_x = edges
                        .intersects(ResizeEdge::LEFT)
                        .then_some(initial_rect.loc.x + (initial_rect.size.w - geometry.size.w));

                    let new_y = edges
                        .intersects(ResizeEdge::TOP)
                        .then_some(initial_rect.loc.y + (initial_rect.size.h - geometry.size.h));

                    (new_x, new_y).into()
                })
            })
            .unwrap_or_default()
    });

    if let Some(new_x) = new_loc.x {
        window_loc.x = new_x;
    }
    if let Some(new_y) = new_loc.y {
        window_loc.y = new_y;
    }

    if new_loc.x.is_some() || new_loc.y.is_some() {
        // If TOP or LEFT side of the window got resized, we have to move it
        space.map_element(window, window_loc, false);
    }

    Some(())
}

/// Keep the requested anchor even when a client commits a different size.
pub fn directed_anchor(surface: &WlSurface, rect: Rectangle<i32, Logical>, anchor: Point<f64, Logical>, previous: Rectangle<i32, Logical>) {
    ResizeSurfaceState::with(surface, |state| {
        let mut axes = match *state {
            ResizeSurfaceState::Directed { axes, active: true, .. } => axes,
            _ => [false; 2],
        };
        axes[0] |= rect.size.w != previous.size.w;
        axes[1] |= rect.size.h != previous.size.h;
        *state = ResizeSurfaceState::Directed { rect, anchor, axes, active: true };
    });
}
pub fn clear_anchor(window: &Window) {
    if let Some(t) = window.toplevel() {
        ResizeSurfaceState::with(t.wl_surface(), |state| *state = ResizeSurfaceState::Idle);
    }
}

pub fn finish_directed(window: &Window) {
    if let Some(t)=window.toplevel() {ResizeSurfaceState::with(t.wl_surface(), |s| {
        if let ResizeSurfaceState::Directed {active,..}=s {*active=false;}
    });}
}
/// Retire the temporary scale only after the client acknowledges the final size.
pub fn directed_finished(window: &Window) -> bool {
    let Some(t)=window.toplevel() else {return false};
    let state=ResizeSurfaceState::with(t.wl_surface(), |s|*s);
    let current=t.current_state();
    matches!(state,ResizeSurfaceState::Directed {active:false,rect,..} if current.size==Some(rect.size) && !current.states.contains(xdg_toplevel::State::Resizing))
}

/// While held, subsequent deltas start from the requested geometry rather than
/// an older client commit. After the final acknowledgement accept its actual size.
pub fn directed_rect(window: &Window) -> Option<Rectangle<i32, Logical>> {
    let t=window.toplevel()?;
    let state=ResizeSurfaceState::with(t.wl_surface(), |s|*s);
    match state {
        ResizeSurfaceState::Directed {rect,anchor,axes,active} => {
            Some(if !active && t.current_state().size==Some(rect.size) && !t.current_state().states.contains(xdg_toplevel::State::Resizing) {
                accepted_rect(rect, window.geometry().size, anchor, axes)
            } else {rect})
        }
        _=>None,
    }
}

/// A final buffer may change the dragged dimensions, while untouched axes stay displayed.
fn accepted_rect(rect: Rectangle<i32, Logical>, actual: Size<i32, Logical>, anchor: Point<f64, Logical>, axes: [bool; 2]) -> Rectangle<i32, Logical> {
    let size = (
        if axes[0] { actual.w.max(1) } else { rect.size.w },
        if axes[1] { actual.h.max(1) } else { rect.size.h },
    ).into();
    crate::window_gesture::anchored_size(rect, size, anchor)
}

/// Accept the final buffer without exposing changes on untouched axes.
pub fn settle_directed(gv: &mut Gavno, window: &Window) {
    if !directed_finished(window) { return; }
    let Some(rect) = directed_rect(window) else { return };
    gv.features.call(crate::module::Feature::Animations, (), || gv.window_animations.cancel(window));
    clear_anchor(window);
    if window.geometry().size != rect.size {
        gv.features.call(crate::module::Feature::Animations, (), || gv.window_animations.geometry_immediate(window, rect.to_f64()));
    }
    if gv.space.element_location(window).is_some() {
        gv.space.map_element(window.clone(), rect.loc, false);
        gv.save_workspace_location(window, rect.loc);
    }
    if let Some(t) = window.toplevel() { t.with_pending_state(|s| s.size = Some(rect.size)); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_ack_preserves_untouched_axes_for_all_floating_resize_edges() {
        let initial = Rectangle::new((100, 200).into(), (800, 600).into());
        for edge in [ResizeEdge::LEFT, ResizeEdge::RIGHT, ResizeEdge::TOP, ResizeEdge::BOTTOM, ResizeEdge::TOP_LEFT, ResizeEdge::BOTTOM_RIGHT] {
            let mut h = crate::state::tests::Harness::new();
            let w = h.window();
            h.gv.map_new_window(w.clone());
            h.commit_size(&w, 800, 600);
            h.gv.space.map_element(w.clone(), initial.loc, true);
            let target = crate::window_gesture::resized(initial, edge, (100.0, 80.0).into());
            h.gv.apply_interactive_resize(&w, target, crate::window_gesture::anchor_for(edge));
            h.gv.finish_interactive_resize(&w);
            h.commit_size(&w, 650, 450);
            assert!(!directed_finished(&w), "final acknowledgement was processed");
            let shown = h.gv.interactive_rect(&w).unwrap();
            if !edge.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
                assert_eq!((shown.loc.x, shown.size.w), (100, 800));
            } else if edge.contains(ResizeEdge::LEFT) {
                assert_eq!(shown.loc.x + shown.size.w, 900);
            } else { assert_eq!(shown.loc.x, 100); }
            if !edge.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
                assert_eq!((shown.loc.y, shown.size.h), (200, 600));
            } else if edge.contains(ResizeEdge::TOP) {
                assert_eq!(shown.loc.y + shown.size.h, 800);
            } else { assert_eq!(shown.loc.y, 200); }
            let fallback = Rectangle::new(h.gv.space.element_location(&w).unwrap(), w.geometry().size).to_f64();
            assert_eq!(h.gv.window_animations.motion(&w).unwrap().rect(fallback), shown.to_f64());
            assert!(!h.gv.window_animations.is_animating());
        }
    }
}
