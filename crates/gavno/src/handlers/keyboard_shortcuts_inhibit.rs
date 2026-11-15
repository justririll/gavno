use crate::Gavno;
use smithay::{
    reexports::wayland_server::{protocol::wl_surface::WlSurface, Resource},
    wayland::keyboard_shortcuts_inhibit::{
        KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
    },
};
impl KeyboardShortcutsInhibitHandler for Gavno {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.protocols.shortcuts
    }
    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        if self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .as_ref()
            == Some(inhibitor.wl_surface())
        {
            inhibitor.activate();
        }
        self.protocols.shortcut_inhibitors.push(inhibitor);
    }
    fn inhibitor_destroyed(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        self.protocols
            .shortcut_inhibitors
            .retain(|i| i != &inhibitor);
    }
}
impl Gavno {
    pub fn update_shortcut_inhibitors(&mut self, focused: Option<&WlSurface>) {
        for inhibitor in &self.protocols.shortcut_inhibitors {
            let active =
                inhibitor.wl_surface().is_alive() && focused == Some(inhibitor.wl_surface());
            if active && !inhibitor.is_active() {
                inhibitor.activate();
            }
            if !active && inhibitor.is_active() {
                inhibitor.inactivate();
            }
        }
    }
}
smithay::delegate_keyboard_shortcuts_inhibit!(Gavno);
