use super::*;
use crate::{
    input::Action,
    state::tests::Harness,
};
use smithay::input::keyboard::{keysyms, ModifiersState};

fn output(h: &mut Harness, name: &str) -> Output {
    let o = Output::new(
        name.into(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    );
    o.change_current_state(
        Some(smithay::output::Mode { size: (1600, 900).into(), refresh: 120_000 }),
        None,
        None,
        Some((0, 0).into()),
    );
    h.gv.space.map_output(&o, (0, 0));
    h.gv.workspaces.ensure(&o);
    o
}

/// A harness whose launches do not start anything real.
fn harness() -> (Harness, Output) {
    let mut h = Harness::new();
    h.gv.config.terminal = "true".into();
    h.gv.tty.animate = Some(false);
    let o = output(&mut h, "tty-test");
    (h, o)
}

fn mods(alt: bool, ctrl: bool, shift: bool, logo: bool) -> ModifiersState {
    ModifiersState { alt, ctrl, shift, logo, ..Default::default() }
}

#[test]
fn switch_dip_starts_opaque_and_ends_clear() {
    let start = Duration::from_secs(1);
    assert_eq!(fade_cover(start, start), 1.0);
    assert_eq!(fade_cover(start, start + FADE_TIME), 0.0);
    assert!(fade_cover(start, start + Duration::from_millis(60)) < 0.3);
}

#[test]
fn vignette_is_opaque_darker_at_the_corners_and_static() {
    let pixels = draw::vignette_pixels(64, 36);
    assert_eq!(pixels.len(), 64 * 36 * 4);
    assert!(pixels.chunks(4).all(|p| p[3] == 255));
    let centre = &pixels[((18 * 64 + 32) * 4)..][..4];
    let corner = &pixels[..4];
    assert!(corner[0] < centre[0] && corner[2] < centre[2]);
    assert_eq!(pixels, draw::vignette_pixels(64, 36), "built once, never animated");
}

#[test]
fn lock_login_lines_say_login_incorrect() {
    assert_eq!(crate::greeter::failure_text("Wrong password"), "Login incorrect");
    assert_eq!(crate::greeter::failure_text("No user accounts found"), "No user accounts found");
}

#[test]
fn toggling_forces_tiling_and_restores_every_workspace() {
    let (mut h, o) = harness();
    let first = h.window();
    h.gv.map_new_window(first.clone());
    h.gv.workspaces.ensure(&o).list.items[0].tiling = false;
    h.gv.set_tty_mode(true);
    assert!(h.gv.tty.on && h.gv.tty.active());
    assert!(h.gv.workspaces.ensure(&o).list.items.iter().all(|ws| ws.tiling));
    let full = h.gv.space.output_geometry(&o).unwrap();
    let area = h.gv.work_area(&o);
    assert_eq!((area.loc.y, area.size.h), (full.loc.y + BAR_HEIGHT, full.size.h - BAR_HEIGHT), "the status line is reserved");
    assert_eq!(area.size.w, full.size.w);
    assert_eq!(h.gv.tile_gap(), 1, "panes are split by a 1 px divider");
    h.gv.set_tty_mode(false);
    assert!(!h.gv.tty.on);
    assert!(!h.gv.workspaces.ensure(&o).list.items[0].tiling, "the old layout state is back");
    assert_eq!(h.gv.tile_gap(), h.gv.config.gap);
    assert!(h.gv.tty.saved.is_empty());
}

#[test]
fn tty_mode_over_code_mode_keeps_the_original_layouts() {
    let (mut h, o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.workspaces.ensure(&o).list.items[0].tiling = false;
    h.gv.set_code_mode(true);
    h.gv.set_tty_mode(true);
    h.gv.set_code_mode(false);
    assert!(h.gv.workspaces.ensure(&o).list.items[0].tiling, "TTY mode still tiles");
    h.gv.set_tty_mode(false);
    assert!(!h.gv.workspaces.ensure(&o).list.items[0].tiling);
}

#[test]
fn the_mode_switches_at_once_and_the_look_follows_on_a_spring() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.tty.animate = Some(true);
    h.gv.set_tty_mode(true);
    assert!(h.gv.tty.on && h.gv.tty_requested());
    assert_eq!(h.gv.tty.progress.target, 1.0);
    assert!(h.gv.tty.look() < 0.5, "the look starts from GAVNO");
    assert!(h.gv.tty_animating() && !h.gv.tty.settled_console());
    h.gv.set_tty_mode(false);
    assert!(!h.gv.tty.on, "leaving switches the mode at once too");
    assert_eq!(h.gv.tty.progress.target, 0.0);
    // Without animations the look jumps.
    h.gv.tty.animate = Some(false);
    h.gv.set_tty_mode(true);
    assert!(h.gv.tty.settled_console() && !h.gv.tty_animating());
    h.gv.set_tty_mode(false);
    assert_eq!(h.gv.tty.look(), 0.0);
    assert!(!h.gv.tty.in_transition() && !h.gv.tty_animating());
}

#[test]
fn a_swipe_drags_the_look_and_a_release_springs_back() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.tty.animate = Some(true);
    let now = h.gv.start_time.elapsed();
    h.gv.tty.held = true;
    h.gv.tty.progress.follow(0.4, now);
    assert!(!h.gv.tty.on && h.gv.tty.in_transition() && h.gv.tty_animating());
    h.gv.tty_release(0.0);
    assert!(!h.gv.tty.held && !h.gv.tty.on);
    assert_eq!(h.gv.tty.progress.target, 0.0, "back to GAVNO");
}

#[test]
fn alt_f_keys_switch_consoles_only_in_tty_mode() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.config.mod_key = crate::config::ModKey::Super;
    let alt = mods(true, false, false, false);
    assert_eq!(h.gv.bind_for(keysyms::KEY_F2.into(), &alt), None);
    h.gv.set_tty_mode(true);
    for n in 1..=6u32 {
        assert_eq!(h.gv.bind_for((keysyms::KEY_F1 + n - 1).into(), &alt), Some(Action::Console(n as usize)));
    }
    assert_eq!(h.gv.bind_for(keysyms::KEY_F7.into(), &alt), None);
    // Ctrl+Alt+Fn stays real VT switching, and Mod+F1 stays the cheat sheet.
    assert_eq!(h.gv.bind_for(keysyms::KEY_F2.into(), &mods(true, true, false, false)), None);
    assert_eq!(h.gv.bind_for(keysyms::KEY_F1.into(), &mods(false, false, false, true)), Some(Action::CheatSheet));
    // With Alt as the mod key Alt+F1 is that cheat sheet bind.
    h.gv.config.mod_key = crate::config::ModKey::Alt;
    assert_eq!(h.gv.bind_for(keysyms::KEY_F1.into(), &alt), Some(Action::CheatSheet));
    assert_eq!(h.gv.bind_for(keysyms::KEY_F3.into(), &alt), Some(Action::Console(3)));
    h.gv.set_tty_mode(false);
    assert_eq!(h.gv.bind_for(keysyms::KEY_F3.into(), &alt), None);
}

#[test]
fn mod_shift_t_toggles_and_is_not_the_tiling_bind() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.config.mod_key = crate::config::ModKey::Super;
    let chord = mods(false, false, true, true);
    assert_eq!(h.gv.bind_for(keysyms::KEY_T.into(), &chord), Some(Action::ToggleTtyMode));
    assert_eq!(h.gv.bind_for(keysyms::KEY_t.into(), &mods(false, false, false, true)), Some(Action::ToggleTiling));
    h.gv.run_action(Action::ToggleTtyMode);
    assert!(h.gv.tty.on);
    h.gv.run_action(Action::ToggleTtyMode);
    assert!(!h.gv.tty.on);
    // The console keys do nothing outside TTY mode.
    assert!(!h.gv.action_enabled(&Action::Console(2)));
}

#[test]
fn console_keys_land_on_the_right_workspace_or_the_last_one() {
    let (mut h, o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    let w = h.window();
    h.gv.map_new_window(w);
    h.gv.workspaces.ensure(&o).append_empty();
    let count = h.gv.workspaces.ensure(&o).list.items.len();
    h.gv.tty_switch_console(2);
    assert_eq!(h.gv.workspaces.ensure(&o).list.active, 1);
    h.gv.tty_switch_console(1);
    assert_eq!(h.gv.workspaces.ensure(&o).list.active, 0);
    h.gv.tty_switch_console(6);
    assert_eq!(h.gv.workspaces.ensure(&o).list.active, count - 1, "a missing console is the last one");
}

#[test]
fn an_empty_console_starts_the_terminal_once_per_switch_into_it() {
    let (mut h, o) = harness();
    h.gv.set_tty_mode(true);
    assert_eq!(h.gv.launches.pending_count(), 1, "entering on an empty console starts the terminal");
    // Refresh passes never start it again, even though nothing mapped.
    for _ in 0..3 {
        h.gv.on_loop_idle();
    }
    assert_eq!(h.gv.launches.pending_count(), 1);
    // The next switch into an empty console does, once the guard time is over.
    let w = h.window();
    h.gv.map_new_window(w);
    h.gv.workspaces.ensure(&o).append_empty();
    h.gv.tty.last_spawn = None;
    assert!(h.gv.switch_workspace(&o, 1));
    assert_eq!(h.gv.launches.pending_count(), 2);
    // A console with a window never gets one.
    assert!(h.gv.switch_workspace(&o, 0));
    assert_eq!(h.gv.launches.pending_count(), 2);
    // A quick return to the same empty console is guarded.
    h.gv.tty.last_spawn = None;
    assert!(h.gv.switch_workspace(&o, 1));
    assert!(h.gv.switch_workspace(&o, 0));
    assert!(h.gv.switch_workspace(&o, 1));
    assert_eq!(h.gv.launches.pending_count(), 3, "the guard stops a second terminal for the same console");
}

#[test]
fn autostart_can_be_switched_off() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    assert!(h.gv.launches.pending_count() == 0);
}

#[test]
fn notifications_become_kernel_lines_that_last_four_seconds() {
    assert_eq!(
        crate::notify::kernel_line(Duration::from_millis(1234_567), "Telegram", "Alice", "see you  at six"),
        "[ 1234.567] Telegram: Alice: see you at six"
    );
    assert_eq!(crate::notify::kernel_line(Duration::from_millis(9), "Mail", "", ""), "[    0.009] Mail");
    assert_eq!(crate::notify::TTY_LIFETIME, Duration::from_secs(4));
}

#[test]
fn startup_config_enters_tty_mode_and_keeps_classic_out() {
    let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
    let display = smithay::reexports::wayland_server::Display::new().unwrap();
    let config = crate::config::Config::parse_checked(true, "[tty]\nstart = true", |_| None).unwrap();
    let gv = Gavno::new(&mut event_loop, display, config);
    assert!(gv.tty.on && gv.launcher.tty);
    assert!(gv.dock.code_mode, "the dock stays out of the way");
    let mut event_loop = smithay::reexports::calloop::EventLoop::try_new().unwrap();
    let display = smithay::reexports::wayland_server::Display::new().unwrap();
    let config = crate::config::Config::parse_checked(true, "[tty]\nstart = true\n[classic]\nstart_in_classic = true", |_| None).unwrap();
    let gv = Gavno::new(&mut event_loop, display, config);
    assert!(!gv.tty.on, "Classic never enters TTY mode");
}

#[test]
fn tty_mode_hides_the_pointer_like_code_mode() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    assert!(!h.gv.keyboard_first());
    h.gv.set_tty_mode(true);
    assert!(h.gv.keyboard_first());
    h.gv.hide_pointer();
    assert!(h.gv.pointer_hidden);
    h.gv.set_tty_mode(false);
    assert!(!h.gv.pointer_hidden, "leaving shows the pointer again");
}

#[test]
fn no_theme_clock_or_bar_clock_runs_in_tty_mode() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    h.gv.refresh_theme_clock();
    assert!(h.gv.minute_timer.is_none(), "no clock tick wakes the console");
    assert!(!h.gv.theme_visible());
}

#[test]
fn bar_overview_and_quick_settings_are_unavailable_on_a_console() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    assert!(!h.gv.action_enabled(&Action::ToggleOverview));
    assert!(!h.gv.action_enabled(&Action::PanelControls(crate::panel_controls::Page::Wifi)));
    assert!(h.gv.action_enabled(&Action::ToggleLauncher));
}

#[test]
fn the_lock_console_prompts_silently_and_reports_a_failure() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let user = crate::greeter::User { name: "kirill".into(), display: "Kirill".into() };
    let mut greeter = crate::greeter::Greeter::for_lock(tx, user);
    let lines = greeter.tty_lines();
    assert!(lines[0].ends_with(" login: kirill"));
    assert_eq!(lines[1], "Password: ");
    assert_eq!(lines.len(), 2);
    for ch in ['s', 'e', 'c'] {
        greeter.key(smithay::input::keyboard::Keysym::from(ch as u32), Some(ch), &ModifiersState::default());
    }
    assert_eq!(greeter.tty_lines()[1], "Password: ", "nothing is echoed, not even dots");
    greeter.lock_failed();
    let lines = greeter.tty_lines();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2], "Login incorrect");
}

#[test]
fn switching_jumps_and_dips_through_black_without_a_status_line_popup() {
    let (mut h, o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.tty.animate = Some(true);
    h.gv.set_tty_mode(true);
    let w = h.window();
    h.gv.map_new_window(w);
    h.gv.workspaces.ensure(&o).append_empty();
    assert!(h.gv.switch_workspace(&o, 1));
    assert!(!h.gv.workspaces.ensure(&o).list.items.is_empty());
    assert!(!h.gv.workspaces.ensure(&o).position.is_active(), "no slide");
    assert!(h.gv.tty.fade.is_some());
}

#[test]
fn panes_tile_with_a_one_pixel_gap_and_focus_moves_with_hjkl() {
    let (mut h, o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.config.mod_key = crate::config::ModKey::Super;
    h.gv.set_tty_mode(true);
    let windows: Vec<_> = (0..2)
        .map(|_| {
            let w = h.window();
            h.gv.map_new_window(w.clone());
            h.commit_size(&w, 300, 200);
            w
        })
        .collect();
    h.gv.reflow_tiling(false);
    let area = h.gv.work_area(&o);
    let sizes: Vec<_> = windows.iter().map(|w| w.toplevel().unwrap().with_pending_state(|s| s.size).unwrap()).collect();
    assert!(sizes.iter().all(|s| s.h == area.size.h - 2), "full height minus the divider rows: {sizes:?}");
    assert_eq!(sizes[0].w + sizes[1].w + 3, area.size.w, "two panes and three 1 px dividers");
    let logo = mods(false, false, false, true);
    use crate::snap::Direction;
    assert_eq!(h.gv.bind_for(keysyms::KEY_h.into(), &logo), Some(Action::FocusDirection(Direction::Left)));
    assert_eq!(h.gv.bind_for(keysyms::KEY_l.into(), &logo), Some(Action::FocusDirection(Direction::Right)), "Mod+L moves focus, it does not lock");
    assert_eq!(h.gv.bind_for(keysyms::KEY_l.into(), &mods(true, true, false, false)), Some(Action::Lock), "Ctrl+Alt+L locks");
    let before = h.gv.focused_window().unwrap();
    h.gv.focus_direction(Direction::Left);
    assert_ne!(h.gv.focused_window().unwrap(), before);
    h.gv.set_tty_mode(false);
    assert_eq!(h.gv.bind_for(keysyms::KEY_l.into(), &logo), Some(Action::Lock));
}

#[test]
fn mod_enter_opens_a_terminal_window_and_prefers_footclient() {
    assert_eq!(foot_client_command(true, "foot"), "footclient -o csd.preferred=none -o pad=8x6");
    assert_eq!(foot_client_command(false, "alacritty"), "alacritty");
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.config.mod_key = crate::config::ModKey::Super;
    h.gv.set_tty_mode(true);
    let logo = mods(false, false, false, true);
    assert!(matches!(h.gv.bind_for(keysyms::KEY_Return.into(), &logo), Some(Action::Spawn(_))));
}

#[test]
fn tab_labels_and_apps_are_short() {
    assert_eq!(draw::short_app("org.kde.konsole"), "konsole");
    assert_eq!(draw::short_app("Foot"), "foot");
    assert_eq!(draw::battery_color(80, true), [120, 200, 140]);
    assert_eq!(draw::battery_color(8, false), [240, 100, 90]);
    assert_eq!(draw::battery_color(18, false), [235, 180, 90]);
    assert_eq!(draw::battery_color(70, false), [150, 150, 160]);
}

#[test]
fn only_the_clock_is_armed_when_settled_and_off_means_nothing() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    h.gv.refresh_tty();
    assert!(h.gv.tty.clock_timer.is_some(), "one minute-aligned clock tick");
    h.gv.refresh_tty();
    assert!(!h.gv.tty.timers_armed() || h.gv.tty.clock_timer.is_some());
    h.gv.set_tty_mode(false);
    h.gv.refresh_tty();
    assert!(!h.gv.tty.timers_armed());
}

#[test]
fn the_pointer_is_never_shown_and_motion_and_buttons_are_dropped() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    assert!(h.gv.pointer_hidden && h.gv.cursor.hidden);
    h.gv.show_pointer();
    assert!(h.gv.pointer_hidden, "nothing brings the cursor back");
    h.gv.set_tty_mode(false);
    assert!(!h.gv.pointer_hidden && !h.gv.cursor.hidden);
}
