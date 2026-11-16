//! Smithay owns one-shot notification timers; no periodic idle source is installed.
use crate::Gavno;
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState},
};
use smithay::{reexports::wayland_server::Resource, wayland::seat::WaylandFocus};

/// Coalesce an event-loop batch of input and inhibitor changes into one activity notification.
#[derive(Default)]
pub struct IdlePolicy {
    pub inhibitors: Vec<WlSurface>,
    activity: bool,
}
impl IdlePolicy {
    pub fn activity(&mut self) {
        self.activity = true;
    }
    pub fn take_activity(&mut self) -> bool {
        std::mem::take(&mut self.activity)
    }
}
impl IdleNotifierHandler for Gavno {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.protocols.idle
    }
}
impl Gavno {
    pub fn refresh_idle(&mut self) {
        self.protocols
            .idle_policy
            .inhibitors
            .retain(|s| s.is_alive());
        let inhibited = self.protocols.idle_policy.inhibitors.iter().any(|surface| {
            let mut root = surface.clone();
            while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
                root = parent;
            }
            let s = &root;
            if !smithay::backend::renderer::utils::with_renderer_surface_state(s, |state| {
                state.buffer().is_some()
            })
            .unwrap_or(false)
            {
                return false;
            }
            if self.protocols.locks.locked {
                return self
                    .protocols
                    .locks
                    .surfaces
                    .iter()
                    .any(|(_, l)| l.wl_surface() == s);
            }
            // Unmapped surfaces must not keep the desktop awake.
            self.space.elements().any(|w| {
                w.wl_surface().as_deref() == Some(s)
                    && !self.space.outputs_for_element(w).is_empty()
            }) || self.space.outputs().any(|o| {
                smithay::desktop::layer_map_for_output(o)
                    .layer_for_surface(s, smithay::desktop::WindowSurfaceType::ALL)
                    .is_some()
            })
        });
        self.protocols.idle.set_is_inhibited(inhibited);
        let activity = self.protocols.idle_policy.take_activity();
        self.refresh_lock_idle(activity, inhibited);
        self.refresh_power_idle(activity, inhibited);
        if activity {
            self.protocols.idle.notify_activity(&self.seat);
        }
    }
}
smithay::delegate_idle_notify!(Gavno);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_batch_resets_timers_once() {
        let mut policy = IdlePolicy::default();
        assert!(!policy.take_activity());
        policy.activity();
        policy.activity();
        assert!(policy.take_activity());
        assert!(!policy.take_activity());
    }
    #[test]
    fn idle_has_no_periodic_reset() {
        let mut policy = IdlePolicy::default();
        policy.activity();
        assert!(policy.take_activity());
        for _ in 0..100 {
            assert!(!policy.take_activity());
        }
        policy.activity();
        assert!(policy.take_activity());
    }
}

#[cfg(test)]
#[path = "idle_notify_tests.rs"]
mod protocol_tests;
