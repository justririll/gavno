//! In-process modules.
//!
//! GAVNO is a small core plus modules. A module reacts to core events and may
//! claim key binds. Snap, tiling, overview, dock and animations are meant to
//! become modules behind this trait; heavy or risky features (web views,
//! games, AI) run out of process as extensions instead, so a crash there never
//! takes the session down.
//!
//! The core takes the module list out of `Gavno` while dispatching, so a module
//! gets full `&mut Gavno` access without aliasing itself.

use smithay::{desktop::Window, input::keyboard::ModifiersState, output::Output};

use crate::Gavno;

/// What a module did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// Pass the event on to the next module and then to the core.
    No,
    /// Stop here; the event is consumed.
    Yes,
}

#[allow(unused_variables)]
pub trait Module {
    /// Stable name, used in config (`modules { snap enabled=true }`) and logs.
    fn name(&self) -> &'static str;

    fn output_added(&mut self, gv: &mut Gavno, output: &Output) {}
    fn window_mapped(&mut self, gv: &mut Gavno, window: &Window) {}
    fn window_unmapped(&mut self, gv: &mut Gavno, window: &Window) {}

    /// A key press with the keysym from the first (latin) layout. Return
    /// `Handled::Yes` to consume it; the release is then swallowed too.
    fn key_pressed(&mut self, gv: &mut Gavno, keysym: u32, mods: &ModifiersState) -> Handled {
        Handled::No
    }
}

impl Gavno {
    /// Runs `f` for every module in order until one handles the event.
    pub fn dispatch_modules(&mut self, mut f: impl FnMut(&mut dyn Module, &mut Gavno) -> Handled) -> Handled {
        let mut modules = std::mem::take(&mut self.modules);
        let mut result = Handled::No;
        for module in modules.iter_mut() {
            if f(module.as_mut(), self) == Handled::Yes {
                result = Handled::Yes;
                break;
            }
        }
        // A module may have registered new modules meanwhile; keep both.
        modules.append(&mut self.modules);
        self.modules = modules;
        result
    }
}
