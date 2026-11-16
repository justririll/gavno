//! Classic shortcuts take precedence; unmatched chords retain GAVNO behaviour.
use crate::{
    input::Action,
    module::Feature,
    snap::Direction,
    window_mode::{mode_state, WindowMode},
    Gavno,
};
use smithay::input::keyboard::{keysyms, Keysym, ModifiersState};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassicAction {
    ToggleMode,
    Start,
    ShowDesktop,
    MinimizeAll,
    Settings,
    TaskManager,
    Quick,
    Notifications,
    Up,
    Down,
    Item(usize),
    TaskView,
    Close,
    Run,
}
pub fn binding(
    sym: Keysym,
    mods: &ModifiersState,
    mod_key: crate::config::ModKey,
    classic: bool,
) -> Option<Action> {
    use ClassicAction::*;
    let main = mod_key.is_held(mods);
    let key = sym.raw();
    let extra = match mod_key {
        crate::config::ModKey::Super => mods.alt,
        crate::config::ModKey::Alt => mods.logo,
    };
    if main && mods.shift && !mods.ctrl && !extra && matches!(key, keysyms::KEY_w | keysyms::KEY_W)
    {
        return Some(Action::Classic(ToggleMode));
    }
    if !classic {
        return None;
    }
    if mods.ctrl && mods.shift && !mods.alt && !mods.logo && key == keysyms::KEY_Escape {
        return Some(Action::Classic(TaskManager));
    }
    if mods.alt && !mods.logo && !mods.ctrl && !mods.shift && key == keysyms::KEY_F4 {
        return Some(Action::Classic(Close));
    }
    if !main || extra || mods.ctrl || mods.shift {
        return None;
    }
    let action = match key {
        keysyms::KEY_e | keysyms::KEY_E => return None, // The normal bind already uses the configured file manager.
        keysyms::KEY_d | keysyms::KEY_D => ShowDesktop,
        keysyms::KEY_m | keysyms::KEY_M => MinimizeAll,
        keysyms::KEY_l | keysyms::KEY_L => return Some(Action::Lock),
        keysyms::KEY_i | keysyms::KEY_I => Settings,
        keysyms::KEY_r | keysyms::KEY_R => Run,
        keysyms::KEY_a | keysyms::KEY_A => Quick,
        keysyms::KEY_n | keysyms::KEY_N => Notifications,
        keysyms::KEY_Up => Up,
        keysyms::KEY_Down => Down,
        keysyms::KEY_Left => return Some(Action::Snap(Direction::Left)),
        keysyms::KEY_Right => return Some(Action::Snap(Direction::Right)),
        keysyms::KEY_Tab => TaskView,
        keysyms::KEY_1..=keysyms::KEY_9 => Item((key - keysyms::KEY_1) as usize),
        _ => return None,
    };
    Some(Action::Classic(action))
}
impl Gavno {
    pub fn run_classic_action(&mut self, action: ClassicAction) {
        self.guard(Feature::Classic, (), |gv| {
            use ClassicAction::*;
            match action {
                ToggleMode => gv.set_shell_mode(gv.classic.mode.other()),
                Start => gv.toggle_start(),
                ShowDesktop => gv.show_classic_desktop(true),
                MinimizeAll => gv.show_classic_desktop(false),
                Settings => crate::state::spawn_shell(&gv.config.classic.settings_command),
                TaskManager => crate::state::spawn_shell(&gv.config.classic.task_manager),
                Quick | Notifications => {
                    if let Some(o) = gv.active_output() {
                        gv.classic.start.close();
                        gv.bar_action(
                            &o,
                            if action == Quick {
                                crate::topbar::BarAction::PowerMenu
                            } else {
                                crate::topbar::BarAction::Calendar
                            },
                        );
                    }
                }
                Up => {
                    if let Some(w) = gv.focused_window() {
                        if mode_state(&w).mode != WindowMode::Maximized {
                            gv.set_window_mode(&w, WindowMode::Maximized, None);
                        }
                    }
                }
                Down => {
                    if let Some(w) = gv.focused_window() {
                        if mode_state(&w).mode != WindowMode::Floating {
                            gv.set_window_mode(&w, WindowMode::Floating, None);
                        } else {
                            gv.minimize_window(&w);
                        }
                    }
                }
                Item(i) => gv.classic_activate(i, false),
                TaskView => {
                    if gv.features.on(Feature::Overview) {
                        gv.toggle_overview();
                    } else {
                        gv.classic.switcher_win = true;
                        gv.switcher_cycle(false);
                    }
                }
                Close => {
                    if gv.focused_window().is_some() {
                        gv.close_focused();
                    } else {
                        if !gv.classic.start.open {
                            gv.toggle_start();
                        }
                        gv.classic.start.open_power();
                    }
                }
                Run => gv.toggle_run_launcher(),
            }
            gv.queue_redraw_all();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classic_table_overrides_and_unlisted_chords_fall_through() {
        let mut h = crate::state::tests::Harness::new();
        for mod_key in [crate::config::ModKey::Super, crate::config::ModKey::Alt] {
            h.gv.config.mod_key = mod_key;
            let mods = ModifiersState {
                logo: mod_key == crate::config::ModKey::Super,
                alt: mod_key == crate::config::ModKey::Alt,
                ..Default::default()
            };
            assert_eq!(
                binding(Keysym::from(keysyms::KEY_d), &mods, mod_key, false),
                None
            );
            h.gv.classic.mode = crate::classic::ShellMode::Classic;
            for (key, expected) in [
                (keysyms::KEY_d, ClassicAction::ShowDesktop),
                (keysyms::KEY_m, ClassicAction::MinimizeAll),
                (keysyms::KEY_i, ClassicAction::Settings),
                (keysyms::KEY_r, ClassicAction::Run),
                (keysyms::KEY_a, ClassicAction::Quick),
                (keysyms::KEY_n, ClassicAction::Notifications),
                (keysyms::KEY_Up, ClassicAction::Up),
                (keysyms::KEY_Down, ClassicAction::Down),
                (keysyms::KEY_Tab, ClassicAction::TaskView),
                (keysyms::KEY_1, ClassicAction::Item(0)),
                (keysyms::KEY_9, ClassicAction::Item(8)),
            ] {
                assert_eq!(
                    h.gv.bind_for(Keysym::from(key), &mods),
                    Some(Action::Classic(expected))
                );
            }
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_Return), &mods),
                Some(Action::Spawn(h.gv.config.terminal.clone()))
            );
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_e), &mods),
                Some(Action::Spawn(h.gv.config.file_manager.clone()))
            );
            let shift = ModifiersState {
                shift: true,
                ..mods
            };
            assert_eq!(
                h.gv.bind_for(Keysym::from(keysyms::KEY_w), &shift),
                Some(Action::Classic(ClassicAction::ToggleMode))
            );
            assert_eq!(
                binding(Keysym::from(keysyms::KEY_w), &shift, mod_key, false),
                Some(Action::Classic(ClassicAction::ToggleMode))
            );
            assert_eq!(
                binding(Keysym::from(keysyms::KEY_d), &shift, mod_key, true),
                None
            );
        }
    }
}
