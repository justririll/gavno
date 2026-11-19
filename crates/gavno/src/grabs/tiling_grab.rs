//! Pointer grabs that move absolute splitter coordinates without springs.
use crate::{
    grabs::resize_grab::ResizeEdge,
    tiling::{Axis, Splitter},
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
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

pub struct SplitterGrab {
    start_data: PointerGrabStartData<Gavno>,
    output: Output,
    splits: Vec<Splitter>,
    /// Leaves of the tree when the drag started. Splits are addressed by
    /// tree path, so a changed topology would retarget another split.
    leaves: Option<Vec<Window>>,
}
impl SplitterGrab {
    pub fn new(start_data: PointerGrabStartData<Gavno>, output: Output, split: Splitter) -> Self {
        Self {
            start_data,
            output,
            splits: vec![split],
            leaves: None,
        }
    }
    pub fn for_tile(
        gv: &Gavno,
        start_data: PointerGrabStartData<Gavno>,
        window: &Window,
        edges: ResizeEdge,
    ) -> Option<Self> {
        gv.features.read(crate::module::Feature::Tiling, None, || {
            use crate::snap::Direction;
            let (output, i) = gv.workspaces.membership(window)?;
            let tree = &gv.workspaces.get(&output)?.list.items[i].tree;
            let area = gv.work_area(&output);
            let mut splits = Vec::new();
            for (edge, dir) in [
                (ResizeEdge::LEFT, Direction::Left),
                (ResizeEdge::RIGHT, Direction::Right),
                (ResizeEdge::TOP, Direction::Up),
                (ResizeEdge::BOTTOM, Direction::Down),
            ] {
                if edges.contains(edge) {
                    if let Some(s) = tree.side(window, dir, area, gv.config.gap) {
                        splits.push(s);
                    }
                }
            }
            Some(Self {
                start_data,
                output,
                splits,
                leaves: None,
            })
        })
    }
}
impl PointerGrab<Gavno> for SplitterGrab {
    fn motion(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        let mut stale = false;
        data.guard(crate::module::Feature::Tiling, (), |data| {
            handle.motion(data, None, event);
            let delta = event.location - self.start_data.location;
            let area = data.work_area(&self.output);
            let gap = data.config.gap;
            let owner = data.workspaces.ensure(&self.output);
            let tree = &mut owner.list.items[owner.list.active].tree;
            let leaves: Vec<Window> = tree.layout(area, gap).0.into_iter().map(|(w, _)| w).collect();
            match &self.leaves {
                None => self.leaves = Some(leaves),
                Some(saved) if *saved != leaves => {
                    stale = true;
                    return;
                }
                Some(_) => {}
            }
            let mut changed = false;
            for original in &self.splits {
                let desired = original.cut.saturating_add(
                    match original.axis {
                        Axis::X => delta.x,
                        Axis::Y => delta.y,
                    }
                    .round() as i32,
                );
                if let Some(s) = tree
                    .layout(area, gap)
                    .1
                    .into_iter()
                    .find(|s| s.path == original.path)
                {
                    changed |= tree.move_split(&s.path, desired.saturating_sub(s.cut), area, gap);
                }
            }
            if changed {
                let primary = self.splits.first().and_then(|original| {
                    tree.layout(area, gap)
                        .1
                        .into_iter()
                        .find(|s| s.path == original.path)
                });
                if let Some(split) = primary {
                    use smithay::input::pointer::{CursorIcon, CursorImageStatus};
                    data.set_cursor_status(CursorImageStatus::Named(match split.axis {
                        Axis::X => CursorIcon::ColResize,
                        Axis::Y => CursorIcon::RowResize,
                    }));
                    data.workspaces.splitter_hover = Some((self.output.clone(), split));
                }
                data.reflow_tiling(false);
            }
        });
        if stale {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }
    fn button(
        &mut self,
        data: &mut Gavno,
        handle: &mut PointerInnerHandle<'_, Gavno>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
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
        data.workspaces.splitter_hover = None;
        data.set_cursor_status(smithay::input::pointer::CursorImageStatus::default_named());
        data.queue_redraw_all();
    }
}
