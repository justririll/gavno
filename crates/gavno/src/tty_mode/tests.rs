use super::*;
use crate::{
    input::Action,
    state::tests::Harness,
    topbar::{status::{Network, Snapshot}, Battery},
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

fn dispatch_for(h: &mut Harness, ms: u64) {
    let end = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < end {
        h.event_loop.dispatch(Duration::from_millis(10), &mut h.gv).unwrap();
    }
}

fn mods(alt: bool, ctrl: bool, shift: bool, logo: bool) -> ModifiersState {
    ModifiersState { alt, ctrl, shift, logo, ..Default::default() }
}

#[test]
fn ramp_fades_in_80_ms_and_back_out() {
    let mut ramp = Ramp::default();
    let t0 = Duration::from_secs(5);
    assert_eq!(ramp.alpha(t0), 0.0);
    assert!(!ramp.animating(t0));
    assert!(ramp.set(true, t0));
    assert!(!ramp.set(true, t0), "no change, no new fade");
    assert!((ramp.alpha(t0 + Duration::from_millis(40)) - 0.5).abs() < 1e-3);
    assert_eq!(ramp.alpha(t0 + Duration::from_millis(80)), 1.0);
    assert!(!ramp.animating(t0 + Duration::from_millis(80)));
    // Reversing half way starts from the visible value.
    assert!(ramp.set(false, t0 + Duration::from_millis(100)));
    assert!((ramp.alpha(t0 + Duration::from_millis(140)) - 0.5).abs() < 1e-3);
    assert_eq!(ramp.alpha(t0 + Duration::from_millis(200)), 0.0);
}

#[test]
fn switch_dip_and_crt_shapes_start_and_end_where_they_should() {
    let start = Duration::from_secs(1);
    assert_eq!(fade_cover(start, start), 1.0);
    assert_eq!(fade_cover(start, start + FADE_TIME), 0.0);
    assert!(fade_cover(start, start + Duration::from_millis(60)) < 0.3);
    // Dark, a bright line, the picture growing, fully on.
    assert_eq!(crt_shape(0.0), (0.0, 1.0));
    assert_eq!(crt_shape(0.29), (0.0, 1.0));
    let (middle, line) = crt_shape(0.6);
    assert!(middle > 0.3 && middle < 1.0 && line < 1.0 && line > 0.0);
    assert_eq!(crt_shape(1.0), (1.0, 0.0));
}

#[test]
fn console_text_uses_host_user_and_number() {
    let lines = draw::console_lines("vivobook", "kirill", 3);
    assert_eq!(lines[0], format!("GAVNO {} vivobook tty3", env!("CARGO_PKG_VERSION")));
    assert_eq!(lines[1], "");
    assert_eq!(lines[2], "vivobook login: kirill");
    assert_eq!(lines[3], "[kirill@vivobook ~]$ ");
}

#[test]
fn status_line_right_side_reads_the_bar_sources() {
    let mut status = Snapshot::default();
    status.network = Network::Wifi { ssid: "home".into(), strength: 87 };
    status.battery = Some(Battery { percent: 64, charging: false, minutes: None });
    assert_eq!(draw::status_right(&status, 14, 32), "wifi 87%  bat 64%  14:32");
    status.battery = None;
    status.network = Network::Offline;
    assert_eq!(draw::status_right(&status, 9, 5), "net off  09:05");
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
    assert_eq!(h.gv.work_area(&o), h.gv.space.output_geometry(&o).unwrap(), "no bar and no dock");
    assert_eq!(h.gv.tile_gap(), 0);
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
fn leaving_plays_the_power_off_first_and_a_new_request_cancels_it() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.tty.animate = Some(true);
    h.gv.set_tty_mode(true);
    assert!(h.gv.tty.crt.is_some_and(|(_, on)| on));
    h.gv.set_tty_mode(false);
    assert!(h.gv.tty.on && h.gv.tty.closing && !h.gv.tty_requested());
    assert!(h.gv.tty.crt.is_some_and(|(_, on)| !on));
    assert!(h.gv.tty.crt_timer.is_some());
    h.gv.set_tty_mode(true);
    assert!(h.gv.tty.on && !h.gv.tty.closing && h.gv.tty.crt_timer.is_none());
    h.gv.set_tty_mode(false);
    h.gv.finish_tty_leave();
    assert!(!h.gv.tty.on && !h.gv.tty.closing);
    assert!(h.gv.tty.crt.is_none() && h.gv.tty.crt_timer.is_none());
    assert!(!h.gv.tty_animating());
}

#[test]
fn the_power_off_ends_by_itself() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.tty.animate = Some(true);
    h.gv.set_tty_mode(true);
    h.gv.set_tty_mode(false);
    dispatch_for(&mut h, 320);
    assert!(!h.gv.tty.on, "the one-shot timer finished the leave");
    assert!(!h.gv.tty.timers_armed());
}

#[test]
fn every_tile_fills_the_console_and_focus_cycles_in_order() {
    let (mut h, o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    let windows: Vec<_> = (0..3)
        .map(|_| {
            let w = h.window();
            h.gv.map_new_window(w.clone());
            h.commit_size(&w, 300, 200);
            w
        })
        .collect();
    h.gv.reflow_tiling(false);
    let full = h.gv.space.output_geometry(&o).unwrap().size;
    for w in &windows {
        assert_eq!(w.toplevel().unwrap().with_pending_state(|s| s.size), Some(full));
    }
    let focused = h.gv.focused_window().unwrap();
    let mut seen = vec![focused.clone()];
    for _ in 0..2 {
        h.gv.focus_direction(crate::snap::Direction::Down);
        seen.push(h.gv.focused_window().unwrap());
    }
    assert!(windows.iter().all(|w| seen.contains(w)), "three steps visit every window");
    h.gv.focus_direction(crate::snap::Direction::Down);
    assert_eq!(h.gv.focused_window(), Some(focused.clone()), "and wrap around");
    h.gv.focus_direction(crate::snap::Direction::Up);
    assert_eq!(h.gv.focused_window(), Some(seen[2].clone()), "Up goes the other way");
    // Only the focused window is drawn: the others stay behind it.
    let scene = draw::scene(&h.gv.space, &o);
    assert_eq!(scene.len(), 1);
    assert_eq!(Some(&scene[0].window), h.gv.focused_window().as_ref());
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
fn mod_j_and_k_walk_the_windows_of_a_console_and_mod_l_still_locks() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.config.mod_key = crate::config::ModKey::Super;
    let logo = mods(false, false, false, true);
    assert_eq!(h.gv.bind_for(keysyms::KEY_j.into(), &logo), None);
    h.gv.set_tty_mode(true);
    use crate::snap::Direction;
    assert_eq!(h.gv.bind_for(keysyms::KEY_j.into(), &logo), Some(Action::FocusDirection(Direction::Down)));
    assert_eq!(h.gv.bind_for(keysyms::KEY_k.into(), &logo), Some(Action::FocusDirection(Direction::Up)));
    assert_eq!(h.gv.bind_for(keysyms::KEY_l.into(), &logo), Some(Action::Lock));
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
fn switching_jumps_instead_of_sliding_and_dips_through_black() {
    let (mut h, o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.tty.animate = Some(true);
    h.gv.set_tty_mode(true);
    let w = h.window();
    h.gv.map_new_window(w);
    h.gv.workspaces.ensure(&o).append_empty();
    assert!(h.gv.switch_workspace(&o, 1));
    assert!(!h.gv.workspaces.ensure(&o).position.is_active(), "no slide");
    assert!(h.gv.tty.fade.is_some());
    assert!(h.gv.tty.status.target(), "the status line shows after a switch");
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
fn holding_mod_shows_the_status_line_after_150_ms_and_releasing_hides_it() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    // Drop the linger of entering first.
    h.gv.tty.linger_until = None;
    if let Some(token) = h.gv.tty.linger_timer.take() {
        h.gv.loop_handle.remove(token);
    }
    h.gv.tty_update_status();
    assert!(!h.gv.tty.status.target());
    h.gv.tty_mod_changed(true, false);
    assert!(h.gv.tty.mod_timer.is_some());
    assert!(!h.gv.tty.status.target(), "not before the hold time");
    dispatch_for(&mut h, 220);
    assert!(h.gv.tty.mod_visible && h.gv.tty.status.target());
    assert!(h.gv.tty.status_wanted(), "the status worker is asked for data");
    h.gv.tty_mod_changed(false, false);
    assert!(!h.gv.tty.status.target());
}

#[test]
fn a_chord_does_not_pop_up_the_status_line() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    h.gv.tty_mod_changed(true, false);
    assert!(h.gv.tty.mod_timer.is_some());
    h.gv.tty_mod_changed(true, true);
    assert!(h.gv.tty.mod_timer.is_none());
    h.gv.tty_mod_changed(false, false);
}

#[test]
fn the_status_line_lingers_for_one_and_a_half_seconds_after_a_switch() {
    let (mut h, o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    let w = h.window();
    h.gv.map_new_window(w);
    h.gv.workspaces.ensure(&o).append_empty();
    assert!(h.gv.switch_workspace(&o, 1));
    assert!(h.gv.tty.status.target());
    assert!(h.gv.tty.linger_timer.is_some());
    dispatch_for(&mut h, 1700);
    assert!(!h.gv.tty.status.target());
    assert!(h.gv.tty.linger_timer.is_none());
}

#[test]
fn nothing_is_armed_when_the_console_is_settled() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    // An empty console blinks its cursor, and only then.
    h.gv.refresh_tty();
    assert!(h.gv.tty.blink_timer.is_some());
    let w = h.window();
    h.gv.map_new_window(w);
    h.gv.refresh_tty();
    assert!(h.gv.tty.blink_timer.is_none(), "a window hides the empty console");
    h.gv.tty.linger_until = None;
    if let Some(token) = h.gv.tty.linger_timer.take() {
        h.gv.loop_handle.remove(token);
    }
    h.gv.tty_update_status();
    dispatch_for(&mut h, 120);
    assert!(!h.gv.tty.timers_armed());
    assert!(!h.gv.tty_animating());
    // Off, nothing at all.
    h.gv.set_tty_mode(false);
    h.gv.refresh_tty();
    assert!(!h.gv.tty.timers_armed());
}

#[test]
fn the_cursor_blinks_every_530_ms_on_an_empty_console() {
    let (mut h, _o) = harness();
    h.gv.config.tty.autostart_terminal = false;
    h.gv.set_tty_mode(true);
    h.gv.refresh_tty();
    assert!(h.gv.tty.blink_on);
    dispatch_for(&mut h, 620);
    assert!(!h.gv.tty.blink_on);
    dispatch_for(&mut h, 560);
    assert!(h.gv.tty.blink_on);
    // The lock screen stops it.
    h.gv.protocols.locks.locked = true;
    h.gv.refresh_tty();
    assert!(h.gv.tty.blink_timer.is_none());
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
