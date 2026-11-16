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
struct Key(u32, KeyState);
impl Event<TestBackend> for Key {
    fn time(&self) -> u64 { 1000 }
    fn device(&self) -> TestDevice { TestDevice }
}
impl KeyboardKeyEvent<TestBackend> for Key {
    fn key_code(&self) -> smithay::input::keyboard::Keycode { self.0.into() }
    fn state(&self) -> KeyState { self.1 }
    fn count(&self) -> u32 { u32::from(self.1 == KeyState::Pressed) }
}
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
    type KeyboardKeyEvent = Key;
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

#[test]
fn lock_key_panic_keeps_input_intercepted_and_gate_closed() {
    let (mut h, _) = setup();
    let w = h.window(); h.gv.map_new_window(w.clone());
    h.gv.focus_window(Some(&w));
    assert!(h.gv.lock_session());
    assert!(h.keyboard_keys().is_empty());
    h.gv.lock.panic_key = true;
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Pressed));
    assert!(h.gv.protocols.locks.locked);
    assert!(h.gv.lock.internal());
    assert!(h.gv.suppressed_keys.contains(&38));
    assert!(h.gv.seat.get_keyboard().unwrap().current_focus().is_none());
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Released));
    h.gv.refresh_internal_lock();
    assert!(h.gv.protocols.locks.locked);
    assert!(h.gv.lock.internal());
    assert!(h.gv.seat.get_keyboard().unwrap().current_focus().is_none());
    assert!(h.keyboard_keys().is_empty(), "lock input reached the client");
}

#[test]
fn overview_crash_allows_a_real_client_click() {
    let (mut h, _) = setup(); let w = h.window(); h.gv.map_new_window(w.clone()); h.commit_size(&w, 400, 300);
    h.gv.window_animations.entries.clear();
    h.gv.toggle_overview(); h.gv.overview.progress.jump(1.0);
    h.gv.features.inject_panic(Feature::Overview);
    h.gv.overview_motion((500.0, 400.0).into());
    assert!(!h.gv.overview.visible());
    let p = h.gv.space.element_location(&w).unwrap().to_f64() + Point::from((20.0, 20.0));
    let under = h.gv.surface_under(p);
    motion(&mut h.gv, under, p);
    button(&mut h.gv, BTN_LEFT, ButtonState::Pressed);
    button(&mut h.gv, BTN_LEFT, ButtonState::Released);
    assert_eq!(h.pointer_buttons(), vec![BTN_LEFT, BTN_LEFT]);
}

#[test]
fn disabled_feature_bindings_reach_the_real_client() {
    use crate::module::Features;
    for id in Feature::ALL {
        let (mut h, _) = setup(); let w = h.window(); h.gv.map_new_window(w.clone()); h.commit_size(&w, 400, 300);
        let m = &mut h.gv.config.modules;
        match id {
            Feature::Dock => m.dock = false, Feature::Bar => m.bar = false,
            Feature::Overview => m.overview = false, Feature::Tiling => m.tiling = false,
            Feature::Snap => m.snap = false, Feature::Switcher => m.switcher = false,
            Feature::Gestures => m.gestures = false, Feature::WindowGestures => m.window_gestures = false,
            Feature::ResizeMode => m.resize_mode = false, Feature::Animations => m.animations = false,
            Feature::Decorations => m.decorations = false, Feature::Ipc => m.ipc = false,
            Feature::Screencopy => m.screencopy = false,
            Feature::Launcher => m.launcher = false,
            Feature::Notifications => m.notifications = false,
        }
        h.gv.features = Features::new(&h.gv.config);
        h.gv.focus_window(Some(&w));
        let (key, modifier, ctrl) = match id {
            Feature::Dock => (58, Some(64), false),
            Feature::Overview => (38, Some(64), false),
            Feature::Tiling => (28, Some(64), false),
            Feature::Snap => (113, Some(64), true),
            Feature::Switcher => { h.gv.config.mod_key = crate::config::ModKey::Super; (23, Some(64), false) }
            Feature::ResizeMode => (27, Some(64), false),
            Feature::Gestures => (133, None, false),
            _ => (38, None, false),
        };
        if let Some(modifier) = modifier { h.gv.on_keyboard::<TestBackend>(Key(modifier, KeyState::Pressed)); }
        if ctrl { h.gv.on_keyboard::<TestBackend>(Key(37, KeyState::Pressed)); }
        h.keyboard_keys();
        h.gv.on_keyboard::<TestBackend>(Key(key, KeyState::Pressed));
        h.gv.on_keyboard::<TestBackend>(Key(key, KeyState::Released));
        assert_eq!(h.keyboard_keys(), vec![key - 8, key - 8], "{id:?} swallowed a disabled binding");
        if ctrl { h.gv.on_keyboard::<TestBackend>(Key(37, KeyState::Released)); }
        if let Some(modifier) = modifier { h.gv.on_keyboard::<TestBackend>(Key(modifier, KeyState::Released)); }
    }
}

#[test]
fn tiling_panic_inside_pointer_grab_recovers_without_reentering_the_seat() {
    use smithay::input::pointer::Focus;
    let (mut h, output) = setup();
    let a = h.window(); let b = h.window();
    h.gv.map_new_window(a.clone()); h.gv.map_new_window(b.clone());
    h.gv.toggle_workspace_tiling(&output);
    h.gv.window_animations.entries.clear();
    let owner = h.gv.workspaces.get(&output).unwrap();
    let split = owner.list.items[owner.list.active].tree.layout(h.gv.work_area(&output), h.gv.config.gap).1.remove(0);
    let pointer = h.gv.seat.get_pointer().unwrap();
    let start = GrabStartData { focus: None, button: BTN_LEFT, location: (800.0, 400.0).into() };
    pointer.set_grab(&mut h.gv, crate::grabs::tiling_grab::SplitterGrab::new(start, output, split), SERIAL_COUNTER.next_serial(), Focus::Clear);
    h.gv.features.inject_panic(Feature::Tiling);
    motion(&mut h.gv, None, (820.0, 400.0).into());
    h.event_loop.dispatch(std::time::Duration::ZERO, &mut h.gv).unwrap();
    assert!(!h.gv.features.on(Feature::Tiling));
    assert!(!pointer.is_grabbed());
    assert!(h.gv.pointer_buttons.is_empty());
    for w in [a, b] { assert!(h.gv.space.element_location(&w).is_some()); assert!(!h.gv.tiled(&w)); }
}
