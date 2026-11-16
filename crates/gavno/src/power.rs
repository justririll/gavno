//! Output power policy. Idle uses one one-shot timer; sleeping clients are
//! paced only when a visible surface actually has a pending frame callback.
use crate::{backend::Backend, Gavno};
use smithay::{
    desktop::{layer_map_for_output, utils::with_surfaces_surface_tree},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    wayland::compositor::{SurfaceAttributes, SurfaceData},
};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Power {
    off: bool,
    /// Only outputs whose DRM power-off commit succeeded can confirm a lock.
    pub(crate) secured_outputs: Vec<smithay::output::Output>,
    idle_timer: Option<RegistrationToken>,
    deadline: Option<Instant>,
    frame_timer: Option<RegistrationToken>,
    last_frames: Option<Instant>,
    keys: Vec<u32>,
    buttons: Vec<u32>,
    pub rendered_frames: u64,
    pub frame_timer_firings: u64,
}
impl Power {
    pub fn is_off(&self) -> bool {
        self.off
    }

    pub(crate) fn consume_key(&mut self, code: u32, pressed: bool, waking: bool) -> bool {
        consume_pair(&mut self.keys, code, pressed, waking)
    }
    pub(crate) fn consume_button(&mut self, code: u32, pressed: bool, waking: bool) -> bool {
        consume_pair(&mut self.buttons, code, pressed, waking)
    }
}

fn consume_pair(captured: &mut Vec<u32>, code: u32, pressed: bool, waking: bool) -> bool {
    if let Some(index) = captured.iter().position(|key| *key == code) {
        if !pressed {
            captured.swap_remove(index);
        }
        return true;
    }
    if waking && pressed {
        captured.push(code);
        return true;
    }
    false
}

impl Gavno {
    fn cancel_power_idle(&mut self) {
        if let Some(token) = self.power.idle_timer.take() {
            self.loop_handle.remove(token);
        }
        self.power.deadline = None;
    }

    /// Activity and inhibitors are shared with the lock policy, including its
    /// event-loop batching. Locking itself never wakes a sleeping output.
    pub(crate) fn refresh_power_idle(&mut self, activity: bool, inhibited: bool) {
        if self.power.off || self.config.display.off_after_idle_secs == 0 || inhibited {
            self.cancel_power_idle();
            return;
        }
        if activity {
            self.cancel_power_idle();
        }
        if let Some(deadline) = self.power.deadline {
            if Instant::now() >= deadline {
                self.power_off_monitors("idle timeout");
            }
            return;
        }
        let deadline = Instant::now()
            + Duration::from_secs(u64::from(self.config.display.off_after_idle_secs));
        self.arm_power_idle(deadline);
    }

    fn arm_power_idle(&mut self, deadline: Instant) {
        self.power.deadline = Some(deadline);
        self.power.idle_timer = self
            .loop_handle
            .insert_source(Timer::from_deadline(deadline), |_, _, gv| {
                let _stall = crate::perf::Stall::new("timer.power_idle");
                gv.power.idle_timer = None;
                // Evaluate input, commits and inhibitor changes from this batch first.
                gv.loop_handle.insert_idle(|gv| gv.refresh_idle());
                TimeoutAction::Drop
            })
            .ok();
    }

    pub fn power_off_monitors(&mut self, reason: &str) {
        if self.power.off {
            return;
        }
        self.power.off = true;
        self.power.last_frames = Some(Instant::now());
        self.power_lock_clock(false);
        self.refresh_greeter_clock();
        self.reconcile_classic_widgets();
        self.cancel_power_idle();
        crate::backend::udev::set_power(self, false);
        tracing::info!(reason, "outputs powered off");
        self.schedule_power_frames();
    }

    pub fn power_on_monitors(&mut self, reason: &str) {
        if !self.power.off {
            return;
        }
        self.power.off = false;
        self.power.secured_outputs.clear();
        if let Some(token) = self.power.frame_timer.take() {
            self.loop_handle.remove(token);
        }
        match &mut self.backend {
            Backend::Winit(data) => data.reset_damage(),
            Backend::Udev(_) => crate::backend::udev::set_power(self, true),
            Backend::None => {}
        }
        self.power_lock_clock(true);
        self.refresh_greeter_clock();
        tracing::info!(reason, "outputs powered on");
        let inhibited = self.protocols.idle.is_inhibited();
        self.refresh_power_idle(true, inhibited);
        self.queue_redraw_all();
    }

    pub(crate) fn schedule_power_frames(&mut self) {
        if !self.power.off || self.power.frame_timer.is_some() {
            return;
        }
        let mut pending = false;
        visible_surfaces(self, |states| {
            pending |= !states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .is_empty();
        });
        if !pending {
            return;
        }
        // Also space the first sleeping callback from the last possible on-screen
        // callback. Commits after this timer fires can arm exactly one next timer.
        let deadline =
            *self.power.last_frames.get_or_insert_with(Instant::now) + Duration::from_secs(1);
        self.power.frame_timer = self
            .loop_handle
            .insert_source(Timer::from_deadline(deadline), |_, _, gv| {
                let _stall = crate::perf::Stall::new("timer.power_frames");
                gv.power.frame_timer = None;
                gv.power.frame_timer_firings += 1;
                if gv.power.off {
                    gv.power.last_frames = Some(Instant::now());
                    let time: Duration = gv.clock.now().into();
                    visible_surfaces(gv, |states| {
                        for callback in states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .frame_callbacks
                            .drain(..)
                        {
                            callback.done(time.as_millis() as u32);
                        }
                    });
                }
                TimeoutAction::Drop
            })
            .ok();
    }
}

/// Visit mapped windows (including popups/subsurfaces), layers, or lock surfaces.
/// No rendering or presentation feedback is fabricated while the CRTC is off.
fn visible_surfaces(gv: &Gavno, mut visit: impl FnMut(&SurfaceData)) {
    for output in gv.space.outputs() {
        if gv.protocols.locks.locked {
            if let Some(surface) = gv.protocols.locks.surface(output) {
                with_surfaces_surface_tree(surface, |_, states| visit(states));
            }
            continue;
        }
        for window in gv
            .space
            .elements()
            .filter(|w| gv.space.outputs_for_element(w).contains(output))
        {
            window.with_surfaces(|_, states| visit(states));
        }
        for layer in layer_map_for_output(output).layers() {
            layer.with_surfaces(|_, states| visit(states));
        }
    }
}

#[cfg(test)]
mod tests;
