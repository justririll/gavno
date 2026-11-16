//! Synthetic Smithay events use the production input dispatch and XKB state.
use smithay::{backend::input::*, input::keyboard::Keycode};

pub struct Backend;
#[derive(PartialEq, Eq, Hash)]
pub struct SyntheticDevice;
impl Device for SyntheticDevice {
    fn id(&self) -> String { "gavno-debug-input".into() }
    fn name(&self) -> String { self.id() }
    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(capability, DeviceCapability::Keyboard | DeviceCapability::Pointer | DeviceCapability::Gesture)
    }
    fn usb_id(&self) -> Option<(u32, u32)> { None }
    fn syspath(&self) -> Option<std::path::PathBuf> { None }
}

pub struct Key { pub time: u64, pub code: u32, pub state: KeyState }
pub struct Button { pub time: u64, pub code: u32, pub state: ButtonState }
pub struct Position { pub time: u64, pub x: f64, pub y: f64 }
pub struct Scroll { pub time: u64, pub dx: f64, pub dy: f64, pub source: AxisSource }
#[derive(Clone, Copy)]
pub struct Gesture {
    pub time: u64,
    pub fingers: u32,
    pub dx: f64,
    pub dy: f64,
    pub scale: f64,
    pub rotation: f64,
    pub cancelled: bool,
}
macro_rules! event {
    ($($name:ty),*) => { $(impl Event<Backend> for $name {
        fn time(&self) -> u64 { self.time }
        fn device(&self) -> SyntheticDevice { SyntheticDevice }
    })* };
}
event!(Key, Button, Position, Scroll, Gesture);
impl KeyboardKeyEvent<Backend> for Key {
    // Smithay expects XKB codes, eight above Linux evdev codes.
    fn key_code(&self) -> Keycode { (self.code + 8).into() }
    fn state(&self) -> KeyState { self.state }
    fn count(&self) -> u32 { u32::from(self.state == KeyState::Pressed) }
}
impl PointerButtonEvent<Backend> for Button {
    fn button_code(&self) -> u32 { self.code }
    fn state(&self) -> ButtonState { self.state }
}
impl AbsolutePositionEvent<Backend> for Position {
    fn x(&self) -> f64 { self.x }
    fn y(&self) -> f64 { self.y }
    // Already logical coordinates relative to the first output's origin.
    fn x_transformed(&self, _: i32) -> f64 { self.x }
    fn y_transformed(&self, _: i32) -> f64 { self.y }
}
impl PointerMotionAbsoluteEvent<Backend> for Position {}
impl PointerAxisEvent<Backend> for Scroll {
    fn amount(&self, axis: Axis) -> Option<f64> {
        Some(if axis == Axis::Horizontal { self.dx } else { self.dy })
    }
    fn amount_v120(&self, _: Axis) -> Option<f64> { None }
    fn source(&self) -> AxisSource { self.source }
    fn relative_direction(&self, _: Axis) -> AxisRelativeDirection { AxisRelativeDirection::Identical }
}
impl GestureBeginEvent<Backend> for Gesture { fn fingers(&self) -> u32 { self.fingers } }
impl GestureEndEvent<Backend> for Gesture { fn cancelled(&self) -> bool { self.cancelled } }
impl GestureSwipeBeginEvent<Backend> for Gesture {}
impl GestureSwipeEndEvent<Backend> for Gesture {}
impl GesturePinchBeginEvent<Backend> for Gesture {}
impl GesturePinchEndEvent<Backend> for Gesture {}
impl GestureHoldBeginEvent<Backend> for Gesture {}
impl GestureHoldEndEvent<Backend> for Gesture {}
impl GestureSwipeUpdateEvent<Backend> for Gesture {
    fn delta_x(&self) -> f64 { self.dx }
    fn delta_y(&self) -> f64 { self.dy }
}
impl GesturePinchUpdateEvent<Backend> for Gesture {
    fn delta_x(&self) -> f64 { self.dx }
    fn delta_y(&self) -> f64 { self.dy }
    fn scale(&self) -> f64 { self.scale }
    fn rotation(&self) -> f64 { self.rotation }
}
impl InputBackend for Backend {
    type Device = SyntheticDevice;
    type KeyboardKeyEvent = Key;
    type PointerButtonEvent = Button;
    type PointerMotionAbsoluteEvent = Position;
    type PointerAxisEvent = Scroll;
    type GestureSwipeBeginEvent = Gesture;
    type GestureSwipeUpdateEvent = Gesture;
    type GestureSwipeEndEvent = Gesture;
    type GesturePinchBeginEvent = Gesture;
    type GesturePinchUpdateEvent = Gesture;
    type GesturePinchEndEvent = Gesture;
    type GestureHoldBeginEvent = Gesture;
    type GestureHoldEndEvent = Gesture;
    type PointerMotionEvent = UnusedEvent;
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
