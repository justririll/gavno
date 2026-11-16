//! Optional feature boundaries. Registry state never schedules frames on its own.
use std::{cell::{Cell, RefCell}, panic::{catch_unwind, AssertUnwindSafe}};

use crate::{config::Config, Gavno};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum Feature {
    Dock, Bar, Overview, Tiling, Snap, Switcher, Gestures,
    WindowGestures, ResizeMode, Animations, Decorations, Ipc, Screencopy, Launcher, Notifications, Media,
}
impl Feature {
    pub const ALL: [Self; 16] = [Self::Dock, Self::Bar, Self::Overview, Self::Tiling,
        Self::Snap, Self::Switcher, Self::Gestures, Self::WindowGestures,
        Self::ResizeMode, Self::Animations, Self::Decorations, Self::Ipc, Self::Screencopy, Self::Launcher, Self::Notifications, Self::Media];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureState {
    Enabled,
    Disabled,
    Crashed { message: String },
}

pub struct Features {
    states: [RefCell<FeatureState>; 16],
    pending: [Cell<bool>; 16],
    resetting: bool,
    #[cfg(test)]
    injected: [Cell<bool>; 16],
    #[cfg(test)]
    pub reset_count: [usize; 16],
}
impl Features {
    pub fn new(config: &Config) -> Self {
        let m = &config.modules;
        let enabled = [m.dock && config.dock.enabled, m.bar && config.bar.enabled,
            m.overview, m.tiling, m.snap, m.switcher, m.gestures, m.window_gestures,
            m.resize_mode, m.animations, m.decorations, m.ipc, m.screencopy, m.launcher, m.notifications, m.media];
        Self {
            states: enabled.map(|on| RefCell::new(if on { FeatureState::Enabled } else { FeatureState::Disabled })),
            pending: std::array::from_fn(|_| Cell::new(false)), resetting: false,
            #[cfg(test)] injected: std::array::from_fn(|_| Cell::new(false)),
            #[cfg(test)] reset_count: [0; 16],
        }
    }
    pub fn on(&self, id: Feature) -> bool { matches!(*self.states[id as usize].borrow(), FeatureState::Enabled) }
    #[cfg(test)]
    pub fn state(&self, id: Feature) -> FeatureState { self.states[id as usize].borrow().clone() }
    #[cfg(test)]
    pub fn inject_panic(&mut self, id: Feature) { self.injected[id as usize].set(true); }
    fn before_call(&self, _id: Feature) {
        #[cfg(test)]
        if self.injected[_id as usize].replace(false) { panic!("injected {_id:?} panic"); }
    }
    fn crashed(&self, id: Feature, payload: Box<dyn std::any::Any + Send>) {
        let message = panic_message(payload.as_ref());
        if self.on(id) {
            tracing::error!(feature = ?id, %message, "feature crashed; disabled until restart");
            *self.states[id as usize].borrow_mut() = FeatureState::Crashed { message };
        }
        self.pending[id as usize].set(true);
    }
    /// For disjoint field borrows, especially the render path. Resets run at idle.
    pub fn call<R>(&mut self, id: Feature, default: R, f: impl FnOnce() -> R) -> R {
        self.read(id, default, f)
    }
    /// Immutable hit testing also catches panics; its resets are drained at idle.
    pub fn read<R>(&self, id: Feature, default: R, f: impl FnOnce() -> R) -> R {
        if !self.on(id) { return default; }
        let _scope = crate::crash_log::GuardScope::enter();
        match catch_unwind(AssertUnwindSafe(|| { self.before_call(id); f() })) {
            Ok(value) => value,
            Err(payload) => { self.crashed(id, payload); default }
        }
    }
}

pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload.downcast_ref::<String>().cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".into())
}

impl Gavno {
    pub fn guard<R>(&mut self, id: Feature, default: R, f: impl FnOnce(&mut Self) -> R) -> R {
        if !self.features.on(id) { return default; }
        let scope = crate::crash_log::GuardScope::enter();
        let result = catch_unwind(AssertUnwindSafe(|| { self.features.before_call(id); f(self) }));
        drop(scope);
        let value = match result {
            Ok(value) => value,
            Err(payload) => { self.features.crashed(id, payload); default }
        };
        // An inner guard can fail while its caller continues. Reset again at the
        // outer boundary to discard any writes made by that continuation.
        if matches!(*self.features.states[id as usize].borrow(), FeatureState::Crashed { .. }) {
            self.features.pending[id as usize].set(true);
        }
        self.run_feature_resets();
        value
    }

    pub fn run_feature_resets(&mut self) {
        if self.features.resetting { return; }
        self.features.resetting = true;
        let mut redraw = false;
        for id in Feature::ALL {
            if !self.features.pending[id as usize].replace(false) { continue; }
            redraw = true;
            #[cfg(test)] { self.features.reset_count[id as usize] += 1; }
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.reset_feature(id))) {
                tracing::error!(feature = ?id, message = %panic_message(payload.as_ref()), "feature reset panicked");
                // Keep recovery independent of the failed feature's entry points.
                if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.drop_feature_state(id))) {
                    tracing::error!(feature = ?id, message = %panic_message(payload.as_ref()), "feature state cleanup panicked");
                }
            }
        }
        self.features.resetting = false;
        if redraw { self.queue_redraw_all(); }
    }

    fn reset_feature(&mut self, id: Feature) {
        match id {
            Feature::Tiling => self.reset_tiling(),
            Feature::Dock => self.reset_dock(),
            Feature::Bar => self.reset_bar(),
            Feature::Overview => {
                self.overview = Default::default();
                for owner in &mut self.workspaces.outputs {
                    owner.held = false;
                    owner.position.jump(owner.list.active as f64);
                }
            }
            Feature::Gestures | Feature::WindowGestures => self.reset_gestures(),
            Feature::ResizeMode => self.reset_resize_mode(),
            Feature::Switcher => self.overlay.reset_switcher(),
            Feature::Snap => self.overlay.clear_preview(),
            Feature::Animations => self.reset_animations(),
            Feature::Decorations => self.window_decor = Default::default(),
            Feature::Ipc => { self.ipc.close(&self.loop_handle); }
            Feature::Screencopy => self.protocols.screencopy.fail_pending(),
            Feature::Launcher => self.launcher = Default::default(),
            Feature::Notifications => self.reset_notifications(),
            Feature::Media => self.reset_media(),
        }
    }
    fn drop_feature_state(&mut self, id: Feature) {
        match id {
            Feature::Dock => self.dock = Default::default(),
            Feature::Bar => { self.topbar = Default::default(); self.topbar.notifications = self.notifications.clone(); self.topbar.now_playing = self.now_playing.clone(); },
            Feature::Overview => self.overview = Default::default(),
            Feature::Gestures => self.gestures = Default::default(),
            Feature::WindowGestures => self.window_gestures = Default::default(),
            Feature::Switcher => self.overlay.reset_switcher(),
            Feature::ResizeMode => self.overlay.resize_mode = Default::default(),
            Feature::Animations => self.window_animations = Default::default(),
            Feature::Decorations => self.window_decor = Default::default(),
            Feature::Ipc => self.ipc.close(&self.loop_handle),
            Feature::Screencopy => self.protocols.screencopy.fail_pending(),
            Feature::Launcher => self.launcher = Default::default(),
            Feature::Notifications => self.reset_notifications(),
            Feature::Media => self.reset_media(),
            Feature::Snap | Feature::Tiling => (),
        }
    }
}
