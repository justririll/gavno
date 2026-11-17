//! Keyboard-only shell mode: all workspaces tile, the pointer hides until the
//! next pointer event, and the dock stays out of the way. Classic never enters
//! code mode, so its behaviour is unchanged.

use smithay::input::pointer::CursorImageStatus;

use crate::{hints::HintContext, Gavno};

impl Gavno {
    /// Turns code mode on or off. Enabling is a no-op in Classic, which keeps
    /// its own behaviour.
    pub fn set_code_mode(&mut self, enable: bool) {
        if enable && self.classic_mode() {
            return;
        }
        if self.code_mode == enable {
            self.sync_bar_hints();
            return;
        }
        self.code_mode = enable;
        if enable {
            self.code_mode_saved.clear();
            for owner in &mut self.workspaces.outputs {
                let output = owner.output.clone();
                for workspace in &mut owner.list.items {
                    self.code_mode_saved
                        .push((output.clone(), workspace.id, workspace.tiling));
                    workspace.tiling = true;
                }
            }
        } else {
            let saved = std::mem::take(&mut self.code_mode_saved);
            for owner in &mut self.workspaces.outputs {
                let output = owner.output.clone();
                let fallback = owner.list.default_tiling;
                for workspace in &mut owner.list.items {
                    workspace.tiling = saved
                        .iter()
                        .find(|(saved_output, id, _)| *saved_output == output && *id == workspace.id)
                        .map(|(_, _, tiling)| *tiling)
                        .unwrap_or(fallback);
                }
            }
            self.show_pointer();
        }
        self.dock.close_menu();
        self.dock.code_mode = enable;
        self.sync_bar_hints();
        self.reflow_tiling(true);
        self.refit_fixed_windows();
        self.queue_redraw_all();
    }

    /// Keeps workspaces created during code mode tiled. Called from the tiling
    /// reflow, so no extra idle callback or timer is needed.
    pub(crate) fn enforce_code_mode_tiling(&mut self) -> bool {
        if !self.code_mode {
            return false;
        }
        let mut changed = false;
        for owner in &mut self.workspaces.outputs {
            for workspace in &mut owner.list.items {
                if !workspace.tiling {
                    workspace.tiling = true;
                    changed = true;
                }
            }
        }
        changed
    }

    /// The context the bar strip shows, or None when the strip is off.
    pub(crate) fn bar_hint_context(&self) -> Option<HintContext> {
        if !self.code_mode || !self.config.bar_hints {
            return None;
        }
        Some(if self.overlay.keyboard.windows.is_some() {
            HintContext::Windows
        } else if self.overview.open() {
            HintContext::Overview
        } else if self.overlay.resize_mode.active() {
            HintContext::Resize
        } else {
            HintContext::Code
        })
    }

    pub(crate) fn sync_bar_hints(&mut self) {
        // Gesture-driven shell transitions also leave code mode.
        if self.code_mode && self.classic_mode() {
            self.set_code_mode(false);
        }
        self.dock.code_mode = self.code_mode;
        self.topbar.code_mode = self.code_mode;
        self.topbar.hint_context = self.bar_hint_context();
        self.topbar.mod_label = match self.config.mod_key {
            crate::config::ModKey::Super => "Super",
            crate::config::ModKey::Alt => "Alt",
        };
    }

    /// Hides the pointer without changing pointer focus. Nothing is drawn on
    /// the software cursor or the hardware cursor plane until the next pointer
    /// activity.
    pub fn hide_pointer(&mut self) {
        if self.pointer_hidden {
            return;
        }
        self.pointer_hidden = true;
        self.cursor.hidden = true;
        self.pointer_image = std::mem::replace(&mut self.cursor_status, CursorImageStatus::Hidden);
        self.queue_redraw_all();
    }

    /// Shows the pointer again after motion, a button, a scroll or a gesture.
    pub fn show_pointer(&mut self) {
        if !self.pointer_hidden {
            return;
        }
        self.pointer_hidden = false;
        self.cursor.hidden = false;
        self.cursor_status = self.pointer_image.clone();
        self.queue_redraw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_config_initializes_chip_strip_and_dock_before_the_first_frame() {
        let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let display = smithay::reexports::wayland_server::Display::new().unwrap();
        let mut config = crate::config::Config::build(true, None, |_| None).0;
        config.code_mode = true;
        let gv = Gavno::new(&mut event_loop, display, config);
        assert!(gv.code_mode && gv.dock.code_mode && gv.topbar.code_mode);
        assert_eq!(gv.topbar.hint_context, Some(HintContext::Code));
    }
}
