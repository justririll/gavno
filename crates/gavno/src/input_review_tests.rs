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
    type PointerMotionEvent = WakeEvent;
    type PointerMotionAbsoluteEvent = WakeEvent;
    type GestureSwipeBeginEvent = WakeEvent;
    type GestureSwipeUpdateEvent = WakeEvent;
    type GestureSwipeEndEvent = WakeEvent;
    type GesturePinchBeginEvent = WakeEvent;
    type GesturePinchUpdateEvent = WakeEvent;
    type GesturePinchEndEvent = WakeEvent;
    type GestureHoldBeginEvent = WakeEvent;
    type GestureHoldEndEvent = WakeEvent;
    type TouchDownEvent = WakeEvent;
    type TouchUpEvent = WakeEvent;
    type TouchMotionEvent = WakeEvent;
    type TouchCancelEvent = WakeEvent;
    type TouchFrameEvent = WakeEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = WakeEvent;
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
            Feature::Classic => m.classic = false,
            Feature::Media => m.media = false,
            Feature::ColorManagement => m.color_management = false,
            Feature::Polkit => m.polkit = false,
            Feature::Theme => m.theme = false,
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

#[test]
fn classic_mod_tap_works_without_pointer_gestures_and_scroll_cancels_it() {
    let (mut h, _) = setup();
    h.gv.config.mod_key=crate::config::ModKey::Super;
    h.gv.config.modules.gestures=false;
    h.gv.features=crate::module::Features::new(&h.gv.config);
    h.gv.set_shell_mode(crate::classic::ShellMode::Classic);
    h.gv.classic.progress.jump(1.0);h.gv.settle_classic();
    h.gv.on_keyboard::<TestBackend>(Key(133,KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(133,KeyState::Released));
    assert!(h.gv.classic.start.open);
    h.gv.classic.start.close();
    h.gv.on_keyboard::<TestBackend>(Key(133,KeyState::Pressed));
    h.gv.on_pointer_axis::<TestBackend>(Scroll);
    h.gv.on_keyboard::<TestBackend>(Key(133,KeyState::Released));
    assert!(!h.gv.classic.start.open);
}

#[test]
fn gavno_mod_tap_opens_launcher_and_run_style_skips_application_scan() {
    let (mut h, _) = setup();h.gv.config.mod_key=crate::config::ModKey::Super;
    h.gv.on_keyboard::<TestBackend>(Key(133,KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(133,KeyState::Released));
    assert!(h.gv.launcher.open);assert!(!h.gv.overview.open());
    h.gv.close_launcher();h.gv.launcher=Default::default();
    h.gv.toggle_run_launcher();assert!(h.gv.launcher.open&&h.gv.launcher.run_style);
}

#[test]
fn polkit_dialog_captures_keys_and_clicks_until_it_closes() {
    let (mut h, _) = setup();
    let w = h.window();
    h.gv.map_new_window(w.clone());
    h.commit_size(&w, 400, 300);
    h.gv.focus_window(Some(&w));
    h.keyboard_keys();
    let outcome = crate::polkit::tests::push_dialog(&mut h.gv, "keys", &["admin"]);
    // 'a' goes to the password field, never to the focused client.
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Released));
    assert_eq!(h.keyboard_keys(), Vec::<u32>::new());
    assert_eq!(h.gv.polkit.queue[0].password.as_str(), "a");
    // Clicks outside the panel do nothing and reach no client.
    motion(&mut h.gv, None, (5.0, 5.0).into());
    button(&mut h.gv, BTN_LEFT, ButtonState::Pressed);
    button(&mut h.gv, BTN_LEFT, ButtonState::Released);
    assert_eq!(h.pointer_buttons(), Vec::<u32>::new());
    assert_eq!(h.gv.polkit.queue.len(), 1);
    // Escape cancels and the keyboard returns to the client.
    h.gv.on_keyboard::<TestBackend>(Key(9, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(9, KeyState::Released));
    assert!(matches!(outcome.try_recv(), Ok(crate::polkit::Outcome::Cancelled)));
    h.keyboard_keys();
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Released));
    assert_eq!(h.keyboard_keys(), vec![30, 30]);
}

#[test]
fn music_panel_captures_xkb_keys_and_outside_clicks_then_releases_input() {
    let (mut h, output) = setup();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    h.commit_size(&window, 400, 300);
    h.gv.focus_window(Some(&window));
    h.keyboard_keys();
    h.gv.now_playing.borrow_mut().music.bus_address = Some("unix:path=/nonexistent/gavno-music-test".into());
    h.gv.toggle_music(&output);
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Released));
    assert!(h.keyboard_keys().is_empty());
    assert_eq!(h.gv.now_playing.borrow().music.session.as_ref().unwrap().query, "a");
    // Use the real XKB layout, rather than injecting a Unicode character.
    let keyboard = h.gv.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(&mut h.gv, |mut ctx| ctx.cycle_next_layout());
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Released));
    assert_eq!(h.gv.now_playing.borrow().music.session.as_ref().unwrap().query, "aф");
    assert!(h.keyboard_keys().is_empty());
    // Shell shortcuts and Super taps must not escape modal capture.
    h.gv.on_keyboard::<TestBackend>(Key(133, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(133, KeyState::Released));
    assert!(!h.gv.launcher.open && !h.gv.overview.open());
    motion(&mut h.gv, None, (5.0, 5.0).into());
    button(&mut h.gv, BTN_LEFT, ButtonState::Pressed);
    button(&mut h.gv, BTN_LEFT, ButtonState::Released);
    assert!(h.pointer_buttons().is_empty());
    assert!(!h.gv.music_open());
    h.gv.toggle_music(&output);
    h.gv.on_keyboard::<TestBackend>(Key(9, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(9, KeyState::Released));
    assert!(!h.gv.music_open());
    assert!(h.keyboard_keys().is_empty());
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Pressed));
    h.gv.on_keyboard::<TestBackend>(Key(38, KeyState::Released));
    assert_eq!(h.keyboard_keys(), vec![30, 30]);
}

// One synthetic event implements the input kinds whose wake path precedes
// shell dispatch (touch and switch currently have no client forwarding path).
#[derive(Clone, Copy)]
struct WakeEvent;
use smithay::{backend::input as backend_input, wayland::seat::WaylandFocus};
impl Event<TestBackend> for WakeEvent {
    fn time(&self) -> u64 { 1000 }
    fn device(&self) -> TestDevice { TestDevice }
}
impl PointerMotionEvent<TestBackend> for WakeEvent {
    fn delta_x(&self) -> f64 { 2.0 }
    fn delta_y(&self) -> f64 { 3.0 }
    fn delta_x_unaccel(&self) -> f64 { 2.0 }
    fn delta_y_unaccel(&self) -> f64 { 3.0 }
}
impl AbsolutePositionEvent<TestBackend> for WakeEvent {
    fn x(&self) -> f64 { 0.5 }
    fn y(&self) -> f64 { 0.5 }
    fn x_transformed(&self, width: i32) -> f64 { f64::from(width) / 2.0 }
    fn y_transformed(&self, height: i32) -> f64 { f64::from(height) / 2.0 }
}
impl backend_input::GestureBeginEvent<TestBackend> for WakeEvent { fn fingers(&self) -> u32 { 3 } }
impl backend_input::GestureEndEvent<TestBackend> for WakeEvent { fn cancelled(&self) -> bool { false } }
impl backend_input::GestureSwipeUpdateEvent<TestBackend> for WakeEvent {
    fn delta_x(&self) -> f64 { 0.0 }
    fn delta_y(&self) -> f64 { 0.0 }
}
impl backend_input::GesturePinchUpdateEvent<TestBackend> for WakeEvent {
    fn delta_x(&self) -> f64 { 0.0 }
    fn delta_y(&self) -> f64 { 0.0 }
    fn scale(&self) -> f64 { 1.0 }
    fn rotation(&self) -> f64 { 0.0 }
}
impl backend_input::TouchEvent<TestBackend> for WakeEvent {
    fn slot(&self) -> backend_input::TouchSlot { Some(0).into() }
}
impl backend_input::SwitchToggleEvent<TestBackend> for WakeEvent {
    fn switch(&self) -> Option<backend_input::Switch> { Some(backend_input::Switch::Lid) }
    fn state(&self) -> backend_input::SwitchState { backend_input::SwitchState::Off }
}
macro_rules! wake_event_traits {
    ($($kind:ident),*) => { $(impl backend_input::$kind<TestBackend> for WakeEvent {})* };
}
wake_event_traits!(PointerMotionAbsoluteEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
    TouchDownEvent, TouchUpEvent, TouchMotionEvent, TouchCancelEvent, TouchFrameEvent);

#[test]
fn every_input_kind_wakes_and_records_idle_activity() {
    let events: Vec<InputEvent<TestBackend>> = vec![
        InputEvent::Keyboard { event: Key(38, KeyState::Pressed) },
        InputEvent::PointerButton { event: Button(BTN_LEFT, ButtonState::Pressed) },
        InputEvent::PointerMotion { event: WakeEvent },
        InputEvent::PointerMotionAbsolute { event: WakeEvent },
        InputEvent::PointerAxis { event: Scroll },
        InputEvent::GestureSwipeBegin { event: WakeEvent },
        InputEvent::GestureSwipeUpdate { event: WakeEvent },
        InputEvent::GestureSwipeEnd { event: WakeEvent },
        InputEvent::GesturePinchBegin { event: WakeEvent },
        InputEvent::GesturePinchUpdate { event: WakeEvent },
        InputEvent::GesturePinchEnd { event: WakeEvent },
        InputEvent::GestureHoldBegin { event: WakeEvent },
        InputEvent::GestureHoldEnd { event: WakeEvent },
        InputEvent::TouchDown { event: WakeEvent },
        InputEvent::TouchMotion { event: WakeEvent },
        InputEvent::TouchUp { event: WakeEvent },
        InputEvent::TouchCancel { event: WakeEvent },
        InputEvent::TouchFrame { event: WakeEvent },
        InputEvent::SwitchToggle { event: WakeEvent },
    ];
    for event in events {
        let (mut h, _) = setup();
        h.gv.power_off_monitors("test");
        h.gv.process_input_event(event);
        assert!(!h.gv.power.is_off());
        assert!(h.gv.protocols.idle_policy.take_activity());
    }
    let (mut h, _) = setup();
    h.gv.power_off_monitors("test");
    h.gv.process_input_event::<TestBackend>(InputEvent::DeviceAdded { device: TestDevice });
    h.gv.process_input_event::<TestBackend>(InputEvent::DeviceRemoved { device: TestDevice });
    assert!(h.gv.power.is_off());
}

#[test]
fn wake_key_and_button_pairs_are_consumed_before_clients_or_shell() {
    let (mut h, _) = setup();
    let window = h.window(); h.gv.map_new_window(window.clone());
    h.commit_size(&window, 800, 600);
    h.gv.focus_window(Some(&window));
    motion(&mut h.gv, Some((window.wl_surface().unwrap().into_owned(), (0.0, 0.0).into())), (500.0, 400.0).into());
    h.keyboard_keys(); h.pointer_buttons();
    h.gv.power_off_monitors("test");
    for state in [KeyState::Pressed, KeyState::Pressed, KeyState::Released] {
        h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(38, state) });
    }
    assert!(!h.gv.power.is_off());
    assert!(h.keyboard_keys().is_empty());
    for state in [KeyState::Pressed, KeyState::Released] {
        h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(38, state) });
    }
    assert_eq!(h.keyboard_keys(), [30, 30]);
    h.gv.power_off_monitors("test");
    for state in [ButtonState::Pressed, ButtonState::Pressed, ButtonState::Released] {
        h.gv.process_input_event::<TestBackend>(InputEvent::PointerButton { event: Button(BTN_LEFT, state) });
    }
    assert!(!h.gv.power.is_off());
    assert!(h.pointer_buttons().is_empty());
    assert!(!h.gv.seat.get_pointer().unwrap().is_grabbed());
    for state in [ButtonState::Pressed, ButtonState::Released] {
        h.gv.process_input_event::<TestBackend>(InputEvent::PointerButton { event: Button(BTN_LEFT, state) });
    }
    assert_eq!(h.pointer_buttons(), [BTN_LEFT, BTN_LEFT]);
    // Release of a press delivered before power-off still balances the client.
    h.gv.focus_window(Some(&window)); h.keyboard_keys();
    h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(38, KeyState::Pressed) });
    h.gv.power_off_monitors("test");
    h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(38, KeyState::Released) });
    assert_eq!(h.keyboard_keys(), [30, 30]);
    // A wake modifier must not trigger Super-tap or leave XKB modifiers stuck.
    h.gv.power_off_monitors("test");
    h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(133, KeyState::Pressed) });
    h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(133, KeyState::Released) });
    assert!(!h.gv.seat.get_keyboard().unwrap().modifier_state().logo);
    assert!(!h.gv.launcher.open);
}

#[test]
fn wake_pointer_motion_reaches_client_and_lock_keeps_its_gate() {
    let (mut h, _) = setup();
    let window = h.window(); h.gv.map_new_window(window.clone());
    h.commit_size(&window, 800, 600);
    h.gv.space.map_element(window.clone(), (0, 0), false);
    motion(&mut h.gv, Some((window.wl_surface().unwrap().into_owned(), (0.0, 0.0).into())), (500.0, 400.0).into());
    h.pointer_positions();
    h.gv.power_off_monitors("test");
    h.gv.process_input_event::<TestBackend>(InputEvent::PointerMotion { event: WakeEvent });
    assert!(!h.gv.power.is_off());
    assert!(!h.pointer_positions().is_empty());
    assert_eq!(h.gv.pointer_location(), (502.0, 403.0).into());
    h.gv.power_off_monitors("test");
    h.gv.lock_session();
    assert!(h.gv.power.is_off());
    h.gv.process_input_event::<TestBackend>(InputEvent::TouchDown { event: WakeEvent });
    assert!(!h.gv.power.is_off());
    assert!(h.gv.protocols.locks.locked);
    h.gv.power_off_monitors("test");
    h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(38, KeyState::Pressed) });
    h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard { event: Key(38, KeyState::Released) });
    assert!(h.keyboard_keys().is_empty());
}

#[test]
fn input_focus_change_revokes_consumed_unmapped_activation() {
    use smithay::wayland::xdg_activation::XdgActivationHandler;
    let (mut h, _) = setup();
    let current = h.window();
    h.gv.map_new_window(current.clone());
    h.gv.space.map_element(current.clone(), (100, 100), false);
    h.commit_size(&current, 400, 300);
    let target = h.window();
    h.gv.unmapped.push(target.clone());
    let (token, data) = {
        let (token, data) = h.gv.protocols.activation.create_external_token(None);
        (token.clone(), data.clone())
    };
    h.gv.request_activation(token, data, target.toplevel().unwrap().wl_surface().clone());
    let epoch = h.gv.launch_focus_epoch();
    // Click empty desktop through the production input dispatcher.
    motion(&mut h.gv, None, (1200.0, 700.0).into());
    h.gv.process_input_event::<TestBackend>(InputEvent::PointerButton {
        event: Button(BTN_LEFT, ButtonState::Pressed),
    });
    h.gv.process_input_event::<TestBackend>(InputEvent::PointerButton {
        event: Button(BTN_LEFT, ButtonState::Released),
    });
    assert_ne!(h.gv.launch_focus_epoch(), epoch);
    let focus_before = h.gv.focused_window();
    h.gv.map_new_window(target.clone());
    assert!(crate::handlers::xdg_activation::is_urgent(&target));
    assert_eq!(h.gv.focused_window(), focus_before);
}

fn dispatch_key(h: &mut crate::state::tests::Harness, code: u32, state: KeyState) {
    h.gv.process_input_event::<TestBackend>(InputEvent::Keyboard {
        event: Key(code, state),
    });
}
fn tap_key(h: &mut crate::state::tests::Harness, code: u32) {
    dispatch_key(h, code, KeyState::Pressed);
    dispatch_key(h, code, KeyState::Released);
}
fn delayed_launch_token(
    gv: &Gavno,
) -> (
    smithay::wayland::xdg_activation::XdgActivationToken,
    smithay::wayland::xdg_activation::XdgActivationTokenData,
) {
    let (token, data) = gv.protocols.activation.tokens().next().unwrap();
    let mut data = data.clone();
    data.timestamp -= std::time::Duration::from_secs(6);
    (token.clone(), data)
}

#[test]
fn client_typing_revokes_late_and_consumed_launch_authorizations() {
    use smithay::wayland::xdg_activation::XdgActivationHandler;
    for deferred in [false, true] {
        for typing in [false, true] {
            let (mut h, _) = setup();
            let browser = h.window();
            if deferred {
                crate::handlers::xdg_shell::prepare_initial_configure(&h.gv, &browser);
                browser.toplevel().unwrap().send_configure();
                h.gv.unmapped.push(browser.clone());
            } else {
                h.gv.map_new_window(browser.clone());
            }
            let terminal = h.window();
            h.gv.map_new_window(terminal.clone());
            h.commit_size(&terminal, 400, 300);
            h.keyboard_keys();
            h.gv.launch_shell("true", None);
            let (token, data) = delayed_launch_token(&h.gv);
            if deferred {
                h.gv.request_activation(
                    token.clone(),
                    data.clone(),
                    browser.toplevel().unwrap().wl_surface().clone(),
                );
            }
            let keyboard = h.gv.seat.get_keyboard().unwrap();
            let enter = keyboard.last_enter();
            let epoch = h.gv.launch_focus_epoch();
            if typing {
                tap_key(&mut h, 38); // A, delivered to the terminal.
                assert!(h.keyboard_keys().contains(&30));
                assert_ne!(h.gv.launch_focus_epoch(), epoch);
            }
            assert_eq!(keyboard.last_enter(), enter, "typing did not move focus");
            assert_eq!(h.gv.focused_window(), Some(terminal.clone()));
            if deferred {
                h.commit_size(&browser, 640, 400);
            } else {
                h.gv.request_activation(
                    token,
                    data,
                    browser.toplevel().unwrap().wl_surface().clone(),
                );
            }
            assert_eq!(crate::handlers::xdg_activation::is_urgent(&browser), typing);
            assert_eq!(
                h.gv.focused_window(),
                Some(if typing { terminal } else { browser })
            );
        }
    }
}

#[test]
fn releases_modifiers_and_shell_consumed_keys_do_not_revoke_launches() {
    for kind in ["release", "modifier", "shortcut", "launcher", "start"] {
        let (mut h, _) = setup();
        h.gv.config.mod_key = crate::config::ModKey::Alt;
        let terminal = h.window();
        h.gv.map_new_window(terminal.clone());
        h.commit_size(&terminal, 400, 300);
        if kind == "release" {
            dispatch_key(&mut h, 38, KeyState::Pressed);
        }
        h.keyboard_keys();
        h.gv.launch_shell("true", None);
        let (token, _) = delayed_launch_token(&h.gv);
        let epoch = h.gv.launch_focus_epoch();
        match kind {
            "release" => dispatch_key(&mut h, 38, KeyState::Released),
            "modifier" => tap_key(&mut h, 50), // Shift_L reaches the client.
            "shortcut" => {
                dispatch_key(&mut h, 64, KeyState::Pressed); // Alt+D opens the launcher.
                tap_key(&mut h, 40);
                dispatch_key(&mut h, 64, KeyState::Released);
                assert!(h.gv.launcher.open);
            }
            "launcher" => {
                h.gv.toggle_run_launcher();
                tap_key(&mut h, 38);
            }
            "start" => {
                h.gv.classic.mode = crate::classic::ShellMode::Classic;
                h.gv.classic.start.open = true;
                tap_key(&mut h, 38);
            }
            _ => unreachable!(),
        }
        assert_eq!(h.gv.launch_focus_epoch(), epoch, "{kind}");
        assert_eq!(h.gv.consume_launch_token(&token), Some(true), "{kind}");
        let delivered = h.keyboard_keys();
        match kind {
            "release" => assert!(delivered.contains(&30)),
            "modifier" => assert!(delivered.contains(&42)),
            _ => assert!(delivered.is_empty(), "{kind} was compositor-owned"),
        }
    }
}

#[test]
fn initiating_launcher_enter_preserves_its_new_authorization() {
    use smithay::wayland::xdg_activation::XdgActivationHandler;
    let (mut h, _) = setup();
    let browser = h.window();
    h.gv.map_new_window(browser.clone());
    let terminal = h.window();
    h.gv.map_new_window(terminal.clone());
    h.commit_size(&terminal, 400, 300);
    h.keyboard_keys();
    let epoch = h.gv.launch_focus_epoch();
    h.gv.toggle_run_launcher();
    for code in [28, 27, 30, 26] {
        tap_key(&mut h, code);
    } // "true"
    assert_eq!(h.gv.protocols.activation.tokens().count(), 0);
    tap_key(&mut h, 36); // Enter launches and closes the launcher.
    assert!(!h.gv.launcher.open);
    assert_eq!(h.gv.launch_focus_epoch(), epoch);
    assert!(h.keyboard_keys().is_empty());
    let (token, data) = delayed_launch_token(&h.gv);
    h.gv.request_activation(
        token,
        data,
        browser.toplevel().unwrap().wl_surface().clone(),
    );
    assert!(!crate::handlers::xdg_activation::is_urgent(&browser));
    assert_eq!(h.gv.focused_window(), Some(browser));
}

fn second_output(h: &mut crate::state::tests::Harness, name: &str, x: i32) -> Output {
    let output = Output::new(name.into(), PhysicalProperties {
        size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "test".into(), model: "test".into(),
    });
    output.change_current_state(Some(Mode { size: (1600, 900).into(), refresh: 60000 }), None, None, Some((x, 0).into()));
    h.gv.space.map_output(&output, (x, 0));
    h.gv.workspaces.ensure(&output);
    output
}

#[test]
fn code_mode_tiles_every_workspace_and_restores_each_mode() {
    let (mut h, output) = setup();
    let other = second_output(&mut h, "second", 1600);
    let first = h.window();
    h.gv.map_new_window(first.clone());
    let second = h.window();
    h.gv.map_new_window(second.clone());
    assert!(h.gv.move_to_workspace(&second, 1));
    let owner = h.gv.workspaces.ensure(&output);
    owner.list.items[0].tiling = true;
    owner.list.items[1].tiling = false;
    h.gv.workspaces.ensure(&other).list.items[0].tiling = true;

    h.gv.set_code_mode(true);
    for output in [&output, &other] {
        assert!(h.gv.workspaces.ensure(output).list.items.iter().all(|ws| ws.tiling), "{}", output.name());
    }
    // A workspace appended during code mode tiles until the mode is off.
    let third = h.window();
    h.gv.map_new_window(third.clone());
    assert!(h.gv.move_to_workspace(&third, 2));
    let new_id = h.gv.workspaces.ensure(&output).list.items.last().unwrap().id;
    h.gv.reflow_tiling(false);
    assert!(h.gv.workspaces.ensure(&output).list.items.iter().all(|ws| ws.tiling));

    h.gv.set_code_mode(false);
    let owner = h.gv.workspaces.ensure(&output);
    assert!(owner.list.items[0].tiling);
    assert!(!owner.list.items[1].tiling);
    let created = owner.list.items.iter().find(|ws| ws.id == new_id).unwrap();
    assert_eq!(created.tiling, owner.list.default_tiling, "workspaces created during code mode get the default");
    assert!(h.gv.workspaces.ensure(&other).list.items[0].tiling);
    assert!(h.gv.code_mode_saved.is_empty());
}

#[test]
fn code_mode_hides_the_pointer_until_pointer_activity() {
    use smithay::input::pointer::CursorImageStatus;
    let (mut h, _) = setup();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    h.commit_size(&window, 800, 600);
    h.gv.focus_window(Some(&window));
    h.gv.set_code_mode(true);
    h.gv.pointer_hidden = false;
    h.gv.cursor_status = CursorImageStatus::default_named();
    dispatch_key(&mut h, 38, KeyState::Pressed); // "a" reaches the client
    dispatch_key(&mut h, 38, KeyState::Released);
    assert!(h.gv.pointer_hidden);
    assert_eq!(h.gv.cursor_status, CursorImageStatus::Hidden);
    assert_eq!(h.gv.focused_window(), Some(window), "hiding must not move pointer or keyboard focus");
    h.gv.process_input_event::<TestBackend>(InputEvent::PointerMotion { event: WakeEvent });
    assert!(!h.gv.pointer_hidden);
    assert_ne!(h.gv.cursor_status, CursorImageStatus::Hidden);
}

#[test]
fn code_mode_vim_aliases_focus_the_expected_window() {
    let (mut h, _) = setup();
    h.gv.config.mod_key = crate::config::ModKey::Super;
    let make = |h: &mut crate::state::tests::Harness, loc: (i32, i32)| {
        let window = h.window();
        h.gv.map_new_window(window.clone());
        h.commit_size(&window, 150, 150);
        h.gv.space.map_element(window.clone(), loc, false);
        window
    };
    let center = make(&mut h, (700, 400));
    let left = make(&mut h, (100, 400));
    let right = make(&mut h, (1300, 400));
    let _up = make(&mut h, (700, 50));
    let down = make(&mut h, (700, 750));
    h.gv.workspaces.snapshot(&h.gv.space);
    h.gv.code_mode = true;
    h.gv.focus_window(Some(&center));
    let mods = smithay::input::keyboard::ModifiersState { logo: true, ..Default::default() };
    for (sym, expected) in [
        (keysyms::KEY_l, right),
        (keysyms::KEY_h, center.clone()),
        (keysyms::KEY_j, down),
        (keysyms::KEY_k, center),
        (keysyms::KEY_h, left),
    ] {
        let action = h.gv.bind_for(Keysym::from(sym), &mods).expect("alias bind");
        assert!(matches!(action, Action::FocusDirection(_)), "{action:?}");
        h.gv.run_action(action);
        assert_eq!(h.gv.focused_window(), Some(expected), "{sym}");
    }
}

#[test]
fn code_mode_replaces_mod_l_lock_and_restores_it() {
    let (mut h, _) = setup();
    h.gv.config.mod_key = crate::config::ModKey::Super;
    let mods = smithay::input::keyboard::ModifiersState { logo: true, ..Default::default() };
    h.gv.set_code_mode(true);
    assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_l), &mods), Some(Action::FocusDirection(crate::snap::Direction::Right)));
    h.gv.set_code_mode(false);
    assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_l), &mods), Some(Action::Lock));
    let ctrl_alt = smithay::input::keyboard::ModifiersState { ctrl: true, alt: true, ..Default::default() };
    h.gv.set_code_mode(true);
    assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_l), &ctrl_alt), Some(Action::Lock));
}

#[test]
fn bar_hint_context_follows_code_and_resize_modes_and_the_config_switch() {
    use crate::hints::HintContext;
    let (mut h, _) = setup();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    h.gv.focus_window(Some(&window));
    h.gv.set_code_mode(true);
    h.gv.refresh_shell();
    assert_eq!(h.gv.topbar.hint_context, Some(HintContext::Code));
    h.gv.enter_resize_mode();
    assert_eq!(h.gv.topbar.hint_context, Some(HintContext::Resize));
    h.gv.exit_resize_mode();
    assert_eq!(h.gv.topbar.hint_context, Some(HintContext::Code));
    h.gv.config.bar_hints = false;
    h.gv.refresh_shell();
    assert_eq!(h.gv.topbar.hint_context, None);
    h.gv.config.bar_hints = true;
    h.gv.refresh_shell();
    assert_eq!(h.gv.topbar.hint_context, Some(HintContext::Code));
    h.gv.set_code_mode(false);
    assert_eq!(h.gv.topbar.hint_context, None);
}

#[test]
fn typing_in_a_client_does_not_request_bar_redraws_in_code_mode() {
    let (mut h, _) = setup();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    h.commit_size(&window, 800, 600);
    h.gv.focus_window(Some(&window));
    h.gv.set_code_mode(true);
    h.gv.window_animations.entries.clear();
    h.gv.hide_pointer();
    h.gv.on_loop_idle();
    h.gv.window_animations.entries.clear();
    h.gv.redraw_requests = 0;
    for _ in 0..8 {
        dispatch_key(&mut h, 38, KeyState::Pressed);
        dispatch_key(&mut h, 38, KeyState::Released);
    }
    assert_eq!(h.gv.redraw_requests, 0, "client typing must not redraw the bar");
}


#[test]
fn window_hints_use_home_row_focus_and_cancel_without_client_keys() {
    let (mut h, output) = setup();
    let a = h.window();
    let s = h.window();
    let hidden = h.window();
    for w in [&a, &s, &hidden] { h.gv.map_new_window(w.clone()); h.commit_size(w, 300, 200); }
    h.gv.minimize_window(&hidden);
    let elsewhere = h.window();
    h.gv.map_new_window(elsewhere.clone());
    h.gv.move_to_workspace(&elsewhere, 1);
    h.gv.focus_window(Some(&s));
    h.gv.set_code_mode(true);
    h.gv.toggle_window_hints();
    let hints = h.gv.overlay.keyboard.windows.as_ref().unwrap();
    assert_eq!(hints.output, output);
    assert_eq!(hints.windows.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), ["a", "s"]);
    assert_eq!(hints.windows[0].1, a);
    assert_eq!(hints.windows[1].1, s);
    assert_eq!(h.gv.topbar.hint_context, Some(crate::hints::HintContext::Windows));
    h.keyboard_keys();
    tap_key(&mut h, 38); // XKB keycode for Latin A.
    assert_eq!(h.gv.focused_window(), Some(a.clone()));
    assert!(!h.gv.overlay.keyboard.open());
    assert!(h.keyboard_keys().is_empty());
    h.gv.toggle_window_hints();
    tap_key(&mut h, 9); // Escape.
    assert!(!h.gv.overlay.keyboard.open());
    assert_eq!(h.gv.focused_window(), Some(a));
    assert!(h.keyboard_keys().is_empty());
    h.gv.toggle_window_hints();
    button(&mut h.gv, BTN_LEFT, ButtonState::Pressed);
    button(&mut h.gv, BTN_LEFT, ButtonState::Released);
    assert!(!h.gv.overlay.keyboard.open());
    assert!(h.pointer_buttons().is_empty());
}

#[test]
fn overview_keyboard_selection_crosses_workspaces_and_enter_focuses() {
    let (mut h, output) = setup();
    let a = h.window();
    let b = h.window();
    for w in [&a, &b] { h.gv.map_new_window(w.clone()); h.commit_size(w, 400, 300); }
    h.gv.move_to_workspace(&b, 1);
    h.gv.focus_window(Some(&a));
    h.gv.set_code_mode(true);
    h.gv.toggle_overview();
    assert_eq!(h.gv.overview.selected, Some(a.clone()));
    assert_eq!(h.gv.topbar.hint_context, Some(crate::hints::HintContext::Overview));
    h.keyboard_keys();
    tap_key(&mut h, 114); // Right.
    assert_eq!(h.gv.overview.selected, Some(b.clone()));
    assert_eq!(h.gv.workspaces.get(&output).unwrap().list.active, 0);
    assert_eq!(h.gv.focused_window(), Some(a));
    tap_key(&mut h, 9); // Escape restores the original workspace view.
    assert_eq!(h.gv.workspaces.get(&output).unwrap().position.target, 0.0);
    h.gv.toggle_overview();
    tap_key(&mut h, 114);
    tap_key(&mut h, 36); // Enter.
    assert_eq!(h.gv.focused_window(), Some(b));
    assert_eq!(h.gv.workspaces.get(&output).unwrap().list.active, 1);
    assert!(!h.gv.overview.open());
    assert_eq!(h.gv.topbar.hint_context, Some(crate::hints::HintContext::Code));
    assert!(h.keyboard_keys().is_empty());
}

#[test]
fn cheat_sheet_binds_toggle_and_swallow_the_dismissal() {
    let (mut h, _) = setup();
    h.gv.config.mod_key = crate::config::ModKey::Super;
    let w = h.window();
    h.gv.map_new_window(w.clone());
    h.gv.focus_window(Some(&w));
    for sym in [keysyms::KEY_slash, keysyms::KEY_F1] {
        let mods = ModifiersState { logo: true, ..Default::default() };
        let action = h.gv.bind_for(Keysym::from(sym), &mods).unwrap();
        assert_eq!(action, Action::CheatSheet);
        h.gv.run_action(action.clone());
        assert!(h.gv.overlay.keyboard.open());
        h.gv.run_action(action);
        assert!(!h.gv.overlay.keyboard.open());
    }
    h.gv.toggle_cheat_sheet();
    h.keyboard_keys();
    tap_key(&mut h, 38);
    assert!(!h.gv.overlay.keyboard.open());
    assert!(h.keyboard_keys().is_empty());
    // The sheet remains available within resize mode and overview.
    h.gv.enter_resize_mode();
    dispatch_key(&mut h, 133, KeyState::Pressed); // Super.
    tap_key(&mut h, 67); // F1.
    dispatch_key(&mut h, 133, KeyState::Released);
    assert!(h.gv.overlay.keyboard.open());
    tap_key(&mut h, 9);
    assert!(h.gv.overlay.resize_mode.active());
    h.gv.exit_resize_mode();
    h.gv.toggle_overview();
    dispatch_key(&mut h, 133, KeyState::Pressed);
    tap_key(&mut h, 67);
    dispatch_key(&mut h, 133, KeyState::Released);
    assert!(h.gv.overlay.keyboard.open());
    tap_key(&mut h, 9);
    assert!(h.gv.overview.open());
}

#[test]
fn resize_mode_ctrl_arrows_move_floating_windows_and_ignore_tiles() {
    let (mut h, output) = setup();
    let w = h.window();
    h.gv.map_new_window(w.clone());
    h.commit_size(&w, 400, 300);
    h.gv.focus_window(Some(&w));
    h.gv.space.map_element(w.clone(), (400, 300), false);
    h.gv.save_workspace_location(&w, (400, 300).into());
    h.gv.enter_resize_mode();
    let before = h.gv.interactive_rect(&w).unwrap();
    dispatch_key(&mut h, 37, KeyState::Pressed); // Ctrl.
    tap_key(&mut h, 114); // Right.
    dispatch_key(&mut h, 37, KeyState::Released);
    let after = h.gv.interactive_rect(&w).unwrap();
    assert_eq!(after.loc.x, before.loc.x + 10);
    assert_eq!(after.loc.y, before.loc.y);
    assert_eq!(after.size, before.size);
    h.gv.resize_mode_key_with_ctrl(keysyms::KEY_Down, true, true);
    assert_eq!(h.gv.interactive_rect(&w).unwrap().loc.y, before.loc.y + 50);
    for _ in 0..100 { h.gv.resize_mode_key_with_ctrl(keysyms::KEY_Right, true, true); }
    let rect = h.gv.interactive_rect(&w).unwrap();
    let area = h.gv.work_area(&output);
    assert!(rect.loc.x + rect.size.w <= area.loc.x + area.size.w);
    h.gv.exit_resize_mode();
    h.gv.set_code_mode(true);
    assert!(h.gv.tiled(&w));
    h.gv.enter_resize_mode();
    let before = h.gv.interactive_rect(&w).unwrap();
    h.gv.resize_mode_key_with_ctrl(keysyms::KEY_l, false, true);
    assert_eq!(h.gv.interactive_rect(&w).unwrap(), before);
    h.gv.exit_resize_mode();
    h.gv.toggle_floating(&w);
    h.gv.space.map_element(w.clone(), (400, 300), false);
    h.gv.enter_resize_mode();
    let before = h.gv.interactive_rect(&w).unwrap();
    h.gv.resize_mode_key_with_ctrl(keysyms::KEY_l, false, true);
    assert_eq!(h.gv.interactive_rect(&w).unwrap().loc.x, before.loc.x + 10);
}

#[test]
fn latin_hint_letters_and_resize_aliases_work_in_russian_layout() {
    let (mut h, _) = setup();
    let w = h.window();
    h.gv.map_new_window(w.clone());
    h.commit_size(&w, 400, 300);
    h.gv.focus_window(Some(&w));
    let keyboard = h.gv.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(&mut h.gv, |mut ctx| ctx.set_layout(smithay::input::keyboard::Layout(1)));
    h.gv.toggle_window_hints();
    tap_key(&mut h, 38); // Physical A, Cyrillic ф in the active layout.
    assert!(!h.gv.overlay.keyboard.open());
    assert_eq!(h.gv.focused_window(), Some(w.clone()));
    h.gv.set_code_mode(true);
    h.gv.toggle_floating(&w);
    h.gv.enter_resize_mode();
    let before = h.gv.interactive_rect(&w).unwrap();
    dispatch_key(&mut h, 37, KeyState::Pressed);
    tap_key(&mut h, 46); // Physical L, Cyrillic д.
    dispatch_key(&mut h, 37, KeyState::Released);
    assert_eq!(h.gv.interactive_rect(&w).unwrap().loc.x, before.loc.x + 10);
}

#[test]
fn hidden_pointer_keeps_client_cursor_updates_and_survives_resize_cursor_refresh() {
    use smithay::input::pointer::{CursorIcon, CursorImageStatus};
    let (mut h, _) = setup();
    let w = h.window();
    h.gv.map_new_window(w.clone());
    h.gv.focus_window(Some(&w));
    h.gv.set_code_mode(true);
    h.gv.hide_pointer();
    let seat = h.gv.seat.clone();
    smithay::input::SeatHandler::cursor_image(&mut h.gv, &seat, CursorImageStatus::Named(CursorIcon::Text));
    h.gv.enter_resize_mode();
    h.gv.refresh_splitter_cursor();
    h.gv.refresh_resize_cursor();
    assert!(h.gv.cursor.hidden);
    assert_eq!(h.gv.cursor_status, CursorImageStatus::Hidden);
    h.gv.show_pointer();
    assert_eq!(h.gv.cursor_status, CursorImageStatus::Named(CursorIcon::Text));
    assert!(!h.gv.cursor.hidden);
    h.gv.hide_pointer();
    h.gv.on_pointer_axis::<TestBackend>(Scroll);
    assert!(!h.gv.pointer_hidden);
    h.gv.hide_pointer();
    button(&mut h.gv, BTN_LEFT, ButtonState::Pressed);
    button(&mut h.gv, BTN_LEFT, ButtonState::Released);
    assert!(!h.gv.pointer_hidden);
}

#[test]
fn modal_focus_changes_clear_held_modifiers_in_the_new_wayland_client() {
    for overview in [false, true] {
        for modifier in [50, 37] { // Shift and Ctrl, in XKB keycodes.
            let (mut h, _) = setup();
            let first = h.window();
            h.gv.map_new_window(first.clone());
            h.commit_size(&first, 300, 200);
            let _previous_client = h.switch_to_new_client();
            let second = h.window();
            h.gv.map_new_window(second.clone());
            h.commit_size(&second, 300, 200);
            h.gv.focus_window(Some(&first));
            h.keyboard_modifiers();
            h.keyboard_keys();
            if overview { h.gv.toggle_overview(); }
            else { h.gv.toggle_window_hints(); }
            dispatch_key(&mut h, modifier, KeyState::Pressed);
            if overview {
                // The second window is left of the focused first window in the scene.
                tap_key(&mut h, 113);
                assert_eq!(h.gv.overview.selected, Some(second.clone()));
                tap_key(&mut h, 36);
            } else {
                assert_eq!(h.gv.overlay.keyboard.windows.as_ref().unwrap().windows[0].1, second);
                tap_key(&mut h, 38); // A selects the second client.
            }
            assert_eq!(h.gv.focused_window(), Some(second));
            assert_ne!(h.keyboard_modifiers().last().unwrap().0, 0, "focus entry must expose the held modifier");
            dispatch_key(&mut h, modifier, KeyState::Released);
            assert_eq!(h.keyboard_modifiers().last(), Some(&(0, 0, 0, 0)), "overview={overview}, modifier={modifier}");
            assert!(h.keyboard_keys().is_empty(), "modal keys and unmatched modifier releases must stay suppressed");
        }
    }
}

fn open_keyboard_test_modal(gv: &mut Gavno, kind: &str) {
    match kind {
        "overview" => gv.toggle_overview(),
        "hints" => gv.toggle_window_hints(),
        "sheet" => gv.toggle_cheat_sheet(),
        _ => unreachable!(),
    }
}

#[test]
fn modal_releases_cancel_volume_brightness_and_launcher_repeats() {
    for kind in ["overview", "hints", "sheet"] {
        for code in [123, 233] { // Volume up and brightness up.
            let (mut h, _) = setup();
            dispatch_key(&mut h, code, KeyState::Pressed);
            assert_eq!(h.gv.media.repeat.as_ref().map(|(key, _, _)| *key), Some(code));
            open_keyboard_test_modal(&mut h.gv, kind);
            dispatch_key(&mut h, code, KeyState::Released);
            assert!(h.gv.media.repeat.is_none(), "{kind}: repeat survived release of {code}");
            assert!(!h.gv.media.core_keys.contains(&code));
            assert!(!h.gv.suppressed_keys.contains(&code));
        }
        let (mut h, _) = setup();
        h.gv.toggle_run_launcher();
        dispatch_key(&mut h, 38, KeyState::Pressed);
        assert_eq!(h.gv.launcher.repeating_key(), Some(38));
        open_keyboard_test_modal(&mut h.gv, kind);
        dispatch_key(&mut h, 38, KeyState::Released);
        assert_eq!(h.gv.launcher.repeating_key(), None, "{kind}: launcher repeat survived release");
    }
}

#[test]
fn modal_opening_mod_release_preserves_the_next_standalone_mod_tap() {
    for (kind, code) in [("overview", 38), ("hints", 42), ("sheet", 67)] {
        let (mut h, _) = setup();
        dispatch_key(&mut h, 64, KeyState::Pressed); // Alt is Mod in the harness.
        tap_key(&mut h, code);
        assert!(h.gv.overview.open() || h.gv.overlay.keyboard.open(), "{kind}");
        dispatch_key(&mut h, 64, KeyState::Released);
        tap_key(&mut h, 9); // Close the modal.
        assert!(!h.gv.launcher.open);
        tap_key(&mut h, 64);
        assert!(h.gv.launcher.open, "{kind}: standalone Mod tap was lost");
    }
}

#[test]
fn overview_respects_the_focused_clients_active_shortcut_inhibitor() {
    let (mut h, _) = setup();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    h.commit_size(&window, 400, 300);
    h.gv.focus_window(Some(&window));
    let _inhibitor = h.inhibit_shortcuts(&window);
    assert!(h.gv.seat.keyboard_shortcuts_inhibited());
    // Gestures can open overview without moving keyboard focus or suspending
    // the client's inhibitor; exercise the resulting production input path.
    h.gv.toggle_overview();
    let selected = h.gv.overview.selected.clone();
    h.keyboard_keys();
    for code in [38, 114, 9] { tap_key(&mut h, code); }
    assert_eq!(h.keyboard_keys(), [30, 30, 106, 106, 1, 1]);
    assert!(h.gv.seat.keyboard_shortcuts_inhibited());
    assert_eq!(h.gv.focused_window(), Some(window));
    assert_eq!(h.gv.overview.selected, selected);
    assert!(h.gv.overview.open(), "inhibited Escape must reach the client");
}

#[test]
fn code_mode_reconnect_does_not_restore_a_removed_workspace_mode() {
    for refresh_while_disconnected in [false, true] {
        let (mut h, left) = setup();
        let right = second_output(&mut h, "external", 1600);
        h.gv.workspaces.ensure(&left).list.items[0].tiling = true;
        h.gv.workspaces.ensure(&right).list.items[0].tiling = true;
        let old_id = h.gv.workspaces.get(&right).unwrap().list.items[0].id;
        h.gv.set_code_mode(true);
        h.gv.space.unmap_output(&right);
        if refresh_while_disconnected {
            h.gv.refresh_workspaces();
            assert!(!h.gv.code_mode_saved.iter().any(|(output, _, _)| *output == right));
        }
        let reconnected = second_output(&mut h, "external", 1600);
        assert_ne!(reconnected, right);
        assert_eq!(h.gv.workspaces.get(&reconnected).unwrap().list.items[0].id, old_id);
        h.gv.reflow_tiling(false);
        assert!(h.gv.workspaces.get(&reconnected).unwrap().list.items[0].tiling);
        h.gv.set_code_mode(false);
        let owner = h.gv.workspaces.get(&reconnected).unwrap();
        assert!(!owner.list.default_tiling);
        assert!(!owner.list.items[0].tiling, "a new workspace must use the normal default");
        assert!(h.gv.workspaces.get(&left).unwrap().list.items[0].tiling, "surviving workspace mode must still restore");
        h.gv.config.mod_key = crate::config::ModKey::Super;
        h.gv.sync_bar_hints();
        assert_eq!(h.gv.topbar.mod_label, "Super");
    }
}

#[test]
fn removing_the_overlay_output_closes_hints_and_sheet_before_migration() {
    for kind in ["hints", "sheet"] {
        for remove_all in [false, true] {
            let (mut h, left) = setup();
            let local = h.window();
            h.gv.map_new_window(local.clone());
            h.commit_size(&local, 300, 200);
            let right = second_output(&mut h, "external", 1600);
            let remote = h.window();
            h.gv.map_new_window(remote.clone());
            h.commit_size(&remote, 300, 200);
            h.gv.space.map_element(remote.clone(), (1800, 200), false);
            h.gv.refresh_workspaces();
            assert_eq!(h.gv.workspaces.membership(&remote), Some((right.clone(), 0)));
            motion(&mut h.gv, None, (1800.0, 400.0).into());
            open_keyboard_test_modal(&mut h.gv, kind);
            assert_eq!(h.gv.overlay.keyboard.output(), Some(&right));
            h.gv.space.unmap_output(&right);
            if remove_all { h.gv.space.unmap_output(&left); }
            h.gv.refresh_workspaces();
            assert!(!h.gv.overlay.keyboard.open(), "{kind}, remove_all={remove_all}");
            assert!(h.gv.overlay.keyboard.output().is_none());
            if !remove_all {
                assert_eq!(h.gv.workspaces.membership(&remote), Some((left, 0)));
                h.gv.focus_window(Some(&local));
                h.keyboard_keys();
                tap_key(&mut h, 38);
                assert_eq!(h.gv.focused_window(), Some(local));
                assert_eq!(h.keyboard_keys(), [30, 30], "an invisible modal must not capture the letter");
            }
        }
    }
}

#[test]
fn settled_overview_ignored_keys_and_missing_candidates_request_no_redraws() {
    let (mut h, output) = setup();
    let window = h.window();
    h.gv.map_new_window(window.clone());
    h.commit_size(&window, 400, 300);
    h.gv.focus_window(Some(&window));
    h.gv.toggle_overview();
    h.gv.overview.progress.jump(1.0);
    h.gv.workspaces.ensure(&output).position.jump(0.0);
    h.gv.window_animations.entries.clear();
    h.gv.redraw_requests = 0;
    for code in [65, 113, 114, 111, 116, 23, 10, 18] {
        // Space, four arrows without candidates, Tab with one window, current
        // workspace 1 and nonexistent workspace 9.
        tap_key(&mut h, code);
    }
    assert_eq!(h.gv.redraw_requests, 0);
    assert_eq!(h.gv.overview.selected, Some(window));
    assert!(h.gv.overview.open());
    let second = h.window();
    h.gv.map_new_window(second.clone());
    h.commit_size(&second, 400, 300);
    h.gv.redraw_requests = 0;
    tap_key(&mut h, 23);
    assert!(h.gv.redraw_requests > 0, "a changed selection must redraw");
    assert_eq!(h.gv.overview.selected, Some(second));
}

#[test]
fn disabled_overview_is_hidden_from_the_cheat_sheet_and_the_bar_hints() {
    use crate::config::Config;
    use crate::config_reload::{apply, Prepared};
    use crate::hints::HintContext;
    let (mut h, _) = setup();
    h.gv.set_code_mode(true);
    h.gv.refresh_shell();
    let has_overview = |gv: &crate::Gavno| {
        gv.topbar.hint_items.iter().any(|hint| hint.label == "overview")
            || crate::hints::cheat_rows(gv).iter().any(|(_, label)| label.to_lowercase().contains("overview"))
    };
    assert!(has_overview(&h.gv));
    let next = Config::build(true, Some("[modules]\noverview=false"), |_| None).0;
    let prepared = Prepared::new(next, &h.gv.config).unwrap();
    apply(&mut h.gv, prepared).unwrap();
    assert_eq!(h.gv.topbar.hint_context, Some(HintContext::Code));
    assert!(!has_overview(&h.gv));
    assert!(h.gv.visible_hints(HintContext::Overview).is_empty());
    // Hints stay bound to Mod+G; Mod+F does nothing.
    assert!(h.gv.topbar.hint_items.iter().any(|hint| hint.label == "hints" && hint.keys == "G"));
    let mods = crate::hints::modifiers(h.gv.config.mod_key, crate::hints::MAIN_FOR_TESTS);
    assert_eq!(h.gv.bind_for(smithay::input::keyboard::Keysym::from(smithay::input::keyboard::keysyms::KEY_f), &mods), None);
    assert_eq!(h.gv.bind_for(smithay::input::keyboard::Keysym::from(smithay::input::keyboard::keysyms::KEY_g), &mods), Some(crate::input::Action::WindowHints));
}
