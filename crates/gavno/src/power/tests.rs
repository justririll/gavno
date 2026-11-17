use super::*;
use crate::state::tests::Harness;
use smithay::{
    output::{Mode, Output, PhysicalProperties, Subpixel},
    wayland::{idle_inhibit::IdleInhibitHandler, seat::WaylandFocus},
};

fn setup() -> (Harness, Output) {
    let mut h = Harness::new();
    h.gv.config.lock.after_idle_secs = 0;
    let output = Output::new(
        "test".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    );
    output.change_current_state(
        Some(Mode {
            size: (800, 600).into(),
            refresh: 60000,
        }),
        None,
        None,
        None,
    );
    h.gv.space.map_output(&output, (0, 0));
    (h, output)
}
fn dispatch(h: &mut Harness, duration: Duration) {
    let until = Instant::now() + duration;
    loop {
        h.event_loop
            .dispatch(
                Some(until.saturating_duration_since(Instant::now())),
                &mut h.gv,
            )
            .unwrap();
        if Instant::now() >= until {
            break;
        }
    }
}

#[test]
fn idle_is_one_shot_coalesces_and_resets_without_polling() {
    let (mut h, _) = setup();
    h.gv.refresh_idle();
    let deadline = h.gv.power.deadline.unwrap();
    assert!(h.gv.power.idle_timer.is_some());
    for _ in 0..10 {
        h.gv.refresh_idle();
    }
    assert_eq!(h.gv.power.deadline, Some(deadline));
    h.gv.protocols.idle_policy.activity();
    h.gv.refresh_idle();
    assert!(h.gv.power.deadline.unwrap() > deadline);
    // Run the actual production timer source, with a short test deadline.
    h.gv.cancel_power_idle();
    h.gv.arm_power_idle(Instant::now() + Duration::from_millis(10));
    dispatch(&mut h, Duration::from_millis(25));
    assert!(h.gv.power.off);
    assert!(h.gv.power.idle_timer.is_none());
    assert!(h.gv.power.frame_timer.is_none());
    assert!(h.gv.power.deadline.is_none());
    for _ in 0..10 {
        h.gv.refresh_idle();
        h.gv.queue_redraw_all();
    }
    assert!(h.gv.power.idle_timer.is_none());
    assert!(h.gv.power.frame_timer.is_none());
    assert_eq!(h.gv.power.frame_timer_firings, 0);
}

#[test]
fn zero_and_activity_cancel_an_expired_timer() {
    let (mut h, _) = setup();
    h.gv.arm_power_idle(Instant::now());
    h.gv.protocols.idle_policy.activity();
    dispatch(&mut h, Duration::from_millis(5));
    assert!(!h.gv.power.off);
    assert!(h.gv.power.deadline.unwrap() > Instant::now());
    h.gv.config.display.off_after_idle_secs = 0;
    h.gv.refresh_idle();
    assert!(h.gv.power.idle_timer.is_none());
    assert!(h.gv.power.deadline.is_none());
}

#[test]
fn only_visible_inhibitors_prevent_power_off() {
    let (mut h, _) = setup();
    let window = h.window(); h.gv.map_new_window(window.clone());
    let surface = window.wl_surface().unwrap().into_owned();
    h.gv.inhibit(surface.clone());
    assert!(!h.gv.protocols.idle.is_inhibited());
    assert!(h.gv.power.idle_timer.is_some());
    h.commit_size(&window, 200, 150); h.gv.space.refresh();
    h.gv.refresh_idle();
    assert!(h.gv.protocols.idle.is_inhibited());
    assert!(h.gv.power.idle_timer.is_none());
    // Even a timeout queued earlier in this batch must see the visible inhibitor.
    h.gv.arm_power_idle(Instant::now());
    dispatch(&mut h, Duration::from_millis(5));
    assert!(!h.gv.power.off);
    h.gv.uninhibit(surface);
    assert!(h.gv.power.idle_timer.is_some());
    h.gv.cancel_power_idle();
    h.gv.arm_power_idle(Instant::now());
    dispatch(&mut h, Duration::from_millis(5));
    assert!(h.gv.power.off);
}

#[test]
fn sleeping_frame_callbacks_are_demand_driven_and_throttled() {
    let (mut h, _) = setup();
    let window = h.window(); h.gv.map_new_window(window.clone());
    h.commit_size(&window, 200, 150); h.gv.space.refresh();
    h.gv.power_off_monitors("test");
    assert!(h.gv.power.frame_timer.is_none());
    h.request_frame(&window);
    assert!(h.gv.power.frame_timer.is_some());
    for _ in 0..100 {
        h.gv.queue_redraw_all();
    }
    dispatch(&mut h, Duration::from_millis(50));
    assert_eq!(h.frame_callbacks(), 0);
    assert_eq!(h.gv.power.frame_timer_firings, 0);
    dispatch(&mut h, Duration::from_millis(1000));
    assert_eq!(h.frame_callbacks(), 1);
    assert_eq!(h.gv.power.frame_timer_firings, 1);
    assert!(h.gv.power.frame_timer.is_none());
    h.request_frame(&window);
    dispatch(&mut h, Duration::from_millis(50));
    assert_eq!(h.frame_callbacks(), 0);
    dispatch(&mut h, Duration::from_millis(1000));
    assert_eq!(h.frame_callbacks(), 1);
    assert_eq!(h.gv.power.frame_timer_firings, 2);
    dispatch(&mut h, Duration::from_millis(1100));
    assert_eq!(
        h.gv.power.frame_timer_firings, 2,
        "no periodic timer while clients are idle"
    );
    h.request_frame(&window);
    h.gv.power_on_monitors("test");
    assert!(h.gv.power.frame_timer.is_none());
    assert!(!h.gv.power.off);
    assert!(h.gv.power.idle_timer.is_some());
}

#[test]
fn lock_and_power_timeouts_are_independent_and_locking_does_not_wake() {
    let (mut h, _) = setup();
    h.gv.config.display.off_after_idle_secs = 0;
    assert!(h.gv.lock_session());
    h.gv.refresh_idle();
    assert!(h.gv.power.idle_timer.is_none());
    h.gv.config.display.off_after_idle_secs = 1;
    h.gv.refresh_idle();
    assert!(
        h.gv.power.idle_timer.is_some(),
        "locked sessions still power off"
    );
    h.gv.power_off_monitors("test");
    h.gv.refresh_internal_lock();
    assert!(h.gv.power.off);
    assert!(!h.gv.is_animating());
    h.gv.power_on_monitors("test");
    assert!(h.gv.protocols.locks.locked);
    assert!(h.gv.lock.internal());
    let (mut h, _) = setup();
    h.gv.power_off_monitors("test");
    assert!(h.gv.lock_session());
    assert!(h.gv.power.off);
}

#[test]
fn reload_rearms_the_timeout_and_zero_wakes() {
    let (mut h, _) = setup();
    h.gv.refresh_idle();
    let mut config = h.gv.config.clone();
    config.display.off_after_idle_secs = 42;
    let prepared = crate::config_reload::Prepared::new(config, &h.gv.config).unwrap();
    let result = crate::config_reload::apply(&mut h.gv, prepared).unwrap();
    assert!(result["applied"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("display.off_after_idle_secs")));
    assert!(
        h.gv.power
            .deadline
            .unwrap()
            .duration_since(Instant::now())
            .as_secs()
            <= 42
    );
    h.gv.power_off_monitors("test");
    let mut config = h.gv.config.clone();
    config.display.off_after_idle_secs = 0;
    let prepared = crate::config_reload::Prepared::new(config, &h.gv.config).unwrap();
    crate::config_reload::apply(&mut h.gv, prepared).unwrap();
    assert!(!h.gv.power.off);
    assert!(h.gv.power.idle_timer.is_none());
    // An explicit power-off with automatic power-off already disabled is also
    // undone by reloading the same zero value.
    h.gv.power_off_monitors("test");
    let prepared = crate::config_reload::Prepared::new(h.gv.config.clone(), &h.gv.config).unwrap();
    crate::config_reload::apply(&mut h.gv, prepared).unwrap();
    assert!(!h.gv.power.off);
}

#[test]
fn dbus_inhibits_keep_the_screen_on_except_while_locked() {
    let (mut h, _) = setup();
    h.gv.config.lock.after_idle_secs = 1;
    h.gv.refresh_idle();
    assert!(h.gv.power.idle_timer.is_some());
    assert!(h.gv.lock.idle_timer_armed());
    h.gv.protocols.idle_policy.external_inhibits = 1;
    h.gv.refresh_idle();
    assert!(h.gv.power.idle_timer.is_none());
    assert!(!h.gv.lock.idle_timer_armed());
    assert!(h.gv.protocols.idle.is_inhibited());
    h.gv.protocols.idle_policy.external_inhibits = 0;
    h.gv.refresh_idle();
    assert!(h.gv.power.idle_timer.is_some());
    assert!(h.gv.lock.idle_timer_armed());
    // While locked the count is ignored.
    h.gv.protocols.idle_policy.external_inhibits = 2;
    assert!(h.gv.lock_session());
    h.gv.refresh_idle();
    assert!(!h.gv.protocols.idle.is_inhibited());
    assert!(h.gv.power.idle_timer.is_some());
}

#[test]
fn simulated_activity_restarts_countdown_but_never_wakes_outputs() {
    let (mut h, _) = setup();
    h.gv.refresh_idle();
    let deadline = h.gv.power.deadline.unwrap();
    std::thread::sleep(Duration::from_millis(5));
    h.gv.protocols.idle_policy.activity();
    h.gv.refresh_idle();
    assert!(h.gv.power.deadline.unwrap() > deadline);
    h.gv.power_off_monitors("test");
    h.gv.protocols.idle_policy.activity();
    h.gv.refresh_idle();
    assert!(h.gv.power.off);
    assert!(h.gv.power.idle_timer.is_none());
}
