use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{keysyms, FilterResult, Keysym, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::shell::wlr_layer::Layer,
};

use crate::{
    grabs::{resize_grab::ResizeEdge, MoveSurfaceGrab, ResizeSurfaceGrab},
    module::Handled,
    state::spawn_shell,
    window_mode::{mode_state, WindowMode},
    Ds,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// A compositor action bound to a key chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Spawn(String),
    CloseWindow,
    /// Switch to virtual terminal N (Ctrl+Alt+F<N>), TTY backend only.
    SwitchVt(i32),
    ToggleMaximize,
    ToggleFullscreen,
}

impl Ds {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotion { event, .. } => self.on_pointer_motion::<I>(event),
            InputEvent::PointerMotionAbsolute { event, .. } => self.on_pointer_motion_absolute::<I>(event),
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<I>(event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<I>(event),
            _ => {}
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let keycode = event.key_code();
        let key_state = event.state();
        let raw_code: u32 = keycode.raw();

        let keyboard = self.seat.get_keyboard().unwrap();
        let action = keyboard.input::<Option<Action>, _>(
            self,
            keycode,
            key_state,
            serial,
            time,
            |ds, mods, handle| {
                if key_state == KeyState::Released {
                    // Swallow the release of a key whose press was a bind.
                    if let Some(i) = ds.suppressed_keys.iter().position(|k| *k == raw_code) {
                        ds.suppressed_keys.swap_remove(i);
                        return FilterResult::Intercept(None);
                    }
                    return FilterResult::Forward;
                }

                // VT switch keysyms come from the XKB keymap (Ctrl+Alt+F<N>).
                let modified = handle.modified_sym().raw();
                if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&modified) {
                    ds.suppressed_keys.push(raw_code);
                    let vt = (modified - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                    return FilterResult::Intercept(Some(Action::SwitchVt(vt)));
                }

                // Greeter mode: every key goes to the login screen.
                if ds.greeter.is_some() {
                    let sym = handle.modified_sym();
                    let mods = *mods;
                    ds.suppressed_keys.push(raw_code);
                    if let Some(g) = ds.greeter.as_mut() {
                        g.key(sym, sym.key_char(), &mods);
                    }
                    return FilterResult::Intercept(None);
                }

                // Binds match the latin layout, so they work with ru active too.
                let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() else {
                    return FilterResult::Forward;
                };
                let mods = *mods;
                if ds.dispatch_modules(|m, ds| m.key_pressed(ds, sym.raw(), &mods)) == Handled::Yes {
                    ds.suppressed_keys.push(raw_code);
                    return FilterResult::Intercept(None);
                }
                match ds.bind_for(sym, &mods) {
                    Some(action) => {
                        ds.suppressed_keys.push(raw_code);
                        FilterResult::Intercept(Some(action))
                    }
                    None => FilterResult::Forward,
                }
            },
        );

        if let Some(Some(action)) = action {
            self.run_action(action);
        }
        if self.greeter.is_some() {
            let caps = keyboard.modifier_state().caps_lock;
            let layout = keyboard.with_xkb_state(self, |ctx| {
                let xkb = ctx.xkb().lock().unwrap();
                xkb.layout_name(xkb.active_layout()).to_owned()
            });
            if let Some(g) = self.greeter.as_mut() {
                g.set_modifiers(caps, &layout);
            }
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
                g.click(local);
            } else {
                g.pointer_motion(local);
            }
        }
        self.queue_redraw_all();
    }

    /// The core binds (KooL Hyprland-Dots subset). Modules see keys first.
    fn bind_for(&self, sym: Keysym, mods: &ModifiersState) -> Option<Action> {
        let main = self.config.mod_key.is_held(mods);
        let sym = Keysym::from(sym.raw()).raw();

        if mods.ctrl && mods.alt && sym == keysyms::KEY_Delete {
            return Some(Action::Quit);
        }
        if !main {
            return None;
        }
        match sym {
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => Some(Action::Spawn(self.config.terminal.clone())),
            keysyms::KEY_q if !mods.shift => Some(Action::CloseWindow),
            keysyms::KEY_d => self.config.launcher.clone().map(Action::Spawn),
            keysyms::KEY_f | keysyms::KEY_F if mods.ctrl && !mods.shift => Some(Action::ToggleMaximize),
            keysyms::KEY_f | keysyms::KEY_F if mods.shift && !mods.ctrl => Some(Action::ToggleFullscreen),
            _ => None,
        }
    }

    pub fn run_action(&mut self, action: Action) {
        match action {
            Action::Quit => self.quit(),
            Action::Spawn(cmd) => spawn_shell(&cmd),
            Action::CloseWindow => self.close_focused(),
            Action::ToggleMaximize => self.toggle_window_mode(WindowMode::Maximized),
            Action::ToggleFullscreen => self.toggle_window_mode(WindowMode::Fullscreen),
            Action::SwitchVt(vt) => crate::backend::switch_vt(self, vt),
        }
    }

    fn on_pointer_motion<I: InputBackend>(&mut self, event: I::PointerMotionEvent) {
        let pointer = self.seat.get_pointer().unwrap();
        let mut pos = pointer.current_location() + event.delta();
        if let Some(bbox) = self.outputs_bbox() {
            pos = clamp_to(pos, bbox);
        }
        let serial = SERIAL_COUNTER.next_serial();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: pos,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.relative_motion(
            self,
            under,
            &RelativeMotionEvent {
                delta: event.delta(),
                delta_unaccel: event.delta_unaccel(),
                utime: event.time(),
            },
        );
        pointer.frame(self);
        if self.greeter.is_some() {
            self.greeter_pointer(false);
        }
        self.queue_redraw_all();
    }

    fn on_pointer_motion_absolute<I: InputBackend>(&mut self, event: I::PointerMotionAbsoluteEvent) {
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
        self.queue_redraw_all();
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        let pointer = self.seat.get_pointer().unwrap();
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button_code();
        let button_state = event.state();

        if self.greeter.is_some() {
            if button_state == ButtonState::Pressed {
                self.greeter_pointer(true);
            }
            return;
        }

        if button_state == ButtonState::Pressed && !pointer.is_grabbed() {
            let pos = pointer.current_location();
            // A launcher holding exclusive keyboard focus keeps it on clicks elsewhere.
            let keep_focus = self.exclusive_layer_focused();
            if let Some(hit) = self.layer_hit(pos, &[Layer::Overlay, Layer::Top]) {
                self.focus_layer(&hit.layer);
            } else {
                let hit = self.space.element_under(pos).map(|(w, l)| (w.clone(), l));
                match hit {
                    Some((window, loc)) => {
                        if !keep_focus {
                            self.focus_window(Some(&window));
                        }
                        let mod_held = self.config.mod_key.is_held(&keyboard.modifier_state());
                        let floating = mode_state(&window).mode == WindowMode::Floating;
                        if mod_held && floating && (button == BTN_LEFT || button == BTN_RIGHT) {
                            // Mod+drag: the press goes to the grab, not to the client.
                            let start_data = GrabStartData {
                                focus: None,
                                button,
                                location: pos,
                            };
                            if button == BTN_LEFT {
                                let grab = MoveSurfaceGrab {
                                    start_data,
                                    window,
                                    initial_window_location: loc,
                                };
                                pointer.set_grab(self, grab, serial, Focus::Clear);
                            } else {
                                self.start_mod_resize(window, loc, pos, start_data, serial);
                            }
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
    }

    /// Mod+RMB resize: the edges nearest to the pointer move, the opposite
    /// ones stay anchored.
    fn start_mod_resize(
        &mut self,
        window: smithay::desktop::Window,
        loc: Point<i32, Logical>,
        pos: Point<f64, Logical>,
        start_data: GrabStartData<Ds>,
        serial: smithay::utils::Serial,
    ) {
        let size = window.geometry().size;
        let rel = pos - loc.to_f64();
        let mut edges = ResizeEdge::empty();
        edges |= if rel.x < size.w as f64 / 2.0 { ResizeEdge::LEFT } else { ResizeEdge::RIGHT };
        edges |= if rel.y < size.h as f64 / 2.0 { ResizeEdge::TOP } else { ResizeEdge::BOTTOM };

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
        let source = event.source();
        let horizontal = event
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.);
        let vertical = event
            .amount(Axis::Vertical)
            .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);

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

fn clamp_to(pos: Point<f64, Logical>, rect: Rectangle<i32, Logical>) -> Point<f64, Logical> {
    let max_x = (rect.loc.x + rect.size.w) as f64 - 1.0;
    let max_y = (rect.loc.y + rect.size.h) as f64 - 1.0;
    (
        pos.x.clamp(rect.loc.x as f64, max_x),
        pos.y.clamp(rect.loc.y as f64, max_y),
    )
        .into()
}
