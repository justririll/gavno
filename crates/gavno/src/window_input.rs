//! Hit testing in the same displayed geometry used by window decoration rendering.
use crate::Gavno;
use smithay::{
    desktop::{Window, WindowSurfaceType},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
};

type Pos = Point<f64, Logical>;
type Rect = Rectangle<f64, Logical>;

fn client_point(position: Pos, displayed: Rect, geometry: Rect) -> Pos {
    geometry.loc
        + Point::from((
            (position.x - displayed.loc.x) * geometry.size.w.max(1.0) / displayed.size.w.max(1.0),
            (position.y - displayed.loc.y) * geometry.size.h.max(1.0) / displayed.size.h.max(1.0),
        ))
}

impl Gavno {
    fn window_client_point(&self, window: &Window, position: Pos) -> Option<Pos> {
        let location = self.space.element_location(window)?;
        let geometry = window.geometry().to_f64();
        let fallback = Rectangle::new(location.to_f64(), geometry.size);
        // Animated geometry is drawn only by the decoration renderer; without
        // it windows are drawn plainly, so hit testing must match that.
        let motion = if self.features.on(crate::module::Feature::Decorations) {
            self.features.read(crate::module::Feature::Animations, None, || self.window_animations.motion(window))
        } else {
            None
        };
        if motion.is_some_and(|m| m.closing) {
            return None;
        }
        let mut displayed = motion.map(|m| m.rect(fallback)).unwrap_or(fallback);
        let size = displayed.size * motion.map(|m| m.scale()).unwrap_or(1.0);
        displayed.loc += Point::from((
            (displayed.size.w - size.w) * 0.5,
            (displayed.size.h - size.h) * 0.5,
        ));
        displayed.size = size;
        Some(client_point(position, displayed, geometry))
    }
    /// Smithay's default click grab saves a translation. Scaled windows need
    /// an inverse transform on every motion, even outside the captured surface.
    pub(crate) fn install_window_click_grab(&mut self, serial: smithay::utils::Serial) {
        use smithay::input::pointer::{ClickGrab, Focus};
        if self.greeter.is_some() {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if !pointer
            .with_grab(|_, grab| grab.is::<ClickGrab<Gavno>>())
            .unwrap_or(false)
        {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some((window, _, surface, origin)) = self.window_input_at(start_data.location) else {
            return;
        };
        if start_data.focus.as_ref().map(|(s, _)| s) != Some(&surface) {
            return;
        }
        let Some(local) = self.window_client_point(&window, start_data.location) else {
            return;
        };
        let offset = local - (start_data.location - origin);
        pointer.set_grab(
            self,
            WindowClickGrab {
                start_data,
                window,
                offset,
            },
            serial,
            Focus::Keep,
        );
    }
    /// Returns the mapped location for grabs and a pointer origin for client delivery.
    /// Smithay subtracts that origin from the global event position. Computing it
    /// from the inverse transform also handles subsurfaces and popup offsets.
    pub(crate) fn window_input_at(
        &self,
        position: Pos,
    ) -> Option<(Window, Point<i32, Logical>, WlSurface, Pos)> {
        if self.greeter.is_some() {
            let (window, origin) = self.space.element_under(position)?;
            let (surface, offset) =
                window.surface_under(position - origin.to_f64(), WindowSurfaceType::ALL)?;
            return Some((
                window.clone(),
                self.space.element_location(window)?,
                surface,
                (origin + offset).to_f64(),
            ));
        }
        self.space.elements().rev().find_map(|window| {
            let location = self.space.element_location(window)?;
            let local = self.window_client_point(window, position)?;
            let (surface, offset) = window.surface_under(local, WindowSurfaceType::ALL)?;
            Some((
                window.clone(),
                location,
                surface,
                position - (local - offset.to_f64()),
            ))
        })
    }
}

use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
struct WindowClickGrab {
    start_data: GrabStartData<Gavno>,
    window: Window,
    offset: Pos,
}
impl PointerGrab<Gavno> for WindowClickGrab {
    fn motion(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        let focus = self.start_data.focus.as_ref().map(|(surface, origin)| {
            let origin = data
                .window_client_point(&self.window, event.location)
                .map(|local| event.location - (local - self.offset))
                .unwrap_or(*origin);
            // Keep captured focus across workspace unmapping so releases arrive.
            (surface.clone(), origin)
        });
        handle.motion(data, focus, event);
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
            // no more buttons are pressed, release the grab
            handle.unset_grab(self, data, event.serial, event.time, false);
        }
    }

    fn axis(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
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
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<Gavno> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut Gavno) {}
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scaled_slot_routes_enlarged_content_and_button_selection_to_front_window() {
        let mut h = crate::state::tests::Harness::new();
        let lower = h.window();
        h.gv.map_new_window(lower.clone());
        h.commit_size(&lower, 1000, 800);
        h.gv.space.map_element(lower.clone(), (0, 0), true);
        h.gv.window_animations.cancel(&lower);
        let upper = h.window();
        h.gv.map_new_window(upper.clone());
        h.commit_size(&upper, 400, 200);
        h.gv.space.map_element(upper.clone(), (100, 200), true);
        let target = Rectangle::new((100, 200).into(), (800, 600).into());
        h.gv.apply_interactive_resize(&upper, target, (0.0, 0.0).into());
        // This lies beyond both actual dimensions, inside the displayed slot.
        let position = (700.0, 650.0).into();
        assert_eq!(
            h.gv.space.element_under(position).map(|(w, _)| w),
            Some(&lower)
        );
        let (selected, _, surface, origin) = h.gv.window_input_at(position).unwrap();
        assert_eq!(selected, upper);
        assert_eq!(&surface, upper.toplevel().unwrap().wl_surface());
        assert_eq!(position - origin, (300.0, 150.0).into());
        assert_eq!(
            h.gv.surface_under(position),
            Some((surface.clone(), origin))
        );
        // Button focus uses the same selection as motion delivery.
        h.gv.focus_window(Some(&selected));
        assert_eq!(h.gv.focused_window(), Some(upper.clone()));
        let pointer = h.gv.seat.get_pointer().unwrap();
        pointer.motion(
            &mut h.gv,
            Some((surface.clone(), origin)),
            &smithay::input::pointer::MotionEvent {
                location: position,
                serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                time: 1,
            },
        );
        assert_eq!(pointer.current_focus(), Some(surface.clone()));
        assert_eq!(h.pointer_positions().last(), Some(&(300.0, 150.0)));
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.button(
            &mut h.gv,
            &ButtonEvent {
                button: 0x110,
                state: smithay::backend::input::ButtonState::Pressed,
                serial,
                time: 2,
            },
        );
        h.gv.install_window_click_grab(serial);
        assert!(pointer
            .with_grab(|_, grab| grab.is::<WindowClickGrab>())
            .unwrap());
        // Captured drags keep inverse coordinates beyond the displayed rectangle.
        pointer.motion(
            &mut h.gv,
            None,
            &MotionEvent {
                location: (1100.0, 1100.0).into(),
                serial,
                time: 3,
            },
        );
        assert_eq!(h.pointer_positions().last(), Some(&(500.0, 300.0)));
        assert_eq!(pointer.current_location(), (1100.0, 1100.0).into());
        h.gv.space.unmap_elem(&upper);
        pointer.motion(
            &mut h.gv,
            None,
            &MotionEvent {
                location: (1150.0, 1100.0).into(),
                serial,
                time: 4,
            },
        );
        assert_eq!(pointer.current_focus(), Some(surface));
        pointer.button(
            &mut h.gv,
            &ButtonEvent {
                button: 0x110,
                state: smithay::backend::input::ButtonState::Released,
                serial,
                time: 4,
            },
        );
        assert!(!pointer.is_grabbed());
        // Points outside the displayed slot still reach the window below.
        assert_eq!(
            h.gv.window_input_at((950.0, 650.0).into()).unwrap().0,
            lower
        );
        assert!(!h.gv.window_animations.is_animating());
    }
    #[test]
    fn inverse_resize_transform_includes_geometry_and_subsurface_offsets() {
        let displayed = Rectangle::new((100.0, 200.0).into(), (800.0, 600.0).into());
        let actual = Rectangle::new((12.0, 24.0).into(), (400.0, 200.0).into());
        let position = (700.0, 650.0).into();
        let local = client_point(position, displayed, actual);
        assert_eq!(local, (312.0, 174.0).into());
        let offset = Point::from((30.0, 40.0));
        let origin = position - (local - offset);
        assert_eq!(position - origin, (282.0, 134.0).into());
    }
}
