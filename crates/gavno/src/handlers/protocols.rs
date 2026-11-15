//! Protocol globals and policy, kept together to keep the main state small.
use crate::Gavno;
use smithay::{
    reexports::{calloop::LoopHandle, wayland_server::DisplayHandle},
    wayland::{
        cursor_shape::CursorShapeManagerState,
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        pointer_constraints::PointerConstraintsState,
        relative_pointer::RelativePointerManagerState,
        selection::{primary_selection::PrimarySelectionState, wlr_data_control::DataControlState},
        session_lock::SessionLockManagerState,
        shell::xdg::decoration::XdgDecorationState,
        xdg_activation::XdgActivationState,
    },
};

pub struct Protocols {
    pub _cursor_shape: CursorShapeManagerState,
    pub activation: XdgActivationState,
    pub activation_serials: std::collections::VecDeque<(
        smithay::utils::Serial,
        smithay::reexports::wayland_server::backend::ClientId,
        std::time::Instant,
    )>,
    pub _decoration: XdgDecorationState,
    pub data_control: DataControlState,
    pub _relative_pointer: RelativePointerManagerState,
    pub _constraints: PointerConstraintsState,
    pub idle: IdleNotifierState<Gavno>,
    pub _idle_inhibit: IdleInhibitManagerState,
    pub idle_policy: super::idle_notify::IdlePolicy,
    pub lock_manager: SessionLockManagerState,
    pub locks: super::session_lock::Locks,
    pub shortcuts: KeyboardShortcutsInhibitState,
    pub shortcut_inhibitors:
        Vec<smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitor>,
    pub screencopy: super::screencopy::Screencopy,
}
impl Protocols {
    pub fn new(
        dh: &DisplayHandle,
        handle: LoopHandle<'static, Gavno>,
        primary: &PrimarySelectionState,
    ) -> Self {
        Self {
            _cursor_shape: CursorShapeManagerState::new::<Gavno>(dh),
            activation: XdgActivationState::new::<Gavno>(dh),
            activation_serials: Default::default(),
            _decoration: XdgDecorationState::new::<Gavno>(dh),
            data_control: DataControlState::new::<Gavno, _>(dh, Some(primary), |_| true),
            _relative_pointer: RelativePointerManagerState::new::<Gavno>(dh),
            _constraints: PointerConstraintsState::new::<Gavno>(dh),
            idle: IdleNotifierState::new(dh, handle),
            _idle_inhibit: IdleInhibitManagerState::new::<Gavno>(dh),
            idle_policy: Default::default(),
            lock_manager: SessionLockManagerState::new::<Gavno, _>(dh, |_| true),
            locks: Default::default(),
            shortcuts: KeyboardShortcutsInhibitState::new::<Gavno>(dh),
            shortcut_inhibitors: Vec::new(),
            screencopy: Default::default(),
        }
    }
}
