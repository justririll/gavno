use crate::Gavno;
use smithay::{
    input::pointer::PointerHandle,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
    wayland::pointer_constraints::{
        with_pointer_constraint, PointerConstraint, PointerConstraintsHandler,
    },
};
impl PointerConstraintsHandler for Gavno {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if pointer.current_focus().as_ref() != Some(surface) {
            return;
        }
        if self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .as_ref()
            != Some(surface)
        {
            return;
        }
        let Some((under, origin)) = self.surface_under(pointer.current_location()) else {
            return;
        };
        if &under != surface {
            return;
        }
        let point = (pointer.current_location() - origin).to_i32_floor();
        with_pointer_constraint(surface, pointer, |constraint| {
            if let Some(c) = constraint {
                if !c.is_active() && c.region().is_none_or(|r| r.contains(point)) {
                    c.activate();
                }
            }
        });
    }
    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        hint: Point<f64, Logical>,
    ) {
        let active = with_pointer_constraint(surface, pointer, |c| {
            c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
        });
        if !active || !hint.x.is_finite() || !hint.y.is_finite() {
            return;
        }
        // A hint changes the eventual unlock position, never emits absolute motion while locked.
        // Smithay stores the committed hint; warping during a lock would violate relative input.
    }
}
impl Gavno {
    pub fn refresh_constraints(&mut self, focus: Option<&WlSurface>) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(surface) = pointer.current_focus() else {
            return;
        };
        let under = self.surface_under(pointer.current_location());
        with_pointer_constraint(&surface, &pointer, |c| {
            let Some(c) = c else { return };
            // A lock's region controls activation, not motion after activation.
            // A confinement whose committed region excludes the pointer must
            // explicitly send unconfined before allowing it to move freely.
            let outside_confinement = matches!(&*c, PointerConstraint::Confined(_))
                && !under.as_ref().is_some_and(|(s, origin)| {
                    s == &surface
                        && c.region().is_none_or(|r| {
                            r.contains((pointer.current_location() - *origin).to_i32_floor())
                        })
                });
            if c.is_active() && (focus != Some(&surface) || outside_confinement) {
                c.deactivate();
            }
        });
    }

    /// Shared by relative and absolute motion; rejected destinations emit no motion.
    pub fn pointer_motion_allowed(&mut self, destination: Point<f64, Logical>) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return true;
        };
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        self.refresh_constraints(focus.as_ref());
        let Some(surface) = pointer.current_focus() else {
            return true;
        };
        self.new_constraint(&surface, &pointer);
        let under = self.surface_under(pointer.current_location());
        let destination_under = self.surface_under(destination);
        with_pointer_constraint(&surface, &pointer, |c| {
            let Some(c) = c.filter(|c| c.is_active()) else {
                return true;
            };
            match &*c {
                PointerConstraint::Locked(_) => false,
                PointerConstraint::Confined(_) => {
                    destination_under
                        .as_ref()
                        .is_some_and(|(s, _)| s == &surface)
                        && under.as_ref().is_some_and(|(_, origin)| {
                            c.region().is_none_or(|r| {
                                r.contains((destination - *origin).to_i32_floor())
                            })
                        })
                }
            }
        })
    }
}
smithay::delegate_pointer_constraints!(Gavno);
