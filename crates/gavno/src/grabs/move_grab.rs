use crate::{
    window_mode::{mode_state, WindowMode},
    Gavno,
};
use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point, Rectangle},
};

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<Gavno>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    moved: bool,
}

impl MoveSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<Gavno>,
        window: Window,
        initial_window_location: Point<i32, Logical>,
    ) -> Self {
        Self {
            start_data,
            window,
            initial_window_location,
            moved: false,
        }
    }
}

impl PointerGrab<Gavno> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        crate::grabs::resize_grab::clear_anchor(&self.window);
        data.window_animations.cancel(&self.window);
        let delta = event.location - self.start_data.location;
        if !self.window.alive() {
            return;
        }
        if !self.moved && delta.x.hypot(delta.y) < 4.0 {
            return;
        }
        if !self.moved {
            self.moved = true;
            let state = mode_state(&self.window);
            if matches!(state.mode, WindowMode::Maximized | WindowMode::Snapped(_)) {
                let size = self.window.geometry().size;
                let restored = state.restore.map(|r| r.size).unwrap_or(size);
                self.initial_window_location = crate::snap::restored_drag_origin(
                    self.start_data.location,
                    Rectangle::new(self.initial_window_location, size),
                    restored,
                );
                data.set_window_mode(&self.window, WindowMode::Floating, None);
                data.window_animations.cancel(&self.window);
            }
        }
        if data.tiled(&self.window) { data.workspaces.moving_tile = Some(self.window.clone()); data.preview_tile_drop(&self.window,event.location); }
        let new_location = self.initial_window_location.to_f64() + delta;
        data.space
            .map_element(self.window.clone(), new_location.to_i32_round(), true);
        if !data.tiling_workspace(&self.window) { data.update_snap_preview(&self.window, event.location); }
        data.queue_redraw_all();
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
            if self.moved && self.window.alive() {
                if data.tiled(&self.window) { data.drop_tile(&self.window,handle.current_location()); }
                else if !data.tiling_workspace(&self.window) { data.apply_drag_snap(&self.window, handle.current_location()); }
            }
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
        data.workspaces.moving_tile = None;
        if data.tiled(&self.window) { data.drop_tile(&self.window,(-1e9,-1e9).into()); }
        data.overlay.hide_preview(data.start_time.elapsed());
        data.queue_redraw_all();
    }
}
