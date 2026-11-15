//! Regression tests drive the production input handlers without a display backend.
use super::*;
use smithay::{
    backend::input::{AxisRelativeDirection, Device, DeviceCapability, UnusedEvent},
    output::{Mode, Output, PhysicalProperties, Subpixel},
};

struct TestBackend;
#[derive(PartialEq, Eq, Hash)]
struct TestDevice;
impl Device for TestDevice {
    fn id(&self) -> String { "test".into() }
    fn name(&self) -> String { "test".into() }
    fn has_capability(&self, _: DeviceCapability) -> bool { true }
    fn usb_id(&self) -> Option<(u32, u32)> { None }
    fn syspath(&self) -> Option<std::path::PathBuf> { None }
}
struct Button(u32, ButtonState);
struct Scroll;
impl Event<TestBackend> for Button {
    fn time(&self) -> u64 { 1000 }
    fn device(&self) -> TestDevice { TestDevice }
}
impl PointerButtonEvent<TestBackend> for Button {
    fn button_code(&self) -> u32 { self.0 }
    fn state(&self) -> ButtonState { self.1 }
}
impl Event<TestBackend> for Scroll {
    fn time(&self) -> u64 { 1000 }
    fn device(&self) -> TestDevice { TestDevice }
}
impl PointerAxisEvent<TestBackend> for Scroll {
    fn amount(&self, axis: Axis) -> Option<f64> { Some(if axis == Axis::Vertical { 10.0 } else { 0.0 }) }
    fn amount_v120(&self, _: Axis) -> Option<f64> { None }
    fn source(&self) -> AxisSource { AxisSource::Finger }
    fn relative_direction(&self, _: Axis) -> AxisRelativeDirection { AxisRelativeDirection::Identical }
}
impl InputBackend for TestBackend {
    type Device = TestDevice;
    type PointerButtonEvent = Button;
    type PointerAxisEvent = Scroll;
    type KeyboardKeyEvent = UnusedEvent;
    type PointerMotionEvent = UnusedEvent;
    type PointerMotionAbsoluteEvent = UnusedEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = UnusedEvent;
}
fn setup() -> (crate::state::tests::Harness, Output) {
    let mut h = crate::state::tests::Harness::new();
    let output = Output::new("test".into(), PhysicalProperties {
        size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "test".into(), model: "test".into(),
    });
    output.change_current_state(Some(Mode { size: (1600, 900).into(), refresh: 60000 }), None, None, None);
    h.gv.space.map_output(&output, (0, 0));
    h.gv.workspaces.ensure(&output);
    (h, output)
}
fn motion(gv: &mut Gavno, focus: Option<(smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, Point<f64, Logical>)>, location: Point<f64, Logical>) {
    let pointer = gv.seat.get_pointer().unwrap();
    pointer.motion(gv, focus, &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: 1 });
}
fn button(gv: &mut Gavno, button: u32, state: ButtonState) {
    gv.on_pointer_button::<TestBackend>(Button(button, state));
}

#[test]
fn client_releases_survive_overview_and_workspace_interception() {
    for overview in [false, true] {
        let (mut h, output) = setup();
        let window = h.window();
        h.gv.map_new_window(window.clone());
        motion(&mut h.gv, Some((window.toplevel().unwrap().wl_surface().clone(), (0.0, 0.0).into())), (500.0, 400.0).into());
        let pointer = h.gv.seat.get_pointer().unwrap();
        // Two buttons ensure all releases, not only the grab's starting button, arrive.
        button(&mut h.gv, BTN_LEFT, ButtonState::Pressed);
        button(&mut h.gv, BTN_RIGHT, ButtonState::Pressed);
        assert!(pointer.is_grabbed());
        if overview { h.gv.toggle_overview(); }
        else { assert!(h.gv.switch_workspace(&output, 1)); }
        assert!(pointer.is_grabbed(), "client grab must survive until releases");
        button(&mut h.gv, BTN_LEFT, ButtonState::Released);
        assert!(pointer.is_grabbed());
        button(&mut h.gv, BTN_RIGHT, ButtonState::Released);
        assert!(!pointer.is_grabbed());
        assert!(h.gv.pointer_buttons.is_empty());
        // A new click must also finish: no stale Smithay pressed-button state.
        h.gv.overview.progress.jump(0.0);
        h.gv.workspaces.ensure(&output).position.jump(1.0);
        pointer.button(&mut h.gv, &ButtonEvent { button: BTN_LEFT, state: ButtonState::Pressed, serial: SERIAL_COUNTER.next_serial(), time: 2 });
        pointer.button(&mut h.gv, &ButtonEvent { button: BTN_LEFT, state: ButtonState::Released, serial: SERIAL_COUNTER.next_serial(), time: 3 });
        assert!(!pointer.is_grabbed());
    }
}

#[test]
fn workspace_changes_end_move_and_resize_before_unmapping() {
    for resize in [false, true] {
        for transfer in [false, true] {
            let (mut h, output) = setup();
            let window = h.window();
            h.gv.map_new_window(window.clone());
            let location = h.gv.space.element_location(&window).unwrap();
            let pointer = h.gv.seat.get_pointer().unwrap();
            let start = GrabStartData { focus: None, button: BTN_LEFT, location: (100.0, 100.0).into() };
            if resize {
                let grab = ResizeSurfaceGrab::start(start, window.clone(), ResizeEdge::TOP_LEFT, Rectangle::new(location, (400, 300).into()));
                pointer.set_grab(&mut h.gv, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
            } else {
                let grab = MoveSurfaceGrab::start(start, window.clone(), location);
                pointer.set_grab(&mut h.gv, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
            }
            motion(&mut h.gv, None, (160.0, 160.0).into());
            assert!(pointer.is_grabbed());
            if transfer { assert!(h.gv.move_to_workspace(&window, 1)); }
            else { assert!(h.gv.switch_workspace(&output, 1)); }
            assert!(!pointer.is_grabbed());
            assert!(h.gv.space.element_location(&window).is_none());
            motion(&mut h.gv, None, (200.0, 200.0).into());
            assert!(h.gv.space.element_location(&window).is_none(), "old grab must not remap a hidden window");
            assert!(!window.toplevel().unwrap().with_pending_state(|s| s.states.contains(xdg_toplevel::State::Resizing)));
        }
    }
}

#[test]
fn scrolling_cancels_super_tap_in_clients_shell_and_overview() {
    for route in 0..3 {
        let (mut h, _) = setup();
        let pos = if route == 1 { (500.0, 5.0) } else { (500.0, 400.0) };
        motion(&mut h.gv, None, pos.into());
        if route == 1 { assert!(h.gv.shell_axis(0.0, 0.0)); }
        if route == 2 { h.gv.toggle_overview(); }
        assert!(!h.gv.gestures.super_tap.key(125, true, true, false));
        h.gv.on_pointer_axis::<TestBackend>(Scroll);
        assert!(!h.gv.gestures.super_tap.key(125, true, false, false));
    }
}
