use super::*;

fn notice(id: u32) -> Notice {
    Notice {
        id,
        gen: id as u64,
        app: "Test".into(),
        icon: String::new(),
        desktop: String::new(),
        summary: format!("Notice {id}"),
        body: "<b>Hello</b> &amp; goodbye".into(),
        actions: vec![("default".into(), "Open".into())],
        urgency: 1,
        timeout: -1,
    }
}
fn model() -> Notifications {
    Notifications {
        active: true,
        ..Default::default()
    }
}
fn visible(n: &Notifications) -> Vec<u32> {
    n.cards
        .iter()
        .filter(|c| c.visible && !c.closing)
        .map(|c| c.notice.id)
        .collect()
}

#[test]
fn replacement_keeps_slot_and_visible_limit_promotes_queue() {
    let mut n = model();
    for id in 1..=5 {
        n.receive(notice(id), Duration::ZERO, 5000);
    }
    n.update(Duration::ZERO, false, 3);
    assert_eq!(visible(&n), [1, 2, 3]);
    let mut replace = notice(2);
    replace.summary = "Updated".into();
    n.receive(replace, Duration::ZERO, 5000);
    assert_eq!(
        n.cards.iter().map(|c| c.notice.id).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    assert_eq!(n.cards[1].notice.summary, "Updated");
    n.close(2, Some(2));
    n.cards[1].spring.jump(0.0);
    n.update(Duration::ZERO, false, 3);
    assert_eq!(visible(&n), [1, 3, 4]);
    n.update(Duration::ZERO, false, 1);
    assert_eq!(visible(&n), [1]);
}
#[test]
fn expiry_persistence_critical_and_no_hidden_timer() {
    let mut n = model();
    let (tx, rx) = mpsc::channel();
    n.signals = Some(tx);
    n.receive(notice(1), Duration::ZERO, 5000);
    let mut persistent = notice(2);
    persistent.timeout = 0;
    n.receive(persistent, Duration::ZERO, 5000);
    let mut critical = notice(3);
    critical.urgency = 2;
    critical.timeout = 1;
    n.receive(critical, Duration::ZERO, 5000);
    n.update(Duration::ZERO, false, 3);
    assert_eq!(n.next_expiry(), Some(Duration::from_secs(5)));
    n.update(Duration::from_secs(6), false, 3);
    assert_eq!(visible(&n), [2, 3]);
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(1, 1, 1));
    assert_eq!(n.next_expiry(), None);
    n.update(Duration::from_secs(6), true, 3);
    assert!(visible(&n).is_empty());
    assert_eq!(n.next_expiry(), None);
    n.update(Duration::from_secs(7), false, 3);
    assert_eq!(visible(&n), [2, 3]);
}
#[test]
fn dnd_suppresses_normal_but_not_critical_and_history_caps_at_fifty() {
    let mut n = model();
    n.dnd = true;
    for id in 1..=55 {
        n.receive(notice(id), Duration::ZERO, 5000);
    }
    let mut critical = notice(56);
    critical.urgency = 2;
    n.receive(critical, Duration::ZERO, 5000);
    n.update(Duration::ZERO, false, 3);
    assert_eq!(visible(&n), [56]);
    assert_eq!(n.history.len(), 50);
    assert_eq!(n.history.front().unwrap().notice.id, 56);
    assert_eq!(n.history.back().unwrap().notice.id, 7);
    assert!(n.unread);
    n.update(Duration::from_secs(6), false, 3);
    n.dnd = false;
    n.update(Duration::from_secs(6), false, 3);
    assert_eq!(visible(&n), [25, 26, 27]);
}
#[test]
fn supported_markup_decodes_once_and_preserves_literal_angles() {
    assert_eq!(
        plain_body(
            "<b>Bold</b> <i>I</i><u>U</u> <a href='x'>link</a> &amp; &lt; &gt; &quot; &apos;"
        ),
        "Bold IU link & < > \" '"
    );
    assert_eq!(plain_body("1 < 2 &amp;lt; <broken"), "1 < 2 &lt; <broken");
    assert_eq!(plain_body("<script>text</script>"), "<script>text</script>");
    assert_eq!(plain_body(&"<".repeat(200_000)).len(), 200_000);
    assert_eq!(
        plain_body(&format!("{}>", "<".repeat(200_000))).len(),
        200_001
    );
}

fn harness() -> (crate::state::tests::Harness, Output) {
    use smithay::output::{Mode, PhysicalProperties, Subpixel};
    let mut h = crate::state::tests::Harness::new();
    let output = Output::new(
        "notifications-test".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "test".into(),
            model: "test".into(),
        },
    );
    output.change_current_state(
        Some(Mode {
            size: (1646, 926).into(),
            refresh: 60000,
        }),
        None,
        None,
        None,
    );
    h.gv.space.map_output(&output, (0, 0));
    h.gv.workspaces.ensure(&output);
    (h, output)
}
fn settle(h: &mut crate::state::tests::Harness) {
    let mut n = h.gv.notifications.borrow_mut();
    for c in &mut n.cards {
        c.spring.jump(if c.closing { 0.0 } else { 1.0 });
    }
    n.layout((1646, 926).into());
}
#[test]
fn channel_card_hit_click_action_and_lock_suppression() {
    let (mut h, output) = harness();
    let tx = channel(&h.gv.loop_handle).unwrap();
    let (signals, rx) = mpsc::channel();
    h.gv.notifications.borrow_mut().signals = Some(signals);
    tx.send(Request::Active).unwrap();
    tx.send(Request::Notify(notice(1))).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    settle(&mut h);
    let r = h.gv.notifications.borrow().cards[0].rect.unwrap();
    let p = (r.loc.x as f64 + 80.0, r.loc.y as f64 + 70.0).into();
    assert!(h.gv.shell_contains(p));
    assert!(h.gv.notifications.borrow().contains(&output, p));
    assert!(h.gv.notification_button(p, 0x110, true));
    assert_eq!(
        rx.try_recv().unwrap(),
        Signal::Action(1, 1, "default".into())
    );
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(1, 1, 2));
    assert!(h.gv.notification_button(p, 0x110, true));
    assert!(rx.try_recv().is_err());
    tx.send(Request::Notify(notice(2))).unwrap();
    h.gv.protocols.locks.locked = true;
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    assert!(visible(&h.gv.notifications.borrow()).is_empty());
    assert!(!h.gv.notifications.borrow().animating());
    assert!(h.gv.notifications.borrow().timer.is_none());
    assert!(!h.gv.notifications.borrow().contains(&output, p));
    assert!(!h.gv.notification_button(p, 0x110, true));
    h.gv.protocols.locks.locked = false;
    h.gv.refresh_notifications();
    assert_eq!(visible(&h.gv.notifications.borrow()), [2]);
}
#[test]
fn hover_pauses_expiry_and_empty_state_has_no_timer_or_animation() {
    let (mut h, _) = harness();
    let tx = channel(&h.gv.loop_handle).unwrap();
    tx.send(Request::Active).unwrap();
    tx.send(Request::Notify(notice(1))).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    settle(&mut h);
    assert!(h.gv.notifications.borrow().timer.is_some());
    let r = h.gv.notifications.borrow().cards[0].rect.unwrap();
    h.gv.notification_motion((r.loc.x as f64 + 40.0, r.loc.y as f64 + 50.0).into());
    assert!(h.gv.notifications.borrow().cards[0].hovered.is_some());
    assert!(h.gv.notifications.borrow().timer.is_none());
    assert!(!h.gv.notifications.borrow().animating());
    h.gv.notification_motion((500.0, 500.0).into());
    assert!(h.gv.notifications.borrow().timer.is_some());
    h.gv.reset_notifications();
    assert!(h.gv.notifications.borrow().timer.is_none());
    assert!(!h.gv.notifications.borrow().animating());
}
#[test]
fn panic_drops_ui_but_channel_remains_usable() {
    let (mut h, _) = harness();
    let tx = channel(&h.gv.loop_handle).unwrap();
    tx.send(Request::Active).unwrap();
    tx.send(Request::Notify(notice(1))).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    h.gv.features.inject_panic(Feature::Notifications);
    h.gv.refresh_notifications();
    assert!(h.gv.notifications.borrow().cards.is_empty());
    tx.send(Request::Notify(notice(2))).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    assert!(h.gv.notifications.borrow().cards.is_empty());
}

#[test]
fn queued_countdown_starts_on_first_show() {
    let mut n = model();
    let (tx, rx) = mpsc::channel();
    n.signals = Some(tx);
    for id in 1..=3 {
        let mut persistent = notice(id);
        persistent.timeout = 0;
        n.receive(persistent, Duration::ZERO, 5000);
    }
    n.receive(notice(4), Duration::ZERO, 5000);
    n.update(Duration::ZERO, false, 3);
    n.update(Duration::from_secs(60), false, 3);
    assert_eq!(visible(&n), [1, 2, 3]);
    assert_eq!(n.cards[3].ttl, Some(Duration::from_secs(5)));
    assert_eq!(n.cards[3].expires, None);
    assert_eq!(n.next_expiry(), None);
    assert!(rx.try_recv().is_err());
    n.close(1, None);
    n.cards[0].spring.jump(0.0);
    n.update(Duration::from_secs(60), false, 3);
    assert_eq!(visible(&n), [2, 3, 4]);
    assert_eq!(n.next_expiry(), Some(Duration::from_secs(65)));
    n.update(Duration::from_secs(64), false, 3);
    assert!(rx.try_recv().is_err());
    n.update(Duration::from_secs(65), false, 3);
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(4, 4, 1));
}

#[test]
fn lock_and_slot_changes_preserve_remaining_countdown() {
    let mut n = model();
    n.receive(notice(1), Duration::ZERO, 5000);
    n.receive(notice(2), Duration::ZERO, 5000);
    n.update(Duration::ZERO, false, 3);
    n.update(Duration::from_secs(2), true, 3);
    assert_eq!(n.cards[0].ttl, Some(Duration::from_secs(3)));
    assert_eq!(n.cards[0].expires, None);
    assert_eq!(n.next_expiry(), None);
    n.update(Duration::from_secs(60), false, 3);
    assert_eq!(n.next_expiry(), Some(Duration::from_secs(63)));
    n.update(Duration::from_secs(61), false, 1);
    assert_eq!(n.cards[1].ttl, Some(Duration::from_secs(2)));
    assert_eq!(n.cards[1].expires, None);
    n.update(Duration::from_secs(62), false, 3);
    assert_eq!(n.cards[1].expires, Some(Duration::from_secs(64)));
    n.update(Duration::from_secs(63), false, 3);
    assert_eq!(visible(&n), [2]);
    n.update(Duration::from_secs(64), false, 3);
    assert!(visible(&n).is_empty());
}

#[test]
fn action_buttons_and_close_use_card_geometry() {
    let (mut h, _) = harness();
    let tx = channel(&h.gv.loop_handle).unwrap();
    let (signals, rx) = mpsc::channel();
    h.gv.notifications.borrow_mut().signals = Some(signals);
    let mut n = notice(1);
    for i in 0..5 {
        n.actions
            .push((format!("action{i}"), format!("Action {i}")));
    }
    tx.send(Request::Active).unwrap();
    tx.send(Request::Notify(n)).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    settle(&mut h);
    let (panel, buttons) = {
        let state = h.gv.notifications.borrow();
        (state.cards[0].rect.unwrap(), state.cards[0].buttons.clone())
    };
    assert_eq!(buttons.len(), 3);
    for (i, (r, _)) in buttons.iter().enumerate() {
        assert!(panel.contains_rect(*r));
        assert!(!r.overlaps(ui::close_rect(panel)));
        for (s, _) in &buttons[i + 1..] {
            assert!(!r.overlaps(*s));
        }
    }
    let r = buttons[1].0;
    assert!(h.gv.notification_button(
        (r.loc.x as f64 + 4.0, r.loc.y as f64 + 4.0).into(),
        0x110,
        true
    ));
    assert_eq!(
        rx.try_recv().unwrap(),
        Signal::Action(1, 1, "action1".into())
    );
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(1, 1, 2));
    tx.send(Request::Notify(notice(2))).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    settle(&mut h);
    h.gv.refresh_notifications();
    let r = ui::close_rect(
        h.gv.notifications
            .borrow()
            .cards
            .iter()
            .find(|c| c.notice.id == 2)
            .unwrap()
            .rect
            .unwrap(),
    );
    assert!(h.gv.notification_button(
        (r.loc.x as f64 + 4.0, r.loc.y as f64 + 4.0).into(),
        0x110,
        true
    ));
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(2, 2, 2));
    assert!(rx.try_recv().is_err());
}

#[test]
fn one_shot_timer_expires_a_visible_card() {
    let (mut h, _) = harness();
    let tx = channel(&h.gv.loop_handle).unwrap();
    let (signals, rx) = mpsc::channel();
    h.gv.notifications.borrow_mut().signals = Some(signals);
    let mut n = notice(1);
    n.timeout = 10;
    tx.send(Request::Active).unwrap();
    tx.send(Request::Notify(n)).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    h.event_loop
        .dispatch(Duration::from_millis(100), &mut h.gv)
        .unwrap();
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(1, 1, 1));
    assert!(h.gv.notifications.borrow().timer.is_none());
}

#[test]
fn pending_cap_keeps_visible_cards_and_prefers_noncritical() {
    let mut n = model();
    let (tx, rx) = mpsc::channel();
    n.signals = Some(tx);
    for id in 1..=40 {
        let mut persistent = notice(id);
        persistent.timeout = 0;
        n.receive(persistent, Duration::ZERO, 5000);
        n.update(Duration::ZERO, false, 3);
    }
    assert_eq!(n.cards.iter().filter(|c| !c.closing).count(), 32);
    assert_eq!(visible(&n), [1, 2, 3]);
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        (4..=11)
            .map(|id| Signal::Closed(id, id as u64, 4))
            .collect::<Vec<_>>()
    );

    let mut n = model();
    let (tx, rx) = mpsc::channel();
    n.signals = Some(tx);
    for id in 1..=32 {
        let mut critical = notice(id);
        critical.urgency = 2;
        n.receive(critical, Duration::ZERO, 5000);
        n.update(Duration::ZERO, false, 3);
    }
    n.receive(notice(33), Duration::ZERO, 5000);
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(4, 4, 4));
    n.receive(notice(34), Duration::ZERO, 5000);
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(33, 33, 4));
    assert_eq!(visible(&n), [1, 2, 3]);
}

#[test]
fn history_actions_require_live_matching_generation_and_dismiss() {
    let mut n = model();
    let (tx, rx) = mpsc::channel();
    n.signals = Some(tx);
    n.receive(notice(1), Duration::ZERO, 5000);
    n.history_action(1);
    assert!(rx.try_recv().is_err());
    n.registry
        .lock()
        .unwrap()
        .live
        .insert(1, (String::new(), 2));
    n.history_action(1);
    assert!(rx.try_recv().is_err());
    assert!(!n.cards[0].closing);
    n.registry
        .lock()
        .unwrap()
        .live
        .insert(1, (String::new(), 1));
    n.history_action(1);
    assert_eq!(
        rx.try_recv().unwrap(),
        Signal::Action(1, 1, "default".into())
    );
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(1, 1, 2));
    assert!(n.cards[0].closing);
    n.registry.lock().unwrap().live.remove(&1);
    n.history_action(1);
    assert!(rx.try_recv().is_err());
}

#[test]
fn replacement_resets_visible_and_queued_ttl_and_uses_latest_generation() {
    let mut n = model();
    let (tx, rx) = mpsc::channel();
    n.signals = Some(tx);
    n.receive(notice(1), Duration::ZERO, 5000);
    n.receive(notice(2), Duration::ZERO, 5000);
    n.update(Duration::ZERO, false, 1);
    let mut replacement = notice(1);
    replacement.gen = 100;
    n.receive(replacement, Duration::from_secs(3), 7000);
    assert_eq!(n.cards[0].expires, Some(Duration::from_secs(10)));
    n.receive(notice(2), Duration::from_secs(3), 8000);
    assert_eq!(n.cards[1].ttl, Some(Duration::from_secs(8)));
    assert_eq!(n.cards[1].expires, None);
    n.close(1, Some(2));
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(1, 100, 2));
}

#[test]
fn reset_disables_server_closes_outstanding_cards_and_retains_active() {
    let (mut h, _) = harness();
    let (tx, rx) = mpsc::channel();
    {
        let mut n = h.gv.notifications.borrow_mut();
        n.active = true;
        n.signals = Some(tx);
        for id in 1..=3 {
            n.receive(notice(id), Duration::ZERO, 5000);
        }
        n.close(1, Some(2));
    }
    assert_eq!(rx.try_recv().unwrap(), Signal::Closed(1, 1, 2));
    h.gv.reset_notifications();
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        [Signal::Closed(2, 2, 4), Signal::Closed(3, 3, 4)]
    );
    let n = h.gv.notifications.borrow();
    assert!(!n.enabled.load(Ordering::Acquire));
    assert!(n.active);
    assert!(n.cards.is_empty());
    assert!(n.history.is_empty());
    drop(n);
    h.gv.refresh_notifications();
    assert!(h.gv.notifications.borrow().enabled.load(Ordering::Acquire));
}

#[test]
fn battery_warns_once_per_threshold_and_only_while_discharging() {
    let b = |percent, charging| crate::topbar::Battery { percent, charging, minutes: None, milliwatts: None, charge_limit: None };
    assert_eq!(battery_warning(Some(&b(40, false)), None), None);
    assert_eq!(battery_warning(Some(&b(15, false)), None), Some((15, 1)));
    assert_eq!(battery_warning(Some(&b(12, false)), Some(15)), None);
    assert_eq!(battery_warning(Some(&b(5, false)), Some(15)), Some((5, 2)));
    assert_eq!(battery_warning(Some(&b(3, false)), Some(5)), None);
    assert_eq!(battery_warning(Some(&b(4, false)), None), Some((5, 2)));
    assert_eq!(battery_warning(Some(&b(4, true)), None), None);
    assert_eq!(battery_warning(None, None), None);
}

#[test]
fn toast_column_moves_beside_or_below_a_popover_or_is_held() {
    use super::ui::column;
    use crate::topbar::geometry::rect;
    // Quick settings: 360 wide at the right edge below the bar.
    for (w, h) in [(1280, 800), (2134, 1200)] {
        let panel = rect(w - 368, 36, 360, 520);
        let (x, y) = column((w, h).into(), 360, Some(panel)).unwrap();
        assert!(!rect(x, y, 360, 96).overlaps(panel), "{w}x{h}: {x},{y}");
        assert_eq!((x, y), (w - 368 - 368, 40), "beside the popover, same height");
        // Calendar in the middle never touches the corner column.
        let calendar = rect((w - 320) / 2, 40, 320, 460);
        assert_eq!(column((w, h).into(), 360, Some(calendar)), Some((w - 368, 40)));
        assert_eq!(column((w, h).into(), 360, None), Some((w - 368, 40)));
    }
    // Too narrow for a side column: below the popover when it fits.
    let panel = rect(332, 36, 360, 400);
    let (x, y) = column((700, 800).into(), 360, Some(panel)).unwrap();
    assert!(y >= 36 + 400 + 8 && !rect(x, y, 360, 96).overlaps(panel));
    // Neither beside nor below: held until the popover closes.
    assert_eq!(column((700, 800).into(), 360, Some(rect(332, 36, 360, 700))), None);
}
#[test]
fn toasts_stay_off_an_open_popover_and_return_when_it_closes() {
    use crate::topbar::geometry::rect;
    let (mut h, _) = harness();
    let tx = channel(&h.gv.loop_handle).unwrap();
    tx.send(Request::Active).unwrap();
    tx.send(Request::Notify(notice(1))).unwrap();
    tx.send(Request::Notify(notice(2))).unwrap();
    h.event_loop.dispatch(Duration::ZERO, &mut h.gv).unwrap();
    settle(&mut h);
    let corner: Vec<_> = h.gv.notifications.borrow().cards.iter().map(|c| c.rect.unwrap()).collect();
    assert!(corner.iter().all(|r| r.loc.x + r.size.w == 1646 - 8));
    let panel = rect(1646 - 368, 36, 360, 560);
    h.gv.notifications.borrow_mut().avoid = Some(panel);
    settle(&mut h);
    let moved: Vec<_> = h.gv.notifications.borrow().cards.iter().map(|c| c.rect.unwrap()).collect();
    assert!(moved.iter().all(|r| !r.overlaps(panel)), "{moved:?} vs {panel:?}");
    // Hit testing follows the moved cards, not the corner.
    let output = h.gv.notifications.borrow().output.clone().unwrap();
    let on_panel = (panel.loc.x as f64 + 100.0, panel.loc.y as f64 + 60.0).into();
    assert!(!h.gv.notifications.borrow().contains(&output, on_panel));
    // A popover with no free column beside or below it holds the toasts.
    h.gv.notifications.borrow_mut().avoid = Some(rect(8, 36, 1630, 860));
    settle(&mut h);
    assert!(h.gv.notifications.borrow().cards.iter().all(|c| c.rect.is_none()));
    h.gv.notifications.borrow_mut().avoid = None;
    settle(&mut h);
    let back: Vec<_> = h.gv.notifications.borrow().cards.iter().map(|c| c.rect.unwrap()).collect();
    assert_eq!(back, corner);
}
