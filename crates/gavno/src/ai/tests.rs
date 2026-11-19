use super::*;
use crate::state::tests::Harness;
use smithay::{
    input::keyboard::{keysyms, Keysym, ModifiersState},
    output::{Output, PhysicalProperties, Subpixel},
};

fn harness(enabled: bool) -> (Harness, mpsc::Receiver<Request>) {
    let mut h = Harness::new();
    h.gv.config.ai.enabled = enabled;
    let (tx, rx) = mpsc::channel();
    h.gv.ai.worker = Some(tx);
    // A non-empty app list keeps the launcher from loading desktop entries.
    h.gv.launcher.test_apps(vec![crate::desktop_entry::DesktopEntry::parse(
        "dummy",
        "[Desktop Entry]\nName=Zzz Dummy\nExec=true\n",
        None,
    )
    .unwrap()]);
    h.gv.toggle_launcher();
    (h, rx)
}

fn type_text(h: &mut Harness, text: &str) {
    for c in text.chars() {
        h.gv.launcher_key(Keysym::from(c as u32), Some(c), &ModifiersState::default());
    }
}

fn enter(h: &mut Harness) {
    h.gv.launcher_key(Keysym::from(keysyms::KEY_Return), None, &ModifiersState::default());
}

fn summary(text: &str) -> Interpretation {
    serde_json::from_value(serde_json::json!({
        "actions": [{"action": "switch_workspace", "workspace": 1}],
        "summary": [text],
    }))
    .unwrap()
}

fn requests(rx: &mpsc::Receiver<Request>) -> Vec<Request> {
    rx.try_iter().collect()
}

#[test]
fn candidates_need_two_words_and_no_strong_match() {
    assert!(is_candidate("move firefox to the second workspace", None));
    assert!(is_candidate("open downloads", Some(2)));
    assert!(!is_candidate("firefox", None));
    assert!(!is_candidate("fire fox", Some(0)));
    assert!(!is_candidate("", None));
}

#[test]
fn disabled_ai_shows_no_row_and_sends_nothing() {
    let (mut h, rx) = harness(false);
    type_text(&mut h, "move firefox to workspace two");
    assert!(!h.gv.launcher.ask_visible());
    enter(&mut h);
    assert!(requests(&rx).is_empty());
}

#[test]
fn typing_prewarms_once_and_enter_asks_but_nothing_runs() {
    let (mut h, rx) = harness(true);
    type_text(&mut h, "pla");
    assert!(requests(&rx).is_empty(), "one word is not a command");
    type_text(&mut h, "y something calm");
    assert!(h.gv.launcher.ask_visible());
    assert!(h.gv.launcher.ask_selected(), "nothing matches, so the AI row is selected");
    let sent = requests(&rx);
    assert!(matches!(sent.as_slice(), [Request::Prewarm { context }] if serde_json::from_str::<Context>(context).is_ok()), "prewarm once, no request yet");
    type_text(&mut h, " now");
    assert!(requests(&rx).is_empty());
    enter(&mut h);
    let sent = requests(&rx);
    assert_eq!(sent.len(), 1);
    let Request::Interpret { seq, query, context } = &sent[0] else { panic!("{sent:?}") };
    assert_eq!(query, "play something calm now");
    assert!(serde_json::from_str::<Context>(context).is_ok());
    assert_eq!(h.gv.launcher.ai_phase, Phase::Asking(*seq));
    assert!(h.gv.ai.executed.is_empty());
    // A second Enter while waiting neither asks again nor runs anything.
    enter(&mut h);
    assert!(requests(&rx).is_empty() && h.gv.ai.executed.is_empty());
}

#[test]
fn preview_runs_only_on_the_second_enter() {
    let (mut h, rx) = harness(true);
    type_text(&mut h, "go to workspace one");
    enter(&mut h);
    let Request::Interpret { seq, .. } = &requests(&rx)[1] else { panic!() };
    h.gv.ai_reply(Reply::Interpreted { seq: *seq, result: Ok(summary("Go to workspace 1")) });
    assert!(matches!(h.gv.launcher.ai_phase, Phase::Preview(_)));
    assert!(h.gv.ai.executed.is_empty(), "a preview executes nothing");
    assert!(h.gv.launcher.open);
    enter(&mut h);
    assert_eq!(h.gv.ai.executed, [Action::SwitchWorkspace { workspace: 1 }]);
    assert!(!h.gv.launcher.open);
}

#[test]
fn escape_and_editing_drop_a_preview_without_running_it() {
    let (mut h, rx) = harness(true);
    type_text(&mut h, "go to workspace one");
    enter(&mut h);
    let Request::Interpret { seq, .. } = &requests(&rx)[1] else { panic!() };
    h.gv.ai_reply(Reply::Interpreted { seq: *seq, result: Ok(summary("Go to workspace 1")) });
    type_text(&mut h, "x");
    assert_eq!(h.gv.launcher.ai_phase, Phase::Idle);
    enter(&mut h);
    let Request::Interpret { seq: second, .. } = &requests(&rx)[0] else { panic!() };
    h.gv.ai_reply(Reply::Interpreted { seq: *second, result: Ok(summary("Go to workspace 1")) });
    h.gv.launcher_key(Keysym::from(keysyms::KEY_Escape), None, &ModifiersState::default());
    assert!(!h.gv.launcher.open);
    assert!(h.gv.ai.executed.is_empty());
    // A late answer for a closed launcher is ignored.
    h.gv.ai_reply(Reply::Interpreted { seq: *second, result: Ok(summary("late")) });
    assert_eq!(h.gv.launcher.ai_phase, Phase::Idle);
}

#[test]
fn stale_replies_are_ignored() {
    let (mut h, rx) = harness(true);
    type_text(&mut h, "go to workspace one");
    enter(&mut h);
    let Request::Interpret { seq, .. } = &requests(&rx)[1] else { panic!() };
    let stale = *seq - 1;
    h.gv.ai_reply(Reply::Interpreted { seq: stale, result: Ok(summary("old")) });
    assert_eq!(h.gv.launcher.ai_phase, Phase::Asking(*seq));
}

#[test]
fn errors_show_as_a_row_and_enter_retries() {
    let (mut h, rx) = harness(true);
    type_text(&mut h, "do something strange");
    enter(&mut h);
    let Request::Interpret { seq, .. } = &requests(&rx)[1] else { panic!() };
    h.gv.ai_reply(Reply::Interpreted { seq: *seq, result: Err("I could not turn that into an action".into()) });
    assert_eq!(h.gv.launcher.ai_phase, Phase::Error("I could not turn that into an action".into()));
    let (title, detail, error) = h.gv.launcher.ask_texts();
    assert!(error && title.contains("could not") && detail.contains("again"));
    assert!(h.gv.ai.executed.is_empty());
    enter(&mut h);
    assert!(matches!(requests(&rx)[0], Request::Interpret { .. }));
    assert!(matches!(h.gv.launcher.ai_phase, Phase::Asking(_)));
}

#[test]
fn a_strong_app_match_keeps_the_app_selected_and_enter_never_asks() {
    let (mut h, rx) = harness(true);
    h.gv.launcher.test_apps(vec![crate::desktop_entry::DesktopEntry::parse(
        "code",
        "[Desktop Entry]\nName=Visual Studio Code\nExec=true\n",
        None,
    )
    .unwrap()]);
    type_text(&mut h, "visual studio");
    assert!(!h.gv.launcher.ask_visible(), "a name prefix is a strong match");
    assert!(!h.gv.launcher.ask_selected());
    assert!(requests(&rx).is_empty());
    // A multi-word query that only matches weakly starts on the AI row but keeps the app reachable.
    h.gv.launcher.test_query("studio code now");
    assert!(h.gv.launcher.ask_visible() && h.gv.launcher.ask_selected());
    h.gv.launcher.test_query("studio code");
    assert!(h.gv.launcher.ask_visible(), "a word match inside the name is weak");
    assert_eq!(h.gv.launcher.test_results(), 1, "the weak match stays listed above the AI row");
}

#[test]
fn the_helper_being_unavailable_is_an_error_row() {
    let (mut h, rx) = harness(true);
    drop(rx);
    type_text(&mut h, "go to workspace one");
    enter(&mut h);
    assert!(matches!(h.gv.launcher.ai_phase, Phase::Error(_)));
}

#[test]
fn naming_a_workspace_marks_it_user_named() {
    let (mut h, _rx) = harness(true);
    let output = Output::new(
        "ai".into(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "t".into(), model: "t".into() },
    );
    output.change_current_state(
        Some(smithay::output::Mode { size: (1600, 900).into(), refresh: 60000 }),
        None,
        None,
        Some((0, 0).into()),
    );
    h.gv.space.map_output(&output, (0, 0));
    h.gv.workspaces.ensure(&output);
    h.gv.ai_execute(vec![Action::NameWorkspace { workspace: 1, name: "Mail".into() }]);
    let ws = &h.gv.workspaces.get(&output).unwrap().list.items[0];
    assert_eq!(ws.name.as_deref(), Some("Mail"));
    assert!(ws.user_named);
    // Heuristics and AI refinement leave it alone.
    h.gv.refresh_workspace_names();
    h.gv.ai_named("ai", ws_id(&h, &output), 0, "Other".into());
    assert_eq!(h.gv.workspaces.get(&output).unwrap().list.items[0].name.as_deref(), Some("Mail"));
    assert_eq!(h.gv.active_workspace_name().as_deref(), Some("Mail"));
}

fn ws_id(h: &Harness, output: &Output) -> u64 {
    h.gv.workspaces.get(output).unwrap().list.items[0].id
}

#[test]
fn unknown_window_ids_do_nothing() {
    let (mut h, _rx) = harness(true);
    h.gv.ai_execute(vec![Action::Focus { window: 9999 }, Action::MoveToWorkspace { window: 9999, workspace: 2 }]);
    assert!(h.gv.focused_window().is_none());
}

#[test]
fn the_model_cannot_name_over_a_changed_window_set() {
    let (mut h, _rx) = harness(true);
    let output = Output::new(
        "ai2".into(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "t".into(), model: "t".into() },
    );
    output.change_current_state(Some(smithay::output::Mode { size: (1600, 900).into(), refresh: 60000 }), None, None, Some((0, 0).into()));
    h.gv.space.map_output(&output, (0, 0));
    h.gv.workspaces.ensure(&output);
    // An empty workspace has the signature of an empty window set.
    let sig = names::signature(&[]);
    h.gv.ai.stable.insert(("ai2".into(), ws_id(&h, &output)), (sig, Instant::now()));
    h.gv.ai_named("ai2", ws_id(&h, &output), sig + 1, "Stale".into());
    assert_eq!(h.gv.workspaces.get(&output).unwrap().list.items[0].name, None);
    h.gv.ai_named("ai2", ws_id(&h, &output), sig, "Fresh".into());
    assert_eq!(h.gv.workspaces.get(&output).unwrap().list.items[0].name.as_deref(), Some("Fresh"));
}
