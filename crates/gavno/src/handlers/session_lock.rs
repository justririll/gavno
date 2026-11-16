//! Fail-closed session locking. Client death never clears `locked`.
use crate::Gavno;
use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    desktop::{utils::under_from_surface_tree, WindowSurfaceType},
    input::pointer::MotionEvent,
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::{
            ext_session_lock_manager_v1::ExtSessionLockManagerV1,
            ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
            ext_session_lock_v1::{self, ExtSessionLockV1},
        },
        wayland_server::{
            self,
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
            Client, DataInit, Dispatch, DisplayHandle, Resource,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::session_lock::{
        ExtLockSurfaceUserData, LockSurface, SessionLockHandler, SessionLockManagerGlobalData,
        SessionLockManagerState, SessionLockState, SessionLocker,
    },
};

#[derive(Default)]
pub struct Locks {
    pub locked: bool,
    owner: Option<ExtSessionLockV1>,
    confirmation: Option<SessionLocker>,
    confirmed: bool,
    pending: Vec<Output>,
    submitted: Vec<Output>,
    secured: Vec<Output>,
    pub surfaces: Vec<(Output, LockSurface)>,
}
impl Locks {
    pub(crate) fn internal(outputs: impl Iterator<Item = Output>) -> Self {
        Self { locked: true, pending: outputs.collect(), ..Default::default() }
    }
    pub(crate) fn secured(&self) -> bool { self.locked && self.pending.is_empty() && !self.secured.is_empty() }
    pub(crate) fn require_secure_frame(&mut self, outputs: impl Iterator<Item = Output>) {
        self.pending = outputs.collect();
        self.submitted.clear();
        self.secured.clear();
    }

    pub fn surface(&self, output: &Output) -> Option<&WlSurface> {
        self.surfaces
            .iter()
            .find(|(o, s)| o == output && s.alive() && mapped(s.wl_surface()))
            .map(|(_, s)| s.wl_surface())
    }
    pub fn submitted(&mut self, output: &Output) {
        if self.locked && self.pending.contains(output) && !self.submitted.contains(output) {
            self.submitted.push(output.clone());
        }
    }
    pub fn presented(&mut self, output: &Output) {
        if self.submitted.contains(output) {
            self.submitted.retain(|o| o != output);
            self.pending.retain(|o| o != output);
            self.secured.push(output.clone());
            self.confirm();
        }
    }
    pub fn outputs_added(&mut self, outputs: impl Iterator<Item = Output>) {
        if !self.locked {
            return;
        }
        for output in outputs {
            if !self.pending.contains(&output) && !self.secured.contains(&output) {
                self.pending.push(output);
            }
        }
    }
    fn confirm(&mut self) {
        if self.pending.is_empty() {
            if let Some(confirmation) = self.confirmation.take() {
                confirmation.lock();
                self.confirmed = true;
            }
        }
    }
}
fn mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false)
}
impl SessionLockHandler for Gavno {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.protocols.lock_manager
    }
    fn lock(&mut self, confirmation: SessionLocker) {
        if self.protocols.locks.locked || self.greeter.is_some() {
            return;
        }
        let owner = confirmation.ext_session_lock().clone();
        self.protocols.locks = Locks {
            locked: true,
            owner: Some(owner),
            confirmation: Some(confirmation),
            pending: self.space.outputs().cloned().collect(),
            ..Default::default()
        };
        self.secure_lock_input();
        self.protocols.locks.confirm();
        self.queue_redraw_all();
    }
    fn unlock(&mut self) {
        if self.lock.internal() { return; }
        self.protocols.locks = Locks::default();
        self.focus_topmost();
        self.queue_redraw_all();
    }
    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            return;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        surface.with_pending_state(|s| {
            s.size = Some((geo.size.w.max(1) as u32, geo.size.h.max(1) as u32).into())
        });
        surface.send_configure();
        self.protocols.locks.surfaces.push((output, surface));
        self.queue_redraw_all();
    }
}
impl Gavno {
    pub(crate) fn secure_lock_input(&mut self) {
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.unset_grab(self);
            keyboard.set_focus(self, None, serial);
            self.update_shortcut_inhibitors(None);
            self.refresh_constraints(None);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.unset_grab(self, serial, 0);
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: pointer.current_location(),
                    serial,
                    time: 0,
                },
            );
            // Buttons held when locking would stay pressed in Smithay (their
            // releases are not delivered while locked); release them now,
            // with no client focused.
            self.release_held_buttons(&pointer);
            pointer.frame(self);
        }
        self.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
    }
    pub fn lock_surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.outputs().find_map(|output| {
            let geo = self.space.output_geometry(output)?;
            if !geo.to_f64().contains(pos) {
                return None;
            }
            let surface = self.protocols.locks.surface(output)?;
            under_from_surface_tree(
                surface,
                pos - geo.loc.to_f64(),
                (0, 0),
                WindowSurfaceType::ALL,
            )
            .map(|(s, loc)| (s, (geo.loc + loc).to_f64()))
        })
    }
    pub fn refresh_lock(&mut self) {
        if !self.protocols.locks.locked {
            return;
        }
        self.protocols
            .locks
            .pending
            .retain(|o| self.space.outputs().any(|live| live == o));
        self.protocols.locks.confirm();
        let old_surface_count = self.protocols.locks.surfaces.len();
        self.protocols
            .locks
            .surfaces
            .retain(|(o, s)| s.alive() && self.space.outputs().any(|live| live == o));
        if self.protocols.locks.surfaces.len() != old_surface_count {
            self.queue_redraw_all();
        }
        for (output, surface) in &self.protocols.locks.surfaces {
            if let Some(geo) = self.space.output_geometry(output) {
                let size = (geo.size.w.max(1) as u32, geo.size.h.max(1) as u32).into();
                surface.with_pending_state(|s| s.size = Some(size));
                surface.send_configure();
            }
        }
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        if !focus.as_ref().is_some_and(|s| {
            s.is_alive()
                && self
                    .protocols
                    .locks
                    .surfaces
                    .iter()
                    .any(|(_, l)| l.wl_surface() == s && mapped(s))
        }) {
            let surface = self
                .protocols
                .locks
                .surfaces
                .iter()
                .find(|(_, s)| mapped(s.wl_surface()))
                .map(|(_, s)| s.wl_surface().clone());
            if focus != surface {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
                }
            }
        }
    }
}

// Smithay 0.7 calls `unlock()` even after posting InvalidUnlock. Validate the
// lock object here before delegating, so a rejected second locker cannot unlock.
impl Dispatch<ExtSessionLockV1, SessionLockState> for Gavno {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ExtSessionLockV1,
        request: ext_session_lock_v1::Request,
        data: &SessionLockState,
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if state.protocols.locks.owner.as_ref() != Some(resource) {
            if !matches!(request, ext_session_lock_v1::Request::Destroy) {
                resource.post_error(
                    ext_session_lock_v1::Error::InvalidUnlock,
                    "Lock object was not accepted",
                );
            }
            return;
        }
        if matches!(request, ext_session_lock_v1::Request::UnlockAndDestroy)
            && !state.protocols.locks.confirmed
        {
            resource.post_error(
                ext_session_lock_v1::Error::InvalidUnlock,
                "Lock has not been confirmed",
            );
            return;
        }
        <SessionLockManagerState as Dispatch<ExtSessionLockV1, SessionLockState, Self>>::request(
            state, client, resource, request, data, dh, init,
        );
    }
}
wayland_server::delegate_global_dispatch!(Gavno: [ExtSessionLockManagerV1: SessionLockManagerGlobalData] => SessionLockManagerState);
wayland_server::delegate_dispatch!(Gavno: [ExtSessionLockManagerV1: ()] => SessionLockManagerState);
wayland_server::delegate_dispatch!(Gavno: [ExtSessionLockSurfaceV1: ExtLockSurfaceUserData] => SessionLockManagerState);

#[cfg(test)]
mod tests {
    use super::*;
    fn output(name: &str) -> Output {
        Output::new(
            name.to_owned(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
            },
        )
    }
    #[test]
    fn confirmation_waits_for_secure_frames_on_every_output() {
        let a = output("a");
        let b = output("b");
        let mut locks = Locks {
            locked: true,
            pending: vec![a.clone(), b.clone()],
            ..Default::default()
        };
        // A previously queued desktop frame cannot count towards locking.
        locks.presented(&a);
        assert_eq!(locks.pending.len(), 2);
        locks.submitted(&a);
        locks.presented(&a);
        assert_eq!(locks.pending, vec![b.clone()]);
        locks.submitted(&b);
        locks.presented(&b);
        assert!(locks.pending.is_empty());
        // Losing a confirmation or a client never unlocks the session.
        assert!(locks.locked);
    }
    #[test]
    fn hotplug_adds_new_outputs_without_readding_secured_ones() {
        let a = output("a");
        let b = output("b");
        let c = output("c");
        let mut locks = Locks {
            locked: true,
            pending: vec![a.clone(), b.clone()],
            ..Default::default()
        };
        locks.submitted(&a);
        locks.presented(&a);
        locks.outputs_added([a, b.clone(), c.clone()].into_iter());
        assert_eq!(locks.pending, vec![b, c]);
    }
}
