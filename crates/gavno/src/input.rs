use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{keysyms, FilterResult, KeyboardTarget, Keysym, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{shell::wlr_layer::Layer,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        pointer_constraints::PointerConstraintsHandler},
};

use crate::{
    grabs::{resize_grab::ResizeEdge, MoveSurfaceGrab, ResizeSurfaceGrab},
    module::Feature,
    snap::Direction,
    window_mode::{mode_state, WindowMode},
    Gavno,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// A compositor action bound to a key chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Classic(crate::classic::binds::ClassicAction),
    Lock,
    Quit,
    Spawn(String),
    CloseWindow,
    Minimize,
    /// Switch to virtual terminal N (Ctrl+Alt+F<N>), TTY backend only.
    SwitchVt(i32),
    ToggleHdr,
    SdrWhite(i32),
    ToggleMaximize,
    ToggleFullscreen,
    ToggleTiling,
    ToggleFloating,
    RotateSplit,
    Snap(Direction),
    FocusDirection(Direction),
    Swap(Direction),
    Resize(Direction),
    ResizeMode,
    SwitcherCycle(bool),
    SwitcherFinish(bool),
    Workspace(usize),
    MoveWorkspace(usize),
    WorkspaceStep(bool),
    ToggleOverview,
    CloseOverview,
    ToggleLauncher,
    /// Built-in screenshot tool (Print, Shift+Print, Mod+Shift+S).
    Screenshot(crate::screenshot::ScreenshotMode),
    /// Clipboard history picker (Mod+V).
    ClipboardHistory,
    /// Keyboard-only shell mode; applies to the GAVNO shell only.
    ToggleCodeMode,
    WindowHints,
    CheatSheet,
    PanelControls(crate::panel_controls::Page),
    /// Webcam finger pointer on/off (Mod+Shift+M).
    ToggleFingerPointer,
}

impl Gavno {
    pub(crate) fn cancel_shell_tap(&mut self) {
        if self.classic_mode() { self.guard(Feature::Classic, (), |gv| gv.gestures.super_tap.cancel()); }
        else { self.guard(Feature::Gestures, (), |gv| gv.gestures.super_tap.cancel()); }
    }
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>)
    where
        I::GestureSwipeUpdateEvent: 'static,
    {
        // Any pointer activity brings a hidden cursor back, without touching
        // pointer focus.
        if matches!(&event,
            InputEvent::PointerMotion { .. } | InputEvent::PointerMotionAbsolute { .. }
            | InputEvent::PointerButton { .. } | InputEvent::PointerAxis { .. }
            | InputEvent::GestureSwipeBegin { .. } | InputEvent::GestureSwipeUpdate { .. }
            | InputEvent::GestureSwipeEnd { .. } | InputEvent::GesturePinchBegin { .. }
            | InputEvent::GesturePinchUpdate { .. } | InputEvent::GesturePinchEnd { .. }
            | InputEvent::GestureHoldBegin { .. } | InputEvent::GestureHoldEnd { .. })
        {
            self.show_pointer();
        }
        if !matches!(&event, InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }) {
            static FIRST_INPUT: std::sync::Once = std::sync::Once::new();
            FIRST_INPUT.call_once(|| crate::perf::startup("input.first_event"));
            self.protocols.idle_policy.activity();
            let waking = self.power.is_off();
            self.power_on_monitors("input");
            match &event {
                InputEvent::Keyboard { event, .. } => {
                    if self.power.consume_key(event.key_code().raw(), event.state() == KeyState::Pressed, waking) { return; }
                }
                InputEvent::PointerButton { event, .. } => {
                    if self.power.consume_button(event.button_code(), event.state() == ButtonState::Pressed, waking) { return; }
                }
                _ => {}
            }
        }
        if self.lock.internal() && !matches!(&event, InputEvent::Keyboard { .. } | InputEvent::PointerMotion { .. }
            | InputEvent::PointerMotionAbsolute { .. } | InputEvent::PointerButton { .. }) { return; }
        // A frozen screenshot ignores touchpad gestures.
        if self.screenshot_active() && matches!(&event,
            InputEvent::GestureSwipeBegin { .. } | InputEvent::GestureSwipeUpdate { .. }
            | InputEvent::GestureSwipeEnd { .. } | InputEvent::GesturePinchBegin { .. }
            | InputEvent::GesturePinchUpdate { .. } | InputEvent::GesturePinchEnd { .. }
            | InputEvent::GestureHoldBegin { .. } | InputEvent::GestureHoldEnd { .. }) { return; }
        let input_started = std::time::Instant::now();
        let keyboard = self.seat.get_keyboard();
        let focus_before = keyboard.as_ref().and_then(|keyboard| keyboard.current_focus());
        let enter_before = keyboard.as_ref().and_then(|keyboard| keyboard.last_enter());
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotion { event, .. } => self.on_pointer_motion::<I>(event),
            InputEvent::PointerMotionAbsolute { event, .. } => self.on_pointer_motion_absolute::<I>(event),
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<I>(event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<I>(event),
            InputEvent::GestureSwipeBegin { event, .. } => self.on_gesture_swipe_begin::<I>(event),
            InputEvent::GestureSwipeUpdate { event, .. } => self.on_gesture_swipe_update::<I>(event),
            InputEvent::GestureSwipeEnd { event, .. } => self.on_gesture_swipe_end::<I>(event),
            InputEvent::GesturePinchBegin { event, .. } => self.on_gesture_pinch_begin::<I>(event),
            InputEvent::GesturePinchUpdate { event, .. } => self.on_gesture_pinch_update::<I>(event),
            InputEvent::GesturePinchEnd { event, .. } => self.on_gesture_pinch_end::<I>(event),
            InputEvent::GestureHoldBegin { event, .. } => self.on_gesture_hold_begin::<I>(event),
            InputEvent::GestureHoldEnd { event, .. } => self.on_gesture_hold_end::<I>(event),
            _ => {}
        }
        if keyboard.as_ref().and_then(|keyboard| keyboard.current_focus()) != focus_before
            || keyboard.as_ref().and_then(|keyboard| keyboard.last_enter()) != enter_before
        {
            self.launch_user_activity(input_started);
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let input_started = std::time::Instant::now();
        let mut modifier_only = false;
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let keycode = event.key_code();
        let key_state = event.state();
        let raw_code: u32 = keycode.raw();

        let keyboard = self.seat.get_keyboard().unwrap();
        let modifiers_before = keyboard.modifier_state();
        let action = keyboard.input::<Option<Action>, _>(
            self,
            keycode,
            key_state,
            serial,
            time,
            |gv, mods, handle| {
                modifier_only = handle.modified_sym().is_modifier_key();
                if gv.greeter.is_none() && !gv.protocols.locks.locked
                    && (gv.features.on(crate::module::Feature::Switcher) && gv.overlay.switcher_open())
                    && key_state == KeyState::Released
                    && (if gv.classic.switcher_win { !gv.config.mod_key.is_held(mods) } else { !mods.alt })
                {
                    gv.switcher_finish(true);
                    gv.classic.switcher_win = false;
                }
                let sym = handle.modified_sym().raw();
                let emergency = (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&sym)
                    || (mods.ctrl && mods.alt && (sym == keysyms::KEY_Delete
                        || handle.raw_latin_sym_or_raw_current_sym().is_some_and(|s| matches!(s.raw(), keysyms::KEY_l | keysyms::KEY_L))));
                // Release bookkeeping runs before every modal return. A modal may
                // have opened after the press started a repeat or a Mod tap.
                if key_state == KeyState::Released {
                    let bar_consumed = gv.bar_key(sym, raw_code, false);
                    gv.launcher_key_release(raw_code);
                    let tap_enabled = gv.greeter.is_none() && !gv.protocols.locks.locked
                        && !gv.seat.keyboard_shortcuts_inhibited()
                        && (gv.classic_mode() || (gv.features.on(Feature::Gestures)
                            && (gv.features.on(Feature::Overview) || gv.features.on(Feature::Launcher))));
                    let is_main = match gv.config.mod_key {
                        crate::config::ModKey::Super => matches!(sym, keysyms::KEY_Super_L | keysyms::KEY_Super_R),
                        crate::config::ModKey::Alt => matches!(sym, keysyms::KEY_Alt_L | keysyms::KEY_Alt_R),
                    };
                    if !tap_enabled || bar_consumed || gv.screenshot_active() || gv.clipboard_picker_open() || gv.overlay.keyboard.open() || gv.overview.open() || gv.music_open() || gv.panel_controls_open() {
                        gv.gestures.super_tap.cancel();
                    }
                    // Always clear the physical key, even if its feature or shell
                    // changed while held. Cancellation above prevents a modal tap.
                    let toggle = gv.gestures.super_tap.key(raw_code, is_main, false, false);
                    let captured = gv.suppressed_keys.contains(&raw_code);
                    gv.suppressed_keys.retain(|k| *k != raw_code);
                    if toggle {
                        let action = if gv.classic_mode() { Action::Classic(crate::classic::binds::ClassicAction::Start) }
                            else if gv.features.on(Feature::Launcher) { gv.config.launcher.clone().map(Action::Spawn).unwrap_or(Action::ToggleLauncher) }
                            else { Action::ToggleOverview };
                        return FilterResult::Intercept(Some(action));
                    }
                    return if bar_consumed || captured || (tap_enabled && is_main) || gv.lock.internal() || gv.greeter.is_some() {
                        FilterResult::Intercept(None)
                    } else { FilterResult::Forward };
                }
                if gv.panel_controls_open() && !gv.polkit_visible() && !gv.protocols.locks.locked
                    && !gv.lock.internal() && gv.greeter.is_none() && !emergency {
                    if let Some(action @ Action::PanelControls(_)) = handle.raw_latin_sym_or_raw_current_sym().and_then(|s| gv.bind_for(s,mods)) {
                        gv.suppressed_keys.push(raw_code);
                        return FilterResult::Intercept(Some(action));
                    }
                    gv.suppressed_keys.push(raw_code);
                    let sym = handle.modified_sym();
                    gv.panel_controls_key(sym, sym.key_char(), mods, raw_code);
                    return FilterResult::Intercept(None);
                }
                // Modal music input precedes shell shortcuts and modifier-tap binds.
                if gv.music_open() && !gv.polkit_visible() && !gv.protocols.locks.locked
                    && gv.greeter.is_none() && !(mods.ctrl && mods.alt)
                    && !(keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&sym) {
                    gv.suppressed_keys.push(raw_code);
                    let sym = handle.modified_sym();
                    gv.music_key(sym, sym.key_char(), mods);
                    return FilterResult::Intercept(None);
                }
                if key_state == KeyState::Pressed && !gv.protocols.locks.locked && gv.greeter.is_none()
                    && !gv.lock.internal() && !gv.polkit_visible() && !gv.overlay.keyboard.open() && !gv.seat.keyboard_shortcuts_inhibited()
                    && handle.raw_latin_sym_or_raw_current_sym().is_some_and(|s| gv.bind_for(s, mods) == Some(Action::CheatSheet))
                {
                    gv.suppressed_keys.push(raw_code);
                    gv.cancel_shell_tap();
                    return FilterResult::Intercept(Some(Action::CheatSheet));
                }
                if key_state == KeyState::Pressed && gv.overview.open() && !gv.overlay.keyboard.open()
                    && !gv.protocols.locks.locked && !gv.lock.internal() && gv.greeter.is_none() && !gv.polkit_visible()
                    && !gv.seat.keyboard_shortcuts_inhibited()
                    && handle.raw_latin_sym_or_raw_current_sym().is_some_and(|s| gv.bind_for(s, mods) == Some(Action::ToggleOverview))
                {
                    gv.suppressed_keys.push(raw_code);
                    gv.cancel_shell_tap();
                    return FilterResult::Intercept(Some(Action::ToggleOverview));
                }
                if gv.greeter.is_none() && !gv.protocols.locks.locked && !gv.lock.internal() && !gv.polkit_visible()
                    && !gv.seat.keyboard_shortcuts_inhibited()
                    && !emergency && (gv.overlay.keyboard.open() || gv.overview.open())
                {
                    let latin = handle.raw_latin_sym_or_raw_current_sym().map(|s| s.raw()).unwrap_or(sym);
                    if !modifier_only {
                        if gv.code_mode { gv.hide_pointer(); }
                        if gv.overlay.keyboard.open() { gv.keyboard_overlay_key(latin); }
                        else { gv.overview_key(latin, mods.shift); }
                    }
                    if !gv.suppressed_keys.contains(&raw_code) { gv.suppressed_keys.push(raw_code); }
                    gv.cancel_shell_tap();
                    return FilterResult::Intercept(None);
                }
                if gv.bar_key(sym, raw_code, key_state == KeyState::Pressed) {
                    if key_state == KeyState::Pressed && !gv.suppressed_keys.contains(&raw_code) { gv.suppressed_keys.push(raw_code); }
                    if key_state == KeyState::Released { gv.suppressed_keys.retain(|k| *k != raw_code); }
                    return FilterResult::Intercept(None);
                }
                if gv.greeter.is_none() && !gv.protocols.locks.locked && !gv.seat.keyboard_shortcuts_inhibited() && (gv.classic_mode() || (gv.features.on(Feature::Gestures) && (gv.features.on(Feature::Overview) || gv.features.on(Feature::Launcher)))) {
                    let is_super = match gv.config.mod_key {
                        crate::config::ModKey::Super => matches!(sym, keysyms::KEY_Super_L | keysyms::KEY_Super_R),
                        crate::config::ModKey::Alt => matches!(sym, keysyms::KEY_Alt_L | keysyms::KEY_Alt_R),
                    };
                    let other_down = gv.desktop_keys.iter().any(|k| *k != raw_code);
                    let toggle = if gv.classic_mode() {
                        gv.guard(Feature::Classic, false, |gv| gv.gestures.super_tap.key(raw_code, is_super, key_state == KeyState::Pressed, other_down))
                    } else { gv.super_tap_key(raw_code, is_super, key_state == KeyState::Pressed, other_down) };
                    if toggle {
                        let action = if gv.classic_mode() { Action::Classic(crate::classic::binds::ClassicAction::Start) } else if gv.features.on(Feature::Launcher) { gv.config.launcher.clone().map(Action::Spawn).unwrap_or(Action::ToggleLauncher) } else { Action::ToggleOverview };
                        return FilterResult::Intercept(Some(action));
                    }
                    if is_super { return FilterResult::Intercept(None); }
                } else { gv.cancel_shell_tap(); }

                // VT switch keysyms come from the XKB keymap (Ctrl+Alt+F<N>).
                let modified = handle.modified_sym().raw();
                if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&modified) {
                    gv.suppressed_keys.push(raw_code);
                    let vt = (modified - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                    return FilterResult::Intercept(Some(Action::SwitchVt(vt)));
                }

                if gv.lock.internal() {
                    let sym = handle.modified_sym();
                    gv.suppressed_keys.push(raw_code);
                    gv.lock.key(sym, sym.key_char(), mods);
                    return FilterResult::Intercept(None);
                }

                // Greeter mode: every key goes to the login screen.
                if gv.greeter.is_some() {
                    let sym = handle.modified_sym();
                    let mods = *mods;
                    gv.suppressed_keys.push(raw_code);
                    if let Some(g) = gv.greeter.as_mut() {
                        g.safe_call((), |g| g.key(sym, sym.key_char(), &mods));
                    }
                    return FilterResult::Intercept(None);
                }

                // The polkit dialog takes every key except the emergency chord.
                if gv.polkit_visible() && !(mods.ctrl && mods.alt) {
                    let sym = handle.modified_sym();
                    gv.suppressed_keys.push(raw_code);
                    gv.polkit_key(sym, sym.key_char(), mods);
                    return FilterResult::Intercept(None);
                }

                // The screenshot overlay and the clipboard picker take every key
                // except the emergency chord.
                if (gv.screenshot_active() || gv.clipboard_picker_open()) && !gv.protocols.locks.locked && !(mods.ctrl && mods.alt) {
                    let sym = handle.modified_sym();
                    let mods = *mods;
                    gv.suppressed_keys.push(raw_code);
                    if gv.screenshot_active() { gv.screenshot_key(sym, &mods); }
                    else { gv.clipboard_key(sym, sym.key_char(), &mods); }
                    return FilterResult::Intercept(None);
                }

                if !gv.protocols.locks.locked && !gv.seat.keyboard_shortcuts_inhibited() {
                    if let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() {
                        let action = gv.features.read(Feature::Classic, None, || crate::classic::binds::binding(sym, mods, gv.config.mod_key, gv.classic_mode()));
                        if let Some(action) = action.filter(|action| gv.action_enabled(action)) {
                            gv.suppressed_keys.push(raw_code);
                            return FilterResult::Intercept(Some(action));
                        }
                    }
                }
                if gv.classic_mode() && gv.classic.start.open && !gv.protocols.locks.locked && !(mods.ctrl && mods.alt) {
                    let sym = handle.modified_sym();
                    gv.suppressed_keys.push(raw_code);
                    gv.classic_start_key(sym, sym.key_char(), mods);
                    return FilterResult::Intercept(None);
                }
                // The open launcher takes every key except the emergency chord.
                if gv.launcher.open && !gv.protocols.locks.locked && !(mods.ctrl && mods.alt) {
                    let sym = handle.modified_sym();
                    let mods = *mods;
                    gv.suppressed_keys.push(raw_code);
                    gv.launcher_key_press(raw_code, sym, sym.key_char(), &mods);
                    return FilterResult::Intercept(None);
                }

                if !gv.protocols.locks.locked && !gv.seat.keyboard_shortcuts_inhibited() {
                    let sym = handle.modified_sym();
                    if gv.classic_desktop_key(sym, sym.key_char(), mods) {
                        gv.suppressed_keys.push(raw_code);
                        return FilterResult::Intercept(None);
                    }
                }
                // The emergency chord and VT switching remain compositor-owned.
                if !gv.protocols.locks.locked && mods.ctrl && mods.alt && handle.modified_sym().raw() == keysyms::KEY_Delete {
                    gv.suppressed_keys.push(raw_code);
                    return FilterResult::Intercept(Some(Action::Quit));
                }
                if !gv.protocols.locks.locked {
                    if let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() {
                        if gv.bind_for(sym, mods) == Some(Action::Lock) {
                            gv.suppressed_keys.push(raw_code);
                            return FilterResult::Intercept(Some(Action::Lock));
                        }
                    }
                }
                if gv.protocols.locks.locked || gv.seat.keyboard_shortcuts_inhibited() {
                    return FilterResult::Forward;
                }

                if gv.resize_mode_key_with_ctrl(handle.raw_latin_sym_or_raw_current_sym().map(|s| s.raw()).unwrap_or(sym), mods.shift, mods.ctrl) {
                    if gv.code_mode { gv.hide_pointer(); }
                    gv.suppressed_keys.push(raw_code);
                    return FilterResult::Intercept(None);
                }
                if gv.features.on(crate::module::Feature::Switcher) && gv.overlay.switcher_open() {
                    let sym = handle.modified_sym().raw();
                    let action = match sym {
                        keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => Some(Action::SwitcherCycle(mods.shift)),
                        keysyms::KEY_Escape => Some(Action::SwitcherFinish(false)),
                        _ => None,
                    };
                    if !gv.suppressed_keys.contains(&raw_code) {
                        gv.suppressed_keys.push(raw_code);
                    }
                    return FilterResult::Intercept(action);
                }

                // Binds match the latin layout, so they work with ru active too.
                let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() else {
                    return FilterResult::Forward;
                };
                let mods = *mods;

                match gv.bind_for(sym, &mods) {
                    Some(action) => {
                        gv.suppressed_keys.push(raw_code);
                        FilterResult::Intercept(Some(action))
                    }
                    None if (gv.features.on(crate::module::Feature::Overview) && gv.overview.visible()) => {
                        gv.suppressed_keys.push(raw_code);
                        FilterResult::Intercept(None)
                    }
                    None => FilterResult::Forward,
                }
            },
        );

        // Smithay suppresses modifier notifications together with intercepted
        // keys. Focus entry may already have sent a held modifier to a new client;
        // publish its cleared state without forwarding an unmatched key release.
        let modifiers = keyboard.modifier_state();
        if key_state == KeyState::Released && action.is_some() && modifiers != modifiers_before {
            if let Some(focus) = keyboard.current_focus() {
                focus.modifiers(&self.seat.clone(), self, modifiers, serial);
            }
        }

        match key_state {
            KeyState::Pressed => { if !self.desktop_keys.contains(&raw_code) { self.desktop_keys.push(raw_code); } }
            KeyState::Released => self.desktop_keys.retain(|k| *k != raw_code),
        }
        // The first key press that reaches a client or a bind hides the
        // pointer. Shell text entry, such as the launcher, leaves it alone.
        if key_state == KeyState::Pressed && self.code_mode
            && !self.protocols.locks.locked && self.greeter.is_none()
        {
            let reaches = match &action {
                Some(Some(_)) => true,
                Some(None) => false,
                None => keyboard.current_focus().is_some(),
            };
            if reaches { self.hide_pointer(); }
        }
        if action.is_none() {
            let focus = keyboard.current_focus();
            if key_state == KeyState::Pressed && !modifier_only && focus.is_some() {
                // Only client-directed presses update user time. Shell actions
                // (including the Enter that launched an app) are intercepted.
                self.launch_user_activity(input_started);
            }
            self.record_activation_serial(focus, serial);
        }
        if let Some(Some(action)) = action {
            self.run_action(action);
        }
        if self.greeter.is_some() || self.lock.internal() {
            let caps = keyboard.modifier_state().caps_lock;
            let layout = keyboard.with_xkb_state(self, |ctx| {
                let xkb = ctx.xkb().lock().unwrap();
                xkb.layout_name(xkb.active_layout()).to_owned()
            });
            if let Some(g) = self.greeter.as_mut() {
                g.safe_call((), |g| g.set_modifiers(caps, &layout));
            }
            if self.lock.internal() { self.lock.modifiers(caps, &layout); }
            self.queue_redraw_all();
        }
    }

    /// Pointer hover/click on the login screen, in output-local coordinates.
    fn greeter_pointer(&mut self, click: bool) {
        let pos = self.pointer_location();
        let Some(origin) = self.active_output().and_then(|o| self.space.output_geometry(&o)).map(|g| g.loc) else {
            return;
        };
        let local = pos - origin.to_f64();
        if let Some(g) = self.greeter.as_mut() {
            if click {
                g.safe_call((), |g| g.click(local));
            } else {
                g.safe_call(false, |g| g.pointer_motion(local));
            }
        }
        self.queue_redraw_all();
    }

    /// The core binds (KooL Hyprland-Dots subset). Modules see keys first.
    pub(crate) fn bind_for(&self, sym: Keysym, mods: &ModifiersState) -> Option<Action> {
        self.features.read(Feature::Classic, None, || crate::classic::binds::binding(sym, mods, self.config.mod_key, self.classic_mode()))
            .or_else(|| self.bind_for_enabled(sym, mods)).filter(|action| self.action_enabled(action))
    }
    pub(crate) fn action_enabled(&self, action: &Action) -> bool {
        let feature = match action {
            Action::ToggleCodeMode if self.classic_mode() => return false,
            Action::Classic(_) => Feature::Classic,
            Action::PanelControls(_) => Feature::Bar,
            Action::ToggleFingerPointer => return self.config.pointer.camera,
            Action::Minimize => Feature::Dock,
            Action::ToggleTiling | Action::ToggleFloating | Action::RotateSplit | Action::Swap(_) => Feature::Tiling,
            Action::Snap(_) | Action::Resize(_) => Feature::Snap,
            Action::ResizeMode => Feature::ResizeMode,
            Action::SwitcherCycle(_) | Action::SwitcherFinish(_) => Feature::Switcher,
            Action::ToggleOverview | Action::CloseOverview => Feature::Overview,
            Action::ToggleLauncher => Feature::Launcher,
            Action::ClipboardHistory => return self.config.clipboard.history,
            _ => return true,
        };
        self.features.on(feature)
    }
    fn bind_for_enabled(&self, sym: Keysym, mods: &ModifiersState) -> Option<Action> {
        let main = self.config.mod_key.is_held(mods);
        let sym = Keysym::from(sym.raw()).raw();
        if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&sym) {
            return Some(Action::SwitchVt((sym - keysyms::KEY_XF86Switch_VT_1 + 1) as i32));
        }

        // Ctrl+Alt+L remains the emergency lock chord in code mode too.
        if matches!(sym, keysyms::KEY_l | keysyms::KEY_L) && !mods.shift && mods.ctrl && mods.alt {
            return Some(Action::Lock);
        }
        if mods.ctrl && mods.alt && sym == keysyms::KEY_Delete {
            return Some(Action::Quit);
        }
        // Screenshots (KooL: Print = full output, Shift+Print = area).
        let area = (sym == keysyms::KEY_Print && mods.shift && !mods.ctrl)
            || (main && mods.shift && !mods.ctrl && matches!(sym, keysyms::KEY_s | keysyms::KEY_S));
        if area || (sym == keysyms::KEY_Print && !mods.shift && !mods.ctrl && !mods.alt) {
            use crate::screenshot::ScreenshotMode;
            return Some(Action::Screenshot(if area { ScreenshotMode::Area } else { ScreenshotMode::Full }));
        }
        if !main && mods.alt && !mods.ctrl && !mods.logo
            && matches!(sym, keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab)
        {
            return Some(Action::SwitcherCycle(mods.shift));
        }
        if (self.features.on(crate::module::Feature::Overview) && self.overview.visible()) && sym == keysyms::KEY_Escape { return Some(Action::CloseOverview); }
        if mods.logo && mods.shift && sym == keysyms::KEY_F23 { return Some(Action::ToggleOverview); }
        if !main {
            return None;
        }
        if mods.shift && !mods.ctrl {
            let page = match sym {
                keysyms::KEY_n | keysyms::KEY_N => Some(crate::panel_controls::Page::Wifi),
                keysyms::KEY_b | keysyms::KEY_B => Some(crate::panel_controls::Page::Bluetooth),
                keysyms::KEY_v | keysyms::KEY_V => Some(crate::panel_controls::Page::Audio),
                _ => None,
            };
            if let Some(page) = page { return Some(Action::PanelControls(page)); }
        }
        if mods.shift && !mods.ctrl && matches!(sym, keysyms::KEY_m | keysyms::KEY_M) { return Some(Action::ToggleFingerPointer); }
        if !mods.ctrl && !mods.shift && matches!(sym, keysyms::KEY_slash | keysyms::KEY_F1) { return Some(Action::CheatSheet); }
        if !mods.ctrl && !mods.shift && matches!(sym, keysyms::KEY_g | keysyms::KEY_G) { return Some(Action::WindowHints); }
        if !self.classic_mode() && mods.shift && !mods.ctrl && matches!(sym, keysyms::KEY_c | keysyms::KEY_C) {
            return Some(Action::ToggleCodeMode);
        }
        // In code mode H/J/K/L stand in for the arrows in every Mod+arrow
        // chord. The alias wins over Mod+L lock and Mod+Shift+H HDR, but never
        // over Ctrl+Alt+L above.
        if self.code_mode {
            if let Some(dir) = vim_arrow(sym).and_then(arrow_direction) {
                return Some(self.direction_action(dir, mods));
            }
        }
        if !self.code_mode && matches!(sym, keysyms::KEY_l | keysyms::KEY_L) && !mods.shift && !mods.ctrl {
            return Some(Action::Lock);
        }
        if (keysyms::KEY_1..=keysyms::KEY_9).contains(&sym) {
            let index = (sym - keysyms::KEY_1) as usize;
            return Some(if mods.shift { Action::MoveWorkspace(index) } else { Action::Workspace(index) });
        }
        if matches!(sym, keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab) { return Some(Action::WorkspaceStep(mods.shift)); }
        if mods.ctrl && matches!(sym, keysyms::KEY_Page_Up | keysyms::KEY_Page_Down) { return Some(Action::WorkspaceStep(sym == keysyms::KEY_Page_Up)); }
        if matches!(sym, keysyms::KEY_a | keysyms::KEY_A) { return Some(Action::ToggleOverview); }
        if let Some(dir) = arrow_direction(sym) {
            return Some(self.direction_action(dir, mods));
        }
        match sym {
            keysyms::KEY_r | keysyms::KEY_R if !mods.ctrl && !mods.shift => Some(Action::ResizeMode),
            keysyms::KEY_t | keysyms::KEY_T if !mods.ctrl && !mods.shift => Some(Action::ToggleTiling),
            keysyms::KEY_space if !mods.ctrl && !mods.shift => Some(if (mods.alt && self.config.mod_key == crate::config::ModKey::Super) || (mods.logo && self.config.mod_key == crate::config::ModKey::Alt) { Action::ToggleTiling } else { Action::ToggleFloating }),
            keysyms::KEY_i | keysyms::KEY_I if mods.shift && !mods.ctrl => Some(Action::RotateSplit),
            keysyms::KEY_h | keysyms::KEY_H if mods.shift && !mods.ctrl => Some(Action::ToggleHdr),
            keysyms::KEY_F2 if mods.shift && !mods.ctrl => Some(Action::SdrWhite(-10)),
            keysyms::KEY_F3 if mods.shift && !mods.ctrl => Some(Action::SdrWhite(10)),
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => Some(Action::Spawn(self.config.terminal.clone())),
            keysyms::KEY_v | keysyms::KEY_V if !mods.shift && !mods.ctrl => Some(Action::ClipboardHistory),
            keysyms::KEY_q if !mods.shift => Some(Action::CloseWindow),
            keysyms::KEY_e | keysyms::KEY_E if !mods.shift && !mods.ctrl => Some(Action::Spawn(self.config.file_manager.clone())),
            keysyms::KEY_m | keysyms::KEY_M if !mods.shift => Some(Action::Minimize),
            // The built-in launcher unless the config names an external one.
            keysyms::KEY_d => Some(self.config.launcher.clone().map(Action::Spawn).unwrap_or(Action::ToggleLauncher)),
            keysyms::KEY_f | keysyms::KEY_F if mods.ctrl && !mods.shift => Some(Action::ToggleMaximize),
            keysyms::KEY_f | keysyms::KEY_F if mods.shift && !mods.ctrl => Some(Action::ToggleFullscreen),
            _ => None,
        }
    }

    /// The action shared by Mod+arrow and its H/J/K/L alias.
    fn direction_action(&self, dir: Direction, mods: &ModifiersState) -> Action {
        if mods.ctrl {
            Action::Snap(dir)
        } else if mods.shift {
            Action::Resize(dir)
        // With Alt as Mod in nested mode, Super is the extra modifier.
        } else if (mods.alt && self.config.mod_key == crate::config::ModKey::Super)
            || (mods.logo && self.config.mod_key == crate::config::ModKey::Alt)
        {
            Action::Swap(dir)
        } else {
            Action::FocusDirection(dir)
        }
    }

    pub fn run_action(&mut self, action: Action) {
        if !self.action_enabled(&action) { return; }
        if self.protocols.locks.locked && !matches!(action, Action::SwitchVt(_)) { return; }
        match action {
            Action::Classic(action) => self.run_classic_action(action),
            Action::Lock => { self.close_launcher(); self.lock_session(); },
            Action::ToggleLauncher => self.toggle_launcher(),
            Action::Screenshot(mode) => self.run_screenshot(mode),
            Action::ClipboardHistory => self.toggle_clipboard_picker(),
            Action::ToggleTiling => { if let Some(o) = self.active_output() { self.toggle_workspace_tiling(&o); } }
            Action::ToggleFloating => { if let Some(w) = self.focused_window() { self.toggle_floating(&w); } }
            Action::RotateSplit => self.rotate_tile(),
            Action::ToggleHdr => crate::backend::udev::toggle_hdr(self),
            Action::SdrWhite(delta) => crate::backend::udev::adjust_sdr_white(self, delta as f32),
            Action::Workspace(index) => { if let Some(output) = self.active_output() { self.switch_workspace(&output, index); } }
            Action::MoveWorkspace(index) => { if let Some(window) = self.focused_window() { self.move_to_workspace(&window, index); } }
            Action::WorkspaceStep(backwards) => self.workspace_step(backwards),
            Action::ToggleOverview => self.toggle_overview(),
            Action::CloseOverview => self.close_overview(),
            Action::WindowHints => self.toggle_window_hints(),
            Action::CheatSheet => self.toggle_cheat_sheet(),
            Action::PanelControls(page) => { if let Some(output) = self.active_output() { self.toggle_panel_controls(page, &output); } },
            Action::ToggleCodeMode => self.set_code_mode(!self.code_mode),
            Action::ToggleFingerPointer => self.toggle_finger_pointer(),
            Action::Quit => self.quit(),
            Action::Spawn(cmd) => self.launch_shell(&cmd, None),
            Action::CloseWindow => self.close_focused(),
            Action::Minimize => { if let Some(window) = self.focused_window() { self.minimize_window(&window); } }
            Action::ToggleMaximize => self.toggle_window_mode(WindowMode::Maximized),
            Action::ToggleFullscreen => self.toggle_window_mode(WindowMode::Fullscreen),
            Action::Snap(dir) => self.snap_direction(dir),
            Action::FocusDirection(dir) => self.focus_direction(dir),
            Action::Swap(dir) => self.swap_direction(dir),
            Action::Resize(dir) => self.resize_direction(dir),
            Action::ResizeMode => self.enter_resize_mode(),
            Action::SwitcherCycle(backwards) => self.switcher_cycle(backwards),
            Action::SwitcherFinish(commit) => { self.switcher_finish(commit); self.classic.switcher_win = false; },
            Action::SwitchVt(vt) => crate::backend::switch_vt(self, vt),
        }
    }

    fn on_pointer_motion<I: InputBackend>(&mut self, event: I::PointerMotionEvent) {
        self.show_pointer();
        let pointer = self.seat.get_pointer().unwrap();
        let current = pointer.current_location();
        let under = self.surface_under(current);
        let mut pos = current + event.delta();
        let proposed=pos;
        if let Some(bbox) = self.outputs_bbox() { pos = clamp_to(pos, bbox); }
        if self.lock.internal() {
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.lock_pointer(false);
            return;
        }
        if self.screenshot_motion(pos) {
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.queue_redraw_all();
            return;
        }
        if self.panel_controls_motion(pos) {
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.queue_redraw_all();
            return;
        }
        if self.music_motion(pos) {
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.queue_redraw_all();
            return;
        }
        self.pressure_motion(current,proposed,pos);
        let allowed = self.pointer_motion_allowed(pos);
        // Relative input belongs to the old focus and remains live while locked.
        pointer.relative_motion(self, under.clone(), &RelativeMotionEvent {
            delta: event.delta(), delta_unaccel: event.delta_unaccel(), utime: event.time(),
        });
        if !allowed { pointer.frame(self); return; }
        self.bar_motion(pos);
        let resizing=self.resize_mode_motion(pos);
        self.overview_motion(pos);
        self.launcher_motion(pos);
        self.clipboard_motion(pos);
        self.classic_motion(pos);
        self.classic_desktop_motion(pos);
        self.notification_motion(pos);
        let new_under = if resizing {None} else {self.surface_under(pos)};
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(self, new_under.clone(), &MotionEvent { location: pos, serial, time: event.time_msec() });
        pointer.frame(self);
        if let Some((surface, _)) = new_under { self.new_constraint(&surface, &pointer); }
        if self.greeter.is_some() {
            self.greeter_pointer(false);
        }
        self.refresh_splitter_cursor();
        self.refresh_resize_cursor();
        self.queue_redraw_all();
    }

    fn on_pointer_motion_absolute<I: InputBackend>(&mut self, event: I::PointerMotionAbsoluteEvent) {
        self.show_pointer();
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
        if self.lock.internal() {
            let pointer = self.seat.get_pointer().unwrap();
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.lock_pointer(false);
            return;
        }
        if self.screenshot_motion(pos) {
            let pointer = self.seat.get_pointer().unwrap();
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.queue_redraw_all();
            return;
        }
        if self.panel_controls_motion(pos) {
            let pointer = self.seat.get_pointer().unwrap();
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.queue_redraw_all();
            return;
        }
        if self.music_motion(pos) {
            let pointer = self.seat.get_pointer().unwrap();
            pointer.motion(self, None, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            pointer.frame(self);
            self.queue_redraw_all();
            return;
        }
        if !self.pointer_motion_allowed(pos) {
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        self.bar_motion(pos);
        let resizing=self.resize_mode_motion(pos);
        self.overview_motion(pos);
        self.launcher_motion(pos);
        self.clipboard_motion(pos);
        self.classic_motion(pos);
        self.classic_desktop_motion(pos);
        self.notification_motion(pos);
        let under = if resizing {None} else {self.surface_under(pos)};
        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: pos,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
        if let Some((surface, _)) = under { self.new_constraint(&surface, &pointer); }
        self.refresh_splitter_cursor();
        self.refresh_resize_cursor();
        self.queue_redraw_all();
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        self.show_pointer();
        let pointer = self.seat.get_pointer().unwrap();
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();

        if self.lock.internal() {
            if button == BTN_LEFT && button_state == ButtonState::Pressed { self.lock_pointer(true); }
            return;
        }
        if self.protocols.locks.locked {
            match button_state {
                ButtonState::Pressed if !self.pointer_buttons.contains(&button) => self.pointer_buttons.push(button),
                ButtonState::Released => self.pointer_buttons.retain(|b| *b != button),
                _ => {}
            }
            self.refresh_lock();
            let under = self.lock_surface_under(pointer.current_location());
            pointer.motion(self, under, &MotionEvent { location: pointer.current_location(), serial, time: event.time_msec() });
            pointer.button(self, &ButtonEvent { button, state: button_state, serial, time: event.time_msec() });
            pointer.frame(self);
            return;
        }

        if self.greeter.is_some() {
            if button_state == ButtonState::Pressed {
                self.greeter_pointer(true);
            }
            return;
        }

        if button_state == ButtonState::Released && self.classic.desktop.drag.is_some()
            && self.classic_desktop_button(button, false) {
            self.suppressed_buttons.retain(|b| *b != button);
            return;
        }
        self.cancel_shell_tap();
        // A new press ends any earlier synthetic release of this button.
        if button_state == ButtonState::Pressed {
            self.suppressed_buttons.retain(|b| *b != button);
        }
        if button_state == ButtonState::Pressed && self.overlay.keyboard.open() {
            self.close_keyboard_overlay();
            self.suppressed_buttons.push(button);
            return;
        }
        // Balance presses already delivered to Smithay before desktop UI can
        // consume their releases. Shell-only presses never enter this list.
        if button_state == ButtonState::Released && self.pointer_buttons.contains(&button) {
            self.pointer_buttons.retain(|b| *b != button);
            self.suppressed_buttons.retain(|b| *b != button);
            pointer.button(self, &ButtonEvent { button, state: button_state, serial, time: event.time_msec() });
            pointer.frame(self);
            return;
        }
        // The screenshot overlay sees both edges of every button; its toast
        // takes one click.
        if self.screenshot_button(pointer.current_location(), button, button_state == ButtonState::Pressed) {
            return;
        }
        if button_state==ButtonState::Released && self.suppressed_buttons.contains(&button) {
            self.suppressed_buttons.retain(|b|*b!=button);
            // Shell presses (panel sliders, dock) are suppressed too; their
            // release still has to end the slider drag.
            self.music_release();
            self.topbar.controls.release();
            return;
        }
        if self.polkit_button(pointer.current_location(), button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.panel_controls_open() && button != BTN_LEFT {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.panel_controls_button(pointer.current_location(), button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.music_button(pointer.current_location(), button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if button_state==ButtonState::Pressed && self.exit_resize_mode() {
            self.suppressed_buttons.push(button);return;
        }
        if self.notification_button(pointer.current_location(), button, button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.clipboard_button(pointer.current_location(), button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.launcher_button(pointer.current_location(), button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.classic.desktop.menu.is_some() && self.classic_desktop_button(button, button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.classic_button(button, button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed && !self.suppressed_buttons.contains(&button) { self.suppressed_buttons.push(button); }
            return;
        }
        if self.classic_decor_button(button, button_state == ButtonState::Pressed, serial, event.time_msec()) { return; }
        if self.shell_button(button, button_state == ButtonState::Pressed) { return; }
        if self.overview_button(button, button_state == ButtonState::Pressed) { return; }
        if self.classic_desktop_button(button, button_state == ButtonState::Pressed) {
            if button_state == ButtonState::Pressed { self.suppressed_buttons.push(button); }
            return;
        }
        if self.desktop_transform_at(pointer.current_location()) && self.layer_hit(pointer.current_location(), &[Layer::Overlay, Layer::Top]).is_none() { return; }
        if button_state == ButtonState::Pressed && button == BTN_LEFT && self.start_band_resize(button,serial,event.time_msec()) {return;}
        if button_state == ButtonState::Pressed && button == BTN_LEFT && !pointer.is_grabbed() {
            if let Some((output, splitter)) = self.splitter_at(pointer.current_location()) {
                let start = GrabStartData { focus: None, button, location: pointer.current_location() };
                let grab = crate::grabs::tiling_grab::SplitterGrab::new(start, output, splitter);
                pointer.set_grab(self, grab, serial, Focus::Clear);
                self.pointer_buttons.push(button);
                pointer.button(self, &ButtonEvent { button, state: button_state, serial, time: event.time_msec() });
                pointer.frame(self);
                return;
            }
        }
        if button_state == ButtonState::Pressed && !pointer.is_grabbed() {
            let pos = pointer.current_location();
            // A launcher holding exclusive keyboard focus keeps it on clicks elsewhere.
            let keep_focus = self.exclusive_layer_focused();
            if let Some(hit) = self.layer_hit(pos, &[Layer::Overlay, Layer::Top]) {
                self.focus_layer(&hit.layer);
            } else {
                let hit = self.window_input_at(pos).map(|(w, l, _, _)| (w, l));
                match hit {
                    Some((window, loc)) => {
                        if !keep_focus {
                            self.focus_window(Some(&window));
                        }
                        let mod_held = self.config.mod_key.is_held(&keyboard.modifier_state());
                        let mode = mode_state(&window).mode;
                        if mod_held
                            && ((button == BTN_LEFT && mode != WindowMode::Fullscreen)
                                || (button == BTN_RIGHT && mode == WindowMode::Floating))
                        {
                            // Mod+drag: the press goes to the grab, not to the client.
                            let start_data = GrabStartData {
                                focus: None,
                                button,
                                location: pos,
                            };
                            if button == BTN_LEFT {
                                let grab = MoveSurfaceGrab::start(start_data, window, loc);
                                pointer.set_grab(self, grab, serial, Focus::Clear);
                            } else {
                                self.start_mod_resize(window, loc, pos, start_data, serial);
                            }
                            self.pointer_buttons.push(button);
                            // Still tell the seat the button is down so the grab
                            // sees the release.
                            pointer.button(
                                self,
                                &ButtonEvent {
                                    button,
                                    state: button_state,
                                    serial,
                                    time: event.time_msec(),
                                },
                            );
                            pointer.frame(self);
                            return;
                        }
                    }
                    None => {
                        if let Some(hit) = self.layer_hit(pos, &[Layer::Bottom, Layer::Background]) {
                            self.focus_layer(&hit.layer);
                        } else if !keep_focus {
                            self.focus_window(None);
                        }
                    }
                }
            }
        }

        self.record_activation_serial(pointer.current_focus(), serial);
        if button_state == ButtonState::Pressed && !self.pointer_buttons.contains(&button) {
            self.pointer_buttons.push(button);
        }
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        if button_state == ButtonState::Pressed { self.install_window_click_grab(serial); }
        pointer.frame(self);
    }

    /// Mod+RMB resize: the edges nearest to the pointer move, the opposite
    /// ones stay anchored.
    fn start_mod_resize(
        &mut self,
        window: smithay::desktop::Window,
        loc: Point<i32, Logical>,
        pos: Point<f64, Logical>,
        start_data: GrabStartData<Gavno>,
        serial: smithay::utils::Serial,
    ) {
        let size = window.geometry().size;
        let rel = pos - loc.to_f64();
        let mut edges = ResizeEdge::empty();
        edges |= if rel.x < size.w as f64 / 2.0 { ResizeEdge::LEFT } else { ResizeEdge::RIGHT };
        edges |= if rel.y < size.h as f64 / 2.0 { ResizeEdge::TOP } else { ResizeEdge::BOTTOM };

        if self.tiled(&window) {
            if let Some(grab) = crate::grabs::tiling_grab::SplitterGrab::for_tile(self,start_data,&window,edges) {
                self.seat.get_pointer().unwrap().set_grab(self,grab,serial,Focus::Clear);
            }
            return;
        }
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });
        toplevel.send_pending_configure();
        let grab = ResizeSurfaceGrab::start(start_data, window, edges, Rectangle::new(loc, size));
        let pointer = self.seat.get_pointer().unwrap();
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        self.show_pointer();
        self.cancel_shell_tap();
        let source = event.source();
        let horizontal = event
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.);
        let vertical = event
            .amount(Axis::Vertical)
            .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);

        if self.polkit_visible() || self.screenshot_active() { return; }
        if self.greeter.is_none() && !self.protocols.locks.locked && self.clipboard_axis(vertical) { return; }
        if self.greeter.is_none() && !self.protocols.locks.locked && self.panel_controls_axis(vertical) { return; }
        if self.greeter.is_none() && !self.protocols.locks.locked && self.music_axis(vertical) { return; }
        if self.greeter.is_none() && !self.protocols.locks.locked && source==AxisSource::Finger {
            if self.precision_scroll((-horizontal,-vertical).into(),[event.amount(Axis::Horizontal),event.amount(Axis::Vertical)]) {return;}
        }
        if self.greeter.is_none() && !self.protocols.locks.locked && self.classic_axis(vertical) { return; }
        if self.greeter.is_none() && !self.protocols.locks.locked && self.classic_desktop_axis(vertical) { return; }
        if self.greeter.is_none() && !self.protocols.locks.locked && self.shell_axis(horizontal, vertical) { return; }
        if self.greeter.is_none() && !self.protocols.locks.locked && source==AxisSource::Finger && self.overview_flick_axis((-horizontal,-vertical).into(),event.time(),[event.amount(Axis::Horizontal),event.amount(Axis::Vertical)]) {return;}
        if self.features.on(crate::module::Feature::Overview) && self.overview.visible() { return; }
        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        if horizontal != 0.0 {
            frame = frame.value(Axis::Horizontal, horizontal);
            if let Some(discrete) = event.amount_v120(Axis::Horizontal) {
                frame = frame.v120(Axis::Horizontal, discrete as i32);
            }
        }
        if vertical != 0.0 {
            frame = frame.value(Axis::Vertical, vertical);
            if let Some(discrete) = event.amount_v120(Axis::Vertical) {
                frame = frame.v120(Axis::Vertical, discrete as i32);
            }
        }
        if source == AxisSource::Finger {
            if event.amount(Axis::Horizontal) == Some(0.0) {
                frame = frame.stop(Axis::Horizontal);
            }
            if event.amount(Axis::Vertical) == Some(0.0) {
                frame = frame.stop(Axis::Vertical);
            }
        }

        let pointer = self.seat.get_pointer().unwrap();
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}

/// Arrow keysyms map to navigation directions.
fn arrow_direction(sym: u32) -> Option<Direction> {
    match sym {
        keysyms::KEY_Left => Some(Direction::Left),
        keysyms::KEY_Right => Some(Direction::Right),
        keysyms::KEY_Up => Some(Direction::Up),
        keysyms::KEY_Down => Some(Direction::Down),
        _ => None,
    }
}

/// Vim alias keysyms map to the arrows they stand in for.
pub(crate) fn vim_arrow(sym: u32) -> Option<u32> {
    match sym {
        keysyms::KEY_h | keysyms::KEY_H => Some(keysyms::KEY_Left),
        keysyms::KEY_j | keysyms::KEY_J => Some(keysyms::KEY_Down),
        keysyms::KEY_k | keysyms::KEY_K => Some(keysyms::KEY_Up),
        keysyms::KEY_l | keysyms::KEY_L => Some(keysyms::KEY_Right),
        _ => None,
    }
}

fn clamp_to(pos: Point<f64, Logical>, rect: Rectangle<i32, Logical>) -> Point<f64, Logical> {
    let max_x = (rect.loc.x + rect.size.w) as f64 - 1.0;
    let max_y = (rect.loc.y + rect.size.h) as f64 - 1.0;
    (
        pos.x.clamp(rect.loc.x as f64, max_x),
        pos.y.clamp(rect.loc.y as f64, max_y),
    )
        .into()
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn lock_chords_and_locked_action_gate() {
        let mut h = crate::state::tests::Harness::new();
        for mod_key in [crate::config::ModKey::Super, crate::config::ModKey::Alt] {
            h.gv.config.mod_key = mod_key;
            let mods = ModifiersState { logo: mod_key == crate::config::ModKey::Super, alt: mod_key == crate::config::ModKey::Alt, ..Default::default() };
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_l), &mods), Some(Action::Lock));
            let mods = ModifiersState { ctrl: true, alt: true, ..Default::default() };
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_l), &mods), Some(Action::Lock));
        }
        h.gv.run_action(Action::Lock);
        assert!(h.gv.protocols.locks.locked);
        h.gv.run_action(Action::ToggleOverview);
        assert!(!h.gv.overview.open());
        h.gv.run_action(Action::Lock);
        assert!(h.gv.protocols.locks.locked);
    }

    #[test]
    fn hdr_shortcuts_follow_configured_modifier() {
        let mut h = crate::state::tests::Harness::new();
        for (mod_key, mut mods) in [
            (crate::config::ModKey::Super, ModifiersState { logo: true, shift: true, ..Default::default() }),
            (crate::config::ModKey::Alt, ModifiersState { alt: true, shift: true, ..Default::default() }),
        ] {
            h.gv.config.mod_key = mod_key;
            for key in [keysyms::KEY_h, keysyms::KEY_H] {
                assert_eq!(h.gv.bind_for(Keysym::from(key), &mods), Some(Action::ToggleHdr));
            }
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_F2), &mods), Some(Action::SdrWhite(-10)));
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_F3), &mods), Some(Action::SdrWhite(10)));
            mods.shift = false;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_h), &mods), None);
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_F2), &mods), None);
        }
        assert!(crate::backend::udev::set_hdr(&mut h.gv, "eDP-1", true).is_err());
    }

    #[test]
    fn tiling_shortcuts_follow_configured_modifier() {
        let mut h=crate::state::tests::Harness::new();
        for (key,mut mods) in [
            (crate::config::ModKey::Super,ModifiersState { logo:true, ..Default::default() }),
            (crate::config::ModKey::Alt,ModifiersState { alt:true, ..Default::default() }),
        ] {
            h.gv.config.mod_key=key;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_t),&mods),Some(Action::ToggleTiling));
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_space),&mods),Some(Action::ToggleFloating));
            mods.shift=true;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_I),&mods),Some(Action::RotateSplit));
            mods.shift=false;
            if key==crate::config::ModKey::Super { mods.alt=true; } else { mods.logo=true; }
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_space),&mods),Some(Action::ToggleTiling));
        }
    }
    #[test]
    fn workspace_shortcuts_follow_configured_modifier() {
        let mut h = crate::state::tests::Harness::new();
        for (mod_key, mut mods) in [
            (crate::config::ModKey::Super, ModifiersState { logo: true, ..Default::default() }),
            (crate::config::ModKey::Alt, ModifiersState { alt: true, ..Default::default() }),
        ] {
            h.gv.config.mod_key = mod_key;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_3), &mods), Some(Action::Workspace(2)));
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_Tab), &mods), Some(Action::WorkspaceStep(false)));
            mods.shift = true;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_3), &mods), Some(Action::MoveWorkspace(2)));
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_Tab), &mods), Some(Action::WorkspaceStep(true)));
            mods.shift = false;
            mods.ctrl = true;
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_Page_Up), &mods), Some(Action::WorkspaceStep(true)));
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_Page_Down), &mods), Some(Action::WorkspaceStep(false)));
            assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_a), &mods), Some(Action::ToggleOverview));
        }
        let copilot = ModifiersState { logo: true, shift: true, ..Default::default() };
        assert_eq!(h.gv.bind_for(Keysym::from(keysyms::KEY_F23), &copilot), Some(Action::ToggleOverview));
    }
}

#[cfg(test)]
#[path = "input_review_tests.rs"]
mod review_tests;

/// Shell command that saves a screenshot to ~/Pictures/Screenshots and copies
/// it to the clipboard. `area` asks slurp for a region; otherwise the whole
/// `output` is captured.
pub fn screenshot_command(area: bool, output: Option<&str>) -> String {
    let target = if area {
        r#"-g "$(slurp)""#.to_owned()
    } else {
        output
            .filter(|o| o.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .map(|o| format!("-o {o}"))
            .unwrap_or_default()
    };
    format!(
        r#"dir="${{XDG_PICTURES_DIR:-$HOME/Pictures}}/Screenshots"; mkdir -p "$dir"; f="$dir/Screenshot_$(date +%F_%H-%M-%S).png"; grim {target} "$f" && wl-copy --type image/png < "$f""#
    )
}

#[cfg(test)]
mod screenshot_tests {
    use super::screenshot_command;

    #[test]
    fn full_and_area_commands() {
        let full = screenshot_command(false, Some("eDP-1"));
        assert!(full.contains(r#"grim -o eDP-1 "$f""#));
        assert!(full.contains("wl-copy --type image/png"));
        assert!(screenshot_command(true, Some("eDP-1")).contains(r#"grim -g "$(slurp)" "$f""#));
        // Output names are never interpolated unless they are plain identifiers.
        assert!(screenshot_command(false, Some("x; rm -rf ~")).contains(r#"grim  "$f""#));
    }
}

impl Gavno {
    pub(crate) fn apply_keyboard_keymap(&mut self, layout: String, variant: String, options: String, keymap: String) -> Result<(), String> {
        let keyboard = self.seat.get_keyboard().ok_or("seat has no keyboard")?;
        // The worker already compiled all rules/symbols. Parsing the resulting
        // text here performs no file I/O and Smithay broadcasts it to clients.
        keyboard.set_keymap_from_string(self, keymap).map_err(|e| format!("cannot apply keymap: {e}"))?;
        self.config.xkb_layout = layout;
        self.config.xkb_variant = variant;
        self.config.xkb_options = Some(options);
        Ok(())
    }
}
